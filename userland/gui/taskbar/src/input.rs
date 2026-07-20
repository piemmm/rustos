//! Routing pointer input into taskbar actions.
//!
//! The [`TaskbarInput`] router turns a stream of device-level pointer
//! [`InputEvent`]s into actions against a [`Taskbar`]: a primary-button press
//! is hit-tested against the bar's computed [`BarLayout`](crate::BarLayout)
//! and drives the model — toggling the start menu, applying the
//! click-to-activate / minimise rule to a task, or reporting a press on a
//! notification icon or the clock.
//!
//! It is the taskbar counterpart of the window manager's input router, and it
//! consumes the **same** shared [`tairix_input`] event vocabulary, so the
//! desktop routes one event type to both. Like that
//! router it holds no pixels, tracks the pointer position from motion events,
//! applies presses at that position, and never panics: a press that misses
//! every region changes nothing.
//!
//! While the start menu is open the router treats it as modal: a primary press
//! inside the popup ([`Taskbar::menu_layout`]) activates the entry under the
//! pointer, a press on the start button toggles the menu shut, and a press
//! anywhere else dismisses the menu (the standard click-away behaviour)
//! without also acting on what it landed on — one click does one thing.

use tairix_geometry::{Point, Scale};
use tairix_input::{InputEvent, PointerButton};

use crate::layout::Hit;
use crate::menu::{MenuAction, MenuEntryId};
use crate::notifications::IconId;
use crate::taskbar::Taskbar;
use crate::tasks::{ActivateOutcome, TaskId};

/// What a [`TaskbarInput`] press did to the taskbar.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum TaskbarResponse {
    /// The event changed no taskbar state (a non-primary button, a release,
    /// pointer motion, a key event, or a press that missed every region).
    Ignored,
    /// The start button was pressed, toggling the start menu. `open` is the
    /// menu's new state.
    StartMenuToggled {
        /// `true` if the menu is now showing.
        open: bool,
    },
    /// A task slot was pressed, applying the click-to-activate / minimise
    /// rule to that task.
    TaskActivated {
        /// The task whose slot was pressed.
        id: TaskId,
        /// What the click did, so the caller can drive the window manager.
        outcome: ActivateOutcome,
    },
    /// A notification icon was pressed.
    NotificationActivated {
        /// The icon that was pressed.
        id: IconId,
    },
    /// The clock was pressed.
    ClockPressed,
    /// An entry inside the open start menu was selected, which closed the
    /// menu. The caller performs the entry's `action`.
    MenuEntrySelected {
        /// The entry that was selected.
        id: MenuEntryId,
        /// The action the selected entry triggers.
        action: MenuAction,
    },
    /// A press outside the open start menu (but not on the start button)
    /// dismissed it, changing nothing else.
    StartMenuDismissed,
}

/// Routes pointer input into [`Taskbar`] actions.
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

    /// Process one input `event` against `taskbar`, hit-testing presses at the
    /// desktop `scale` (the compositor's output density),
    /// returning what changed.
    ///
    /// Only a primary-button press acts; pointer motion updates the tracked
    /// position, and every other event is [`TaskbarResponse::Ignored`].
    pub fn handle(
        &mut self,
        event: InputEvent,
        taskbar: &mut Taskbar,
        scale: Scale,
    ) -> TaskbarResponse {
        match event {
            InputEvent::PointerMoved { to } => {
                self.pointer = to;
                TaskbarResponse::Ignored
            }
            InputEvent::PointerPressed {
                button: PointerButton::Primary,
            } => self.press_primary(taskbar, scale),
            InputEvent::PointerPressed { .. }
            | InputEvent::PointerReleased { .. }
            | InputEvent::PointerScrolled { .. }
            | InputEvent::KeyPressed { .. }
            | InputEvent::KeyReleased { .. } => TaskbarResponse::Ignored,
        }
    }

    /// Handle a primary-button press at the current pointer position,
    /// hit-tested at the desktop `scale`.
    fn press_primary(&mut self, taskbar: &mut Taskbar, scale: Scale) -> TaskbarResponse {
        let hit = taskbar.hit_test(self.pointer, scale);

        // While the menu is open it is modal: every press except one on the
        // start button (which toggles it shut, handled below) routes to the
        // popup or dismisses the menu.
        if taskbar.start_menu().is_open() && hit != Some(Hit::StartButton) {
            return self.press_open_menu(taskbar, scale);
        }

        let Some(hit) = hit else {
            return TaskbarResponse::Ignored;
        };
        match hit {
            Hit::StartButton => {
                let open = taskbar.start_menu_mut().toggle();
                TaskbarResponse::StartMenuToggled { open }
            }
            Hit::Task(index) => {
                let Some(id) = taskbar.tasks().entries().get(index).map(|entry| entry.id) else {
                    return TaskbarResponse::Ignored;
                };
                let outcome = taskbar.tasks_mut().activate(id);
                TaskbarResponse::TaskActivated { id, outcome }
            }
            Hit::Notification(index) => {
                let Some(id) = taskbar
                    .notifications()
                    .icons()
                    .get(index)
                    .map(|icon| icon.id)
                else {
                    return TaskbarResponse::Ignored;
                };
                TaskbarResponse::NotificationActivated { id }
            }
            Hit::Clock => TaskbarResponse::ClockPressed,
        }
    }

    /// Handle a primary press while the start menu is open: select the entry
    /// under the pointer, or dismiss the menu if the press misses the popup.
    fn press_open_menu(&mut self, taskbar: &mut Taskbar, scale: Scale) -> TaskbarResponse {
        let layout = taskbar.menu_layout(scale);
        let Some(index) = layout.hit_test(self.pointer) else {
            taskbar.start_menu_mut().close();
            return TaskbarResponse::StartMenuDismissed;
        };
        let Some(id) = taskbar
            .start_menu()
            .entries()
            .get(index)
            .map(|entry| entry.id)
        else {
            return TaskbarResponse::Ignored;
        };
        match taskbar.start_menu_mut().activate(id) {
            Some(action) => TaskbarResponse::MenuEntrySelected { id, action },
            None => TaskbarResponse::Ignored,
        }
    }
}
