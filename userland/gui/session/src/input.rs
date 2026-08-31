//! The desktop's **input seat**: one pointer, one keyboard, and the decision
//! about which surface each event belongs to.
//!
//! The session owns a seat — one display plus the keyboard and pointer
//! attached to it (`lib/seat`, `docs/src/desktop/seat.md`) — and this is where
//! that seat's input is routed once it has arrived inside the session.
//!
//! The desktop has two independent input routers — the window manager's
//! [`InputRouter`] (focus, click-to-activate, interactive move- and
//! resize-grabs, every application window and the desktop layer behind them)
//! and the taskbar's [`TaskbarInput`] (the bar, its program-library popup, its
//! context menus, the hover window picker, the notification popover, and the
//! Switchboard capsule's readout) — and both consume the **same** shared
//! `tairix_input` (`lib/input`) event vocabulary. A real input source produces
//! one stream, so something must decide which router each event belongs to.
//! Neither GUI crate may depend on the other, so that decision is session
//! glue, and [`SessionInputRouter`] is that glue.
//!
//! # Why a seat, and not two routers guessing
//!
//! Each router knows its own geometry. Neither can see the *stack*, and
//! without the stack geometry is not an answer: the bar's clock stays at the
//! bar's coordinates when a window is dragged across it, so a router that
//! hit-tested only its own rectangles would act on gestures the user aimed at
//! the window in front of it — a click doing something on a control that is
//! not even visible, hover feedback lighting up beneath someone else's window,
//! a popover opening over it. Nothing pins the bar topmost; it is an ordinary
//! compositor window, and every application window is raised above it the
//! moment it is opened or clicked.
//!
//! So the seat owns the two facts neither router can: **where the pointer is**,
//! and **which surface it rests on**. It resolves the second before it
//! delivers anything, hands the event to that one router, and tells the other
//! that the pointer has left — the enter/leave pair
//! ([`PointerFocus`]) every window system needs,
//! for exactly the reason every window system needs it.
//!
//! # The policy
//!
//! One event does exactly one thing, and it happens where the user was
//! looking:
//!
//! * **A modal surface of the bar's holds the pointer.** While the bar's
//!   context menu or its program-library popup is open, every pointer event
//!   and every key routes to the taskbar, wherever the pointer is. That is an
//!   *active grab* in the ordinary window-system sense, and it is what lets a
//!   press anywhere dismiss the surface (the click-away) without also acting
//!   on what it landed on. Nothing leaks to the windows beneath.
//! * **A held button holds the pointer.** The first press takes an implicit
//!   grab for whichever surface it landed on, and every event up to the
//!   release of the *last* button goes there — so a window drag that runs
//!   under the bar keeps dragging, a Switchboard capsule press that slides off
//!   the capsule still resolves on the capsule's own terms, and a release can
//!   never be claimed by a surface that did not see the press. There is no
//!   "offer it to one and then the other": the surface that took the press
//!   owns the gesture.
//! * **Otherwise the stack decides.** The surface the compositor finds drawn
//!   under the pointer ([`Compositor::window_at`]) gets the event: the
//!   taskbar when that window is one the [`TaskbarPresenter`] placed, the
//!   window manager for anything else — an application window, a session
//!   dialog, the lock screen, or the desktop layer when there is no window at
//!   all. The test is per event position, never per surface: a window covering
//!   the bar's trailing end leaves every button it does not cover still the
//!   bar's.
//! * **Motion is delivered, not fanned.** Only the surface holding the pointer
//!   is told the pointer moved, so only it updates hover feedback and resolves
//!   pointer gestures. The other is told the pointer *left*, which is the only
//!   way a hover can end: when a window rises over a hovered control the
//!   pointer has not moved at all, and re-testing its unchanged position would
//!   answer "still hovered" and strand the highlight — and any popover it
//!   opened — over the window now in front of it.
//! * **Keys follow the keyboard, not the pointer.** They go to the window
//!   manager, which delivers them to the focused window; the taskbar takes
//!   them only while one of its modal surfaces is open. A pointer resting on
//!   the bar never diverts a keystroke from the window the user is typing in.
//!
//! # What this is worth beyond correctness
//!
//! The bar is *trusted* desktop chrome: its menus can offer to lock the
//! screen, to log out, to re-authenticate for a privileged application. An
//! unprivileged window that could drive that chrome's state — provoke a
//! popover to appear over itself at a moment it chose, or have a click it
//! received acted on by a control the user could not see — would have a
//! user-interface redressing primitive. Resolving every pointer event against
//! the stack, in one place, is what denies it: chrome reacts only to input the
//! user actually directed at chrome. It is the same reason the lock screen is
//! safe here — its window is not one the presenter placed, so while it is up
//! the bar cannot be reached by the pointer at all.
//!
//! The seat holds no pixels and grants itself no authority: it owns the two
//! inner routers and drives them against the embedder's [`Compositor`],
//! [`Taskbar`], and [`TaskbarPresenter`], passed in on each
//! [`handle`](SessionInputRouter::handle). Composing the taskbar and
//! window-manager crates is the permitted `userland/gui/*` edge; nothing
//! outside `userland/gui/*` depends on this glue. It never panics: every
//! routed sub-call is itself total and fails closed.

