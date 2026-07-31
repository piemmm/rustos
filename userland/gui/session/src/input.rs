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
//!   bar *or* on one of its open, non-modal popovers (the notification
//!   popover and the Switchboard capsule's instrument readout); every other
//!   press goes to the window manager. The two never both act on one press,
//!   so a click on the bar or a notification card never also activates a
//!   window behind it. The popovers open outward from the bar and never
//!   overlap it, so the taskbar surfaces never contend for a press. A press
//!   away from the bar additionally *releases* a pinned readout — pure
//!   presentation state, like hover, so the press still performs its one
//!   window-manager action.
//! * **A middle press routes to the taskbar over the bar or a popover**
//!   (over the Switchboard capsule it switches to the previous task) and is
//!   ignored elsewhere — the window manager has no middle-button action.
//! * **A scroll over the Switchboard capsule or its open readout routes to
//!   the taskbar** (it cycles the running tasks); every other scroll goes
//!   to the window manager's viewport under the pointer.
//! * **Pointer motion is fanned to both routers** so their tracked pointer
//!   positions stay in step (a press is hit-tested at the last motion's
//!   position). The window manager acts on motion to drag a grabbed window;
//!   the taskbar only refreshes its hover feedback.
//! * **A primary release goes to the window manager**, which ends an
//!   in-flight move-grab; the taskbar ignores releases while its popup is
//!   closed.
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

    /// Route one input `event` to the taskbar or the window manager, returning
    /// what changed.
    ///
    /// While the taskbar's context menu or the program-library popup is open
    /// every event routes to the taskbar (both surfaces are modal).
    /// Otherwise a press goes to whichever router claims the pointer (the
    /// taskbar when the pointer is over the bar or one of its open popovers,
    /// the window manager otherwise — releasing a pinned readout on the
    /// way); a scroll over the Switchboard capsule or its readout cycles
    /// tasks in the taskbar while any other scroll goes to the window
    /// manager; motion is fanned to both so their pointers stay in step and
    /// the window manager can drag a grabbed window; a release goes to the
    /// window manager to end a grab. See the [module docs](self) for the
    /// full policy.
    pub fn handle(
        &mut self,
        event: InputEvent,
        compositor: &mut Compositor,
        taskbar: &mut Taskbar,
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
            return taskbar_response(self.taskbar.handle(event, taskbar, scale));
        }
        match event {
            InputEvent::PointerMoved { .. } => {
                // Keep both routers' tracked pointer in step; the window
                // manager acts on motion (dragging a grabbed window) and the
                // taskbar refreshes its hover feedback.
                self.taskbar.handle(event, taskbar, scale);
                wm_response(self.wm.handle(event, compositor))
            }
            InputEvent::PointerPressed { button } => {
                // A press belongs to whichever surface owns the pixel under
                // the pointer: the bar claims presses over itself (a
                // secondary press there opens a pin's context menu; a middle
                // press over the capsule switches to the previous task), an
                // open non-modal popover — the notification popover or the
                // capsule's readout — claims presses over it, and the window
                // manager takes every remaining primary or secondary press.
                // The popovers open outward and never overlap the bar, so
                // the taskbar surfaces never contend. A press away from the
                // bar also releases a pinned readout — presentation state,
                // like hover, never the press's one action.
                let pointer = self.taskbar.pointer();
                let on_taskbar = taskbar.hit_test(pointer, scale).is_some()
                    || taskbar
                        .notifications_layout(scale)
                        .is_some_and(|popover| popover.contains(pointer))
                    || taskbar
                        .tray_readout_layout(scale)
                        .is_some_and(|readout| readout.contains(pointer));
                if on_taskbar {
                    return taskbar_response(self.taskbar.handle(event, taskbar, scale));
                }
                taskbar.release_tray_pin();
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
                // pointer.
                let pointer = self.taskbar.pointer();
                let on_capsule = matches!(taskbar.hit_test(pointer, scale), Some(Hit::Switchboard))
                    || taskbar
                        .tray_readout_layout(scale)
                        .is_some_and(|readout| readout.contains(pointer));
                if on_capsule {
                    taskbar_response(self.taskbar.handle(event, taskbar, scale))
                } else {
                    wm_response(self.wm.handle(event, compositor))
                }
            }
            // The window manager takes the rest and the taskbar none of them:
            // a primary release ends a move-grab and keys go to the focused
            // window.
            InputEvent::PointerReleased {
                button: PointerButton::Primary,
            }
            | InputEvent::KeyPressed { .. }
            | InputEvent::KeyReleased { .. } => wm_response(self.wm.handle(event, compositor)),
            InputEvent::PointerReleased { .. } => SessionInputResponse::Ignored,
        }
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
