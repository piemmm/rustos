//! The shared pane frame every Resources pane draws into
//! (`plans/NEW-SWITCHBOARD.md` S4).
//!
//! A pane is instrument-led: a hero carrying the device's headline reading
//! and the instrument that gives it shape, then blocks of the detail behind
//! it. A block holds whatever its reading *is* — a composition, a grid of
//! per-core cells, the tasks costing the device most, a status pill, or
//! genuine facts — so a resource's shape over time is never flattened into
//! key/value text.
//!
//! # One flow, resolved once
//!
//! Every pane compiles to a flat run of short, self-contained [`PaneItem`]s,
//! each holding the control it draws and knowing its own row and column
//! before any paint. Row spans are fixed and width-independent, so the
//! scroll range is exact, a pane taller than its viewport scrolls a row at a
//! time, and the paint never lays out anything: it walks the items the
//! viewport covers and draws them.

use alloc::string::String;
use alloc::vec::Vec;

use tairix_geometry::{to_i32, Rect, Scale};
use tairix_icon::IconKind;
use tairix_raster::{Color, Surface};
use tairix_theme::{SignalRole, Theme};

use tairix_controls::{
    Chart, CompositionBar, CompositionSegment, Fact, FactList, MeterValue, MetricInstrument,
    MetricLayout, MetricTile, PressureKind, ProgressValue, StatusPill,
};

use crate::view::reading::{reading_text, HealthSeverity, Reading, ReadingFact, Unmeasured};

/// The pane's headline reading and the instrument that gives it shape.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PaneHero {
    /// The headline figure.
    pub value: Reading,
    /// The quiet unit beside it ("% busy", "of 16 GB", "MB/s"). Empty when
    /// the figure already carries its own unit.
    pub unit: String,
    /// Context lines under the reading, in reading order.
    pub context: Vec<String>,
    /// The instrument, chosen by the reading rather than by the renderer.
    pub instrument: HeroInstrument,
    /// What the instrument's horizontal extent means ("busy share, all
    /// cores"), so a rate a reader can act on states its own window.
    pub caption: String,
}

impl PaneHero {
    /// A hero with no instrument: a fact pane states facts.
    #[must_use]
    pub fn facts(value: Reading, unit: &str) -> Self {
        Self {
            value,
            unit: String::from(unit),
            context: Vec::new(),
            instrument: HeroInstrument::None,
            caption: String::new(),
        }
    }

    /// This hero with `context` under its reading.
    #[must_use]
    pub fn with_context(mut self, context: Vec<String>) -> Self {
        self.context = context;
        self
    }

    /// This hero with `instrument` captioned `caption`.
    #[must_use]
    pub fn with_instrument(mut self, instrument: HeroInstrument, caption: &str) -> Self {
        self.instrument = instrument;
        self.caption = String::from(caption);
        self
    }
}

/// Which instrument a pane's hero draws.
///
/// A rate trends, because its shape over time *is* the reading and it has no
/// fixed ceiling to fill a bar against; a fraction of a measured whole
/// tracks. A fact pane has neither, and the absence of an instrument is what
/// says the reading is a fact.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum HeroInstrument {
    /// A rate's recent history, in permille, oldest first, with the
    /// opposing direction where one is measured.
    Trend {
        /// The primary series.
        samples: Vec<u16>,
        /// The opposing direction, mirrored below the axis. [`None`] leaves
        /// the trace a single series over the whole box rather than showing
        /// an empty half.
        opposing: Option<Vec<u16>>,
    },
    /// A proportional bar at this permille fraction, or an unmeasured track
    /// where the fraction is not known — never a bar at nought, which would
    /// read as "idle" when the truth is "unknown".
    Track(Option<u16>),
    /// No instrument.
    None,
}

/// How wide a block sits in the pane's flow.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum BlockSpan {
    /// The whole pane width.
    Full,
    /// One of two side-by-side columns, paired with the next `Half` block.
    Half,
}

/// One block of a pane's detail: what it is called, how wide it sits, what
/// it holds, and what the figures in it are *not*.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PaneBlock {
    /// The block's quiet title.
    pub title: String,
    /// How wide it sits.
    pub span: BlockSpan,
    /// What it holds.
    pub body: BlockBody,
    /// A line under the block stating what the figures mean and what they
    /// do not. Empty when the readings speak for themselves.
    pub note: String,
}