use tairix_taskbar::{Taskbar, TaskbarInput, TaskbarResponse};
use tairix_wm::{
    Compositor, InputEvent, InputResponse, InputRouter, Modifiers, Point, PointerButton,
    PointerFocus, Scale, WindowId,
};

use crate::presenter::TaskbarPresenter;

/// What the [`SessionInputRouter`] did with one [`InputEvent`].
///
/// The seat routes each event to exactly one of the two desktop
/// routers, so its outcome is either a taskbar action, a window-manager
/// action, or nothing. A sub-router that consumed the event but changed no
/// state collapses to [`Ignored`](Self::Ignored), exactly as the underlying
/// routers report their own no-ops, so the embedder sees one uniform "nothing
/// happened" outcome.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SessionInputResponse {
    /// The event changed no desktop state (a press or motion that the surface
    /// holding the pointer did not act on).
    Ignored,
    /// The event was routed to the taskbar, which acted on it.
    Taskbar(TaskbarResponse),
    /// The event was routed to the window manager, which acted on it.
    WindowManager(InputResponse),
}

/// Which of the desktop's two routers the pointer rests on.
///
/// Two variants, because that is exactly the fan-out decision: the window
/// manager sorts application windows, session surfaces, and the desktop layer
/// out among themselves by the same stacking hit test, and the taskbar sorts
/// its own bar, menus, and popovers out by its own layout. The seat only has
/// to say which of the two is looking at the pixel under the pointer.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
enum PointerOwner {
    /// The desktop's own chrome: the bar, and every popup and popover it
    /// opens — the windows the [`TaskbarPresenter`] placed.
    Chrome,
    /// The window manager's: an application window, a surface the session
    /// placed itself (the lock screen, a dialog), or the desktop layer behind
    /// them all when no window is under the pointer.
    ///
    Windows,
}

/// The bit each pointer button holds in the seat's held-button set.
///
/// A set rather than a flag because the implicit grab ends when the *last*
/// button comes up: pressing primary, then secondary, then releasing primary
/// must keep the gesture with the surface that took the first press, exactly
/// as it does everywhere else.
const fn button_bit(button: PointerButton) -> u8 {
    match button {
        PointerButton::Primary => 1 << 0,
        PointerButton::Secondary => 1 << 1,
        PointerButton::Middle => 1 << 2,
    }
}

/// The desktop's seat: one pointer, one keyboard, routed to the taskbar and
/// the window manager.
///
/// It owns the window manager's [`InputRouter`] and the taskbar's
/// [`TaskbarInput`], the pointer position, and the pointer's focus — see the
/// [module docs](self) for the policy. Drive it through
/// [`handle`](Self::handle); re-resolve the focus after the window stack
/// changes with
/// [`refresh_pointer_focus`](Self::refresh_pointer_focus); start a title-bar
/// drag through [`begin_move`](Self::begin_move).
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct SessionInputRouter {
    wm: InputRouter,
    taskbar: TaskbarInput,
    /// Where the pointer is. The desktop's **one** copy of that fact: the
    /// inner routers hold a position only for the span they hold the pointer,
    /// and everything that needs the live position — the cursor overlay, the
    /// cursor's shape, the desktop layer's own hit tests — reads it here.
    pointer: Point,
    /// Which router the pointer currently rests on, and therefore which one
    /// has been told it holds the pointer. `None` when it is not the seat's to
    /// route: before the first event, and while an embedder-owned modal
    /// surface has taken the stream ([`yield_pointer`](Self::yield_pointer)).
    /// Kept in step by [`focus_on`](Self::focus_on), the one place either
    /// router is told.
    focus: Option<PointerOwner>,
    /// The surface holding the implicit grab a button press took, if any
    /// button is down. It outranks the stack for as long as it lasts, so a
    /// gesture completes where it started.
    grab: Option<PointerOwner>,
    /// The pointer buttons currently held, as [`button_bit`] flags. The grab
    /// ends when this reaches zero, never on the first release.
    buttons: u8,
}

