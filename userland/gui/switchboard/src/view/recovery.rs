//! The Recovery section: hung objects and the recovery actions they offer
//! (`plans/NEW-SWITCHBOARD.md` S3).
//!
//! Owns the caller's fault view model ([`RecoveryItem`]), the
//! [`RecoveryControl`] vocabulary a row offers, and the section's layout,
//! painting and input.

use alloc::string::String;
use alloc::vec::Vec;

use tairix_abi::ProcId;
use tairix_font::BitmapFont;
use tairix_geometry::{to_i32, Rect, Region, Scale};
use tairix_input::{InputEvent, Key};
use tairix_raster::{Color, Surface};
use tairix_theme::{SignalRole, Theme};

use tairix_controls::{
    damage, ActionRail, AuthorityState, Button, ButtonContent, Card, ControlRole, ControlState,
    EventMark, Fact, FactList, MetricLayout, MetricTile, Panel, PressureKind, RailAction,
    RecoveryState, SelectionState, StatusPill, Tab, Tabs, TabsAction, TabsOrientation, Timeline,
    TimelineEvent,
};

use super::frame::{SectionAnatomy, SectionFrame, ACTION_RAIL_WIDTH, DETAIL_PANE_WIDTH};
use super::system_data::{absence_statement, reading_text, selection_prompt, Reading, Unmeasured};
use super::{
    action_state, resolve_selection, select_pressed_card, ListInfo, SectionCtx, SectionOutcome,
    SectionView, SwitchboardAction, SwitchboardModel, UNMEASURED_READING,
};

/// One hung or recoverable object (`plans/NEW-SWITCHBOARD.md`).
///
/// Carries everything the section shows about one fault: the master card's
/// line, the detail pane's identity and pages, the impact column's four
/// readings, and which recovery commands the caller may take.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RecoveryItem {
    /// The faulting task's stable, never-reused instance identity.
    ///
    /// This, not the row's position, is what a selection is remembered by:
    /// the list is rebuilt from scratch on every sample, so an index would
    /// silently re-point at a different fault as soon as one above it
    /// cleared. A numeric pid would be no better — the kernel reuses it.
    pub proc_id: ProcId,
    /// The faulting task's numeric id, for display beside its name.
    pub pid: u64,
    /// The object's display name.
    pub name: String,
    /// A short trailing detail (e.g. how long it has been unresponsive).
    pub detail: String,
    /// How long ago this service first observed the fault, or why it
    /// cannot say.
    pub since: Reading,
    /// The object's recovery posture.
    pub recovery: RecoveryState,
    /// What the fault costs the machine while it stands.
    pub impact: FaultImpact,
    /// The plain statement of what state the object is in.
    pub status: String,
    /// What a reader should do about it.
    pub recommendation: String,
    /// The fault's marks, oldest first, for the detail pane's timeline.
    pub marks: Vec<FaultMark>,
    /// The kernel's crash record for this fault, matched by process
    /// identity, or [`None`] when this fault produced none.
    pub crash: Option<CrashSnapshot>,
    /// The faulting task's own CPU share.
    pub cpu: Reading,
    /// The faulting task's own resident memory.
    pub memory: Reading,
    /// The faulting task's own storage throughput.
    pub disk: Reading,
    /// The faulting task's own network throughput.
    ///
    /// There is no per-process network accounting anywhere in the System
    /// Information API, so this is always absent; it is carried rather
    /// than omitted because a reader comparing four resources must be told
    /// the fourth is unmeasured, not left to assume it is nought.
    pub network: Reading,
    /// Whether an ordinary restart is available.
    pub can_restart: bool,
    /// Whether the high-impact force action is available.
    pub can_force: bool,
}

/// What a standing fault costs the machine — the tier the detail pane's
/// status pill names.
///
/// Derived from the fault's own posture rather than guessed at: a task the
/// kernel has stopped is holding whatever it held, while one that is merely
/// unresponsive is still costing a seat its interaction.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum FaultImpact {
    /// The fault is confined to the task itself.
    Contained,
    /// The fault is blocking work a reader is waiting on.
    Blocking,
}

