//! Input routing: focus, click-to-activate, and interactive move-grabs.
//!
//! The [`InputRouter`] turns a stream of device-level pointer
//! [`InputEvent`]s into window-manager actions against a
//! [`Compositor`]: it tracks the pointer position and the focused
//! window, raises and focuses the window under a primary-button press
//! (*click-to-activate*), and drives an interactive *move-grab* so a
//! window can be dragged across the screen.
//!
//! The router holds no pixels and performs no compositing; it is the
//! policy layer that sits on top of the compositor's scene graph. A
//! move-grab is started explicitly through [`InputRouter::begin_move`]
//! rather than armed on every press, so a press that lands on a
//! window's *content* (reported back to the client through
//! [`InputResponse::Activated`]) never moves the window — only a grab
//! the desktop decorations choose to start, typically when the press
//! landed on a title bar, does. This keeps content interaction and
//! window dragging cleanly separated (no "drag
//! anywhere" hack).
//!
//! Keyboard input is delivered to the focused window: the router owns
//! *which* window has the keyboard ([`InputRouter::focused`]) and routes a
//! [`InputEvent::KeyPressed`] / [`InputEvent::KeyReleased`] to it as an
//! [`InputResponse::Key`], leaving the bytes-on-the-wire encoding to
//! `tairix_abi`'s `KeyInput`. A key with no focused window (focus on the
//! desktop, or the focused window since gone) is never misdelivered to some
//! other window: it is reported as an [`InputResponse::DesktopKey`] for the
//! desktop layer's owner to interpret.
//!
//! # The pointer is delivered, not assumed
//!
//! The router acts on the pointer only while it *holds* it. The desktop's seat
//! owns the window stack and resolves, per event, which surface the pointer
//! rests on — the window manager's windows and the desktop layer behind them,
//! or the desktop's own chrome (its bar and the popovers that bar opens) — and
//! delivers the event to that one router. [`set_pointer_focus`] is how the
//! seat says so: an [`Entered`] carries the position the pointer arrived at,
//! and a [`Left`] is what makes the hover this router is drawing go away —
//! a title bar's command control unlights when the pointer crosses onto the
//! bar, exactly as it unlights when the pointer crosses onto another window.
//!
//! A hover cannot be ended by inferring it from a position, because the
//! position often does not change: a window raised over a hovered title bar
//! leaves the pointer exactly where it was. Only the seat knows, so only the
//! seat can say.
//!
//! An in-flight grab holds the pointer for its whole gesture, so a drag that
//! runs off the window — or under the bar — keeps arriving here; that is the
//! seat's rule, and it is why this router never has to re-derive who owns a
//! drag.
//!
//! [`set_pointer_focus`]: InputRouter::set_pointer_focus
//! [`Entered`]: tairix_input::PointerFocus::Entered
//! [`Left`]: tairix_input::PointerFocus::Left
//!
//! Input that lands on nothing is likewise reported rather than swallowed.
//! The desktop beneath the window stack is a real surface with a real owner
//! ([`Compositor::set_desktop`](crate::Compositor::set_desktop)), so a
//! primary press, a secondary press, and a motion that resolve to no window
//! come back as [`DesktopPressed`], [`DesktopSecondaryPressed`], and
//! [`DesktopPointerMoved`]. The router still takes no action of its own for
//! them — it only names where they landed.
//!
//! [`DesktopPressed`]: InputResponse::DesktopPressed
//! [`DesktopSecondaryPressed`]: InputResponse::DesktopSecondaryPressed
//! [`DesktopPointerMoved`]: InputResponse::DesktopPointerMoved

use tairix_controls::{
    damage, FurniturePart, ResizeEdge, ResizeEvent, ResizeGrabber, ScrollOrientation,
    TitleBarEvent, TrackHit, WindowControlKind,
};

use crate::geometry::{Point, Rect};
use crate::viewport::FurnitureHit;
use crate::window::{Window, WindowId};
use crate::Compositor;

// The device-level pointer vocabulary the router consumes is shared with the
// taskbar, which routes the same events but may not depend on the window
// manager. It therefore lives in `lib/input`; the
// compositor re-exports it so callers keep referring to
// `tairix_wm::{InputEvent, PointerButton}` (one definition).
pub use tairix_input::{InputEvent, Key, Modifiers, NamedKey, PointerButton, PointerFocus};

