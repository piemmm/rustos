//! Routing device input into taskbar actions.
//!
//! The [`TaskbarInput`] router turns a stream of device-level
//! [`InputEvent`]s into actions against a [`Taskbar`]: a primary-button press
//! is hit-tested against the bar's computed [`BarLayout`](crate::BarLayout)
//! and drives the model — opening the program-library popup, reporting the
//! Files button, applying the click-to-activate / minimise rule to a task,
//! or reporting a press on the clock. A press on a status signal is claimed
//! but inert (a live readout, not an action target this stage), and a press
//! on an open notification popover dismisses the card it lands on.
//!
//! It is the taskbar counterpart of the window manager's input router, and it
//! consumes the **same** shared [`tairix_input`] event vocabulary, so the
//! desktop routes one event type to both. Like that
//! router it holds no pixels, tracks the pointer position from motion events,
//! applies presses at that position, and never panics: a press that misses
//! every region changes nothing.
//!
//! While the program-library popup is open the router treats it as modal and
//! consumes the whole event stream — presses, releases, scroll, and keys all
//! route into the popup ([`LibraryPopup`](crate::LibraryPopup)); a press on
//! the Library button toggles the popup shut, and a press outside the panel
//! dismisses it (the standard click-away behaviour) without also acting on
//! what it landed on — one click does one thing. The popup's key model gives
//! every action a keyboard path; the desktop routes key events here only
//! while the popup is open, so the focused window's keys are untouched
//! otherwise.
//!
//! A secondary press opens the bar's one context surface
//! ([`BarMenu`](crate::BarMenu)): on a pinned shortcut with the popup
//! closed, or on a program-library entry row inside the open popup. While
//! the menu is open it is the top modal layer — the whole stream routes
//! into it first, and a press outside its plate dismisses only the menu
//! (one click does one thing), leaving whatever is beneath for the next
//! click.
//!
//! The Switchboard capsule at the trailing end has its own quiet
//! microinteractions: a primary press pins its readout open (and a primary
//! press anywhere else releases the pin before acting as usual — collapsing
//! an informational readout is never the click's action), a press inside
//! the open readout is claimed inert exactly like the notification
//! popover's chrome, scrolling over the capsule or its readout cycles the
//! task list, and a middle press over the capsule switches back to the
//! previous task.

use tairix_geometry::{Point, Rect, Scale};
use tairix_input::{InputEvent, PointerButton};
use tairix_proglib::EntryId;

use crate::layout::Hit;
use crate::library::{LibraryRow, PopupOutcome};
use crate::menu::{MenuChoice, MenuOutcome};
use crate::pins::PinView;
use crate::taskbar::Taskbar;
use crate::tasks::{ActivateOutcome, TaskId};

/// What a [`TaskbarInput`] event did to the taskbar.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TaskbarResponse {
    /// The event changed no state the embedder must act on. Pixel-only
    /// changes (a hover, a popup scroll or edit) latch the taskbar's repaint
    /// flag instead ([`Taskbar::take_repaint`]).
    Ignored,
    /// The Library button opened the program-library popup.
    OpenLibrary,
    /// The program-library popup closed without launching anything — the
    /// Library button toggled it shut, a press outside dismissed it, or
    /// `Escape` was pressed.
    LibraryDismissed,
    /// An entry in the program-library popup was chosen, closing the popup.
    /// The embedder resolves the entry's bundle and launches it.
    LibraryLaunch {
        /// The catalog identifier of the chosen entry.
        entry: EntryId,
    },
    /// The Files button was pressed. The embedder opens the file manager —
    /// raising an already-open files window rather than launching a second.
    OpenFiles,
    /// A task slot was pressed, applying the click-to-activate / minimise
    /// rule to that task.
    TaskActivated {
        /// The task whose slot was pressed.
        id: TaskId,
        /// What the click did, so the caller can drive the window manager.
        outcome: ActivateOutcome,
    },
    /// A raised notification's card was clicked to dismiss it. The embedder
    /// clears the notification identified by `(producer, key)` from the
    /// model — and from the session, which owns the live feed.
    DismissNotification {
        /// The dismissed notification's attested producer.
        producer: u64,
        /// The producer-chosen key naming the dismissed notification.
        key: u32,
    },
    /// The clock was pressed.
    ClockPressed,
    /// A pinned shortcut with no running window was activated. The embedder
    /// launches the pinned application (a pin whose application is already
    /// running reports [`TaskActivated`](Self::TaskActivated) instead).
    ActivatePin {
        /// The pin's strip index.
        index: usize,
    },
    /// *Unpin* was chosen for the pin at this index. The embedder removes
    /// it from the per-user pin store and re-resolves the strip.
    Unpin {
        /// The pin's strip index.
        index: usize,
    },
    /// *Pin to taskbar* was chosen for a program-library entry. The
    /// embedder appends it to the per-user pin store and re-resolves the
    /// strip.
    PinEntry {
        /// The catalog identifier of the entry to pin.
        entry: EntryId,
    },
}

