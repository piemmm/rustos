//! The Background section: jobs with known or working progress
//! (`plans/NEW-SWITCHBOARD.md` S3, S4).
//!
//! Owns the caller's job view model ([`JobSummary`]), the [`JobControl`] the
//! action rail offers for the selected job, and the section's layout,
//! painting and input.
//!
//! Nothing in this system keeps a registry of background jobs — no service
//! publishes one and the System Information API has no query for one — so
//! the list is empty on every real machine and says why. The anatomy around
//! it is the shape a registry would fill, not a promise that one exists:
//! the section states the absence in the reader's own words rather than
//! drawing an empty list, which reads as "nothing is running".

use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::mem;

use tairix_font::BitmapFont;
use tairix_geometry::{to_i32, Rect, Region, Scale};
use tairix_input::{InputEvent, Key};
use tairix_raster::{Color, Surface};
use tairix_theme::Theme;

use tairix_controls::{
    ActivityState, Button, ButtonContent, Card, Chart, ControlRole, ControlState, Fact, FactList,
    Panel, PressureKind, RailAction, SelectionState, Timeline, TimelineEvent, Toggle,
};

use super::frame::{SectionAnatomy, SectionFrame, ACTION_RAIL_WIDTH, DETAIL_PANE_WIDTH};
use super::refresh::{resettle_cards, restate_rail};
use super::system_data::absence_statement;
use super::{
    action_state, resolve_selection, select_pressed_card, ActionRail, ActionVerdict, FocusSweep,
    ListInfo, SectionCtx, SectionOutcome, SectionView, SwitchboardAction, SwitchboardModel,
    Unmeasured,
};

/// One background job with known or working progress
/// (`plans/NEW-SWITCHBOARD.md`).
///
/// Rendered as a master [`Card`]; the job's progress drives the card's Heat
/// Seam, and the commands it permits are offered by the section's action
/// rail for whichever job is selected.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct JobSummary {
    /// The job's display name.
    ///
    /// This is also the job's identity while nothing registers jobs: it is
    /// what a selection is remembered by across a refresh. A registry that
    /// later issues real job ids replaces it here, and the selection rule
    /// above needs no change.
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

/// The rail's title. The rail control carries no caption of its own, so the
/// section seats it in a [`Panel`], which already defines what a titled
/// container looks like.
const RAIL_TITLE: &str = "JOB ACTIONS";

/// The detail pane's caption with nothing selected.
const DETAIL_TITLE: &str = "JOB";

/// The footer's Auto-throttle switch label.
const THROTTLE_LABEL: &str = "Auto-throttle";

/// The footer's logical height: one switch and its label.
const FOOTER_HEIGHT: u32 = 28;

/// What the section says instead of an empty list.
///
/// Two statements, because they answer two different questions: the shared
/// absence line says a reading is missing and why, and the sentence beneath
/// says what would have to exist for it to appear. A reader who sees only
/// an empty list concludes nothing is running, which is a different — and
/// unverified — claim.
fn jobs_absence() -> String {
    absence_statement("the background-job list", Unmeasured::NoInterface)
}

/// The sentence beneath the absence line, naming what is missing.
const NO_REGISTRY: &str =
    "No component in this system keeps a registry of background jobs, so there is nothing to list.";

/// The Background section: the job cards, the selected job's detail and
/// commands, the throttle switch, and the keyboard's place among them.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct JobsSection {
    /// The jobs this sample reported, in model order.
    pub(super) items: Vec<JobSummary>,
    /// One master card per job, in the same order.
    pub(super) cards: Vec<Card>,
    /// Which job is selected, by its own identity rather than by its
    /// position, so a refresh that reorders the list keeps the reader on the
    /// job they chose.
    pub(super) selected: Option<String>,
    /// The selected job's commands.
    pub(super) rail: ActionRail,
    /// The plate the rail is seated in, which carries its caption.
    pub(super) rail_panel: Panel,
    /// The footer's Auto-throttle switch.
    pub(super) throttle: Toggle,
    /// Where the content cursor is: a job card, then a rail command.
    pub(super) focus: usize,
    /// Which of the focused stop's own actions the cursor is on.
    pub(super) action: usize,
}

/// Which kind of thing the content cursor is on.
///
/// The cursor is one flat run of stops over two different kinds of control,
/// and a refresh that changes the number of jobs moves where the rail
/// begins. Naming the kinds lets a refresh put the cursor back on the same
/// *kind* of thing rather than on whatever now sits at the old number.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
enum Stop {
    /// The job card at this position.
    Card(usize),
    /// The rail command at this position.
    Rail(usize),
}

