//! The Switchboard reference composition (spec §17).
//!
//! Switchboard is the design language's flagship surface: it shows live task,
//! background-job, recovery, and system-overview state. It is assembled here
//! **purely from the shared Reactive Alloy controls** — the window-manager
//! [`WindowFrame`]/[`TitleBar`](crate::TitleBar)/[`ResizeGrabber`] furniture, a
//! header resource band of [`Meter`]s, a [`Tabs`] strip,
//! the collection controls ([`ListRow`], [`Card`], [`Panel`]), action
//! [`Button`]s, and one shared [`ScrollBar`] — with no application-painted
//! chrome and no second copy of any control's behaviour. It is the proof that
//! no TAIRiX surface needs custom chrome.
//!
//! # What it composes
//!
//! - The outer window is a [`WindowFrame`] with the standard
//!   [`TitleBar`](crate::TitleBar) and
//!   the four window commands; the only application region is the client
//!   viewport, so the client can never receive furniture input (the frame's
//!   hit map enforces this).
//! - Immediately below the title bar sits an always-visible header resource
//!   band: one [`Meter`] per [`ResourceSummary`] in the model, spaced evenly
//!   across the band's width. It is a read-only instrument, not a control —
//!   it takes no pointer or keyboard input and never produces a
//!   [`SwitchboardAction`] — so a press over it can never be mistaken for a
//!   press on the tab strip, the section content, or the scrollbar below it.
//!   An empty resource list collapses the band to zero height rather than
//!   drawing an empty strip.
//! - A [`Tabs`] strip selects one of the four [`Section`]s (Tasks, Jobs,
//!   Recovery, Overview). The host chooses which one the panel opens on —
//!   Recovery when the user reached for a flagged capsule, Tasks otherwise —
//!   with [`Switchboard::select_section`], never by feeding synthetic input.
//! - Each section's content is a vertical list drawn from the shared
//!   collection controls; when it exceeds the viewport the standard vertical
//!   [`ScrollBar`] governs it (mouse wheel, thumb drag, end buttons, track
//!   paging, and keyboard, all from the one shared scroll engine).
//! - A [`ResizeGrabber`] sits at the scrollbar junction, kept clear of the
//!   scroll thumb.
//!
//! # Data in, typed actions out
//!
//! The caller builds a [`SwitchboardModel`] of typed view models
//! ([`TaskSummary`], [`JobSummary`], [`RecoveryItem`], [`ResourceSummary`],
//! [`ServiceSummary`], [`SystemAction`]); Switchboard turns it into controls.
//! It performs no privileged work: every interaction emits a typed
//! [`SwitchboardAction`] the hosting service authorises and applies (a denied
//! action renders distinctly and fails closed, never activating).
//!
//! # Refreshing live data
//!
//! The model is a sample of a system that keeps moving, so a host publishes a
//! fresh one — around once a second — with
//! [`Switchboard::set_model`](Switchboard::set_model) rather than building the
//! composition again. The rows, cards, and meters are re-derived from the new
//! model; the selected section, every section's scroll offset, and the
//! keyboard focus are the user's and survive, so a scrolled or
//! keyboard-navigated list is never snatched back to the top by the next
//! sample. Row selection, hover, and a half-finished press name a row that may
//! now be a different object, so they are dropped rather than re-asserted.

use alloc::string::String;
use alloc::vec::Vec;
use core::cmp::Ordering;

use tairix_font::BitmapFont;
use tairix_geometry::{Point, Rect, Scale};
use tairix_input::{InputEvent, Key, Modifiers, NamedKey, PointerButton};
use tairix_raster::Surface;
use tairix_theme::Theme;

use crate::button::{Button, ButtonAction, ButtonContent};
use crate::collection::{Card, CardAction, ListRow, Panel, PanelAction, RowAction};
use crate::menu::{Menu, MenuAction, MenuItem};
use crate::meter::{Meter, MeterValue, MAX_HISTORY_SAMPLES};
use crate::paint::{clamp_permille, to_i32};
use crate::scroll::{ScrollModel, ScrollOrientation, ScrollRange};
use crate::scrollbar::{ScrollAction, ScrollBar};
use crate::state::{
    ActivityState, AuthorityState, ControlRole, ControlState, PressureKind, PressureState,
    RecoveryState, WindowActivationState, WindowControlKind, WindowFurnitureState, WindowSizeState,
};
use crate::tabs::{Tab, Tabs, TabsAction};
use crate::text::{TextAction, TextField};
use crate::window::{
    FrameLayout, FurniturePart, ResizeEvent, ResizeGrabber, ScrollCorner, TitleBarEvent,
    WindowFrame,
};

/// One of Switchboard's six top-level sections (spec §17).
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub enum Section {
    /// Live application/task list.
    Tasks,
    /// Background jobs with known or working progress.
    Jobs,
    /// Resource-pressure causes and their recommended relief actions.
    Pressure,
    /// Grouped tasks that move, pause, and close together.
    Activities,
    /// Hung objects and their recovery actions.
    Recovery,
    /// Resource, service, and system-action overview.
    Overview,
}

impl Section {
    /// The sections in tab order.
    pub const ALL: [Section; 6] = [
        Section::Tasks,
        Section::Jobs,
        Section::Pressure,
        Section::Activities,
        Section::Recovery,
        Section::Overview,
    ];

    /// The section's zero-based tab index.
    #[must_use]
    pub const fn index(self) -> usize {
        match self {
            Section::Tasks => 0,
            Section::Jobs => 1,
            Section::Pressure => 2,
            Section::Activities => 3,
            Section::Recovery => 4,
            Section::Overview => 5,
        }
    }

    /// The section for a tab index, or `None` if out of range (fail closed).
    #[must_use]
    pub fn from_index(index: usize) -> Option<Section> {
        Section::ALL.get(index).copied()
    }

    /// The section's tab label.
    #[must_use]
    pub const fn title(self) -> &'static str {
        match self {
            Section::Tasks => "Tasks",
            Section::Jobs => "Jobs",
            Section::Pressure => "Pressure",
            Section::Activities => "Activities",
            Section::Recovery => "Recovery",
            Section::Overview => "Overview",
        }
    }
}

// --- View models -----------------------------------------------------------

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
    /// [`SwitchboardModel::activities`]; `None` when it is ungrouped.
    pub group: Option<usize>,
}

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

/// One hung or recoverable object (spec §17).
///
/// Rendered as a recovery [`ListRow`] with a leading recovery
/// rail and bead, a Restart action ([`ControlRole::Recovery`]), and a Force
/// action ([`ControlRole::Destructive`] with a confirmation posture).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RecoveryItem {
    /// The object's display name.
    pub name: String,
    /// A short trailing detail (e.g. how long it has been unresponsive).
    pub detail: String,
    /// The object's recovery posture.
    pub recovery: RecoveryState,
    /// Whether an ordinary restart is available.
    pub can_restart: bool,
    /// Whether the high-impact force action is available.
    pub can_force: bool,
}

/// One system resource reading (spec §17).
///
/// One fact drives two renderings that must never disagree: the Overview
/// section's resource [`Card`] (identity, numeric reading, and a semantic
/// Pressure Rail) and the always-visible header band's [`Meter`] (the same
/// identity and reading, the same rail tint, and — when the caller supplies
/// one — a bounded history sparkline). [`ResourceSummary::new`] alone leaves
/// the meter honestly quiet: an unmeasured value at no pressure, never a
/// fabricated reading; a host that can measure the resource adds
/// [`with_meter`](Self::with_meter).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResourceSummary {
    /// The resource's display name (e.g. "CPU", "Memory").
    pub name: String,
    /// The numeric reading as display text (e.g. "62%", "5.1 GiB").
    pub reading: String,
    /// Which resource this is, mapping to its semantic rail colour.
    pub kind: PressureKind,
    /// The resource's load, drawn as the Overview card's Heat Seam.
    pub activity: ActivityState,
    meter: MeterValue,
    meter_pressure: PressureState,
    history: [u16; MAX_HISTORY_SAMPLES],
    history_len: usize,
}

impl ResourceSummary {
    /// A resource reading named `name`, showing `reading` for `kind`, with
    /// the Overview card's Heat Seam driven by `activity`.
    ///
    /// The header band's meter starts honestly unmeasured, at no pressure,
    /// with no history — a host with no wired query or a denied capability
    /// for this resource stops here rather than fabricating a reading; add
    /// a real measurement with [`with_meter`](Self::with_meter).
    #[must_use]
    pub fn new(
        name: impl Into<String>,
        reading: impl Into<String>,
        kind: PressureKind,
        activity: ActivityState,
    ) -> Self {
        Self {
            name: name.into(),
            reading: reading.into(),
            kind,
            activity,
            meter: MeterValue::Unmeasured,
            meter_pressure: PressureState::None,
            history: [0; MAX_HISTORY_SAMPLES],
            history_len: 0,
        }
    }

    /// This resource with the header band meter's measured `value`,
    /// `pressure` emphasis, and an oldest-to-newest sparkline `samples`.
    ///
    /// Each sample is a permille fraction, clamped fail closed; the series
    /// is capped to the most recent [`MAX_HISTORY_SAMPLES`], dropping the
    /// oldest first, and held inline in this struct — never on the heap — so
    /// building the model never allocates on the meter's account.
    #[must_use]
    pub fn with_meter(
        mut self,
        value: MeterValue,
        pressure: PressureState,
        samples: impl IntoIterator<Item = u16>,
    ) -> Self {
        self.meter = value;
        self.meter_pressure = pressure;
        self.history_len = 0;
        for sample in samples {
            if self.history_len == MAX_HISTORY_SAMPLES {
                self.history.copy_within(1.., 0);
                self.history_len -= 1;
            }
            self.history[self.history_len] = clamp_permille(sample);
            self.history_len += 1;
        }
        self
    }
}

/// One system service row (spec §17).
///
/// Rendered as a [`ListRow`] with a state bead and one
/// capability-aware action [`Button`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ServiceSummary {
    /// The service's display name.
    pub name: String,
    /// A short trailing detail (e.g. its state).
    pub detail: String,
    /// The service's recovery posture, if any.
    pub recovery: RecoveryState,
    /// The action's label (e.g. "Restart", "Stop").
    pub action: String,
    /// Whether the caller may perform the action (fail closed when false).
    pub action_allowed: bool,
}

