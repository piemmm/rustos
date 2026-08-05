//! The Tasks section: the live task/application table
//! (`plans/NEW-SWITCHBOARD.md` S3, S4).
//!
//! Owns the caller's task view model ([`TaskSummary`]), the header band
//! (census [`MetricTile`]s, the filter [`Tabs`] and the [`SearchField`]),
//! the sortable [`TableHeader`] and its [`TableRow`]s, the footer band (the
//! shown/total count, the grouping [`ComboBox`] and the auto-refresh
//! [`Toggle`]), the grouping [`Menu`] a row's group action opens, and the
//! section's layout, painting and input.
//!
//! # Arrangement, not a second query
//!
//! Filtering, searching, sorting and grouping are pure *arrangements* of the
//! one set of rows the sample produced: the section's own `arrange` step is
//! the only place the shown order is decided, and it re-derives that order
//! from the adopted [`TaskSummary`]s rather than asking the system for a
//! different answer. Nothing here reads a figure the service did not measure.

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::cmp::Ordering;

use tairix_abi::sysinfo::ProcessState;
use tairix_geometry::{to_i32, Rect, Scale};
use tairix_icon::IconKind;
use tairix_input::{InputEvent, Key, Modifiers, PointerButton};
use tairix_raster::Surface;
use tairix_theme::Theme;

use tairix_controls::{
    ActivityState, Button, ButtonAction, CellAlign, Chart, ComboAction, ComboBox, ControlState,
    HeaderAction, HeaderColumn, Menu, MenuAction, MenuItem, MetricLayout, MetricTile, PressureKind,
    PressureState, RecoveryState, RowAction, SearchField, SelectorAction, SortOrder, StatusPill,
    Tab, TableCell, TableHeader, TableRow, Tabs, TabsAction, Toggle,
};

use super::frame::{SectionAnatomy, SectionFrame};
use super::{
    action_state, ListInfo, SectionCtx, SectionOutcome, SectionView, Switchboard,
    SwitchboardAction, SwitchboardModel, UNMEASURED_READING,
};
use crate::format::{format_bytes, format_rate, percent};

/// What kind of thing a task row *is*, as the row's Type column.
///
/// Each variant names a source the service genuinely reads, so the column
/// states a fact rather than a classification nobody measured. The System
/// Information API's process list reports processes and says nothing about
/// which of them a user would call an application, so there is deliberately
/// no `App` variant: guessing one would be a fabricated reading.
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq, Hash)]
pub enum TaskKind {
    /// A row from the process list — every row the service can build today.
    #[default]
    Process,
    /// A row from a background-job registry. No such registry exists yet,
    /// so nothing produces this variant; it is the Jobs filter's honest
    /// zero rather than a promise.
    Job,
    /// A row from the service manager's own registry, on the same footing
    /// as [`Self::Job`].
    Service,
}

impl TaskKind {
    /// The Type column's text for this kind.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Process => "Process",
            Self::Job => "Job",
            Self::Service => "Service",
        }
    }

    /// The glyph the Task column draws beside the name, naming what the row
    /// is rather than decorating it.
    #[must_use]
    pub const fn icon(self) -> IconKind {
        match self {
            Self::Process => IconKind::Executable,
            Self::Job => IconKind::Generic,
            Self::Service => IconKind::ServiceBundle,
        }
    }
}

/// One live task/application, as the caller's typed view model
/// (`plans/NEW-SWITCHBOARD.md`).
///
/// Switchboard renders it as a [`TableRow`] carrying the task's activity as
/// a Heat Seam, its resource pressure as a Pressure Rail, and its recovery
/// posture as a Signal Bead, with the row's own action [`Button`]s in the
/// trailing Actions column.
///
/// Every measured figure is an [`Option`]: `None` means the service did not
/// measure it, and the cell renders the explicit unmeasured mark. A zero
/// would read as a genuine idle reading, so an absent figure is never
/// flattened into one.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct TaskSummary {
    /// The task's display name.
    pub name: String,
    /// What this row is, for the Type column.
    pub kind: TaskKind,
    /// The task's lifecycle state, for the State column. `None` for a row
    /// whose source reports no lifecycle.
    pub lifecycle: Option<ProcessState>,
    /// The task's CPU share over the last sample interval, in permille.
    /// `None` on the first sample of a task (no interval to divide by).
    pub cpu_permille: Option<u16>,
    /// Bytes of memory mapped in the task's address space.
    pub memory_bytes: Option<u64>,
    /// The task's storage throughput over the last sample interval, in
    /// bytes per second, derived from the delta of its own I/O counters.
    /// `None` when there is no previous reading to delta against.
    pub disk_bytes_per_sec: Option<u64>,
    /// The task's own recent CPU readings, oldest first, for the Activity
    /// column's sparkline. Empty until the task has been measured once.
    pub cpu_history: Vec<u16>,
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

impl TaskSummary {
    /// The lifecycle's State-column text, or the unmeasured mark for a row
    /// whose source reports none.
    fn state_text(&self) -> &'static str {
        match self.lifecycle {
            Some(ProcessState::Runnable) => "Runnable",
            Some(ProcessState::Running) => "Running",
            Some(ProcessState::Blocked) => "Blocked",
            Some(ProcessState::Zombie) => "Zombie",
            Some(ProcessState::Stopped) => "Stopped",
            None => UNMEASURED_READING,
        }
    }

    /// Whether this task is in a condition the Recovery list would name.
    fn is_faulted(&self) -> bool {
        self.recovery != RecoveryState::None
    }
}

/// One column of the Tasks table: its heading, its share of the row's
/// width, how its cells align, and whether it can be sorted by.
///
/// One declaration per column, read by the heading, the cells and the
/// per-cell geometry alike, so a column can never be described one way in
/// the header and another in the rows.
struct ColumnSpec {
    /// The column heading's text.
    title: &'static str,
    /// The column's relative share of the row's content width.
    weight: u32,
    /// How this column's cells and heading align their text.
    align: CellAlign,
    /// Whether the heading offers to sort by this column.
    sortable: bool,
}

