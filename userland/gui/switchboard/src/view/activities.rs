//! The Activities section: the grouped tasks that move, pause and close
//! together (`plans/NEW-SWITCHBOARD.md` S3).
//!
//! Owns the caller's activity view model ([`ActivitySummary`] and its
//! [`ActivityMember`]s), the [`ActivityControl`] vocabulary a row offers, the
//! inline rename [`TextField`], and the section's layout, painting and input.

use alloc::string::String;
use alloc::vec::Vec;

use tairix_font::BitmapFont;
use tairix_geometry::{to_i32, Rect, Scale};
use tairix_input::{InputEvent, Key, Modifiers};
use tairix_raster::Surface;
use tairix_theme::Theme;

use tairix_controls::{
    ActivityState, AuthorityState, Button, ButtonAction, ButtonContent, ControlRole, ControlState,
    ListRow, RowAction, TextAction, TextField,
};

use super::{ActionVerdict, ListInfo, Section, Switchboard, SwitchboardAction};

/// An action a Switchboard activity header row can request (spec T12).
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum ActivityControl {
    /// Switch to the activity.
    Switch,
    /// Pause every member of the activity.
    Pause,
    /// Resume every member of the activity.
    Resume,
    /// Close the activity and every member.
    Close,
}

/// One task grouped into an [`ActivitySummary`] (spec T12).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ActivityMember {
    /// The member's display name.
    pub name: String,
    /// A short trailing detail (e.g. owner, CPU%).
    pub detail: String,
    /// The member's live activity, drawn as its own Heat Seam.
    pub activity: ActivityState,
}

/// One activity: a named group of tasks that move, pause, and close together
/// (spec T12).
///
/// Rendered as a header [`ListRow`] plus one [`ListRow`] per
/// [`member`](Self::members), indented beneath it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ActivitySummary {
    /// A stable identity for this activity, independent of its position in
    /// the list, so an in-flight rename can survive a refresh that reorders
    /// or shortens [`SwitchboardModel::activities`](super::SwitchboardModel::activities).
    pub id: u64,
    /// The activity's display name.
    pub name: String,
    /// A short trailing detail (e.g. member count).
    pub detail: String,
    /// The activity's combined live activity, drawn as the header's Heat
    /// Seam.
    pub activity: ActivityState,
    /// Whether every member is currently paused.
    pub paused: bool,
    /// Whether the caller may pause/resume/close this activity.
    pub can_control: bool,
    /// Whether another task may still be grouped into this activity.
    pub can_accept_member: bool,
    /// The activity's member tasks.
    pub members: Vec<ActivityMember>,
}

/// One activity rendered as a header [`ListRow`] plus its Switch/Pause-or-
/// Resume/Rename/Close [`Button`]s, and one [`ListRow`] per member.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct ActivityEntry {
    pub(super) id: u64,
    pub(super) name: String,
    pub(super) detail: String,
    pub(super) activity: ActivityState,
    pub(super) header: ListRow,
    pub(super) switch: Button,
    pub(super) pause_resume: Button,
    pub(super) rename: Button,
    pub(super) close: Button,
    pub(super) paused: bool,
    pub(super) can_control: bool,
    pub(super) can_accept_member: bool,
    pub(super) members: Vec<ListRow>,
}

/// Which row a flattened Activities-section list index names: an activity's
/// own header row, or one of its member rows.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub(super) enum ActivityRow {
    /// The header row of the activity at this index.
    Header(usize),
    /// A member row: the owning activity's index, then the member's index
    /// within it.
    Member(usize, usize),
}

/// An in-flight inline rename of an activity's header row.
///
/// `id` is the activity's stable identity (spec T12): a model refresh that
/// still has an activity with this `id` relocates `index` to match, so typing
/// survives a refresh unless the activity itself is gone.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct RenameEdit {
    pub(super) id: u64,
    pub(super) index: usize,
    pub(super) field: TextField,
}

impl Switchboard {
    /// Build an activity's header row + Switch/Pause-or-Resume/Rename/Close
    /// buttons, and one row per member.
    pub(super) fn build_activity(summary: ActivitySummary) -> ActivityEntry {
        let header = Self::build_activity_header(&summary.name, &summary.detail, summary.activity);
        let switch = Button::new(
            ButtonContent::Label(String::from("Switch")),
            ControlRole::Primary,
        );
        let gated = if summary.can_control {
            ActionVerdict::Ready
        } else {
            ActionVerdict::DeniedByAuthority
        };
        let mut pause_resume = Button::labelled(if summary.paused { "Resume" } else { "Pause" });
        pause_resume.set_state(gated.to_state());
        let rename = Button::labelled("Rename");
        let mut close = Button::new(
            ButtonContent::Label(String::from("Close")),
            ControlRole::Destructive,
        );
        close.set_state(if summary.can_control {
            ControlState::idle().with_authority(AuthorityState::NeedsConfirmation)
        } else {
            ActionVerdict::DeniedByAuthority.to_state()
        });
        let members = summary
            .members
            .into_iter()
            .map(|member| {
                ListRow::new(member.name)
                    .with_trailing(member.detail)
                    .with_state(ControlState::idle().with_activity(member.activity))
            })
            .collect();
        ActivityEntry {
            id: summary.id,
            name: summary.name,
            detail: summary.detail,
            activity: summary.activity,
            header,
            switch,
            pause_resume,
            rename,
            close,
            paused: summary.paused,
            can_control: summary.can_control,
            can_accept_member: summary.can_accept_member,
            members,
        }
    }