impl PaneBlock {
    /// A half-width block titled `title` holding `body`.
    #[must_use]
    pub fn half(title: &str, body: BlockBody) -> Self {
        Self {
            title: String::from(title),
            span: BlockSpan::Half,
            body,
            note: String::new(),
        }
    }

    /// A full-width block titled `title` holding `body`.
    #[must_use]
    pub fn full(title: &str, body: BlockBody) -> Self {
        Self {
            span: BlockSpan::Full,
            ..Self::half(title, body)
        }
    }

    /// This block with `note` under it.
    #[must_use]
    pub fn with_note(mut self, note: &str) -> Self {
        self.note = String::from(note);
        self
    }
}

/// What one block holds.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum BlockBody {
    /// Labelled readings — genuine facts, not a resource flattened into
    /// text.
    Facts(Vec<ReadingFact>),
    /// A measured whole split into its named parts.
    Composition(Vec<CompositionPart>),
    /// One cell per logical CPU, each carrying its own core's trace.
    Cores(Vec<CoreCell>),
    /// The tasks costing this device most.
    Consumers(Vec<ConsumerRow>),
    /// A status pill and the readings it resolves from.
    Health {
        /// The pill's own label.
        pill: String,
        /// How badly the device is faring, which tones the pill.
        severity: HealthSeverity,
        /// The buckets the pill resolves from.
        facts: Vec<ReadingFact>,
    },
    /// A statement that something is absent, in words.
    ///
    /// An empty list is *not* such a statement — it reads as "none" — so a
    /// reading with no interface says so here instead.
    Absence(String),
}

/// One named part of a composition's measured whole.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CompositionPart {
    /// What the part is called.
    pub label: String,
    /// Its own measured quantity.
    pub amount: String,
    /// Its share of the whole, in permille.
    pub share: u16,
    /// Whether this is the share that is *not* in use, which draws as the
    /// track's quiet tail and is always last.
    pub remainder: bool,
}

/// One cell of the per-core grid.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CoreCell {
    /// The core's name.
    pub label: String,
    /// Its performance class, as the badge states it.
    pub badge: String,
    /// Its busy share.
    pub busy: Reading,
    /// Its live measured clock.
    pub clock: Reading,
    /// Its own bounded trace, oldest first, in permille.
    pub trend: Vec<u16>,
}

/// One task in a device's top-consumers block.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConsumerRow {
    /// The task's display name.
    pub name: String,
    /// The glyph naming what the row is.
    pub icon: IconKind,
    /// What it costs this device.
    pub amount: String,
    /// That cost as a share of the largest consumer, so the track compares
    /// the tasks with one another rather than against a device total a sum
    /// of tasks is not.
    pub share: u16,
}

/// How many logical rows the hero claims: its reading, its context lines,
/// and the instrument beside them.
const HERO_ROWS: u32 = 4;
/// How many logical rows a pressure banner claims.
const BANNER_ROWS: u32 = 2;
/// How many logical rows one per-core cell claims: its name and badge, its
/// trace, and its busy share beside its clock.
const CELL_ROWS: u32 = 3;
/// Most per-core cells that sit side by side in one grid row, however wide
/// the pane is: past six the cells are too narrow to read a clock in.
const CELLS_PER_ROW_MAX: u32 = 6;
/// The logical width one per-core cell needs at the reference density for
/// its name, badge, busy share and clock.
const CELL_WIDTH: u32 = 132;

/// One drawable of a pane's flow: the control it paints, where it sits, and
/// how many rows it claims.
///
/// Built once when the pane is adopted, so a paint allocates nothing and
/// lays nothing out. Spans are fixed and width-independent, which is what
/// makes the scroll range exact.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(in crate::view) struct PaneItem {
    /// The first row this item occupies.
    pub(in crate::view) row: u32,
    /// How many rows it claims.
    pub(in crate::view) rows: u32,
    /// Which column it sits in.
    pub(in crate::view) column: PaneColumn,
    /// What it draws.
    pub(in crate::view) body: ItemBody,
}

/// Which column of the pane's flow an item sits in.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub(in crate::view) enum PaneColumn {
    /// The whole pane width.
    Full,
    /// The leading half.
    Leading,
    /// The trailing half.
    Trailing,
}