impl FaultImpact {
    /// The words the status pill shows.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            FaultImpact::Contained => "Contained",
            FaultImpact::Blocking => "Blocking",
        }
    }

    /// The impact a `recovery` posture carries.
    #[must_use]
    pub const fn of(recovery: RecoveryState) -> Self {
        match recovery {
            RecoveryState::Hung => FaultImpact::Blocking,
            _ => FaultImpact::Contained,
        }
    }
}

/// One mark on a fault's timeline: when it happened and what it was.
///
/// Only marks this service actually observed are carried. A fault it first
/// saw already faulted has one mark, not an invented history.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FaultMark {
    /// When it happened, relative to now.
    pub stamp: String,
    /// What happened.
    pub text: String,
    /// Whether this mark is the fault itself rather than an observation
    /// around it.
    pub is_fault: bool,
}

/// The kernel's post-mortem of one crashed task, as the detail pane shows
/// it.
///
/// Every field here is read from the kernel's own crash record; none is
/// derived, defaulted or filled in. A record the query never returned is
/// [`None`] on the item, never an empty snapshot — "this fault has no
/// crash record" and "this crash recorded nothing" are different facts.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CrashSnapshot {
    /// Why the resolver refused the faulting access.
    pub cause: String,
    /// Where the faulting address sat, and how far from its anchor.
    pub location: String,
    /// Whether the refused access was a write.
    pub write: bool,
    /// The owning user and group at crash time.
    pub owner: String,
    /// The faulting program counter, noting whether it is
    /// program-relative or absolute.
    pub pc: String,
    /// The faulting stack pointer.
    pub sp: String,
    /// The faulting frame pointer, or the note that it is not meaningful.
    pub fp: String,
    /// The named general-purpose registers, in the order the kernel
    /// recorded them.
    pub registers: Vec<(String, u64)>,
    /// The backtrace frames, innermost first.
    pub frames: Vec<u64>,
}

/// A recovery action a Switchboard recovery row can request.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum RecoveryControl {
    /// Restart the hung object (an ordinary recovery).
    Restart,
    /// Force the object (the high-impact, confirmation-gated action).
    Force,
}

/// One page of the detail pane, selected from its tab strip.
///
/// A fixed, ordered set: each page has its own body and its own honest
/// empty form, so a page that is not here has nothing to draw.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub(super) enum FaultPage {
    /// The marks this service observed around the fault.
    Timeline,
    /// The kernel's post-mortem of the crashed task.
    CrashSnapshot,
    /// The fault's log entries.
    Logs,
}

impl FaultPage {
    /// Every page, in the order the tab strip shows them.
    pub(super) const ALL: [FaultPage; 3] = [
        FaultPage::Timeline,
        FaultPage::CrashSnapshot,
        FaultPage::Logs,
    ];

    /// The page's tab label.
    pub(super) const fn title(self) -> &'static str {
        match self {
            FaultPage::Timeline => "Timeline",
            FaultPage::CrashSnapshot => "Crash Snapshot",
            FaultPage::Logs => "Logs",
        }
    }

    /// The page's position in the strip.
    pub(super) const fn index(self) -> usize {
        match self {
            FaultPage::Timeline => 0,
            FaultPage::CrashSnapshot => 1,
            FaultPage::Logs => 2,
        }
    }

    /// The page at `index`, or [`None`] past the last one (fail closed).
    pub(super) const fn from_index(index: usize) -> Option<Self> {
        match index {
            0 => Some(FaultPage::Timeline),
            1 => Some(FaultPage::CrashSnapshot),
            2 => Some(FaultPage::Logs),
            _ => None,
        }
    }
}

/// The rail's title. The rail control carries no caption of its own, so
/// the section seats it in a [`Panel`], which already defines what a titled
/// container looks like.
const RAIL_TITLE: &str = "RECOVERY ACTIONS";

/// The impact column's logical width: one unplated reading tile.
const IMPACT_WIDTH: u32 = 104;

/// The footer's logical height: one line stating how many faults have
/// cleared.
const FOOTER_HEIGHT: u32 = 22;