/// One system-level action shown in the Overview panel header (spec §17).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SystemAction {
    /// The action's label (e.g. "Lock", "Shut Down").
    pub label: String,
    /// The action's role (e.g. [`ControlRole::System`] or
    /// [`ControlRole::Destructive`]); it drives the button's emphasis.
    pub role: ControlRole,
    /// Whether the caller may perform the action (fail closed when false).
    pub allowed: bool,
}

/// The typed outcome of checking whether an action may be performed, mapped
/// to exactly one [`ControlState`] (spec §13) so every Switchboard action
/// verdict — however it was reached — renders and fails closed the same way.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum ActionVerdict {
    /// The action is available.
    Ready,
    /// The object's own state makes the action invalid right now.
    DisabledByState,
    /// The caller lacks the authority to perform the action.
    DeniedByAuthority,
}

impl ActionVerdict {
    /// The one [`ControlState`] this verdict renders and fails closed as:
    /// [`ActionVerdict::Ready`] is idle and interactive,
    /// [`ActionVerdict::DisabledByState`] is a plain disabled control, and
    /// [`ActionVerdict::DeniedByAuthority`] carries the Authority Mark.
    #[must_use]
    pub const fn to_state(self) -> ControlState {
        match self {
            ActionVerdict::Ready => ControlState::idle(),
            ActionVerdict::DisabledByState => ControlState::disabled(),
            ActionVerdict::DeniedByAuthority => {
                ControlState::idle().with_authority(AuthorityState::NeedsCapability)
            }
        }
    }
}

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
    /// The culprit's index within [`SwitchboardModel::tasks`], if it is a
    /// task, so [`PressureControl::ShowTasks`] can focus it.
    pub task_index: Option<usize>,
    /// The recommended and alternative relief actions.
    pub actions: Vec<PressureAction>,
}

/// An action a Switchboard activity header row can request (spec T12).
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum ActivityControl {
    /// Switch to the activity.
    Switch,
    /// Pause every member of the activity.
    Pause,
    /// Resume every member of the activity.
    Resume,
    /// Close the activity and every member.
    Close,
}

/// One task grouped into an [`ActivitySummary`] (spec T12).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ActivityMember {
    /// The member's display name.
    pub name: String,
    /// A short trailing detail (e.g. owner, CPU%).
    pub detail: String,
    /// The member's live activity, drawn as its own Heat Seam.
    pub activity: ActivityState,
}

/// One activity: a named group of tasks that move, pause, and close together
/// (spec T12).
///
/// Rendered as a header [`ListRow`] plus one [`ListRow`] per
/// [`member`](Self::members), indented beneath it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ActivitySummary {
    /// A stable identity for this activity, independent of its position in
    /// the list, so an in-flight rename can survive a refresh that reorders
    /// or shortens [`SwitchboardModel::activities`].
    pub id: u64,
    /// The activity's display name.
    pub name: String,
    /// A short trailing detail (e.g. member count).
    pub detail: String,
    /// The activity's combined live activity, drawn as the header's Heat
    /// Seam.
    pub activity: ActivityState,
    /// Whether every member is currently paused.
    pub paused: bool,
    /// Whether the caller may pause/resume/close this activity.
    pub can_control: bool,
    /// Whether another task may still be grouped into this activity.
    pub can_accept_member: bool,
    /// The activity's member tasks.
    pub members: Vec<ActivityMember>,
}

/// The complete typed model Switchboard renders (spec §17).
///
/// It is one sample of a moving system, not a lasting handle: the caller hands
/// it to [`Switchboard::new`] to build the surface and hands each later sample
/// to [`Switchboard::set_model`], which re-derives the controls while leaving
/// the user's place in the surface alone. It carries no interaction state — no
/// selected section, scroll position, or focus — because those belong to the
/// live composition and would be stale here from the first user interaction.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SwitchboardModel {
    /// The window title.
    pub title: String,
    /// The window's furniture state (activation, size, movable, resizable).
    pub furniture: WindowFurnitureState,
    /// The live tasks.
    pub tasks: Vec<TaskSummary>,
    /// The background jobs.
    pub jobs: Vec<JobSummary>,
    /// The hung/recoverable objects.
    pub recovery: Vec<RecoveryItem>,
    /// The system resources. Drives both the always-visible header meter
    /// band and the Overview section's resource cards from the one model.
    pub resources: Vec<ResourceSummary>,
    /// The system services.
    pub services: Vec<ServiceSummary>,
    /// The system-level actions.
    pub system_actions: Vec<SystemAction>,
    /// The resource-pressure causes and their recommended relief actions.
    pub pressure: Vec<PressureCause>,
    /// The activities: named groups of tasks that move, pause, and close
    /// together.
    pub activities: Vec<ActivitySummary>,
    /// Whether the caller may group a task into a new activity.
    pub can_create_activity: bool,
}

impl SwitchboardModel {
    /// An empty model with the given title on an active, restored, movable,
    /// resizable window — the resting frame every Switchboard opens in.
    #[must_use]
    pub fn new(title: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            furniture: WindowFurnitureState {
                activation: WindowActivationState::Active,
                size: WindowSizeState::Restored,
                movable: true,
                resizable: true,
            },
            tasks: Vec::new(),
            jobs: Vec::new(),
            recovery: Vec::new(),
            resources: Vec::new(),
            services: Vec::new(),
            system_actions: Vec::new(),
            pressure: Vec::new(),
            activities: Vec::new(),
            can_create_activity: true,
        }
    }
}

/// A background-job action a Switchboard job card can request.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum JobControl {
    /// Pause the job.
    Pause,
    /// Cancel the job.
    Cancel,
}

/// A recovery action a Switchboard recovery row can request.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum RecoveryControl {
    /// Restart the hung object (an ordinary recovery).
    Restart,
    /// Force the object (the high-impact, confirmation-gated action).
    Force,
}

/// The typed outcome of interacting with a [`Switchboard`].
///
/// Switchboard never performs an operation itself: it reports the intent and
/// the hosting service authorises, validates, and applies it, then feeds the
/// updated model back (a refusal fails closed rather than acting).
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum SwitchboardAction {
    /// A window command (close/minimize/put-to-back/size toggle) was invoked.
    Window(WindowControlKind),
    /// The window should be activated (title-bar press).
    Activate,
    /// A cooperative window move gesture began.
    MoveBegin,
    /// A cooperative window move continued to `to` (screen coordinates).
    MoveTo {
        /// The new pointer position.
        to: Point,
    },
    /// A cooperative window move ended.
    MoveEnd,
    /// A resize gesture began; the host should capture the pointer.
    ResizeBegin,
    /// A resize gesture continued to `to` (screen coordinates).
    ResizeTo {
        /// The new pointer position.
        to: Point,
    },
    /// A resize gesture ended.
    ResizeEnd,
    /// A resize gesture was cancelled (restore the pre-drag geometry).
    ResizeCancel,
    /// The active section changed.
    SectionChanged {
        /// The newly selected section.
        section: Section,
    },
    /// A task's row action was invoked.
    Task {
        /// The task's index within the model.
        index: usize,
    },
    /// A background job's action was invoked.
    Job {
        /// The job's index within the model.
        index: usize,
        /// Which job action.
        control: JobControl,
    },
    /// A recovery action was invoked.
    Recovery {
        /// The object's index within the model.
        index: usize,
        /// Which recovery action.
        control: RecoveryControl,
    },
    /// A service's action was invoked.
    Service {
        /// The service's index within the model.
        index: usize,
    },
    /// A system-level action was invoked.
    System {
        /// The action's index within the model.
        index: usize,
    },
    /// The active section was scrolled to `offset` (in item units).
    Scrolled {
        /// The new first-visible item index.
        offset: u64,
    },
    /// A pressure cause's relief action was invoked.
    Pressure {
        /// The cause's index within the model.
        index: usize,
        /// Which relief action.
        control: PressureControl,
    },
    /// A task was grouped into an activity, or into a newly created one.
    TaskGrouped {
        /// The task's index within the model.
        task: usize,
        /// The activity's index within the model, or `None` to create a new
        /// activity containing just this task.
        activity: Option<usize>,
    },
    /// A task was removed from its activity.
    TaskUngrouped {
        /// The task's index within the model.
        task: usize,
    },
    /// An activity's action was invoked.
    Activity {
        /// The activity's index within the model.
        index: usize,
        /// Which activity action.
        control: ActivityControl,
    },
    /// An activity's inline rename was committed. The new name is read with
    /// [`Switchboard::submitted_activity_name`].
    ActivityRenamed {
        /// The activity's index within the model.
        index: usize,
    },
}

// --- Internal control state ------------------------------------------------

/// The title of the Overview section's system-action panel, named once so the
/// resting composition and every refresh of it cannot drift apart.
const SYSTEM_PANEL_TITLE: &str = "System";

/// The composed [`ControlState`] for an action whose availability is `allowed`.
///
/// A permitted action is interactive; a refused one is
/// [`AuthorityState::NeedsCapability`] so it renders with the Authority Mark
/// and fails closed on activation, never collapsing to a plain disabled look.
/// The one mapping every action verdict renders through is
/// [`ActionVerdict::to_state`]; this is simply its `bool` shorthand.
fn action_state(allowed: bool) -> ControlState {
    if allowed {
        ActionVerdict::Ready.to_state()
    } else {
        ActionVerdict::DeniedByAuthority.to_state()
    }
}

/// One task rendered as a [`ListRow`] plus its primary action [`Button`] and
/// its `Group` [`Button`] (which opens the Group popup menu).
#[derive(Clone, Debug, Eq, PartialEq)]
struct TaskEntry {
    row: ListRow,
    action: Button,
    group_button: Button,
    /// The task's activity, as of the last [`Switchboard::adopt`], mirroring
    /// [`TaskSummary::group`] so the Group popup can be built without the
    /// model.
    group: Option<usize>,
}

/// One recovery object rendered as a [`ListRow`] plus Restart and Force
/// [`Button`]s.
#[derive(Clone, Debug, Eq, PartialEq)]
struct RecoveryEntry {
    row: ListRow,
    restart: Button,
    force: Button,
}

/// One resource rendered as an Overview [`Card`] and the header band's
/// [`Meter`], both built once from the same [`ResourceSummary`] rather than
/// re-derived per frame.
#[derive(Clone, Debug, Eq, PartialEq)]
struct ResourceEntry {
    card: Card,
    meter: Meter,
}

