//! The Tasks section: the live task/application list
//! (`plans/NEW-SWITCHBOARD.md` S3).
//!
//! Owns the caller's task view model ([`TaskSummary`]), the retained
//! [`ListRow`] entries derived from it, the grouping [`Menu`] a row's group
//! action opens, and the section's layout, painting and input.

use alloc::string::String;
use alloc::vec::Vec;
use core::cmp::Ordering;

use tairix_font::BitmapFont;
use tairix_geometry::{to_i32, Rect, Scale};
use tairix_input::{InputEvent, Key, PointerButton};
use tairix_raster::Surface;
use tairix_theme::Theme;

use tairix_controls::{
    ActivityState, Button, ButtonAction, ControlState, ListRow, Menu, MenuAction, MenuItem,
    PressureState, RecoveryState, RowAction,
};

use super::{action_state, ListInfo, SbLayout, Section, Switchboard, SwitchboardAction};

/// One live task/application, as the caller's typed view model (spec §17).
///
/// Switchboard renders it as a [`ListRow`] carrying the task's
/// activity as a Heat Seam, its resource pressure as a Pressure Rail, and its
/// recovery posture as a Signal Bead, with a single row action [`Button`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TaskSummary {
    /// The task's display name.
    pub name: String,
    /// A short trailing detail (e.g. owner, CPU%).
    pub detail: String,
    /// The resource pressure the task is under, if any.
    pub pressure: PressureState,
    /// What work the task is doing.
    pub activity: ActivityState,
    /// The task's recovery posture (hung, restart recommended, …).
    pub recovery: RecoveryState,
    /// The row action's label (e.g. "Sleep", "End").
    pub action: String,
    /// Whether the caller may perform the row action. A false value renders
    /// the action denied (Authority Mark) and fails closed on activation.
    pub action_allowed: bool,
    /// The activity this task is grouped into, as an index into
    /// [`SwitchboardModel::activities`](super::SwitchboardModel::activities); `None` when it is ungrouped.
    pub group: Option<usize>,
}

/// One task rendered as a [`ListRow`] plus its primary action [`Button`] and
/// its `Group` [`Button`] (which opens the Group popup menu).
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct TaskEntry {
    pub(super) row: ListRow,
    pub(super) action: Button,
    pub(super) group_button: Button,
    /// The task's activity, as of the last [`Switchboard::adopt`], mirroring
    /// [`TaskSummary::group`] so the Group popup can be built without the
    /// model.
    pub(super) group: Option<usize>,
}

/// The Group popup [`Menu`], anchored on a Tasks row's `Group` button.
///
/// It names the task by index rather than by a captured screen rectangle: the
/// anchor rectangle is re-derived from the current layout every time the
/// popup is rendered or hit-tested, so it never goes stale across a resize —
/// and it needs no bounds/scale/theme to open from the keyboard, which
/// [`Switchboard::on_key`] cannot supply.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct GroupPopup {
    pub(super) task: usize,
    pub(super) menu: Menu,
}

impl Switchboard {
    /// Build a task's row + primary action button + Group button.
    pub(super) fn build_task(task: TaskSummary) -> TaskEntry {
        let state = ControlState::idle()
            .with_pressure(task.pressure)
            .with_activity(task.activity)
            .with_recovery(task.recovery);
        let row = ListRow::new(task.name)
            .with_trailing(task.detail)
            .with_state(state);
        let mut action = Button::labelled(task.action);
        action.set_state(action_state(task.action_allowed));
        let group_button = Button::labelled("Group");
        TaskEntry {
            row,
            action,
            group_button,
            group: task.group,
        }
    }