/// What the [`InputRouter`] did with an [`InputEvent`].
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum InputResponse {
    /// The event changed no window-manager state.
    Ignored,
    /// A primary press activated `window`: it was raised to the top of
    /// the z-order and given focus. `local` is the press position in
    /// that window's surface coordinates, for the client to interpret.
    Activated {
        /// The activated window.
        window: WindowId,
        /// Press position relative to the window's top-left corner.
        local: Point,
    },
    /// A secondary (right) press landed on `window`'s client area: the
    /// window was raised to the top of the z-order and given focus, and the
    /// press is delivered to the client as a secondary-button press (which a
    /// client uses to open its context menu). `local` is the press position
    /// in the window's surface coordinates. A secondary press on window
    /// furniture opens no menu (it is consumed), and one on the desktop is
    /// reported to the desktop layer's owner as
    /// [`DesktopSecondaryPressed`](Self::DesktopSecondaryPressed): the window
    /// manager never synthesises a context menu of its own — only the surface
    /// the press landed on decides what it means.
    SecondaryActivated {
        /// The window whose client received the secondary press.
        window: WindowId,
        /// Press position relative to the window's top-left corner.
        local: Point,
    },
    /// A primary press landed on the desktop background; focus, if any,
    /// was cleared.
    DesktopPressed,
    /// A secondary (right) press landed on the desktop background, so the
    /// desktop layer's owner decides what it means (it opens the desktop's
    /// own context menu). Unlike the primary press this clears no focus and
    /// changes no window-manager state: a right-click asks a question of the
    /// surface under the pointer, it does not activate anything. The press
    /// position is the router's own [`pointer`](InputRouter::pointer).
    DesktopSecondaryPressed,
    /// Pointer motion resolved to the desktop background — no window lies
    /// under it and no grab is in flight — so the desktop layer's owner
    /// interprets it (hover feedback, a drag it started itself). The
    /// position is the router's own [`pointer`](InputRouter::pointer), which
    /// this motion has already updated.
    DesktopPointerMoved,
    /// A key event while focus rests on the desktop rather than a window.
    /// The router takes no action of its own; the desktop's owner decides
    /// what the key means (moving its selection, activating, clearing it).
    DesktopKey {
        /// The key that changed state.
        key: Key,
        /// The modifiers held while the key changed state.
        modifiers: Modifiers,
        /// `true` for a press, `false` for a release.
        pressed: bool,
    },
    /// An active move-grab dragged `window` to a new `origin`.
    Moved {
        /// The window being dragged.
        window: WindowId,
        /// The window's new top-left position.
        origin: Point,
    },
    /// An active move-grab ended (primary released, or the grabbed
    /// window vanished).
    MoveEnded {
        /// The window whose move-grab ended.
        window: WindowId,
    },
    /// A key event was delivered to the focused `window` for the client to
    /// interpret. The router takes no action of its own on a key; it only
    /// names the recipient.
    Key {
        /// The window that holds the keyboard and received the event.
        window: WindowId,
        /// The key that changed state.
        key: Key,
        /// The modifiers held while the key changed state.
        modifiers: Modifiers,
        /// `true` for a press, `false` for a release.
        pressed: bool,
    },
    /// A root-viewport scrollbar's offset changed (a wheel tick, a track
    /// page step, or a thumb drag). The embedder re-reads the window's
    /// [`RootViewport`](crate::RootViewport) models and forwards the new
    /// offset to the client, which re-renders its content.
    Scrolled {
        /// The window whose root viewport scrolled.
        window: WindowId,
    },
    /// A primary press landed on a window's scrollbar furniture (a track or
    /// the corner) and was consumed by the window manager. It is **not**
    /// delivered to the client: the furniture owns that region, so an
    /// application look-alike inside the client can never intercept it.
    FurniturePressed {
        /// The window whose furniture received the press.
        window: WindowId,
    },
    /// A wheel gesture landed on a window that owns its own content
    /// scrolling (it exposes no window-manager root viewport), so the
    /// window manager consumed nothing and the ticks belong to the
    /// application. The embedder forwards them to that window's owner over
    /// the window channel; the application applies them to its nested
    /// scroll model. Ticks are in device detent units (positive `dx`
    /// toward the logical end, positive `dy` downward).
    AppScroll {
        /// The window the pointer was over.
        window: WindowId,
        /// Signed horizontal scroll ticks.
        dx: i32,
        /// Signed vertical scroll ticks.
        dy: i32,
    },
    /// A window-command control on the decorated frame (close, minimize,
    /// put-to-back, size-toggle) was activated — by a completed primary
    /// click on the button, or by Space/Enter on the keyboard-focused
    /// control. The window manager owns the frame furniture, so the
    /// activation is never delivered to the client; the embedder maps the
    /// typed command to the window lifecycle (a cooperative close request, a
    /// restack, a size-state change) over the existing window path.
    WindowControl {
        /// The decorated window whose frame control was activated.
        window: WindowId,
        /// Which window command the control represents.
        control: WindowControlKind,
    },
    /// A secondary press landed on a window-command control — the alternate
    /// gesture beside the control's own command, which the control never
    /// performs.
    ///
    /// The window manager changes nothing for it: the window is raised and
    /// focused as any secondary press raises and focuses, the control's
    /// drawn state is untouched, and no window command runs. The embedder
    /// decides what (if anything) the gesture means for that control and
    /// tells the owning client; a surface with no owning client is left
    /// alone.
    WindowControlAlternate {
        /// The decorated window whose frame control took the press.
        window: WindowId,
        /// Which window command the pressed control represents.
        control: WindowControlKind,
    },
    /// An active resize-grab dragged a decorated window's frame edge, so the
    /// window manager resized the window to a new outer geometry (its client
    /// content surface grew or shrank to match). The embedder forwards the
    /// new client size to the owning client so it re-renders at that size.
    Resized {
        /// The window being resized.
        window: WindowId,
    },
    /// An active resize-grab ended (primary released, the gesture was
    /// cancelled, or the resized window vanished).
    ResizeEnded {
        /// The window whose resize-grab ended.
        window: WindowId,
    },
    /// A client-area pointer motion the window manager consumed no furniture
    /// for, delivered to the application to interpret (a hover highlight, an
    /// in-content scrollbar-thumb drag). `local` is the position in the
    /// window's client-viewport coordinates. While the client holds the
    /// implicit pointer grab a primary press started, the motion is reported
    /// to the grabbed window even after the pointer leaves it — `local`
    /// clamped into the client — so an in-content drag tracks like every
    /// other grab; otherwise it is a plain hover over the window under the
    /// pointer.
    ClientPointerMoved {
        /// The window whose client received the motion.
        window: WindowId,
        /// Motion position in the window's client-viewport coordinates.
        local: Point,
    },
    /// A primary release that ended a client pointer grab, delivered to the
    /// grabbed window so an in-content click or drag completes (a tab or
    /// combo selection, a released scrollbar thumb). `local` is the release
    /// position in the window's client-viewport coordinates, clamped into
    /// the client.
    ClientPointerReleased {
        /// The window whose client received the release.
        window: WindowId,
        /// Release position in the window's client-viewport coordinates.
        local: Point,
    },
}

/// An in-flight interactive move: the grabbed window and the offset
/// from the pointer to the window's top-left corner, held constant for
/// the duration of the drag so the window tracks the pointer without
/// jumping.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
struct MoveGrab {
    window: WindowId,
    offset: Point,
    /// The title bar's drag surface in window-local coordinates, captured at
    /// grab start so a motion sample never re-lays the furniture out.
    /// [`Rect::EMPTY`] for an undecorated window, which is not clamped at all
    /// — it has no move surface of its own to keep reachable.
    drag: Rect,
}