impl SessionInputRouter {
    /// A seat with the pointer at the screen origin, on the desktop, with no
    /// focus and no in-flight grab.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The current pointer position in screen coordinates.
    ///
    /// The seat tracks the device, so this is the desktop's one live answer.
    /// The inner routers' own positions are where each was last handed the
    /// pointer, which is what their hit tests need and is not the same fact.
    #[must_use]
    pub const fn pointer(&self) -> Point {
        self.pointer
    }

    /// The window that owns the keyboard, or `None` when focus rests on the
    /// desktop. Delegates to the window manager's router, which owns focus.
    #[must_use]
    pub fn focused(&self) -> Option<WindowId> {
        self.wm.focused()
    }

    /// The window manager's input router.
    ///
    /// The desktop's pointer-cursor controller reads its interaction state —
    /// the in-flight move- and resize-grabs — to choose the on-screen cursor
    /// shape ([`desired_cursor`](tairix_wm::desired_cursor)). It is the window
    /// manager's router that owns that state, so the controller reads it here
    /// rather than the seat keeping a second copy. The *position* the shape is
    /// chosen at is the seat's ([`pointer`](Self::pointer)) and is passed
    /// alongside: the cursor has to be right over the bar too, which this
    /// router never holds.
    #[must_use]
    pub const fn wm(&self) -> &InputRouter {
        &self.wm
    }

    /// The strip index of the application whose hover window picker is
    /// pending — the slot the pointer is resting its dwell out on — or `None`
    /// when none is.
    ///
    /// The embedder scales that application's window frames while the dwell
    /// runs, so the picker appears already drawn. It is the bar's router that
    /// owns the dwell, so the answer is read from there rather than the seat
    /// keeping a second copy.
    #[must_use]
    pub const fn dwelling_app(&self) -> Option<usize> {
        self.taskbar.dwelling_app()
    }

    /// `park_ns` shortened to the moment the hover picker's pending open
    /// dwell or closing grace is due, or left exactly as it is when neither
    /// is pending.
    #[must_use]
    pub fn park_deadline_ns(&self, now_ns: u64, park_ns: u64) -> u64 {
        self.taskbar.park_deadline_ns(now_ns, park_ns)
    }

    /// Resolve whichever of the hover picker's timed edges has come due at
    /// `now_ns`.
    pub fn tick(&mut self, taskbar: &mut Taskbar, now_ns: u64) -> TaskbarResponse {
        self.taskbar.tick(taskbar, now_ns)
    }

    /// `true` while an interactive window move-grab is in progress.
    #[must_use]
    pub fn is_moving(&self) -> bool {
        self.wm.is_moving()
    }

    /// Give keyboard focus to `window`, validated against `compositor`.
    ///
    /// The session glue calls this when the taskbar activates a running task by
    /// id, so the window manager's focus follows the bar. Returns `false`,
    /// changing nothing, when `compositor` does not know `window`. Delegates to the window manager's router, which owns focus.
    pub fn focus(&mut self, window: WindowId, compositor: &Compositor) -> bool {
        self.wm.focus(window, compositor)
    }

    /// Drop keyboard focus, leaving it on the desktop — the counterpart of
    /// [`focus`](Self::focus), called when the focused window is minimised from
    /// the taskbar.
    pub fn unfocus(&mut self) {
        self.wm.unfocus();
    }

    /// Start an interactive move-grab on the focused window, anchored at the
    /// current pointer position. Returns `false` (starting no grab) when there
    /// is no focused window or it is no longer known to `compositor`. Decorations call this on a title-bar press; the
    /// subsequent motion then drags the window.
    pub fn begin_move(&mut self, compositor: &Compositor) -> bool {
        self.wm.begin_move(compositor)
    }

    /// The modifiers the seat currently holds.
    ///
    /// Held by the window-manager router, which every modifier edge reaches
    /// whatever the pointer is over, so the desktop has one answer to stamp
    /// onto the pointer events it delivers.
    #[must_use]
    pub const fn modifiers(&self) -> Modifiers {
        self.wm.modifiers()
    }