/// The rectangles the detail pane's parts occupy, resolved once so the
/// paint and the hit test cannot disagree about where a part is.
#[derive(Copy, Clone, Debug)]
struct DetailLayout {
    /// The job's name and how far through it is.
    headline: Rect,
    /// The throughput trace.
    chart: Rect,
    /// The leading column of facts.
    facts_left: Rect,
    /// The trailing column of facts.
    facts_right: Rect,
    /// The job's marks.
    timeline: Rect,
}

impl JobsSection {
    /// An empty Background section: no jobs, cursor at the top.
    pub(super) fn new() -> Self {
        Self {
            items: Vec::new(),
            cards: Vec::new(),
            selected: None,
            rail: ActionRail::new(Vec::new()),
            rail_panel: Panel::new(RAIL_TITLE),
            throttle: throttle_switch(),
            focus: 0,
            action: 0,
        }
    }

    /// The position of the selected job in the current list, or [`None`]
    /// when nothing is selected.
    pub(super) fn selected_index(&self) -> Option<usize> {
        let name = self.selected.as_deref()?;
        self.items.iter().position(|item| item.name == name)
    }

    /// The selected job, or [`None`] when nothing is selected.
    pub(super) fn selected_item(&self) -> Option<&JobSummary> {
        self.items.get(self.selected_index()?)
    }

    /// Which kind of stop the cursor at `index` is on, or [`None`] past the
    /// last stop (fail closed).
    fn stop_at(&self, index: usize) -> Option<Stop> {
        if self.items.is_empty() {
            return None;
        }
        if index < self.items.len() {
            return Some(Stop::Card(index));
        }
        let slot = index.saturating_sub(self.items.len());
        (slot < self.rail.len()).then_some(Stop::Rail(slot))
    }

    /// Select the job at `row`, if there is one.
    fn select_row(&mut self, row: usize) {
        if let Some(item) = self.items.get(row) {
            self.selected = Some(item.name.clone());
            self.rebuild_selection();
        }
    }

    /// Rebuild everything that depends on *which* job is selected: the
    /// cards' selection marks and the rail's commands.
    fn rebuild_selection(&mut self) {
        let selected = self.selected.clone();
        for (card, item) in self.cards.iter_mut().zip(self.items.iter()) {
            card.set_state(card_state(item, selected.as_deref() == Some(&item.name)));
        }
        let commands = match self.selected_item() {
            Some(item) => alloc::vec![pause_button(item), cancel_button(item)],
            None => Vec::new(),
        };
        restate_rail(&mut self.rail, commands);
    }

    /// The rail's own content rectangle inside its plate, or [`None`] when
    /// the frame dropped the rail under width pressure.
    pub(super) fn rail_content(
        &self,
        frame: &SectionFrame,
        scale: Scale,
        theme: &Theme,
    ) -> Option<Rect> {
        self.rail_panel.content_rect(frame.rail?, scale, theme)
    }

    /// Where the detail pane's parts sit inside `content`, or [`None`] when
    /// the pane is too small to seat even its headline.
    fn detail_layout(
        content: Rect,
        scale: Scale,
        theme: &Theme,
        font: BitmapFont,
    ) -> Option<DetailLayout> {
        let gap = scale.scale_length(theme.metrics().control_gap);
        let line = font.line_height();
        let facts_h = FactList::row_height(scale, theme).saturating_mul(3);
        let mut top = content.top();
        let mut left = content.height;
        let mut take = |height: u32| -> Rect {
            let height = height.min(left);
            let rect = Rect::new(content.left(), top, content.width, height);
            top = top.saturating_add(to_i32(height));
            left = left.saturating_sub(height);
            let spent = gap.min(left);
            top = top.saturating_add(to_i32(spent));
            left = left.saturating_sub(spent);
            rect
        };
        let headline = take(line);
        if headline.is_empty() {
            return None;
        }
        let chart = take(line.saturating_mul(3));
        let facts = take(facts_h);
        let timeline = Rect::new(content.left(), top, content.width, left);
        let column = facts.width.saturating_sub(gap) / 2;
        Some(DetailLayout {
            headline,
            chart,
            facts_left: Rect::new(facts.left(), facts.top(), column, facts.height),
            facts_right: Rect::new(
                facts
                    .left()
                    .saturating_add(to_i32(column.saturating_add(gap))),
                facts.top(),
                column,
                facts.height,
            ),
            timeline,
        })
    }