/// The Tasks table's columns, in draw order (`plans/switchboard1.png`).
///
/// The Activity column carries a sparkline rather than text, and the
/// Actions column carries the row's buttons, so neither is sortable: there
/// is no single value to order by.
const COLUMNS: [ColumnSpec; 10] = [
    ColumnSpec {
        title: "Task",
        weight: 26,
        align: CellAlign::Leading,
        sortable: true,
    },
    ColumnSpec {
        title: "Type",
        weight: 9,
        align: CellAlign::Leading,
        sortable: true,
    },
    ColumnSpec {
        title: "State",
        weight: 9,
        align: CellAlign::Leading,
        sortable: true,
    },
    ColumnSpec {
        title: "Activity",
        weight: 10,
        align: CellAlign::Center,
        sortable: false,
    },
    ColumnSpec {
        title: "CPU",
        weight: 7,
        align: CellAlign::Trailing,
        sortable: true,
    },
    ColumnSpec {
        title: "Memory",
        weight: 9,
        align: CellAlign::Trailing,
        sortable: true,
    },
    ColumnSpec {
        title: "Disk",
        weight: 9,
        align: CellAlign::Trailing,
        sortable: true,
    },
    ColumnSpec {
        title: "Network",
        weight: 9,
        align: CellAlign::Trailing,
        sortable: false,
    },
    ColumnSpec {
        title: "Last active",
        weight: 10,
        align: CellAlign::Trailing,
        sortable: false,
    },
    ColumnSpec {
        title: "Actions",
        weight: 14,
        align: CellAlign::Trailing,
        sortable: false,
    },
];

/// The Task column: the row's icon and name.
const COL_TASK: usize = 0;
/// The Type column.
const COL_TYPE: usize = 1;
/// The State column.
const COL_STATE: usize = 2;
/// The Activity column, whose rect the CPU sparkline is drawn into.
const COL_ACTIVITY: usize = 3;
/// The CPU column.
const COL_CPU: usize = 4;
/// The Memory column.
const COL_MEMORY: usize = 5;
/// The Disk column.
const COL_DISK: usize = 6;
/// The Network column, which has no interface to read and is always
/// unmeasured.
const COL_NETWORK: usize = 7;
/// The Last-active column, which has no interface to read and is always
/// unmeasured.
const COL_LAST_ACTIVE: usize = 8;
/// The trailing Actions column, which holds the row's own buttons.
const COL_ACTIONS: usize = 9;

/// The column weights alone, in draw order — the one geometry every column
/// query is resolved through, so the heading, the cells, the sparkline and
/// the action buttons can never land in different places.
///
/// Held as a constant rather than collected per call: every row asks for it
/// twice on every redraw, and the table redraws on every sample, so
/// building a fresh vector each time would be pure per-row waste for values
/// that cannot change.
const COLUMN_WEIGHTS: [u32; COLUMNS.len()] = column_weights();

/// [`COLUMN_WEIGHTS`], derived from the one column declaration so the two
/// can never disagree.
const fn column_weights() -> [u32; COLUMNS.len()] {
    let mut weights = [0; COLUMNS.len()];
    let mut i = 0;
    while i < COLUMNS.len() {
        weights[i] = COLUMNS[i].weight;
        i += 1;
    }
    weights
}

/// Which rows the filter strip is showing.
///
/// Only filters something real backs are offered. A "background" filter
/// would need a foreground/background distinction the process list does not
/// report, and a "recent" filter a last-active time nothing measures, so
/// neither exists here rather than each showing a guess.
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq, Hash)]
pub(super) enum TaskFilter {
    /// Every row.
    #[default]
    All,
    /// Rows the process list produced.
    Processes,
    /// Rows a background-job registry produced.
    Jobs,
    /// Rows the service registry produced.
    Services,
    /// Rows in a condition the Recovery list would name — stopped, or
    /// reported unresponsive.
    Faults,
}

impl TaskFilter {
    /// The filters the strip offers, in tab order.
    const ALL: [Self; 5] = [
        Self::All,
        Self::Processes,
        Self::Jobs,
        Self::Services,
        Self::Faults,
    ];

    /// This filter's tab label, without its count.
    const fn label(self) -> &'static str {
        match self {
            Self::All => "All",
            Self::Processes => "Processes",
            Self::Jobs => "Jobs",
            Self::Services => "Services",
            Self::Faults => "Faults",
        }
    }

    /// Whether `task` belongs in this filter — the one predicate the tab's
    /// count and the shown rows are both derived from, so a tab can never
    /// promise a count its rows do not deliver.
    fn admits(self, task: &TaskSummary) -> bool {
        match self {
            Self::All => true,
            Self::Processes => task.kind == TaskKind::Process,
            Self::Jobs => task.kind == TaskKind::Job,
            Self::Services => task.kind == TaskKind::Service,
            Self::Faults => task.is_faulted(),
        }
    }
}

/// How the footer's grouping control arranges the shown rows.
///
/// Grouping is an arrangement of the same rows, applied as the primary
/// ordering key before whatever column sort is active; it never adds,
/// removes or re-reads a row.
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq, Hash)]
pub(super) enum TaskGrouping {
    /// No grouping: the sort alone decides the order.
    #[default]
    Ungrouped,
    /// Rows of the same [`TaskKind`] together.
    ByType,
    /// Working rows before idle ones.
    ByActivity,
}

impl TaskGrouping {
    /// The groupings the footer offers, in choice order.
    const ALL: [Self; 3] = [Self::Ungrouped, Self::ByType, Self::ByActivity];

    /// This grouping's choice label.
    const fn label(self) -> &'static str {
        match self {
            Self::Ungrouped => "Ungrouped",
            Self::ByType => "By type",
            Self::ByActivity => "By activity",
        }
    }

    /// The group `task` falls in under this grouping. Rows sort by this
    /// first, so equal keys stay adjacent; `Ungrouped` gives every row the
    /// same key and so changes nothing.
    fn key(self, task: &TaskSummary) -> u8 {
        match self {
            Self::Ungrouped => 0,
            Self::ByType => match task.kind {
                TaskKind::Process => 0,
                TaskKind::Job => 1,
                TaskKind::Service => 2,
            },
            Self::ByActivity => match task.activity {
                ActivityState::Working => 0,
                _ => 1,
            },
        }
    }
}

/// Which of a Tasks section's three cursor bands the keyboard is in.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
enum FocusBand {
    /// A header control, by its stop.
    Header(usize),
    /// A shown row, by its position in the arrangement.
    Row(usize),
    /// A footer control, by its stop.
    Footer(usize),
}

/// The header band's own keyboard stops, ahead of the rows: the filter
/// strip, the search field, then the sortable column headings.
const HEADER_STOPS: usize = 3;
/// The filter strip's stop.
const STOP_FILTERS: usize = 0;
/// The search field's stop.
const STOP_SEARCH: usize = 1;
/// The column headings' stop.
const STOP_SORT: usize = 2;
/// The footer band's own keyboard stops, after the rows: the grouping
/// control, then the auto-refresh toggle.
const FOOTER_STOPS: usize = 2;
/// The grouping control's offset within the footer's stops.
const STOP_GROUPING: usize = 0;
/// The auto-refresh toggle's offset within the footer's stops.
const STOP_REFRESH: usize = 1;

/// The census band's logical height: one tile row.
const CENSUS_HEIGHT: u32 = 56;
/// The filter/search band's logical height.
const FILTER_HEIGHT: u32 = 28;
/// The footer band's logical height.
const FOOTER_HEIGHT: u32 = 28;

