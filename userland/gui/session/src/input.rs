//! Fanning one input-event stream to the taskbar and the window manager.
//!
//! The desktop has two independent input routers — the window manager's
//! [`InputRouter`] (focus, click-to-activate, interactive move-grabs) and the
//! taskbar's [`TaskbarInput`] (the launcher buttons, the program-library
//! popup, task activate/minimise, the notification popover, and the clock) —
//! and both consume the **same** shared `tairix_input` (`lib/input`) event
//! vocabulary.
//! A real input source produces one stream of events, so something must
//! decide which router each event belongs to. Neither GUI crate may depend
//! on the other, so that decision is session glue, and
//! [`SessionInputRouter`] is that glue.
//!
//! The policy is deliberately simple — one event does exactly one thing:
//!
//! * **While the program-library popup is open it is modal**: every press
//!   (any button), release, scroll, and key event routes to the taskbar,
//!   which drives the popup — selecting rows, editing the search, working
//!   the scrollbar, or dismissing on a click-away. Nothing leaks to the
//!   windows beneath a modal popup.
//! * **Otherwise the taskbar claims a press** when the pointer lands on the
//!   bar *or* on one of its open, non-modal surfaces (the hover window
//!   picker, the notification popover, and the Switchboard capsule's
//!   instrument readout) **and that surface is what is drawn there**; every
//!   other press goes to the window manager. The
//!   two never both act on one press, so a click on a picker cell or a
//!   notification card never also activates a window behind it. Each of
//!   those surfaces opens outward from the bar and never overlaps it, so the
//!   taskbar surfaces never contend for a press.
//! * **A window over the bar owns the presses on it.** Nothing pins the bar
//!   topmost — it is an ordinary compositor window, and a window dragged over
//!   it is genuinely in front of it — so the bar's geometry containing the
//!   pointer is not enough. The claim is gated on the window the compositor
//!   finds on top there being one the taskbar presenter placed
//!   ([`TaskbarPresenter::owns_window`]), which is what stops a covered clock
//!   or launcher from swallowing a click meant for the window above it.
//!   The test is per press position, not per bar: a window covering the
//!   trailing end leaves every button it does not cover still the bar's.
//! * **A middle press routes to the taskbar over the bar or a popover**
//!   (over the Switchboard capsule it switches to the previous task) and is
//!   ignored elsewhere — the window manager has no middle-button action.
//! * **A scroll over the Switchboard capsule or its open readout routes to
//!   the taskbar** (it cycles the running tasks), under the same
//!   is-it-covered test as a press; every other scroll goes
//!   to the window manager's viewport under the pointer.
//! * **Pointer motion is fanned to both routers** so their tracked pointer
//!   positions stay in step (a press is hit-tested at the last motion's
//!   position). The window manager acts on motion to drag a grabbed window;
//!   the taskbar refreshes its hover feedback and resolves a Switchboard
//!   capsule press already held past its long-press threshold, which is a
//!   real action and so outranks the drag when it fires.
//! * **A primary release is offered to the taskbar first**, because a quick
//!   press on the Switchboard capsule resolves on release; only a release
//!   the taskbar does not claim goes to the window manager, which ends an
//!   in-flight move-grab. The two can never contend: a grab exists only
//!   when the press went to the window manager, and the taskbar has no
//!   gesture in flight then.
//! * **Key events go to the window manager**, which delivers them to the
//!   focused window; the taskbar takes keyboard input only while its popup
//!   is open.
//!
//! The router holds no pixels and grants itself no authority: it owns the two
//! inner routers and drives them against the embedder's [`Compositor`] and
//! [`Taskbar`], passed in on each [`handle`](SessionInputRouter::handle).
//! Composing the taskbar and window-manager crates is the permitted
//! `userland/gui/*` edge; nothing outside `userland/gui/*` depends on
//! this glue. It never panics: every routed sub-call is itself total
//! and fails closed.

use tairix_taskbar::{Hit, Taskbar, TaskbarInput, TaskbarResponse};
use tairix_wm::{
    Compositor, InputEvent, InputResponse, InputRouter, Point, PointerButton, WindowId,
};

use crate::presenter::TaskbarPresenter;