/// The Recovery section: the fault cards, the selected fault's detail,
/// impact and commands, and the keyboard's place among them.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct RecoverySection {
    /// The faults this sample reported, in model order.
    pub(super) items: Vec<RecoveryItem>,
    /// One master card per fault, in the same order.
    pub(super) cards: Vec<Card>,
    /// Which fault is selected, by its stable identity rather than by its
    /// position, so a refresh that reorders the list keeps the reader on
    /// the fault they chose.
    pub(super) selected: Option<ProcId>,
    /// Which detail page is showing.
    pub(super) page: FaultPage,
    /// The detail pane's page strip.
    pub(super) pages: Tabs,
    /// The selected fault's commands.
    pub(super) rail: ActionRail,
    /// The plate the rail is seated in, which carries its caption.
    pub(super) rail_panel: Panel,
    /// How many faults have cleared, as the model reports it.
    pub(super) resolved: usize,
    /// Where the content cursor is: a fault card, then the page strip,
    /// then a rail command.
    pub(super) focus: usize,
    /// Which of the focused stop's own actions the cursor is on.
    pub(super) action: usize,
}

/// Which kind of thing the content cursor is on.
///
/// The cursor is one flat run of stops over three different kinds of
/// control, and a refresh that changes the number of faults moves where
/// each kind begins. Naming the kinds lets a refresh put the cursor back
/// on the *same kind of thing* rather than on whatever now happens to sit
/// at the old number.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
enum Stop {
    /// The fault card at this position.
    Card(usize),
    /// The detail pane's page strip.
    Pages,
    /// The rail command at this position.
    Rail(usize),
}

/// The rectangles the detail pane's parts occupy, resolved once so the
/// paint and the hit test cannot disagree about where a tab is.
#[derive(Copy, Clone, Debug)]
struct DetailLayout {
    /// The fault's name and task id.
    identity: Rect,
    /// The impact pill.
    pill: Rect,
    /// The status and recommendation facts.
    facts: Rect,
    /// The page strip.
    pages: Rect,
    /// Whatever is left for the selected page's body.
    body: Rect,
}

impl RecoverySection {
    /// An empty Recovery section: nothing faulted, cursor at the top.
    pub(super) fn new() -> Self {
        Self {
            items: Vec::new(),
            cards: Vec::new(),
            selected: None,
            page: FaultPage::Timeline,
            pages: page_tabs(FaultPage::Timeline),
            rail: ActionRail::new(Vec::new()),
            rail_panel: Panel::new(RAIL_TITLE),
            resolved: 0,
            focus: 0,
            action: 0,
        }
    }

    /// The position of the selected fault in the current list, or [`None`]
    /// when nothing is selected (an empty list, or a selection whose fault
    /// has cleared).
    pub(super) fn selected_index(&self) -> Option<usize> {
        let id = self.selected?;
        self.items.iter().position(|item| item.proc_id == id)
    }

    /// The selected fault, or [`None`] when nothing is selected.
    pub(super) fn selected_item(&self) -> Option<&RecoveryItem> {
        self.items.get(self.selected_index()?)
    }

    /// How many commands the rail holds for the selected fault.
    fn rail_len(&self) -> usize {
        self.rail.len()
    }

    /// Which kind of stop the cursor at `index` is on, or [`None`] past
    /// the last stop (fail closed).
    fn stop_at(&self, index: usize) -> Option<Stop> {
        if self.items.is_empty() {
            return None;
        }
        if index < self.items.len() {
            return Some(Stop::Card(index));
        }
        if index == self.items.len() {
            return Some(Stop::Pages);
        }
        let slot = index.saturating_sub(self.items.len()).saturating_sub(1);
        (slot < self.rail_len()).then_some(Stop::Rail(slot))
    }

    /// The cursor index a `stop` sits at in the current list.
    fn index_of(&self, stop: Stop) -> usize {
        match stop {
            Stop::Card(row) => row.min(self.items.len().saturating_sub(1)),
            Stop::Pages => self.items.len(),
            Stop::Rail(slot) => self
                .items
                .len()
                .saturating_add(1)
                .saturating_add(slot.min(self.rail_len().saturating_sub(1))),
        }
    }

