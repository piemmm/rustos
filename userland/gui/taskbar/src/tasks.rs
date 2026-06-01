//! The running-task list shown in the middle of the taskbar.
//!
//! There is one [`TaskEntry`] per top-level window. At most one task is
//! *focused* (the active window). A task is independently *minimised* or
//! not. Clicking a task entry follows the familiar taskbar rule
//! ([`TaskList::activate`]): clicking the focused, non-minimised task
//! minimises it; clicking any other task (or a minimised one) restores and
//! focuses it.

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

/// The ordered list of running tasks plus the focused one.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct TaskList {
    entries: Vec<TaskEntry>,
    focused: Option<TaskId>,
}

impl TaskList {
    /// An empty task list.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            entries: Vec::new(),
            focused: None,
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

    /// Add a new task for a freshly opened window.
    ///
    /// A duplicate id changes nothing and returns `false` (fail closed,
    /// `AGENTS.md` §2.9); the window manager assigns unique ids, so a clash
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
            self.entries[index].minimised = false;
            self.focused = Some(id);
            ActivateOutcome::Activated
        }
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