/// The share of the filter band's width the search field claims, as a
/// denominator: the strip takes the rest.
const SEARCH_WIDTH_DIVISOR: u32 = 4;

/// One task rendered as a [`TableRow`] plus its primary action [`Button`]
/// and its `Group` [`Button`] (which opens the Group popup menu).
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct TaskEntry {
    pub(super) row: TableRow,
    pub(super) action: Button,
    pub(super) group_button: Button,
    /// The task's own CPU history, as the Activity column's sparkline.
    pub(super) spark: Chart,
    /// The task's activity, as of the last
    /// [`adopt`](SectionView::adopt), mirroring [`TaskSummary::group`] so the
    /// Group popup can be built without the model.
    pub(super) group: Option<usize>,
}

/// One activity a task's Group popup may move it into: as much of the
/// activity as the popup's row needs, and no more.
///
/// The popup is this section's own control and has to be buildable from the
/// keyboard, with only a key in hand — so the choices it offers are derived
/// here from the same sample the rows are, rather than reached for across the
/// composition in the Activities section's own state.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct GroupTarget {
    /// The activity's display name, which labels its popup row.
    pub(super) name: String,
    /// Whether the activity can still take another member; a full one is
    /// offered disabled with its reason rather than hidden.
    pub(super) can_accept_member: bool,
}

/// The Group popup [`Menu`], anchored on a Tasks row's `Group` button.
///
/// It names the task by its index in the *model* rather than by a captured
/// screen rectangle or a shown-row position: the anchor rectangle is
/// re-derived from the current arrangement and layout every time the popup
/// is rendered or hit-tested, so it survives a resize, a scroll, and a
/// re-filter or re-sort that moves the row — and it needs no
/// bounds/scale/theme to open from the keyboard, which
/// [`Switchboard::on_key`] cannot supply.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct GroupPopup {
    pub(super) task: usize,
    pub(super) menu: Menu,
}

/// The Tasks section: the adopted rows, the arrangement shown over them,
/// the header and footer bands, the Group popup, and the keyboard's place
/// among all of it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct TasksSection {
    /// Every adopted task, in model order — what the filter, search, sort
    /// and grouping arrange, and what a reported action's index names.
    pub(super) tasks: Vec<TaskSummary>,
    /// One row plus its two actions per *shown* task, in shown order.
    pub(super) entries: Vec<TaskEntry>,
    /// `order[i]` is the model index of shown row `i`, so a row the reader
    /// points at resolves back to the task rather than to a position.
    pub(super) order: Vec<usize>,
    /// The four census tiles, in header order.
    pub(super) census: Vec<MetricTile>,
    /// The filter strip, each tab labelled with its own real count.
    pub(super) filters: Tabs,
    /// The name search over the shown rows.
    pub(super) search: SearchField,
    /// The sortable column headings.
    pub(super) header: TableHeader,
    /// The footer's shown/total readout, rebuilt whenever the arrangement
    /// changes so it can never quote a count the table is not showing.
    pub(super) count: StatusPill,
    /// The footer's grouping choice.
    pub(super) grouping: ComboBox,
    /// The footer's auto-refresh toggle.
    pub(super) auto_refresh: Toggle,
    /// The activities the Group popup offers, in model order.
    pub(super) group_targets: Vec<GroupTarget>,
    /// Whether the caller may group a task into a *new* activity.
    pub(super) can_create_activity: bool,
    /// The open Group popup, or `None` while it is closed.
    pub(super) popup: Option<GroupPopup>,
    /// Where the content cursor is among this section's focusable things:
    /// the header's stops, then one per shown row, then the footer's.
    pub(super) focus: usize,
    /// Which of the focused thing's actions the cursor is on.
    pub(super) action: usize,
}

impl TasksSection {
    /// The number of inline actions a task row carries: its primary action,
    /// then `Group`.
    const BUTTONS: u32 = 2;

    /// An empty Tasks section: no tasks, no popup, cursor on the filters.
    pub(super) fn new() -> Self {
        let mut section = Self {
            tasks: Vec::new(),
            entries: Vec::new(),
            order: Vec::new(),
            census: Vec::new(),
            filters: Tabs::new(
                TaskFilter::ALL
                    .iter()
                    .map(|filter| Tab::new(tab_label(*filter, 0)))
                    .collect(),
            ),
            search: SearchField::new().with_placeholder("Search tasks"),
            count: StatusPill::new(count_line(0, 0)),
            header: TableHeader::new(
                COLUMNS
                    .iter()
                    .map(|column| {
                        let heading = if column.sortable {
                            HeaderColumn::new(column.title)
                        } else {
                            HeaderColumn::fixed(column.title)
                        };
                        heading.with_align(column.align)
                    })
                    .collect(),
            ),
            grouping: ComboBox::new(
                TaskGrouping::ALL
                    .iter()
                    .map(|grouping| grouping.label().to_string())
                    .collect(),
            )
            .with_selected(0),
            auto_refresh: Toggle::new("Auto-refresh", true),
            group_targets: Vec::new(),
            can_create_activity: true,
            popup: None,
            focus: 0,
            action: 0,
        };
        section.census = section.build_census();
        section
    }

    /// The filter the strip currently shows, or [`TaskFilter::All`] when
    /// the selection is somehow out of range (fail closed to showing
    /// everything rather than hiding rows nobody asked to hide).
    fn filter(&self) -> TaskFilter {
        self.filters
            .selected()
            .and_then(|index| TaskFilter::ALL.get(index).copied())
            .unwrap_or(TaskFilter::All)
    }

    /// The grouping the footer currently shows, or
    /// [`TaskGrouping::Ungrouped`] when the selection is out of range.
    fn grouping(&self) -> TaskGrouping {
        self.grouping
            .selected()
            .and_then(|index| TaskGrouping::ALL.get(index).copied())
            .unwrap_or(TaskGrouping::Ungrouped)
    }

    /// Whether `task` survives the active filter *and* the active search —
    /// the one predicate deciding which rows are shown, so the footer's
    /// count and the rows themselves can never disagree.
    ///
    /// The search matches on the task's name, case-insensitively, so a
    /// reader who types what they see finds it whatever its capitalisation.
    fn shows(&self, task: &TaskSummary) -> bool {
        if !self.filter().admits(task) {
            return false;
        }
        let query = self.search.text();
        query.is_empty() || contains_ignore_case(&task.name, query)
    }