    /// Paint the master list, or the statement that stands in for it.
    fn render_primary(&self, surface: &mut Surface, ctx: SectionCtx<'_>) {
        if self.items.is_empty() {
            render_absence(surface, ctx.frame.primary, ctx);
            return;
        }
        let info = self.list_info(&ctx.frame, ctx.scale, ctx.theme);
        for slot in 0..info.visible() {
            let Some(card) = self.cards.get(ctx.start + slot as usize) else {
                break;
            };
            card.render(surface, info.item_rect(slot), ctx.scale, ctx.theme);
        }
    }

    /// Paint the selected job's detail pane.
    fn render_detail(&self, surface: &mut Surface, ctx: SectionCtx<'_>) {
        let Some(rect) = ctx.frame.detail else {
            return;
        };
        let panel = Panel::new(self.selected_item().map_or(DETAIL_TITLE, |item| &item.name));
        panel.render(surface, rect, ctx.scale, ctx.theme);
        let Some(content) = panel.content_rect(rect, ctx.scale, ctx.theme) else {
            return;
        };
        let Some(item) = self.selected_item() else {
            render_absence(surface, content, ctx);
            return;
        };
        let Some(layout) = Self::detail_layout(content, ctx.scale, ctx.theme, ctx.font) else {
            return;
        };
        ctx.font.draw_text(
            surface,
            layout.headline.left(),
            layout.headline.top(),
            &headline_text(item),
            Color::from(ctx.theme.palette().on_surface),
        );
        throughput_chart(item).render(surface, layout.chart, ctx.scale, ctx.theme);
        let (left, right) = detail_facts(item);
        left.render(surface, layout.facts_left, ctx.scale, ctx.theme);
        right.render(surface, layout.facts_right, ctx.scale, ctx.theme);
        job_timeline(item).render(surface, layout.timeline, ctx.scale, ctx.theme);
    }

    /// Paint the action rail inside its titled plate.
    fn render_rail(&self, surface: &mut Surface, ctx: SectionCtx<'_>) {
        let Some(rail) = ctx.frame.rail else {
            return;
        };
        self.rail_panel.render(surface, rail, ctx.scale, ctx.theme);
        if let Some(content) = self.rail_panel.content_rect(rail, ctx.scale, ctx.theme) {
            self.rail.render(surface, content, ctx.scale, ctx.theme);
        }
    }
}

/// Paint an absence statement and its explanation into `rect`.
///
/// Both lines or neither: a box too short for the explanation still says a
/// reading is missing, which is the part a reader must not be left to infer.
fn render_absence(surface: &mut Surface, rect: Rect, ctx: SectionCtx<'_>) {
    if rect.is_empty() {
        return;
    }
    let muted = Color::from(ctx.theme.palette().on_surface_muted);
    ctx.font
        .draw_text(surface, rect.left(), rect.top(), &jobs_absence(), muted);
    let second = rect.top().saturating_add(to_i32(ctx.font.line_height()));
    if second < rect.bottom() {
        ctx.font
            .draw_text(surface, rect.left(), second, NO_REGISTRY, muted);
    }
}

/// The footer's Auto-throttle switch.
///
/// It is off and disabled: throttling a job needs a registry to name the job
/// to and a service to ask, and neither exists. A switch a reader could flip
/// to no effect would be a lie about what the system can do, so it wears the
/// refusal instead — as a plainly disabled control, not the Authority Mark,
/// because the caller's authority is not what is missing.
fn throttle_switch() -> Toggle {
    let mut throttle = Toggle::new(THROTTLE_LABEL, false);
    throttle.set_state(ActionVerdict::DisabledByState.to_state());
    throttle
}

/// A master card's state: its progress, and whether it is the selected job.
fn card_state(item: &JobSummary, selected: bool) -> ControlState {
    let state = ControlState::idle().with_activity(item.activity);
    if selected {
        state.with_selection(SelectionState::Selected)
    } else {
        state
    }
}

/// One job's master card: what is running and what it is working on.
fn build_card(item: &JobSummary, selected: bool) -> Card {
    Card::new(item.name.clone())
        .with_body(item.detail.clone())
        .with_state(card_state(item, selected))
}

/// The Pause command for `item`.
fn pause_button(item: &JobSummary) -> Button {
    let mut pause = Button::labelled("Pause");
    pause.set_state(action_state(item.can_pause));
    pause
}

/// The Cancel command for `item`.
fn cancel_button(item: &JobSummary) -> Button {
    let mut cancel = Button::new(
        ButtonContent::Label(String::from("Cancel")),
        ControlRole::Destructive,
    );
    cancel.set_state(action_state(item.can_cancel));
    cancel
}

