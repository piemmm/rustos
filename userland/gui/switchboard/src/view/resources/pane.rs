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

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use tairix_abi::sysinfo::CpuCoreClass;
use tairix_geometry::{to_i32, Rect, Scale};
use tairix_icon::IconArtwork;
use tairix_raster::{Color, Surface};
use tairix_theme::{SignalRole, TextRole, Theme};

use tairix_controls::{
    inset, plate_border, Chart, CompositionBar, CompositionSegment, Fact, FactList, MeterValue,
    MetricInstrument, MetricLayout, MetricTile, PressureKind, ProgressValue, StatusPill,
    MAX_CHART_SAMPLES,
};
use tairix_font::BitmapFont;

use super::device::Trace;
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
            instrument: HeroInstrument::default(),
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

/// Which instruments a pane's hero draws.
///
/// The two answer different questions and a hero may want both, which is what
/// the boards draw for the processor and for memory alike: the trace beside the
/// reading says *what this has been doing*, and the bar under the context lines
/// says *how much of it is in use now*. A fact pane has neither, and the
/// absence of both is what says the reading is a fact.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct HeroInstrument {
    /// A rate's recent history and how it is tinted.
    /// [`Trace::Absent`] draws no trace.
    pub trace: Trace,
    /// A proportional bar at this permille fraction, or `Some(None)` for an
    /// unmeasured track — never a bar at nought, which would read as "idle"
    /// when the truth is "unknown". `None` draws no bar.
    pub track: Option<Option<u16>>,
}

impl HeroInstrument {
    /// A trace and no bar.
    #[must_use]
    pub fn trend(trace: Trace) -> Self {
        Self {
            trace,
            ..Self::default()
        }
    }

    /// A bar at `fraction` and no trace.
    #[must_use]
    pub fn track(fraction: Option<u16>) -> Self {
        Self {
            track: Some(fraction),
            ..Self::default()
        }
    }

    /// This instrument set with a bar at `fraction` beside whatever trace it
    /// already carries.
    #[must_use]
    pub fn with_track(mut self, fraction: Option<u16>) -> Self {
        self.track = Some(fraction);
        self
    }
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
}

