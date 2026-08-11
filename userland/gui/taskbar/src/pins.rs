//! The pinned-shortcut strip: the per-pin view models the session resolves
//! and the strip's live pointer state.
//!
//! A pin is per-user configuration (the `lib/taskpins` store, read and
//! written by the session under the user's own identity); what the bar holds
//! is the session's *resolved view* of each pin: a display label, the class
//! glyph, optional per-application artwork (rasterised by the session
//! through its sandboxed icon pipeline — the bar never parses image bytes),
//! and the running desktop window the pin currently matches, if any. The
//! strip derives each pin's live window-visibility state from the
//! [`TaskList`] at paint time, so there is never a second copy of window
//! state to fall out of step: a pinned application that is also running
//! shows the same Running/Active/Minimized reading as its task button.

use alloc::string::String;
use alloc::vec::Vec;

use tairix_controls::{
    ControlState, PointerState, TaskVisibility, TaskbarItem, TaskbarPresentation,
};
use tairix_icon::IconKind;
use tairix_proglib::EntryId;
use tairix_raster::Surface;

use crate::tasks::{TaskId, TaskList};

/// One pinned shortcut as the session resolved it for display.
///
/// The session builds these from the pin store, the program-library catalog
/// (label and icon for an `entry` pin), and its own launch table (which
/// desktop-launched window, if any, currently runs the pinned bundle).
#[derive(Clone, Debug)]
pub struct PinView {
    label: String,
    icon: IconKind,
    entry: Option<EntryId>,
    artwork: Option<Surface>,
    window: Option<TaskId>,
}

impl PinView {
    /// A pin view with the given display label and class glyph, not running
    /// and without per-application artwork.
    #[must_use]
    pub fn new(label: impl Into<String>, icon: IconKind) -> Self {
        Self {
            label: label.into(),
            icon,
            entry: None,
            artwork: None,
            window: None,
        }
    }

    /// This view identified as pinning the given program-library entry (so
    /// the library popup's context menu can offer *Unpin* for it).
    #[must_use]
    pub fn with_entry(mut self, entry: EntryId) -> Self {
        self.entry = Some(entry);
        self
    }

    /// This view with the application's own rasterised icon artwork.
    #[must_use]
    pub fn with_artwork(mut self, artwork: Surface) -> Self {
        self.artwork = Some(artwork);
        self
    }

    /// This view matched to a running desktop window.
    #[must_use]
    pub fn with_window(mut self, window: TaskId) -> Self {
        self.window = Some(window);
        self
    }

    /// The pin's display label (shown by context surfaces, not on the slot).
    #[must_use]
    pub fn label(&self) -> &str {
        &self.label
    }

    /// The program-library entry this pin references, if it is an entry pin.
    #[must_use]
    pub fn entry(&self) -> Option<&EntryId> {
        self.entry.as_ref()
    }

    /// The pin's class glyph, drawn when no artwork is available.
    #[must_use]
    pub fn icon(&self) -> IconKind {
        self.icon
    }

    /// The application's rasterised icon artwork, if the session loaded one.
    #[must_use]
    pub fn artwork(&self) -> Option<&Surface> {
        self.artwork.as_ref()
    }

    /// The running desktop window this pin currently matches, if any.
    #[must_use]
    pub fn window(&self) -> Option<TaskId> {
        self.window
    }
}

/// The pin strip: the resolved pins in display order plus hover state.
#[derive(Clone, Debug, Default)]
pub struct PinStrip {
    pins: Vec<PinView>,
    hover: Option<usize>,
}

impl PinStrip {
    /// An empty strip.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Replace the strip's pins with the session's freshly resolved views.
    pub fn set_pins(&mut self, pins: Vec<PinView>) {
        self.hover = self.hover.filter(|&index| index < pins.len());
        self.pins = pins;
    }

    /// The resolved pins, in display order.
    #[must_use]
    pub fn pins(&self) -> &[PinView] {
        &self.pins
    }

    /// The pin at `index`, if any.
    #[must_use]
    pub fn get(&self, index: usize) -> Option<&PinView> {
        self.pins.get(index)
    }

    /// The number of pins.
    #[must_use]
    pub fn len(&self) -> usize {
        self.pins.len()
    }

    /// Whether nothing is pinned.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.pins.is_empty()
    }

    /// The hovered pin index, if the pointer rests on one.
    #[must_use]
    pub fn hover(&self) -> Option<usize> {
        self.hover
    }

    /// The strip index of the pin referencing the given program-library
    /// entry, if that entry is pinned.
    #[must_use]
    pub fn position_of_entry(&self, entry: &EntryId) -> Option<usize> {
        self.pins
            .iter()
            .position(|pin| pin.entry.as_ref() == Some(entry))
    }

    /// Track the hovered pin, reporting whether the visual state changed.
    pub(crate) fn set_hover(&mut self, hover: Option<usize>) -> bool {
        let hover = hover.filter(|&index| index < self.pins.len());
        if self.hover == hover {
            return false;
        }
        self.hover = hover;
        true
    }

    /// The live window-visibility reading for the pin at `index`, derived
    /// from the task list at read time.
    ///
    /// A pin whose matched window the task list no longer knows reads as
    /// closed (fail closed on a stale mapping) rather than pretending the
    /// application still runs.
    #[must_use]
    pub fn visibility(&self, index: usize, tasks: &TaskList) -> TaskVisibility {
        let Some(window) = self.pins.get(index).and_then(PinView::window) else {
            return TaskVisibility::Closed;
        };
        if !tasks.entries().iter().any(|entry| entry.id == window) {
            return TaskVisibility::Closed;
        }
        if tasks.focused() == Some(window) {
            TaskVisibility::Active
        } else if tasks.is_minimised(window) {
            TaskVisibility::Minimized
        } else {
            TaskVisibility::Running
        }
    }

    /// The shared control for the pin at `index`, ready to paint: an
    /// icon-only [`TaskbarItem`] carrying the pin's identity, its live
    /// visibility, and the strip's hover state.
    #[must_use]
    pub(crate) fn item(&self, index: usize, tasks: &TaskList) -> Option<TaskbarItem> {
        let pin = self.pins.get(index)?;
        let pointer = if self.hover == Some(index) {
            PointerState::Hover
        } else {
            PointerState::None
        };
        Some(
            TaskbarItem::new(pin.label.clone(), pin.icon)
                .with_presentation(TaskbarPresentation::Icon)
                .with_visibility(self.visibility(index, tasks))
                .with_state(ControlState::idle().with_pointer(pointer)),
        )
    }
}
