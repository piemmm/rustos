//! The Pressure section: the flagged resource-pressure causes and the relief
//! actions each recommends (`plans/NEW-SWITCHBOARD.md` S3).
//!
//! Owns the caller's cause view model ([`PressureCause`]), the
//! [`PressureControl`]/[`PressureAction`] vocabulary a cause card offers, and
//! the section's layout, painting and input.

use alloc::string::String;
use alloc::vec::Vec;

use tairix_font::BitmapFont;
use tairix_geometry::{Rect, Scale};
use tairix_input::InputEvent;
use tairix_raster::Surface;
use tairix_theme::Theme;

use tairix_controls::{
    ActivityState, Button, ButtonContent, Card, CardAction, ControlRole, ControlState,
    PressureKind, PressureState,
};

use super::{ActionVerdict, ListInfo, Section, Switchboard, SwitchboardAction};

/// A relief action a Switchboard pressure card can recommend or offer
/// (spec `plans/NEW-TASKBAR.md` T12).
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum PressureControl {
    /// Pause the culprit.
    Pause,
    /// Lower the culprit's scheduling priority.
    LowerPriority,
    /// Show the culprit on the Tasks section, focused.
    ShowTasks,
}

/// One footer action on a pressure [`Card`] (spec T12).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PressureAction {
    /// The action's button label.
    pub label: String,
    /// Which relief action this is.
    pub control: PressureControl,
    /// Whether the action is available, and if not, why.
    pub verdict: ActionVerdict,
    /// Whether this is the model's recommended action (Action Warmth,
    /// [`ControlRole::Recommended`]); every other action stays
    /// [`ControlRole::Neutral`].
    pub recommended: bool,
}

/// One cause of resource pressure, shown as a Pressure section [`Card`]
/// (spec T12).
///
/// The card's title is [`culprit`](Self::culprit) and its body is
/// [`cause`](Self::cause); its leading rail and heat seam come from `kind`
/// and `activity`, and its footer is one [`Button`] per
/// [`action`](Self::actions).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PressureCause {
    /// The pressured resource's display name (e.g. "Memory").
    pub resource: String,
    /// Which resource this is, driving the card's semantic Pressure Rail.
    pub kind: PressureKind,
    /// The object responsible, in plain language (the card's title).
    pub culprit: String,
    /// A plain-language explanation of the pressure (the card's body).
    pub cause: String,
    /// The culprit's live rate of work, drawn as the card's Heat Seam.
    pub activity: ActivityState,
    /// The culprit's index within [`SwitchboardModel::tasks`](super::SwitchboardModel::tasks), if it is a
    /// task, so [`PressureControl::ShowTasks`] can focus it.
    pub task_index: Option<usize>,
    /// The recommended and alternative relief actions.
    pub actions: Vec<PressureAction>,
}

/// One pressure cause rendered as a [`Card`], plus the cause's own relief
/// actions so a footer activation can be mapped back to its
/// [`ActionVerdict`] and [`PressureControl`] without the model.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct PressureEntry {
    pub(super) card: Card,
    pub(super) actions: Vec<PressureAction>,
    pub(super) task_index: Option<usize>,
}

impl Switchboard {
    /// Build a pressure cause's card, with one footer button per relief
    /// action.
    pub(super) fn build_pressure(cause: PressureCause) -> PressureEntry {
        let PressureCause {
            resource: _,
            kind,
            culprit,
            cause: cause_text,
            activity,
            task_index,
            actions,
        } = cause;
        let footer = actions
            .iter()
            .map(|action| {
                let role = if action.recommended {
                    ControlRole::Recommended
                } else {
                    ControlRole::Neutral
                };
                let mut button = Button::new(ButtonContent::Label(action.label.clone()), role);
                button.set_state(action.verdict.to_state());
                button
            })
            .collect();
        let card = Card::new(culprit)
            .with_body(cause_text)
            .with_state(
                ControlState::idle()
                    .with_pressure(PressureState::Under(kind))
                    .with_activity(activity),
            )
            .with_footer(footer);
        PressureEntry {
            card,
            actions,
            task_index,
        }
    }

    /// Render the visible pressure cards.
    pub(super) fn render_pressure_cards(
        &self,
        surface: &mut Surface,
        info: ListInfo,
        scale: Scale,
        theme: &Theme,
        font: BitmapFont,
    ) {
        let gap = scale.scale_length(theme.metrics().control_gap);
        let start = usize::try_from(self.offsets[self.section.index()]).unwrap_or(0);
        for slot in 0..info.visible() {
            let Some(entry) = self.pressure.get(start + slot as usize) else {
                break;
            };
            let item = info.item_rect(slot);
            let card_rect = Rect::new(
                item.left(),
                item.top(),
                item.width,
                item.height.saturating_sub(gap),
            );
            entry.card.render(surface, card_rect, scale, theme, font);
        }
    }

    /// Route a pointer event to the pressure cards' footer relief actions.
    pub(super) fn pressure_on_pointer(
        &mut self,
        event: &InputEvent,
        info: ListInfo,
        start: usize,
        scale: Scale,
        theme: &Theme,
    ) -> Option<SwitchboardAction> {
        for slot in 0..info.visible() {
            let idx = start + slot as usize;
            let item = info.item_rect(slot);
            let Some(entry) = self.pressure.get_mut(idx) else {
                break;
            };
            if let Some(CardAction::FooterActivated { index }) =
                entry.card.on_pointer(event, item, scale, theme)
            {
                return self.resolve_pressure_footer(idx, index);
            }
        }
        None
    }

    /// Map a pressure card's activated footer button to its
    /// [`SwitchboardAction`], failing closed unless the action's verdict is
    /// [`ActionVerdict::Ready`] (the button's own state already refuses
    /// activation, but the verdict is checked again here rather than trusted
    /// implicitly).
    ///
    /// [`PressureControl::ShowTasks`] is resolved internally: it runs the
    /// section transition to [`Section::Tasks`], focuses the cause's task,
    /// and reports that transition's [`SwitchboardAction::SectionChanged`].
    pub(super) fn resolve_pressure_footer(
        &mut self,
        cause: usize,
        action_index: usize,
    ) -> Option<SwitchboardAction> {
        let entry = self.pressure.get(cause)?;
        let action = entry.actions.get(action_index)?;
        if action.verdict != ActionVerdict::Ready {
            return None;
        }
        let control = action.control;
        let task_index = entry.task_index;
        match control {
            PressureControl::Pause | PressureControl::LowerPriority => {
                Some(SwitchboardAction::Pressure {
                    index: cause,
                    control,
                })
            }
            PressureControl::ShowTasks => self.resolve_show_tasks(task_index),
        }
    }

    /// Run the one section transition to [`Section::Tasks`], focus
    /// `task_index` (clamped into range; `None` focuses the first task), and
    /// report the transition's [`SwitchboardAction::SectionChanged`].
    fn resolve_show_tasks(&mut self, task_index: Option<usize>) -> Option<SwitchboardAction> {
        let action = self.select_section_index(Section::Tasks.index());
        self.content_focus = task_index
            .unwrap_or(0)
            .min(self.tasks.len().saturating_sub(1));
        self.row_action = 0;
        self.ensure_focus_visible();
        self.apply_focus_marks();
        action
    }
}

#[cfg(test)]
#[path = "pressure_tests.rs"]
mod tests;
