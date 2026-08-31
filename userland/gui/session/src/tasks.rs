//! Keeping the taskbar's running-task list in step with the window stack.
//!
//! The taskbar models a window registry — one
//! [`TaskEntry`](tairix_taskbar::TaskEntry) per top-level window, read by
//! its hover window picker and its Switchboard capsule — but it owns no
//! window manager, and the window manager owns no task list. So joining the
//! two is session glue, and [`TaskBridge`] is that glue.
//!
//! A task is named by a [`TaskId`] and a window by a
//! [`WindowId`]; the window manager mints the latter as an
//! *opaque* token, so the bridge owns the correspondence between the two rather
//! than reaching into either id. It mints a stable [`TaskId`] for each window
//! it is told about and translates between the two whenever the taskbar acts on
//! a window or the window manager moves focus.
//!
//! The bridge performs four operations, each total and fail-closed:
//!
//! * [`open`](TaskBridge::open) adds a window to the compositor, registers it
//!   as a running task, and focuses it (a freshly opened window takes focus).
//! * [`close`](TaskBridge::close) removes a window from the compositor and its
//!   task from the bar, dropping focus if the closed window held it.
//! * [`raise`](TaskBridge::raise) shows, raises, and focuses a task's window
//!   — what choosing a cell in the bar's window picker, or the bar's own
//!   raise-the-application click, asks for.
//! * [`sync_focus`](TaskBridge::sync_focus) mirrors a window-manager focus
//!   change (the user clicked a window directly) back into the bar's highlight.
//!
//! The bridge holds no pixels and grants itself no authority: the
//! [`Compositor`], the [`SessionInputRouter`], and the
//! [`Taskbar`] are the embedder's, passed in on each call.

use alloc::string::String;
use alloc::vec::Vec;

use tairix_taskbar::{TaskId, Taskbar};
use tairix_wm::{Compositor, Point, Surface, WindowId};

use crate::input::SessionInputRouter;

/// Owns the correspondence between compositor windows and taskbar tasks.
///
/// Each tracked top-level window has exactly one task; the bridge mints a
/// stable [`TaskId`] per window and never reuses one, so a task id always names
/// the same window for its lifetime. See the [module docs](self) for the
/// operations.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct TaskBridge {
    tasks: Vec<(WindowId, TaskId)>,
    next: u64,
}