    /// Select the fault at `row`, if there is one.
    fn select_row(&mut self, row: usize) {
        if let Some(item) = self.items.get(row) {
            self.selected = Some(item.proc_id);
            self.rebuild_selection();
        }
    }

    /// Show `page` and mark it on the strip.
    fn select_page(&mut self, page: FaultPage) {
        self.page = page;
        self.pages.set_selected(page.index());
        self.pages.set_current(Some(page.index()));
    }

    /// Rebuild everything that depends on *which* fault is selected: the
    /// cards' selection marks and the rail's commands.
    fn rebuild_selection(&mut self) {
        let selected = self.selected;
        for (card, item) in self.cards.iter_mut().zip(self.items.iter()) {
            card.set_state(card_state(item, selected == Some(item.proc_id)));
        }
        self.rail = ActionRail::new(match self.selected_item() {
            Some(item) => alloc::vec![restart_button(item), force_button(item)],
            None => Vec::new(),
        });
    }

    /// Where the detail pane's parts sit inside `content`, or [`None`]
    /// when the pane is too small to seat even its identity line.
    fn detail_layout(
        content: Rect,
        pages: &Tabs,
        scale: Scale,
        theme: &Theme,
        font: BitmapFont,
    ) -> Option<DetailLayout> {
        let gap = scale.scale_length(theme.metrics().control_gap);
        let line = font.line_height();
        let pill_h = StatusPill::measured_height(scale, theme);
        let facts_h = FactList::row_height(scale, theme).saturating_mul(3);
        let pages_h = pages.measured_extent(scale, theme);
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
        let identity = take(line);
        if identity.is_empty() {
            return None;
        }
        let pill = take(pill_h);
        let facts = take(facts_h);
        let pages = take(pages_h);
        let body = Rect::new(content.left(), top, content.width, left);
        Some(DetailLayout {
            identity,
            pill,
            facts,
            pages,
            body,
        })
    }

    /// The plate the detail pane draws in: its caption is the selected
    /// fault's own name, so the pane says what it is describing.
    fn detail_panel(&self) -> Panel {
        Panel::new(self.selected_item().map_or(DETAIL_TITLE, |item| &item.name))
    }

