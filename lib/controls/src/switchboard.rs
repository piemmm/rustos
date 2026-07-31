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

use tairix_font::BitmapFont;
use tairix_geometry::{Point, Rect, Scale};
use tairix_input::{InputEvent, Key, NamedKey};
use tairix_raster::Surface;
use tairix_theme::Theme;

use crate::button::{Button, ButtonAction, ButtonContent};
use crate::collection::{Card, CardAction, ListRow, Panel, PanelAction, RowAction};
use crate::meter::{Meter, MeterValue, MAX_HISTORY_SAMPLES};
use crate::paint::{clamp_permille, to_i32};
use crate::scroll::{ScrollModel, ScrollOrientation, ScrollRange};
use crate::scrollbar::{ScrollAction, ScrollBar};
use crate::state::{
    ActivityState, AuthorityState, ControlRole, ControlState, PressureKind, PressureState,
    RecoveryState, WindowActivationState, WindowControlKind, WindowFurnitureState, WindowSizeState,
};
use crate::tabs::{Tab, Tabs, TabsAction};
use crate::window::{
    FrameLayout, FurniturePart, ResizeEvent, ResizeGrabber, ScrollCorner, TitleBarEvent,
    WindowFrame,
};

/// One of Switchboard's four top-level sections (spec §17).
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub enum Section {
    /// Live application/task list.
    Tasks,
    /// Background jobs with known or working progress.
    Jobs,
    /// Hung objects and their recovery actions.
    Recovery,
    /// Resource, service, and system-action overview.
    Overview,
}

impl Section {
    /// The sections in tab order.
    pub const ALL: [Section; 4] = [
        Section::Tasks,
        Section::Jobs,
        Section::Recovery,
        Section::Overview,
    ];

    /// The section's zero-based tab index.
    #[must_use]
    pub const fn index(self) -> usize {
        match self {
            Section::Tasks => 0,
            Section::Jobs => 1,
            Section::Recovery => 2,
            Section::Overview => 3,
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
fn action_state(allowed: bool) -> ControlState {
    if allowed {
        ControlState::idle()
    } else {
        ControlState::idle().with_authority(AuthorityState::NeedsCapability)
    }
}

/// One task rendered as a [`ListRow`] plus its single action [`Button`].
#[derive(Clone, Debug, Eq, PartialEq)]
struct TaskEntry {
    row: ListRow,
    action: Button,
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
    panel: Panel,
    section: Section,
    offsets: [u64; 4],
    focus: FocusRegion,
    content_focus: usize,
    pointer: Point,
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
            panel: Panel::new(SYSTEM_PANEL_TITLE),
            section: Section::Tasks,
            offsets: [0; 4],
            focus: FocusRegion::Content,
            content_focus: 0,
            pointer: Point::ORIGIN,
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

        self.content_focus = self
            .content_focus
            .min(self.active_count().saturating_sub(1));
        self.apply_focus_marks();
    }

    /// Build a task's row + action button.
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
        TaskEntry { row, action }
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
            Section::Recovery => self.recovery.len(),
            Section::Overview => self.services.len(),
        }
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
            Section::Recovery => self.render_recovery_rows(surface, info, scale, theme, font),
            Section::Overview => self.render_overview(surface, layout, info, scale, theme, font),
        }
    }

    /// Render the visible task rows and their action buttons.
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
            let (row_rect, buttons) = Self::split_row(info.item_rect(slot), 1, scale, theme);
            entry.row.render(surface, row_rect, scale, theme, font);
            if let Some(rect) = buttons.first() {
                entry.action.render(surface, *rect, scale, theme, font);
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
            Section::Recovery => self.recovery_on_pointer(event, info, start, scale, theme),
            Section::Overview => self.overview_on_pointer(event, layout, info, start, scale, theme),
        }
    }

    /// Route a pointer event to the task rows (their action buttons and row
    /// selection).
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
            let (row_rect, buttons) = Self::split_row(info.item_rect(slot), 1, scale, theme);
            let Some(entry) = self.tasks.get_mut(idx) else {
                break;
            };
            if buttons.first().is_some_and(|rect| {
                entry.action.on_pointer(event, *rect) == Some(ButtonAction::Activated)
            }) {
                return Some(SwitchboardAction::Task { index: idx });
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

    /// Feed one key event, returning the typed action it produced (if any).
    ///
    /// Tab cycles keyboard focus between the tab strip, the content list, the
    /// scrollbar, and the title-bar command group; keys are then routed to the
    /// focused region's control.
    pub fn on_key(&mut self, key: Key) -> Option<SwitchboardAction> {
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

    /// Route a key to the focused content item (Up/Down move focus, Enter/Space
    /// activate its primary action).
    fn content_on_key(&mut self, key: Key) -> Option<SwitchboardAction> {
        let count = self.active_count();
        if count == 0 {
            return None;
        }
        match key {
            Key::Named(NamedKey::Down) => {
                self.content_focus = (self.content_focus + 1).min(count - 1);
                self.ensure_focus_visible();
                self.apply_focus_marks();
                None
            }
            Key::Named(NamedKey::Up) => {
                self.content_focus = self.content_focus.saturating_sub(1);
                self.ensure_focus_visible();
                self.apply_focus_marks();
                None
            }
            _ => self.activate_focused_item(key),
        }
    }

    /// Feed an activation key to the focused item's primary action button.
    fn activate_focused_item(&mut self, key: Key) -> Option<SwitchboardAction> {
        let idx = self.content_focus;
        match self.section {
            Section::Tasks => {
                let entry = self.tasks.get_mut(idx)?;
                (entry.action.on_key(key) == Some(ButtonAction::Activated))
                    .then_some(SwitchboardAction::Task { index: idx })
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
            Section::Recovery => {
                let entry = self.recovery.get_mut(idx)?;
                (entry.restart.on_key(key) == Some(ButtonAction::Activated)).then_some(
                    SwitchboardAction::Recovery {
                        index: idx,
                        control: RecoveryControl::Restart,
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
        for (i, entry) in self.tasks.iter_mut().enumerate() {
            entry
                .action
                .set_focused(content && self.section == Section::Tasks && i == idx);
        }
        for (i, card) in self.jobs.iter_mut().enumerate() {
            let focus_here = content && self.section == Section::Jobs && i == idx;
            for (b, button) in card.footer_mut().iter_mut().enumerate() {
                button.set_focused(focus_here && b == 0);
            }
        }
        for (i, entry) in self.recovery.iter_mut().enumerate() {
            let focus_here = content && self.section == Section::Recovery && i == idx;
            entry.restart.set_focused(focus_here);
            entry.force.set_focused(false);
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
