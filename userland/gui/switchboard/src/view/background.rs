//! The Background section: jobs with known or working progress
//! (`plans/NEW-SWITCHBOARD.md` S3).
//!
//! Owns the caller's job view model ([`JobSummary`]), the [`JobControl`] a
//! job card's footer offers, and the section's layout, painting and input.

use alloc::string::String;

use tairix_font::BitmapFont;
use tairix_geometry::{Rect, Scale};
use tairix_input::InputEvent;
use tairix_raster::Surface;
use tairix_theme::Theme;

use tairix_controls::{
    ActivityState, Button, ButtonContent, Card, CardAction, ControlRole, ControlState,
};

use super::{action_state, ListInfo, Switchboard, SwitchboardAction};

/// One background job with known or working progress (spec §17).
///
/// Rendered as a [`Card`]: the job's progress drives the card's
/// Heat Seam, and its Pause/Cancel actions are footer [`Button`]s
/// that share the job's identity.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct JobSummary {
    /// The job's display name.
    pub name: String,
    /// A short body line (e.g. destination, item count).
    pub detail: String,
    /// The job's progress/activity, drawn as the card's Heat Seam.
    pub activity: ActivityState,
    /// Whether the job may be paused.
    pub can_pause: bool,
    /// Whether the job may be cancelled.
    pub can_cancel: bool,
}

/// A background-job action a Switchboard job card can request.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum JobControl {
    /// Pause the job.
    Pause,
    /// Cancel the job.
    Cancel,
}

impl Switchboard {
    /// Build a background-job card with its footer actions.
    pub(super) fn build_job(job: JobSummary) -> Card {
        let mut pause = Button::labelled("Pause");
        pause.set_state(action_state(job.can_pause));
        let mut cancel = Button::new(
            ButtonContent::Label(String::from("Cancel")),
            ControlRole::Destructive,
        );
        cancel.set_state(action_state(job.can_cancel));
        Card::new(job.name)
            .with_body(job.detail)
            .with_state(ControlState::idle().with_activity(job.activity))
            .with_footer(alloc::vec![pause, cancel])
    }

    /// Render the visible job cards.
    pub(super) fn render_job_cards(
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
            let Some(card) = self.jobs.get(start + slot as usize) else {
                break;
            };
            let item = info.item_rect(slot);
            let card_rect = Rect::new(
                item.left(),
                item.top(),
                item.width,
                item.height.saturating_sub(gap),
            );
            card.render(surface, card_rect, scale, theme, font);
        }
    }

    /// Route a pointer event to the job cards' footer actions.
    pub(super) fn jobs_on_pointer(
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
            let Some(card) = self.jobs.get_mut(idx) else {
                break;
            };
            if let Some(CardAction::FooterActivated { index }) =
                card.on_pointer(event, item, scale, theme)
            {
                return Some(SwitchboardAction::Job {
                    index: idx,
                    control: job_control(index),
                });
            }
        }
        None
    }
}
/// Map a job card's footer-button index to its typed control (0 = pause,
/// otherwise cancel), matching the footer order [`Switchboard::build_job`] lays
/// down.
pub(super) fn job_control(index: usize) -> JobControl {
    if index == 0 {
        JobControl::Pause
    } else {
        JobControl::Cancel
    }
}

#[cfg(test)]
#[path = "background_tests.rs"]
mod tests;
