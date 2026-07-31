//! Routing device input into taskbar actions.
//!
//! The [`TaskbarInput`] router turns a stream of device-level
//! [`InputEvent`]s into actions against a [`Taskbar`]: a primary-button press
//! is hit-tested against the bar's computed [`BarLayout`](crate::BarLayout)
//! and drives the model — opening the program-library popup, reporting the
//! Files button, applying the click-to-activate / minimise rule to a task,
//! or reporting a press on a notification icon or the clock.
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

use tairix_geometry::{Point, Scale};
use tairix_input::{InputEvent, PointerButton};
use tairix_proglib::EntryId;

use crate::layout::Hit;
use crate::library::PopupOutcome;
use crate::notifications::IconId;
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
    /// A notification icon was pressed.
    NotificationActivated {
        /// The icon that was pressed.
        id: IconId,
    },
    /// The clock was pressed.
    ClockPressed,
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
    /// With the popup closed only a primary-button press acts; pointer
    /// motion updates the tracked position (and the leading buttons' hover
    /// feedback), and every other event is [`TaskbarResponse::Ignored`].
    /// With the popup open the whole stream routes into it (see the
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
        if taskbar.library().is_open() {
            return self.route_to_popup(event, taskbar, scale);
        }
        match event {
            InputEvent::PointerPressed {
                button: PointerButton::Primary,
            } => self.press_primary(taskbar, scale),
            InputEvent::PointerMoved { .. }
            | InputEvent::PointerPressed { .. }
            | InputEvent::PointerReleased { .. }
            | InputEvent::PointerScrolled { .. }
            | InputEvent::KeyPressed { .. }
            | InputEvent::KeyReleased { .. } => TaskbarResponse::Ignored,
        }
    }

    /// Handle a primary-button press at the current pointer position with
    /// the popup closed, hit-tested at the desktop `scale`.
    fn press_primary(&mut self, taskbar: &mut Taskbar, scale: Scale) -> TaskbarResponse {
        let Some(hit) = taskbar.hit_test(self.pointer, scale) else {
            return TaskbarResponse::Ignored;
        };
        match hit {
            Hit::Library => {
                taskbar.open_library();
                TaskbarResponse::OpenLibrary
            }
            Hit::Files => TaskbarResponse::OpenFiles,
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
}