/// Routes device input into [`Taskbar`] actions.
///
/// The router's only state is the current pointer position, updated by
/// [`InputEvent::PointerMoved`]; presses act at that position, exactly as a
/// real pointing device reports motion separately from clicks.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct TaskbarInput {
    pointer: Point,
}

impl TaskbarInput {
    /// Create a router with the pointer at the screen origin.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The current pointer position in screen coordinates.
    #[must_use]
    pub const fn pointer(&self) -> Point {
        self.pointer
    }

    /// Process one input `event` against `taskbar`, hit-testing at the
    /// desktop `scale` (the compositor's output density), returning what
    /// changed.
    ///
    /// With the popup and menu closed only a primary or secondary press
    /// acts; pointer motion updates the tracked position (and the bar's
    /// hover feedback), and every other event is
    /// [`TaskbarResponse::Ignored`]. With the context menu open the whole
    /// stream routes into it; with the popup open it routes there (see the
    /// [module docs](self)).
    pub fn handle(
        &mut self,
        event: InputEvent,
        taskbar: &mut Taskbar,
        scale: Scale,
    ) -> TaskbarResponse {
        if let InputEvent::PointerMoved { to } = event {
            self.pointer = to;
            taskbar.track_hover(to, scale);
        }
        if taskbar.menu().is_open() {
            return self.route_to_menu(event, taskbar, scale);
        }
        if taskbar.library().is_open() {
            return self.route_to_popup(event, taskbar, scale);
        }
        // The notification popover and the Switchboard readout are
        // non-modal: unlike the menu and library popup they do not swallow
        // the whole stream. A primary press that lands on the popover
        // dismisses the card it hits (or is claimed harmlessly on the
        // panel's chrome); one that lands inside the open readout is claimed
        // inert. Either way the press neither acts on the bar beneath nor
        // reaches the windows below; every other event routes on as usual.
        if matches!(
            event,
            InputEvent::PointerPressed {
                button: PointerButton::Primary
            }
        ) {
            if let Some(response) = self.press_notification(taskbar, scale) {
                return response;
            }
            if self.over_tray_readout(taskbar, scale) {
                return TaskbarResponse::Ignored;
            }
        }
        match event {
            InputEvent::PointerPressed {
                button: PointerButton::Primary,
            } => self.press_primary(taskbar, scale),
            InputEvent::PointerPressed {
                button: PointerButton::Secondary,
            } => self.press_secondary(taskbar, scale),
            InputEvent::PointerPressed {
                button: PointerButton::Middle,
            } => self.press_middle(taskbar, scale),
            InputEvent::PointerScrolled { dx, dy } => self.scroll_tasks(taskbar, scale, dx, dy),
            InputEvent::PointerMoved { .. }
            | InputEvent::PointerReleased { .. }
            | InputEvent::KeyPressed { .. }
            | InputEvent::KeyReleased { .. } => TaskbarResponse::Ignored,
        }
    }