    /// Re-derive the shown rows from the adopted tasks: filter, search,
    /// group, sort, then build one entry per surviving row.
    ///
    /// The one place the shown order is decided. The sort is stable and is
    /// applied over the *filtered* rows, so re-filtering never reshuffles
    /// rows the reader was already looking at, and rows the active sort
    /// cannot separate keep the order the sample reported them in.
    fn arrange(&mut self) {
        let band = self.focus_band();
        let grouping = self.grouping();
        let sort = self.header.sort();
        let mut order: Vec<usize> = (0..self.tasks.len())
            .filter(|index| self.tasks.get(*index).is_some_and(|task| self.shows(task)))
            .collect();
        order.sort_by(|a, b| {
            let (Some(left), Some(right)) = (self.tasks.get(*a), self.tasks.get(*b)) else {
                return Ordering::Equal;
            };
            let grouped = grouping.key(left).cmp(&grouping.key(right));
            if grouped != Ordering::Equal {
                return grouped;
            }
            match sort {
                Some((column, order)) => {
                    let compared = compare_column(left, right, column);
                    if order == SortOrder::Ascending {
                        compared
                    } else {
                        compared.reverse()
                    }
                }
                None => Ordering::Equal,
            }
        });
        self.entries = order
            .iter()
            .filter_map(|index| self.tasks.get(*index))
            .map(Self::build)
            .collect();
        self.order = order;
        self.count = StatusPill::new(count_line(self.entries.len(), self.tasks.len()));
        self.restore_band(band);
    }

    /// Which band the content cursor is in, and where within it.
    ///
    /// A raw cursor index means different things either side of a change in
    /// how many rows are shown — index 4 is a row in a long list and a
    /// footer control in an empty one — so a re-arrangement resolves the
    /// cursor through its band rather than by keeping the number.
    fn focus_band(&self) -> FocusBand {
        match self.focus.checked_sub(HEADER_STOPS) {
            None => FocusBand::Header(self.focus),
            Some(past) if past < self.entries.len() => FocusBand::Row(past),
            Some(past) => FocusBand::Footer(past.saturating_sub(self.entries.len())),
        }
    }

    /// Put the cursor back in `band` against the arrangement now on show.
    ///
    /// A row the arrangement no longer has falls back to the last row it
    /// does have, and a table filtered down to nothing puts the cursor on
    /// the header — where the reader's next act, changing the filter or the
    /// search, actually lives — rather than stranding it on the footer.
    fn restore_band(&mut self, band: FocusBand) {
        self.focus = match band {
            FocusBand::Header(stop) => stop.min(HEADER_STOPS.saturating_sub(1)),
            FocusBand::Row(row) => {
                if self.entries.is_empty() {
                    0
                } else {
                    HEADER_STOPS.saturating_add(row.min(self.entries.len().saturating_sub(1)))
                }
            }
            FocusBand::Footer(stop) => HEADER_STOPS
                .saturating_add(self.entries.len())
                .saturating_add(stop.min(FOOTER_STOPS.saturating_sub(1))),
        };
        self.action = self
            .action
            .min(self.focused_action_count().saturating_sub(1));
    }

    /// Build a task's table row + primary action button + Group button +
    /// its own CPU sparkline.
    fn build(task: &TaskSummary) -> TaskEntry {
        let state = ControlState::idle()
            .with_pressure(task.pressure)
            .with_activity(task.activity)
            .with_recovery(task.recovery);
        let mut cells = Vec::with_capacity(COLUMNS.len());
        cells.push(TaskEntry::cell(COL_TASK, &task.name).with_icon(task.kind.icon()));
        cells.push(TaskEntry::cell(COL_TYPE, task.kind.label()));
        cells.push(TaskEntry::cell(COL_STATE, task.state_text()));
        // The Activity column's reading is the sparkline drawn over it, so
        // its cell carries no text of its own to draw underneath.
        cells.push(TaskEntry::cell(COL_ACTIVITY, ""));
        cells.push(TaskEntry::reading(COL_CPU, task.cpu_permille.map(percent)));
        cells.push(TaskEntry::reading(
            COL_MEMORY,
            task.memory_bytes.map(format_bytes),
        ));
        cells.push(TaskEntry::reading(
            COL_DISK,
            task.disk_bytes_per_sec.map(format_rate),
        ));
        // Neither of the next two has any interface to read: nothing in the
        // System Information API reports a per-task network figure or a
        // last-active time, so both are unmeasured for every row rather
        // than a zero or a plausible-looking number.
        cells.push(TaskEntry::reading(COL_NETWORK, None));
        cells.push(TaskEntry::reading(COL_LAST_ACTIVE, None));
        cells.push(TaskEntry::cell(COL_ACTIONS, ""));

        let row = TableRow::new(cells).with_state(state);
        let mut action = Button::labelled(task.action.clone());
        action.set_state(action_state(task.action_allowed));
        let group_button = Button::labelled("Group");
        TaskEntry {
            row,
            action,
            group_button,
            spark: Chart::new(PressureKind::Cpu).with_samples(task.cpu_history.iter().copied()),
            group: task.group,
        }
    }

    /// The four census tiles, each counting something the model genuinely
    /// carries.
    ///
    /// Processes counts the rows the process list produced; Jobs and
    /// Services count the rows a job registry and the service manager
    /// produced, which is honestly zero while neither exists; Alerts counts
    /// the tasks in a condition the Recovery list would name.
    ///
    /// Every tile counts adopted rows through the same filter predicate the
    /// tabs count through, so the Alerts tile and the Faults tab can never
    /// state different numbers for the same tasks.
    fn build_census(&self) -> Vec<MetricTile> {
        let counts = [
            ("Processes", self.count_of(TaskFilter::Processes)),
            ("Jobs", self.count_of(TaskFilter::Jobs)),
            ("Services", self.count_of(TaskFilter::Services)),
            ("Alerts", self.count_of(TaskFilter::Faults)),
        ];
        counts
            .iter()
            .map(|(label, count)| {
                MetricTile::new(*label, count_text(*count), PressureKind::Cpu)
                    .with_layout(MetricLayout::Stacked)
                    .unplated()
            })
            .collect()
    }

    /// How many adopted tasks `filter` admits — the count a tab shows and
    /// the count its rows deliver, from the one predicate.
    fn count_of(&self, filter: TaskFilter) -> usize {
        self.tasks.iter().filter(|task| filter.admits(task)).count()
    }

    /// Re-label every filter tab with its own live count.
    fn relabel_filters(&mut self) {
        let counts: Vec<usize> = TaskFilter::ALL
            .iter()
            .map(|filter| self.count_of(*filter))
            .collect();
        let selected = self.filters.selected().unwrap_or(0);
        self.filters = Tabs::new(
            TaskFilter::ALL
                .iter()
                .zip(counts)
                .map(|(filter, count)| Tab::new(tab_label(*filter, count)))
                .collect(),
        );
        self.filters.set_selected(selected);
    }

    /// The shown row a model `task` index currently occupies, or `None`
    /// when the active filter or search is hiding it.
    fn shown_row_of(&self, task: usize) -> Option<usize> {
        self.order.iter().position(|index| *index == task)
    }