/// What one pane item draws.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(in crate::view) enum ItemBody {
    /// The pane's hero: its reading and, beside it, its instrument.
    Hero {
        /// The reading, its unit and its first context line.
        tile: MetricTile,
        /// The rate's trace, where the reading is a rate.
        chart: Option<Chart>,
        /// The context lines after the first, which the tile has no room
        /// for.
        context: Vec<String>,
        /// What the trace's extent means.
        caption: String,
    },
    /// A block's quiet title.
    Title(String),
    /// One labelled reading.
    Fact(FactList),
    /// A measured whole split into its named parts.
    Composition(CompositionBar),
    /// One row of the per-core grid.
    Cells(Vec<CellView>),
    /// One top-consumer row: the task, what it costs, and the track
    /// comparing it with the largest consumer.
    Consumer(MetricTile),
    /// A status pill.
    Pill(StatusPill),
    /// A line of quiet prose: a block's note, or a statement of absence.
    Note(String),
}

/// One per-core cell, built.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(in crate::view) struct CellView {
    /// The core's name, its badge and its readings.
    pub(in crate::view) tile: MetricTile,
    /// The core's own trace.
    pub(in crate::view) trend: Chart,
    /// The performance-class badge.
    pub(in crate::view) badge: StatusPill,
}

/// Compile a pane's hero, banner and blocks into the flow the frame draws.
///
/// The one place a pane becomes drawable, so no pane carries a second
/// definition of its own layout and every pane scrolls, wraps and degrades
/// identically.
pub(super) fn compile(
    hero: &PaneHero,
    banner: bool,
    blocks: &[PaneBlock],
    kind: PressureKind,
    cells_per_row: u32,
) -> Vec<PaneItem> {
    let mut items = Vec::new();
    let mut row = 0;
    if banner {
        // The banner is composition, not a control: the frame draws its
        // pill, its lines and its relief button from the model, so the flow
        // reserves its rows and nothing else.
        row += BANNER_ROWS;
    }
    items.push(PaneItem {
        row,
        rows: HERO_ROWS,
        column: PaneColumn::Full,
        body: hero_body(hero, kind),
    });
    row += HERO_ROWS;

    let mut pending: Option<(u32, u32)> = None;
    for block in blocks {
        let column = match (block.span, pending) {
            (BlockSpan::Full, _) => {
                if let Some((start, height)) = pending.take() {
                    row = start.saturating_add(height);
                }
                PaneColumn::Full
            }
            (BlockSpan::Half, None) => PaneColumn::Leading,
            (BlockSpan::Half, Some((start, _))) => {
                row = start;
                PaneColumn::Trailing
            }
        };
        let start = row;
        row = push_block(&mut items, block, kind, row, column, cells_per_row);
        pending = match column {
            PaneColumn::Leading => Some((start, row.saturating_sub(start))),
            PaneColumn::Trailing => {
                let (paired_start, paired_height) = pending.unwrap_or((start, 0));
                let height = row.saturating_sub(start).max(paired_height);
                row = paired_start.saturating_add(height);
                None
            }
            PaneColumn::Full => None,
        };
    }
    if let Some((start, height)) = pending {
        row = start.saturating_add(height);
    }
    let _ = row;
    items
}

/// The hero's own drawable: its reading beside its instrument.
fn hero_body(hero: &PaneHero, kind: PressureKind) -> ItemBody {
    let mut context = hero.context.iter();
    let mut tile = MetricTile::new(String::new(), reading_text(&hero.value), kind)
        .with_layout(MetricLayout::Stacked)
        .unplated();
    if !hero.unit.is_empty() {
        tile = tile.with_unit(hero.unit.clone());
    }
    if let Some(first) = context.next() {
        tile = tile.with_detail(first.clone());
    }
    let chart = match &hero.instrument {
        HeroInstrument::Trend { samples, opposing } => {
            let mut chart = Chart::new(kind).with_samples(samples.iter().copied());
            if let Some(opposing) = opposing {
                chart = chart.with_opposing(kind, opposing.iter().copied());
            }
            Some(chart)
        }
        HeroInstrument::Track(fraction) => {
            tile = tile.with_instrument(MetricInstrument::Track(match fraction {
                Some(permille) => MeterValue::Measured(ProgressValue::new(*permille)),
                None => MeterValue::Unmeasured,
            }));
            None
        }
        HeroInstrument::None => None,
    };
    ItemBody::Hero {
        tile,
        chart,
        context: context.cloned().collect(),
        caption: hero.caption.clone(),
    }
}