    /// Handle a primary-button press at the current pointer position with
    /// the popup closed, hit-tested at the desktop `scale`.
    ///
    /// A press that is not on the Switchboard capsule first releases a
    /// pinned readout — collapsing an informational readout is not an
    /// action, so the press still acts on whatever it landed on (one click
    /// does one thing).
    fn press_primary(&mut self, taskbar: &mut Taskbar, scale: Scale) -> TaskbarResponse {
        let hit = taskbar.hit_test(self.pointer, scale);
        if hit != Some(Hit::Switchboard) && taskbar.tray().is_pinned() {
            taskbar.tray_mut().set_pinned(false);
            taskbar.request_repaint();
        }
        let Some(hit) = hit else {
            return TaskbarResponse::Ignored;
        };
        match hit {
            Hit::Library => {
                taskbar.open_library();
                TaskbarResponse::OpenLibrary
            }
            Hit::Files => TaskbarResponse::OpenFiles,
            Hit::Pin(index) => Self::activate_pin(taskbar, index),
            Hit::Task(index) => {
                let Some(id) = taskbar.tasks().entries().get(index).map(|entry| entry.id) else {
                    return TaskbarResponse::Ignored;
                };
                let outcome = taskbar.tasks_mut().activate(id);
                TaskbarResponse::TaskActivated { id, outcome }
            }
            // A status signal is a live readout, not an action target this
            // stage: a press is claimed but does nothing (its interactive
            // treatment arrives with the live tray-signal feed).
            Hit::Notification(_) => TaskbarResponse::Ignored,
            Hit::Clock => TaskbarResponse::ClockPressed,
            // Pressing the capsule toggles the readout pin and nothing
            // else: the readout is presentation, and the Switchboard
            // overview window this press will eventually raise arrives
            // with a later stage (`plans/NEW-TASKBAR.md`).
            Hit::Switchboard => {
                taskbar.tray_mut().toggle_pinned();
                taskbar.request_repaint();
                TaskbarResponse::Ignored
            }
        }
    }

    /// Route a primary press against the open notification popover, if the
    /// press lands within it. Returns `Some` when the popover claims the
    /// press — a [`DismissNotification`](TaskbarResponse::DismissNotification)
    /// for the card it hit, or [`Ignored`](TaskbarResponse::Ignored) for a
    /// press on the panel chrome between cards — and `None` when the press
    /// falls outside the popover and should route to the bar. The popover is
    /// presented above the bar and never overlaps it, so this position test
    /// is unambiguous.
    fn press_notification(&self, taskbar: &Taskbar, scale: Scale) -> Option<TaskbarResponse> {
        let layout = taskbar.notifications_layout(scale)?;
        if !layout.contains(self.pointer) {
            return None;
        }
        if let Some(index) = layout.card_at(self.pointer) {
            if let Some(note) = taskbar.notifications().notification(index) {
                return Some(TaskbarResponse::DismissNotification {
                    producer: note.producer,
                    key: note.key,
                });
            }
        }
        Some(TaskbarResponse::Ignored)
    }

    /// Whether the current pointer position lies inside the open Switchboard
    /// readout panel.
    fn over_tray_readout(&self, taskbar: &Taskbar, scale: Scale) -> bool {
        taskbar
            .tray_readout_layout(scale)
            .is_some_and(|readout| readout.contains(self.pointer))
    }

    /// Handle a middle-button press at the current pointer position: over
    /// the Switchboard capsule it switches to the previous task (the
    /// MRU-of-two the task list remembers); anywhere else it is ignored. No
    /// remembered task, or one that vanished, changes nothing (fail closed).
    fn press_middle(&self, taskbar: &mut Taskbar, scale: Scale) -> TaskbarResponse {
        if taskbar.hit_test(self.pointer, scale) != Some(Hit::Switchboard) {
            return TaskbarResponse::Ignored;
        }
        let Some(id) = taskbar.tasks().previous() else {
            return TaskbarResponse::Ignored;
        };
        Self::focus_task(taskbar, id)
    }

