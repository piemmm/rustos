//! The Switchboard window's screen (`plans/NEW-SWITCHBOARD.md`).
//!
//! This is the surface behind the always-right-most Switchboard taskbar icon:
//! the live task, background-job, pressure, activity, recovery, and
//! system-overview state this service samples, laid out for a reader. It is
//! assembled **purely from the shared Reactive Alloy controls** (spec §17) —
//! the window-manager
//! [`WindowFrame`]/[`TitleBar`](tairix_controls::TitleBar)/[`ResizeGrabber`]
//! furniture, a header resource band of [`Meter`]s, a [`Breadcrumb`] location
//! band, the collection controls ([`ListRow`](tairix_controls::ListRow),
//! [`Card`], [`Panel`]), action [`Button`](tairix_controls::Button)s, and one
//! shared [`ScrollBar`] — so the application paints no chrome of its own and
//! carries no second copy of any control's behaviour.
//!
//! # Package layout
//!
//! This module is the shared skeleton every section draws into: the
//! [`Switchboard`] retained widget tree, [`SwitchboardModel`], [`Section`],
//! [`SwitchboardAction`], the window frame and chrome, input dispatch, the
//! scroll model, and the per-section layout primitives. Each section owns its
//! own view models and layout in its own sibling module — [`mod@tasks`],
//! [`mod@background`], [`mod@pressure`], [`mod@activities`],
//! [`mod@recovery`], and [`mod@system`] — and every type that was public from
//! this screen before the split is re-exported here unchanged.
//!
//! # What it composes
//!
//! - The outer window is a [`WindowFrame`] with the standard
//!   [`TitleBar`](tairix_controls::TitleBar) and
//!   the four window commands; the only application region is the client
//!   viewport, so the client can never receive furniture input (the frame's
//!   hit map enforces this).
//! - Immediately below the title bar sits an always-visible header resource
//!   band: one column per [`ResourceSummary`] in the model, spaced evenly
//!   across the band's width. Each column is a [`Meter`]'s label and reading
//!   over one instrument — a [`Chart`](tairix_controls::Chart) of the
//!   resource's recent history where
//!   there is one to plot, the meter's own track where there is not, never
//!   both of the same number. They are read-only instruments, not controls —
//!   they take no pointer or keyboard input and never produce a
//!   [`SwitchboardAction`] — so a press over the band can never be mistaken
//!   for a press on the location band, the section content, or the scrollbar
//!   below it. An empty resource list collapses the band to zero height
//!   rather than drawing an empty strip.
//! - Below it sits the **location band**: a [`Breadcrumb`] reading
//!   `Switchboard › <section>` with a section-list [`IconButton`] at its
//!   trailing end. The trail's leading crumb and that command both open the
//!   one [`Menu`] of the six [`Section`]s — the one being shown marked
//!   selected — and choosing a row switches section. The host chooses which
//!   section the panel opens on — Recovery when the user reached for a
//!   flagged capsule, Tasks otherwise — with
//!   [`Switchboard::select_section`], never by feeding synthetic input.
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
use tairix_geometry::{to_i32, Point, Rect, Scale};
use tairix_input::{InputEvent, Key, NamedKey, PointerButton};
use tairix_raster::Surface;
use tairix_theme::Theme;

use tairix_controls::{
    ActionRail, AuthorityState, Breadcrumb, BreadcrumbAction, ButtonAction, Card, CardAction,
    ControlRole, ControlState, Crumb, FrameLayout, FurniturePart, IconButton, Menu, MenuAction,
    MenuItem, Meter, Panel, RenderInvariant, ResizeEvent, ResizeGrabber, ScrollAction, ScrollBar,
    ScrollCorner, ScrollModel, ScrollOrientation, ScrollRange, SelectionState, TitleBarEvent,
    WindowActivationState, WindowControlKind, WindowFrame, WindowFurnitureState, WindowSizeState,
};
use tairix_icon::IconKind;

pub mod activities;
pub mod background;
pub mod pressure;
pub mod recovery;
pub mod system;
pub mod tasks;

pub use activities::{ActivityControl, ActivityMember, ActivitySummary};
pub use background::{JobControl, JobSummary};
pub use pressure::{PressureAction, PressureCause, PressureControl};
pub use recovery::{RecoveryControl, RecoveryItem};
pub use system::{ResourceSummary, ServiceSummary, SystemAction};
pub use tasks::TaskSummary;