/// Append one block's items, returning the row after it.
fn push_block(
    items: &mut Vec<PaneItem>,
    block: &PaneBlock,
    kind: PressureKind,
    start: u32,
    column: PaneColumn,
    cells_per_row: u32,
) -> u32 {
    let mut row = start;
    let mut push = |rows: u32, body: ItemBody| {
        items.push(PaneItem {
            row,
            rows,
            column,
            body,
        });
        row = row.saturating_add(rows);
    };
    push(1, ItemBody::Title(block.title.clone()));
    match &block.body {
        BlockBody::Facts(facts) => {
            for fact in facts {
                push(1, ItemBody::Fact(fact_list(fact)));
            }
        }
        BlockBody::Composition(parts) => match composition(kind, parts) {
            // Shares that do not account for the whole fail construction
            // rather than drawing a silently short bar, so the block states
            // the absence instead of under-reporting where the resource went.
            Some(bar) => push(
                1 + u32::try_from(parts.len()).unwrap_or(0),
                ItemBody::Composition(bar),
            ),
            None => push(
                1,
                ItemBody::Note(crate::view::reading::absence_statement(
                    "this composition",
                    Unmeasured::Unavailable,
                )),
            ),
        },
        BlockBody::Cores(cells) => {
            for chunk in cells.chunks(usize::try_from(cells_per_row.max(1)).unwrap_or(1)) {
                push(
                    CELL_ROWS,
                    ItemBody::Cells(chunk.iter().map(|cell| cell_view(cell, kind)).collect()),
                );
            }
        }
        BlockBody::Consumers(rows) => {
            for consumer in rows {
                push(1, ItemBody::Consumer(consumer_row(consumer, kind)));
            }
        }
        BlockBody::Health {
            pill,
            severity,
            facts,
        } => {
            push(
                1,
                ItemBody::Pill(StatusPill::new(pill.clone()).with_tone(health_tone(*severity))),
            );
            for fact in facts {
                push(1, ItemBody::Fact(fact_list(fact)));
            }
        }
        BlockBody::Absence(statement) => push(1, ItemBody::Note(statement.clone())),
    }
    if !block.note.is_empty() {
        push(1, ItemBody::Note(block.note.clone()));
    }
    row
}

/// One labelled reading as the one-fact list that draws it, toned so an
/// absent value is visibly not a measurement.
fn fact_list(fact: &ReadingFact) -> FactList {
    let built = Fact::new(fact.label.clone(), reading_text(&fact.value));
    FactList::new(alloc::vec![match fact.value.absence() {
        Some(Unmeasured::NotPermitted) => built.with_tone(SignalRole::Denied),
        Some(Unmeasured::Unavailable | Unmeasured::NoInterface) => {
            built.with_tone(SignalRole::Warning)
        }
        None => built,
    }])
}

/// A composition's parts as the bar that draws them, or [`None`] when the
/// shares do not describe a whole the bar could honestly draw.
fn composition(kind: PressureKind, parts: &[CompositionPart]) -> Option<CompositionBar> {
    let segments: Vec<CompositionSegment> = parts
        .iter()
        .map(|part| {
            if part.remainder {
                CompositionSegment::remainder(part.label.clone(), part.amount.clone(), part.share)
            } else {
                CompositionSegment::new(part.label.clone(), part.amount.clone(), part.share)
            }
        })
        .collect();
    CompositionBar::new(kind, segments).ok()
}

/// One per-core cell, built.
fn cell_view(cell: &CoreCell, kind: PressureKind) -> CellView {
    CellView {
        tile: MetricTile::new(cell.label.clone(), reading_text(&cell.busy), kind)
            .with_detail(reading_text(&cell.clock))
            .with_layout(MetricLayout::Stacked)
            .unplated(),
        trend: Chart::new(kind).with_samples(cell.trend.iter().copied()),
        badge: StatusPill::new(cell.badge.clone()),
    }
}

/// One top-consumer row: the task, what it costs, and a track comparing it
/// with the largest consumer.
///
/// An unplated tile with a track instrument rather than a table row: no cell
/// kind draws a proportional bar, and the tile already defines exactly this
/// anatomy — an identity glyph, a label, a reading, and a measured track
/// tinted by the resource it is about.
fn consumer_row(consumer: &ConsumerRow, kind: PressureKind) -> MetricTile {
    MetricTile::new(consumer.name.clone(), consumer.amount.clone(), kind)
        .with_icon(consumer.icon)
        .with_layout(MetricLayout::Inline)
        .with_instrument(MetricInstrument::Track(MeterValue::Measured(
            ProgressValue::new(consumer.share),
        )))
        .unplated()
}