    /// Handle a scroll over the Switchboard capsule (or its open readout):
    /// cycle the task list, focusing the entry after the focused one for a
    /// positive step and the one before it for a negative step, wrapping at
    /// both ends (no focused task starts at the first or last entry). The
    /// vertical delta decides; the horizontal one is the fallback when it is
    /// zero. No tasks, no net direction, or a pointer anywhere else changes
    /// nothing.
    fn scroll_tasks(
        &self,
        taskbar: &mut Taskbar,
        scale: Scale,
        dx: i32,
        dy: i32,
    ) -> TaskbarResponse {
        let over_capsule = taskbar.layout(scale).switchboard.contains(self.pointer);
        if !over_capsule && !self.over_tray_readout(taskbar, scale) {
            return TaskbarResponse::Ignored;
        }
        let step = if dy != 0 { dy } else { dx };
        if step == 0 {
            return TaskbarResponse::Ignored;
        }
        let entries = taskbar.tasks().entries();
        if entries.is_empty() {
            return TaskbarResponse::Ignored;
        }
        let focused = taskbar
            .tasks()
            .focused()
            .and_then(|id| entries.iter().position(|entry| entry.id == id));
        let index = if step > 0 {
            focused.map_or(0, |index| (index + 1) % entries.len())
        } else {
            focused.map_or(entries.len() - 1, |index| {
                (index + entries.len() - 1) % entries.len()
            })
        };
        let Some(id) = entries.get(index).map(|entry| entry.id) else {
            return TaskbarResponse::Ignored;
        };
        Self::focus_task(taskbar, id)
    }

    /// Restore-and-focus the task with `id` (never the minimise toggle),
    /// reporting the activation — or nothing when the task vanished (fail
    /// closed).
    fn focus_task(taskbar: &mut Taskbar, id: TaskId) -> TaskbarResponse {
        if taskbar.tasks_mut().set_focused(Some(id)) {
            TaskbarResponse::TaskActivated {
                id,
                outcome: ActivateOutcome::Activated,
            }
        } else {
            TaskbarResponse::Ignored
        }
    }

    /// Handle a secondary-button press at the current pointer position with
    /// the popup and menu closed: a press on a pinned shortcut opens its
    /// context menu; anywhere else on the bar is claimed and does nothing.
    fn press_secondary(&mut self, taskbar: &mut Taskbar, scale: Scale) -> TaskbarResponse {
        let layout = taskbar.layout(scale);
        if let Some(Hit::Pin(index)) = layout.hit_test(self.pointer) {
            let anchor = layout.pins.get(index).copied().unwrap_or(Rect::EMPTY);
            taskbar.open_pin_menu(index, anchor);
        }
        TaskbarResponse::Ignored
    }

    /// Apply a click to the pin at `index`: a pin whose application has a
    /// live window follows the same click-to-activate / minimise rule as
    /// its task button; one with no window asks the embedder to launch it.
    fn activate_pin(taskbar: &mut Taskbar, index: usize) -> TaskbarResponse {
        if taskbar.pins().get(index).is_none() {
            return TaskbarResponse::Ignored;
        }
        match Self::pin_window(taskbar, index) {
            Some(id) => {
                let outcome = taskbar.tasks_mut().activate(id);
                TaskbarResponse::TaskActivated { id, outcome }
            }
            None => TaskbarResponse::ActivatePin { index },
        }
    }

    /// The live window behind the pin at `index`: its matched window id,
    /// only while the task list still knows that window (a stale match
    /// reads as not running, fail closed).
    fn pin_window(taskbar: &Taskbar, index: usize) -> Option<TaskId> {
        let id = taskbar.pins().get(index).and_then(PinView::window)?;
        taskbar
            .tasks()
            .entries()
            .iter()
            .any(|entry| entry.id == id)
            .then_some(id)
    }

    /// Route one event into the open context menu (the top modal layer).
    fn route_to_menu(
        &mut self,
        event: InputEvent,
        taskbar: &mut Taskbar,
        scale: Scale,
    ) -> TaskbarResponse {
        let Some(layout) = taskbar.menu_layout(scale) else {
            // An open menu always lays out; a missing layout means the menu
            // just closed under us — drop the claim rather than guess.
            taskbar.close_menu();
            return TaskbarResponse::Ignored;
        };
        let theme = taskbar.theme().clone();
        let outcome = match event {
            InputEvent::KeyPressed { key, .. } => taskbar.menu_mut().route_key(key),
            InputEvent::KeyReleased { .. } => MenuOutcome::Ignored,
            ref pointer_event => taskbar.menu_mut().route_pointer(
                pointer_event,
                self.pointer,
                &layout,
                scale,
                &theme,
            ),
        };
        match outcome {
            MenuOutcome::Ignored => TaskbarResponse::Ignored,
            MenuOutcome::Changed | MenuOutcome::Dismissed => {
                taskbar.request_repaint();
                TaskbarResponse::Ignored
            }
            MenuOutcome::Choose(choice) => {
                taskbar.request_repaint();
                Self::apply_choice(taskbar, choice)
            }
        }
    }