    /// Route one input `event` to the surface holding the pointer, resolving
    /// any time-driven taskbar gesture against the monotonic `now_ns`, and
    /// return what changed.
    ///
    /// Every pointer event takes the same three steps, in this order, and
    /// there is no fourth: **resolve** who holds the pointer (a modal surface
    /// of the bar's, an in-flight button grab, or whatever the compositor
    /// draws under the pointer), **move the focus** there — leaving the
    /// surface it left, entering the one it reached — and **deliver** the
    /// event to that one router. Keys are the keyboard's, not the pointer's,
    /// and go to the window manager unless a modal taskbar surface is open.
    /// See the [module docs](self) for the reasoning behind each.
    ///
    /// `presenter` is the taskbar presenter that placed the bar's compositor
    /// windows: it is what tells "the bar is under the pointer" from "a
    /// window covering the bar is". It is read, never driven — routing an
    /// event presents nothing.
    pub fn handle(
        &mut self,
        event: InputEvent,
        compositor: &mut Compositor,
        taskbar: &mut Taskbar,
        presenter: &TaskbarPresenter,
        now_ns: u64,
    ) -> SessionInputResponse {
        // The taskbar hit-tests at the output's density, which the compositor
        // owns; the seat reads it here rather than keeping its own copy.
        let scale = compositor.scale();
        // A modifier edge is neither a key any surface receives nor a pointer
        // event: it is seat state, so it always reaches the window manager,
        // which holds the one copy the desktop stamps onto what it routes.
        if matches!(event, InputEvent::ModifiersChanged { .. }) {
            return wm_response(self.wm.handle(event, compositor));
        }
        // The keyboard has a focus of its own, and the pointer does not decide
        // it: a pointer resting on the bar must never divert a keystroke from
        // the window the user is typing in. A modal surface of the bar's takes
        // the keys because it *is* what the keyboard is for while it is up.
        if matches!(
            event,
            InputEvent::KeyPressed { .. } | InputEvent::KeyReleased { .. }
        ) {
            if modal(taskbar) {
                return taskbar_response(self.taskbar.handle(event, taskbar, scale, now_ns));
            }
            return wm_response(self.wm.handle(event, compositor));
        }
        // The device moved the pointer: the seat owns that position, and every
        // resolution below is against the new one.
        if let InputEvent::PointerMoved { to } = event {
            self.pointer = to;
        }
        let owner = self.owner(compositor, taskbar, presenter);
        // A press takes the implicit grab before it is delivered, so the
        // release that ends the gesture cannot be resolved anywhere else even
        // if the stack changes under it (this press may well have raised a
        // window over the pointer).
        if let InputEvent::PointerPressed { button } = event {
            self.buttons |= button_bit(button);
            self.grab = Some(owner);
        }
        self.focus_on(Some(owner), compositor, taskbar, scale);
        let response = match owner {
            PointerOwner::Chrome => {
                taskbar_response(self.taskbar.handle(event, taskbar, scale, now_ns))
            }
            PointerOwner::Windows => wm_response(self.wm.handle(event, compositor)),
        };
        // The last button coming up releases the pointer, which is very often
        // not over the surface the gesture started on — a window dragged so
        // its title bar ends under the bar, a capsule press slid onto a
        // window. Re-resolving here is what hands it back, and what lets the
        // hover under it appear without waiting for a motion.
        if let InputEvent::PointerReleased { button } = event {
            self.buttons &= !button_bit(button);
            if self.buttons == 0 && self.grab.take().is_some() {
                self.refresh_pointer_focus(compositor, taskbar, presenter);
            }
        }
        response
    }

    /// Re-resolve which surface the pointer rests on, moving the focus if the
    /// answer changed.
    ///
    /// The answer depends on the window stack, so it goes stale whenever the
    /// stack does — a window opened, closed, raised, moved, hidden, or a
    /// popover of the bar's placed or removed — and none of those is a pointer
    /// event. The desktop therefore calls this wherever it brings the screen
    /// up to date, so a hover is never left showing on a surface something
    /// else is now drawn over, and a surface the pointer has just been
    /// *revealed* on shows its hover without the user having to jiggle the
    /// pointer to provoke it.
    ///
    /// It resolves and nothing more: no event is delivered, no gesture
    /// resolved, no window raised. An in-flight grab pins the answer, so this
    /// cannot take the pointer away from a drag mid-gesture.
    pub fn refresh_pointer_focus(
        &mut self,
        compositor: &mut Compositor,
        taskbar: &mut Taskbar,
        presenter: &TaskbarPresenter,
    ) {
        let scale = compositor.scale();
        let owner = self.owner(compositor, taskbar, presenter);
        self.focus_on(Some(owner), compositor, taskbar, scale);
    }