/// An in-flight scrollbar thumb drag: the window, which bar is being
/// dragged, and the pointer-to-thumb-start anchor captured when the drag
/// began, held constant so the content does not jump on grab.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
struct ScrollGrab {
    window: WindowId,
    orientation: ScrollOrientation,
    anchor: i32,
}

/// An in-flight interactive resize: the decorated window, which frame edge is
/// being dragged, the shared [`ResizeGrabber`] driving the gesture lifecycle,
/// and the outer geometry and pointer position captured when the drag began —
/// held constant so each motion recomputes the new geometry from the original
/// (no drift) and an Escape restores the pre-drag rectangle exactly.
#[derive(Clone, Debug, Eq, PartialEq)]
struct ResizeGrab {
    window: WindowId,
    edge: ResizeEdge,
    grabber: ResizeGrabber,
    start_outer: Rect,
    start_pointer: Point,
    /// The smallest outer size this window may be dragged down to, in
    /// physical pixels, captured at grab start so the clamp never re-derives
    /// the frame metrics mid-drag.
    min_outer: (u32, u32),
}

impl ResizeGrab {
    /// Advance the gesture with pointer `event` over the captured outer
    /// rectangle, returning what the grabber made of it.
    fn gesture(&mut self, event: &InputEvent) -> Option<ResizeEvent> {
        self.grabber
            .on_pointer(event, self.start_outer, &mut damage::sink())
    }

    /// Offer `key` to the gesture (Escape cancels), returning what the grabber
    /// made of it.
    fn gesture_key(&mut self, key: Key) -> Option<ResizeEvent> {
        self.grabber
            .on_key(key, self.start_outer, &mut damage::sink())
    }
}

/// Routes pointer input into [`Compositor`] actions.
///
/// The router is the desktop's input-policy state: the current pointer
/// position, the focused window, and any in-flight move-grab. It is
/// driven entirely through [`InputRouter::handle`] and
/// [`InputRouter::begin_move`]; it never panics and never grants
/// itself authority over a window it was not handed by the compositor.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct InputRouter {
    pointer: Point,
    focused: Option<WindowId>,
    grab: Option<MoveGrab>,
    scroll_grab: Option<ScrollGrab>,
    /// The decorated window whose title-bar command controls have captured
    /// the current primary press, so the release completes the click on the
    /// same frame (and can never leak to the client).
    control_grab: Option<WindowId>,
    /// The in-flight interactive resize, if any.
    resize_grab: Option<ResizeGrab>,
    /// Whether the focused decorated window's frame furniture holds the
    /// keyboard. A press on a title-bar command control claims it (so Space,
    /// Enter, and the arrow keys drive the controls); a press on the client
    /// returns it, so a decorated window's content keeps its keys until the
    /// user reaches for the furniture.
    furniture_key_focus: bool,
    /// The client window holding the implicit pointer grab a primary press on
    /// its content started, so the motion and the release complete on that
    /// same window — an in-content drag (a scrollbar thumb) or click (a tab, a
    /// combo item) never leaks to a window the pointer later crosses. Cleared
    /// on the release, and on any press that starts a furniture, move, or
    /// desktop interaction instead.
    client_grab: Option<WindowId>,
    /// The decorated window whose furniture last received a hover sample, so
    /// the pointer leaving it can be told to drop the highlight. Only the
    /// window manager sees a pointer over its own decorations, so nothing
    /// else can clear one.
    frame_hover: Option<WindowId>,
}

impl InputRouter {
    /// Create a router with the pointer at the screen origin, no focus,
    /// and no active grab.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The current pointer position in screen coordinates.
    #[must_use]
    pub const fn pointer(&self) -> Point {
        self.pointer
    }

    /// The focused window, the one that receives keyboard input, or
    /// `None` when focus rests on the desktop.
    #[must_use]
    pub const fn focused(&self) -> Option<WindowId> {
        self.focused
    }

    /// `true` while an interactive move-grab is in progress.
    #[must_use]
    pub const fn is_moving(&self) -> bool {
        self.grab.is_some()
    }

    /// `true` while a scrollbar thumb is captured for dragging.
    #[must_use]
    pub const fn is_scrolling(&self) -> bool {
        self.scroll_grab.is_some()
    }

    /// The frame edge an interactive window resize-grab is dragging, or `None`
    /// when no resize is in progress.
    ///
    /// The edge is what the pointer shape is chosen from, so the cursor states
    /// which way the window is being dragged for as long as the grab lasts —
    /// including while the pointer has run past the edge it started on.
    #[must_use]
    pub const fn resizing_edge(&self) -> Option<ResizeEdge> {
        match &self.resize_grab {
            Some(grab) => Some(grab.edge),
            None => None,
        }
    }

    /// Give keyboard focus to `window`, validated against `compositor`.
    ///
    /// This is the programmatic counterpart of focusing by a pointer press:
    /// the taskbar's running-task list activates a window by id rather than by
    /// position, so the session glue moves focus here to keep the window
    /// manager's focus in step with the bar. Unlike a press it does not raise
    /// the window — the caller raises it (and shows it) so the policy stays in
    /// one place.
    ///
    /// Returns `false`, changing nothing, when `compositor` does not know
    /// `window` (fail closed; the router never grants itself
    /// focus over a window it was not handed).
    pub fn focus(&mut self, window: WindowId, compositor: &Compositor) -> bool {
        if compositor.window(window).is_none() {
            return false;
        }
        self.focused = Some(window);
        true
    }

    /// Drop keyboard focus, leaving it on the desktop.
    ///
    /// The session glue calls this when the focused window is minimised from
    /// the taskbar: the window is hidden, so nothing should hold the keyboard.
    pub fn unfocus(&mut self) {
        self.focused = None;
    }