/// One service rendered as a [`ListRow`] plus its action [`Button`].
#[derive(Clone, Debug, Eq, PartialEq)]
struct ServiceEntry {
    row: ListRow,
    action: Button,
}

/// One pressure cause rendered as a [`Card`], plus the cause's own relief
/// actions so a footer activation can be mapped back to its
/// [`ActionVerdict`] and [`PressureControl`] without the model.
#[derive(Clone, Debug, Eq, PartialEq)]
struct PressureEntry {
    card: Card,
    actions: Vec<PressureAction>,
    task_index: Option<usize>,
}

/// One activity rendered as a header [`ListRow`] plus its Switch/Pause-or-
/// Resume/Rename/Close [`Button`]s, and one [`ListRow`] per member.
#[derive(Clone, Debug, Eq, PartialEq)]
struct ActivityEntry {
    id: u64,
    name: String,
    detail: String,
    activity: ActivityState,
    header: ListRow,
    switch: Button,
    pause_resume: Button,
    rename: Button,
    close: Button,
    paused: bool,
    can_control: bool,
    can_accept_member: bool,
    members: Vec<ListRow>,
}

/// Which row a flattened Activities-section list index names: an activity's
/// own header row, or one of its member rows.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
enum ActivityRow {
    /// The header row of the activity at this index.
    Header(usize),
    /// A member row: the owning activity's index, then the member's index
    /// within it.
    Member(usize, usize),
}

/// The Group popup [`Menu`], anchored on a Tasks row's `Group` button.
///
/// It names the task by index rather than by a captured screen rectangle: the
/// anchor rectangle is re-derived from the current layout every time the
/// popup is rendered or hit-tested, so it never goes stale across a resize —
/// and it needs no bounds/scale/theme to open from the keyboard, which
/// [`Switchboard::on_key`] cannot supply.
#[derive(Clone, Debug, Eq, PartialEq)]
struct GroupPopup {
    task: usize,
    menu: Menu,
}

/// An in-flight inline rename of an activity's header row.
///
/// `id` is the activity's stable identity (spec T12): a model refresh that
/// still has an activity with this `id` relocates `index` to match, so typing
/// survives a refresh unless the activity itself is gone.
#[derive(Clone, Debug, Eq, PartialEq)]
struct RenameEdit {
    id: u64,
    index: usize,
    field: TextField,
}

/// Which region of the composition currently holds keyboard focus, cycled by
/// the Tab key so the whole surface is keyboard-navigable.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
enum FocusRegion {
    /// The window-command group in the title bar.
    TitleBar,
    /// The section tab strip.
    Tabs,
    /// The active section's content list.
    Content,
    /// The vertical scrollbar.
    Scrollbar,
}

impl FocusRegion {
    /// The regions in Tab-cycle order.
    const ORDER: [FocusRegion; 4] = [
        FocusRegion::Tabs,
        FocusRegion::Content,
        FocusRegion::Scrollbar,
        FocusRegion::TitleBar,
    ];

    /// The next region in the cycle.
    fn next(self) -> FocusRegion {
        let idx = Self::ORDER.iter().position(|&r| r == self).unwrap_or(0);
        Self::ORDER[(idx + 1) % Self::ORDER.len()]
    }
}

// --- Switchboard -----------------------------------------------------------

/// The Switchboard reference composition (spec §17).
///
/// A stateful composed surface built entirely from the shared Reactive Alloy
/// controls. Build it from a [`SwitchboardModel`] with [`Switchboard::new`],
/// choose the section it opens on with
/// [`select_section`](Switchboard::select_section), paint it with
/// [`render`](Switchboard::render), and feed it input with
/// [`on_pointer`](Switchboard::on_pointer) and [`on_key`](Switchboard::on_key);
/// each interaction returns a typed [`SwitchboardAction`] for the hosting
/// service to authorise and apply.
///
/// It outlives any one sample of the data: publish each fresh reading with
/// [`set_model`](Switchboard::set_model), which re-derives every row, card, and
/// meter but keeps the section, scroll offsets, and keyboard focus the user
/// chose.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Switchboard {
    frame: WindowFrame,
    tabs: Tabs,
    grabber: ResizeGrabber,
    corner: ScrollCorner,
    scroll: ScrollBar,
    tasks: Vec<TaskEntry>,
    jobs: Vec<Card>,
    recovery: Vec<RecoveryEntry>,
    resources: Vec<ResourceEntry>,
    services: Vec<ServiceEntry>,
    pressure: Vec<PressureEntry>,
    activities: Vec<ActivityEntry>,
    can_create_activity: bool,
    panel: Panel,
    section: Section,
    offsets: [u64; 6],
    focus: FocusRegion,
    content_focus: usize,
    /// Which of the focused row's/card's several buttons is keyboard-focused
    /// (Left/Right cycles it; Enter activates it). Reset whenever
    /// `content_focus` or `section` changes.
    row_action: usize,
    pointer: Point,
    group_popup: Option<GroupPopup>,
    rename: Option<RenameEdit>,
    submitted_activity_name: Option<String>,
}

impl Switchboard {
    /// Build a Switchboard from a typed model, turning each view model into
    /// its shared control.
    ///
    /// It opens on [`Section::Tasks`] at the top of the list; a host that
    /// wants another section calls
    /// [`select_section`](Switchboard::select_section), and one that samples
    /// live state feeds each new reading to
    /// [`set_model`](Switchboard::set_model) rather than building again.
    #[must_use]
    pub fn new(model: SwitchboardModel) -> Self {
        let mut switchboard = Self {
            frame: WindowFrame::new(model.furniture),
            tabs: Tabs::new(
                Section::ALL
                    .iter()
                    .map(|s| Tab::new(s.title()))
                    .collect::<Vec<_>>(),
            ),
            grabber: ResizeGrabber::new(),
            corner: ScrollCorner::new(),
            scroll: ScrollBar::new(
                ScrollOrientation::Vertical,
                ScrollModel::new(ScrollRange::EMPTY, 1, 1),
            ),
            tasks: Vec::new(),
            jobs: Vec::new(),
            recovery: Vec::new(),
            resources: Vec::new(),
            services: Vec::new(),
            pressure: Vec::new(),
            activities: Vec::new(),
            can_create_activity: true,
            panel: Panel::new(SYSTEM_PANEL_TITLE),
            section: Section::Tasks,
            offsets: [0; 6],
            focus: FocusRegion::Content,
            content_focus: 0,
            row_action: 0,
            pointer: Point::ORIGIN,
            group_popup: None,
            rename: None,
            submitted_activity_name: None,
        };
        switchboard
            .frame
            .title_bar_mut()
            .set_app_name("Switchboard");
        switchboard.tabs.set_selected(Section::Tasks.index());
        switchboard.adopt(model);
        switchboard
    }

    /// Show `model` in place of the one currently drawn, keeping the parts of
    /// the surface the *user* owns.
    ///
    /// A host samples live system state continuously — roughly once a second —
    /// and this is how it publishes each new reading. Rebuilding the whole
    /// composition instead would throw away the user's place in the list every
    /// sample, snapping a scrolled or keyboard-navigated list back to the top.
    ///
    /// **Kept, because the user set it:** the selected [`Section`] and its tab
    /// mark, every section's scroll offset, the keyboard focus region and its
    /// position in the list, the last pointer position, and any window move,
    /// resize, or scroll-thumb drag in flight.
    ///
    /// **Dropped, because it names a row that may now be a different object:**
    /// row selection, pointer hover, and any half-finished press. A row index
    /// survives a refresh only as a *position* in the list, never as an
    /// identity: the rows are rebuilt from `model`, so a press begun on one
    /// task can never complete against whatever task now occupies that slot,
    /// and a highlight is never re-asserted onto a row the pointer is not
    /// really over. Hover returns with the next pointer movement.
    ///
    /// The list position the keyboard focus names is clamped into the new
    /// content, and the active section's scroll offset is re-ranged through
    /// the same clamp a section switch uses, so a list that shrank leaves
    /// neither past its end. An emptied section leaves a valid, renderable
    /// state with nothing to activate.
    pub fn set_model(&mut self, model: SwitchboardModel) {
        self.adopt(model);
        self.set_scroll_range(
            self.active_count(),
            self.scroll.model().range().viewport_extent(),
        );
    }

    /// Derive every model-shaped part of the composition from `model` — the
    /// window furniture and title, each section's rows, cards, and meters, and
    /// the Overview panel's system actions — then re-assert the keyboard focus
    /// onto the controls that replaced the old ones.
    ///
    /// This is the one model-to-controls derivation. Both
    /// [`new`](Switchboard::new) and [`set_model`](Switchboard::set_model) run
    /// it, so a refreshed Switchboard holds exactly the controls a freshly
    /// built one would, marked exactly the same way. The focused list position
    /// is clamped into the new content first, so it can never address a row
    /// the new model does not have.
    fn adopt(&mut self, model: SwitchboardModel) {
        let furniture = model.furniture;
        self.frame.set_furniture(furniture);
        self.frame.title_bar_mut().set_title(&model.title);

        let active = furniture.activation != WindowActivationState::Inactive;
        self.grabber
            .set_enabled(furniture.resizable && furniture.size == WindowSizeState::Restored);
        self.grabber.set_active_frame(active);
        self.corner.set_active_frame(active);

        self.tasks = model.tasks.into_iter().map(Self::build_task).collect();
        self.jobs = model.jobs.into_iter().map(Self::build_job).collect();
        self.recovery = model
            .recovery
            .into_iter()
            .map(Self::build_recovery)
            .collect();
        self.resources = model
            .resources
            .into_iter()
            .map(Self::build_resource)
            .collect();
        self.services = model
            .services
            .into_iter()
            .map(Self::build_service)
            .collect();
        self.panel = Panel::new(SYSTEM_PANEL_TITLE).with_actions(
            model
                .system_actions
                .into_iter()
                .map(Self::build_system_button)
                .collect(),
        );
        self.pressure = model
            .pressure
            .into_iter()
            .map(Self::build_pressure)
            .collect();
        self.activities = model
            .activities
            .into_iter()
            .map(Self::build_activity)
            .collect();
        self.can_create_activity = model.can_create_activity;

        // The Group popup only ever anchors on a Tasks row, and every section
        // change already drops it; a refresh drops it too, rather than
        // re-validating a menu built from the now-superseded activity list.
        self.group_popup = None;

        // An in-flight rename survives a refresh only as long as its activity
        // still exists, re-located by stable id — never by its old position,
        // which a refresh may have shifted or removed entirely (fail closed).
        self.rename = self.rename.take().and_then(|edit| {
            self.activities
                .iter()
                .position(|a| a.id == edit.id)
                .map(|index| RenameEdit {
                    id: edit.id,
                    index,
                    field: edit.field,
                })
        });
        self.submitted_activity_name = None;

        self.content_focus = self
            .content_focus
            .min(self.active_count().saturating_sub(1));
        self.row_action = 0;
        self.apply_focus_marks();
    }

