//! The running-task list shown in the middle of the taskbar.
//!
//! There is one [`TaskEntry`] per top-level window. At most one task is
//! *focused* (the active window). A task is independently *minimised* or
//! not. Clicking a task entry follows the familiar taskbar rule
//! ([`TaskList::activate`]): clicking the focused, non-minimised task
//! minimises it; clicking any other task (or a minimised one) restores and
//! focuses it. The list also remembers the [`previous`](TaskList::previous)
//! task — the one focused immediately before the last handover to a
//! different task — the MRU-of-two behind the Switchboard capsule's
//! middle-click switch.

use alloc::string::String;
use alloc::vec::Vec;

/// A stable identifier for a task, matching the window manager's top-level
/// window id.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash, Ord, PartialOrd)]
pub struct TaskId(pub u64);

/// One running task: the window id, its title, and whether it is minimised.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TaskEntry {
    /// The task's window id.
    pub id: TaskId,
    /// The window title shown on the entry.
    pub title: String,
    /// `true` when the window is minimised (hidden but still running).
    pub minimised: bool,
}

/// What [`TaskList::activate`] did, so the caller can drive the window
/// manager accordingly.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum ActivateOutcome {
    /// The task is now focused and restored (raise + activate it).
    Activated,
    /// The previously focused task was minimised (hide it).
    Minimised,
    /// No task has that id; nothing changed.
    Unknown,
}

/// The ordered list of running tasks plus the focused one and the
/// previous-task memory.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct TaskList {
    entries: Vec<TaskEntry>,
    focused: Option<TaskId>,
    previous: Option<TaskId>,
}

impl TaskList {
    /// An empty task list.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            entries: Vec::new(),
            focused: None,
            previous: None,
        }
    }

    /// The tasks in display order.
    #[must_use]
    pub fn entries(&self) -> &[TaskEntry] {
        &self.entries
    }

    /// The number of tasks.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// `true` when no task is running.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The currently focused task, if any.
    #[must_use]
    pub const fn focused(&self) -> Option<TaskId> {
        self.focused
    }

    /// The task focused immediately before the last handover of focus to a
    /// *different* task — the middle-click "switch to the previous task"
    /// target (the classic MRU-of-two: switching between two tasks toggles
    /// them).
    ///
    /// Updated only when focus actually moves to a different task, never by
    /// a re-focus of the current one or a handover to the desktop
    /// ([`set_focused`](Self::set_focused)`(None)`), and cleared when the
    /// remembered task closes. `None` also when the last handover arrived
    /// from the desktop rather than from another task.
    #[must_use]
    pub const fn previous(&self) -> Option<TaskId> {
        self.previous
    }

    /// Mirror the window manager's focus into the list.
    ///
    /// The window manager owns keyboard focus and changes it when the user
    /// clicks a window directly (not through the bar), so the session glue
    /// relays that focus here to keep the highlighted entry in step. Focusing a
    /// task also restores it (a focused window is never minimised).
    ///
    /// Passing `None` clears the highlight (focus rests on the desktop).
    /// Passing an unknown id changes nothing and returns `false` (fail closed); the focus and minimised state are left untouched.
    pub fn set_focused(&mut self, id: Option<TaskId>) -> bool {
        match id {
            None => {
                self.focused = None;
                true
            }
            Some(id) => {
                let Some(index) = self.position(id) else {
                    return false;
                };
                if self.focused != Some(id) {
                    self.previous = self.focused;
                }
                self.entries[index].minimised = false;
                self.focused = Some(id);
                true
            }
        }
    }

    /// Add a new task for a freshly opened window.
    ///
    /// A duplicate id changes nothing and returns `false` (fail closed); the window manager assigns unique ids, so a clash
    /// signals a bug rather than a benign retry.
    pub fn add(&mut self, id: TaskId, title: impl Into<String>) -> bool {
        if self.position(id).is_some() {
            return false;
        }
        self.entries.push(TaskEntry {
            id,
            title: title.into(),
            minimised: false,
        });
        true
    }

    /// Retitle the task with `id`, relabelling its entry.
    ///
    /// The window manager relays an owner's retitle here so the bar's
    /// label tracks the window's own title bar. Returns `false`, changing
    /// nothing, for an unknown id (fail closed); the order, focus, and
    /// minimised state are untouched — only the label moves.
    pub fn retitle(&mut self, id: TaskId, title: impl Into<String>) -> bool {
        let Some(index) = self.position(id) else {
            return false;
        };
        self.entries[index].title = title.into();
        true
    }

    /// Remove the task for a closed window, clearing focus if it held it.
    /// Returns `false` for an unknown id.
    pub fn remove(&mut self, id: TaskId) -> bool {
        let Some(index) = self.position(id) else {
            return false;
        };
        self.entries.remove(index);
        if self.focused == Some(id) {
            self.focused = None;
        }
        if self.previous == Some(id) {
            self.previous = None;
        }
        true
    }

    /// Apply the click-to-activate / minimise rule to the task with `id`.
    pub fn activate(&mut self, id: TaskId) -> ActivateOutcome {
        let Some(index) = self.position(id) else {
            return ActivateOutcome::Unknown;
        };
        if self.focused == Some(id) && !self.entries[index].minimised {
            self.entries[index].minimised = true;
            self.focused = None;
            ActivateOutcome::Minimised
        } else {
            if self.focused != Some(id) {
                self.previous = self.focused;
            }
            self.entries[index].minimised = false;
            self.focused = Some(id);
            ActivateOutcome::Activated
        }
    }

    /// Minimise the task with `id` unconditionally, dropping focus if it
    /// held it.
    ///
    /// This is the window-manager-driven counterpart to [`activate`]'s
    /// toggle: the title-bar minimize control (and any other direct
    /// "minimise this window" request) minimises regardless of the current
    /// focus/minimised state, where [`activate`] toggles. An already
    /// minimised task stays minimised. Returns `false`, changing nothing,
    /// for an unknown id (fail closed).
    ///
    /// [`activate`]: Self::activate
    pub fn minimise(&mut self, id: TaskId) -> bool {
        let Some(index) = self.position(id) else {
            return false;
        };
        self.entries[index].minimised = true;
        if self.focused == Some(id) {
            self.focused = None;
        }
        true
    }

    /// `true` when the task with `id` is minimised. Unknown ids are not
    /// minimised.
    #[must_use]
    pub fn is_minimised(&self, id: TaskId) -> bool {
        self.position(id)
            .is_some_and(|index| self.entries[index].minimised)
    }

    fn position(&self, id: TaskId) -> Option<usize> {
        self.entries.iter().position(|e| e.id == id)
    }
}