    /// The Group popup's anchor rectangle: the Tasks row `task`'s `Group`
    /// button, re-derived from the current layout and scroll offset every
    /// time, so it can never go stale across a resize or a scroll.
    ///
    /// A `task` scrolled out of view (the popup stays open while the list
    /// keeps scrolling) has no rectangle to anchor on; the content area's own
    /// rectangle is used instead so the popup still lands somewhere inside
    /// the window (fail closed, never a panic).
    pub(super) fn group_anchor_rect(
        &self,
        task: usize,
        layout: &SbLayout,
        scale: Scale,
        theme: &Theme,
    ) -> Rect {
        let info = self.list_info(layout, scale, theme);
        let start = usize::try_from(self.offsets[Section::Tasks.index()]).unwrap_or(0);
        if let Some(slot) = task.checked_sub(start) {
            if let Ok(slot) = u32::try_from(slot) {
                if slot < info.visible() {
                    let (_, buttons) = Self::split_row(
                        info.item_rect(slot),
                        Self::row_actions(Section::Tasks),
                        scale,
                        theme,
                    );
                    if let Some(rect) = buttons.get(1) {
                        return *rect;
                    }
                }
            }
        }
        layout.content
    }

    /// The Group popup's on-screen rectangle: `menu`'s preferred size, placed
    /// below `anchor` (or above it when there is no room below), clamped
    /// inside `bounds` so it never draws outside the window.
    pub(super) fn popup_rect(
        menu: &Menu,
        anchor: Rect,
        bounds: Rect,
        scale: Scale,
        theme: &Theme,
        font: BitmapFont,
    ) -> Rect {
        let w = menu.preferred_width(scale, theme, font).min(bounds.width);
        let h = menu.preferred_height(scale, theme).min(bounds.height);
        let max_x = bounds.left().max(bounds.right() - to_i32(w));
        let x = anchor.left().clamp(bounds.left(), max_x);
        let below = anchor.bottom();
        let y = if below + to_i32(h) <= bounds.bottom() {
            below
        } else {
            (anchor.top() - to_i32(h)).max(bounds.top())
        };
        let max_y = bounds.top().max(bounds.bottom() - to_i32(h));
        let y = y.clamp(bounds.top(), max_y);
        Rect::new(x, y, w, h)
    }

    /// Render the visible task rows and their primary action + Group buttons.
    pub(super) fn render_task_rows(
        &self,
        surface: &mut Surface,
        info: ListInfo,
        scale: Scale,
        theme: &Theme,
        font: BitmapFont,
    ) {
        let start = usize::try_from(self.offsets[self.section.index()]).unwrap_or(0);
        for slot in 0..info.visible() {
            let Some(entry) = self.tasks.get(start + slot as usize) else {
                break;
            };
            let (row_rect, buttons) = Self::split_row(
                info.item_rect(slot),
                Self::row_actions(Section::Tasks),
                scale,
                theme,
            );
            entry
                .row
                .render(surface, row_rect, scale, theme, font, None);
            if let Some(rect) = buttons.first() {
                entry.action.render(surface, *rect, scale, theme, font);
            }
            if let Some(rect) = buttons.get(1) {
                entry
                    .group_button
                    .render(surface, *rect, scale, theme, font);
            }
        }
    }

