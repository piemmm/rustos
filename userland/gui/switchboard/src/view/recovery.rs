//! The Recovery section: hung objects and the recovery actions they offer
//! (`plans/NEW-SWITCHBOARD.md` S3).
//!
//! Owns the caller's fault view model ([`RecoveryItem`]), the
//! [`RecoveryControl`] vocabulary a row offers, and the section's layout,
//! painting and input.

use alloc::string::String;

use tairix_font::BitmapFont;
use tairix_geometry::Scale;
use tairix_input::InputEvent;
use tairix_raster::Surface;
use tairix_theme::Theme;

use tairix_controls::{
    AuthorityState, Button, ButtonAction, ButtonContent, ControlRole, ControlState, ListRow,
    RecoveryState, RowAction,
};

use super::{action_state, ListInfo, Section, Switchboard, SwitchboardAction};

/// One hung or recoverable object (spec §17).
///
/// Rendered as a recovery [`ListRow`] with a leading recovery
/// rail and bead, a Restart action ([`ControlRole::Recovery`]), and a Force
/// action ([`ControlRole::Destructive`] with a confirmation posture).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RecoveryItem {
    /// The object's display name.
    pub name: String,
    /// A short trailing detail (e.g. how long it has been unresponsive).
    pub detail: String,
    /// The object's recovery posture.
    pub recovery: RecoveryState,
    /// Whether an ordinary restart is available.
    pub can_restart: bool,
    /// Whether the high-impact force action is available.
    pub can_force: bool,
}

/// A recovery action a Switchboard recovery row can request.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum RecoveryControl {
    /// Restart the hung object (an ordinary recovery).
    Restart,
    /// Force the object (the high-impact, confirmation-gated action).
    Force,
}

/// One recovery object rendered as a [`ListRow`] plus Restart and Force
/// [`Button`]s.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct RecoveryEntry {
    pub(super) row: ListRow,
    pub(super) restart: Button,
    pub(super) force: Button,
}

impl Switchboard {
    /// Build a recovery object's row + Restart/Force buttons.
    pub(super) fn build_recovery(item: RecoveryItem) -> RecoveryEntry {
        let row = ListRow::new(item.name)
            .with_trailing(item.detail)
            .with_role(ControlRole::Recovery)
            .with_state(ControlState::idle().with_recovery(item.recovery));
        let mut restart = Button::new(
            ButtonContent::Label(String::from("Restart")),
            ControlRole::Recovery,
        );
        restart.set_state(action_state(item.can_restart));
        let mut force = Button::new(
            ButtonContent::Label(String::from("Force")),
            ControlRole::Destructive,
        );
        // A permitted force action carries a deliberate confirmation posture; a
        // refused one shows the Authority Mark and fails closed.
        force.set_state(if item.can_force {
            ControlState::idle().with_authority(AuthorityState::NeedsConfirmation)
        } else {
            action_state(false)
        });
        RecoveryEntry {
            row,
            restart,
            force,
        }
    }

    /// Render the visible recovery rows and their Restart/Force buttons.
    pub(super) fn render_recovery_rows(
        &self,
        surface: &mut Surface,
        info: ListInfo,
        scale: Scale,
        theme: &Theme,
        font: BitmapFont,
    ) {
        let start = usize::try_from(self.offsets[self.section.index()]).unwrap_or(0);
        for slot in 0..info.visible() {
            let Some(entry) = self.recovery.get(start + slot as usize) else {
                break;
            };
            let (row_rect, buttons) = Self::split_row(
                info.item_rect(slot),
                Self::row_actions(Section::Recovery),
                scale,
                theme,
            );
            entry
                .row
                .render(surface, row_rect, scale, theme, font, None);
            if let Some(rect) = buttons.first() {
                entry.restart.render(surface, *rect, scale, theme, font);
            }
            if let Some(rect) = buttons.get(1) {
                entry.force.render(surface, *rect, scale, theme, font);
            }
        }
    }

    /// Route a pointer event to the recovery rows (Restart/Force buttons and
    /// row selection).
    pub(super) fn recovery_on_pointer(
        &mut self,
        event: &InputEvent,
        info: ListInfo,
        start: usize,
        scale: Scale,
        theme: &Theme,
    ) -> Option<SwitchboardAction> {
        let mut selected = None;
        for slot in 0..info.visible() {
            let idx = start + slot as usize;
            let (row_rect, buttons) = Self::split_row(
                info.item_rect(slot),
                Self::row_actions(Section::Recovery),
                scale,
                theme,
            );
            let Some(entry) = self.recovery.get_mut(idx) else {
                break;
            };
            if buttons.first().is_some_and(|rect| {
                entry.restart.on_pointer(event, *rect) == Some(ButtonAction::Activated)
            }) {
                return Some(SwitchboardAction::Recovery {
                    index: idx,
                    control: RecoveryControl::Restart,
                });
            }
            if buttons.get(1).is_some_and(|rect| {
                entry.force.on_pointer(event, *rect) == Some(ButtonAction::Activated)
            }) {
                return Some(SwitchboardAction::Recovery {
                    index: idx,
                    control: RecoveryControl::Force,
                });
            }
            if entry.row.on_pointer(event, row_rect) == Some(RowAction::Activated) {
                selected = Some(idx);
            }
        }
        if let Some(idx) = selected {
            for (i, entry) in self.recovery.iter_mut().enumerate() {
                entry.row.set_selected(i == idx);
            }
        }
        None
    }
}

#[cfg(test)]
#[path = "recovery_tests.rs"]
mod tests;