    /// Take the seat's answer to "does the pointer rest on one of the windows
    /// this router routes?", applying what changes when it does.
    ///
    /// The seat resolves this from the window stack — which only it can see —
    /// before it delivers any pointer event, so the two facts a router needs
    /// arrive together: it is handed the events it owns, and it is told the
    /// moment it stops owning them.
    ///
    /// * [`Entered`](PointerFocus::Entered) adopts the position the pointer
    ///   arrived at and lights the decoration under it. The pointer can arrive
    ///   without moving — a window above it closed, a grab ended, a modal
    ///   surface shut — and no motion event exists for those, which is why the
    ///   position travels with the answer.
    /// * [`Left`](PointerFocus::Left) drops the hover the frame under the
    ///   pointer was drawing. It cannot be inferred from a position: when a
    ///   window is raised over a hovered title bar the pointer has not moved at
    ///   all, and hit-testing it would answer "still on the control" and leave
    ///   it lit under the window now in front of it.
    ///
    /// Nothing else is touched: no focus moves, no window raises, no grab
    /// starts or ends. A grab holds the pointer for its whole gesture, so the
    /// seat never reports a leave while one is in flight.
    pub fn set_pointer_focus(&mut self, focus: PointerFocus, compositor: &mut Compositor) {
        match focus {
            PointerFocus::Entered { at } => {
                self.pointer = at;
                let over = Self::hover_target(at, compositor);
                self.hover_furniture(over, compositor);
            }
            PointerFocus::Left => {
                if let Some(left) = self.frame_hover.take() {
                    compositor.frame_pointer_left(left);
                }
            }
        }
    }

    /// The window whose *decorations* lie under `at`, or `None` when the
    /// pointer is over a client area, over the desktop, or over a window with
    /// no frame.
    ///
    /// The one definition of "which furniture is under the pointer", shared by
    /// the motion path and by the seat's enter, so a decoration lights up the
    /// same way whether the pointer moved onto it or the window above it went
    /// away.
    fn hover_target(at: Point, compositor: &Compositor) -> Option<WindowId> {
        let over = compositor.window_at(at);
        let on_client = over.is_some_and(|window| over_client(window, at, compositor));
        over.filter(|_| !on_client)
    }

    /// Process one input `event` against `compositor`, returning what
    /// changed.
    ///
    /// Repainting is the compositor's: every furniture control this event
    /// reaches reports its own repainted bounds where the frame is mutated, so
    /// the router carries no damage of its own.
    pub fn handle(&mut self, event: InputEvent, compositor: &mut Compositor) -> InputResponse {
        match event {
            InputEvent::PointerMoved { to } => {
                self.pointer = to;
                if self.scroll_grab.is_some() {
                    self.scroll_drag_to(to, compositor)
                } else if self.resize_grab.is_some() {
                    self.resize_drag_to(to, compositor)
                } else if self.control_grab.is_some() {
                    self.control_drag_to(&event, compositor)
                } else if self.grab.is_some() {
                    self.drag_to(to, compositor)
                } else {
                    self.client_pointer_moved(to, compositor)
                }
            }
            InputEvent::PointerScrolled { dx, dy } => self.wheel(dx, dy, compositor),
            InputEvent::PointerPressed {
                button: PointerButton::Primary,
            } => self.press_primary(compositor),
            InputEvent::PointerReleased {
                button: PointerButton::Primary,
            } => self.release_primary(compositor),
            InputEvent::PointerPressed {
                button: PointerButton::Secondary,
            } => self.press_secondary(compositor),
            InputEvent::PointerPressed { .. } | InputEvent::PointerReleased { .. } => {
                InputResponse::Ignored
            }
            InputEvent::KeyPressed { key, modifiers } => {
                self.deliver_key(key, modifiers, true, compositor)
            }
            InputEvent::KeyReleased { key, modifiers } => {
                self.deliver_key(key, modifiers, false, compositor)
            }
        }
    }

    /// Deliver a key event to the focused window, naming it as the recipient.
    ///
    /// Returns [`InputResponse::DesktopKey`] when focus rests on the desktop
    /// or the focused window is no longer known to `compositor` — in the
    /// latter case focus is dropped first, so a stale window never keeps the
    /// keyboard (fail closed) and the key goes to the desktop rather than to
    /// whichever window happens to be there now.
    fn deliver_key(
        &mut self,
        key: Key,
        modifiers: Modifiers,
        pressed: bool,
        compositor: &mut Compositor,
    ) -> InputResponse {
        // Escape cancels an in-flight resize, restoring the pre-drag geometry
        // exactly — the shared grabber owns that cancel semantics.
        if pressed {
            if let Some(grab) = self.resize_grab.as_mut() {
                if let Some(ResizeEvent::Cancel) = grab.gesture_key(key) {
                    let window = grab.window;
                    let start_outer = grab.start_outer;
                    self.resize_grab = None;
                    compositor.resize_window(window, start_outer);
                    return InputResponse::ResizeEnded { window };
                }
            }
        }
        let desktop_key = InputResponse::DesktopKey {
            key,
            modifiers,
            pressed,
        };
        let Some(window) = self.focused else {
            return desktop_key;
        };
        if compositor.window(window).is_none() {
            self.focused = None;
            self.furniture_key_focus = false;
            return desktop_key;
        }
        // When the focused decorated window's frame furniture holds the
        // keyboard, keys drive its command controls (Space/Enter activate the
        // focused control; the arrows move focus between them) and never reach
        // the client — the furniture owns those keys.
        if self.furniture_key_focus && compositor.window_frame(window).is_some() {
            if pressed {
                if let Some(TitleBarEvent::Control(control)) = compositor.frame_key(window, key) {
                    return InputResponse::WindowControl { window, control };
                }
            }
            return InputResponse::Ignored;
        }
        InputResponse::Key {
            window,
            key,
            modifiers,
            pressed,
        }
    }

    /// Start an interactive move-grab on the focused window, anchored at
    /// the current pointer position. Returns `false` when there is no
    /// focused window or it is no longer known to the compositor, in
    /// which case no grab is started (fail closed).
    ///
    /// Decorations call this when a press lands on a window's move
    /// handle (e.g. its title bar); the subsequent pointer motion then
    /// drags the window through [`InputResponse::Moved`].
    pub fn begin_move(&mut self, compositor: &Compositor) -> bool {
        let Some(window) = self.focused else {
            return false;
        };
        let Some(origin) = compositor.window(window).map(Window::origin) else {
            return false;
        };
        let drag = compositor
            .window_drag_surface(window)
            .map_or(Rect::EMPTY, |rect| {
                Rect::new(
                    rect.left().saturating_sub(origin.x),
                    rect.top().saturating_sub(origin.y),
                    rect.width,
                    rect.height,
                )
            });
        // Starting a move supersedes the implicit client pointer grab: this
        // press moves the window, so its motion and release drive the move,
        // never leak to the client as an in-content drag.
        self.client_grab = None;
        self.grab = Some(MoveGrab {
            window,
            offset: Point::new(
                self.pointer.x.saturating_sub(origin.x),
                self.pointer.y.saturating_sub(origin.y),
            ),
            drag,
        });
        true
    }