    /// The content-cursor stop that focuses shown row `row`, for a caller
    /// that knows a row and needs the cursor position naming it.
    pub(super) fn focus_index_for_row(&self, row: usize) -> usize {
        HEADER_STOPS.saturating_add(row.min(self.entries.len().saturating_sub(1)))
    }

    /// The rectangles of the header band's two rows: the census tiles above
    /// the filter strip and search field.
    fn header_rows(frame: &SectionFrame, scale: Scale) -> (Rect, Rect) {
        let census_h = scale.scale_length(CENSUS_HEIGHT).min(frame.header.height);
        let census = Rect::new(
            frame.header.left(),
            frame.header.top(),
            frame.header.width,
            census_h,
        );
        let filter = Rect::new(
            frame.header.left(),
            frame.header.top() + to_i32(census_h),
            frame.header.width,
            frame.header.height.saturating_sub(census_h),
        );
        (census, filter)
    }

    /// The filter strip's and search field's rectangles within the filter
    /// band: the search claims a trailing share, the strip takes the rest.
    fn filter_split(band: Rect) -> (Rect, Rect) {
        let search_w = band.width / SEARCH_WIDTH_DIVISOR;
        let tabs_w = band.width.saturating_sub(search_w);
        (
            Rect::new(band.left(), band.top(), tabs_w, band.height),
            Rect::new(
                band.left() + to_i32(tabs_w),
                band.top(),
                search_w,
                band.height,
            ),
        )
    }

    /// The footer's three rectangles: the count text, the grouping control,
    /// then the auto-refresh toggle, each claiming a third.
    fn footer_split(frame: &SectionFrame) -> [Rect; 3] {
        let third = frame.footer.width / 3;
        let last = frame.footer.width.saturating_sub(third.saturating_mul(2));
        [
            Rect::new(
                frame.footer.left(),
                frame.footer.top(),
                third,
                frame.footer.height,
            ),
            Rect::new(
                frame.footer.left() + to_i32(third),
                frame.footer.top(),
                third,
                frame.footer.height,
            ),
            Rect::new(
                frame.footer.left() + to_i32(third.saturating_mul(2)),
                frame.footer.top(),
                last,
                frame.footer.height,
            ),
        ]
    }

    /// The pinned column-heading rectangle at the top of the primary
    /// region, above the rows that scroll beneath it.
    fn header_rect(frame: &SectionFrame, scale: Scale, theme: &Theme) -> Rect {
        let h = Switchboard::row_item_height(scale, theme).min(frame.primary.height);
        Rect::new(
            frame.primary.left(),
            frame.primary.top(),
            frame.primary.width,
            h,
        )
    }

    /// The expanded grouping popup's rectangle, clamped inside the window.
    fn grouping_popup_rect(&self, ctx: SectionCtx<'_>) -> Rect {
        let field = Self::footer_split(&ctx.frame)[1];
        let (w, h) = self
            .grouping
            .popup_size(field.width, ctx.scale, ctx.theme, ctx.font);
        // A footer sits at the bottom of the content, so the list opens
        // upward from the field rather than off the bottom of the window.
        let top = field.top().saturating_sub(to_i32(h)).max(ctx.bounds.top());
        let left = field
            .left()
            .min(ctx.bounds.right().saturating_sub(to_i32(w)))
            .max(ctx.bounds.left());
        Rect::new(left, top, w, h)
    }

    /// The Group popup's anchor rectangle: the shown row's `Group` button,
    /// re-derived from the current arrangement, layout and scroll offset
    /// every time, so it can never go stale across a resize, a scroll, or a
    /// re-filter that moved the row.
    ///
    /// A task that is scrolled out of view, or that the active filter is
    /// hiding, has no rectangle to anchor on; the primary column's own
    /// rectangle is used instead so the popup still lands somewhere inside
    /// the window (fail closed, never a panic).
    fn anchor_rect(&self, task: usize, ctx: SectionCtx<'_>) -> Rect {
        let info = self.list_info(&ctx.frame, ctx.scale, ctx.theme);
        if let Some(row) = self.shown_row_of(task) {
            if let Some(slot) = row.checked_sub(ctx.start) {
                if let Ok(slot) = u32::try_from(slot) {
                    if slot < info.visible() {
                        if let Some(entry) = self.entries.get(row) {
                            let buttons =
                                entry.action_rects(info.item_rect(slot), ctx.scale, ctx.theme);
                            if let Some(rect) = buttons.get(1) {
                                return *rect;
                            }
                        }
                    }
                }
            }
        }
        ctx.frame.primary
    }