/// What the [`SessionInputRouter`] did with one [`InputEvent`].
///
/// The session router routes each event to exactly one of the two desktop
/// routers, so its outcome is either a taskbar action, a window-manager
/// action, or nothing. A sub-router that consumed the event but changed no
/// state collapses to [`Ignored`](Self::Ignored), exactly as the underlying
/// routers report their own no-ops, so the embedder sees one uniform "nothing
/// happened" outcome.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SessionInputResponse {
    /// The event changed no desktop state (a non-primary button, or a press
    /// or motion that neither router acted on).
    Ignored,
    /// The event was routed to the taskbar, which acted on it.
    Taskbar(TaskbarResponse),
    /// The event was routed to the window manager, which acted on it.
    WindowManager(InputResponse),
}

/// Routes one pointer-event stream to the taskbar and the window manager.
///
/// It owns the window manager's [`InputRouter`] and the taskbar's
/// [`TaskbarInput`] and decides, per event, which one applies it — see the
/// [module docs](self) for the policy. Drive it through
/// [`handle`](Self::handle); start a title-bar drag through
/// [`begin_move`](Self::begin_move).
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct SessionInputRouter {
    wm: InputRouter,
    taskbar: TaskbarInput,
}

impl SessionInputRouter {
    /// A router with the pointer at the screen origin, no focus, and no
    /// in-flight grab.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The current pointer position in screen coordinates. Both inner routers
    /// track the same position, because motion is fanned to both.
    #[must_use]
    pub fn pointer(&self) -> Point {
        self.taskbar.pointer()
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
    /// the tracked pointer and the in-flight move-grab — to choose the
    /// on-screen cursor shape ([`desired_cursor`](tairix_wm::desired_cursor)).
    /// It is the window manager's router that owns that state (motion is
    /// fanned to both inner routers, so its pointer is always in step), so the
    /// controller reads it here rather than the session router keeping a
    /// second copy.
    #[must_use]
    pub const fn wm(&self) -> &InputRouter {
        &self.wm
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

    /// Route one input `event` to the taskbar or the window manager,
    /// resolving any time-driven taskbar gesture against the monotonic
    /// `now_ns`, and return what changed.
    ///
    /// While the taskbar's context menu or the program-library popup is open
    /// every event routes to the taskbar (both surfaces are modal).
    /// Otherwise a press goes to whichever router claims the pointer (the
    /// taskbar when the pointer is over the bar or one of its open popovers
    /// *and* that surface is the one drawn there, the window manager
    /// otherwise); a scroll over the Switchboard capsule
    /// or its readout cycles tasks in the taskbar while any other scroll
    /// goes to the window manager; motion is fanned to both so their
    /// pointers stay in step and the window manager can drag a grabbed
    /// window; a primary release is the taskbar's to claim before it ends a
    /// window-manager grab. See the [module docs](self) for the full policy.
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
        // owns; the session reads it here rather than
        // keeping its own copy.
        let scale = compositor.scale();
        // An open context menu or popup is modal: the whole stream is the
        // taskbar's. Motion is still *tracked* by the window manager so its
        // pointer stays in step for the moment the surface closes, but its
        // outcome is discarded — nothing may be delivered to the windows
        // beneath a modal surface, and no grab can be in flight (presses
        // never reached the window manager while one was open).
        if taskbar.menu().is_open() || taskbar.library().is_open() {
            if matches!(event, InputEvent::PointerMoved { .. }) {
                let _ = self.wm.handle(event, compositor);
            }
            return taskbar_response(self.taskbar.handle(event, taskbar, scale, now_ns));
        }
        match event {
            InputEvent::PointerMoved { .. } => {
                // Keep both routers' tracked pointer in step; the window
                // manager acts on motion (dragging a grabbed window) and the
                // taskbar refreshes its hover feedback. Motion is also when
                // a capsule press held past its threshold resolves, and that
                // is a real action: it takes the outcome, while the drag
                // still applied.
                let acted = self.taskbar.handle(event, taskbar, scale, now_ns);
                let dragged = self.wm.handle(event, compositor);
                if matches!(acted, TaskbarResponse::Ignored) {
                    wm_response(dragged)
                } else {
                    taskbar_response(acted)
                }
            }
            InputEvent::PointerPressed { button } => {
                // A press belongs to whichever surface owns the pixel under
                // the pointer: the bar claims presses over itself (a
                // secondary press there opens the menu the application under
                // it declared; a middle press over the capsule switches to
                // the previous task), an open non-modal surface — the hover
                // window picker, the notification popover, or the capsule's
                // readout — claims presses over it, and the window manager
                // takes every remaining primary or secondary press. Each of
                // those surfaces opens outward and never overlaps the bar, so
                // the taskbar surfaces never contend.
                //
                // "Owns the pixel" is the whole rule, so a window stacked
                // over the bar takes the press back: the bar is no more
                // topmost than any other window, and a click lands on what
                // the user can see.
                let pointer = self.taskbar.pointer();
                let over_taskbar = taskbar.hit_test(pointer, scale).is_some()
                    || taskbar
                        .picker_layout(scale)
                        .is_some_and(|picker| picker.panel.contains(pointer))
                    || taskbar
                        .notifications_layout(scale)
                        .is_some_and(|popover| popover.contains(pointer))
                    || taskbar
                        .tray_readout_layout(scale)
                        .is_some_and(|readout| readout.contains(pointer));
                let on_taskbar = over_taskbar && !self.covered(compositor, presenter);
                if on_taskbar {
                    return taskbar_response(self.taskbar.handle(event, taskbar, scale, now_ns));
                }
                if matches!(button, PointerButton::Primary | PointerButton::Secondary) {
                    wm_response(self.wm.handle(event, compositor))
                } else {
                    SessionInputResponse::Ignored
                }
            }
            InputEvent::PointerScrolled { .. } => {
                // A scroll over the Switchboard capsule (or its open
                // readout) cycles the running tasks; everywhere else the
                // wheel belongs to the window manager's viewport under the
                // pointer — including a capsule with a window over it, which
                // is that window's viewport and not the bar's.
                let pointer = self.taskbar.pointer();
                let over_capsule =
                    matches!(taskbar.hit_test(pointer, scale), Some(Hit::Switchboard))
                        || taskbar
                            .tray_readout_layout(scale)
                            .is_some_and(|readout| readout.contains(pointer));
                let on_capsule = over_capsule && !self.covered(compositor, presenter);
                if on_capsule {
                    taskbar_response(self.taskbar.handle(event, taskbar, scale, now_ns))
                } else {
                    wm_response(self.wm.handle(event, compositor))
                }
            }
            // A primary release is the taskbar's first: a quick press on the
            // Switchboard capsule resolves here. One it does not claim ends
            // an in-flight move-grab in the window manager instead.
            InputEvent::PointerReleased {
                button: PointerButton::Primary,
            } => match self.taskbar.handle(event, taskbar, scale, now_ns) {
                TaskbarResponse::Ignored => wm_response(self.wm.handle(event, compositor)),
                acted => SessionInputResponse::Taskbar(acted),
            },
            // Keys go to the window manager, which delivers them to the
            // focused window; the taskbar takes keyboard input only while one
            // of its modal surfaces is open, handled above.
            InputEvent::KeyPressed { .. } | InputEvent::KeyReleased { .. } => {
                wm_response(self.wm.handle(event, compositor))
            }
            InputEvent::PointerReleased { .. } => SessionInputResponse::Ignored,
        }
    }

    /// Whether the pixel under the pointer belongs to a window the taskbar
    /// does not own — that is, whether something is drawn *over* the bar (or
    /// over one of its popovers) exactly where the pointer is.
    ///
    /// Nothing pins the bar topmost. It is an ordinary compositor window, and
    /// a window raised above it — dragged over it by its title bar, say —
    /// really does own the pixels the pointer is on, so the bar's own
    /// geometry containing the pointer is not enough to make a press the
    /// bar's. Without this, a window covering the clock, the launcher, or the
    /// capsule would keep taking clicks aimed at itself, and the covered
    /// control would act on a gesture the user made at something else
    /// entirely.
    ///
    /// The answer is the compositor's own top-down hit test against the
    /// window ids the [`TaskbarPresenter`] minted, so it is per pointer
    /// position rather than per bar, and it needs no second copy of either
    /// the stack or those ids. Nothing on top at all — the pointer is over
    /// the desktop layer, or the bar has never been presented, so there is no
    /// window there to be covered by — is not "covered": the taskbar's
    /// geometry alone then decides, as it always did.
    fn covered(&self, compositor: &Compositor, presenter: &TaskbarPresenter) -> bool {
        compositor
            .window_at(self.taskbar.pointer())
            .is_some_and(|top| !presenter.owns_window(top))
    }
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
