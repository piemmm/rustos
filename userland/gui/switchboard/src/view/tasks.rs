//! The Tasks section: the live task/application table
//! (`plans/NEW-SWITCHBOARD.md` S3, S4).
//!
//! Owns the caller's task view model ([`TaskSummary`]), the census
//! [`MetricTile`]s the location band seats, the header band (the filter
//! [`Tabs`] over its own row and the [`SearchField`] over the next), the
//! sortable [`TableHeader`] and its [`TableRow`]s, the selected task's
//! command [`ActionRail`], the footer band (the shown/total count, the
//! auto-refresh [`Toggle`] and the grouping [`ComboBox`]), the grouping
//! [`Menu`] the Group command opens, and the section's layout, painting and
//! input.
//!
//! # The commands act on the selection, not on a row
//!
//! The table states what each task *is*; the trailing rail states what may be
//! *done* to whichever task is selected. Keeping the commands out of the rows
//! is what lets the rail name a task's whole repertoire — switch to it, pause
//! it, lower it, end it — instead of the one or two buttons a row's trailing
//! cell could hold, and it keeps the anchored commands still while the rows
//! scroll beneath them.
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
use core::mem;

use tairix_abi::origin::ProcId;
use tairix_abi::sysinfo::ProcessState;
use tairix_geometry::{to_i32, Rect, Region, Scale};
use tairix_icon::IconKind;
use tairix_input::{InputEvent, Key, Modifiers, NamedKey, PointerButton};
use tairix_raster::Surface;
use tairix_theme::Theme;

use tairix_controls::{
    ActionRail, ActivityState, Button, ButtonContent, CellAlign, Chart, ComboAction, ComboBox,
    ControlRole, ControlState, HeaderAction, HeaderColumn, Menu, MenuAction, MenuItem,
    MetricLayout, MetricTile, Panel, PressureKind, PressureState, RailAction, RecoveryState,
    RowAction, SearchField, SelectionState, SelectorAction, SortOrder, StatusPill, Tab, TableCell,
    TableHeader, TableRow, Tabs, TabsAction, Toggle,
};

use super::frame::{BandSummary, SectionAnatomy, SectionFrame, ACTION_RAIL_WIDTH};
use super::refresh::{carry_hover, restate_rail};
use super::{
    resolve_selection, ActionVerdict, FocusSweep, ListInfo, SectionCtx, SectionOutcome,
    SectionView, Switchboard, SwitchboardAction, SwitchboardModel, UNMEASURED_READING,
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
            Self::Job => IconKind::Job,
            Self::Service => IconKind::ServiceBundle,
        }
    }
}

/// A command the Tasks section can invoke on the selected task.
///
/// Each variant names an operation the service can genuinely carry out
/// ([`crate::model::apply_action`]), so the rail offers no command the system
/// cannot perform.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum TaskControl {
    /// Raise the task's own window and give it the focus.
    Switch,
    /// Show where the task's window is.
    Reveal,
    /// Suspend the task.
    Pause,
    /// Continue a suspended task.
    Resume,
    /// Lower the task's scheduling priority.
    LowerPriority,
    /// Show the task's own log entries.
    OpenLogs,
    /// End the task outright.
    ForceQuit,
}

/// What the caller may do to one task: one verdict per command, decided
/// where the caller's authority and the task's own state are both known
/// (`crate::model`) rather than guessed at render time.
///
/// [`Default`] refuses everything, so a task built without an explicit
/// verdict offers no command at all rather than a permitted one.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct TaskAuthority {
    /// Whether the session may be asked to raise the task's window. Shared
    /// by [`TaskControl::Switch`] and [`TaskControl::Reveal`], which are the
    /// same request of the session.
    pub switch: ActionVerdict,
    /// Whether the task may be suspended.
    pub pause: ActionVerdict,
    /// Whether the task may be continued.
    pub resume: ActionVerdict,
    /// Whether the task's priority may be lowered.
    pub lower_priority: ActionVerdict,
    /// Whether the task may be ended outright.
    pub force_quit: ActionVerdict,
}

impl Default for TaskAuthority {
    /// Every command refused: an unstated authority never grants one.
    fn default() -> Self {
        Self {
            switch: ActionVerdict::DeniedByAuthority,
            pause: ActionVerdict::DeniedByAuthority,
            resume: ActionVerdict::DeniedByAuthority,
            lower_priority: ActionVerdict::DeniedByAuthority,
            force_quit: ActionVerdict::DeniedByAuthority,
        }
    }
}

impl TaskAuthority {
    /// The verdict for one command — the single mapping the rail renders
    /// through and [`crate::model::apply_action`] re-checks against, so what
    /// is drawn and what is permitted can never disagree.
    ///
    /// [`TaskControl::OpenLogs`] is always [`ActionVerdict::DisabledByState`]:
    /// no capability-gated query for a task's own log entries exists yet, so
    /// the command states its absence plainly rather than pretending to be
    /// available or hiding the fact that logs are the natural next question.
    #[must_use]
    pub const fn verdict(&self, control: TaskControl) -> ActionVerdict {
        match control {
            TaskControl::Switch | TaskControl::Reveal => self.switch,
            TaskControl::Pause => self.pause,
            TaskControl::Resume => self.resume,
            TaskControl::LowerPriority => self.lower_priority,
            TaskControl::OpenLogs => ActionVerdict::DisabledByState,
            TaskControl::ForceQuit => self.force_quit,
        }
    }
}

/// One live task/application, as the caller's typed view model
/// (`plans/NEW-SWITCHBOARD.md`).
///
/// Switchboard renders it as a [`TableRow`] carrying the task's resource
/// pressure as a Pressure Rail and its recovery posture as a Signal Bead;
/// its activity is the Activity column's own sparkline, drawn where the
/// heading names it rather than as a seam under the whole row.
///
/// Every measured figure is an [`Option`]: `None` means the service did not
/// measure it, and the cell renders the explicit unmeasured mark. A zero
/// would read as a genuine idle reading, so an absent figure is never
/// flattened into one.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TaskSummary {
    /// The task's stable, never-reused instance identity.
    ///
    /// What the selection and the rail's subject are keyed by, so neither
    /// silently re-points at a different task when a refresh, a re-sort or a
    /// re-filter moves the rows around it. A numeric pid would be no better
    /// — the kernel reuses it.
    pub proc_id: ProcId,
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
    /// What the caller may do to this task, one verdict per rail command.
    pub authority: TaskAuthority,
    /// The activity this task is grouped into, as an index into
    /// [`SwitchboardModel::activities`](super::SwitchboardModel::activities); `None` when it is ungrouped.
    pub group: Option<usize>,
}