use activities::{ActivityEntry, ActivityRow, RenameEdit};
use background::job_control;
use pressure::PressureEntry;
use recovery::RecoveryEntry;
use system::{ResourceEntry, ServiceEntry};
use tasks::{GroupPopup, TaskEntry};

#[cfg(test)]
mod test_support;

#[cfg(test)]
#[path = "mod_tests.rs"]
mod tests;

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

/// The title of the Overview section's system-action panel, named once so the
/// resting composition and every refresh of it cannot drift apart.
const SYSTEM_PANEL_TITLE: &str = "System";

/// The screen's own name: the title bar's application name and the leading
/// crumb of the location trail are the same word, so it is spelled once.
const APP_NAME: &str = "Switchboard";

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

/// Which region of the composition currently holds keyboard focus, cycled by
/// the Tab key so the whole surface is keyboard-navigable.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
enum FocusRegion {
    /// The window-command group in the title bar.
    TitleBar,
    /// The location band: its trail's leading crumb, which opens the section
    /// list.
    Location,
    /// The active section's content list.
    Content,
    /// The vertical scrollbar.
    Scrollbar,
}

impl FocusRegion {
    /// The regions in Tab-cycle order.
    const ORDER: [FocusRegion; 4] = [
        FocusRegion::Location,
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

/// This application's Switchboard screen (spec §17).
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
///
/// # Equality is render equivalence
///
/// Equal `Switchboard`s draw the same pixels for the same bounds, scale,
/// theme, and font, so a host may use `==` as its repaint gate: a composition
/// that compares equal to the one already on screen needs neither a render nor
/// a present. Everything the picture depends on takes part in that comparison
/// — the model-derived rows, cards, and meters, the section, the scroll
/// offsets, hover and press highlights, the focus rings, the open Group popup,
/// and the in-flight rename. The last pointer coordinate does not: it is pure
/// hit-testing input that no render path reads, and a sample that crosses no
/// control would otherwise force a full repaint of an unchanged surface.
///
/// The relation is deliberately conservative in the safe direction only.
/// Unequal compositions *may* still draw identically (a focus index that moves
/// while focus rests elsewhere), which costs one needless repaint; equal ones
/// never differ on screen. The exclusion lives in the type of the excluded
/// field — a crate-internal wrapper that always compares equal — rather than
/// in a hand-written `PartialEq`, so a field added later counts towards
/// equality by default.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Switchboard {
    frame: WindowFrame,
    /// The location trail: the screen's name, then the section on show.
    trail: Breadcrumb,
    /// The location band's trailing command, which opens the same section list
    /// the trail's leading crumb does.
    section_list: IconButton,
    /// The open section list, or `None` while it is closed.
    section_menu: Option<Menu>,
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
    /// The last pointer position, kept so a press can be resolved against the
    /// coordinate the pointer actually reached — hit-testing input, never a
    /// drawn property.
    pointer: RenderInvariant<Point>,
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
            trail: Self::build_trail(Section::Tasks),
            section_list: IconButton::new(IconKind::ListMenu, ControlRole::Navigation),
            section_menu: None,
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
            pointer: RenderInvariant::new(Point::ORIGIN),
            group_popup: None,
            rename: None,
            submitted_activity_name: None,
        };
        switchboard.frame.title_bar_mut().set_app_name(APP_NAME);
        switchboard.adopt(model);
        switchboard
    }

    /// The location trail for `section`: the screen's own name, then the
    /// section being shown.
    ///
    /// The leading crumb is the activatable ancestor — addressing it opens the
    /// section list — and the trailing crumb is the current location, which a
    /// [`Breadcrumb`] never activates, so the reader can never "navigate" to
    /// the section already on show.
    fn build_trail(section: Section) -> Breadcrumb {
        Breadcrumb::new(alloc::vec![
            Crumb::new(APP_NAME),
            Crumb::new(section.title()),
        ])
    }

    /// The section list both routes open: one row per [`Section`], with
    /// `section` marked selected *and* current — the same pair
    /// [`ComboBox`](tairix_controls::ComboBox) marks its own choice with, so
    /// there is one convention for "this is the one you are on" rather than a
    /// second invented here.
    fn build_section_menu(section: Section) -> Menu {
        let mut menu = Menu::new(
            Section::ALL
                .iter()
                .map(|s| {
                    let selection = if *s == section {
                        SelectionState::Selected
                    } else {
                        SelectionState::Unselected
                    };
                    MenuItem::new(s.title())
                        .with_state(ControlState::idle().with_selection(selection))
                })
                .collect::<Vec<_>>(),
        );
        menu.set_current(Some(section.index()));
        menu
    }

    /// Show `model` in place of the one currently drawn, keeping the parts of
    /// the surface the *user* owns.
    ///
    /// A host samples live system state continuously — roughly once a second —
    /// and this is how it publishes each new reading. Rebuilding the whole
    /// composition instead would throw away the user's place in the list every
    /// sample, snapping a scrolled or keyboard-navigated list back to the top.
    ///
    /// **Kept, because the user set it:** the selected [`Section`] and the
    /// location trail naming it, every section's scroll offset, the keyboard
    /// focus region and its position in the list, the last pointer position,
    /// an open section list, and any window move, resize, or scroll-thumb drag
    /// in flight.
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
        // An open section list survives: its rows are the closed `Section` set,
        // so no sample can make it stale, and closing it would snatch a menu
        // out from under the reader mid-gesture.
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
    /// reads a committed [`TextField::text`](tairix_controls::TextField::text)
    /// before refreshing its model.
    #[must_use]
    pub fn submitted_activity_name(&self) -> Option<&str> {
        self.submitted_activity_name.as_deref()
    }

    /// Show `section`, as if it had been chosen from the section list, and
    /// report the change.
    ///
    /// This is how a host opens Switchboard already showing the section the
    /// user asked for — Recovery for a long-press on a flagged tray capsule,
    /// Tasks for an ordinary press — instead of steering the selection with
    /// synthetic input. Call it after [`new`](Switchboard::new) and before the
    /// first [`render`](Switchboard::render), or at any later point.
    ///
    /// The selected section is the composition's own live state, not the
    /// caller's: the location trail's trailing crumb, the keyboard focus
    /// position, and the per-section scroll offsets all hang off it and move
    /// with every choice from the section list. So it lives here and not on
    /// [`SwitchboardModel`], which is the data the caller hands in once and
    /// [`new`](Switchboard::new) consumes; a section field there would be a
    /// second owner of the same fact, stale from the first user interaction.
    /// Read it back with [`section`](Switchboard::section).
    ///
    /// This runs the one transition the pointer and the keyboard run, so all
    /// three agree by construction: afterwards the trail names the new section,
    /// the content area draws that section, and [`scroll_offset`] reports the
    /// new section's own offset, re-ranged and re-clamped against its content
    /// by the next [`render`](Switchboard::render) or
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

    /// The header resource band's measured height: zero when there is nothing
    /// to show — an empty resource list means no band at all — otherwise the
    /// one column height every resource in the band shares.
    ///
    /// A column is the [`Meter`]'s label and reading over *one* instrument
    /// slot: the theme's chart box when any resource has a history to plot,
    /// otherwise just the meter's own track. Only one instrument reports a
    /// given resource, so a column never carries both a track and a graph of
    /// the same number.
    fn band_height(
        scale: Scale,
        theme: &Theme,
        font: BitmapFont,
        resources: &[ResourceEntry],
    ) -> u32 {
        if resources.is_empty() {
            return 0;
        }
        if resources.iter().all(|entry| entry.chart.is_empty()) {
            return Meter::measured_height(scale, theme, font);
        }
        Meter::reading_height(scale, theme, font)
            .saturating_add(scale.scale_length(theme.metrics().chart_height))
    }

    /// Split one band column into the rectangle its [`Meter`] draws in and the
    /// instrument slot beneath the reading.
    ///
    /// The meter is handed the whole column when it owns the slot — it draws
    /// its label, its reading, and its track in the space left over — and only
    /// the text height when a [`tairix_controls::Chart`] owns the slot instead, so the meter's
    /// own track cannot draw under a graph of the same number.
    fn band_column_split(
        column: Rect,
        scale: Scale,
        theme: &Theme,
        font: BitmapFont,
        plotted: bool,
    ) -> (Rect, Option<Rect>) {
        if !plotted {
            return (column, None);
        }
        let reading_h = Meter::reading_height(scale, theme, font).min(column.height);
        let slot_h = column.height.saturating_sub(reading_h);
        let reading = Rect::new(column.left(), column.top(), column.width, reading_h);
        if slot_h == 0 {
            return (reading, None);
        }
        let top = column.top() + to_i32(reading_h);
        (
            reading,
            Some(Rect::new(column.left(), top, column.width, slot_h)),
        )
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
    /// The always-visible header resource band, above the location band. Zero
    /// height when the model has no resources.
    band: Rect,
    /// The location band along the top of the client, below the resource band:
    /// the trail and its trailing section-list command, split by
    /// [`Switchboard::location_split`].
    location: Rect,
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
    /// resources) immediately below the title bar, and the location band and
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

        let band_h = Self::band_height(scale, theme, font, &self.resources).min(client.height);
        let band = Rect::new(client.left(), client.top(), client.width, band_h);

        let below_band_top = client.top() + to_i32(band_h);
        let below_band_h = client.height.saturating_sub(band_h);
        let location_h = scale
            .scale_length(theme.metrics().control_height)
            .max(1)
            .min(below_band_h);
        let location = Rect::new(client.left(), below_band_top, client.width, location_h);

        let below_top = below_band_top + to_i32(location_h);
        let below_h = below_band_h.saturating_sub(location_h);
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
            location,
            content,
            scroll,
            corner,
        }
    }

    /// Split the location band into the rectangle its [`Breadcrumb`] draws in
    /// and the square its trailing section-list command occupies.
    ///
    /// The command is a square at the band's trailing edge — the shape an
    /// [`IconButton`] wants — and the trail takes everything left of it, less
    /// the theme's control gap. Both the paint and the hit test read this one
    /// split, so a press can never land on a control drawn somewhere else.
    fn location_split(location: Rect, theme: &Theme, scale: Scale) -> (Rect, Rect) {
        let side = location.height.min(location.width);
        let gap = scale.scale_length(theme.metrics().control_gap).max(1);
        let command = Rect::new(
            location.right() - to_i32(side),
            location.top(),
            side,
            location.height,
        );
        let trail_w = location
            .width
            .saturating_sub(side)
            .saturating_sub(gap)
            .min(location.width);
        let trail = Rect::new(location.left(), location.top(), trail_w, location.height);
        (trail, command)
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

    /// How many inline action buttons a row of `section` carries.
    ///
    /// The render pass, the hit-test pass, the Group popup's anchor, and the
    /// action column's geometry must all agree on this count or a click will
    /// land on a button the user is not looking at, so it is defined once
    /// here rather than restated as a literal at each site. A section whose
    /// items are cards carries none: a card draws its own footer actions
    /// inside itself, so there is no anchored column beside the list.
    #[must_use]
    const fn row_actions(section: Section) -> u32 {
        match section {
            Section::Tasks | Section::Recovery => 2,
            Section::Activities => 4,
            Section::Overview => 1,
            Section::Jobs | Section::Pressure => 0,
        }
    }

    /// The anchored action column of the active section: the strip the rows'
    /// inline action buttons stand in, spanning the whole visible list.
    ///
    /// Every row lays its actions against the same trailing edge, so the
    /// column is one rectangle rather than a per-row fact; it is derived from
    /// the same [`Switchboard::split_row`] geometry the buttons themselves
    /// are laid out with, so the column cannot drift away from its contents.
    /// [`None`] when the section's items carry no inline actions.
    fn action_column(
        info: ListInfo,
        section: Section,
        scale: Scale,
        theme: &Theme,
    ) -> Option<Rect> {
        let buttons = Self::row_actions(section);
        if buttons == 0 {
            return None;
        }
        let probe = info.item_rect(0);
        let (_, rects) = Self::split_row(probe, buttons, scale, theme);
        let first = rects.first()?;
        let width = u32::try_from((probe.right() - first.left()).max(0)).ok()?;
        Some(Rect::new(
            first.left(),
            info.list_rect.top(),
            width,
            info.list_rect.height,
        ))
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
        self.render_location(surface, layout.location, scale, theme, font);
        self.render_section(surface, &layout, scale, theme, font);

        // The scrollbar and its junction/resize corner, drawn last so they sit
        // above the content and the corner never overlaps the thumb.
        self.scroll.render(surface, layout.scroll, scale, theme);
        self.corner.render(surface, layout.corner, scale, theme);
        self.grabber.render(surface, layout.corner, scale, theme);

        // The popups, painted last of all so they sit above every other
        // region, including the scrollbar and grabber. Only one can be open:
        // each is modal over the whole composition while it is, so no input
        // can reach the control that would open the other.
        if let Some(popup) = &self.group_popup {
            let anchor = self.group_anchor_rect(popup.task, &layout, scale, theme);
            let rect = Self::popup_rect(&popup.menu, anchor, bounds, scale, theme, font);
            popup.menu.render(surface, rect, scale, theme, font);
        }
        if let Some(menu) = &self.section_menu {
            let rect = Self::popup_rect(menu, layout.location, bounds, scale, theme, font);
            menu.render(surface, rect, scale, theme, font);
        }
    }

    /// Paint the location band: the trail naming where the reader is, then the
    /// command that opens the section list, over the one
    /// [`Switchboard::location_split`] the hit test reads.
    fn render_location(
        &self,
        surface: &mut Surface,
        location: Rect,
        scale: Scale,
        theme: &Theme,
        font: BitmapFont,
    ) {
        let (trail, command) = Self::location_split(location, theme, scale);
        self.trail.render(surface, trail, scale, theme, font);
        self.section_list
            .render(surface, command, scale, theme, font, None);
    }

    /// Paint the always-visible header resource band: every resource's meter
    /// reading over its one instrument, evenly spaced across the band's width
    /// through [`Switchboard::band_meter_rect`]. A zero-height `band` (no
    /// resources) draws nothing.
    ///
    /// A resource with no history recorded yet keeps its meter's track, so the
    /// band never shows a graph box with no graph in it.
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
            let column = Self::band_meter_rect(band, i, count, scale, theme);
            let (reading, slot) =
                Self::band_column_split(column, scale, theme, font, !entry.chart.is_empty());
            entry.meter.render(surface, reading, scale, theme, font);
            if let Some(slot) = slot {
                entry.chart.render(surface, slot, scale, theme);
            }
        }
    }

    /// Paint the active section's content, then the Edge Wake on the action
    /// column if the list beside it is displaced.
    ///
    /// The column is anchored — its buttons hold the same screen position at
    /// every offset — so without the wake a user cannot tell from a still
    /// frame whether the column is pinned or simply happens to be where the
    /// rows left it. The wake itself is [`ActionRail`]'s own (see
    /// [`ActionRail::with_edge_wake`]): this composition paints no chrome of
    /// its own, so it renders an itemless rail over the column purely to
    /// carry the wake, and it draws last so a row's own plate cannot paint
    /// over it.
    ///
    /// The buttons themselves stay each row's own retained controls rather
    /// than becoming this rail's items: a rail stacks its items contiguously
    /// from the top of its own bounds and owns them, which can express
    /// neither a scrolled window of a longer list nor the Activities list's
    /// button-less member rows between its header rows
    /// (`plans/NEW-SWITCHBOARD.md` S3).
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
        if let Some(column) = Self::action_column(info, self.section, scale, theme) {
            let scrolled = self.offsets[self.section.index()] != 0;
            ActionRail::new(Vec::new())
                .with_edge_wake(scrolled)
                .render(surface, column, scale, theme, font);
        }
    }

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
            *self.pointer = *to;
        }

        // An open popup is modal over the rest of the composition: every event
        // routes to it first, and a primary press outside its bounds dismisses
        // it rather than falling through to whatever sits beneath.
        if self.group_popup.is_some() {
            return self.group_popup_on_pointer(event, bounds, scale, theme, font);
        }
        if self.section_menu.is_some() {
            return self.section_menu_on_pointer(event, bounds, scale, theme, font);
        }

        self.sync_scroll(bounds, scale, theme, font);
        let layout = self.compute_layout(bounds, scale, theme, font);

        // The header resource band is an instrument, not a control: it takes
        // no pointer input, so a press over it must fall through to nothing
        // rather than reaching the location band, the content, or the scrollbar
        // it happens to sit above (no fabricated SwitchboardAction).
        if layout.band.contains(*self.pointer) {
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

        // The location band: the trail's leading crumb and the trailing
        // section-list command are two ways to the same list.
        let (trail, command) = Self::location_split(layout.location, theme, scale);
        if let Some(BreadcrumbAction::Activate { .. }) =
            self.trail.on_pointer(event, trail, scale, theme, font)
        {
            self.open_section_menu();
            return None;
        }
        if self.section_list.on_pointer(event, command) == Some(ButtonAction::Activated) {
            self.open_section_menu();
            return None;
        }

        // The active section's content.
        self.section_on_pointer(event, &layout, scale, theme)
    }

    /// Open the section list on the section currently shown, so the reader
    /// starts from where they are.
    fn open_section_menu(&mut self) {
        self.section_menu = Some(Self::build_section_menu(self.section));
    }

    /// Route a pointer event to the open section list: a primary press off its
    /// rows closes it, an activated row switches section, and its own dismissal
    /// closes it. The one anchor is the location band both routes opened it
    /// from.
    fn section_menu_on_pointer(
        &mut self,
        event: &InputEvent,
        bounds: Rect,
        scale: Scale,
        theme: &Theme,
        font: BitmapFont,
    ) -> Option<SwitchboardAction> {
        let layout = self.compute_layout(bounds, scale, theme, font);
        let menu = self.section_menu.as_ref()?;
        let rect = Self::popup_rect(menu, layout.location, bounds, scale, theme, font);

        if let InputEvent::PointerPressed {
            button: PointerButton::Primary,
        } = event
        {
            if menu.row_at(rect, scale, theme, *self.pointer).is_none() {
                self.section_menu = None;
                return None;
            }
        }

        let menu = self.section_menu.as_mut()?;
        match menu.on_pointer(event, rect, scale, theme) {
            Some(MenuAction::Activated { index }) => self.choose_section_row(index),
            Some(MenuAction::Dismissed) => {
                self.section_menu = None;
                None
            }
            Some(MenuAction::OpenSubmenu { .. }) | None => None,
        }
    }

    /// Route a key to the open section list, closing it on a choice or a
    /// dismissal.
    fn section_menu_on_key(&mut self, key: Key) -> Option<SwitchboardAction> {
        let action = self.section_menu.as_mut()?.on_key(key);
        match action {
            Some(MenuAction::Activated { index }) => self.choose_section_row(index),
            Some(MenuAction::Dismissed) => {
                self.section_menu = None;
                None
            }
            Some(MenuAction::OpenSubmenu { .. }) | None => None,
        }
    }

    /// Apply a choice from the section list: the list closes either way, and an
    /// out-of-range row changes nothing (fail closed).
    fn choose_section_row(&mut self, index: usize) -> Option<SwitchboardAction> {
        self.section_menu = None;
        self.select_section_index(index)
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

    /// Feed one key event, returning the typed action it produced (if any).
    ///
    /// An in-flight rename, then an open popup, take every key first: each is
    /// modal over the composition, so no key reaches the regions beneath it
    /// until it commits, cancels, or dismisses. Otherwise Tab cycles keyboard
    /// focus between the location band, the content list, the scrollbar, and
    /// the title-bar command group; keys are then routed to the focused
    /// region's control.
    ///
    /// Sections are reachable without a pointer: with focus on the location
    /// band, Space or Enter opens the section list, Up/Down walk it, and Enter
    /// shows the section under the cursor — the [`Menu`]'s own keys, with
    /// Escape closing it and leaving the section as it was.
    pub fn on_key(&mut self, key: Key) -> Option<SwitchboardAction> {
        if self.rename.is_some() {
            return self.rename_on_key(key);
        }
        if self.group_popup.is_some() {
            return self.group_popup_on_key(key);
        }
        if self.section_menu.is_some() {
            return self.section_menu_on_key(key);
        }
        if key == Key::Named(NamedKey::Tab) {
            self.focus = self.focus.next();
            self.apply_focus_marks();
            return None;
        }
        match self.focus {
            FocusRegion::TitleBar => self.frame.title_bar_mut().on_key(key).map(translate_title),
            // The trail is the band's keyboard route: its leading crumb opens
            // the section list, which then owns the keys until it closes. The
            // trailing command is the same command for the pointer, so giving
            // it its own stop would be a second stop for one action.
            FocusRegion::Location => {
                if let Some(BreadcrumbAction::Activate { .. }) = self.trail.on_key(key) {
                    self.open_section_menu();
                }
                None
            }
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

    /// The one section transition: every path that changes the shown section —
    /// the section list, the keyboard, and
    /// [`select_section`](Switchboard::select_section) — runs this, so the
    /// location trail, the content, and the per-section scroll offset can never
    /// disagree.
    ///
    /// It re-spells the trail, shows the section, and puts keyboard focus back
    /// on its first item; the offset stays each section's own and is re-clamped
    /// against the new content by the next scroll sync. Re-selecting the shown
    /// section is a no-op, and an out-of-range index changes nothing (fail
    /// closed); both report no change.
    fn select_section_index(&mut self, index: usize) -> Option<SwitchboardAction> {
        let section = Section::from_index(index)?;
        if section == self.section {
            return None;
        }
        self.section = section;
        self.trail = Self::build_trail(section);
        self.content_focus = 0;
        self.row_action = 0;
        // Both popups name the section that opened them — the Group popup
        // anchors on one of its rows, the section list marks it as the one on
        // show — so a section change drops them rather than leaving either
        // standing over, or lying about, unrelated content.
        self.group_popup = None;
        self.section_menu = None;
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

    /// Reflect the current focus region on the sub-controls: the focused crumb
    /// in the location trail, the focused scrollbar, and the focused content
    /// item's primary action.
    ///
    /// The focused content item is also a **Focus Field**: its row (or card)
    /// and *every* one of its actions are marked as members of the group,
    /// while only the one action `row_action` names takes the ring. That is
    /// what makes a row read as a related set rather than as one lit button
    /// beside some unrelated neighbours, and it is why membership is set from
    /// the same `focus_here` fact the ring is — the two can never disagree.
    fn apply_focus_marks(&mut self) {
        // The trail's leading crumb is the band's one keyboard stop; the
        // trailing crumb is the current location a breadcrumb never focuses.
        self.trail
            .set_focus((self.focus == FocusRegion::Location).then_some(0));
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
            entry.row.set_in_focus_field(focus_here);
            entry.action.set_focused(focus_here && action == 0);
            entry.action.set_in_focus_field(focus_here);
            entry.group_button.set_focused(focus_here && action == 1);
            entry.group_button.set_in_focus_field(focus_here);
        }
        for (i, card) in self.jobs.iter_mut().enumerate() {
            let focus_here = content && self.section == Section::Jobs && i == idx;
            card.set_in_focus_field(focus_here);
            for (b, button) in card.footer_mut().iter_mut().enumerate() {
                button.set_focused(focus_here && b == action);
                button.set_in_focus_field(focus_here);
            }
        }
        for (i, entry) in self.pressure.iter_mut().enumerate() {
            let focus_here = content && self.section == Section::Pressure && i == idx;
            entry.card.set_in_focus_field(focus_here);
            for (b, button) in entry.card.footer_mut().iter_mut().enumerate() {
                button.set_focused(focus_here && b == action);
                button.set_in_focus_field(focus_here);
            }
        }
        // Only an Activities header row carries buttons, so the flattened row
        // focus marks a button only when it names a header; a member row is a
        // field of one.
        let focused_activity = (content && self.section == Section::Activities)
            .then(|| self.activity_row_at(idx))
            .flatten();
        let focused_header = focused_activity.and_then(|row| match row {
            ActivityRow::Header(ai) => Some(ai),
            ActivityRow::Member(..) => None,
        });
        let focused_member = focused_activity.and_then(|row| match row {
            ActivityRow::Member(ai, mi) => Some((ai, mi)),
            ActivityRow::Header(..) => None,
        });
        for (i, entry) in self.activities.iter_mut().enumerate() {
            let focus_here = focused_header == Some(i);
            entry.header.set_in_focus_field(focus_here);
            entry.switch.set_focused(focus_here && action == 0);
            entry.switch.set_in_focus_field(focus_here);
            entry.pause_resume.set_focused(focus_here && action == 1);
            entry.pause_resume.set_in_focus_field(focus_here);
            entry.rename.set_focused(focus_here && action == 2);
            entry.rename.set_in_focus_field(focus_here);
            entry.close.set_focused(focus_here && action == 3);
            entry.close.set_in_focus_field(focus_here);
            for (m, member) in entry.members.iter_mut().enumerate() {
                member.set_in_focus_field(focused_member == Some((i, m)));
            }
        }
        for (i, entry) in self.recovery.iter_mut().enumerate() {
            let focus_here = content && self.section == Section::Recovery && i == idx;
            entry.row.set_in_focus_field(focus_here);
            entry.restart.set_focused(focus_here && action == 0);
            entry.restart.set_in_focus_field(focus_here);
            entry.force.set_focused(focus_here && action == 1);
            entry.force.set_in_focus_field(focus_here);
        }
        for (i, entry) in self.services.iter_mut().enumerate() {
            let focus_here = content && self.section == Section::Overview && i == idx;
            entry.row.set_in_focus_field(focus_here);
            entry.action.set_focused(focus_here);
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