impl TaskBridge {
    /// A bridge tracking no windows.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            tasks: Vec::new(),
            next: 0,
        }
    }

    /// The number of tracked windows (one task each).
    #[must_use]
    pub fn len(&self) -> usize {
        self.tasks.len()
    }

    /// `true` when no window is tracked.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.tasks.is_empty()
    }

    /// The task id for `window`, or `None` when it is not tracked.
    #[must_use]
    pub fn task_for(&self, window: WindowId) -> Option<TaskId> {
        self.tasks
            .iter()
            .find(|&&(w, _)| w == window)
            .map(|&(_, t)| t)
    }

    /// The window for `task`, or `None` when no tracked window has that id.
    #[must_use]
    pub fn window_for(&self, task: TaskId) -> Option<WindowId> {
        self.tasks
            .iter()
            .find(|&&(_, t)| t == task)
            .map(|&(w, _)| w)
    }

    /// Open `surface` as a top-level window at `origin`, register it as a
    /// running task titled `title`, and focus it.
    ///
    /// Returns the new [`WindowId`]. Returns `None`,
    /// changing nothing, only if the task-id space is exhausted — a window the
    /// bridge cannot name as a task would drift from the bar, so it is not
    /// opened (fail closed).
    pub fn open(
        &mut self,
        compositor: &mut Compositor,
        router: &mut SessionInputRouter,
        taskbar: &mut Taskbar,
        origin: Point,
        surface: Surface,
        title: impl Into<String>,
    ) -> Option<WindowId> {
        let task = TaskId(self.next);
        let next = self.next.checked_add(1)?;
        let window = compositor.add_window(origin, surface);
        self.next = next;
        self.tasks.push((window, task));
        taskbar.tasks_mut().add(task, title);
        focus_window(compositor, router, taskbar, window, task);
        Some(window)
    }

    /// Retitle `window`: relabel its taskbar entry and, when it wears the
    /// window-manager frame, its title bar — both from this one call, so the
    /// two can never show different names.
    ///
    /// A tracked window that carries no title bar (the session's own
    /// undecorated trusted surfaces) still relabels on the bar: the entry is
    /// the task's label, not the decoration's. Returns `false`, changing
    /// nothing, when `window` is not tracked.
    pub fn retitle(
        &mut self,
        compositor: &mut Compositor,
        taskbar: &mut Taskbar,
        window: WindowId,
        title: &str,
    ) -> bool {
        let Some(task) = self.task_for(window) else {
            return false;
        };
        compositor.set_window_title(window, title);
        taskbar.tasks_mut().retitle(task, title)
    }

    /// Close `window`: remove it from the compositor and its task from the bar,
    /// dropping focus if it held it.
    ///
    /// Returns `false`, changing nothing, when `window` is not tracked.
    pub fn close(
        &mut self,
        compositor: &mut Compositor,
        router: &mut SessionInputRouter,
        taskbar: &mut Taskbar,
        window: WindowId,
    ) -> bool {
        let Some(index) = self.tasks.iter().position(|&(w, _)| w == window) else {
            return false;
        };
        let (_, task) = self.tasks.remove(index);
        taskbar.tasks_mut().remove(task);
        if router.focused() == Some(window) {
            router.unfocus();
        }
        compositor.remove(window);
        true
    }

    /// Show, raise, and focus `task`'s window.
    ///
    /// What choosing a cell in the bar's hover window picker asks for, and
    /// what a click on the slot of an application that declared no default
    /// action asks for. The taskbar has already restored the entry in its
    /// own model; this brings the window manager into step. Returns
    /// `false`, changing nothing, for a task the bridge does not track.
    pub fn raise(
        &self,
        compositor: &mut Compositor,
        router: &mut SessionInputRouter,
        task: TaskId,
    ) -> bool {
        let Some(window) = self.window_for(task) else {
            return false;
        };
        show_raise_focus(compositor, router, window)
    }

    /// Minimise `window`: mark its taskbar entry minimised, hide it in the
    /// compositor, and drop focus if it held it.
    ///
    /// Minimising is the title bar's own control — an icon-bar slot is an
    /// application, not a window, so it offers no minimise of its own — and
    /// a minimised window comes back by being chosen in the hover picker.
    /// Returns `false`, changing nothing, for a window the bridge does not
    /// track.
    pub fn minimize(
        &self,
        compositor: &mut Compositor,
        router: &mut SessionInputRouter,
        taskbar: &mut Taskbar,
        window: WindowId,
    ) -> bool {
        let Some(task) = self.task_for(window) else {
            return false;
        };
        taskbar.tasks_mut().minimise(task);
        compositor.set_visible(window, false);
        if router.focused() == Some(window) {
            router.unfocus();
        }
        true
    }

    /// Mirror a window-manager focus change into the taskbar's highlight,
    /// returning whether the highlight actually moved.
    ///
    /// The window manager focuses a window when the user clicks it directly;
    /// the bar's running-task list highlights the focused task, so this relays
    /// the new focus. Passing `None` (a press on the desktop) clears the
    /// highlight. A focused window the bridge does not track — the bar's own
    /// surface, say — leaves the highlight untouched and returns `false`, so a click the window manager handled but that owns
    /// no task neither blanks the highlight nor forces a needless repaint.
    pub fn sync_focus(&self, taskbar: &mut Taskbar, window: Option<WindowId>) -> bool {
        let target = match window {
            None => None,
            Some(window) => match self.task_for(window) {
                Some(task) => Some(task),
                None => return false,
            },
        };
        // Reading first and writing only on a real move keeps a click that
        // lands on the already-focused window from latching a bar repaint:
        // handing out the mutable task list is indistinguishable from
        // changing it, so the taskbar must assume the worst.
        if taskbar.tasks().focused() == target {
            return false;
        }
        taskbar.tasks_mut().set_focused(target);
        true
    }
}

/// Show, raise, and focus `window`/`task` and highlight the task on the bar —
/// what [`TaskBridge::open`] does for a freshly opened window. The window
/// manager's focus and the on-screen stack are moved through the shared
/// [`show_raise_focus`] path; only the bar highlight is extra here (an
/// activated task already highlighted itself).
fn focus_window(
    compositor: &mut Compositor,
    router: &mut SessionInputRouter,
    taskbar: &mut Taskbar,
    window: WindowId,
    task: TaskId,
) {
    show_raise_focus(compositor, router, window);
    taskbar.tasks_mut().set_focused(Some(task));
}

/// Show `window`, raise it to the top of the stack, and give the keyboard to
/// the front of the family that rose — the one path shared by
/// [`TaskBridge::open`] and [`TaskBridge::raise`]. Returns whether focus moved
/// (it does not when the compositor no longer knows `window`).
///
/// The keyboard goes to the front-most window of the family rather than to
/// `window` itself, because a raise brings a window's transients up with it: a
/// settings sheet or menu its owner opened ends up *above* the owner, so
/// focusing the owner would type into a window the user cannot see the top of
/// and leave that transient's own keys — including the Escape that closes it —
/// going somewhere else. An owner with no transient open is its own family
/// front, so the ordinary case is unchanged.
fn show_raise_focus(
    compositor: &mut Compositor,
    router: &mut SessionInputRouter,
    window: WindowId,
) -> bool {
    compositor.set_visible(window, true);
    compositor.raise(window);
    let front = compositor.family_front(window).unwrap_or(window);
    router.focus(front, compositor)
}
