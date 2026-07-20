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
//! `tairix_abi`'s `KeyInput`. A key with no focused window
//! (focus on the desktop, or the focused window since gone) is ignored
//! rather than misdelivered.

use tairix_controls::{ScrollOrientation, TrackHit};

use crate::geometry::Point;
use crate::viewport::FurnitureHit;
use crate::window::{Window, WindowId};
use crate::Compositor;

// The device-level pointer vocabulary the router consumes is shared with the
// taskbar, which routes the same events but may not depend on the window
// manager. It therefore lives in `lib/input`; the
// compositor re-exports it so callers keep referring to
// `tairix_wm::{InputEvent, PointerButton}` (one definition).
pub use tairix_input::{InputEvent, Key, Modifiers, NamedKey, PointerButton};

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
    /// A primary press landed on the desktop background; focus, if any,
    /// was cleared.
    DesktopPressed,
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
}

/// An in-flight interactive move: the grabbed window and the offset
/// from the pointer to the window's top-left corner, held constant for
/// the duration of the drag so the window tracks the pointer without
/// jumping.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
struct MoveGrab {
    window: WindowId,
    offset: Point,
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

    /// Process one input `event` against `compositor`, returning what
    /// changed.
    pub fn handle(&mut self, event: InputEvent, compositor: &mut Compositor) -> InputResponse {
        match event {
            InputEvent::PointerMoved { to } => {
                self.pointer = to;
                if self.scroll_grab.is_some() {
                    self.scroll_drag_to(to, compositor)
                } else {
                    self.drag_to(to, compositor)
                }
            }
            InputEvent::PointerScrolled { dx, dy } => self.wheel(dx, dy, compositor),
            InputEvent::PointerPressed {
                button: PointerButton::Primary,
            } => self.press_primary(compositor),
            InputEvent::PointerReleased {
                button: PointerButton::Primary,
            } => self.release_primary(),
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
    /// Returns [`InputResponse::Ignored`] when focus rests on the desktop or
    /// the focused window is no longer known to `compositor` — in the latter
    /// case focus is dropped so a stale window never keeps the keyboard
    /// (fail closed).
    fn deliver_key(
        &mut self,
        key: Key,
        modifiers: Modifiers,
        pressed: bool,
        compositor: &Compositor,
    ) -> InputResponse {
        let Some(window) = self.focused else {
            return InputResponse::Ignored;
        };
        if compositor.window(window).is_none() {
            self.focused = None;
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
        self.grab = Some(MoveGrab {
            window,
            offset: Point::new(
                self.pointer.x.saturating_sub(origin.x),
                self.pointer.y.saturating_sub(origin.y),
            ),
        });
        true
    }

    /// Handle a primary-button press at the current pointer position.
    fn press_primary(&mut self, compositor: &mut Compositor) -> InputResponse {
        let Some(window) = compositor.window_at(self.pointer) else {
            self.focused = None;
            return InputResponse::DesktopPressed;
        };
        compositor.raise(window);
        self.focused = Some(window);
        // A press on the root-viewport furniture is the window manager's, not
        // the client's: the furniture hit map keeps it away from the surface.
        if let Some(hit) = compositor.furniture_hit(window, self.pointer) {
            if !matches!(hit, FurnitureHit::Client) {
                return self.press_furniture(window, hit, compositor);
            }
        }
        let origin = compositor
            .window(window)
            .map_or(Point::ORIGIN, Window::origin);
        InputResponse::Activated {
            window,
            local: Point::new(
                self.pointer.x.saturating_sub(origin.x),
                self.pointer.y.saturating_sub(origin.y),
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

    /// Route a scroll-wheel gesture to the root viewport under the pointer.
    ///
    /// The ticks drive the shared scroll model (one line step per tick);
    /// the pointer does not move. Returns [`InputResponse::Scrolled`] when an
    /// offset changed, else [`InputResponse::Ignored`] (no window, no root
    /// viewport, or already at the bound).
    fn wheel(&mut self, dx: i32, dy: i32, compositor: &mut Compositor) -> InputResponse {
        let Some(window) = compositor.window_at(self.pointer) else {
            return InputResponse::Ignored;
        };
        match compositor.scroll_root(window, |vp| vp.wheel(dx, dy)) {
            Some(true) => InputResponse::Scrolled { window },
            _ => InputResponse::Ignored,
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

    /// Handle a primary-button release: end any active move-grab or scrollbar
    /// thumb drag. A released thumb has already committed its offset, so
    /// ending the capture reports nothing further.
    fn release_primary(&mut self) -> InputResponse {
        if self.scroll_grab.take().is_some() {
            return InputResponse::Ignored;
        }
        match self.grab.take() {
            Some(grab) => InputResponse::MoveEnded {
                window: grab.window,
            },
            None => InputResponse::Ignored,
        }
    }

    /// Apply pointer motion to an active move-grab, if any.
    fn drag_to(&mut self, to: Point, compositor: &mut Compositor) -> InputResponse {
        let Some(grab) = self.grab else {
            return InputResponse::Ignored;
        };
        let origin = Point::new(
            to.x.saturating_sub(grab.offset.x),
            to.y.saturating_sub(grab.offset.y),
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