    /// Open the Group popup, anchored on the given task's `Group` button.
    ///
    /// The item list is built once from the current group targets (spec T12):
    /// each activity, disabled with a reason when it is the task's current
    /// activity or is full; then `"New activity"`, disabled when the caller
    /// may not create one; then, only when the task is already grouped,
    /// `"Remove from activity"`.
    fn open_popup(&mut self, task: usize) {
        let Some(row) = self.shown_row_of(task) else {
            return;
        };
        let Some(entry) = self.entries.get(row) else {
            return;
        };
        let current = entry.group;
        let mut items: Vec<MenuItem> = self
            .group_targets
            .iter()
            .enumerate()
            .map(|(i, target)| {
                let mut item = MenuItem::new(target.name.clone());
                if current == Some(i) {
                    item = item
                        .with_state(ControlState::disabled())
                        .with_reason("Current activity");
                } else if !target.can_accept_member {
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
        self.popup = Some(GroupPopup {
            task,
            menu: Menu::new(items),
        });
    }

    /// Map an activated Group popup row to its [`SwitchboardAction`] and
    /// close the popup.
    fn resolve_activation(&mut self, index: usize) -> Option<SectionOutcome> {
        let popup = self.popup.take()?;
        let task = popup.task;
        let action = match index.cmp(&self.group_targets.len()) {
            Ordering::Less => SwitchboardAction::TaskGrouped {
                task,
                activity: Some(index),
            },
            Ordering::Equal => SwitchboardAction::TaskGrouped {
                task,
                activity: None,
            },
            Ordering::Greater => SwitchboardAction::TaskUngrouped { task },
        };
        Some(SectionOutcome::Action(action))
    }

    /// Which shown row the content cursor is on, or `None` when it is on
    /// the header or the footer.
    fn focused_row(&self) -> Option<usize> {
        let row = self.focus.checked_sub(HEADER_STOPS)?;
        (row < self.entries.len()).then_some(row)
    }

    /// Which footer stop the content cursor is on, or `None` when it is on
    /// the header or a row.
    fn focused_footer(&self) -> Option<usize> {
        let past_rows = self
            .focus
            .checked_sub(HEADER_STOPS.saturating_add(self.entries.len()))?;
        (past_rows < FOOTER_STOPS).then_some(past_rows)
    }

    /// Total content-cursor stops: the header's, one per shown row, then
    /// the footer's.
    ///
    /// The header and footer are always reachable, so the cursor still has
    /// somewhere to be when the filter, the search, or an empty sample
    /// leaves no rows at all.
    fn focus_count(&self) -> usize {
        HEADER_STOPS
            .saturating_add(self.entries.len())
            .saturating_add(FOOTER_STOPS)
    }

    /// Apply a sort request from the column headings and re-arrange.
    ///
    /// The header only *reports* the request; committing it here and
    /// re-reading it in [`Self::arrange`] keeps what is drawn and what is
    /// ordered the same one fact.
    fn apply_sort(&mut self, column: usize, order: SortOrder) {
        self.header.set_sort(Some((column, order)));
        self.arrange();
    }

    /// Feed a key to whichever header control the cursor is on.
    fn header_on_key(&mut self, key: Key) -> Option<SectionOutcome> {
        match self.focus {
            STOP_FILTERS => {
                if let Some(TabsAction::Selected { index }) = self.filters.on_key(key) {
                    self.filters.set_selected(index);
                    self.arrange();
                }
                None
            }
            STOP_SEARCH => {
                if self.search.on_key(key, Modifiers::default()).is_some() {
                    self.arrange();
                }
                None
            }
            STOP_SORT => {
                if let Some(HeaderAction::Sort { column, order }) = self.header.on_key(key) {
                    self.apply_sort(column, order);
                }
                None
            }
            _ => None,
        }
    }

    /// Feed a key to whichever footer control the cursor is on.
    fn footer_on_key(&mut self, stop: usize, key: Key) -> Option<SectionOutcome> {
        match stop {
            STOP_GROUPING => {
                if let Some(ComboAction::Selected { index }) = self.grouping.on_key(key) {
                    self.grouping.set_selected(index);
                    self.arrange();
                }
                None
            }
            STOP_REFRESH => {
                if let Some(SelectorAction::Set { on }) = self.auto_refresh.on_key(key) {
                    self.auto_refresh.set_on(on);
                }
                None
            }
            _ => None,
        }
    }
}

impl TaskEntry {
    /// A cell of plain label text for `column`, aligned as that column
    /// declares.
    fn cell(column: usize, text: &str) -> TableCell {
        let align = COLUMNS
            .get(column)
            .map_or(CellAlign::Leading, |spec| spec.align);
        TableCell::new(text).with_align(align)
    }

    /// A cell for a measured figure: the reading when the service measured
    /// it, or the explicit unmeasured mark — rendered disabled, so an
    /// absent figure cannot be mistaken for a small one — when it did not.
    fn reading(column: usize, text: Option<String>) -> TableCell {
        let align = COLUMNS
            .get(column)
            .map_or(CellAlign::Trailing, |spec| spec.align);
        match text {
            Some(text) => TableCell::numeric(text).with_align(align),
            None => TableCell::new(UNMEASURED_READING)
                .with_align(align)
                .with_state(ControlState::disabled()),
        }
    }

    /// The rectangles of this row's action buttons, laid out inside the
    /// Actions column's own cell rect.
    ///
    /// The geometry comes from [`TableRow::cell_rects`] — the same spans the
    /// row draws its cells with — so the buttons can never drift from the
    /// column the heading names.
    pub(super) fn action_rects(&self, bounds: Rect, scale: Scale, theme: &Theme) -> Vec<Rect> {
        let rects = self.row.cell_rects(bounds, scale, theme, &COLUMN_WEIGHTS);
        let Some(cell) = rects.get(COL_ACTIONS) else {
            return Vec::new();
        };
        let each = cell.width / TasksSection::BUTTONS;
        (0..TasksSection::BUTTONS)
            .map(|i| {
                Rect::new(
                    cell.left() + to_i32(each.saturating_mul(i)),
                    cell.top(),
                    each,
                    cell.height,
                )
            })
            .collect()
    }

    /// The Activity column's own rectangle, which the sparkline is drawn
    /// into — taken from the row's cell spans rather than re-derived.
    fn spark_rect(&self, bounds: Rect, scale: Scale, theme: &Theme) -> Option<Rect> {
        self.row
            .cell_rects(bounds, scale, theme, &COLUMN_WEIGHTS)
            .get(COL_ACTIVITY)
            .copied()
    }
}

/// A filter tab's label: its name and its own live count.
fn tab_label(filter: TaskFilter, count: usize) -> String {
    format!("{} {count}", filter.label())
}

/// The footer's shown/total readout.
fn count_line(shown: usize, total: usize) -> String {
    format!("{shown} of {total} shown")
}

/// A count as tile text.
fn count_text(count: usize) -> String {
    format!("{count}")
}

/// Whether `haystack` contains `needle`, ignoring ASCII case.
///
/// A task's name is arbitrary bytes rendered as display text, so the search
/// folds only ASCII case: anything beyond it is matched exactly rather than
/// by a locale rule this surface has no business inventing.
fn contains_ignore_case(haystack: &str, needle: &str) -> bool {
    if needle.is_empty() {
        return true;
    }
    let hay: Vec<u8> = haystack.bytes().map(|b| b.to_ascii_lowercase()).collect();
    let pin: Vec<u8> = needle.bytes().map(|b| b.to_ascii_lowercase()).collect();
    hay.windows(pin.len())
        .any(|window| window == pin.as_slice())
}

/// Order two tasks by one sortable column.
///
/// The one comparison the sort uses, so every column orders by the value
/// its cell shows. An unmeasured figure sorts *after* every measured one in
/// ascending order rather than as a zero, so "sort by CPU" never buries a
/// real reading under rows nobody measured. A column with no single value
/// to order by compares equal, which — the sort being stable — leaves the
/// rows exactly as they were.
fn compare_column(left: &TaskSummary, right: &TaskSummary, column: usize) -> Ordering {
    match column {
        COL_TASK => left.name.cmp(&right.name),
        COL_TYPE => left.kind.label().cmp(right.kind.label()),
        COL_STATE => left.state_text().cmp(right.state_text()),
        COL_CPU => compare_reading(left.cpu_permille, right.cpu_permille),
        COL_MEMORY => compare_reading(left.memory_bytes, right.memory_bytes),
        COL_DISK => compare_reading(left.disk_bytes_per_sec, right.disk_bytes_per_sec),
        _ => Ordering::Equal,
    }
}

/// Order two optional readings, an unmeasured one last.
fn compare_reading<T: Ord>(left: Option<T>, right: Option<T>) -> Ordering {
    match (left, right) {
        (Some(left), Some(right)) => left.cmp(&right),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => Ordering::Equal,
    }
}

impl SectionView for TasksSection {
    fn anatomy(&self) -> SectionAnatomy {
        SectionAnatomy {
            sidebar_width: 0,
            header_height: CENSUS_HEIGHT.saturating_add(FILTER_HEIGHT),
            detail_width: 0,
            impact_width: 0,
            rail_width: 0,
            footer_height: FOOTER_HEIGHT,
            primary_row_commands: 0,
        }
    }

    /// Adopt a fresh sample — unless the reader has turned auto-refresh
    /// off, in which case the table keeps showing the sample it already
    /// has rather than moving under them.
    fn adopt(&mut self, model: &SwitchboardModel) {
        if !self.auto_refresh.is_on() {
            return;
        }
        self.tasks.clone_from(&model.tasks);
        self.group_targets = model
            .activities
            .iter()
            .map(|activity| GroupTarget {
                name: activity.name.clone(),
                can_accept_member: activity.can_accept_member,
            })
            .collect();
        self.can_create_activity = model.can_create_activity;
        // The popup's rows are the group targets this sample has just
        // replaced, so a refresh drops it rather than re-validating a menu
        // built from the superseded list.
        self.popup = None;
        self.relabel_filters();
        self.census = self.build_census();
        self.arrange();
        self.action = 0;
    }

    fn item_count(&self) -> usize {
        self.entries.len()
    }

    fn list_info(&self, frame: &SectionFrame, scale: Scale, theme: &Theme) -> ListInfo {
        let header = Self::header_rect(frame, scale, theme);
        let rows = Rect::new(
            frame.primary.left(),
            frame.primary.top() + to_i32(header.height),
            frame.primary.width,
            frame.primary.height.saturating_sub(header.height),
        );
        ListInfo::rows(rows, self.entries.len(), scale, theme)
    }

    /// Zero: this table's actions are its trailing Actions *column*, which
    /// scrolls with the rows, not an anchored rail beside them.
    fn row_buttons(&self) -> u32 {
        0
    }

    fn focused_action_count(&self) -> usize {
        match self.focus {
            STOP_FILTERS => self.filters.len().max(1),
            STOP_SEARCH => 1,
            STOP_SORT => self.header.columns().len().max(1),
            _ => {
                if self.focused_row().is_some() {
                    Self::BUTTONS as usize
                } else {
                    1
                }
            }
        }
    }

    fn content_focus(&self) -> usize {
        self.focus
    }

    fn set_content_focus(&mut self, index: usize) {
        self.focus = index;
    }

    fn focus_span(&self) -> usize {
        self.focus_count()
    }

    fn focus_row(&self, index: usize) -> Option<usize> {
        let row = index.checked_sub(HEADER_STOPS)?;
        (row < self.entries.len()).then_some(row)
    }

    fn row_action(&self) -> usize {
        self.action
    }

    fn set_row_action(&mut self, index: usize) {
        self.action = index;
        // The filter strip and the column headings hold their own internal
        // cursor, so the shared action cursor is mirrored onto them rather
        // than kept as a second, separately-moving idea of the same thing.
        match self.focus {
            STOP_FILTERS => self.filters.set_current(Some(index)),
            STOP_SORT => self.header.set_focus(Some(index)),
            _ => {}
        }
    }

    fn activate_focused(&mut self, key: Key) -> Option<SectionOutcome> {
        if self.focus < HEADER_STOPS {
            return self.header_on_key(key);
        }
        if let Some(stop) = self.focused_footer() {
            return self.footer_on_key(stop, key);
        }
        let row = self.focused_row()?;
        let index = *self.order.get(row)?;
        let action = self.action;
        let entry = self.entries.get_mut(row)?;
        if action == 0 {
            return (entry.action.on_key(key) == Some(ButtonAction::Activated))
                .then_some(SectionOutcome::Action(SwitchboardAction::Task { index }));
        }
        if entry.group_button.on_key(key) == Some(ButtonAction::Activated) {
            self.open_popup(index);
        }
        None
    }

    fn render(&self, surface: &mut Surface, ctx: SectionCtx<'_>) {
        let (census, filter) = Self::header_rows(&ctx.frame, ctx.scale);
        let tile_w = census
            .width
            .checked_div(u32::try_from(self.census.len().max(1)).unwrap_or(1))
            .unwrap_or(0);
        for (i, tile) in self.census.iter().enumerate() {
            let x = census.left() + to_i32(tile_w.saturating_mul(u32::try_from(i).unwrap_or(0)));
            tile.render(
                surface,
                Rect::new(x, census.top(), tile_w, census.height),
                ctx.scale,
                ctx.theme,
                ctx.font,
            );
        }
        let (tabs, search) = Self::filter_split(filter);
        self.filters
            .render(surface, tabs, ctx.scale, ctx.theme, ctx.font);
        self.search
            .render(surface, search, ctx.scale, ctx.theme, ctx.font);

        self.header.render(
            surface,
            Self::header_rect(&ctx.frame, ctx.scale, ctx.theme),
            ctx.scale,
            ctx.theme,
            ctx.font,
            &COLUMN_WEIGHTS,
        );

        let info = self.list_info(&ctx.frame, ctx.scale, ctx.theme);
        for slot in 0..info.visible() {
            let Some(entry) = self.entries.get(ctx.start + slot as usize) else {
                break;
            };
            let item = info.item_rect(slot);
            entry.row.render(
                surface,
                item,
                ctx.scale,
                ctx.theme,
                ctx.font,
                &COLUMN_WEIGHTS,
            );
            if let Some(rect) = entry.spark_rect(item, ctx.scale, ctx.theme) {
                entry.spark.render(surface, rect, ctx.scale, ctx.theme);
            }
            let buttons = entry.action_rects(item, ctx.scale, ctx.theme);
            if let Some(rect) = buttons.first() {
                entry
                    .action
                    .render(surface, *rect, ctx.scale, ctx.theme, ctx.font);
            }
            if let Some(rect) = buttons.get(1) {
                entry
                    .group_button
                    .render(surface, *rect, ctx.scale, ctx.theme, ctx.font);
            }
        }

        let [count, grouping, refresh] = Self::footer_split(&ctx.frame);
        self.count
            .render(surface, count, ctx.scale, ctx.theme, ctx.font);
        self.grouping
            .render(surface, grouping, ctx.scale, ctx.theme, ctx.font);
        self.auto_refresh
            .render(surface, refresh, ctx.scale, ctx.theme, ctx.font);
    }

    fn on_pointer(&mut self, event: &InputEvent, ctx: SectionCtx<'_>) -> Option<SectionOutcome> {
        let (_, filter) = Self::header_rows(&ctx.frame, ctx.scale);
        let (tabs, search) = Self::filter_split(filter);
        if let Some(TabsAction::Selected { index }) = self.filters.on_pointer(event, tabs) {
            self.filters.set_selected(index);
            self.arrange();
            return None;
        }
        if self
            .search
            .on_pointer(event, search, ctx.scale, ctx.theme, ctx.font)
            .is_some()
        {
            self.arrange();
            return None;
        }
        if let Some(HeaderAction::Sort { column, order }) = self.header.on_pointer(
            event,
            Self::header_rect(&ctx.frame, ctx.scale, ctx.theme),
            ctx.scale,
            ctx.theme,
            &COLUMN_WEIGHTS,
        ) {
            self.apply_sort(column, order);
            return None;
        }

        let [_, grouping, refresh] = Self::footer_split(&ctx.frame);
        let popup = self.grouping_popup_rect(ctx);
        match self
            .grouping
            .on_pointer(event, grouping, popup, ctx.scale, ctx.theme)
        {
            Some(ComboAction::Selected { index }) => {
                self.grouping.set_selected(index);
                self.arrange();
                return None;
            }
            Some(ComboAction::Opened | ComboAction::Closed) => return None,
            None => {}
        }
        if let Some(SelectorAction::Set { on }) = self.auto_refresh.on_pointer(event, refresh) {
            self.auto_refresh.set_on(on);
            return None;
        }

        let info = self.list_info(&ctx.frame, ctx.scale, ctx.theme);
        let mut selected = None;
        for slot in 0..info.visible() {
            let row = ctx.start + slot as usize;
            let Some(&index) = self.order.get(row) else {
                break;
            };
            let item = info.item_rect(slot);
            let Some(entry) = self.entries.get_mut(row) else {
                break;
            };
            let buttons = entry.action_rects(item, ctx.scale, ctx.theme);
            if buttons.first().is_some_and(|rect| {
                entry.action.on_pointer(event, *rect) == Some(ButtonAction::Activated)
            }) {
                return Some(SectionOutcome::Action(SwitchboardAction::Task { index }));
            }
            if buttons.get(1).is_some_and(|rect| {
                entry.group_button.on_pointer(event, *rect) == Some(ButtonAction::Activated)
            }) {
                self.open_popup(index);
                return None;
            }
            if entry.row.on_pointer(event, item) == Some(RowAction::Activated) {
                selected = Some(index);
            }
        }
        if let Some(index) = selected {
            // Selection names the task, not the position it happens to
            // occupy: a re-sort or re-filter must not move the highlight to
            // whatever row slid into that slot.
            for (row, entry) in self.entries.iter_mut().enumerate() {
                entry.row.set_selected(self.order.get(row) == Some(&index));
            }
        }
        None
    }

    fn apply_focus_marks(&mut self, focused: bool) {
        let (stop, action) = (self.focus, self.action);
        let row_focus = self.focused_row();
        let footer_focus = self.focused_footer();

        self.filters
            .set_current((focused && stop == STOP_FILTERS).then_some(action));
        self.search.set_focused(focused && stop == STOP_SEARCH);
        self.header
            .set_focus((focused && stop == STOP_SORT).then_some(action));
        self.grouping
            .set_focused(focused && footer_focus == Some(STOP_GROUPING));
        self.auto_refresh
            .set_focused(focused && footer_focus == Some(STOP_REFRESH));

        for (i, entry) in self.entries.iter_mut().enumerate() {
            let here = focused && row_focus == Some(i);
            // The row is a member of the field but never takes the ring:
            // the ring belongs to whichever of its actions the cursor is on.
            entry.row.set_in_focus_field(here);
            entry.action.set_focused(here && action == 0);
            entry.action.set_in_focus_field(here);
            entry.group_button.set_focused(here && action == 1);
            entry.group_button.set_in_focus_field(here);
        }
    }

    fn holds_keyboard(&self) -> bool {
        self.popup.is_some() || self.grouping.is_expanded()
    }

    fn holds_pointer(&self) -> bool {
        self.popup.is_some() || self.grouping.is_expanded()
    }

    fn render_overlay(&self, surface: &mut Surface, ctx: SectionCtx<'_>) {
        if self.grouping.is_expanded() {
            self.grouping.render_popup(
                surface,
                self.grouping_popup_rect(ctx),
                ctx.scale,
                ctx.theme,
                ctx.font,
            );
        }
        let Some(popup) = &self.popup else {
            return;
        };
        let anchor = self.anchor_rect(popup.task, ctx);
        let rect = Switchboard::popup_rect(
            &popup.menu,
            anchor,
            ctx.bounds,
            ctx.scale,
            ctx.theme,
            ctx.font,
        );
        popup
            .menu
            .render(surface, rect, ctx.scale, ctx.theme, ctx.font);
    }

    /// A primary press outside the popup's bounds dismisses it without
    /// emitting; otherwise the event feeds the popup itself.
    fn overlay_on_pointer(
        &mut self,
        event: &InputEvent,
        ctx: SectionCtx<'_>,
    ) -> Option<SectionOutcome> {
        if self.grouping.is_expanded() {
            let [_, field, _] = Self::footer_split(&ctx.frame);
            let popup = self.grouping_popup_rect(ctx);
            if let Some(ComboAction::Selected { index }) = self
                .grouping
                .on_pointer(event, field, popup, ctx.scale, ctx.theme)
            {
                self.grouping.set_selected(index);
                self.arrange();
            }
            return None;
        }
        let popup = self.popup.as_ref()?;
        let anchor = self.anchor_rect(popup.task, ctx);
        let popup_rect = Switchboard::popup_rect(
            &popup.menu,
            anchor,
            ctx.bounds,
            ctx.scale,
            ctx.theme,
            ctx.font,
        );

        if let InputEvent::PointerPressed {
            button: PointerButton::Primary,
        } = event
        {
            if popup
                .menu
                .row_at(popup_rect, ctx.scale, ctx.theme, ctx.pointer)
                .is_none()
            {
                self.popup = None;
                return None;
            }
        }

        let popup = self.popup.as_mut()?;
        match popup
            .menu
            .on_pointer(event, popup_rect, ctx.scale, ctx.theme)
        {
            Some(MenuAction::Activated { index }) => self.resolve_activation(index),
            Some(MenuAction::Dismissed) => {
                self.popup = None;
                None
            }
            Some(MenuAction::OpenSubmenu { .. }) | None => None,
        }
    }

    /// Arrows move the popup's focus, Enter or Space activates the focused
    /// row, and Escape dismisses without emitting.
    fn overlay_on_key(&mut self, key: Key) -> Option<SectionOutcome> {
        if self.grouping.is_expanded() {
            if let Some(ComboAction::Selected { index }) = self.grouping.on_key(key) {
                self.grouping.set_selected(index);
                self.arrange();
            }
            return None;
        }
        let action = self.popup.as_mut()?.menu.on_key(key);
        match action {
            Some(MenuAction::Activated { index }) => self.resolve_activation(index),
            Some(MenuAction::Dismissed) => {
                self.popup = None;
                None
            }
            Some(MenuAction::OpenSubmenu { .. }) | None => None,
        }
    }

    fn dismiss_overlay(&mut self) {
        self.popup = None;
    }
}

#[cfg(test)]
#[path = "tasks_tests.rs"]
mod tests;
