//! The Pressure section: the flagged resource-pressure causes and the relief
//! actions each recommends (`plans/NEW-SWITCHBOARD.md` S3).
//!
//! Owns the caller's cause view model ([`PressureCause`]), the
//! [`PressureControl`]/[`PressureAction`] vocabulary a cause card offers, and
//! the section's layout, painting and input.

use alloc::string::String;
use alloc::vec::Vec;

use tairix_font::BitmapFont;
use tairix_geometry::{to_i32, Rect, Scale};
use tairix_input::{InputEvent, Key};
use tairix_raster::{Color, Surface};
use tairix_theme::Theme;

use tairix_controls::{
    ActivityState, Button, ButtonContent, Card, CardAction, ControlRole, ControlState, Fact,
    FactList, Panel, PressureKind, PressureState, SelectionState,
};

use super::frame::{SectionAnatomy, SectionFrame, DETAIL_PANE_WIDTH};
use super::system_data::{reading_text, selection_prompt, Reading};
use super::{
    resolve_selection, select_pressed_card, ActionVerdict, ListInfo, SectionCtx, SectionOutcome,
    SectionView, SwitchboardAction, SwitchboardModel,
};

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
/// [`action`](Self::actions). Selecting the card opens the detail pane,
/// which restates [`amount`](Self::amount) and [`since`](Self::since) as
/// facts rather than the card's own prose, so a reader who wants the raw
/// figures has them without parsing [`cause`](Self::cause)'s sentence.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PressureCause {
    /// The pressured resource's display name (e.g. "Memory").
    pub resource: String,
    /// Which resource this is, driving the card's semantic Pressure Rail.
    ///
    /// Also this cause's selection identity: the service raises at most one
    /// cause per resource, so a resource that stays pressured across a
    /// refresh is the same cause a reader had open, and one that clears
    /// takes its selection with it.
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
    /// How pressured the whole resource is right now (e.g. "92%"), or why
    /// that cannot be said.
    pub amount: Reading,
    /// How long this resource has stood in its current pressure band, or
    /// why that cannot be said.
    pub since: Reading,
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

/// The detail pane's caption with nothing selected.
const DETAIL_TITLE: &str = "CAUSE";

/// What the Relief fact says for a cause whose model recommends nothing.
///
/// A cause always offers Show tasks, but that is a way to *look*, not a
/// relief; saying so is honest where naming it as the recommendation would
/// imply the pressure would ease.
const NO_RELIEF: &str = "None recommended";

/// The rectangles the detail pane's parts occupy, resolved once so the
/// pane's parts cannot be laid out two different ways.
#[derive(Copy, Clone, Debug)]
struct DetailLayout {
    /// The culprit's name and the resource it is straining.
    identity: Rect,
    /// The cause's four facts.
    facts: Rect,
}

/// The Pressure section: the cause cards, the selected cause's detail, and
/// the keyboard's place among them.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct PressureSection {
    /// The causes this sample flagged, in model order, so the detail pane
    /// can state the selected cause's figures rather than re-deriving them
    /// from its card's prose.
    pub(super) items: Vec<PressureCause>,
    /// One card plus its relief actions per cause, in the same order.
    pub(super) entries: Vec<PressureEntry>,
    /// Which cause is selected, by the resource it is about rather than by
    /// its position: the service raises at most one cause per resource, so
    /// a resource that is still pressured after a refresh keeps the reader
    /// on the cause they opened however far it has moved, and one that has
    /// eased takes its selection with it.
    pub(super) selected: Option<PressureKind>,
    /// Which card the content cursor is on.
    pub(super) focus: usize,
    /// Which of the focused card's footer actions the cursor is on.
    pub(super) action: usize,
}

impl PressureSection {
    /// An empty Pressure section: nothing flagged, cursor at the top.
    pub(super) fn new() -> Self {
        Self {
            items: Vec::new(),
            entries: Vec::new(),
            selected: None,
            focus: 0,
            action: 0,
        }
    }