    /// Translate a chosen menu row into the typed response the embedder
    /// resolves.
    fn apply_choice(taskbar: &mut Taskbar, choice: MenuChoice) -> TaskbarResponse {
        match choice {
            MenuChoice::RestorePin(index) => match Self::pin_window(taskbar, index) {
                // *Open* on a running pin restores and focuses — never the
                // press's minimise toggle.
                Some(id) if taskbar.tasks_mut().set_focused(Some(id)) => {
                    TaskbarResponse::TaskActivated {
                        id,
                        outcome: ActivateOutcome::Activated,
                    }
                }
                // The window vanished while the menu was open: launching is
                // the honest reading of *Open*.
                _ => TaskbarResponse::ActivatePin { index },
            },
            MenuChoice::LaunchPin(index) => TaskbarResponse::ActivatePin { index },
            MenuChoice::Unpin(index) => TaskbarResponse::Unpin { index },
            MenuChoice::OpenEntry(entry) => {
                // Launching from the entry menu behaves exactly like
                // launching from the row itself: the popup closes.
                taskbar.close_library();
                TaskbarResponse::LibraryLaunch { entry }
            }
            MenuChoice::PinEntry(entry) => TaskbarResponse::PinEntry { entry },
        }
    }

    /// Route one event into the open program-library popup.
    ///
    /// A primary press on the Library button toggles the popup shut before
    /// the popup sees the event — the button is the popup's own invoker, so
    /// it is the one bar region a modal popup does not swallow.
    fn route_to_popup(
        &mut self,
        event: InputEvent,
        taskbar: &mut Taskbar,
        scale: Scale,
    ) -> TaskbarResponse {
        if matches!(
            event,
            InputEvent::PointerPressed {
                button: PointerButton::Primary
            }
        ) && taskbar.hit_test(self.pointer, scale) == Some(Hit::Library)
        {
            taskbar.close_library();
            return TaskbarResponse::LibraryDismissed;
        }
        if matches!(
            event,
            InputEvent::PointerPressed {
                button: PointerButton::Secondary
            }
        ) {
            if let Some((entry, anchor)) = Self::entry_row_at(taskbar, self.pointer, scale) {
                taskbar.open_entry_menu(entry, anchor);
                return TaskbarResponse::Ignored;
            }
        }

        let layout = taskbar.library_layout(scale);
        let theme = taskbar.theme().clone();
        let outcome = match event {
            InputEvent::KeyPressed { key, modifiers } => {
                taskbar.library_mut().route_key(key, modifiers, &layout)
            }
            InputEvent::KeyReleased { .. } => PopupOutcome::Ignored,
            ref pointer_event => taskbar.library_mut().route_pointer(
                pointer_event,
                self.pointer,
                &layout,
                &theme,
                scale,
            ),
        };
        match outcome {
            PopupOutcome::Ignored => TaskbarResponse::Ignored,
            PopupOutcome::Changed => {
                taskbar.request_repaint();
                TaskbarResponse::Ignored
            }
            PopupOutcome::Launch(entry) => {
                taskbar.close_library();
                TaskbarResponse::LibraryLaunch { entry }
            }
            PopupOutcome::Dismiss => {
                taskbar.close_library();
                TaskbarResponse::LibraryDismissed
            }
        }
    }

    /// The program-library *entry* row under `point` in the open popup, with
    /// its screen-space rectangle — the anchor for its context menu. Folder
    /// rows and misses return `None`.
    fn entry_row_at(taskbar: &Taskbar, point: Point, scale: Scale) -> Option<(EntryId, Rect)> {
        let layout = taskbar.library_layout(scale);
        let row = layout.row_at(point)?;
        let anchor = layout
            .rows
            .iter()
            .find(|(index, _)| *index == row)
            .map(|(_, rect)| *rect)?;
        match taskbar.library().rows().get(row)? {
            LibraryRow::Entry { id, .. } => Some((id.clone(), anchor)),
            LibraryRow::Folder { .. } => None,
        }
    }
}