    /// Route a pointer event to the task rows (their primary action and Group
    /// buttons, and row selection).
    pub(super) fn tasks_on_pointer(
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
                Self::row_actions(Section::Tasks),
                scale,
                theme,
            );
            let Some(entry) = self.tasks.get_mut(idx) else {
                break;
            };
            if buttons.first().is_some_and(|rect| {
                entry.action.on_pointer(event, *rect) == Some(ButtonAction::Activated)
            }) {
                return Some(SwitchboardAction::Task { index: idx });
            }
            if buttons.get(1).is_some_and(|rect| {
                entry.group_button.on_pointer(event, *rect) == Some(ButtonAction::Activated)
            }) {
                self.open_group_popup(idx);
                return None;
            }
            if entry.row.on_pointer(event, row_rect) == Some(RowAction::Activated) {
                selected = Some(idx);
            }
        }
        if let Some(idx) = selected {
            for (i, entry) in self.tasks.iter_mut().enumerate() {
                entry.row.set_selected(i == idx);
            }
        }
        None
    }

    /// Route a pointer event to the open Group popup: a primary press outside
    /// its bounds dismisses it without emitting; otherwise the event feeds the
    /// popup itself.
    pub(super) fn group_popup_on_pointer(
        &mut self,
        event: &InputEvent,
        bounds: Rect,
        scale: Scale,
        theme: &Theme,
        font: BitmapFont,
    ) -> Option<SwitchboardAction> {
        let popup = self.group_popup.as_ref()?;
        let layout = self.compute_layout(bounds, scale, theme, font);
        let anchor = self.group_anchor_rect(popup.task, &layout, scale, theme);
        let popup_rect = Self::popup_rect(&popup.menu, anchor, bounds, scale, theme, font);

        if let InputEvent::PointerPressed {
            button: PointerButton::Primary,
        } = event
        {
            if popup
                .menu
                .row_at(popup_rect, scale, theme, *self.pointer)
                .is_none()
            {
                self.group_popup = None;
                return None;
            }
        }

        let popup = self.group_popup.as_mut()?;
        match popup.menu.on_pointer(event, popup_rect, scale, theme) {
            Some(MenuAction::Activated { index }) => self.resolve_group_activation(index),
            Some(MenuAction::Dismissed) => {
                self.group_popup = None;
                None
            }
            Some(MenuAction::OpenSubmenu { .. }) | None => None,
        }
    }

    /// Open the Group popup, anchored on the given task's `Group` button.
    ///
    /// The item list is built once from the current `activities` and
    /// `can_create_activity` (spec T12): each activity, disabled with a
    /// reason when it is the task's current activity or is full; then
    /// `"New activity"`, disabled when the caller may not create one; then,
    /// only when the task is already grouped, `"Remove from activity"`.
    pub(super) fn open_group_popup(&mut self, task: usize) {
        let Some(entry) = self.tasks.get(task) else {
            return;
        };
        let current = entry.group;
        let mut items: Vec<MenuItem> = self
            .activities
            .iter()
            .enumerate()
            .map(|(i, activity)| {
                let mut item = MenuItem::new(activity.name.clone());
                if current == Some(i) {
                    item = item
                        .with_state(ControlState::disabled())
                        .with_reason("Current activity");
                } else if !activity.can_accept_member {
                    item = item
                        .with_state(ControlState::disabled())
                        .with_reason("Activity is full");
                }
                item
            })
            .collect();
        let mut new_activity = MenuItem::new("New activity");
        if !self.can_create_activity {
            new_activity = new_activity
                .with_state(ControlState::disabled())
                .with_reason("Activity limit reached");
        }
        items.push(new_activity);
        if current.is_some() {
            items.push(MenuItem::new("Remove from activity"));
        }
        self.group_popup = Some(GroupPopup {
            task,
            menu: Menu::new(items),
        });
    }

    /// Map an activated Group popup row to its [`SwitchboardAction`] and
    /// close the popup.
    fn resolve_group_activation(&mut self, index: usize) -> Option<SwitchboardAction> {
        let popup = self.group_popup.take()?;
        let task = popup.task;
        match index.cmp(&self.activities.len()) {
            Ordering::Less => Some(SwitchboardAction::TaskGrouped {
                task,
                activity: Some(index),
            }),
            Ordering::Equal => Some(SwitchboardAction::TaskGrouped {
                task,
                activity: None,
            }),
            Ordering::Greater => Some(SwitchboardAction::TaskUngrouped { task }),
        }
    }

    /// Route a key to the open Group popup: arrows move its focus, Enter or
    /// Space activates the focused row, and Escape dismisses without
    /// emitting.
    pub(super) fn group_popup_on_key(&mut self, key: Key) -> Option<SwitchboardAction> {
        let action = self.group_popup.as_mut()?.menu.on_key(key);
        match action {
            Some(MenuAction::Activated { index }) => self.resolve_group_activation(index),
            Some(MenuAction::Dismissed) => {
                self.group_popup = None;
                None
            }
            Some(MenuAction::OpenSubmenu { .. }) | None => None,
        }
    }
}

#[cfg(test)]
#[path = "tasks_tests.rs"]
mod tests;