    /// The position of the selected cause in the current list, or [`None`]
    /// when nothing is selected (an empty list, or a selection whose
    /// resource has eased).
    pub(super) fn selected_index(&self) -> Option<usize> {
        let kind = self.selected?;
        self.items.iter().position(|item| item.kind == kind)
    }

    /// The selected cause, or [`None`] when nothing is selected.
    pub(super) fn selected_item(&self) -> Option<&PressureCause> {
        self.items.get(self.selected_index()?)
    }

    /// Select the cause at `row`, if there is one, and mark its card.
    fn select_row(&mut self, row: usize) {
        if let Some(item) = self.items.get(row) {
            self.selected = Some(item.kind);
            self.mark_selection();
        }
    }

    /// Put the selection mark on the selected cause's card and take it off
    /// every other, so the card the detail pane describes is the one that
    /// looks chosen.
    fn mark_selection(&mut self) {
        let selected = self.selected;
        for (entry, item) in self.entries.iter_mut().zip(self.items.iter()) {
            entry
                .card
                .set_state(card_state(item, selected == Some(item.kind)));
        }
    }

    /// Where the detail pane's parts sit inside `content`, or [`None`] when
    /// the pane is too small to seat even its identity line.
    fn detail_layout(
        content: Rect,
        scale: Scale,
        theme: &Theme,
        font: BitmapFont,
    ) -> Option<DetailLayout> {
        let gap = scale.scale_length(theme.metrics().control_gap);
        let line = font.line_height();
        let identity = Rect::new(
            content.left(),
            content.top(),
            content.width,
            line.min(content.height),
        );
        if identity.is_empty() {
            return None;
        }
        let spent = line.saturating_add(gap).min(content.height);
        let facts = Rect::new(
            content.left(),
            content.top().saturating_add(to_i32(spent)),
            content.width,
            content.height.saturating_sub(spent),
        );
        Some(DetailLayout { identity, facts })
    }

    /// The plate the detail pane draws in: its caption is the pressured
    /// resource, so the pane says which cause it is describing.
    fn detail_panel(&self) -> Panel {
        Panel::new(
            self.selected_item()
                .map_or(DETAIL_TITLE, |item| &item.resource),
        )
    }

    /// Paint the selected cause's detail pane.
    fn render_detail(&self, surface: &mut Surface, ctx: SectionCtx<'_>) {
        let Some(rect) = ctx.frame.detail else {
            return;
        };
        let panel = self.detail_panel();
        panel.render(surface, rect, ctx.scale, ctx.theme, ctx.font);
        let Some(content) = panel.content_rect(rect, ctx.scale, ctx.theme) else {
            return;
        };
        let muted = Color::from(ctx.theme.palette().on_surface_muted);
        let Some(item) = self.selected_item() else {
            ctx.font.draw_text(
                surface,
                content.left(),
                content.top(),
                &selection_prompt("a cause"),
                muted,
            );
            return;
        };
        let Some(layout) = Self::detail_layout(content, ctx.scale, ctx.theme, ctx.font) else {
            return;
        };
        ctx.font.draw_text(
            surface,
            layout.identity.left(),
            layout.identity.top(),
            &identity_text(item),
            Color::from(ctx.theme.palette().on_surface),
        );
        detail_facts(item).render(surface, layout.facts, ctx.scale, ctx.theme, ctx.font);
    }

    /// Build a pressure cause's card, with one footer button per relief
    /// action, marked as `selected` when it is the cause the detail pane
    /// describes.
    fn build(cause: &PressureCause, selected: bool) -> PressureEntry {
        let footer = cause
            .actions
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
        let card = Card::new(cause.culprit.clone())
            .with_body(cause.cause.clone())
            .with_state(card_state(cause, selected))
            .with_footer(footer);
        PressureEntry {
            card,
            actions: cause.actions.clone(),
            task_index: cause.task_index,
        }
    }

