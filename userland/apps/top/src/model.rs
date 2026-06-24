//! The `top` view model: the process snapshot plus the cursor, scroll
//! position, scope, and help-overlay state, and how an input [`Event`] moves
//! them.
//!
//! The model is deliberately free of any terminal I/O so it is exhaustively
//! testable on the host: the [`crate::app`] glue feeds it
//! events and asks [`crate::app::render`] to draw it. All of curses' geometry
//! quirks (clamping a selection, keeping it on screen) live here once.

use alloc::vec::Vec;

use rustos_abi::sysinfo::ProcessRecord;
use rustos_curses::Event;
use rustos_procinfo::{for_each_process, Transport};

use crate::error::TopError;

/// Which processes the viewer is showing.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum Scope {
    /// The caller's own processes (the ungated `SELF_PROCESS_LIST`).
    Own,
    /// Every process system-wide (`GLOBAL_PROCESS_LIST`, which the service
    /// gates on `CAP_SYSINFO_GLOBAL`).
    All,
}

impl Scope {
    /// Whether this scope is the system-wide view.
    #[must_use]
    pub const fn is_all(self) -> bool {
        matches!(self, Scope::All)
    }

    /// A short human label for the status line.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Scope::Own => "own",
            Scope::All => "all",
        }
    }
}

/// What the [`crate::app`] loop should do after handling an event.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum Action {
    /// Nothing changed; no redraw is required.
    Ignore,
    /// View state changed; redraw from the existing snapshot.
    Redraw,
    /// The snapshot is stale (the scope changed or a refresh was asked for);
    /// re-query the service, then redraw.
    Refresh,
    /// The user asked to quit.
    Quit,
}

/// The live `top` view state.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Model {
    processes: Vec<ProcessRecord>,
    scope: Scope,
    selected: usize,
    top: usize,
    viewport: usize,
    show_help: bool,
}

impl Model {
    /// A fresh, empty model for the given `scope`. Call [`Model::refresh`] to
    /// populate it.
    #[must_use]
    pub fn new(scope: Scope) -> Model {
        Model {
            processes: Vec::new(),
            scope,
            selected: 0,
            top: 0,
            viewport: 1,
            show_help: false,
        }
    }

    /// Re-query the process list for the current [`Scope`] and adopt the
    /// fresh snapshot, clamping the selection into the new bounds.
    ///
    /// # Errors
    ///
    /// * [`TopError::PermissionDenied`] — the system-wide view was refused
    ///   for want of `CAP_SYSINFO_GLOBAL`.
    /// * [`TopError::Service`] — the transport failed or the reply did not
    ///   decode against `sysinfo-v1`.
    pub fn refresh(&mut self, transport: &dyn Transport) -> Result<(), TopError> {
        let mut next = Vec::new();
        for_each_process(transport, self.scope.is_all(), |record| {
            next.push(*record);
            Ok(())
        })?;
        self.processes = next;
        self.clamp_selection();
        Ok(())
    }

    /// The processes currently held.
    #[must_use]
    pub fn processes(&self) -> &[ProcessRecord] {
        &self.processes
    }

    /// The current scope.
    #[must_use]
    pub const fn scope(&self) -> Scope {
        self.scope
    }

    /// The index of the selected process, or `None` when the list is empty.
    #[must_use]
    pub fn selected(&self) -> Option<usize> {
        if self.processes.is_empty() {
            None
        } else {
            Some(self.selected)
        }
    }

    /// The index of the first process row drawn (the scroll offset).
    #[must_use]
    pub const fn scroll_top(&self) -> usize {
        self.top
    }

    /// Whether the help overlay is showing.
    #[must_use]
    pub const fn help_visible(&self) -> bool {
        self.show_help
    }

    /// Tell the model how many process rows fit on screen, so it can keep the
    /// selection visible. A zero is treated as one row.
    pub fn set_viewport(&mut self, rows: usize) {
        self.viewport = rows.max(1);
        self.scroll_into_view();
    }

    /// Handle one decoded input [`Event`] and report what the loop should do.
    pub fn handle_event(&mut self, event: &Event) -> Action {
        match event {
            Event::Char('q' | 'Q') => Action::Quit,
            Event::Up => self.move_selection(-1),
            Event::Down => self.move_selection(1),
            Event::PageUp => self.move_selection(-(self.page_step())),
            Event::PageDown => self.move_selection(self.page_step()),
            Event::Home => self.move_to(0),
            Event::End => self.move_to(self.processes.len().saturating_sub(1)),
            Event::Char('a' | 'A') => self.toggle_scope(),
            Event::Char('r' | 'R') => Action::Refresh,
            Event::Char('?' | 'h') => {
                self.show_help = !self.show_help;
                Action::Redraw
            }
            _ => Action::Ignore,
        }
    }

    /// One page of movement: a viewport's worth of rows.
    fn page_step(&self) -> isize {
        isize::try_from(self.viewport).unwrap_or(1).max(1)
    }

    /// Flip between the own-processes and system-wide views, asking for a
    /// re-query against the new scope.
    fn toggle_scope(&mut self) -> Action {
        self.scope = match self.scope {
            Scope::Own => Scope::All,
            Scope::All => Scope::Own,
        };
        Action::Refresh
    }

    /// Move the selection by `delta` rows (clamped to the list), keeping it
    /// on screen. Returns [`Action::Redraw`] when something moved.
    fn move_selection(&mut self, delta: isize) -> Action {
        if self.processes.is_empty() {
            return Action::Ignore;
        }
        let last = self.processes.len() - 1;
        let current = isize::try_from(self.selected).unwrap_or(0);
        let target = (current + delta).clamp(0, isize::try_from(last).unwrap_or(0));
        let target = usize::try_from(target).unwrap_or(0);
        self.move_to(target)
    }

    /// Move the selection to an absolute row, keeping it on screen.
    fn move_to(&mut self, index: usize) -> Action {
        if self.processes.is_empty() {
            return Action::Ignore;
        }
        let clamped = index.min(self.processes.len() - 1);
        if clamped == self.selected {
            return Action::Ignore;
        }
        self.selected = clamped;
        self.scroll_into_view();
        Action::Redraw
    }

    /// Clamp the selection (and scroll) into the current list bounds, used
    /// after a refresh changes the row count.
    fn clamp_selection(&mut self) {
        if self.processes.is_empty() {
            self.selected = 0;
            self.top = 0;
            return;
        }
        self.selected = self.selected.min(self.processes.len() - 1);
        self.scroll_into_view();
    }

    /// Adjust the scroll offset so the selected row lies within the viewport.
    fn scroll_into_view(&mut self) {
        if self.processes.is_empty() {
            self.top = 0;
            return;
        }
        if self.selected < self.top {
            self.top = self.selected;
        } else if self.selected >= self.top + self.viewport {
            self.top = self.selected + 1 - self.viewport;
        }
        let max_top = self.processes.len().saturating_sub(self.viewport);
        self.top = self.top.min(max_top);
    }
}