    /// The detail pane's own content rectangle inside its plate, or
    /// [`None`] when the frame dropped the pane under width pressure.
    fn detail_content(&self, frame: &SectionFrame, scale: Scale, theme: &Theme) -> Option<Rect> {
        self.detail_panel()
            .content_rect(frame.detail?, scale, theme)
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

    /// Paint the selected fault's detail pane.
    fn render_detail(&self, surface: &mut Surface, ctx: SectionCtx<'_>) {
        let Some(rect) = ctx.frame.detail else {
            return;
        };
        let panel = self.detail_panel();
        panel.render(surface, rect, ctx.scale, ctx.theme);
        let Some(content) = panel.content_rect(rect, ctx.scale, ctx.theme) else {
            return;
        };
        let palette = ctx.theme.palette();
        let Some(item) = self.selected_item() else {
            ctx.font.draw_text(
                surface,
                content.left(),
                content.top(),
                &selection_prompt("a fault"),
                Color::from(palette.on_surface_muted),
            );
            return;
        };
        let Some(layout) =
            Self::detail_layout(content, &self.pages, ctx.scale, ctx.theme, ctx.font)
        else {
            return;
        };
        ctx.font.draw_text(
            surface,
            layout.identity.left(),
            layout.identity.top(),
            &identity_text(item),
            Color::from(palette.on_surface),
        );
        impact_pill(item).render(surface, layout.pill, ctx.scale, ctx.theme);
        detail_facts(item).render(surface, layout.facts, ctx.scale, ctx.theme);
        self.pages
            .render(surface, layout.pages, ctx.scale, ctx.theme);
        self.render_page(surface, item, layout.body, ctx);
    }

    /// Paint the selected page's body.
    fn render_page(
        &self,
        surface: &mut Surface,
        item: &RecoveryItem,
        body: Rect,
        ctx: SectionCtx<'_>,
    ) {
        if body.is_empty() {
            return;
        }
        let muted = Color::from(ctx.theme.palette().on_surface_muted);
        match self.page {
            FaultPage::Timeline => {
                fault_timeline(item).render(surface, body, ctx.scale, ctx.theme);
            }
            FaultPage::CrashSnapshot => match crash_facts(item) {
                Some(facts) => facts.render(surface, body, ctx.scale, ctx.theme),
                None => {
                    ctx.font
                        .draw_text(surface, body.left(), body.top(), NO_CRASH_RECORD, muted);
                }
            },
            FaultPage::Logs => {
                ctx.font
                    .draw_text(surface, body.left(), body.top(), &logs_absence(), muted);
            }
        }
    }

    /// Paint the selected fault's four impact readings, stacked.
    fn render_impact(&self, surface: &mut Surface, ctx: SectionCtx<'_>) {
        let Some(column) = ctx.frame.impact else {
            return;
        };
        let Some(item) = self.selected_item() else {
            return;
        };
        let tiles = impact_tiles(item);
        let gap = ctx.scale.scale_length(ctx.theme.metrics().control_gap);
        let count = u32::try_from(tiles.len()).unwrap_or(1).max(1);
        let spread = gap.saturating_mul(count.saturating_sub(1));
        let each = column.height.saturating_sub(spread) / count;
        for (index, tile) in tiles.iter().enumerate() {
            let offset = u32::try_from(index)
                .unwrap_or(0)
                .saturating_mul(each.saturating_add(gap));
            let rect = Rect::new(
                column.left(),
                column.top().saturating_add(to_i32(offset)),
                column.width,
                each,
            );
            tile.render(surface, rect, ctx.scale, ctx.theme);
        }
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

    /// Paint the footer's resolved-fault count.
    fn render_footer(&self, surface: &mut Surface, ctx: SectionCtx<'_>) {
        let band = ctx.frame.footer;
        if band.is_empty() {
            return;
        }
        ctx.font.draw_text(
            surface,
            band.left(),
            band.top(),
            &resolved_text(self.resolved),
            Color::from(ctx.theme.palette().on_surface_muted),
        );
    }
}

/// The detail pane's caption when no fault is selected.
const DETAIL_TITLE: &str = "FAULT";

/// What the Crash Snapshot page says for a fault the kernel recorded no
/// crash for.
///
/// A fault is not always a crash — a task the kernel stopped, or one that
/// has merely gone unresponsive, has faulted without ever taking a
/// user fault — so this is a statement of fact, not an absent reading, and
/// deliberately does not wear the unmeasured mark.
const NO_CRASH_RECORD: &str = "No crash record: this fault did not raise a user fault.";

/// What the Logs page says.
///
/// There is no log-query interface anywhere in the System Information API,
/// so this page can never be filled by a grant or a retry. It states that
/// rather than showing an empty list, which a reader would take to mean
/// the fault logged nothing.
fn logs_absence() -> String {
    absence_statement("this fault's log entries", Unmeasured::NoInterface)
}

/// The footer's line: how many faults have cleared while the reader has
/// been watching.
fn resolved_text(resolved: usize) -> String {
    match resolved {
        1 => String::from("1 fault resolved"),
        n => alloc::format!("{n} faults resolved"),
    }
}

/// The detail pane's identity line: the fault's name and the task it is.
fn identity_text(item: &RecoveryItem) -> String {
    alloc::format!("{} · task {}", item.name, item.pid)
}

/// The page strip, with `selected` showing.
fn page_tabs(selected: FaultPage) -> Tabs {
    let mut tabs = Tabs::new(
        FaultPage::ALL
            .iter()
            .map(|page| Tab::new(page.title()))
            .collect(),
    )
    .with_orientation(TabsOrientation::Horizontal);
    tabs.set_selected(selected.index());
    tabs.set_current(Some(selected.index()));
    tabs
}

/// A master card's state: its recovery posture, and whether it is the
/// selected fault.
fn card_state(item: &RecoveryItem, selected: bool) -> ControlState {
    let state = ControlState::idle().with_recovery(item.recovery);
    if selected {
        state.with_selection(SelectionState::Selected)
    } else {
        state
    }
}

/// One fault's master card: what faulted, what happened, and how long ago.
fn build_card(item: &RecoveryItem, selected: bool) -> Card {
    Card::new(item.name.clone())
        .with_body(card_body(item))
        .with_role(ControlRole::Recovery)
        .with_state(card_state(item, selected))
}

/// A card's body line: what happened, then how long ago it did.
fn card_body(item: &RecoveryItem) -> String {
    match &item.since {
        Reading::Measured(elapsed) => alloc::format!("{} · {elapsed}", item.detail),
        Reading::Absent(reason) => {
            alloc::format!(
                "{} · {UNMEASURED_READING} — {}",
                item.detail,
                reason.reason()
            )
        }
    }
}

/// The Restart command for `item`.
fn restart_button(item: &RecoveryItem) -> Button {
    let mut restart = Button::new(
        ButtonContent::Label(String::from("Restart")),
        ControlRole::Recovery,
    );
    restart.set_state(action_state(item.can_restart));
    restart
}

/// The Force command for `item`.
///
/// A permitted force action carries a deliberate confirmation posture; a
/// refused one shows the Authority Mark and fails closed.
fn force_button(item: &RecoveryItem) -> Button {
    let mut force = Button::new(
        ButtonContent::Label(String::from("Force")),
        ControlRole::Destructive,
    );
    force.set_state(if item.can_force {
        ControlState::idle().with_authority(AuthorityState::NeedsConfirmation)
    } else {
        action_state(false)
    });
    force
}

/// The status pill naming what the fault costs while it stands.
fn impact_pill(item: &RecoveryItem) -> StatusPill {
    StatusPill::new(item.impact.label()).with_tone(match item.impact {
        FaultImpact::Blocking => SignalRole::Warning,
        FaultImpact::Contained => SignalRole::Recovery,
    })
}

/// The detail pane's facts: what state the fault is in, how long it has
/// been in it, and what to do about it.
fn detail_facts(item: &RecoveryItem) -> FactList {
    FactList::new(alloc::vec![
        Fact::new("Status", item.status.clone()),
        Fact::new("Faulted", reading_text(&item.since)),
        Fact::new("Recommendation", item.recommendation.clone()),
    ])
}

/// The Timeline page's marks, oldest first.
fn fault_timeline(item: &RecoveryItem) -> Timeline {
    Timeline::new(
        item.marks
            .iter()
            .map(|mark| {
                let event = TimelineEvent::new(mark.stamp.clone(), mark.text.clone());
                if mark.is_fault {
                    event
                        .with_mark(EventMark::Notable)
                        .with_tone(SignalRole::Warning)
                } else {
                    event
                }
            })
            .collect(),
    )
}

/// The Crash Snapshot page's facts, or [`None`] for a fault with no crash
/// record.
fn crash_facts(item: &RecoveryItem) -> Option<FactList> {
    let crash = item.crash.as_ref()?;
    let mut facts = alloc::vec![
        Fact::new("Cause", crash.cause.clone()),
        Fact::new("Address", crash.location.clone()),
        Fact::new(
            "Access",
            String::from(if crash.write { "write" } else { "read" })
        ),
        Fact::new("Owner", crash.owner.clone()),
        Fact::new("pc", crash.pc.clone()),
        Fact::new("sp", crash.sp.clone()),
        Fact::new("fp", crash.fp.clone()),
    ];
    for (name, value) in &crash.registers {
        facts.push(Fact::new(name.clone(), alloc::format!("{value:#018x}")));
    }
    for (depth, frame) in crash.frames.iter().enumerate() {
        facts.push(Fact::new(
            alloc::format!("frame {depth}"),
            alloc::format!("{frame:#018x}"),
        ));
    }
    Some(FactList::new(facts))
}

/// The impact column's four readings for `item`, unplated because the
/// column is the plate.
fn impact_tiles(item: &RecoveryItem) -> Vec<MetricTile> {
    [
        ("CPU", &item.cpu, PressureKind::Cpu),
        ("MEMORY", &item.memory, PressureKind::Memory),
        ("DISK", &item.disk, PressureKind::Disk),
        ("NETWORK", &item.network, PressureKind::Network),
    ]
    .into_iter()
    .map(|(name, reading, kind)| {
        MetricTile::new(name, reading_text(reading), kind)
            .with_layout(MetricLayout::Stacked)
            .unplated()
    })
    .collect()
}

impl SectionView for RecoverySection {
    fn anatomy(&self) -> SectionAnatomy {
        SectionAnatomy {
            band_summary: None,
            sidebar_width: 0,
            header_height: 0,
            detail_width: DETAIL_PANE_WIDTH,
            impact_width: IMPACT_WIDTH,
            rail_width: ACTION_RAIL_WIDTH,
            footer_height: FOOTER_HEIGHT,
            primary_row_commands: 0,
        }
    }

    /// Adopt a fresh sample, keeping the reader where they were.
    ///
    /// The list is rebuilt from scratch every sample, so the selection is
    /// re-resolved against the faulting task's own stable identity: a fault
    /// that is still faulted stays selected however far it has moved in the
    /// list, and only a fault that has genuinely cleared loses the
    /// selection. The cursor is put back on the same *kind* of stop for the
    /// same reason — a row cursor follows the fault it was on rather than
    /// staying on a number that now names a different one.
    fn adopt(&mut self, model: &SwitchboardModel) {
        let previous = self.selected;
        let stop = self.stop_at(self.focus);
        self.resolved = model.recovery_resolved;
        self.items.clone_from(&model.recovery);
        self.selected = resolve_selection(previous, self.items.iter().map(|item| item.proc_id));
        self.cards = self
            .items
            .iter()
            .map(|item| build_card(item, self.selected == Some(item.proc_id)))
            .collect();
        self.rebuild_selection();

        self.focus = match stop {
            Some(Stop::Card(_)) | None => self.selected_index().unwrap_or(0),
            Some(Stop::Pages) => self.index_of(Stop::Pages),
            Some(Stop::Rail(slot)) => self.index_of(Stop::Rail(slot)),
        };
        self.focus = self.focus.min(self.focus_span().saturating_sub(1));
        self.action = 0;
    }

    fn item_count(&self) -> usize {
        self.items.len()
    }

    /// The cursor walks the fault cards, then the detail pane's page
    /// strip, then the selected fault's commands. With nothing faulted
    /// there is nothing to detail and nothing to command, so there are no
    /// stops at all.
    fn focus_span(&self) -> usize {
        if self.items.is_empty() {
            return 0;
        }
        self.items
            .len()
            .saturating_add(1)
            .saturating_add(self.rail_len())
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

    /// Zero: a fault's commands live in the anchored rail beside the list,
    /// not inside its card.
    fn row_buttons(&self) -> u32 {
        0
    }

    fn focused_action_count(&self) -> usize {
        match self.stop_at(self.focus) {
            Some(Stop::Pages) => FaultPage::ALL.len(),
            _ => 1,
        }
    }

    fn content_focus(&self) -> usize {
        self.focus
    }

    /// Move the cursor, selecting the fault a card stop names so the
    /// detail, impact and rail always describe the card the reader is on.
    fn set_content_focus(&mut self, index: usize) {
        self.focus = index;
        if let Some(Stop::Card(row)) = self.stop_at(index) {
            self.select_row(row);
        }
    }

    fn row_action(&self) -> usize {
        self.action
    }

    /// Move the within-stop cursor. On the page strip that *is* the page
    /// selection, so Left/Right walk the pages the way they walk any other
    /// row's actions.
    fn set_row_action(&mut self, index: usize) {
        self.action = index;
        if matches!(self.stop_at(self.focus), Some(Stop::Pages)) {
            if let Some(page) = FaultPage::from_index(index) {
                self.select_page(page);
            }
        }
    }

    /// Commit the focused stop.
    ///
    /// A rail stop hands the key to the rail, which forwards it to the
    /// focused button so the button decides for itself whether it may fire:
    /// a disabled command, or one whose Authority Mark denies the caller,
    /// refuses the keyboard exactly as it refuses the pointer.
    fn activate_focused(&mut self, key: Key) -> Option<SectionOutcome> {
        match self.stop_at(self.focus)? {
            Stop::Card(row) => {
                self.select_row(row);
                None
            }
            Stop::Pages => {
                let TabsAction::Selected { index } = self.pages.on_key(key)?;
                let page = FaultPage::from_index(index)?;
                self.select_page(page);
                self.action = index;
                None
            }
            Stop::Rail(slot) => {
                let index = self.selected_index()?;
                // The keyboard path carries no layout, so the rail has no
                // rectangle to report here and reports nothing.
                self.rail
                    .set_focus(Some(slot), Rect::EMPTY, &mut damage::sink());
                let RailAction::Activate { index: fired } =
                    self.rail.on_key(key, Rect::EMPTY, &mut damage::sink())?;
                Some(SectionOutcome::Action(SwitchboardAction::Recovery {
                    index,
                    control: rail_control(fired)?,
                }))
            }
        }
    }

    fn render(&self, surface: &mut Surface, ctx: SectionCtx<'_>) {
        let info = self.list_info(&ctx.frame, ctx.scale, ctx.theme);
        for slot in 0..info.visible() {
            let Some(card) = self.cards.get(ctx.start + slot as usize) else {
                break;
            };
            card.render(surface, info.item_rect(slot), ctx.scale, ctx.theme);
        }
        self.render_detail(surface, ctx);
        self.render_impact(surface, ctx);
        self.render_rail(surface, ctx);
        self.render_footer(surface, ctx);
    }

    /// Route a pointer event to the fault cards, then the detail page strip,
    /// then the recovery rail.
    ///
    /// A card that reports any interaction — a body press or (once one
    /// carries footer buttons) a footer click — becomes the selected fault,
    /// so pressing a card opens its detail.
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

        if let Some(content) = self.detail_content(&ctx.frame, ctx.scale, ctx.theme) {
            if let Some(layout) =
                Self::detail_layout(content, &self.pages, ctx.scale, ctx.theme, ctx.font)
            {
                if let Some(TabsAction::Selected { index }) =
                    self.pages.on_pointer(event, layout.pages)
                {
                    if let Some(page) = FaultPage::from_index(index) {
                        self.select_page(page);
                    }
                    return None;
                }
            }
        }

        let index = self.selected_index()?;
        let rail = self.rail_content(&ctx.frame, ctx.scale, ctx.theme)?;
        let RailAction::Activate { index: fired } = self
            .rail
            .on_pointer(event, rail, ctx.scale, ctx.theme, damage)?;
        Some(SectionOutcome::Action(SwitchboardAction::Recovery {
            index,
            control: rail_control(fired)?,
        }))
    }

    fn apply_focus_marks(&mut self, focused: bool) {
        let stop = focused.then(|| self.stop_at(self.focus)).flatten();
        for (i, card) in self.cards.iter_mut().enumerate() {
            card.set_in_focus_field(stop == Some(Stop::Card(i)));
        }
        self.pages
            .set_current(Some(if matches!(stop, Some(Stop::Pages)) {
                self.action.min(FaultPage::ALL.len().saturating_sub(1))
            } else {
                self.page.index()
            }));
        let slot = match stop {
            Some(Stop::Rail(slot)) => Some(slot),
            _ => None,
        };
        // Focus marking runs off the keyboard path, which has no layout, so
        // the rail has no rectangle to report here.
        self.rail.set_focus(slot, Rect::EMPTY, &mut damage::sink());
        for (index, button) in self.rail.items_mut().iter_mut().enumerate() {
            button.set_focused(slot == Some(index));
            button.set_in_focus_field(slot.is_some());
        }
    }
}

/// The command a rail slot names, or [`None`] for a slot the rail does not
/// have (fail closed).
fn rail_control(slot: usize) -> Option<RecoveryControl> {
    match slot {
        0 => Some(RecoveryControl::Restart),
        1 => Some(RecoveryControl::Force),
        _ => None,
    }
}

#[cfg(test)]
#[path = "recovery_tests.rs"]
mod tests;