/// The tone a volume's health pill is drawn in: a failing volume is a
/// recovery matter, a degraded one a caution, a healthy one takes the
/// ordinary success role.
const fn health_tone(severity: HealthSeverity) -> SignalRole {
    match severity {
        HealthSeverity::Failing => SignalRole::Recovery,
        HealthSeverity::Degraded => SignalRole::Warning,
        HealthSeverity::Healthy => SignalRole::Success,
    }
}

/// How many rows the whole flow claims — the scroll range's content extent.
#[must_use]
pub(super) fn extent(items: &[PaneItem]) -> usize {
    let rows = items
        .iter()
        .map(|item| item.row.saturating_add(item.rows))
        .max()
        .unwrap_or(0);
    usize::try_from(rows).unwrap_or(usize::MAX)
}

/// How many per-core cells one grid row packs into a pane `width` wide.
///
/// The grid re-wraps rather than squeezing: a pane too narrow for six cells
/// draws fewer per row and scrolls, which is what keeps every core's trace
/// readable at every window width. The count is a layout input to
/// [`compile`], so a width change recompiles the flow and the scroll range
/// stays exact.
#[must_use]
pub(super) fn cells_per_row(width: u32, scale: Scale) -> u32 {
    let cell = scale.scale_length(CELL_WIDTH).max(1);
    (width / cell).clamp(1, CELLS_PER_ROW_MAX)
}

/// Where one item draws within `primary`, given the first visible row.
///
/// The rectangle is clamped into `primary`: an item the reader has scrolled
/// halfway through draws into what is left of the viewport rather than over
/// the header above it, and its own control omits whatever no longer fits.
/// An item wholly outside the viewport answers [`None`] and is not drawn at
/// all.
#[must_use]
pub(super) fn item_rect(
    item: &PaneItem,
    primary: Rect,
    start: u32,
    pitch: u32,
    gap: u32,
) -> Option<Rect> {
    let top =
        i64::from(primary.top()) + (i64::from(item.row) - i64::from(start)) * i64::from(pitch);
    let bottom = top + i64::from(item.rows.saturating_mul(pitch));
    let clipped_top = top.max(i64::from(primary.top()));
    let clipped_bottom = bottom.min(i64::from(primary.bottom()));
    if clipped_bottom <= clipped_top {
        return None;
    }
    let (left, width) = column_bounds(item.column, primary, gap);
    if width == 0 {
        return None;
    }
    let height = u32::try_from(clipped_bottom - clipped_top).unwrap_or(0);
    Some(Rect::new(
        left,
        i32::try_from(clipped_top).unwrap_or(primary.top()),
        width,
        height,
    ))
}

/// The horizontal extent of one pane column within `primary`.
fn column_bounds(column: PaneColumn, primary: Rect, gap: u32) -> (i32, u32) {
    match column {
        PaneColumn::Full => (primary.left(), primary.width),
        PaneColumn::Leading | PaneColumn::Trailing => {
            let half = primary.width.saturating_sub(gap) / 2;
            match column {
                PaneColumn::Trailing => (
                    primary.left() + to_i32(half.saturating_add(gap)),
                    primary.width.saturating_sub(half).saturating_sub(gap),
                ),
                _ => (primary.left(), half),
            }
        }
    }
}

/// Paint the items the viewport covers into `primary`.
///
/// Nothing is laid out here and nothing is allocated: every item already
/// knows its row, its span and its column, so the walk is the visible window
/// and the draw.
pub(super) fn render(
    surface: &mut Surface,
    items: &[PaneItem],
    primary: Rect,
    start: u32,
    scale: Scale,
    theme: &Theme,
    font: tairix_font::BitmapFont,
) {
    let pitch = crate::view::Switchboard::row_item_height(scale, theme);
    let gap = scale.scale_length(theme.metrics().control_gap).max(1);
    for item in items {
        let Some(rect) = item_rect(item, primary, start, pitch, gap) else {
            continue;
        };
        render_item(surface, &item.body, rect, scale, theme, font);
    }
}