    /// Build (or rebuild, after a rename commit) an activity header row from
    /// its name, trailing detail, and live activity — the one place that
    /// composes a header [`ListRow`], so a rename can never drift from how
    /// [`build_activity`](Self::build_activity) first built it.
    fn build_activity_header(name: &str, detail: &str, activity: ActivityState) -> ListRow {
        ListRow::new(name)
            .with_trailing(detail)
            .with_state(ControlState::idle().with_activity(activity))
    }

    /// The flattened row count of the Activities section: one header row per
    /// activity plus one row per member.
    pub(super) fn total_activity_rows(&self) -> usize {
        self.activities.iter().map(|a| 1 + a.members.len()).sum()
    }

    /// The activity row a flattened Activities-section index names — its
    /// owning activity's header, or one of its members — or `None` past the
    /// end of the flattened list.
    pub(super) fn activity_row_at(&self, index: usize) -> Option<ActivityRow> {
        let mut remaining = index;
        for (ai, entry) in self.activities.iter().enumerate() {
            if remaining == 0 {
                return Some(ActivityRow::Header(ai));
            }
            remaining -= 1;
            if remaining < entry.members.len() {
                return Some(ActivityRow::Member(ai, remaining));
            }
            remaining -= entry.members.len();
        }
        None
    }

    /// Render the visible activity rows: a header row (with its Switch/Pause-
    /// or-Resume/Rename/Close buttons, or an in-flight rename field in place
    /// of the header) followed by its indented member rows.
    pub(super) fn render_activity_rows(
        &self,
        surface: &mut Surface,
        info: ListInfo,
        scale: Scale,
        theme: &Theme,
        font: BitmapFont,
    ) {
        let indent = scale.scale_length(theme.metrics().control_height);
        let start = usize::try_from(self.offsets[self.section.index()]).unwrap_or(0);
        for slot in 0..info.visible() {
            let Some(row) = self.activity_row_at(start + slot as usize) else {
                break;
            };
            let item = info.item_rect(slot);
            match row {
                ActivityRow::Header(ai) => {
                    let Some(entry) = self.activities.get(ai) else {
                        continue;
                    };
                    let (row_rect, buttons) =
                        Self::split_row(item, Self::row_actions(Section::Activities), scale, theme);
                    if let Some(edit) = self.rename.as_ref().filter(|e| e.index == ai) {
                        edit.field.render(surface, row_rect, scale, theme, font);
                    } else {
                        entry
                            .header
                            .render(surface, row_rect, scale, theme, font, None);
                    }
                    if let Some(rect) = buttons.first() {
                        entry.switch.render(surface, *rect, scale, theme, font);
                    }
                    if let Some(rect) = buttons.get(1) {
                        entry
                            .pause_resume
                            .render(surface, *rect, scale, theme, font);
                    }
                    if let Some(rect) = buttons.get(2) {
                        entry.rename.render(surface, *rect, scale, theme, font);
                    }
                    if let Some(rect) = buttons.get(3) {
                        entry.close.render(surface, *rect, scale, theme, font);
                    }
                }
                ActivityRow::Member(ai, mi) => {
                    let Some(member) = self
                        .activities
                        .get(ai)
                        .and_then(|entry| entry.members.get(mi))
                    else {
                        continue;
                    };
                    let indented = Rect::new(
                        item.left() + to_i32(indent),
                        item.top(),
                        item.width.saturating_sub(indent),
                        item.height,
                    );
                    let (row_rect, _) = Self::split_row(indented, 0, scale, theme);
                    member.render(surface, row_rect, scale, theme, font, None);
                }
            }
        }
    }