    /// Handle a primary-button press at the current pointer position.
    fn press_primary(&mut self, compositor: &mut Compositor) -> InputResponse {
        // A fresh press supersedes any prior client grab; it is re-armed below
        // only when this press lands on client content.
        self.client_grab = None;
        let Some(window) = compositor.window_at(self.pointer) else {
            self.focused = None;
            return InputResponse::DesktopPressed;
        };
        compositor.raise(window);
        self.focused = Some(window);

        // A press on the outer decoration frame is the window manager's, kept
        // off the client by the frame's furniture hit map: the title bar
        // begins a cooperative move, a command control latches for its click,
        // a resize edge begins a resize, and the inert rim is consumed. Only a
        // press classified as the client falls through to the client paths.
        if let Some(part) = compositor.frame_hit(window, self.pointer) {
            match part {
                FurniturePart::TitleBar => {
                    self.begin_move(compositor);
                    return InputResponse::FurniturePressed { window };
                }
                FurniturePart::WindowControl(_) => {
                    return self.begin_control_grab(window, compositor);
                }
                FurniturePart::ResizeEdge(edge) => {
                    return self.begin_resize_grab(window, edge, compositor);
                }
                FurniturePart::Frame => {
                    return InputResponse::FurniturePressed { window };
                }
                FurniturePart::Client | FurniturePart::Outside => {}
            }
        }

        // A press on the root-viewport furniture is the window manager's, not
        // the client's: the furniture hit map keeps it away from the surface.
        if let Some(hit) = compositor.furniture_hit(window, self.pointer) {
            if !matches!(hit, FurnitureHit::Client) {
                return self.press_furniture(window, hit, compositor);
            }
        }

        // A client press returns the keyboard to the client (a decorated
        // window's furniture no longer holds it) and activates the window. The
        // press position is reported relative to the client viewport, so a
        // decorated window's client sees content-local coordinates.
        self.furniture_key_focus = false;
        // The client content holds the implicit pointer grab until release, so
        // its motion and the release complete on this window (an in-content
        // drag/click) rather than leaking to a window the pointer crosses.
        self.client_grab = Some(window);
        let client = compositor
            .window_client_rect(window)
            .map_or(Point::ORIGIN, |rect| rect.origin);
        InputResponse::Activated {
            window,
            local: Point::new(
                self.pointer.x.saturating_sub(client.x),
                self.pointer.y.saturating_sub(client.y),
            ),
        }
    }

    /// Route a pointer motion no window-manager grab claimed: during a client
    /// pointer grab it is an in-content drag reported to the grabbed window
    /// (even once the pointer leaves it, `local` clamped into the client);
    /// otherwise it is a hover over the client content under the pointer. A
    /// motion over the desktop, over window furniture, or over a window that
    /// has vanished is [`InputResponse::Ignored`] (no client owns it) — but a
    /// motion over furniture still reaches that furniture, so a decoration
    /// highlights under the pointer like any other control.
    fn client_pointer_moved(&mut self, to: Point, compositor: &mut Compositor) -> InputResponse {
        if let Some(window) = self.client_grab {
            let Some(client) = compositor.window_client_rect(window) else {
                self.client_grab = None;
                return InputResponse::Ignored;
            };
            return InputResponse::ClientPointerMoved {
                window,
                local: client_local(to, client),
            };
        }
        let over = compositor.window_at(to);
        self.hover_furniture(Self::hover_target(to, compositor), compositor);
        let Some(window) = over else {
            return InputResponse::DesktopPointerMoved;
        };
        if !over_client(window, to, compositor) {
            return InputResponse::Ignored;
        }
        let Some(client) = compositor.window_client_rect(window) else {
            return InputResponse::Ignored;
        };
        InputResponse::ClientPointerMoved {
            window,
            local: client_local(to, client),
        }
    }

    /// Hand the current pointer position to the furniture of `over` — the
    /// window whose decorations lie under it, `None` for none — and to the
    /// window the pointer has just left, so its highlight is dropped.
    ///
    /// A frame is never told the pointer is on it while the pointer is on a
    /// client, and never left believing the pointer is still on it once it is
    /// not: a decoration lights up under the pointer and goes out behind it,
    /// exactly as a control in a window does. Nothing is delivered while a
    /// grab is in flight, because the grab already owns the motion.
    ///
    /// The frame repaints only what it reports changing, so a sample that
    /// stays within one control, or merely crosses the drag region, costs a
    /// hit test and nothing else.
    fn hover_furniture(&mut self, over: Option<WindowId>, compositor: &mut Compositor) {
        if self.frame_hover.is_none() && over.is_none() {
            return;
        }
        let moved = InputEvent::PointerMoved { to: self.pointer };
        if let Some(left) = self.frame_hover.filter(|&id| Some(id) != over) {
            compositor.frame_pointer(left, &moved);
        }
        self.frame_hover = over.filter(|&id| compositor.window_frame(id).is_some());
        if let Some(id) = self.frame_hover {
            compositor.frame_pointer(id, &moved);
        }
    }