/// Paint one item into the rectangle the flow resolved for it.
fn render_item(
    surface: &mut Surface,
    body: &ItemBody,
    rect: Rect,
    scale: Scale,
    theme: &Theme,
    font: tairix_font::BitmapFont,
) {
    let palette = theme.palette();
    let gap = scale.scale_length(theme.metrics().control_gap).max(1);
    match body {
        ItemBody::Hero {
            tile,
            chart,
            context,
            caption,
        } => {
            let reading_w = match chart {
                // The trace takes the greater share: a rate's shape is the
                // reading, and the figure beside it needs only its own width.
                Some(_) => rect.width / 3,
                None => rect.width,
            };
            let reading = Rect::new(rect.left(), rect.top(), reading_w, rect.height);
            tile.render(surface, reading, scale, theme, None);
            let mut y = rect
                .top()
                .saturating_add(to_i32(tile.measured_height(scale, theme)));
            for line in context {
                if y.saturating_add(to_i32(font.line_height())) > rect.bottom() {
                    break;
                }
                font.draw_text(
                    surface,
                    reading.left(),
                    y,
                    line,
                    Color::from(palette.on_surface_muted),
                );
                y = y.saturating_add(to_i32(font.line_height()));
            }
            if let Some(chart) = chart {
                let left = rect.left() + to_i32(reading_w.saturating_add(gap));
                let width = rect.width.saturating_sub(reading_w).saturating_sub(gap);
                let caption_h = font.line_height().min(rect.height);
                let plot_h = rect.height.saturating_sub(caption_h);
                chart.render(
                    surface,
                    Rect::new(left, rect.top(), width, plot_h),
                    scale,
                    theme,
                );
                if !caption.is_empty() {
                    font.draw_text(
                        surface,
                        left,
                        rect.top() + to_i32(plot_h),
                        caption,
                        Color::from(palette.on_surface_muted),
                    );
                }
            }
        }
        ItemBody::Title(text) => {
            font.draw_text(
                surface,
                rect.left(),
                rect.top(),
                text,
                Color::from(palette.accent),
            );
        }
        ItemBody::Fact(list) => list.render(surface, rect, scale, theme),
        ItemBody::Composition(bar) => bar.render(surface, rect, scale, theme),
        ItemBody::Cells(cells) => render_cells(surface, cells, rect, scale, theme),
        ItemBody::Consumer(tile) => tile.render(surface, rect, scale, theme, None),
        ItemBody::Pill(pill) => {
            let width = pill.measured_width(scale, theme).min(rect.width);
            let height = StatusPill::measured_height(scale, theme).min(rect.height);
            pill.render(
                surface,
                Rect::new(rect.left(), rect.top(), width, height),
                scale,
                theme,
            );
        }
        ItemBody::Note(text) => {
            font.draw_text(
                surface,
                rect.left(),
                rect.top(),
                text,
                Color::from(palette.on_surface_muted),
            );
        }
    }
}

/// Paint one grid row's cells side by side, each with its own trace under
/// its name and its class badge in the corner.
fn render_cells(
    surface: &mut Surface,
    cells: &[CellView],
    rect: Rect,
    scale: Scale,
    theme: &Theme,
) {
    let count = u32::try_from(cells.len()).unwrap_or(1).max(1);
    let gap = scale.scale_length(theme.metrics().control_gap).max(1);
    let width = rect
        .width
        .saturating_sub(gap.saturating_mul(count.saturating_sub(1)))
        / count;
    if width == 0 {
        return;
    }
    for (index, cell) in cells.iter().enumerate() {
        let step = width
            .saturating_add(gap)
            .saturating_mul(u32::try_from(index).unwrap_or(0));
        let left = rect.left() + to_i32(step);
        let bounds = Rect::new(left, rect.top(), width, rect.height);
        // The trace sits between the cell's name and its readings, which is
        // the whole point of a per-core cell: the shape, not just the figure.
        let trend_h = bounds.height / 3;
        let head_h = bounds.height.saturating_sub(trend_h);
        cell.trend.render(
            surface,
            Rect::new(left, bounds.top() + to_i32(head_h / 2), width, trend_h),
            scale,
            theme,
        );
        cell.tile.render(surface, bounds, scale, theme, None);
        let badge_w = cell.badge.measured_width(scale, theme);
        let badge_h = StatusPill::measured_height(scale, theme);
        if badge_w < width && badge_h <= bounds.height {
            cell.badge.render(
                surface,
                Rect::new(
                    left + to_i32(width.saturating_sub(badge_w)),
                    bounds.top(),
                    badge_w,
                    badge_h,
                ),
                scale,
                theme,
            );
        }
    }
}