    /// Build a task's row + primary action button + Group button.
    fn build_task(task: TaskSummary) -> TaskEntry {
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

    /// Build a pressure cause's card, with one footer button per relief
    /// action.
    fn build_pressure(cause: PressureCause) -> PressureEntry {
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

    /// Build an activity's header row + Switch/Pause-or-Resume/Rename/Close
    /// buttons, and one row per member.
    fn build_activity(summary: ActivitySummary) -> ActivityEntry {
        let header = Self::build_activity_header(&summary.name, &summary.detail, summary.activity);
        let switch = Button::new(
            ButtonContent::Label(String::from("Switch")),
            ControlRole::Primary,
        );
        let gated = if summary.can_control {
            ActionVerdict::Ready
        } else {
            ActionVerdict::DeniedByAuthority
        };
        let mut pause_resume = Button::labelled(if summary.paused { "Resume" } else { "Pause" });
        pause_resume.set_state(gated.to_state());
        let rename = Button::labelled("Rename");
        let mut close = Button::new(
            ButtonContent::Label(String::from("Close")),
            ControlRole::Destructive,
        );
        close.set_state(if summary.can_control {
            ControlState::idle().with_authority(AuthorityState::NeedsConfirmation)
        } else {
            ActionVerdict::DeniedByAuthority.to_state()
        });
        let members = summary
            .members
            .into_iter()
            .map(|member| {
                ListRow::new(member.name)
                    .with_trailing(member.detail)
                    .with_state(ControlState::idle().with_activity(member.activity))
            })
            .collect();
        ActivityEntry {
            id: summary.id,
            name: summary.name,
            detail: summary.detail,
            activity: summary.activity,
            header,
            switch,
            pause_resume,
            rename,
            close,
            paused: summary.paused,
            can_control: summary.can_control,
            can_accept_member: summary.can_accept_member,
            members,
        }
    }

    /// Build (or rebuild, after a rename commit) an activity header row from
    /// its name, trailing detail, and live activity — the one place that
    /// composes a header [`ListRow`], so a rename can never drift from how
    /// [`build_activity`](Self::build_activity) first built it.
    fn build_activity_header(name: &str, detail: &str, activity: ActivityState) -> ListRow {
        ListRow::new(name)
            .with_trailing(detail)
            .with_state(ControlState::idle().with_activity(activity))
    }

    /// Build a background-job card with its footer actions.
    fn build_job(job: JobSummary) -> Card {
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

    /// Build a recovery object's row + Restart/Force buttons.
    fn build_recovery(item: RecoveryItem) -> RecoveryEntry {
        let row = ListRow::new(item.name)
            .with_trailing(item.detail)
            .with_role(ControlRole::Recovery)
            .with_state(ControlState::idle().with_recovery(item.recovery));
        let mut restart = Button::new(
            ButtonContent::Label(String::from("Restart")),
            ControlRole::Recovery,
        );
        restart.set_state(action_state(item.can_restart));
        let mut force = Button::new(
            ButtonContent::Label(String::from("Force")),
            ControlRole::Destructive,
        );
        // A permitted force action carries a deliberate confirmation posture; a
        // refused one shows the Authority Mark and fails closed.
        force.set_state(if item.can_force {
            ControlState::idle().with_authority(AuthorityState::NeedsConfirmation)
        } else {
            action_state(false)
        });
        RecoveryEntry {
            row,
            restart,
            force,
        }
    }

    /// Build an Overview resource card and the header band's meter for the
    /// same resource, from the one summary.
    fn build_resource(res: ResourceSummary) -> ResourceEntry {
        let card = Card::new(res.name.clone())
            .with_body(res.reading.clone())
            .with_state(
                ControlState::idle()
                    .with_pressure(PressureState::Under(res.kind))
                    .with_activity(res.activity),
            );
        let meter = Meter::new(res.name, res.reading, res.kind, res.meter)
            .with_pressure(res.meter_pressure)
            .with_samples(res.history[..res.history_len].iter().copied());
        ResourceEntry { card, meter }
    }

    /// Build an Overview service row + action button.
    fn build_service(svc: ServiceSummary) -> ServiceEntry {
        let row = ListRow::new(svc.name)
            .with_trailing(svc.detail)
            .with_state(ControlState::idle().with_recovery(svc.recovery));
        let mut action = Button::labelled(svc.action);
        action.set_state(action_state(svc.action_allowed));
        ServiceEntry { row, action }
    }

    /// Build a system-action header button.
    fn build_system_button(action: SystemAction) -> Button {
        let mut button = Button::new(ButtonContent::Label(action.label), action.role);
        button.set_state(action_state(action.allowed));
        button
    }

    /// The currently selected section.
    #[must_use]
    pub fn section(&self) -> Section {
        self.section
    }

    /// The active section's current scroll offset (first-visible item index).
    #[must_use]
    pub fn scroll_offset(&self) -> u64 {
        self.offsets[self.section.index()]
    }

    /// The name committed by the most recent inline activity rename.
    ///
    /// `Some` from the [`SwitchboardAction::ActivityRenamed`] emission until
    /// the next [`set_model`](Switchboard::set_model), mirroring how a host
    /// reads a committed [`TextField::text`] before refreshing its model.
    #[must_use]
    pub fn submitted_activity_name(&self) -> Option<&str> {
        self.submitted_activity_name.as_deref()
    }

    /// Show `section`, as if its tab had been pressed, and report the change.
    ///
    /// This is how a host opens Switchboard already showing the section the
    /// user asked for — Recovery for a long-press on a flagged tray capsule,
    /// Tasks for an ordinary press — instead of steering the selection with
    /// synthetic input. Call it after [`new`](Switchboard::new) and before the
    /// first [`render`](Switchboard::render), or at any later point.
    ///
    /// The selected section is the composition's own live state, not the
    /// caller's: the tab strip's selected index, the keyboard focus position,
    /// and the per-section scroll offsets all hang off it and move with every
    /// tab press. So it lives here and not on [`SwitchboardModel`], which is
    /// the data the caller hands in once and [`new`](Switchboard::new)
    /// consumes; a section field there would be a second owner of the same
    /// fact, stale from the first user interaction. Read it back with
    /// [`section`](Switchboard::section).
    ///
    /// This runs the one transition the tab strip and the keyboard run, so all
    /// three agree by construction: afterwards the strip marks the new tab, the
    /// content area draws that section, and [`scroll_offset`] reports the new
    /// section's own offset, re-ranged and re-clamped against its content by
    /// the next [`render`](Switchboard::render) or
    /// [`on_pointer`](Switchboard::on_pointer).
    ///
    /// Selecting the section already shown changes nothing — no scroll reset,
    /// no focus reset — and returns `None`. [`Section`] is a closed enum, so
    /// there is no invalid section to reject and no error to report; the only
    /// outcomes are "changed" and "already there".
    ///
    /// [`scroll_offset`]: Switchboard::scroll_offset
    pub fn select_section(&mut self, section: Section) -> Option<SwitchboardAction> {
        self.select_section_index(section.index())
    }

    /// The physical height of one list-row item (a control plus a gap).
    fn row_item_height(scale: Scale, theme: &Theme) -> u32 {
        let m = theme.metrics();
        (scale.scale_length(m.control_height).max(1))
            .saturating_add(scale.scale_length(m.control_gap))
    }

    /// The physical height of one job/resource card item.
    fn card_item_height(scale: Scale, theme: &Theme) -> u32 {
        let m = theme.metrics();
        scale
            .scale_length(m.control_height)
            .saturating_mul(3)
            .saturating_add(scale.scale_length(m.control_gap).saturating_mul(2))
    }

    /// The width reserved for one inline row action button.
    fn action_width(scale: Scale, theme: &Theme) -> u32 {
        scale
            .scale_length(theme.metrics().control_height.saturating_mul(4))
            .max(1)
    }

    /// The header resource band's measured height: zero when there is
    /// nothing to show — an empty resource list means no band at all —
    /// otherwise the one row height every meter in the band shares
    /// ([`Meter::measured_height`]).
    fn band_height(scale: Scale, theme: &Theme, font: BitmapFont, has_resources: bool) -> u32 {
        if has_resources {
            Meter::measured_height(scale, theme, font)
        } else {
            0
        }
    }

    /// The rectangle for the meter at `index` of `count`, evenly spaced
    /// across `band`'s width with the theme's control gap between neighbours.
    fn band_meter_rect(
        band: Rect,
        index: usize,
        count: usize,
        scale: Scale,
        theme: &Theme,
    ) -> Rect {
        let gap = scale.scale_length(theme.metrics().control_gap).max(1);
        let count = u32::try_from(count).unwrap_or(1).max(1);
        let total_gap = gap.saturating_mul(count.saturating_sub(1));
        let each_w = band
            .width
            .saturating_sub(total_gap)
            .checked_div(count)
            .unwrap_or(0);
        let idx = u32::try_from(index).unwrap_or(0);
        let left = band.left() + to_i32(idx.saturating_mul(each_w.saturating_add(gap)));
        Rect::new(left, band.top(), each_w, band.height)
    }
}

/// The laid-out regions of a Switchboard for one outer bounds.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
struct SbLayout {
    /// The window frame's laid-out rectangles.
    frame: FrameLayout,
    /// The always-visible header resource band, above the tab strip. Zero
    /// height when the model has no resources.
    band: Rect,
    /// The tab strip along the top of the client, below the band.
    tabs: Rect,
    /// The section content area (excludes the scrollbar gutter).
    content: Rect,
    /// The vertical scrollbar track (kept clear of the corner below).
    scroll: Rect,
    /// The scrollbar junction / resize corner.
    corner: Rect,
}

/// The scrollable list of the active section: where it draws, one item's
/// height, and how many items it holds.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
struct ListInfo {
    /// The rectangle the item list occupies.
    list_rect: Rect,
    /// The physical height of one item.
    item_h: u32,
    /// The number of items in the list.
    count: usize,
}

impl ListInfo {
    /// How many whole items fit in the list rectangle.
    fn visible(self) -> u32 {
        self.list_rect.height.checked_div(self.item_h).unwrap_or(0)
    }

    /// The surface rectangle of the item at visible `slot`.
    fn item_rect(self, slot: u32) -> Rect {
        Rect::new(
            self.list_rect.left(),
            self.list_rect.top() + to_i32(slot.saturating_mul(self.item_h)),
            self.list_rect.width,
            self.item_h,
        )
    }
}

impl Switchboard {
    /// Lay the composition out within `bounds` for the active theme.
    ///
    /// The header resource band claims its measured height (zero with no
    /// resources) immediately below the title bar, and the tab strip and
    /// every region below it shift down by exactly that height, clipped so a
    /// window too short for the full anatomy still lays out in bounds
    /// (fail closed, never negative or overlapping).
    fn compute_layout(
        &self,
        bounds: Rect,
        scale: Scale,
        theme: &Theme,
        font: BitmapFont,
    ) -> SbLayout {
        let frame = self.frame.layout(bounds, scale, theme);
        let client = frame.client;

        let band_h =
            Self::band_height(scale, theme, font, !self.resources.is_empty()).min(client.height);
        let band = Rect::new(client.left(), client.top(), client.width, band_h);

        let below_band_top = client.top() + to_i32(band_h);
        let below_band_h = client.height.saturating_sub(band_h);
        let tab_h = scale
            .scale_length(theme.metrics().control_height)
            .max(1)
            .min(below_band_h);
        let tabs = Rect::new(client.left(), below_band_top, client.width, tab_h);

        let below_top = below_band_top + to_i32(tab_h);
        let below_h = below_band_h.saturating_sub(tab_h);
        let gutter = scale
            .scale_length(theme.metrics().scrollbar_breadth)
            .max(1)
            .min(client.width);
        let content_w = client.width.saturating_sub(gutter);
        let content = Rect::new(client.left(), below_top, content_w, below_h);

        let scroll_h = below_h.saturating_sub(gutter);
        let gutter_left = client.left() + to_i32(content_w);
        let scroll = Rect::new(gutter_left, below_top, gutter, scroll_h);
        let corner = Rect::new(gutter_left, below_top + to_i32(scroll_h), gutter, gutter);

        SbLayout {
            frame,
            band,
            tabs,
            content,
            scroll,
            corner,
        }
    }

    /// The scrollable list metrics for the active section.
    fn list_info(&self, layout: &SbLayout, scale: Scale, theme: &Theme) -> ListInfo {
        let content = layout.content;
        match self.section {
            Section::Tasks => ListInfo {
                list_rect: content,
                item_h: Self::row_item_height(scale, theme),
                count: self.tasks.len(),
            },
            Section::Jobs => ListInfo {
                list_rect: content,
                item_h: Self::card_item_height(scale, theme),
                count: self.jobs.len(),
            },
            Section::Pressure => ListInfo {
                list_rect: content,
                item_h: Self::card_item_height(scale, theme),
                count: self.pressure.len(),
            },
            Section::Activities => ListInfo {
                list_rect: content,
                item_h: Self::row_item_height(scale, theme),
                count: self.total_activity_rows(),
            },
            Section::Recovery => ListInfo {
                list_rect: content,
                item_h: Self::row_item_height(scale, theme),
                count: self.recovery.len(),
            },
            Section::Overview => {
                let pc = self
                    .panel
                    .content_rect(content, scale, theme)
                    .unwrap_or(Rect::EMPTY);
                let card_h = Self::card_item_height(scale, theme);
                let block = u32::try_from(self.resources.len())
                    .unwrap_or(0)
                    .saturating_mul(card_h)
                    .min(pc.height);
                let list_rect = Rect::new(
                    pc.left(),
                    pc.top() + to_i32(block),
                    pc.width,
                    pc.height.saturating_sub(block),
                );
                ListInfo {
                    list_rect,
                    item_h: Self::row_item_height(scale, theme),
                    count: self.services.len(),
                }
            }
        }
    }

    /// Split a list-item rectangle into the row content rect and `buttons`
    /// inline action-button rects (laid from the trailing edge), so the row
    /// text and its actions never overlap.
    fn split_row(item: Rect, buttons: u32, scale: Scale, theme: &Theme) -> (Rect, Vec<Rect>) {
        let m = theme.metrics();
        let inset = scale.scale_length(m.control_inset).max(1);
        let gap = scale.scale_length(m.control_gap).max(1);
        let ctrl_h = scale.scale_length(m.control_height).max(1).min(item.height);
        let aw = Self::action_width(scale, theme);
        let total = if buttons == 0 {
            0
        } else {
            aw.saturating_mul(buttons)
                .saturating_add(gap.saturating_mul(buttons.saturating_sub(1)))
        };
        let right = item.right() - to_i32(inset);
        let by = item.top() + (to_i32(item.height) - to_i32(ctrl_h)).max(0) / 2;
        let mut rects = Vec::new();
        let mut bx = right - to_i32(total);
        for _ in 0..buttons {
            rects.push(Rect::new(bx, by, aw, ctrl_h));
            bx = bx.saturating_add(to_i32(aw)).saturating_add(to_i32(gap));
        }
        let row_right = if buttons == 0 {
            right
        } else {
            right - to_i32(total) - to_i32(gap)
        };
        let row_w = u32::try_from((row_right - item.left()).max(0)).unwrap_or(0);
        let row_rect = Rect::new(item.left(), item.top(), row_w, ctrl_h);
        (row_rect, rects)
    }

    /// The number of items in the active section's scrollable list.
    fn active_count(&self) -> usize {
        match self.section {
            Section::Tasks => self.tasks.len(),
            Section::Jobs => self.jobs.len(),
            Section::Pressure => self.pressure.len(),
            Section::Activities => self.total_activity_rows(),
            Section::Recovery => self.recovery.len(),
            Section::Overview => self.services.len(),
        }
    }

    /// The flattened row count of the Activities section: one header row per
    /// activity plus one row per member.
    fn total_activity_rows(&self) -> usize {
        self.activities.iter().map(|a| 1 + a.members.len()).sum()
    }

    /// The activity row a flattened Activities-section index names — its
    /// owning activity's header, or one of its members — or `None` past the
    /// end of the flattened list.
    fn activity_row_at(&self, index: usize) -> Option<ActivityRow> {
        let mut remaining = index;
        for (ai, entry) in self.activities.iter().enumerate() {
            if remaining == 0 {
                return Some(ActivityRow::Header(ai));
            }
            remaining -= 1;
            if remaining < entry.members.len() {
                return Some(ActivityRow::Member(ai, remaining));
            }
            remaining -= entry.members.len();
        }
        None
    }

    /// Re-range the scrollbar over `count` items in a `viewport` of whole
    /// visible items, keeping the active section's stored offset and writing
    /// back whatever the range clamped it to.
    ///
    /// This is the one place an offset is clamped: a section switch, a resize,
    /// and a model refresh all re-range through here, so a list that shrank can
    /// never leave the offset past its end.
    fn set_scroll_range(&mut self, count: usize, viewport: u64) {
        let content = u64::try_from(count).unwrap_or(u64::MAX);
        let range = ScrollRange::new(content, viewport, self.offsets[self.section.index()]);
        self.scroll
            .set_model(ScrollModel::new(range, 1, viewport.max(1)));
        self.offsets[self.section.index()] = self.scroll.model().offset();
    }

    /// Rebuild the scrollbar's model from the active section's list metrics and
    /// the stored per-section offset, and persist the (re-clamped) offset.
    ///
    /// The scroll unit is items: the range's content extent is the item count
    /// and its viewport extent is the number of whole items that fit, so a
    /// range change (a section switch or a resize) re-clamps the offset rather
    /// than leaving it out of bounds.
    fn sync_scroll(&mut self, bounds: Rect, scale: Scale, theme: &Theme, font: BitmapFont) {
        let layout = self.compute_layout(bounds, scale, theme, font);
        let info = self.list_info(&layout, scale, theme);
        self.set_scroll_range(info.count, u64::from(info.visible()));
    }

    /// The Group popup's anchor rectangle: the Tasks row `task`'s `Group`
    /// button, re-derived from the current layout and scroll offset every
    /// time, so it can never go stale across a resize or a scroll.
    ///
    /// A `task` scrolled out of view (the popup stays open while the list
    /// keeps scrolling) has no rectangle to anchor on; the content area's own
    /// rectangle is used instead so the popup still lands somewhere inside
    /// the window (fail closed, never a panic).
    fn group_anchor_rect(
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
                    let (_, buttons) = Self::split_row(info.item_rect(slot), 2, scale, theme);
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
    fn popup_rect(
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
}

// --- Rendering -------------------------------------------------------------

impl Switchboard {
    /// Paint the whole Switchboard into `surface` at `bounds` for the active
    /// theme. Must be called each frame: it re-syncs the scroll model to the
    /// current layout before drawing.
    pub fn render(
        &mut self,
        surface: &mut Surface,
        bounds: Rect,
        scale: Scale,
        theme: &Theme,
        font: BitmapFont,
    ) {
        self.sync_scroll(bounds, scale, theme, font);
        let layout = self.compute_layout(bounds, scale, theme, font);

        self.frame.render(surface, bounds, scale, theme, font);
        self.render_band(surface, layout.band, scale, theme, font);
        self.tabs.render(surface, layout.tabs, scale, theme, font);
        self.render_section(surface, &layout, scale, theme, font);

        // The scrollbar and its junction/resize corner, drawn last so they sit
        // above the content and the corner never overlaps the thumb.
        self.scroll.render(surface, layout.scroll, scale, theme);
        self.corner.render(surface, layout.corner, scale, theme);
        self.grabber.render(surface, layout.corner, scale, theme);

        // The Group popup, painted last of all so it sits above every other
        // region, including the scrollbar and grabber.
        if let Some(popup) = &self.group_popup {
            let anchor = self.group_anchor_rect(popup.task, &layout, scale, theme);
            let rect = Self::popup_rect(&popup.menu, anchor, bounds, scale, theme, font);
            popup.menu.render(surface, rect, scale, theme, font);
        }
    }

    /// Paint the always-visible header resource band: every resource's
    /// meter, evenly spaced across the band's width through
    /// [`Switchboard::band_meter_rect`]. A zero-height `band` (no resources)
    /// draws nothing.
    fn render_band(
        &self,
        surface: &mut Surface,
        band: Rect,
        scale: Scale,
        theme: &Theme,
        font: BitmapFont,
    ) {
        let count = self.resources.len();
        for (i, entry) in self.resources.iter().enumerate() {
            let rect = Self::band_meter_rect(band, i, count, scale, theme);
            entry.meter.render(surface, rect, scale, theme, font);
        }
    }

    /// Paint the active section's content.
    fn render_section(
        &self,
        surface: &mut Surface,
        layout: &SbLayout,
        scale: Scale,
        theme: &Theme,
        font: BitmapFont,
    ) {
        let info = self.list_info(layout, scale, theme);
        match self.section {
            Section::Tasks => self.render_task_rows(surface, info, scale, theme, font),
            Section::Jobs => self.render_job_cards(surface, info, scale, theme, font),
            Section::Pressure => self.render_pressure_cards(surface, info, scale, theme, font),
            Section::Activities => self.render_activity_rows(surface, info, scale, theme, font),
            Section::Recovery => self.render_recovery_rows(surface, info, scale, theme, font),
            Section::Overview => self.render_overview(surface, layout, info, scale, theme, font),
        }
    }

    /// Render the visible task rows and their primary action + Group buttons.
    fn render_task_rows(
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
            let (row_rect, buttons) = Self::split_row(info.item_rect(slot), 2, scale, theme);
            entry.row.render(surface, row_rect, scale, theme, font);
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

    /// Render the visible pressure cards.
    fn render_pressure_cards(
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

    /// Render the visible activity rows: a header row (with its Switch/Pause-
    /// or-Resume/Rename/Close buttons, or an in-flight rename field in place
    /// of the header) followed by its indented member rows.
    fn render_activity_rows(
        &self,
        surface: &mut Surface,
        info: ListInfo,
        scale: Scale,
        theme: &Theme,
        font: BitmapFont,
    ) {
        let indent = scale.scale_length(theme.metrics().control_height);
        let start = usize::try_from(self.offsets[self.section.index()]).unwrap_or(0);
        for slot in 0..info.visible() {
            let Some(row) = self.activity_row_at(start + slot as usize) else {
                break;
            };
            let item = info.item_rect(slot);
            match row {
                ActivityRow::Header(ai) => {
                    let Some(entry) = self.activities.get(ai) else {
                        continue;
                    };
                    let (row_rect, buttons) = Self::split_row(item, 4, scale, theme);
                    if let Some(edit) = self.rename.as_ref().filter(|e| e.index == ai) {
                        edit.field.render(surface, row_rect, scale, theme, font);
                    } else {
                        entry.header.render(surface, row_rect, scale, theme, font);
                    }
                    if let Some(rect) = buttons.first() {
                        entry.switch.render(surface, *rect, scale, theme, font);
                    }
                    if let Some(rect) = buttons.get(1) {
                        entry
                            .pause_resume
                            .render(surface, *rect, scale, theme, font);
                    }
                    if let Some(rect) = buttons.get(2) {
                        entry.rename.render(surface, *rect, scale, theme, font);
                    }
                    if let Some(rect) = buttons.get(3) {
                        entry.close.render(surface, *rect, scale, theme, font);
                    }
                }
                ActivityRow::Member(ai, mi) => {
                    let Some(member) = self
                        .activities
                        .get(ai)
                        .and_then(|entry| entry.members.get(mi))
                    else {
                        continue;
                    };
                    let indented = Rect::new(
                        item.left() + to_i32(indent),
                        item.top(),
                        item.width.saturating_sub(indent),
                        item.height,
                    );
                    let (row_rect, _) = Self::split_row(indented, 0, scale, theme);
                    member.render(surface, row_rect, scale, theme, font);
                }
            }
        }
    }

    /// Render the visible job cards.
    fn render_job_cards(
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

    /// Render the visible recovery rows and their Restart/Force buttons.
    fn render_recovery_rows(
        &self,
        surface: &mut Surface,
        info: ListInfo,
        scale: Scale,
        theme: &Theme,
        font: BitmapFont,
    ) {
        let start = usize::try_from(self.offsets[self.section.index()]).unwrap_or(0);
        for slot in 0..info.visible() {
            let Some(entry) = self.recovery.get(start + slot as usize) else {
                break;
            };
            let (row_rect, buttons) = Self::split_row(info.item_rect(slot), 2, scale, theme);
            entry.row.render(surface, row_rect, scale, theme, font);
            if let Some(rect) = buttons.first() {
                entry.restart.render(surface, *rect, scale, theme, font);
            }
            if let Some(rect) = buttons.get(1) {
                entry.force.render(surface, *rect, scale, theme, font);
            }
        }
    }

    /// Render the Overview panel: the panel chrome + system-action header, the
    /// fixed resource-card block, and the scrollable service rows below it.
    fn render_overview(
        &self,
        surface: &mut Surface,
        layout: &SbLayout,
        info: ListInfo,
        scale: Scale,
        theme: &Theme,
        font: BitmapFont,
    ) {
        self.panel
            .render(surface, layout.content, scale, theme, font);
        let Some(pc) = self.panel.content_rect(layout.content, scale, theme) else {
            return;
        };
        let gap = scale.scale_length(theme.metrics().control_gap);
        let card_h = Self::card_item_height(scale, theme);
        for (i, entry) in self.resources.iter().enumerate() {
            let top = pc.top() + to_i32(u32::try_from(i).unwrap_or(0).saturating_mul(card_h));
            if top + to_i32(card_h) > pc.bottom() {
                break;
            }
            let rect = Rect::new(pc.left(), top, pc.width, card_h.saturating_sub(gap));
            entry.card.render(surface, rect, scale, theme, font);
        }

        let start = usize::try_from(self.offsets[self.section.index()]).unwrap_or(0);
        for slot in 0..info.visible() {
            let Some(entry) = self.services.get(start + slot as usize) else {
                break;
            };
            let (row_rect, buttons) = Self::split_row(info.item_rect(slot), 1, scale, theme);
            entry.row.render(surface, row_rect, scale, theme, font);
            if let Some(rect) = buttons.first() {
                entry.action.render(surface, *rect, scale, theme, font);
            }
        }
    }
}

// --- Input -----------------------------------------------------------------

impl Switchboard {
    /// Classify a point against the window frame's furniture hit map, so a
    /// host can prove the client viewport and the frame furniture stay
    /// strictly separate (the client can never receive furniture input).
    #[must_use]
    pub fn furniture_at(
        &self,
        bounds: Rect,
        scale: Scale,
        theme: &Theme,
        point: Point,
    ) -> FurniturePart {
        self.frame.hit(bounds, scale, theme, point)
    }

    /// Feed one pointer or scroll event, returning the typed action it
    /// produced (if any). Must be preceded by a [`render`](Switchboard::render)
    /// so the scroll model matches the current layout.
    pub fn on_pointer(
        &mut self,
        event: &InputEvent,
        bounds: Rect,
        scale: Scale,
        theme: &Theme,
        font: BitmapFont,
    ) -> Option<SwitchboardAction> {
        if let InputEvent::PointerMoved { to } = event {
            self.pointer = *to;
        }

        // The Group popup is modal over the rest of the composition: every
        // event routes to it first, and a primary press outside its bounds
        // dismisses it rather than falling through to whatever sits beneath.
        if self.group_popup.is_some() {
            return self.group_popup_on_pointer(event, bounds, scale, theme, font);
        }

        self.sync_scroll(bounds, scale, theme, font);
        let layout = self.compute_layout(bounds, scale, theme, font);

        // The header resource band is an instrument, not a control: it takes
        // no pointer input, so a press over it must fall through to nothing
        // rather than reaching the tab strip, the content, or the scrollbar
        // it happens to sit above (no fabricated SwitchboardAction).
        if layout.band.contains(self.pointer) {
            return None;
        }

        // The mouse wheel scrolls the active section (spec §17 / no deferral).
        if let InputEvent::PointerScrolled { dx, dy } = event {
            if let Some(ScrollAction::ScrollTo { offset }) = self.scroll.wheel(*dx, *dy) {
                self.offsets[self.section.index()] = offset;
                return Some(SwitchboardAction::Scrolled { offset });
            }
            return None;
        }

        // Title bar (window commands + cooperative move). Fed first so a move
        // drag that leaves the bar still continues.
        if let Some(event) =
            self.frame
                .title_bar_mut()
                .on_pointer(event, layout.frame.title_bar, scale, theme)
        {
            return Some(translate_title(event));
        }

        // The resize grabber at the scrollbar junction.
        if let Some(event) = self.grabber.on_pointer(event, layout.corner) {
            return Some(translate_resize(event));
        }

        // The scrollbar.
        if let Some(ScrollAction::ScrollTo { offset }) =
            self.scroll.on_pointer(event, layout.scroll, scale, theme)
        {
            self.offsets[self.section.index()] = offset;
            return Some(SwitchboardAction::Scrolled { offset });
        }

        // The tab strip.
        if let Some(TabsAction::Selected { index }) = self.tabs.on_pointer(event, layout.tabs) {
            return self.select_section_index(index);
        }

        // The active section's content.
        self.section_on_pointer(event, &layout, scale, theme)
    }

    /// Route a pointer event to the active section's items.
    fn section_on_pointer(
        &mut self,
        event: &InputEvent,
        layout: &SbLayout,
        scale: Scale,
        theme: &Theme,
    ) -> Option<SwitchboardAction> {
        let info = self.list_info(layout, scale, theme);
        let start = usize::try_from(self.offsets[self.section.index()]).unwrap_or(0);
        match self.section {
            Section::Tasks => self.tasks_on_pointer(event, info, start, scale, theme),
            Section::Jobs => self.jobs_on_pointer(event, info, start, scale, theme),
            Section::Pressure => self.pressure_on_pointer(event, info, start, scale, theme),
            Section::Activities => self.activities_on_pointer(event, info, start, scale, theme),
            Section::Recovery => self.recovery_on_pointer(event, info, start, scale, theme),
            Section::Overview => self.overview_on_pointer(event, layout, info, start, scale, theme),
        }
    }

    /// Route a pointer event to the task rows (their primary action and Group
    /// buttons, and row selection).
    fn tasks_on_pointer(
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
            let (row_rect, buttons) = Self::split_row(info.item_rect(slot), 2, scale, theme);
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

    /// Route a pointer event to the pressure cards' footer relief actions.
    fn pressure_on_pointer(
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

    /// Route a pointer event to the Activities section: header rows (their
    /// Switch/Pause-or-Resume/Rename/Close buttons, or an in-flight rename
    /// field) and member rows (selection only).
    fn activities_on_pointer(
        &mut self,
        event: &InputEvent,
        info: ListInfo,
        start: usize,
        scale: Scale,
        theme: &Theme,
    ) -> Option<SwitchboardAction> {
        let indent = scale.scale_length(theme.metrics().control_height);
        for slot in 0..info.visible() {
            let Some(row) = self.activity_row_at(start + slot as usize) else {
                break;
            };
            let item = info.item_rect(slot);
            match row {
                ActivityRow::Header(ai) => {
                    let (_, buttons) = Self::split_row(item, 4, scale, theme);
                    let Some(entry) = self.activities.get_mut(ai) else {
                        continue;
                    };
                    if buttons.first().is_some_and(|rect| {
                        entry.switch.on_pointer(event, *rect) == Some(ButtonAction::Activated)
                    }) {
                        return Some(SwitchboardAction::Activity {
                            index: ai,
                            control: ActivityControl::Switch,
                        });
                    }
                    if buttons.get(1).is_some_and(|rect| {
                        entry.pause_resume.on_pointer(event, *rect) == Some(ButtonAction::Activated)
                    }) {
                        let control = if entry.paused {
                            ActivityControl::Resume
                        } else {
                            ActivityControl::Pause
                        };
                        return Some(SwitchboardAction::Activity { index: ai, control });
                    }
                    if buttons.get(2).is_some_and(|rect| {
                        entry.rename.on_pointer(event, *rect) == Some(ButtonAction::Activated)
                    }) {
                        self.begin_rename(ai);
                        return None;
                    }
                    if buttons.get(3).is_some_and(|rect| {
                        entry.close.on_pointer(event, *rect) == Some(ButtonAction::Activated)
                    }) {
                        return Some(SwitchboardAction::Activity {
                            index: ai,
                            control: ActivityControl::Close,
                        });
                    }
                }
                ActivityRow::Member(ai, mi) => {
                    let indented = Rect::new(
                        item.left() + to_i32(indent),
                        item.top(),
                        item.width.saturating_sub(indent),
                        item.height,
                    );
                    let (row_rect, _) = Self::split_row(indented, 0, scale, theme);
                    let Some(member) = self
                        .activities
                        .get_mut(ai)
                        .and_then(|entry| entry.members.get_mut(mi))
                    else {
                        continue;
                    };
                    if member.on_pointer(event, row_rect) == Some(RowAction::Activated) {
                        if let Some(entry) = self.activities.get_mut(ai) {
                            for (i, row) in entry.members.iter_mut().enumerate() {
                                row.set_selected(i == mi);
                            }
                        }
                    }
                }
            }
        }
        None
    }

    /// Route a pointer event to the job cards' footer actions.
    fn jobs_on_pointer(
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

    /// Route a pointer event to the recovery rows (Restart/Force buttons and
    /// row selection).
    fn recovery_on_pointer(
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
            let (row_rect, buttons) = Self::split_row(info.item_rect(slot), 2, scale, theme);
            let Some(entry) = self.recovery.get_mut(idx) else {
                break;
            };
            if buttons.first().is_some_and(|rect| {
                entry.restart.on_pointer(event, *rect) == Some(ButtonAction::Activated)
            }) {
                return Some(SwitchboardAction::Recovery {
                    index: idx,
                    control: RecoveryControl::Restart,
                });
            }
            if buttons.get(1).is_some_and(|rect| {
                entry.force.on_pointer(event, *rect) == Some(ButtonAction::Activated)
            }) {
                return Some(SwitchboardAction::Recovery {
                    index: idx,
                    control: RecoveryControl::Force,
                });
            }
            if entry.row.on_pointer(event, row_rect) == Some(RowAction::Activated) {
                selected = Some(idx);
            }
        }
        if let Some(idx) = selected {
            for (i, entry) in self.recovery.iter_mut().enumerate() {
                entry.row.set_selected(i == idx);
            }
        }
        None
    }

    /// Route a pointer event to the Overview panel header (system actions) and
    /// its service rows.
    fn overview_on_pointer(
        &mut self,
        event: &InputEvent,
        layout: &SbLayout,
        info: ListInfo,
        start: usize,
        scale: Scale,
        theme: &Theme,
    ) -> Option<SwitchboardAction> {
        if let Some(PanelAction::HeaderActivated { index }) =
            self.panel.on_pointer(event, layout.content, scale, theme)
        {
            return Some(SwitchboardAction::System { index });
        }
        let mut selected = None;
        for slot in 0..info.visible() {
            let idx = start + slot as usize;
            let (row_rect, buttons) = Self::split_row(info.item_rect(slot), 1, scale, theme);
            let Some(entry) = self.services.get_mut(idx) else {
                break;
            };
            if buttons.first().is_some_and(|rect| {
                entry.action.on_pointer(event, *rect) == Some(ButtonAction::Activated)
            }) {
                return Some(SwitchboardAction::Service { index: idx });
            }
            if entry.row.on_pointer(event, row_rect) == Some(RowAction::Activated) {
                selected = Some(idx);
            }
        }
        if let Some(idx) = selected {
            for (i, entry) in self.services.iter_mut().enumerate() {
                entry.row.set_selected(i == idx);
            }
        }
        None
    }

    /// Route a pointer event to the open Group popup: a primary press outside
    /// its bounds dismisses it without emitting; otherwise the event feeds the
    /// popup itself.
    fn group_popup_on_pointer(
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
                .row_at(popup_rect, scale, theme, self.pointer)
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
    fn open_group_popup(&mut self, task: usize) {
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

    /// Map a pressure card's activated footer button to its
    /// [`SwitchboardAction`], failing closed unless the action's verdict is
    /// [`ActionVerdict::Ready`] (the button's own state already refuses
    /// activation, but the verdict is checked again here rather than trusted
    /// implicitly).
    ///
    /// [`PressureControl::ShowTasks`] is resolved internally: it runs the
    /// section transition to [`Section::Tasks`], focuses the cause's task,
    /// and reports that transition's [`SwitchboardAction::SectionChanged`].
    fn resolve_pressure_footer(
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

    /// Begin an inline rename of the activity at `index`, pre-filled with its
    /// current name.
    fn begin_rename(&mut self, index: usize) {
        let Some(entry) = self.activities.get(index) else {
            return;
        };
        let mut field = TextField::new().with_text(&entry.name).with_max_len(48);
        field.set_focused(true);
        self.rename = Some(RenameEdit {
            id: entry.id,
            index,
            field,
        });
    }

    /// Feed one key event, returning the typed action it produced (if any).
    ///
    /// An in-flight rename, then an open Group popup, take every key first:
    /// both are modal editors over the composition, so no key reaches the
    /// regions beneath them until they commit, cancel, or dismiss. Otherwise
    /// Tab cycles keyboard focus between the tab strip, the content list, the
    /// scrollbar, and the title-bar command group; keys are then routed to the
    /// focused region's control.
    pub fn on_key(&mut self, key: Key) -> Option<SwitchboardAction> {
        if self.rename.is_some() {
            return self.rename_on_key(key);
        }
        if self.group_popup.is_some() {
            return self.group_popup_on_key(key);
        }
        if key == Key::Named(NamedKey::Tab) {
            self.focus = self.focus.next();
            self.apply_focus_marks();
            return None;
        }
        match self.focus {
            FocusRegion::TitleBar => self.frame.title_bar_mut().on_key(key).map(translate_title),
            FocusRegion::Tabs => match self.tabs.on_key(key) {
                Some(TabsAction::Selected { index }) => self.select_section_index(index),
                None => None,
            },
            FocusRegion::Scrollbar => match self.scroll.on_key(key) {
                Some(ScrollAction::ScrollTo { offset }) => {
                    self.offsets[self.section.index()] = offset;
                    Some(SwitchboardAction::Scrolled { offset })
                }
                None => None,
            },
            FocusRegion::Content => self.content_on_key(key),
        }
    }

    /// Route a key to the focused content item: Up/Down move the row focus
    /// (resetting the action focus to the row's first button), Left/Right
    /// move the action focus along the row's buttons, and Enter/Space
    /// activate the action-focused button.
    fn content_on_key(&mut self, key: Key) -> Option<SwitchboardAction> {
        let count = self.active_count();
        if count == 0 {
            return None;
        }
        match key {
            Key::Named(NamedKey::Down) => {
                self.content_focus = (self.content_focus + 1).min(count - 1);
                self.row_action = 0;
                self.ensure_focus_visible();
                self.apply_focus_marks();
                None
            }
            Key::Named(NamedKey::Up) => {
                self.content_focus = self.content_focus.saturating_sub(1);
                self.row_action = 0;
                self.ensure_focus_visible();
                self.apply_focus_marks();
                None
            }
            Key::Named(NamedKey::Right) => {
                let last = self.focused_action_count().saturating_sub(1);
                self.row_action = (self.row_action + 1).min(last);
                self.apply_focus_marks();
                None
            }
            Key::Named(NamedKey::Left) => {
                self.row_action = self.row_action.saturating_sub(1);
                self.apply_focus_marks();
                None
            }
            _ => self.activate_focused_item(key),
        }
    }

    /// How many inline action buttons the focused content item carries — the
    /// bound for the Left/Right action focus. Activities member rows are
    /// display-only and carry none.
    fn focused_action_count(&self) -> usize {
        match self.section {
            Section::Tasks | Section::Recovery => 2,
            Section::Jobs => self
                .jobs
                .get(self.content_focus)
                .map_or(0, |card| card.footer().len()),
            Section::Pressure => self
                .pressure
                .get(self.content_focus)
                .map_or(0, |entry| entry.card.footer().len()),
            Section::Activities => match self.activity_row_at(self.content_focus) {
                Some(ActivityRow::Header(_)) => 4,
                Some(ActivityRow::Member(..)) | None => 0,
            },
            Section::Overview => 1,
        }
    }

    /// Feed an activation key to the focused item's action-focused button.
    /// A disabled or denied button refuses the key itself, so a refused
    /// activation emits nothing (fail closed).
    fn activate_focused_item(&mut self, key: Key) -> Option<SwitchboardAction> {
        let idx = self.content_focus;
        match self.section {
            Section::Tasks => {
                let entry = self.tasks.get_mut(idx)?;
                if self.row_action == 0 {
                    return (entry.action.on_key(key) == Some(ButtonAction::Activated))
                        .then_some(SwitchboardAction::Task { index: idx });
                }
                if entry.group_button.on_key(key) == Some(ButtonAction::Activated) {
                    self.open_group_popup(idx);
                }
                None
            }
            Section::Jobs => {
                let card = self.jobs.get_mut(idx)?;
                card.on_key(key)
                    .map(
                        |CardAction::FooterActivated { index }| SwitchboardAction::Job {
                            index: idx,
                            control: job_control(index),
                        },
                    )
            }
            Section::Pressure => {
                let action = self.pressure.get_mut(idx)?.card.on_key(key);
                let CardAction::FooterActivated { index } = action?;
                self.resolve_pressure_footer(idx, index)
            }
            Section::Activities => self.activate_focused_activity(key),
            Section::Recovery => {
                let entry = self.recovery.get_mut(idx)?;
                if self.row_action == 0 {
                    return (entry.restart.on_key(key) == Some(ButtonAction::Activated)).then_some(
                        SwitchboardAction::Recovery {
                            index: idx,
                            control: RecoveryControl::Restart,
                        },
                    );
                }
                (entry.force.on_key(key) == Some(ButtonAction::Activated)).then_some(
                    SwitchboardAction::Recovery {
                        index: idx,
                        control: RecoveryControl::Force,
                    },
                )
            }
            Section::Overview => {
                let entry = self.services.get_mut(idx)?;
                (entry.action.on_key(key) == Some(ButtonAction::Activated))
                    .then_some(SwitchboardAction::Service { index: idx })
            }
        }
    }

    /// Activate the focused Activities row's action-focused header button
    /// (Switch, Pause-or-Resume, Rename, Close, in action-focus order).
    /// Member rows are display-only, so they activate nothing.
    fn activate_focused_activity(&mut self, key: Key) -> Option<SwitchboardAction> {
        let Some(ActivityRow::Header(ai)) = self.activity_row_at(self.content_focus) else {
            return None;
        };
        let entry = self.activities.get_mut(ai)?;
        match self.row_action {
            0 => (entry.switch.on_key(key) == Some(ButtonAction::Activated)).then_some(
                SwitchboardAction::Activity {
                    index: ai,
                    control: ActivityControl::Switch,
                },
            ),
            1 => {
                let control = if entry.paused {
                    ActivityControl::Resume
                } else {
                    ActivityControl::Pause
                };
                (entry.pause_resume.on_key(key) == Some(ButtonAction::Activated))
                    .then_some(SwitchboardAction::Activity { index: ai, control })
            }
            2 => {
                if entry.rename.on_key(key) == Some(ButtonAction::Activated) {
                    self.begin_rename(ai);
                }
                None
            }
            _ => (entry.close.on_key(key) == Some(ButtonAction::Activated)).then_some(
                SwitchboardAction::Activity {
                    index: ai,
                    control: ActivityControl::Close,
                },
            ),
        }
    }

    /// Route a key to the in-flight rename field: Enter commits (rebuilding
    /// the header row and reporting the rename), Escape cancels without
    /// emitting, and everything else edits the field.
    fn rename_on_key(&mut self, key: Key) -> Option<SwitchboardAction> {
        let action = self
            .rename
            .as_mut()?
            .field
            .on_key(key, Modifiers::default());
        match action {
            Some(TextAction::Submitted) => {
                let edit = self.rename.take()?;
                let index = edit.index;
                let entry = self.activities.get_mut(index)?;
                entry.name = String::from(edit.field.text());
                entry.header =
                    Self::build_activity_header(&entry.name, &entry.detail, entry.activity);
                self.submitted_activity_name = Some(entry.name.clone());
                Some(SwitchboardAction::ActivityRenamed { index })
            }
            Some(TextAction::Cancelled) => {
                self.rename = None;
                None
            }
            Some(TextAction::Edited) | None => None,
        }
    }

    /// Route a key to the open Group popup: arrows move its focus, Enter or
    /// Space activates the focused row, and Escape dismisses without
    /// emitting.
    fn group_popup_on_key(&mut self, key: Key) -> Option<SwitchboardAction> {
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

    /// The one section transition: every path that changes the shown section —
    /// a tab press, the keyboard, and [`select_section`](Switchboard::select_section)
    /// — runs this, so the tab strip, the content, and the per-section scroll
    /// offset can never disagree.
    ///
    /// It marks the tab, shows the section, and puts keyboard focus back on its
    /// first item; the offset stays each section's own and is re-clamped
    /// against the new content by the next scroll sync. Re-selecting the shown
    /// section is a no-op, and an out-of-range index changes nothing (fail
    /// closed); both report no change.
    fn select_section_index(&mut self, index: usize) -> Option<SwitchboardAction> {
        let section = Section::from_index(index)?;
        if section == self.section {
            return None;
        }
        self.section = section;
        self.tabs.set_selected(index);
        self.content_focus = 0;
        self.row_action = 0;
        // The Group popup anchors on a row of the section that opened it; a
        // section change invalidates that anchor, so the popup drops rather
        // than floating over unrelated content.
        self.group_popup = None;
        self.apply_focus_marks();
        Some(SwitchboardAction::SectionChanged { section })
    }

    /// Nudge the active section's offset so the focused content item stays
    /// visible, using the last-synced viewport extent.
    fn ensure_focus_visible(&mut self) {
        let viewport = self.scroll.model().range().viewport_extent();
        if viewport == 0 {
            return;
        }
        let idx = u64::try_from(self.content_focus).unwrap_or(0);
        let mut offset = self.offsets[self.section.index()];
        if idx < offset {
            offset = idx;
        } else if idx >= offset + viewport {
            offset = idx + 1 - viewport;
        }
        self.scroll.set_model(self.scroll.model().scroll_to(offset));
        self.offsets[self.section.index()] = self.scroll.model().offset();
    }

    /// Reflect the current focus region on the sub-controls: the focused tab,
    /// the focused scrollbar, and the focused content item's primary action.
    fn apply_focus_marks(&mut self) {
        let sel = self.tabs.selected().unwrap_or(0);
        self.tabs
            .set_current((self.focus == FocusRegion::Tabs).then_some(sel));
        self.scroll
            .set_focused(self.focus == FocusRegion::Scrollbar);

        if self.focus != FocusRegion::TitleBar {
            for kind in [
                WindowControlKind::Close,
                WindowControlKind::Minimize,
                WindowControlKind::PutToBack,
                WindowControlKind::SizeToggle,
            ] {
                self.frame
                    .title_bar_mut()
                    .control_mut(kind)
                    .set_focused(false);
            }
        }

        let content = self.focus == FocusRegion::Content;
        let idx = self.content_focus;
        let action = self.row_action;
        for (i, entry) in self.tasks.iter_mut().enumerate() {
            let focus_here = content && self.section == Section::Tasks && i == idx;
            entry.action.set_focused(focus_here && action == 0);
            entry.group_button.set_focused(focus_here && action == 1);
        }
        for (i, card) in self.jobs.iter_mut().enumerate() {
            let focus_here = content && self.section == Section::Jobs && i == idx;
            for (b, button) in card.footer_mut().iter_mut().enumerate() {
                button.set_focused(focus_here && b == action);
            }
        }
        for (i, entry) in self.pressure.iter_mut().enumerate() {
            let focus_here = content && self.section == Section::Pressure && i == idx;
            for (b, button) in entry.card.footer_mut().iter_mut().enumerate() {
                button.set_focused(focus_here && b == action);
            }
        }
        // Only an Activities header row carries buttons, so the flattened row
        // focus marks a button only when it names a header.
        let focused_header = (content && self.section == Section::Activities)
            .then(|| self.activity_row_at(idx))
            .flatten()
            .and_then(|row| match row {
                ActivityRow::Header(ai) => Some(ai),
                ActivityRow::Member(..) => None,
            });
        for (i, entry) in self.activities.iter_mut().enumerate() {
            let focus_here = focused_header == Some(i);
            entry.switch.set_focused(focus_here && action == 0);
            entry.pause_resume.set_focused(focus_here && action == 1);
            entry.rename.set_focused(focus_here && action == 2);
            entry.close.set_focused(focus_here && action == 3);
        }
        for (i, entry) in self.recovery.iter_mut().enumerate() {
            let focus_here = content && self.section == Section::Recovery && i == idx;
            entry.restart.set_focused(focus_here && action == 0);
            entry.force.set_focused(focus_here && action == 1);
        }
        for (i, entry) in self.services.iter_mut().enumerate() {
            entry
                .action
                .set_focused(content && self.section == Section::Overview && i == idx);
        }
    }
}

/// Translate a title-bar event into a Switchboard action, mapping a command to
/// [`SwitchboardAction::Window`] and a drag to the cooperative-move actions.
fn translate_title(event: TitleBarEvent) -> SwitchboardAction {
    match event {
        TitleBarEvent::Control(kind) => SwitchboardAction::Window(kind),
        TitleBarEvent::Activate => SwitchboardAction::Activate,
        TitleBarEvent::DragBegin => SwitchboardAction::MoveBegin,
        TitleBarEvent::DragMoved { to } => SwitchboardAction::MoveTo { to },
        TitleBarEvent::DragEnd => SwitchboardAction::MoveEnd,
    }
}

/// Translate a resize-grabber event into a Switchboard resize action.
fn translate_resize(event: ResizeEvent) -> SwitchboardAction {
    match event {
        ResizeEvent::Begin => SwitchboardAction::ResizeBegin,
        ResizeEvent::Moved { to } => SwitchboardAction::ResizeTo { to },
        ResizeEvent::End => SwitchboardAction::ResizeEnd,
        ResizeEvent::Cancel => SwitchboardAction::ResizeCancel,
    }
}

/// Map a job card's footer-button index to its typed control (0 = pause,
/// otherwise cancel), matching the footer order [`Switchboard::build_job`] lays
/// down.
fn job_control(index: usize) -> JobControl {
    if index == 0 {
        JobControl::Pause
    } else {
        JobControl::Cancel
    }
}

#[cfg(test)]
#[path = "switchboard_tests.rs"]
mod tests;