/// The detail pane's headline: the job's name and how far through it is.
fn headline_text(item: &JobSummary) -> String {
    match item.activity {
        ActivityState::Progress(value) => {
            alloc::format!("{} · {}%", item.name, value.permille() / 10)
        }
        _ => alloc::format!("{} · working", item.name),
    }
}

/// The job's throughput trace.
///
/// A job reports its progress, not its rate, so the trace plots the one
/// series the model actually carries rather than a rate derived from it.
fn throughput_chart(item: &JobSummary) -> Chart {
    let chart = Chart::new(PressureKind::Disk);
    match item.activity {
        ActivityState::Progress(value) => chart.with_samples([value.permille()]),
        _ => chart,
    }
}

/// The detail pane's two columns of facts.
fn detail_facts(item: &JobSummary) -> (FactList, FactList) {
    (
        FactList::new(alloc::vec![
            Fact::new("Job", item.name.clone()),
            Fact::new("Doing", item.detail.clone()),
        ]),
        FactList::new(alloc::vec![
            Fact::new("Pause", permitted(item.can_pause).to_string()),
            Fact::new("Cancel", permitted(item.can_cancel).to_string()),
        ]),
    )
}

/// Whether a command is offered to this caller, in words.
const fn permitted(allowed: bool) -> &'static str {
    if allowed {
        "permitted"
    } else {
        "not permitted"
    }
}

/// The job's marks.
///
/// Only what the model carries: a job that reports progress has started, and
/// that is the one mark this service can attest. A history nothing recorded
/// is not invented.
fn job_timeline(item: &JobSummary) -> Timeline {
    Timeline::new(alloc::vec![TimelineEvent::new(
        String::from("when observed"),
        alloc::format!("Running: {}", item.detail),
    )])
}

impl SectionView for JobsSection {
    fn anatomy(&self) -> SectionAnatomy {
        SectionAnatomy {
            band_summary: None,
            sidebar_width: 0,
            header_height: 0,
            detail_width: DETAIL_PANE_WIDTH,
            impact_width: 0,
            rail_width: ACTION_RAIL_WIDTH,
            footer_height: FOOTER_HEIGHT,
            primary_row_commands: 0,
        }
    }

    /// Adopt a fresh sample, keeping the reader on the job they chose.
    ///
    /// The list is rebuilt from scratch every sample, so the selection is
    /// re-resolved against the job's own identity through the one shared
    /// rule: a job that is still running stays selected however far it has
    /// moved, and only a job that has genuinely finished loses it.
    fn adopt(&mut self, model: &SwitchboardModel) {
        let previous = self.selected.take();
        let stop = self.stop_at(self.focus);
        self.items.clone_from(&model.jobs);
        self.selected = resolve_selection(
            previous.as_deref(),
            self.items.iter().map(|item| item.name.as_str()),
        )
        .map(String::from);
        let retired = mem::take(&mut self.cards);
        self.cards = self
            .items
            .iter()
            .map(|item| {
                let selected = self.selected.as_deref() == Some(&item.name);
                build_card(item, selected)
            })
            .collect();
        self.rebuild_selection();
        // A card holds its own record of which footer action the pointer is on
        // and which one a press began on, neither of which can be restated
        // from outside, so a card the sample did not change is kept.
        resettle_cards(retired, &mut self.cards);

        self.focus = match stop {
            Some(Stop::Rail(slot)) => self
                .items
                .len()
                .saturating_add(slot.min(self.rail.len().saturating_sub(1))),
            Some(Stop::Card(_)) | None => self.selected_index().unwrap_or(0),
        };
        self.focus = self.focus.min(self.focus_span().saturating_sub(1));
        self.action = 0;
    }

    fn item_count(&self) -> usize {
        self.items.len()
    }

    /// The cursor walks the job cards, then the selected job's commands.
    /// With nothing running there is nothing to detail and nothing to
    /// command, so there are no stops at all.
    fn focus_span(&self) -> usize {
        if self.items.is_empty() {
            return 0;
        }
        self.items.len().saturating_add(self.rail.len())
    }

    fn focus_row(&self, index: usize) -> Option<usize> {
        match self.stop_at(index) {
            Some(Stop::Card(row)) => Some(row),
            _ => None,
        }
    }

    fn list_info(&self, frame: &SectionFrame, scale: Scale, theme: &Theme) -> ListInfo {
        ListInfo::cards(frame.primary, self.items.len(), scale, theme)
    }

    /// Zero: a job's commands live in the anchored rail beside the list, not
    /// inside its card.
    fn row_buttons(&self) -> u32 {
        0
    }