    /// Handle a secondary (right) press at the current pointer position.
    ///
    /// Like [`press_primary`](Self::press_primary) it raises and focuses the
    /// window under the pointer, but it starts no move/resize/control grab and
    /// pages no scrollbar: a right-click's only meaning is "open the context
    /// menu of the surface under the pointer". A press on the client area is
    /// delivered as [`InputResponse::SecondaryActivated`] for the client to
    /// interpret; one on the desktop background is reported as
    /// [`InputResponse::DesktopSecondaryPressed`] for the desktop layer's
    /// owner; one on window furniture opens no menu (the window manager
    /// consumes it). The WM never synthesises a menu of its own — only the
    /// surface pressed decides what a right-click means.
    ///
    /// One furniture region answers more than "consumed": a press on a
    /// window-command control is reported as
    /// [`InputResponse::WindowControlAlternate`] so the embedder can offer
    /// the control's alternate gesture. The control itself neither arms nor
    /// changes appearance, and no window command runs.
    fn press_secondary(&mut self, compositor: &mut Compositor) -> InputResponse {
        let Some(window) = compositor.window_at(self.pointer) else {
            return InputResponse::DesktopSecondaryPressed;
        };
        compositor.raise(window);
        self.focused = Some(window);
        // Route the press through the frame furniture, which owns the one
        // definition of which control lies under a point. A synthetic move
        // first, so the control knows the pointer is over it.
        let moved = InputEvent::PointerMoved { to: self.pointer };
        compositor.frame_pointer(window, &moved);
        let press = InputEvent::PointerPressed {
            button: PointerButton::Secondary,
        };
        if let Some(TitleBarEvent::AlternateControl(control)) =
            compositor.frame_pointer(window, &press)
        {
            return InputResponse::WindowControlAlternate { window, control };
        }
        // A right-click on the outer decoration frame or the root-viewport
        // furniture is not a client press: the window manager owns those
        // regions and offers no context menu there, so consume it.
        if let Some(part) = compositor.frame_hit(window, self.pointer) {
            if !matches!(part, FurniturePart::Client | FurniturePart::Outside) {
                return InputResponse::FurniturePressed { window };
            }
        }
        if let Some(hit) = compositor.furniture_hit(window, self.pointer) {
            if !matches!(hit, FurnitureHit::Client) {
                return InputResponse::FurniturePressed { window };
            }
        }
        // A client right-press returns the keyboard to the client and is
        // delivered in content-local coordinates, exactly as the primary
        // client press is, so the client's context-menu hit-test sees the
        // same coordinate space its listing does.
        self.furniture_key_focus = false;
        let client = compositor
            .window_client_rect(window)
            .map_or(Point::ORIGIN, |rect| rect.origin);
        InputResponse::SecondaryActivated {
            window,
            local: Point::new(
                self.pointer.x.saturating_sub(client.x),
                self.pointer.y.saturating_sub(client.y),
            ),
        }
    }

    /// Handle a primary press that landed on window furniture: a thumb starts
    /// a drag, a track region pages toward it, and the corner is inert. The
    /// press is always consumed by the window manager.
    fn press_furniture(
        &mut self,
        window: WindowId,
        hit: FurnitureHit,
        compositor: &mut Compositor,
    ) -> InputResponse {
        match hit {
            FurnitureHit::Vertical(TrackHit::Thumb) => {
                self.begin_scroll_grab(window, ScrollOrientation::Vertical, compositor);
            }
            FurnitureHit::Horizontal(TrackHit::Thumb) => {
                self.begin_scroll_grab(window, ScrollOrientation::Horizontal, compositor);
            }
            FurnitureHit::Vertical(region) => {
                let forward = matches!(region, TrackHit::AfterThumb);
                compositor.scroll_root(window, |vp| vp.page(ScrollOrientation::Vertical, forward));
            }
            FurnitureHit::Horizontal(region) => {
                let forward = matches!(region, TrackHit::AfterThumb);
                compositor
                    .scroll_root(window, |vp| vp.page(ScrollOrientation::Horizontal, forward));
            }
            FurnitureHit::Corner | FurnitureHit::Client => {}
        }
        InputResponse::FurniturePressed { window }
    }

    /// Capture the scrollbar thumb of `orientation` for dragging, recording
    /// the pointer-to-thumb-start anchor so the content does not jump. Does
    /// nothing (no grab) when that bar is absent or its track is degenerate.
    fn begin_scroll_grab(
        &mut self,
        window: WindowId,
        orientation: ScrollOrientation,
        compositor: &Compositor,
    ) {
        let Some(bounds) = compositor.window(window).map(Window::bounds) else {
            return;
        };
        let Some(viewport) = compositor.root_viewport(window) else {
            return;
        };
        let Some((track, geometry)) = viewport.track_and_geometry(bounds, orientation) else {
            return;
        };
        let along = match orientation {
            ScrollOrientation::Vertical => self.pointer.y - track.top(),
            ScrollOrientation::Horizontal => self.pointer.x - track.left(),
        };
        let thumb_start = i32::try_from(geometry.thumb().start).unwrap_or(0);
        self.scroll_grab = Some(ScrollGrab {
            window,
            orientation,
            anchor: along - thumb_start,
        });
    }

    /// Route a scroll-wheel gesture to the window under the pointer.
    ///
    /// The ticks drive the shared scroll model (one line step per tick);
    /// the pointer does not move. When the window exposes a window-manager
    /// root viewport, the ticks scroll it: [`InputResponse::Scrolled`] if
    /// an offset changed, else [`InputResponse::Ignored`] (already at the
    /// bound). When the window owns its own content scrolling (no root
    /// viewport), the ticks are the application's:
    /// [`InputResponse::AppScroll`] names the recipient so the embedder can
    /// forward them over the window channel. With no window under the
    /// pointer the gesture is [`InputResponse::Ignored`].
    fn wheel(&mut self, dx: i32, dy: i32, compositor: &mut Compositor) -> InputResponse {
        let Some(window) = compositor.window_at(self.pointer) else {
            return InputResponse::Ignored;
        };
        match compositor.scroll_root(window, |vp| vp.wheel(dx, dy)) {
            Some(true) => InputResponse::Scrolled { window },
            Some(false) => InputResponse::Ignored,
            None => InputResponse::AppScroll { window, dx, dy },
        }
    }