    /// Map a pressure card's activated footer button to its outcome, failing
    /// closed unless the action's verdict is [`ActionVerdict::Ready`] (the
    /// button's own state already refuses activation, but the verdict is
    /// checked again here rather than trusted implicitly).
    ///
    /// [`PressureControl::ShowTasks`] is not this section's to perform: it
    /// asks the screen to show the Tasks section with the culprit focused,
    /// which is the one section transition every other route runs too.
    fn resolve_footer(&self, cause: usize, action_index: usize) -> Option<SectionOutcome> {
        let entry = self.entries.get(cause)?;
        let action = entry.actions.get(action_index)?;
        if action.verdict != ActionVerdict::Ready {
            return None;
        }
        let control = action.control;
        match control {
            PressureControl::Pause | PressureControl::LowerPriority => {
                Some(SectionOutcome::Action(SwitchboardAction::Pressure {
                    index: cause,
                    control,
                }))
            }
            PressureControl::ShowTasks => Some(SectionOutcome::ShowTask {
                task: entry.task_index,
            }),
        }
    }
}

/// A cause card's state: the resource it is straining, the culprit's live
/// rate of work, and whether it is the selected cause.
///
/// The one place a card's state is composed, so a card built by a refresh
/// and a card re-marked by a selection can never disagree about what else
/// the card is showing.
fn card_state(cause: &PressureCause, selected: bool) -> ControlState {
    let state = ControlState::idle()
        .with_pressure(PressureState::Under(cause.kind))
        .with_activity(cause.activity);
    if selected {
        state.with_selection(SelectionState::Selected)
    } else {
        state
    }
}

/// The detail pane's identity line: who is straining what.
fn identity_text(cause: &PressureCause) -> String {
    alloc::format!("{} · {}", cause.culprit, cause.resource)
}

/// The detail pane's facts: what is pressured, by how much, how long it has
/// stood in that band, and what to do about it.
///
/// Every figure is the reading the service measured or the statement that
/// there is none — the card's own prose sentence is never re-parsed to
/// recover a number from it.
fn detail_facts(cause: &PressureCause) -> FactList {
    FactList::new(alloc::vec![
        Fact::new("Resource", cause.resource.clone()),
        Fact::new("Pressure", reading_text(&cause.amount)),
        Fact::new("In band", reading_text(&cause.since)),
        Fact::new("Relief", recommended_relief(cause)),
    ])
}

/// What the pane recommends doing about a cause: the model's own
/// recommended relief, and — where the caller cannot take it — why not.
///
/// A refused command is named with its refusal rather than hidden, so a
/// reader is told what would relieve the pressure and that this session may
/// not do it; the command itself still fails closed at its button.
fn recommended_relief(cause: &PressureCause) -> String {
    let Some(action) = cause.actions.iter().find(|action| action.recommended) else {
        return String::from(NO_RELIEF);
    };
    match action.verdict {
        ActionVerdict::Ready => action.label.clone(),
        ActionVerdict::DisabledByState => {
            alloc::format!("{} — not available in this state", action.label)
        }
        ActionVerdict::DeniedByAuthority => alloc::format!("{} — not permitted", action.label),
    }
}

impl SectionView for PressureSection {
    /// A master list of cause cards with the selected cause's detail beside
    /// it. There is no action rail: a cause's relief commands live in its
    /// own card footer, where the card that offers them is unambiguous.
    fn anatomy(&self) -> SectionAnatomy {
        SectionAnatomy {
            band_summary: None,
            sidebar_width: 0,
            header_height: 0,
            detail_width: DETAIL_PANE_WIDTH,
            impact_width: 0,
            rail_width: 0,
            footer_height: 0,
            primary_row_commands: 0,
        }
    }