impl Default for TaskSummary {
    /// An unnamed, unmeasured task offering no command — the shape a caller
    /// fills in field by field, never a row that reads as a real one.
    fn default() -> Self {
        Self {
            proc_id: ProcId::KERNEL,
            name: String::new(),
            kind: TaskKind::default(),
            lifecycle: None,
            cpu_permille: None,
            memory_bytes: None,
            disk_bytes_per_sec: None,
            cpu_history: Vec::new(),
            pressure: PressureState::None,
            activity: ActivityState::Idle,
            recovery: RecoveryState::None,
            authority: TaskAuthority::default(),
            group: None,
        }
    }
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
/// Every column is a *reading* about the task; what may be done to it is the
/// trailing rail's business, not a column's. The Activity column carries a
/// sparkline rather than text and so is not sortable: there is no single
/// value to order by.
const COLUMNS: [ColumnSpec; 9] = [
    ColumnSpec {
        title: "Task",
        weight: 28,
        align: CellAlign::Leading,
        sortable: true,
    },
    ColumnSpec {
        title: "Type",
        weight: 10,
        align: CellAlign::Leading,
        sortable: true,
    },
    ColumnSpec {
        title: "State",
        weight: 10,
        align: CellAlign::Leading,
        sortable: true,
    },
    ColumnSpec {
        title: "Activity",
        weight: 12,
        align: CellAlign::Center,
        sortable: false,
    },
    ColumnSpec {
        title: "CPU",
        weight: 8,
        align: CellAlign::Trailing,
        sortable: true,
    },
    ColumnSpec {
        title: "Memory",
        weight: 10,
        align: CellAlign::Trailing,
        sortable: true,
    },
    ColumnSpec {
        title: "Disk",
        weight: 10,
        align: CellAlign::Trailing,
        sortable: true,
    },
    ColumnSpec {
        title: "Network",
        weight: 10,
        align: CellAlign::Trailing,
        sortable: false,
    },
    ColumnSpec {
        title: "Last active",
        weight: 12,
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

/// The column weights alone, in draw order — the one geometry every column
/// query is resolved through, so the heading, the cells and the sparkline can
/// never land in different places.
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

/// Which of a Tasks section's four cursor bands the keyboard is in.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
enum FocusBand {
    /// A header control, by its stop.
    Header(usize),
    /// A shown row, by its position in the arrangement.
    Row(usize),
    /// A rail command, by its slot.
    Rail(usize),
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

/// The census tiles' logical height, which is the height the location band
/// grows to in order to seat them: a stacked tile shows its label above its
/// reading, so it needs more than a single line.
const CENSUS_HEIGHT: u32 = 52;
/// One census tile's logical width: enough at the reference density for the
/// longest label ("Services") beside its icon.
const CENSUS_TILE_WIDTH: u32 = 104;
/// The filter strip's own logical row height.
const FILTER_HEIGHT: u32 = 28;
/// The search field's own logical row height, beneath the filter strip.
const SEARCH_HEIGHT: u32 = 30;
/// The footer band's logical height.
const FOOTER_HEIGHT: u32 = 28;

/// The rail's caption. The rail control carries no caption of its own, so
/// the section seats it in a [`Panel`], which already defines what a titled
/// container looks like.
const RAIL_TITLE: &str = "ACTIONS";

/// One command the rail offers for the selected task.
///
/// Most are a [`TaskControl`] the service carries out; `Group` is this
/// section's own popup, which reports its choice as a grouping edit rather
/// than as a task control, so it is named here rather than forced into the
/// control vocabulary.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
enum TaskCommand {
    /// Invoke a control on the selected task.
    Control(TaskControl),
    /// Open the Group popup for the selected task.
    Group,
}

/// One census tile: what it counts, what it says, and the glyph and identity
/// tint it wears.
struct CensusSpec {
    label: &'static str,
    /// The filter whose admitted tasks this tile counts — the same predicate
    /// the matching tab counts through, so a tile and its tab can never
    /// state different numbers for the same tasks.
    filter: TaskFilter,
    icon: IconKind,
    /// What tints the tile's glyph. An identity colour per kind of thing
    /// counted, not a claim that a resource is under strain.
    tint: PressureKind,
}

/// The census tiles, in reading order (`plans/switchboard1.png`).
///
/// The one declaration: the tiles are built from it and the room the location
/// band is asked for is measured from it, so the band can never seat a
/// different number of tiles than the section draws.
const CENSUS: [CensusSpec; 4] = [
    CensusSpec {
        label: "Processes",
        filter: TaskFilter::Processes,
        icon: IconKind::Executable,
        tint: PressureKind::Cpu,
    },
    CensusSpec {
        label: "Jobs",
        filter: TaskFilter::Jobs,
        icon: IconKind::Job,
        tint: PressureKind::Disk,
    },
    CensusSpec {
        label: "Services",
        filter: TaskFilter::Services,
        icon: IconKind::ServiceBundle,
        tint: PressureKind::Network,
    },
    CensusSpec {
        label: "Alerts",
        filter: TaskFilter::Faults,
        icon: IconKind::Bell,
        tint: PressureKind::Thermal,
    },
];

/// One rail command's presentation: what it does, what it says, the glyph
/// that says it without words, and the weight the plate carries.
struct CommandSpec {
    command: TaskCommand,
    label: &'static str,
    icon: IconKind,
    role: ControlRole,
}

/// The rail's commands, in the order they are offered
/// (`plans/switchboard1.png`).
///
/// Reading order is the order a reader reaches for them: go to the task,
/// then find it, then throttle it, then group it, and only last end it.
/// Force quit is [`ControlRole::Destructive`] so its plate wears the danger
/// rim, and it sits at the foot of the list where a mis-aimed press is
/// least likely to land on it.
const RAIL_COMMANDS: [CommandSpec; 8] = [
    CommandSpec {
        command: TaskCommand::Control(TaskControl::Switch),
        label: "Switch to",
        icon: IconKind::TaskSwitch,
        role: ControlRole::Neutral,
    },
    CommandSpec {
        command: TaskCommand::Control(TaskControl::Reveal),
        label: "Reveal window",
        icon: IconKind::Reveal,
        role: ControlRole::Neutral,
    },
    CommandSpec {
        command: TaskCommand::Control(TaskControl::Pause),
        label: "Pause",
        icon: IconKind::Pause,
        role: ControlRole::Neutral,
    },
    CommandSpec {
        command: TaskCommand::Control(TaskControl::Resume),
        label: "Resume",
        icon: IconKind::Resume,
        role: ControlRole::Neutral,
    },
    CommandSpec {
        command: TaskCommand::Control(TaskControl::LowerPriority),
        label: "Lower priority",
        icon: IconKind::Priority,
        role: ControlRole::Neutral,
    },
    CommandSpec {
        command: TaskCommand::Control(TaskControl::OpenLogs),
        label: "Open logs",
        icon: IconKind::Text,
        role: ControlRole::Neutral,
    },
    CommandSpec {
        command: TaskCommand::Group,
        label: "Group\u{2026}",
        icon: IconKind::Library,
        role: ControlRole::Neutral,
    },
    CommandSpec {
        command: TaskCommand::Control(TaskControl::ForceQuit),
        label: "Force quit",
        icon: IconKind::Quit,
        role: ControlRole::Destructive,
    },
];

/// One task rendered as a [`TableRow`] and its Activity sparkline.
///
/// The row carries no buttons: what may be done to a task belongs to the
/// section's own rail, which acts on the selection.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct TaskEntry {
    pub(super) row: TableRow,
    /// The task's own CPU history, as the Activity column's sparkline.
    pub(super) spark: Chart,
    /// The activity this task is grouped into, as of the last
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

/// Where the footer's controls sit: the shown/total count and the
/// auto-refresh toggle under the table, the grouping choice under the rail.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
struct FooterLayout {
    /// The shown/total readout.
    count: Rect,
    /// The auto-refresh toggle.
    refresh: Rect,
    /// The grouping choice, or `None` when the frame seated no rail for it
    /// to stand under.
    grouping: Option<Rect>,
}

/// The Tasks section: the adopted rows, the arrangement shown over them,
/// the header and footer bands, the selected task's commands, the Group
/// popup, and the keyboard's place among all of it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct TasksSection {
    /// Every adopted task, in model order — what the filter, search, sort
    /// and grouping arrange, and what a reported action's index names.
    pub(super) tasks: Vec<TaskSummary>,
    /// One row per *shown* task, in shown order.
    pub(super) entries: Vec<TaskEntry>,
    /// `order[i]` is the model index of shown row `i`, so a row the reader
    /// points at resolves back to the task rather than to a position.
    pub(super) order: Vec<usize>,
    /// The four census tiles the location band seats, in reading order.
    pub(super) census: Vec<MetricTile>,
    /// The selected task's own identity, so the selection survives a
    /// refresh, a re-filter and a re-sort rather than following whichever
    /// row slid into its place.
    pub(super) selected: Option<ProcId>,
    /// The selected task's commands.
    pub(super) rail: ActionRail,
    /// The plate the rail is seated in, which carries its caption.
    pub(super) rail_panel: Panel,
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
    /// the header's stops, one per shown row, the rail's commands, then the
    /// footer's stops.
    pub(super) focus: usize,
    /// Which of the focused thing's actions the cursor is on.
    pub(super) action: usize,
}

impl TasksSection {
    /// An empty Tasks section: no tasks, no selection, no popup, cursor on
    /// the filters.
    pub(super) fn new() -> Self {
        let mut section = Self {
            tasks: Vec::new(),
            entries: Vec::new(),
            order: Vec::new(),
            census: Vec::new(),
            selected: None,
            rail: ActionRail::new(Vec::new()),
            rail_panel: Panel::new(RAIL_TITLE),
            filters: filter_tabs(),
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
        self.order = order;
        // The selection is re-resolved against the rows now on show, so a
        // task the filter or the search has hidden stops being the subject
        // of commands the reader can no longer see it for.
        self.selected = resolve_selection(
            self.selected,
            self.order
                .iter()
                .filter_map(|index| self.tasks.get(*index))
                .map(|task| task.proc_id),
        );
        let selected = self.selected;
        let retired = mem::take(&mut self.entries);
        self.entries = self
            .order
            .iter()
            .filter_map(|index| self.tasks.get(*index))
            .map(|task| Self::build(task, selected == Some(task.proc_id)))
            .collect();
        // The rows are re-derived per slot rather than matched by identity, so
        // the slot carries the hover the pointer is still over and never a
        // press begun on whichever task held it.
        carry_hover(
            retired.iter().map(|entry| &entry.row),
            self.entries.iter_mut().map(|entry| &mut entry.row),
        );
        self.count = StatusPill::new(count_line(self.entries.len(), self.tasks.len()));
        self.rebuild_rail();
        self.restore_band(band);
    }

    /// The model index of the selected task, or `None` when nothing is
    /// selected or the selection is not among the rows on show.
    fn selected_index(&self) -> Option<usize> {
        let id = self.selected?;
        self.tasks.iter().position(|task| task.proc_id == id)
    }

    /// The identity of the task shown at row `row`.
    fn id_at_row(&self, row: usize) -> Option<ProcId> {
        self.tasks
            .get(*self.order.get(row)?)
            .map(|task| task.proc_id)
    }

    /// The selected task, or `None` when nothing is selected.
    fn selected_task(&self) -> Option<&TaskSummary> {
        self.tasks.get(self.selected_index()?)
    }

    /// Select `id` and rebuild everything that depends on which task is
    /// selected: the rows' selection marks and the rail's commands.
    fn select(&mut self, id: ProcId) {
        self.selected = Some(id);
        for row in 0..self.entries.len() {
            let selected = self.id_at_row(row) == Some(id);
            if let Some(entry) = self.entries.get_mut(row) {
                entry.row.set_selected(selected);
            }
        }
        self.rebuild_rail();
    }

    /// Rebuild the rail from the selected task's own verdicts.
    ///
    /// With nothing selected the rail holds no commands at all rather than a
    /// row of disabled ones: there is no subject for them to act on, and an
    /// empty rail states that more plainly than eight refusals would.
    fn rebuild_rail(&mut self) {
        let items = match self.selected_task() {
            Some(task) => {
                let authority = task.authority;
                RAIL_COMMANDS
                    .iter()
                    .map(|spec| command_button(spec, authority))
                    .collect()
            }
            None => Vec::new(),
        };
        restate_rail(&mut self.rail, items);
    }

    /// Which band the content cursor is in, and where within it.
    ///
    /// A raw cursor index means different things either side of a change in
    /// how many rows are shown — index 4 is a row in a long list and a
    /// footer control in an empty one — so a re-arrangement resolves the
    /// cursor through its band rather than by keeping the number.
    fn focus_band(&self) -> FocusBand {
        let Some(past_header) = self.focus.checked_sub(HEADER_STOPS) else {
            return FocusBand::Header(self.focus);
        };
        if past_header < self.entries.len() {
            return FocusBand::Row(past_header);
        }
        let past_rows = past_header.saturating_sub(self.entries.len());
        if past_rows < self.rail.len() {
            return FocusBand::Rail(past_rows);
        }
        FocusBand::Footer(past_rows.saturating_sub(self.rail.len()))
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
            // A rail whose commands have gone with the selection has no stop
            // to return to, so the cursor falls back to the last row — where
            // choosing a subject, which is what brings the rail back, lives.
            FocusBand::Rail(slot) => {
                if self.rail.is_empty() {
                    self.rows_end()
                } else {
                    HEADER_STOPS
                        .saturating_add(self.entries.len())
                        .saturating_add(slot.min(self.rail.len().saturating_sub(1)))
                }
            }
            FocusBand::Footer(stop) => HEADER_STOPS
                .saturating_add(self.entries.len())
                .saturating_add(self.rail.len())
                .saturating_add(stop.min(FOOTER_STOPS.saturating_sub(1))),
        };
        self.action = self
            .action
            .min(self.focused_action_count().saturating_sub(1));
    }

    /// The cursor stop of the last shown row, or the first header stop when
    /// no row is shown at all.
    fn rows_end(&self) -> usize {
        if self.entries.is_empty() {
            0
        } else {
            HEADER_STOPS.saturating_add(self.entries.len().saturating_sub(1))
        }
    }

    /// Build a task's table row and its own CPU sparkline.
    ///
    /// The row's state carries its resource pressure (a Pressure Rail down
    /// its leading edge), its recovery posture (a Signal Bead) and whether it
    /// is the selected row — but deliberately *not* its activity: an activity
    /// in a control's state paints a Heat Seam along the whole lower edge,
    /// which under a table row reads as a rule beneath every working task
    /// rather than as a reading about one. The activity is shown in the
    /// Activity column instead, as the sparkline the heading promises.
    fn build(task: &TaskSummary, selected: bool) -> TaskEntry {
        let mut state = ControlState::idle()
            .with_pressure(task.pressure)
            .with_recovery(task.recovery);
        if selected {
            state = state.with_selection(SelectionState::Selected);
        }
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

        TaskEntry {
            row: TableRow::new(cells).with_state(state),
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
    ///
    /// Each tile is plated and carries the glyph of the thing it counts, so
    /// the census reads as four distinct readings on the band rather than as
    /// four numbers running into the location trail beside them. The tile's
    /// [`PressureKind`] is what tints that glyph, so each kind of thing keeps
    /// its own identity colour; it is a tint, not a pressure verdict, and no
    /// tile claims a resource is under strain.
    fn build_census(&self) -> Vec<MetricTile> {
        CENSUS
            .iter()
            .map(|spec| {
                MetricTile::new(
                    spec.label,
                    count_text(self.count_of(spec.filter)),
                    spec.tint,
                )
                .with_layout(MetricLayout::Stacked)
                .with_icon(spec.icon)
            })
            .collect()
    }

    /// How many adopted tasks `filter` admits — the count a tab shows and
    /// the count its rows deliver, from the one predicate.
    fn count_of(&self, filter: TaskFilter) -> usize {
        self.tasks.iter().filter(|task| filter.admits(task)).count()
    }

    /// Re-label every filter tab with its own live count, in place.
    ///
    /// The strip holds one tab per filter for the life of the section, so a
    /// refresh has only labels to say. Building a fresh strip instead would
    /// drop what the *screen* holds — which tab the pointer rests on, which
    /// one the keyboard cursor is on, and any press waiting for its release
    /// — so a count moving under a resting pointer would blink the highlight
    /// off and swallow a click in flight.
    fn relabel_filters(&mut self) {
        let counts: Vec<usize> = TaskFilter::ALL
            .iter()
            .map(|filter| self.count_of(*filter))
            .collect();
        for ((tab, filter), count) in self
            .filters
            .tabs_mut()
            .iter_mut()
            .zip(TaskFilter::ALL.iter())
            .zip(counts)
        {
            tab.set_label(tab_label(*filter, count));
        }
    }

    /// The content-cursor stop that focuses shown row `row`, for a caller
    /// that knows a row and needs the cursor position naming it.
    pub(super) fn focus_index_for_row(&self, row: usize) -> usize {
        HEADER_STOPS.saturating_add(row.min(self.entries.len().saturating_sub(1)))
    }

    /// The content-cursor stop that focuses rail slot `slot`.
    #[cfg(test)]
    pub(super) fn rail_focus_index(&self, slot: usize) -> usize {
        HEADER_STOPS
            .saturating_add(self.entries.len())
            .saturating_add(slot.min(self.rail.len().saturating_sub(1)))
    }

    /// The rectangles of the header band's two rows: the filter strip, then
    /// the search field beneath it.
    ///
    /// The search reads over the whole table it searches rather than being
    /// squeezed into the end of the filter row: the two are separate
    /// questions — *which kind* of task, and *which* task — so each gets its
    /// own row and its own full width. The strip is clipped before the search
    /// is, so a header band too short for both still shows the filters.
    fn header_rows(frame: &SectionFrame, scale: Scale) -> (Rect, Rect) {
        let filters_h = scale.scale_length(FILTER_HEIGHT).min(frame.header.height);
        let filters = Rect::new(
            frame.header.left(),
            frame.header.top(),
            frame.header.width,
            filters_h,
        );
        let search = Rect::new(
            frame.header.left(),
            frame.header.top() + to_i32(filters_h),
            frame.header.width,
            frame.header.height.saturating_sub(filters_h),
        );
        (filters, search)
    }

    /// The census tiles' own rectangles within the band summary the location
    /// band seated, laid out in reading order with the theme's control gap
    /// between them.
    ///
    /// The one layout the paint reads, so a tile can never be drawn outside
    /// the region the band resolved for the whole census.
    fn census_rects(&self, summary: Rect, scale: Scale, theme: &Theme) -> Vec<Rect> {
        let count = u32::try_from(self.census.len()).unwrap_or(0);
        if count == 0 {
            return Vec::new();
        }
        let gap = scale.scale_length(theme.metrics().control_gap);
        let gaps = gap.saturating_mul(count.saturating_sub(1));
        let each = summary.width.saturating_sub(gaps) / count;
        (0..count)
            .map(|i| {
                Rect::new(
                    summary.left() + to_i32(each.saturating_add(gap).saturating_mul(i)),
                    summary.top(),
                    each,
                    summary.height,
                )
            })
            .collect()
    }

    /// The footer's rectangles: the shown/total count and the auto-refresh
    /// toggle share the width the table occupies, and the grouping control
    /// takes the column the rail stands in.
    ///
    /// Seating the grouping control under the rail rather than between the
    /// other two keeps each footer control beneath what it governs — the
    /// count and the refresh under the table, the arrangement under the
    /// commands — and it is the last region to be dropped, since a frame too
    /// narrow for the rail has no column to seat it in.
    fn footer_split(frame: &SectionFrame) -> FooterLayout {
        let table_w = frame.primary.width.min(frame.footer.width);
        let half = table_w / 2;
        let count = Rect::new(
            frame.footer.left(),
            frame.footer.top(),
            half,
            frame.footer.height,
        );
        let refresh = Rect::new(
            frame.footer.left() + to_i32(half),
            frame.footer.top(),
            table_w.saturating_sub(half),
            frame.footer.height,
        );
        let grouping = frame.rail.map(|rail| {
            Rect::new(
                rail.left(),
                frame.footer.top(),
                rail.width,
                frame.footer.height,
            )
        });
        FooterLayout {
            count,
            refresh,
            grouping,
        }
    }

    /// The rail's own content rectangle inside the plate that captions it,
    /// or `None` when the frame seated no rail or the plate leaves no room.
    fn rail_content(
        frame: &SectionFrame,
        panel: &Panel,
        scale: Scale,
        theme: &Theme,
    ) -> Option<Rect> {
        panel.content_rect(frame.rail?, scale, theme)
    }

    /// The rail's item rectangles, in rail order — the very rectangles the
    /// paint and the hit test share.
    #[cfg(test)]
    pub(super) fn rail_item_rects(&self, ctx: &SectionCtx<'_>) -> Vec<Rect> {
        let Some(content) = Self::rail_content(&ctx.frame, &self.rail_panel, ctx.scale, ctx.theme)
        else {
            return Vec::new();
        };
        (0..self.rail.len())
            .filter_map(|slot| self.rail.item_rect(content, slot, ctx.scale, ctx.theme))
            .collect()
    }

    /// Which rail slot holds the `Group…` command.
    pub(super) fn group_slot() -> usize {
        RAIL_COMMANDS
            .iter()
            .position(|spec| spec.command == TaskCommand::Group)
            .unwrap_or(0)
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

    /// Mark the column headings' focused heading, against the pinned heading
    /// rectangle the paint and the hit test share.
    fn mark_header(&mut self, index: Option<usize>, sweep: &mut FocusSweep<'_, '_>) {
        match sweep.ctx {
            Some(ctx) => self.header.set_focus(
                index,
                Self::header_rect(&ctx.frame, ctx.scale, ctx.theme),
                ctx.scale,
                ctx.theme,
                &COLUMN_WEIGHTS,
                sweep.damage,
            ),
            None => self.header.adopt_focus(index),
        }
    }

    /// Mark the filter strip's keyboard cursor, against the strip rectangle
    /// the paint and the hit test share.
    fn mark_filters(&mut self, index: Option<usize>, sweep: &mut FocusSweep<'_, '_>) {
        match sweep.ctx {
            Some(ctx) => {
                let (filters, _) = Self::header_rows(&ctx.frame, ctx.scale);
                self.filters.set_current(index, filters, sweep.damage);
            }
            None => self.filters.adopt_current(index),
        }
    }

    /// The grouping control's own field rectangle.
    ///
    /// A frame too narrow to seat the rail has no footer slot for it, so it
    /// falls back to the footer itself — somewhere inside the window rather
    /// than off its edge.
    fn grouping_field(frame: &SectionFrame) -> Rect {
        Self::footer_split(frame).grouping.unwrap_or(frame.footer)
    }

    /// The expanded grouping popup's rectangle, clamped inside the window.
    fn grouping_popup_rect(&self, ctx: SectionCtx<'_>) -> Rect {
        let field = Self::grouping_field(&ctx.frame);
        let (w, h) = self.grouping.popup_size(field.width, ctx.scale, ctx.theme);
        // A footer sits at the bottom of the content, so the list opens
        // upward from the field rather than off the bottom of the window.
        let top = field.top().saturating_sub(to_i32(h)).max(ctx.bounds.top());
        let left = field
            .left()
            .min(ctx.bounds.right().saturating_sub(to_i32(w)))
            .max(ctx.bounds.left());
        Rect::new(left, top, w, h)
    }

    /// The Group popup's anchor rectangle: the rail's own `Group` command,
    /// re-derived from the current layout every time, so it can never go
    /// stale across a resize or a scroll.
    ///
    /// The rail is anchored beside the table rather than scrolling with it,
    /// so the anchor no longer depends on where — or whether — the subject's
    /// row is on screen. A frame too narrow to seat the rail has no command
    /// to anchor on; the primary column's own rectangle is used instead so
    /// the popup still lands inside the window (fail closed, never a panic).
    fn anchor_rect(&self, ctx: SectionCtx<'_>) -> Rect {
        let anchored = Self::rail_content(&ctx.frame, &self.rail_panel, ctx.scale, ctx.theme)
            .and_then(|content| {
                self.rail
                    .item_rect(content, Self::group_slot(), ctx.scale, ctx.theme)
            });
        anchored.unwrap_or(ctx.frame.primary)
    }

    /// Open the Group popup for the given task.
    ///
    /// The item list is built once from the current group targets (spec T12):
    /// each activity, disabled with a reason when it is the task's current
    /// activity or is full; then `"New activity"`, disabled when the caller
    /// may not create one; then, only when the task is already grouped,
    /// `"Remove from activity"`.
    fn open_popup(&mut self, task: usize) {
        let Some(current) = self.tasks.get(task).map(|task| task.group) else {
            return;
        };
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

    /// Which rail command the content cursor is on, or `None` when it is
    /// elsewhere.
    fn focused_rail(&self) -> Option<usize> {
        let past_rows = self
            .focus
            .checked_sub(HEADER_STOPS.saturating_add(self.entries.len()))?;
        (past_rows < self.rail.len()).then_some(past_rows)
    }

    /// Which footer stop the content cursor is on, or `None` when it is
    /// elsewhere.
    fn focused_footer(&self) -> Option<usize> {
        let past_rail = self.focus.checked_sub(
            HEADER_STOPS
                .saturating_add(self.entries.len())
                .saturating_add(self.rail.len()),
        )?;
        (past_rail < FOOTER_STOPS).then_some(past_rail)
    }

    /// Total content-cursor stops: the header's, one per shown row, one per
    /// rail command, then the footer's.
    ///
    /// The header and footer are always reachable, so the cursor still has
    /// somewhere to be when the filter, the search, or an empty sample
    /// leaves no rows at all — and an empty rail simply contributes no stops
    /// rather than a stop that does nothing.
    fn focus_count(&self) -> usize {
        HEADER_STOPS
            .saturating_add(self.entries.len())
            .saturating_add(self.rail.len())
            .saturating_add(FOOTER_STOPS)
    }

    /// Dispatch the rail command in `slot` for the selected task.
    ///
    /// Nothing is dispatched without a selection: the rail holds no commands
    /// then, so this can only be reached with a subject in hand.
    fn invoke_rail(&mut self, slot: usize) -> Option<SectionOutcome> {
        let task = self.selected_index()?;
        match RAIL_COMMANDS.get(slot)?.command {
            TaskCommand::Control(control) => {
                Some(SectionOutcome::Action(SwitchboardAction::Task {
                    index: task,
                    control,
                }))
            }
            TaskCommand::Group => {
                self.open_popup(task);
                None
            }
        }
    }

    /// Apply a sort request from the column headings and re-arrange,
    /// reporting the headings whose caret changed.
    ///
    /// The header only *reports* the request; committing it here and
    /// re-reading it in [`Self::arrange`] keeps what is drawn and what is
    /// ordered the same one fact.
    fn apply_sort(
        &mut self,
        column: usize,
        order: SortOrder,
        ctx: SectionCtx<'_>,
        damage: &mut Region,
    ) {
        self.header.set_sort(
            Some((column, order)),
            Self::header_rect(&ctx.frame, ctx.scale, ctx.theme),
            ctx.scale,
            ctx.theme,
            &COLUMN_WEIGHTS,
            damage,
        );
        self.arrange();
    }

    /// Feed a key to whichever header control the cursor is on.
    fn header_on_key(
        &mut self,
        key: Key,
        ctx: SectionCtx<'_>,
        damage: &mut Region,
    ) -> Option<SectionOutcome> {
        let (filters, search) = Self::header_rows(&ctx.frame, ctx.scale);
        match self.focus {
            STOP_FILTERS => {
                if let Some(TabsAction::Selected { index }) =
                    self.filters.on_key(key, filters, damage)
                {
                    self.filters.set_selected(index, filters, damage);
                    self.arrange();
                }
                None
            }
            STOP_SEARCH => {
                if self
                    .search
                    .on_key(key, Modifiers::default(), search, damage)
                    .is_some()
                {
                    self.arrange();
                }
                None
            }
            STOP_SORT => {
                if let Some(HeaderAction::Sort { column, order }) = self.header.on_key(
                    key,
                    Self::header_rect(&ctx.frame, ctx.scale, ctx.theme),
                    ctx.scale,
                    ctx.theme,
                    &COLUMN_WEIGHTS,
                    damage,
                ) {
                    self.apply_sort(column, order, ctx, damage);
                }
                None
            }
            _ => None,
        }
    }

    /// Feed a key to the row the cursor is on: a row is selected, which is
    /// what the rail's commands act on.
    fn row_on_key(&mut self, row: usize, key: Key) -> Option<SectionOutcome> {
        if !matches!(key, Key::Named(NamedKey::Enter) | Key::Char(' ')) {
            return None;
        }
        let id = self.id_at_row(row)?;
        self.select(id);
        None
    }

    /// Feed a key to whichever footer control the cursor is on.
    fn footer_on_key(
        &mut self,
        stop: usize,
        key: Key,
        ctx: SectionCtx<'_>,
        damage: &mut Region,
    ) -> Option<SectionOutcome> {
        match stop {
            STOP_GROUPING => {
                if let Some(ComboAction::Selected { index }) = self.grouping.on_key(
                    key,
                    Self::grouping_field(&ctx.frame),
                    self.grouping_popup_rect(ctx),
                    ctx.scale,
                    ctx.theme,
                    damage,
                ) {
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

/// The filter strip a fresh section opens with: one tab per filter, showing
/// the filter the section starts on and a zero count until the first sample
/// lands.
///
/// This is the only strip a section ever holds; a refresh re-labels these tabs
/// rather than replacing them.
fn filter_tabs() -> Tabs {
    let mut tabs = Tabs::new(
        TaskFilter::ALL
            .iter()
            .map(|filter| Tab::new(tab_label(*filter, 0)))
            .collect(),
    );
    tabs.adopt_selected(
        TaskFilter::ALL
            .iter()
            .position(|filter| *filter == TaskFilter::default())
            .unwrap_or(0),
    );
    tabs
}

/// The footer's shown/total readout.
fn count_line(shown: usize, total: usize) -> String {
    format!("{shown} of {total} shown")
}

/// A count as tile text.
fn count_text(count: usize) -> String {
    format!("{count}")
}

/// How many census tiles there are, for the room the location band is asked
/// to seat them in.
///
/// Derived from the one [`CENSUS`] declaration, so the room asked for and the
/// tiles drawn cannot disagree.
fn census_tiles() -> u32 {
    u32::try_from(CENSUS.len()).unwrap_or(0)
}

/// One rail command's [`Button`], carrying the verdict `authority` reached
/// for it.
///
/// A refused command keeps its slot with the Authority Mark, and one the
/// task's own state rules out is plainly disabled, so the rail always states
/// the task's whole repertoire and why a part of it is unavailable rather
/// than hiding commands and leaving the reader to guess.
fn command_button(spec: &CommandSpec, authority: TaskAuthority) -> Button {
    let mut button = Button::new(
        ButtonContent::IconLabel {
            icon: spec.icon,
            label: String::from(spec.label),
        },
        spec.role,
    );
    button.set_state(match spec.command {
        TaskCommand::Control(control) => authority.verdict(control).to_state(),
        // Grouping is this section's own arrangement of tasks it can already
        // see, so it needs no authority over the task itself.
        TaskCommand::Group => ActionVerdict::Ready.to_state(),
    });
    button
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
    /// The census sits in the location band beside the trail; the header band
    /// carries the filter strip and the search field, one row each; the rail
    /// carries the selected task's commands; and the footer carries the
    /// count, the refresh toggle and the grouping choice.
    fn anatomy(&self) -> SectionAnatomy {
        SectionAnatomy {
            band_summary: Some(BandSummary {
                width: CENSUS_TILE_WIDTH.saturating_mul(census_tiles()),
                height: CENSUS_HEIGHT,
            }),
            sidebar_width: 0,
            header_height: FILTER_HEIGHT.saturating_add(SEARCH_HEIGHT),
            detail_width: 0,
            impact_width: 0,
            rail_width: ACTION_RAIL_WIDTH,
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

    /// Zero: a task's commands live in the anchored rail beside the table,
    /// so no row carries inline buttons of its own.
    fn row_buttons(&self) -> u32 {
        0
    }

    /// One per filter tab or sortable heading where the cursor traverses a
    /// strip; one everywhere else, since a row carries no controls of its own
    /// and a rail command is its own cursor stop.
    fn focused_action_count(&self) -> usize {
        match self.focus {
            STOP_FILTERS => self.filters.len().max(1),
            STOP_SORT => self.header.columns().len().max(1),
            _ => 1,
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

    fn set_row_action(&mut self, index: usize, sweep: &mut FocusSweep<'_, '_>) {
        self.action = index;
        // The filter strip and the column headings hold their own internal
        // cursor, so the shared action cursor is mirrored onto them rather
        // than kept as a second, separately-moving idea of the same thing.
        match self.focus {
            STOP_FILTERS => self.mark_filters(Some(index), sweep),
            STOP_SORT => self.mark_header(Some(index), sweep),
            _ => {}
        }
    }

    fn activate_focused(
        &mut self,
        key: Key,
        ctx: SectionCtx<'_>,
        damage: &mut Region,
    ) -> Option<SectionOutcome> {
        if self.focus < HEADER_STOPS {
            return self.header_on_key(key, ctx, damage);
        }
        if let Some(stop) = self.focused_footer() {
            return self.footer_on_key(stop, key, ctx, damage);
        }
        if let Some(slot) = self.focused_rail() {
            // The rail's own item decides whether it may act, so a refused
            // command consumes the key without dispatching anything.
            let rail = Self::rail_content(&ctx.frame, &self.rail_panel, ctx.scale, ctx.theme)
                .unwrap_or(Rect::EMPTY);
            self.rail.set_focus(Some(slot), rail, damage);
            let RailAction::Activate { index } = self.rail.on_key(key, rail, damage)?;
            return self.invoke_rail(index);
        }
        let row = self.focused_row()?;
        self.row_on_key(row, key)
    }

    /// Paint the census tiles the location band seated for this section.
    fn render_band(&self, surface: &mut Surface, rect: Rect, scale: Scale, theme: &Theme) {
        for (tile, rect) in self
            .census
            .iter()
            .zip(self.census_rects(rect, scale, theme))
        {
            tile.render(surface, rect, scale, theme, None);
        }
    }

    fn render(&self, surface: &mut Surface, ctx: SectionCtx<'_>) {
        let (filters, search) = Self::header_rows(&ctx.frame, ctx.scale);
        self.filters.render(surface, filters, ctx.scale, ctx.theme);
        self.search.render(surface, search, ctx.scale, ctx.theme);

        self.header.render(
            surface,
            Self::header_rect(&ctx.frame, ctx.scale, ctx.theme),
            ctx.scale,
            ctx.theme,
            &COLUMN_WEIGHTS,
        );

        let info = self.list_info(&ctx.frame, ctx.scale, ctx.theme);
        for slot in 0..info.visible() {
            let Some(entry) = self.entries.get(ctx.start + slot as usize) else {
                break;
            };
            let item = info.item_rect(slot);
            entry
                .row
                .render(surface, item, ctx.scale, ctx.theme, &COLUMN_WEIGHTS, None);
            if let Some(rect) = entry.spark_rect(item, ctx.scale, ctx.theme) {
                entry.spark.render(surface, rect, ctx.scale, ctx.theme);
            }
        }

        // The commands, in the plate that captions them. The plate is drawn
        // whether or not a task is selected, so the column keeps its place
        // and its caption rather than appearing and vanishing under the
        // reader as the selection changes.
        if let Some(rail) = ctx.frame.rail {
            self.rail_panel.render(surface, rail, ctx.scale, ctx.theme);
            if let Some(content) =
                Self::rail_content(&ctx.frame, &self.rail_panel, ctx.scale, ctx.theme)
            {
                self.rail.render(surface, content, ctx.scale, ctx.theme);
            }
        }

        let footer = Self::footer_split(&ctx.frame);
        self.count
            .render(surface, footer.count, ctx.scale, ctx.theme);
        self.auto_refresh
            .render(surface, footer.refresh, ctx.scale, ctx.theme);
        if let Some(grouping) = footer.grouping {
            self.grouping
                .render(surface, grouping, ctx.scale, ctx.theme);
        }
    }

    fn on_pointer(
        &mut self,
        event: &InputEvent,
        ctx: SectionCtx<'_>,
        damage: &mut Region,
    ) -> Option<SectionOutcome> {
        let (tabs, search) = Self::header_rows(&ctx.frame, ctx.scale);
        if let Some(TabsAction::Selected { index }) = self.filters.on_pointer(event, tabs, damage) {
            self.filters.set_selected(index, tabs, damage);
            self.arrange();
            return None;
        }
        if self
            .search
            .on_pointer(event, search, ctx.scale, ctx.theme, damage)
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
            self.apply_sort(column, order, ctx, damage);
            return None;
        }

        let footer = Self::footer_split(&ctx.frame);
        if let Some(grouping) = footer.grouping {
            let popup = self.grouping_popup_rect(ctx);
            match self
                .grouping
                .on_pointer(event, grouping, popup, ctx.scale, ctx.theme, damage)
            {
                Some(ComboAction::Selected { index }) => {
                    self.grouping.set_selected(index);
                    self.arrange();
                    return None;
                }
                Some(ComboAction::Opened | ComboAction::Closed) => return None,
                None => {}
            }
        }
        if let Some(SelectorAction::Set { on }) =
            self.auto_refresh.on_pointer(event, footer.refresh, damage)
        {
            self.auto_refresh.set_on(on);
            return None;
        }

        // The commands, before the rows: the rail is anchored beside the
        // table and never overlaps it, so the order is only a matter of
        // reaching the pressed control in one pass.
        if let Some(content) =
            Self::rail_content(&ctx.frame, &self.rail_panel, ctx.scale, ctx.theme)
        {
            if let Some(RailAction::Activate { index }) = self
                .rail
                .on_pointer(event, content, ctx.scale, ctx.theme, damage)
            {
                return self.invoke_rail(index);
            }
        }

        let info = self.list_info(&ctx.frame, ctx.scale, ctx.theme);
        let mut pressed = None;
        for slot in 0..info.visible() {
            let row = ctx.start + slot as usize;
            let Some(id) = self.id_at_row(row) else {
                break;
            };
            let item = info.item_rect(slot);
            let Some(entry) = self.entries.get_mut(row) else {
                break;
            };
            if entry.row.on_pointer(event, item, damage) == Some(RowAction::Activated) {
                pressed = Some(id);
            }
        }
        if let Some(id) = pressed {
            // Selection names the task, not the position it happens to
            // occupy: a re-sort or re-filter must not move the highlight to
            // whatever row slid into that slot. Choosing a task is also what
            // gives the rail its subject, so the commands are rebuilt for it.
            self.select(id);
        }
        None
    }

    fn apply_focus_marks(&mut self, focused: bool, sweep: &mut FocusSweep<'_, '_>) {
        let (stop, action) = (self.focus, self.action);
        let row_focus = self.focused_row();
        let rail_focus = self.focused_rail();
        let footer_focus = self.focused_footer();

        self.mark_filters((focused && stop == STOP_FILTERS).then_some(action), sweep);
        self.search.set_focused(focused && stop == STOP_SEARCH);
        self.mark_header((focused && stop == STOP_SORT).then_some(action), sweep);
        self.grouping
            .set_focused(focused && footer_focus == Some(STOP_GROUPING));
        self.auto_refresh
            .set_focused(focused && footer_focus == Some(STOP_REFRESH));

        // A row carries no controls of its own, so it takes the ring itself
        // rather than passing it to an action.
        for (i, entry) in self.entries.iter_mut().enumerate() {
            let here = focused && row_focus == Some(i);
            entry.row.set_focused(here);
            entry.row.set_in_focus_field(here);
        }

        let slot = focused.then_some(rail_focus).flatten();
        let rail = sweep
            .ctx
            .and_then(|ctx| Self::rail_content(&ctx.frame, &self.rail_panel, ctx.scale, ctx.theme));
        sweep.rail(&mut self.rail, slot, rail);
        for (index, button) in self.rail.items_mut().iter_mut().enumerate() {
            button.set_focused(slot == Some(index));
            button.set_in_focus_field(focused);
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
            );
        }
        let Some(popup) = &self.popup else {
            return;
        };
        let anchor = self.anchor_rect(ctx);
        let rect = Switchboard::popup_rect(&popup.menu, anchor, ctx.bounds, ctx.scale, ctx.theme);
        popup.menu.render(surface, rect, ctx.scale, ctx.theme);
    }

    /// A primary press outside the popup's bounds dismisses it without
    /// emitting; otherwise the event feeds the popup itself.
    fn overlay_on_pointer(
        &mut self,
        event: &InputEvent,
        ctx: SectionCtx<'_>,
        damage: &mut Region,
    ) -> Option<SectionOutcome> {
        if self.grouping.is_expanded() {
            let field = Self::grouping_field(&ctx.frame);
            let popup = self.grouping_popup_rect(ctx);
            if let Some(ComboAction::Selected { index }) = self
                .grouping
                .on_pointer(event, field, popup, ctx.scale, ctx.theme, damage)
            {
                self.grouping.set_selected(index);
                self.arrange();
            }
            return None;
        }
        let popup = self.popup.as_ref()?;
        let anchor = self.anchor_rect(ctx);
        let popup_rect =
            Switchboard::popup_rect(&popup.menu, anchor, ctx.bounds, ctx.scale, ctx.theme);

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
            .on_pointer(event, popup_rect, ctx.scale, ctx.theme, damage)
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
    fn overlay_on_key(
        &mut self,
        key: Key,
        ctx: SectionCtx<'_>,
        damage: &mut Region,
    ) -> Option<SectionOutcome> {
        if self.grouping.is_expanded() {
            if let Some(ComboAction::Selected { index }) = self.grouping.on_key(
                key,
                Self::grouping_field(&ctx.frame),
                self.grouping_popup_rect(ctx),
                ctx.scale,
                ctx.theme,
                damage,
            ) {
                self.grouping.set_selected(index);
                self.arrange();
            }
            return None;
        }
        let anchor = self.anchor_rect(ctx);
        let popup = self.popup.as_mut()?;
        let rect = Switchboard::popup_rect(&popup.menu, anchor, ctx.bounds, ctx.scale, ctx.theme);
        let action = popup.menu.on_key(key, rect, ctx.scale, ctx.theme, damage);
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