    /// Apply pointer motion to an active scrollbar thumb drag.
    ///
    /// The thumb start is derived from the current range each move, so a
    /// content change mid-drag re-clamps the offset rather than producing an
    /// invalid one; the captured anchor keeps the grab point under the
    /// pointer.
    fn scroll_drag_to(&mut self, to: Point, compositor: &mut Compositor) -> InputResponse {
        let Some(grab) = self.scroll_grab else {
            return InputResponse::Ignored;
        };
        let Some(bounds) = compositor.window(grab.window).map(Window::bounds) else {
            self.scroll_grab = None;
            return InputResponse::Ignored;
        };
        let Some(viewport) = compositor.root_viewport(grab.window) else {
            self.scroll_grab = None;
            return InputResponse::Ignored;
        };
        let Some((track, geometry)) = viewport.track_and_geometry(bounds, grab.orientation) else {
            self.scroll_grab = None;
            return InputResponse::Ignored;
        };
        let along = match grab.orientation {
            ScrollOrientation::Vertical => to.y - track.top(),
            ScrollOrientation::Horizontal => to.x - track.left(),
        };
        let offset = geometry.offset_for_drag(along, grab.anchor);
        match compositor.scroll_root(grab.window, |vp| vp.set_offset(grab.orientation, offset)) {
            Some(true) => InputResponse::Scrolled {
                window: grab.window,
            },
            _ => InputResponse::Ignored,
        }
    }

    /// Begin a command-control interaction: the press latched a title-bar
    /// button, so the release must complete the click on the same frame. The
    /// control's own press-state is set by feeding the press to the frame (a
    /// synthetic move first, so the control knows the pointer is over it), and
    /// the furniture takes keyboard focus so Space/Enter can also drive it.
    /// The press itself is consumed by the window manager, never the client.
    fn begin_control_grab(
        &mut self,
        window: WindowId,
        compositor: &mut Compositor,
    ) -> InputResponse {
        self.control_grab = Some(window);
        self.furniture_key_focus = true;
        let moved = InputEvent::PointerMoved { to: self.pointer };
        compositor.frame_pointer(window, &moved);
        let press = InputEvent::PointerPressed {
            button: PointerButton::Primary,
        };
        // A control that fired on the press itself (not the usual click, which
        // completes on release) is dispatched immediately and ends the grab.
        if let Some(TitleBarEvent::Control(control)) = compositor.frame_pointer(window, &press) {
            self.control_grab = None;
            return InputResponse::WindowControl { window, control };
        }
        InputResponse::FurniturePressed { window }
    }

    /// Continue a command-control interaction: feed the motion to the frame so
    /// the latched control tracks hover, and keep the press consumed. The
    /// window vanishing mid-interaction ends the grab (fail closed).
    fn control_drag_to(
        &mut self,
        event: &InputEvent,
        compositor: &mut Compositor,
    ) -> InputResponse {
        let Some(window) = self.control_grab else {
            return InputResponse::Ignored;
        };
        if compositor.window_frame(window).is_none() {
            self.control_grab = None;
            return InputResponse::Ignored;
        }
        compositor.frame_pointer(window, event);
        InputResponse::FurniturePressed { window }
    }

    /// Begin an interactive resize on `window` at frame `edge`, capturing the
    /// outer geometry and pointer so each motion recomputes the new rectangle
    /// from the original. Falls back to consuming the press when the window is
    /// not a resizable decorated window (fail closed — no grab is started).
    fn begin_resize_grab(
        &mut self,
        window: WindowId,
        edge: ResizeEdge,
        compositor: &Compositor,
    ) -> InputResponse {
        let (Some(start_outer), Some(min_outer)) = (
            compositor.window(window).map(Window::bounds),
            compositor.window_min_outer_size(window),
        ) else {
            return InputResponse::FurniturePressed { window };
        };
        let mut grab = ResizeGrab {
            window,
            edge,
            grabber: ResizeGrabber::new(),
            start_outer,
            start_pointer: self.pointer,
            min_outer,
        };
        // Prime the grabber's pointer, then begin its gesture over the whole
        // outer rectangle (the pointer is inside it): the shared control owns
        // the gesture lifecycle and the Escape cancel.
        grab.gesture(&InputEvent::PointerMoved { to: self.pointer });
        grab.gesture(&InputEvent::PointerPressed {
            button: PointerButton::Primary,
        });
        self.resize_grab = Some(grab);
        InputResponse::FurniturePressed { window }
    }

    /// Apply pointer motion to an active resize-grab: drive the shared grabber,
    /// recompute the clamped outer rectangle for the grabbed edge, and resize
    /// the window to it. The window vanishing ends the grab (fail closed).
    fn resize_drag_to(&mut self, to: Point, compositor: &mut Compositor) -> InputResponse {
        let Some(grab) = self.resize_grab.as_mut() else {
            return InputResponse::Ignored;
        };
        let moved = InputEvent::PointerMoved { to };
        if !matches!(grab.gesture(&moved), Some(ResizeEvent::Moved { .. })) {
            return InputResponse::FurniturePressed {
                window: grab.window,
            };
        }
        let window = grab.window;
        let new_outer = compute_resized_outer(grab, to);
        if compositor.resize_window(window, new_outer) {
            InputResponse::Resized { window }
        } else {
            self.resize_grab = None;
            InputResponse::ResizeEnded { window }
        }
    }

    /// Handle a primary-button release: end any active scrollbar thumb drag,
    /// resize-grab, command-control interaction, or move-grab. A released
    /// thumb or resized window has already committed its geometry, and a
    /// completed control click is dispatched here (the release is what
    /// activates a button).
    fn release_primary(&mut self, compositor: &mut Compositor) -> InputResponse {
        if self.scroll_grab.take().is_some() {
            return InputResponse::Ignored;
        }
        if let Some(mut grab) = self.resize_grab.take() {
            grab.gesture(&InputEvent::PointerReleased {
                button: PointerButton::Primary,
            });
            return InputResponse::ResizeEnded {
                window: grab.window,
            };
        }
        if let Some(window) = self.control_grab.take() {
            let released = InputEvent::PointerReleased {
                button: PointerButton::Primary,
            };
            if let Some(TitleBarEvent::Control(control)) =
                compositor.frame_pointer(window, &released)
            {
                return InputResponse::WindowControl { window, control };
            }
            return InputResponse::FurniturePressed { window };
        }
        if let Some(window) = self.client_grab.take() {
            let Some(client) = compositor.window_client_rect(window) else {
                return InputResponse::Ignored;
            };
            return InputResponse::ClientPointerReleased {
                window,
                local: client_local(self.pointer, client),
            };
        }
        match self.grab.take() {
            Some(grab) => InputResponse::MoveEnded {
                window: grab.window,
            },
            None => InputResponse::Ignored,
        }
    }