    /// Adopt a fresh sample, keeping the reader on the cause they opened.
    ///
    /// The list is rebuilt every sample, so the selection is re-resolved
    /// against the resource each cause is about: a resource that is still
    /// pressured stays selected however far its card has moved, and only a
    /// resource that has eased loses the selection. The cursor follows the
    /// selected card for the same reason, rather than staying on a number
    /// that now names a different cause.
    fn adopt(&mut self, model: &SwitchboardModel) {
        let previous = self.selected;
        self.items.clone_from(&model.pressure);
        self.selected = resolve_selection(previous, self.items.iter().map(|item| item.kind));
        self.entries = self
            .items
            .iter()
            .map(|item| Self::build(item, self.selected == Some(item.kind)))
            .collect();
        self.focus = self
            .selected_index()
            .unwrap_or(0)
            .min(self.entries.len().saturating_sub(1));
        self.action = 0;
    }

    fn item_count(&self) -> usize {
        self.entries.len()
    }

    fn list_info(&self, frame: &SectionFrame, scale: Scale, theme: &Theme) -> ListInfo {
        ListInfo::cards(frame.primary, self.entries.len(), scale, theme)
    }

    fn row_buttons(&self) -> u32 {
        0
    }

    fn focused_action_count(&self) -> usize {
        self.entries
            .get(self.focus)
            .map_or(0, |entry| entry.card.footer().len())
    }

    fn content_focus(&self) -> usize {
        self.focus
    }

    /// Move the cursor, selecting the cause it lands on so the detail pane
    /// always describes the card the reader is on.
    fn set_content_focus(&mut self, index: usize) {
        self.focus = index;
        self.select_row(index);
    }

    fn row_action(&self) -> usize {
        self.action
    }

    fn set_row_action(&mut self, index: usize) {
        self.action = index;
    }

    fn activate_focused(&mut self, key: Key) -> Option<SectionOutcome> {
        let cause = self.focus;
        match self.entries.get_mut(cause)?.card.on_key(key)? {
            CardAction::FooterActivated { index } => self.resolve_footer(cause, index),
            CardAction::Pressed => None,
        }
    }

    fn render(&self, surface: &mut Surface, ctx: SectionCtx<'_>) {
        let info = self.list_info(&ctx.frame, ctx.scale, ctx.theme);
        let gap = ctx.scale.scale_length(ctx.theme.metrics().control_gap);
        for slot in 0..info.visible() {
            let Some(entry) = self.entries.get(ctx.start + slot as usize) else {
                break;
            };
            let item = info.item_rect(slot);
            let card_rect = Rect::new(
                item.left(),
                item.top(),
                item.width,
                item.height.saturating_sub(gap),
            );
            entry
                .card
                .render(surface, card_rect, ctx.scale, ctx.theme, ctx.font);
        }
        self.render_detail(surface, ctx);
    }

    /// Route a pointer event to the cause cards.
    ///
    /// A card that reports any interaction becomes the selected cause, so
    /// pressing a card opens its detail; a footer activation additionally
    /// resolves to that footer's outcome.
    fn on_pointer(&mut self, event: &InputEvent, ctx: SectionCtx<'_>) -> Option<SectionOutcome> {
        let info = self.list_info(&ctx.frame, ctx.scale, ctx.theme);
        let chosen = select_pressed_card(&info, ctx.start, |cause, item| {
            self.entries
                .get_mut(cause)?
                .card
                .on_pointer(event, item, ctx.scale, ctx.theme)
        });
        let (cause, action) = chosen?;
        self.focus = cause;
        self.select_row(cause);
        match action {
            CardAction::FooterActivated { index } => self.resolve_footer(cause, index),
            CardAction::Pressed => None,
        }
    }

    fn apply_focus_marks(&mut self, focused: bool) {
        let (index, action) = (self.focus, self.action);
        for (i, entry) in self.entries.iter_mut().enumerate() {
            let here = focused && i == index;
            entry.card.set_in_focus_field(here);
            for (b, button) in entry.card.footer_mut().iter_mut().enumerate() {
                button.set_focused(here && b == action);
                button.set_in_focus_field(here);
            }
        }
    }
}

#[cfg(test)]
#[path = "pressure_tests.rs"]
mod tests;