    fn focused_action_count(&self) -> usize {
        1
    }

    fn content_focus(&self) -> usize {
        self.focus
    }

    /// Move the cursor, selecting the job a card stop names so the detail
    /// and the rail always describe the card the reader is on.
    fn set_content_focus(&mut self, index: usize) {
        self.focus = index;
        if let Some(Stop::Card(row)) = self.stop_at(index) {
            self.select_row(row);
        }
    }

    fn row_action(&self) -> usize {
        self.action
    }

    fn set_row_action(&mut self, index: usize, _sweep: &mut FocusSweep<'_, '_>) {
        self.action = index;
    }

    /// Commit the focused stop.
    ///
    /// A rail stop hands the key to the rail, which forwards it to the
    /// focused button so the button decides for itself whether it may fire:
    /// a command this caller may not take refuses the keyboard exactly as it
    /// refuses the pointer.
    fn activate_focused(
        &mut self,
        key: Key,
        ctx: SectionCtx<'_>,
        damage: &mut Region,
    ) -> Option<SectionOutcome> {
        match self.stop_at(self.focus)? {
            Stop::Card(row) => {
                self.select_row(row);
                None
            }
            Stop::Rail(slot) => {
                let index = self.selected_index()?;
                let rail = self
                    .rail_content(&ctx.frame, ctx.scale, ctx.theme)
                    .unwrap_or(Rect::EMPTY);
                self.rail.set_focus(Some(slot), rail, damage);
                let RailAction::Activate { index: fired } = self.rail.on_key(key, rail, damage)?;
                Some(SectionOutcome::Action(SwitchboardAction::Job {
                    index,
                    control: job_control(fired)?,
                }))
            }
        }
    }

    fn render(&self, surface: &mut Surface, ctx: SectionCtx<'_>) {
        self.render_primary(surface, ctx);
        self.render_detail(surface, ctx);
        self.render_rail(surface, ctx);
        let band = ctx.frame.footer;
        if !band.is_empty() {
            self.throttle.render(surface, band, ctx.scale, ctx.theme);
        }
    }

    /// Route a pointer event to the job cards, then the action rail.
    ///
    /// A card that reports any interaction — a body press or (once one
    /// carries footer buttons) a footer click — becomes the selected job, so
    /// pressing a card opens its detail.
    fn on_pointer(
        &mut self,
        event: &InputEvent,
        ctx: SectionCtx<'_>,
        damage: &mut Region,
    ) -> Option<SectionOutcome> {
        let info = self.list_info(&ctx.frame, ctx.scale, ctx.theme);
        let chosen = select_pressed_card(&info, ctx.start, |index, rect| {
            self.cards
                .get_mut(index)?
                .on_pointer(event, rect, ctx.scale, ctx.theme, damage)
        });
        if let Some((row, _)) = chosen {
            self.focus = row;
            self.select_row(row);
            return None;
        }

        let index = self.selected_index()?;
        let rail = self.rail_content(&ctx.frame, ctx.scale, ctx.theme)?;
        let RailAction::Activate { index: fired } = self
            .rail
            .on_pointer(event, rail, ctx.scale, ctx.theme, damage)?;
        Some(SectionOutcome::Action(SwitchboardAction::Job {
            index,
            control: job_control(fired)?,
        }))
    }

    fn apply_focus_marks(&mut self, focused: bool, sweep: &mut FocusSweep<'_, '_>) {
        let stop = focused.then(|| self.stop_at(self.focus)).flatten();
        for (i, card) in self.cards.iter_mut().enumerate() {
            card.set_in_focus_field(stop == Some(Stop::Card(i)));
        }
        let slot = match stop {
            Some(Stop::Rail(slot)) => Some(slot),
            _ => None,
        };
        let rail = sweep
            .ctx
            .and_then(|ctx| self.rail_content(&ctx.frame, ctx.scale, ctx.theme));
        sweep.rail(&mut self.rail, slot, rail);
        for (index, button) in self.rail.items_mut().iter_mut().enumerate() {
            button.set_focused(slot == Some(index));
            button.set_in_focus_field(slot.is_some());
        }
    }
}

/// The command a rail slot names, or [`None`] for a slot the rail does not
/// have (fail closed).
fn job_control(slot: usize) -> Option<JobControl> {
    match slot {
        0 => Some(JobControl::Pause),
        1 => Some(JobControl::Cancel),
        _ => None,
    }
}

#[cfg(test)]
#[path = "background_tests.rs"]
mod tests;