    /// Apply pointer motion to an active move-grab, if any, keeping a
    /// grabbable patch of the window's move surface on screen.
    fn drag_to(&mut self, to: Point, compositor: &mut Compositor) -> InputResponse {
        let Some(grab) = self.grab else {
            return InputResponse::Ignored;
        };
        let origin = clamp_move_origin(
            Point::new(
                to.x.saturating_sub(grab.offset.x),
                to.y.saturating_sub(grab.offset.y),
            ),
            grab.drag,
            compositor.screen_rect(),
        );
        if compositor.move_window(grab.window, origin) {
            InputResponse::Moved {
                window: grab.window,
                origin,
            }
        } else {
            self.grab = None;
            InputResponse::MoveEnded {
                window: grab.window,
            }
        }
    }
}

/// The new outer rectangle a resize-grab produces when the pointer is at `to`:
/// the grabbed edge(s) of the captured `start_outer` move by the pointer
/// delta, the un-grabbed edges stay put, and the result is clamped to the
/// window's captured minimum outer size — the greater of what its title bar
/// needs to seat its commands with a drag surface between them and what its
/// application declared it can lay out at. The top edge is never a resize
/// edge (the title bar lives there), so it is fixed.
fn compute_resized_outer(grab: &ResizeGrab, to: Point) -> Rect {
    let start = grab.start_outer;
    let dx = to.x - grab.start_pointer.x;
    let dy = to.y - grab.start_pointer.y;
    let (left_edge, right_edge, bottom_edge) = match grab.edge {
        ResizeEdge::Left => (true, false, false),
        ResizeEdge::Right => (false, true, false),
        ResizeEdge::Bottom => (false, false, true),
        ResizeEdge::BottomLeft => (true, false, true),
        ResizeEdge::BottomRight => (false, true, true),
    };
    let min_w = i32::try_from(grab.min_outer.0).unwrap_or(i32::MAX);
    let min_h = i32::try_from(grab.min_outer.1).unwrap_or(i32::MAX);

    let top = start.top();
    let mut left = start.left();
    let mut right = start.right();
    let mut bottom = start.bottom();
    if left_edge {
        left = start.left() + dx;
        if right - left < min_w {
            left = right - min_w;
        }
    }
    if right_edge {
        right = start.right() + dx;
        if right - left < min_w {
            right = left + min_w;
        }
    }
    if bottom_edge {
        bottom = start.bottom() + dy;
        if bottom - top < min_h {
            bottom = top + min_h;
        }
    }
    let width = u32::try_from(right - left).unwrap_or(0);
    let height = u32::try_from(bottom - top).unwrap_or(0);
    Rect::new(left, top, width, height)
}

/// `origin` clamped so `drag` — the title bar's move surface in window-local
/// coordinates — keeps a grabbable patch inside `screen`.
///
/// A window may hang off any edge, which is what a desktop expects, but never
/// so far that nothing is left to drag it back by. The whole band stays on
/// screen vertically, because a title bar half above the top edge is one the
/// pointer cannot reach along its length; sideways a square of the band's own
/// height is enough, which is the smallest patch that is still a comfortable
/// pointer target. `screen` is the whole framebuffer, so a single big desktop
/// spanning several monitors is one region and a window may straddle two of
/// them.
///
/// An empty `drag` — an undecorated window, or a band with no span between its
/// clusters — leaves `origin` alone: there is nothing to keep reachable, and
/// clamping against a rectangle with no area would pin the window to an edge
/// for no gain.
fn clamp_move_origin(origin: Point, drag: Rect, screen: Rect) -> Point {
    if drag.width == 0 || drag.height == 0 || screen.width == 0 || screen.height == 0 {
        return origin;
    }
    let reach = i32::try_from(drag.width.min(drag.height)).unwrap_or(i32::MAX);
    let min_x = screen
        .left()
        .saturating_add(reach)
        .saturating_sub(drag.right());
    let max_x = screen
        .right()
        .saturating_sub(reach)
        .saturating_sub(drag.left());
    let min_y = screen.top().saturating_sub(drag.top());
    let max_y = screen.bottom().saturating_sub(drag.bottom());
    Point::new(
        origin.x.clamp(min_x, max_x.max(min_x)),
        origin.y.clamp(min_y, max_y.max(min_y)),
    )
}

/// Whether `point` over `window` falls on the client content rather than
/// window furniture (the outer frame, title bar, or a root-viewport
/// scrollbar). An undecorated window with no root viewport is all client.
fn over_client(window: WindowId, point: Point, compositor: &Compositor) -> bool {
    if let Some(part) = compositor.frame_hit(window, point) {
        if !matches!(part, FurniturePart::Client) {
            return false;
        }
    }
    if let Some(hit) = compositor.furniture_hit(window, point) {
        if !matches!(hit, FurnitureHit::Client) {
            return false;
        }
    }
    true
}

/// The client-local position of screen `point`, clamped into `client`.
///
/// A hover already lands inside the client, so the clamp is a no-op there; it
/// matters during a client pointer grab, where the pointer may leave the
/// window and the delivered position must stay a valid in-content coordinate
/// (so an in-content drag keeps tracking the grabbed edge rather than jumping
/// or wrapping). The result is always non-negative, so the session can encode
/// it as the unsigned window-local pixels the pointer event carries.
fn client_local(point: Point, client: Rect) -> Point {
    let max_x = client.right().saturating_sub(1).max(client.left());
    let max_y = client.bottom().saturating_sub(1).max(client.top());
    let x = point.x.clamp(client.left(), max_x) - client.left();
    let y = point.y.clamp(client.top(), max_y) - client.top();
    Point::new(x, y)
}
