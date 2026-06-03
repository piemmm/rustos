//! Fanning one pointer-event stream to the taskbar and the window manager.
//!
//! The desktop has two independent input routers — the window manager's
//! [`InputRouter`] (focus, click-to-activate, interactive move-grabs) and the
//! taskbar's [`TaskbarInput`] (start-menu toggle, task activate/minimise,
//! notification/clock presses) — and both consume the **same** shared
//! `rustos_input` (`lib/input`) event vocabulary (`AGENTS.md` §17.4, §2.2). A
//! real input source produces one stream of events, so something must decide
//! which router each event belongs to. Neither GUI crate may depend on the
//! other (§17.4), so that decision is session glue, and [`SessionInputRouter`]
//! is that glue.
//!
//! The policy is deliberately simple — one event does exactly one thing
//! (`AGENTS.md` §2.1):
//!
//! * **The taskbar claims a primary press** when its start menu is open (the
//!   menu is modal, so a press anywhere selects an entry or dismisses it) or
//!   when the pointer lands on the bar; otherwise the press goes to the window
//!   manager. The two never both act on one press, so a click on the bar never
//!   also activates a window behind it.
//! * **Pointer motion is fanned to both routers** so their tracked pointer
//!   positions stay in step (a press is hit-tested at the last motion's
//!   position). Only the window manager acts on motion — to drag a grabbed
//!   window — so a motion's outcome is the window manager's.
//! * **A primary release goes to the window manager**, which ends an
//!   in-flight move-grab; the taskbar ignores releases.
//!
//! The router holds no pixels and grants itself no authority: it owns the two
//! inner routers and drives them against the embedder's [`Compositor`] and
//! [`Taskbar`], passed in on each [`handle`](SessionInputRouter::handle).
//! Composing the taskbar and window-manager crates is the permitted
//! `userland/gui/*` edge (§17.4); nothing outside `userland/gui/*` depends on
//! this glue (§17.3). It never panics: every routed sub-call is itself total
//! and fails closed (`AGENTS.md` §2.9).

use rustos_taskbar::{Taskbar, TaskbarInput, TaskbarResponse};
use rustos_wm::{
    Compositor, InputEvent, InputResponse, InputRouter, Point, PointerButton, Scale, WindowId,
};

/// What the [`SessionInputRouter`] did with one [`InputEvent`].
///
/// The session router routes each event to exactly one of the two desktop
/// routers, so its outcome is either a taskbar action, a window-manager
/// action, or nothing. A sub-router that consumed the event but changed no
/// state collapses to [`Ignored`](Self::Ignored), exactly as the underlying
/// routers report their own no-ops, so the embedder sees one uniform "nothing
/// happened" outcome.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
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

    /// `true` while an interactive window move-grab is in progress.
    #[must_use]
    pub fn is_moving(&self) -> bool {
        self.wm.is_moving()
    }

    /// Start an interactive move-grab on the focused window, anchored at the
    /// current pointer position. Returns `false` (starting no grab) when there
    /// is no focused window or it is no longer known to `compositor`
    /// (`AGENTS.md` §2.9). Decorations call this on a title-bar press; the
    /// subsequent motion then drags the window.
    pub fn begin_move(&mut self, compositor: &Compositor) -> bool {
        self.wm.begin_move(compositor)
    }

    /// Route one input `event` to the taskbar or the window manager, returning
    /// what changed.
    ///
    /// A primary press goes to whichever router claims the pointer (the
    /// taskbar when its menu is open or the pointer is over the bar, the
    /// window manager otherwise); motion is fanned to both so their pointers
    /// stay in step and the window manager can drag a grabbed window; a
    /// release goes to the window manager to end a grab. See the
    /// [module docs](self) for the full policy.
    pub fn handle(
        &mut self,
        event: InputEvent,
        compositor: &mut Compositor,
        taskbar: &mut Taskbar,
    ) -> SessionInputResponse {
        // The taskbar hit-tests at the output's density, which the compositor
        // owns (`AGENTS.md` §10); the session reads it here rather than
        // keeping its own copy (§2.2).
        let scale = compositor.scale();
        match event {
            InputEvent::PointerMoved { .. } => {
                // Keep both routers' tracked pointer in step; only the window
                // manager acts on motion (dragging a grabbed window).
                self.taskbar.handle(event, taskbar, scale);
                wm_response(self.wm.handle(event, compositor))
            }
            InputEvent::PointerPressed {
                button: PointerButton::Primary,
            } => {
                if taskbar_claims(taskbar, self.taskbar.pointer(), scale) {
                    taskbar_response(self.taskbar.handle(event, taskbar, scale))
                } else {
                    wm_response(self.wm.handle(event, compositor))
                }
            }
            InputEvent::PointerReleased {
                button: PointerButton::Primary,
            } => wm_response(self.wm.handle(event, compositor)),
            InputEvent::PointerPressed { .. } | InputEvent::PointerReleased { .. } => {
                SessionInputResponse::Ignored
            }
        }
    }
}

/// `true` when the taskbar should receive a primary press at `pointer`: its
/// start menu is open (modal), or the pointer is over the bar laid out at the
/// output `scale`.
fn taskbar_claims(taskbar: &Taskbar, pointer: Point, scale: Scale) -> bool {
    taskbar.start_menu().is_open() || taskbar.hit_test(pointer, scale).is_some()
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