impl PaneBlock {
    /// A half-width block titled `title` holding `body`.
    #[must_use]
    pub fn half(title: &str, body: BlockBody) -> Self {
        Self {
            title: String::from(title),
            span: BlockSpan::Half,
            body,
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

impl BlockBody {
    /// Whether this body draws its own plates, so the block around it draws
    /// none.
    ///
    /// A grid of per-core cells already rims every cell — that rim is what
    /// separates one core's figures from its neighbour's — so a plate around
    /// the grid would nest one rim inside another and the boards show none.
    #[must_use]
    pub const fn self_plating(&self) -> bool {
        matches!(self, BlockBody::Cores(_))
    }
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
    /// Its performance class, which names both the badge's letter and the
    /// tone it wears — a throughput core reads as the compute colour and an
    /// efficiency core as the healthy one, so a heterogeneous machine's two
    /// kinds separate at a glance.
    pub class: CpuCoreClass,
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
    /// The application-bundle directory the desktop launched the task from,
    /// when it launched it — what the row draws its icon from. [`None`] for a
    /// process nothing attests a bundle for.
    pub bundle: Option<String>,
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
    /// Whether it draws inside its block's plate, so its rectangle is inset
    /// from the column by the plate's own padding.
    pub(in crate::view) plated: bool,
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
    /// The plate a block's own rows are drawn on. Spans the block's whole
    /// band and is drawn before them, so it is the ground they stand on.
    Plate,
    /// A block's quiet title, with the plate's hairline rule under it unless
    /// the block's body brings its own plates.
    Title {
        /// The block's name.
        text: String,
        /// Whether the hairline rule is drawn under it.
        ruled: bool,
    },
    /// One labelled reading.
    Fact(FactList),
    /// A measured whole split into its named parts.
    Composition(CompositionBar),
    /// One row of the per-core grid.
    Cells {
        /// The row's cells, in grid order.
        cells: Vec<CellView>,
        /// The grid's column count, which every row divides by — so a
        /// final short row's cells are the size of every other row's
        /// rather than stretching to fill it.
        columns: u32,
    },
    /// One top-consumer row: the task, what it costs, and the track
    /// comparing it with the largest consumer.
    Consumer {
        /// The reading itself.
        tile: MetricTile,
        /// The bundle the task was launched from, where the session attested
        /// one, so the row's icon is that application's own picture.
        bundle: Option<String>,
        /// The task's kernel-attested name, which resolves the icon of every
        /// process the desktop did not launch itself.
        name: String,
    },
    /// A status pill.
    Pill(StatusPill),
    /// A statement of absence, in words.
    Note(String),
}

/// One per-core cell, built.
///
/// Three rows the boards fix: the core's name with its class badge opposite,
/// the trace between, then the busy share with its live clock opposite. Not a
/// [`MetricTile`] — a tile stacks its label, reading and detail from the top,
/// which left the trace drawn across the readings and a third of the cell
/// empty beneath them.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(in crate::view) struct CellView {
    /// The core's name.
    pub(in crate::view) label: String,
    /// Its busy share, the reading the cell leads its bottom row with.
    pub(in crate::view) busy: String,
    /// Its live clock, trailing that same row.
    pub(in crate::view) clock: String,
    /// The core's own trace.
    pub(in crate::view) trend: Chart,
    /// The performance-class badge: its letter and the tone it wears.
    pub(in crate::view) badge: (&'static str, SignalRole),
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
        plated: false,
        body: ItemBody::Plate,
    });
    items.push(PaneItem {
        row,
        rows: HERO_ROWS,
        column: PaneColumn::Full,
        plated: true,
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
    // The one figure the pane is built around, which is what the display role
    // names; its unit stays at body size beside it.
    let mut tile = MetricTile::new(String::new(), reading_text(&hero.value), kind)
        .with_layout(MetricLayout::Stacked)
        .with_value_role(TextRole::Display)
        .unplated();
    if !hero.unit.is_empty() {
        tile = tile.with_unit(hero.unit.clone());
    }
    if let Some(first) = context.next() {
        tile = tile.with_detail(first.clone());
    }
    // The two instruments are independent: the trace answers what the resource
    // has been doing and the bar how much of it is in use, and the boards draw
    // a hero carrying both.
    let instrument = &hero.instrument;
    if let Some(fraction) = instrument.track {
        tile = tile.with_instrument(MetricInstrument::Track(match fraction {
            Some(permille) => MeterValue::Measured(ProgressValue::new(permille)),
            None => MeterValue::Unmeasured,
        }));
    }
    let chart = instrument.trace.chart();
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
    // A body that plates its own items needs no plate around them, so the
    // block's rows then sit on the section ground at full width.
    let plated = !block.body.self_plating();
    let plate = plated.then(|| {
        let slot = items.len();
        items.push(PaneItem {
            row,
            rows: 0,
            column,
            plated: false,
            body: ItemBody::Plate,
        });
        slot
    });
    // Scoped so the plate's own span can be written once the block's rows are
    // known: nothing else can say how tall a block turned out to be.
    {
        let mut push = |rows: u32, body: ItemBody| {
            items.push(PaneItem {
                row,
                rows,
                column,
                plated,
                body,
            });
            row = row.saturating_add(rows);
        };
        push(
            1,
            ItemBody::Title {
                text: block.title.clone(),
                ruled: plated,
            },
        );
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
                let columns = grid_columns(cells.len(), cells_per_row);
                for chunk in cells.chunks(usize::try_from(columns).unwrap_or(1)) {
                    push(
                        CELL_ROWS,
                        ItemBody::Cells {
                            cells: chunk.iter().map(|cell| cell_view(cell, kind)).collect(),
                            columns,
                        },
                    );
                }
            }
            BlockBody::Consumers(rows) => {
                for consumer in rows {
                    push(
                        1,
                        ItemBody::Consumer {
                            tile: consumer_row(consumer, kind),
                            bundle: consumer.bundle.clone(),
                            name: consumer.name.clone(),
                        },
                    );
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
    }
    // A plated block's rows are inset from the top of its band, so its last
    // row would otherwise end level with the band — running its content over
    // the plate's own rim and margin, which is what put the hero's share bar
    // outside its plate. The block claims one row past its content instead, so
    // the plate closes below the last reading rather than through it.
    if plated {
        row = row.saturating_add(1);
    }
    if let Some(slot) = plate.and_then(|slot| items.get_mut(slot)) {
        slot.rows = row.saturating_sub(start);
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
        label: cell.label.clone(),
        busy: reading_text(&cell.busy),
        clock: reading_text(&cell.clock),
        trend: Chart::new(kind.signal_role()).with_samples(cell.trend.iter().copied()),
        badge: class_badge(cell.class),
    }
}

/// The letter one core's class badge shows and the signal tone it wears.
const fn class_badge(class: CpuCoreClass) -> (&'static str, SignalRole) {
    match class {
        CpuCoreClass::Performance => ("P", SignalRole::Cpu),
        CpuCoreClass::Efficiency => ("E", SignalRole::Success),
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
        .with_icon(crate::view::task_icon_kind(consumer.bundle.as_deref()))
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

/// The per-core grid's column count for `count` cells where a row this wide
/// holds at `most` of them.
///
/// The grid spreads its cells evenly over the rows it needs rather than
/// filling each row and leaving a straggler: four cores in a pane three
/// cells wide draw as two rows of two. Every row then divides this one
/// count, so a final short row's cells are the size of every other row's.
fn grid_columns(count: usize, most: u32) -> u32 {
    let most = usize::try_from(most.max(1)).unwrap_or(1);
    let count = count.max(1);
    let columns = count.div_ceil(count.div_ceil(most));
    u32::try_from(columns).unwrap_or(1)
}

/// One per-core cell's slot width in a grid `columns` wide across `width`.
///
/// The slots abut and each cell's own plate margin makes the gap, so the
/// spacing across a grid row is the same gap as the spacing down a pane. A
/// function of the grid rather than of a row, so every cell of the grid is the
/// same size — the last row's included.
fn cell_width(width: u32, columns: u32) -> u32 {
    width / columns.max(1)
}

/// The flow's row pitch, so the paint and a refresh resolving the rectangle an
/// item owes read one arithmetic.
///
/// There is no inter-column figure to carry: a slot's gap is its own plate's
/// margin, so the columns and the grid divide their width evenly.
#[must_use]
pub(super) fn pitch(scale: Scale, theme: &Theme) -> u32 {
    crate::view::Switchboard::row_item_height(scale, theme)
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
    pad: u32,
) -> Option<Rect> {
    let top =
        i64::from(primary.top()) + (i64::from(item.row) - i64::from(start)) * i64::from(pitch);
    let bottom = top + i64::from(item.rows.saturating_mul(pitch));
    let clipped_top = top.max(i64::from(primary.top()));
    let clipped_bottom = bottom.min(i64::from(primary.bottom()));
    if clipped_bottom <= clipped_top {
        return None;
    }
    let (left, width) = column_bounds(item.column, primary);
    if width == 0 {
        return None;
    }
    let height = u32::try_from(clipped_bottom - clipped_top).unwrap_or(0);
    let band = Rect::new(
        left,
        i32::try_from(clipped_top).unwrap_or(primary.top()),
        width,
        height,
    );
    if !item.plated {
        return Some(band);
    }
    // Inside the block's plate: the same padding the plate's own paint
    // reports, so a row lands where the plate says its content goes.
    let inner = Rect::new(
        band.left().saturating_add(to_i32(pad)),
        band.top().saturating_add(to_i32(pad)),
        band.width.saturating_sub(pad.saturating_mul(2)),
        band.height.saturating_sub(pad),
    );
    (!inner.is_empty()).then_some(inner)
}

/// The horizontal extent of one pane column within `primary`.
fn column_bounds(column: PaneColumn, primary: Rect) -> (i32, u32) {
    match column {
        PaneColumn::Full => (primary.left(), primary.width),
        PaneColumn::Leading | PaneColumn::Trailing => {
            let half = primary.width / 2;
            match column {
                PaneColumn::Trailing => (
                    primary.left() + to_i32(half),
                    primary.width.saturating_sub(half),
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
    window: PaneWindow<'_>,
    artwork: &mut dyn IconArtwork,
) {
    let pitch = pitch(window.scale, window.theme);
    let pad = crate::view::block::content_inset(window.scale, window.theme);
    for item in items {
        let Some(rect) = item_rect(item, window.primary, window.start, pitch, pad) else {
            continue;
        };
        render_item(surface, &item.body, rect, window, artwork);
    }
}

/// The window a pane's flow is drawn through: the rectangle it fills, the row
/// it is scrolled to, and the theme, scale and face every control resolves
/// from.
///
/// Grouped because the section already holds them together — it is the drawing
/// half of its own [`SectionCtx`](crate::view::SectionCtx) — and passing them
/// one by one alongside the items and the artwork lookup made a parameter list
/// nobody could read.
#[derive(Copy, Clone)]
pub(super) struct PaneWindow<'a> {
    /// The pane's own rectangle within the section.
    pub(super) primary: Rect,
    /// The first visible row of the flow.
    pub(super) start: u32,
    /// The active UI scale.
    pub(super) scale: Scale,
    /// The active theme.
    pub(super) theme: &'a Theme,
    /// The text face the flow's own prose is drawn in.
    pub(super) font: tairix_font::BitmapFont,
    /// The session's own account root, which resolves the icon of a consumer
    /// loaded from this user's own program store.
    pub(super) home: Option<&'a str>,
}

/// Paint one item into the rectangle the flow resolved for it.
///
/// Takes the whole `window` rather than the pieces of it an item happens to
/// need: it is one render context, and threading its parts one by one is what
/// grouping them existed to stop.
fn render_item(
    surface: &mut Surface,
    body: &ItemBody,
    rect: Rect,
    window: PaneWindow<'_>,
    artwork: &mut dyn IconArtwork,
) {
    let PaneWindow {
        scale,
        theme,
        font,
        home,
        ..
    } = window;
    let palette = theme.palette();
    match body {
        ItemBody::Hero {
            tile,
            chart,
            context,
            caption,
        } => render_hero(
            surface,
            HeroParts {
                tile,
                chart: chart.as_ref(),
                context,
                caption,
            },
            rect,
            window,
        ),
        ItemBody::Plate => {
            crate::view::block::plate(surface, rect, scale, theme);
        }
        ItemBody::Title { text, ruled } => {
            if *ruled {
                crate::view::block::title(surface, rect, scale, theme, text);
            } else {
                crate::view::block::bare_title(surface, rect, scale, theme, text);
            }
        }
        ItemBody::Fact(list) => list.render(surface, rect, scale, theme),
        ItemBody::Composition(bar) => bar.render(surface, rect, scale, theme),
        ItemBody::Cells { cells, columns } => {
            render_cells(surface, cells, *columns, rect, scale, theme);
        }
        ItemBody::Consumer { tile, bundle, name } => {
            let side = tile.icon_side(rect, scale, theme);
            let request = crate::view::task_icon(bundle.as_deref(), name, home);
            let picture = artwork.artwork(request, side);
            tile.render(surface, rect, scale, theme, picture);
        }
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
                font.truncate_to_width(text, rect.width),
                Color::from(palette.on_surface_muted),
            );
        }
    }
}

/// What a hero draws: its reading tile, its optional trace, the context lines
/// under the reading, and what the trace's extent means.
///
/// Grouped so the hero's own painter takes one value rather than four
/// positional arguments of similar shape. A bundle of borrows, so it copies
/// like the [`PaneWindow`] beside it.
#[derive(Copy, Clone)]
struct HeroParts<'a> {
    /// The headline figure and its unit.
    tile: &'a MetricTile,
    /// The trace beside it, where the reading has a history.
    chart: Option<&'a Chart>,
    /// The lines under the reading, in reading order.
    context: &'a [String],
    /// What the trace's horizontal extent means.
    caption: &'a str,
}

/// Paint a pane's hero: the reading column, then the trace and its caption.
fn render_hero(surface: &mut Surface, parts: HeroParts<'_>, rect: Rect, window: PaneWindow<'_>) {
    let PaneWindow {
        scale, theme, font, ..
    } = window;
    let Some(rect) = hero_rect(rect, scale, theme) else {
        return;
    };
    let muted = Color::from(theme.palette().on_surface_muted);
    let gap = scale.scale_length(theme.metrics().control_gap).max(1);
    let reading_w = match parts.chart {
        Some(_) => reading_column(&parts, rect.width, gap, font),
        None => rect.width,
    };
    let reading = Rect::new(rect.left(), rect.top(), reading_w, rect.height);
    parts.tile.render(surface, reading, scale, theme, None);

    let mut y = rect
        .top()
        .saturating_add(to_i32(parts.tile.measured_height(scale, theme)));
    for line in parts.context {
        if y.saturating_add(to_i32(font.line_height())) > rect.bottom() {
            break;
        }
        font.draw_text(
            surface,
            reading.left(),
            y,
            font.truncate_to_width(line, reading.width),
            muted,
        );
        y = y.saturating_add(to_i32(font.line_height()));
    }

    let Some(chart) = parts.chart else {
        return;
    };
    let left = rect.left() + to_i32(reading_w.saturating_add(gap));
    let width = rect.width.saturating_sub(reading_w).saturating_sub(gap);
    let axis = BitmapFont::for_role(theme.fonts(), TextRole::Caption, scale);
    let axis_h = axis.line_height().min(rect.height);
    let plot_h = rect.height.saturating_sub(axis_h);
    chart.render(
        surface,
        Rect::new(left, rect.top(), width, plot_h),
        scale,
        theme,
    );
    render_axis(
        surface,
        Rect::new(left, rect.top() + to_i32(plot_h), width, axis_h),
        parts.caption,
        (axis, theme),
    );
}

/// The hero's drawable rect within the band the flow gave it.
///
/// A row inside a plate is inset at the top only: that inset is the row's
/// leading and the rows stack. The hero is not a row — it spans the whole
/// plate — so it closes the bottom edge itself, or its axis sits on the rim.
fn hero_rect(band: Rect, scale: Scale, theme: &Theme) -> Option<Rect> {
    let rect = Rect::new(
        band.left(),
        band.top(),
        band.width,
        band.height
            .saturating_sub(crate::view::block::content_inset(scale, theme)),
    );
    (!rect.is_empty()).then_some(rect)
}

/// How wide the hero's reading column sits when a trace shares its row: what
/// its own widest context line needs, bounded so the trace always keeps the
/// greater share.
///
/// Measured rather than a fixed fraction of the hero. A third of the pane is
/// narrower than `53% committed · 7.4 GiB available` at body size, so the line
/// truncated mid-reading — and a fraction that happens to fit one pane's
/// wording says nothing about the next one's.
fn reading_column(parts: &HeroParts<'_>, width: u32, gap: u32, font: BitmapFont) -> u32 {
    let widest = parts
        .context
        .iter()
        .map(|line| font.text_width(line))
        .max()
        .unwrap_or(0);
    let floor = width / 4;
    // Half the hero less the gap: past that the trace is the smaller half and
    // stops being the thing the pane leads with.
    let ceiling = width.saturating_sub(gap) / 2;
    widest
        .saturating_add(gap)
        .clamp(floor.min(ceiling), ceiling)
}

/// The trailing marker: the box's newest slot is the present.
const AXIS_NOW: &str = "now";

/// How much of the muted foreground an axis label keeps, against the plate it
/// sits on.
///
/// An axis label is instrument furniture, not a reading: it says where the box
/// begins and ends and what its extent means, and at a reading's own weight it
/// competes with the trace above it. Derived from the theme's own muted
/// foreground rather than authored as a palette role, exactly as a toned
/// pill's wash is, so the whole row moves with a theme rather than needing one.
const AXIS_INK_PERMILLE: u16 = 570;

/// Paint a trace's axis row: how far back the box reaches, what its extent
/// means, and that its trailing edge is now.
///
/// The two markers are what make the box a *window* rather than a shape — a
/// trace with no span stated is a picture, not a reading — and the span is
/// derived from the sampler's own cadence, so it cannot drift from the
/// interval the points were taken at.
fn render_axis(
    surface: &mut Surface,
    rect: Rect,
    caption: &str,
    (font, theme): (BitmapFont, &Theme),
) {
    if rect.is_empty() {
        return;
    }
    let palette = theme.palette();
    let ink = Color::from(
        palette
            .on_surface_muted
            .mix(palette.surface, AXIS_INK_PERMILLE),
    );
    let span = trace_window_label();
    let now = AXIS_NOW;
    let span_w = font.text_width(&span);
    let now_w = font.text_width(now);
    font.draw_text(surface, rect.left(), rect.top(), &span, ink);
    font.draw_text(
        surface,
        rect.left() + to_i32(rect.width.saturating_sub(now_w)),
        rect.top(),
        now,
        ink,
    );
    // Centred in the box, and drawn only where it clears both markers: a
    // caption overlapping the span it is captioning reads as neither.
    let room = rect
        .width
        .saturating_sub(span_w.saturating_add(now_w))
        .saturating_sub(font.text_width("  ").saturating_mul(2));
    let fitted = font.truncate_to_width(caption, room);
    if fitted.is_empty() {
        return;
    }
    let text_w = font.text_width(fitted).min(rect.width);
    font.draw_text(
        surface,
        rect.left() + to_i32(rect.width.saturating_sub(text_w) / 2),
        rect.top(),
        fitted,
        ink,
    );
}

/// How far back a full trace reaches, from the chart's own window and the
/// sampler's own cadence.
///
/// Spelled in whole seconds rather than through the shared duration format:
/// that one answers "how long has this stood" and drops the seconds above a
/// minute, which would label a 128-second window `2m`.
fn trace_window_label() -> String {
    let seconds = u64::try_from(MAX_CHART_SAMPLES).unwrap_or(0)
        * (crate::schedule::SAMPLE_PERIOD_NS / 1_000_000_000).max(1);
    format!("-{seconds} s")
}

/// Paint one grid row's cells side by side, each with its own trace under
/// its name and its class badge in the corner.
///
/// The cell width comes from the grid's `columns`, never from this row's own
/// length, so a final short row draws its cells at the grid's pitch and
/// leaves the trailing slots empty instead of stretching them.
fn render_cells(
    surface: &mut Surface,
    cells: &[CellView],
    columns: u32,
    rect: Rect,
    scale: Scale,
    theme: &Theme,
) {
    let width = cell_width(rect.width, columns);
    if width == 0 {
        return;
    }
    for (index, cell) in cells.iter().enumerate() {
        let step = width.saturating_mul(u32::try_from(index).unwrap_or(0));
        let left = rect.left() + to_i32(step);
        // A cell is the same plate a block draws, and it is what separates
        // one core's figures from its neighbour's in a grid of a dozen; the
        // tile inside stays unplated so the cell's name, trace and two
        // readings share one surface rather than nesting a plate per reading.
        let Some(inner) = crate::view::block::plate(
            surface,
            Rect::new(left, rect.top(), width, rect.height),
            scale,
            theme,
        ) else {
            continue;
        };
        render_cell(surface, cell, inner, scale, theme);
    }
}

/// Paint one cell's three rows into the plate's interior: its name with the
/// class badge opposite, its trace between, and its busy share with the live
/// clock opposite.
///
/// The rows are measured from their own faces rather than by thirds, so the
/// readings sit on the cell's bottom line and the trace takes whatever height
/// is left between — the boards' anatomy, where dividing the interior in three
/// left the figures floating above a third of empty plate.
fn render_cell(surface: &mut Surface, cell: &CellView, inner: Rect, scale: Scale, theme: &Theme) {
    let palette = theme.palette();
    let name = BitmapFont::for_role(theme.fonts(), TextRole::Caption, scale);
    let figure = BitmapFont::for_role(theme.fonts(), TextRole::Metric, scale);
    let head_h = name.line_height().max(badge_side(scale, theme));
    let foot_h = figure.line_height().max(name.line_height());
    let Some(trend_h) = inner.height.checked_sub(head_h.saturating_add(foot_h)) else {
        return;
    };

    name.draw_text(
        surface,
        inner.left(),
        inner.top() + to_i32(head_h.saturating_sub(name.line_height()) / 2),
        name.truncate_to_width(&cell.label, inner.width),
        Color::from(palette.on_surface_muted),
    );
    render_badge(surface, cell.badge, inner, head_h, scale, theme);

    if trend_h > 0 {
        cell.trend.render(
            surface,
            Rect::new(
                inner.left(),
                inner.top() + to_i32(head_h),
                inner.width,
                trend_h,
            ),
            scale,
            theme,
        );
    }

    let foot_y = inner.top() + to_i32(head_h.saturating_add(trend_h));
    figure.draw_text(
        surface,
        inner.left(),
        foot_y,
        figure.truncate_to_width(&cell.busy, inner.width),
        Color::from(palette.on_surface),
    );
    let clock = name.truncate_to_width(&cell.clock, inner.width);
    let clock_w = name.text_width(clock).min(inner.width);
    name.draw_text(
        surface,
        inner.left() + to_i32(inner.width.saturating_sub(clock_w)),
        foot_y + to_i32(foot_h.saturating_sub(name.line_height()) / 2),
        clock,
        Color::from(palette.on_surface_muted),
    );
}

/// The side of a class badge: the header role's line with a hairline of
/// breathing room, so it reads as a compact mark in the cell's corner rather
/// than as a control seated there.
fn badge_side(scale: Scale, theme: &Theme) -> u32 {
    BitmapFont::for_role(theme.fonts(), TextRole::SectionHeader, scale)
        .line_height()
        .saturating_add(plate_border(theme, scale).saturating_mul(2))
}

/// Paint one core's class badge in the trailing corner of `inner`'s head row:
/// a small rounded box, its rim and letter in the class's tone.
///
/// Outlined rather than washed, and a box rather than a capsule: at badge size
/// a wash is indistinguishable from the plate behind it, and a capsule reads as
/// a pill of prose rather than as a mark. Its interior stays the plate's own
/// ground, so the rim and the letter carry the whole of it.
fn render_badge(
    surface: &mut Surface,
    badge: (&'static str, SignalRole),
    inner: Rect,
    head_h: u32,
    scale: Scale,
    theme: &Theme,
) {
    let (letter, tone) = badge;
    let side = badge_side(scale, theme);
    let font = BitmapFont::for_role(theme.fonts(), TextRole::SectionHeader, scale);
    let width = side.max(font.text_width(letter).saturating_add(side / 2));
    if width > inner.width || side > inner.height {
        return;
    }
    let (Ok(x), Ok(y)) = (
        u32::try_from(inner.left() + to_i32(inner.width.saturating_sub(width))),
        u32::try_from(inner.top() + to_i32(head_h.saturating_sub(side) / 2)),
    ) else {
        return;
    };
    let border = plate_border(theme, scale);
    // The theme's corner *proportion* rather than its length: a control's
    // radius over a control's height, applied to a mark a fraction of that
    // size. Taking the length itself made the radius half the badge's side,
    // which is a capsule — the shape a pill of prose wears, not a badge.
    let metrics = theme.metrics();
    let radius = scale
        .scale_length(metrics.control_corner_radius)
        .saturating_mul(side)
        / scale.scale_length(metrics.control_height).max(1);
    let ink = Color::from(theme.palette().signal(tone));
    surface.set_round_rect(x, y, width, side, radius, ink);
    if let Some((ix, iy, iw, ih)) = inset(x, y, width, side, border) {
        surface.set_round_rect(
            ix,
            iy,
            iw,
            ih,
            radius.saturating_sub(border),
            Color::from(theme.palette().surface_raised),
        );
    }
    let text_w = font.text_width(letter).min(width);
    font.draw_text(
        surface,
        to_i32(x + (width.saturating_sub(text_w) / 2)),
        to_i32(y + (side.saturating_sub(font.line_height()) / 2)),
        letter,
        ink,
    );
}

#[cfg(test)]
#[path = "pane_tests.rs"]
mod tests;