    /// Route a pointer event to the Activities section: header rows (their
    /// Switch/Pause-or-Resume/Rename/Close buttons, or an in-flight rename
    /// field) and member rows (selection only).
    pub(super) fn activities_on_pointer(
        &mut self,
        event: &InputEvent,
        info: ListInfo,
        start: usize,
        scale: Scale,
        theme: &Theme,
    ) -> Option<SwitchboardAction> {
        let indent = scale.scale_length(theme.metrics().control_height);
        for slot in 0..info.visible() {
            let Some(row) = self.activity_row_at(start + slot as usize) else {
                break;
            };
            let item = info.item_rect(slot);
            match row {
                ActivityRow::Header(ai) => {
                    let (_, buttons) =
                        Self::split_row(item, Self::row_actions(Section::Activities), scale, theme);
                    let Some(entry) = self.activities.get_mut(ai) else {
                        continue;
                    };
                    if buttons.first().is_some_and(|rect| {
                        entry.switch.on_pointer(event, *rect) == Some(ButtonAction::Activated)
                    }) {
                        return Some(SwitchboardAction::Activity {
                            index: ai,
                            control: ActivityControl::Switch,
                        });
                    }
                    if buttons.get(1).is_some_and(|rect| {
                        entry.pause_resume.on_pointer(event, *rect) == Some(ButtonAction::Activated)
                    }) {
                        let control = if entry.paused {
                            ActivityControl::Resume
                        } else {
                            ActivityControl::Pause
                        };
                        return Some(SwitchboardAction::Activity { index: ai, control });
                    }
                    if buttons.get(2).is_some_and(|rect| {
                        entry.rename.on_pointer(event, *rect) == Some(ButtonAction::Activated)
                    }) {
                        self.begin_rename(ai);
                        return None;
                    }
                    if buttons.get(3).is_some_and(|rect| {
                        entry.close.on_pointer(event, *rect) == Some(ButtonAction::Activated)
                    }) {
                        return Some(SwitchboardAction::Activity {
                            index: ai,
                            control: ActivityControl::Close,
                        });
                    }
                }
                ActivityRow::Member(ai, mi) => {
                    let indented = Rect::new(
                        item.left() + to_i32(indent),
                        item.top(),
                        item.width.saturating_sub(indent),
                        item.height,
                    );
                    let (row_rect, _) = Self::split_row(indented, 0, scale, theme);
                    let Some(member) = self
                        .activities
                        .get_mut(ai)
                        .and_then(|entry| entry.members.get_mut(mi))
                    else {
                        continue;
                    };
                    if member.on_pointer(event, row_rect) == Some(RowAction::Activated) {
                        if let Some(entry) = self.activities.get_mut(ai) {
                            for (i, row) in entry.members.iter_mut().enumerate() {
                                row.set_selected(i == mi);
                            }
                        }
                    }
                }
            }
        }
        None
    }

    /// Begin an inline rename of the activity at `index`, pre-filled with its
    /// current name.
    fn begin_rename(&mut self, index: usize) {
        let Some(entry) = self.activities.get(index) else {
            return;
        };
        let mut field = TextField::new().with_text(&entry.name).with_max_len(48);
        field.set_focused(true);
        self.rename = Some(RenameEdit {
            id: entry.id,
            index,
            field,
        });
    }

    /// Activate the focused Activities row's action-focused header button
    /// (Switch, Pause-or-Resume, Rename, Close, in action-focus order).
    /// Member rows are display-only, so they activate nothing.
    pub(super) fn activate_focused_activity(&mut self, key: Key) -> Option<SwitchboardAction> {
        let Some(ActivityRow::Header(ai)) = self.activity_row_at(self.content_focus) else {
            return None;
        };
        let entry = self.activities.get_mut(ai)?;
        match self.row_action {
            0 => (entry.switch.on_key(key) == Some(ButtonAction::Activated)).then_some(
                SwitchboardAction::Activity {
                    index: ai,
                    control: ActivityControl::Switch,
                },
            ),
            1 => {
                let control = if entry.paused {
                    ActivityControl::Resume
                } else {
                    ActivityControl::Pause
                };
                (entry.pause_resume.on_key(key) == Some(ButtonAction::Activated))
                    .then_some(SwitchboardAction::Activity { index: ai, control })
            }
            2 => {
                if entry.rename.on_key(key) == Some(ButtonAction::Activated) {
                    self.begin_rename(ai);
                }
                None
            }
            _ => (entry.close.on_key(key) == Some(ButtonAction::Activated)).then_some(
                SwitchboardAction::Activity {
                    index: ai,
                    control: ActivityControl::Close,
                },
            ),
        }
    }

    /// Route a key to the in-flight rename field: Enter commits (rebuilding
    /// the header row and reporting the rename), Escape cancels without
    /// emitting, and everything else edits the field.
    pub(super) fn rename_on_key(&mut self, key: Key) -> Option<SwitchboardAction> {
        let action = self
            .rename
            .as_mut()?
            .field
            .on_key(key, Modifiers::default());
        match action {
            Some(TextAction::Submitted) => {
                let edit = self.rename.take()?;
                let index = edit.index;
                let entry = self.activities.get_mut(index)?;
                entry.name = String::from(edit.field.text());
                entry.header =
                    Self::build_activity_header(&entry.name, &entry.detail, entry.activity);
                self.submitted_activity_name = Some(entry.name.clone());
                Some(SwitchboardAction::ActivityRenamed { index })
            }
            Some(TextAction::Cancelled) => {
                self.rename = None;
                None
            }
            Some(TextAction::Edited) | None => None,
        }
    }
}

#[cfg(test)]
#[path = "activities_tests.rs"]
mod tests;