    /// Give up the pointer: end any in-flight button gesture and tell both
    /// routers the pointer has left.
    ///
    /// The embedder calls this when it takes the input stream away from the
    /// seat for a modal surface of its own — the screen lock, or the pinboard's
    /// backdrop menu — both of which drain the seat's channels straight into
    /// themselves so that nothing behind the plate can be reached.
    ///
    /// It is needed because the seat's implicit grab is a function of the
    /// presses and releases it *sees*. A gesture in flight when the stream is
    /// taken away ends with a release the seat will never be given, so without
    /// this the grab would be held for a button that can never come up and the
    /// pointer could never be resolved against the stack again. Dropping the
    /// focus at the same time is what stops the bar sitting there with a lit
    /// control under a lock screen.
    ///
    /// Idempotent, so the drain that takes the stream can simply say it on
    /// every pass rather than reasoning about which pass was the first. The
    /// stream coming back needs no announcement: the next event resolves the
    /// pointer from the stack, and the next press starts a fresh gesture.
    pub fn yield_pointer(&mut self, compositor: &mut Compositor, taskbar: &mut Taskbar) {
        let scale = compositor.scale();
        self.buttons = 0;
        self.grab = None;
        self.focus_on(None, compositor, taskbar, scale);
    }

    /// Which surface holds the pointer right now, in precedence order.
    ///
    /// 1. **A modal surface of the bar's** — its context menu or its
    ///    program-library popup — holds an active grab: the whole stream is
    ///    the bar's until it closes, which is what makes a press anywhere a
    ///    dismissal and keeps the windows beneath a modal surface untouched.
    /// 2. **An in-flight button grab** holds it for the rest of the gesture,
    ///    so a drag that leaves the surface it started on still completes
    ///    there and a release is never claimed by a surface that never saw the
    ///    press.
    /// 3. **Otherwise the stack**: the surface the compositor draws under the
    ///    pointer. A window the [`TaskbarPresenter`] placed is the bar's;
    ///    anything else — an application window, a surface the session placed
    ///    itself, or no window at all — is the window manager's.
    fn owner(
        &self,
        compositor: &Compositor,
        taskbar: &Taskbar,
        presenter: &TaskbarPresenter,
    ) -> PointerOwner {
        if modal(taskbar) {
            return PointerOwner::Chrome;
        }
        if let Some(grab) = self.grab {
            return grab;
        }
        match compositor.window_at(self.pointer) {
            Some(top) if presenter.owns_window(top) => PointerOwner::Chrome,
            _ => PointerOwner::Windows,
        }
    }

    /// Move the pointer's focus to `owner`, telling the surface it left and
    /// the surface it reached. A focus that has not moved tells nobody
    /// anything.
    ///
    /// This is the **only** place either router is told about the pointer's
    /// focus, so the two can never both believe they hold it, and neither can
    /// be left believing it after the pointer has gone. The leave comes first:
    /// the hover being dropped and the hover being taken up are one crossing,
    /// and doing them in the other order would show both at once for the width
    /// of a frame.
    fn focus_on(
        &mut self,
        owner: Option<PointerOwner>,
        compositor: &mut Compositor,
        taskbar: &mut Taskbar,
        scale: Scale,
    ) {
        if owner == self.focus {
            return;
        }
        match self.focus {
            Some(PointerOwner::Chrome) => {
                self.taskbar
                    .set_pointer_focus(PointerFocus::Left, taskbar, scale);
            }
            Some(PointerOwner::Windows) => {
                self.wm.set_pointer_focus(PointerFocus::Left, compositor);
            }
            None => {}
        }
        self.focus = owner;
        let entered = PointerFocus::Entered { at: self.pointer };
        match owner {
            Some(PointerOwner::Chrome) => self.taskbar.set_pointer_focus(entered, taskbar, scale),
            Some(PointerOwner::Windows) => self.wm.set_pointer_focus(entered, compositor),
            None => {}
        }
    }
}

/// Whether one of the bar's modal surfaces is open, and therefore holds an
/// active grab on both the pointer and the keyboard.
///
/// One definition, read by the pointer's resolution and the keyboard's alike:
/// a menu that took the keys but not the clicks (or the reverse) would be a
/// modal surface only some of the time.
fn modal(taskbar: &Taskbar) -> bool {
    taskbar.library().is_open()
}

/// Wrap a taskbar router outcome, collapsing its no-op to
/// [`SessionInputResponse::Ignored`].
fn taskbar_response(response: TaskbarResponse) -> SessionInputResponse {
    match response {
        TaskbarResponse::Ignored => SessionInputResponse::Ignored,
        acted => SessionInputResponse::Taskbar(acted),
    }
}

/// Wrap a window-manager router outcome, collapsing its no-op to
/// [`SessionInputResponse::Ignored`].
fn wm_response(response: InputResponse) -> SessionInputResponse {
    match response {
        InputResponse::Ignored => SessionInputResponse::Ignored,
        acted => SessionInputResponse::WindowManager(acted),
    }
}
