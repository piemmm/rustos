//! The Switchboard window's screen (`plans/NEW-SWITCHBOARD.md`).
//!
//! This is the surface behind the always-right-most Switchboard taskbar icon:
//! the live task, background-job, pressure, activity, recovery, and
//! system-overview state this service samples, laid out for a reader. It is
//! assembled **purely from the shared Reactive Alloy controls** (spec §17) —
//! the window-manager
//! [`WindowFrame`]/[`TitleBar`](tairix_controls::TitleBar)/[`ResizeGrabber`]
//! furniture, a [`Breadcrumb`] location band, the collection controls
//! ([`ListRow`](tairix_controls::ListRow), [`Card`](tairix_controls::Card),
//! [`Panel`](tairix_controls::Panel)), action
//! [`Button`](tairix_controls::Button)s, and one shared [`ScrollBar`] — so the
//! application paints no chrome of its own and carries no second copy of any
//! control's behaviour.
//!
//! # Package layout
//!
//! This module is the shared skeleton every section draws into: the
//! [`Switchboard`] retained widget tree, [`SwitchboardModel`], [`Section`],
//! [`SwitchboardAction`], the window frame and chrome, input dispatch, the
//! scroll model, and the list geometry every section's primary column reuses.
//! Each section is a struct in its own sibling module — [`mod@tasks`],
//! [`mod@background`], [`mod@pressure`], [`mod@activities`],
//! [`mod@recovery`], and [`mod@system`] — owning its own view models,
//! controls and cursor behind one internal section dispatch, and every type
//! that was public from this screen before the split is re-exported here
//! unchanged.
//!
//! # What it composes
//!
//! - The outer window is a [`WindowFrame`] with the standard
//!   [`TitleBar`](tairix_controls::TitleBar) and
//!   the four window commands; the only application region is the client
//!   viewport, so the client can never receive furniture input (the frame's
//!   hit map enforces this).
//! - Immediately below the title bar sits the **location band**: a [`Breadcrumb`] reading
//!   `Switchboard › <section>` with a section-list [`IconButton`] at its
//!   trailing end. The trail's leading crumb and that command both open the
//!   one [`Menu`] of the six [`Section`]s — the one being shown marked
//!   selected — and choosing a row switches section. The host chooses which
//!   section the panel opens on — Recovery when the user reached for a
//!   flagged capsule, Tasks otherwise — with
//!   [`Switchboard::select_section`], never by feeding synthetic input.
//! - Each section lays itself out into the one
//!   [`SectionFrame`] anatomy resolved from what that
//!   section asked for, and its primary column is a vertical list drawn from
//!   the shared collection controls; when the list exceeds the viewport the
//!   standard vertical [`ScrollBar`] governs it (mouse wheel, thumb drag, end
//!   buttons, track paging, and keyboard, all from the one shared scroll
//!   engine).
//! - A [`ResizeGrabber`] sits at the scrollbar junction, kept clear of the
//!   scroll thumb.
//!
//! # Data in, typed actions out
//!
//! The caller builds a [`SwitchboardModel`] of typed view models
//! ([`TaskSummary`], [`JobSummary`], [`RecoveryItem`], [`SystemReport`],
//! [`SystemAction`]); Switchboard turns it into controls.
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
    ActionRail, AuthorityState, Breadcrumb, BreadcrumbAction, ButtonAction, CardAction,
    ControlRole, ControlState, Crumb, FrameLayout, FurniturePart, IconButton, Menu, MenuAction,
    MenuItem, RenderInvariant, ResizeEvent, ResizeGrabber, ScrollAction, ScrollBar, ScrollCorner,
    ScrollModel, ScrollOrientation, ScrollRange, SelectionState, TitleBarEvent,
    WindowActivationState, WindowControlKind, WindowFrame, WindowFurnitureState, WindowSizeState,
};
use tairix_icon::IconKind;

pub mod activities;
pub mod background;
pub mod frame;
pub mod pressure;
pub mod recovery;
pub mod system;
pub mod system_data;
pub mod tasks;

pub use activities::{ActivityControl, ActivityMember, ActivitySummary};
pub use background::{JobControl, JobSummary};
pub use pressure::{PressureAction, PressureCause, PressureControl};
pub use recovery::{CrashSnapshot, FaultImpact, FaultMark, RecoveryControl, RecoveryItem};
pub use system_data::{
    HeadlineTile, HealthSeverity, LimitRow, NetworkInterface, Reading, SessionSeat, StorageVolume,
    SystemAction, SystemFact, SystemPage, SystemReport, TileInstrument, Unmeasured,
};
pub use tasks::{TaskAuthority, TaskControl, TaskKind, TaskSummary};

use activities::ActivitiesSection;
use background::JobsSection;
use frame::{
    action_button_width, resolve_band, resolve_section_frame, row_commands_width, BandLayout,
    SectionAnatomy, SectionFrame,
};
use pressure::PressureSection;
use recovery::RecoverySection;
use system::SystemSection;
use tasks::TasksSection;

#[cfg(test)]
mod test_support;

#[cfg(test)]
#[path = "mod_tests.rs"]
mod tests;

/// One of Switchboard's six top-level sections (`plans/NEW-SWITCHBOARD.md`).
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
    /// The machine itself: its readings, its pages, and its own actions.
    System,
}

impl Section {
    /// The sections in tab order.
    pub const ALL: [Section; 6] = [
        Section::Tasks,
        Section::Jobs,
        Section::Pressure,
        Section::Activities,
        Section::Recovery,
        Section::System,
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
            Section::System => 5,
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
            Section::System => "System",
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

/// The complete typed model Switchboard renders
/// (`plans/NEW-SWITCHBOARD.md`).
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
    /// How many faults have cleared since the service started watching.
    ///
    /// Only something that folds one sample into the next can see a fault
    /// disappear, so this is counted where the samples meet and carried
    /// here — never re-derived by the screen, which sees one model at a
    /// time and would count differently depending on what it saw before.
    pub recovery_resolved: usize,
    /// Everything the System section shows: its header readings, its eight
    /// pages' bodies, and the machine's own actions.
    pub system: SystemReport,
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
            recovery_resolved: 0,
            system: SystemReport::default(),
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
    /// A command was invoked on the selected task.
    Task {
        /// The task's index within the model.
        index: usize,
        /// Which task command.
        control: TaskControl,
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

/// The screen's own name: the title bar's application name and the leading
/// crumb of the location trail are the same word, so it is spelled once.
const APP_NAME: &str = "Switchboard";

/// The text every surface shows in place of a figure the service did not
/// measure.
///
/// One word, spelled once, so a resource meter and a table cell can never
/// say "no reading" two different ways. It is deliberately a word rather
/// than a dash or a zero: a reader must be able to tell "nothing measured
/// this" from "measured, and it was nothing".
pub const UNMEASURED_READING: &str = "unknown";

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

/// The selection a section should hold after a refresh, given the identity
/// it held before and the identities its fresh list carries.
///
/// A section's list is rebuilt from scratch on every sample, so a selection
/// remembered as a row *number* silently re-points at a different subject
/// the moment one above it leaves. Remembering the subject's own stable
/// identity instead — and re-finding it here — is what makes a selection
/// survive a refresh and drop only when the subject genuinely goes. Every
/// section that has a selection to keep resolves it through this one rule,
/// so two lists cannot answer the same question differently.
///
/// The fallback is the first subject in the fresh list: a section with
/// something to show always has something selected, and one with nothing to
/// show selects nothing rather than a subject that is not there.
fn resolve_selection<Id: Copy + Eq>(
    previous: Option<Id>,
    mut present: impl Iterator<Item = Id> + Clone,
) -> Option<Id> {
    if let Some(id) = previous {
        if present.clone().any(|candidate| candidate == id) {
            return Some(id);
        }
    }
    present.next()
}

/// Walk a section's row of on-screen master cards, reporting whichever one
/// reported an interaction — a completed body press or a completed footer
/// click — together with its own reported action.
///
/// Every Switchboard master/detail section shares this exact walk: locate
/// each visible slot's rectangle from `info` and hand it to the card living
/// there through `card_at` (which feeds the pointer event that drove this
/// call and returns what the card reported), remembering the last one that
/// answered. A press on a card's own body is exactly as much "this is now
/// the selected cause" as a footer click is, which is what makes every
/// section's rustdoc claim — that pressing a card opens its detail —
/// actually true; a card that additionally carries footer buttons still
/// reports which one fired, so the caller resolves that button's own meaning
/// on top of the selection. One walk here, rather than one written per
/// section, is what keeps that property from drifting out of step between
/// sections as they evolve.
fn select_pressed_card(
    info: &ListInfo,
    start: usize,
    mut card_at: impl FnMut(usize, Rect) -> Option<CardAction>,
) -> Option<(usize, CardAction)> {
    let mut chosen = None;
    for slot in 0..info.visible() {
        let index = start + slot as usize;
        let rect = info.item_rect(slot);
        if let Some(action) = card_at(index, rect) {
            chosen = Some((index, action));
        }
    }
    chosen
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

/// What one section's own input handling produced, before the screen turns it
/// into the action a host sees.
///
/// A section reports most intents directly, but it cannot run the transitions
/// that belong to the whole composition: the Pressure section's "Show tasks"
/// relief has to switch section and then place *another* section's content
/// cursor. Naming that request instead of performing it keeps the one section
/// transition and the one piece of focus arithmetic where every other route
/// already finds them.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
enum SectionOutcome {
    /// Report this action to the host.
    Action(SwitchboardAction),
    /// Show [`Section::Tasks`] with the task at this index focused; `None`
    /// focuses the first task.
    ShowTask {
        /// The task to focus, clamped into the Tasks list by the screen.
        task: Option<usize>,
    },
}

/// Everything a section needs to lay itself out, paint, and hit-test for one
/// frame.
///
/// The screen resolves this once per repaint or event and hands the same
/// bundle to every section entry point, so a section never re-derives its own
/// regions, never re-reads the scroll offset, and no call site restates the
/// same seven parameters.
#[derive(Copy, Clone, Debug)]
struct SectionCtx<'a> {
    /// This section's regions, resolved from its [`SectionView::anatomy`].
    frame: SectionFrame,
    /// The whole window's bounds, so an overlay clamps inside the window
    /// rather than inside the section that opened it.
    bounds: Rect,
    /// The index of the primary column's first visible item.
    start: usize,
    /// The last pointer position, for the hit tests a control cannot resolve
    /// from the event alone.
    pointer: Point,
    /// The active UI scale.
    scale: Scale,
    /// The active theme.
    theme: &'a Theme,
    /// The text font.
    font: BitmapFont,
}

/// One Switchboard section: its view models, its retained controls, its
/// content cursor, and its own painting and input.
///
/// This is the whole surface the screen needs from a section, so it reaches
/// the section on show through a single dispatch
/// ([`Switchboard::active`]/[`Switchboard::active_mut`]) rather than a `match`
/// per question. Everything *shared* deliberately stays out: the window
/// chrome, the scroll model, the section transition, and the keyboard policy
/// (Tab cycles the regions, the arrows move the content cursor, and the
/// focused item is scrolled into view) all live in the screen, so a section
/// reports its counts and holds its own cursor rather than re-deriving any of
/// that for itself.
trait SectionView {
    /// The regions this section asks the frame to seat.
    fn anatomy(&self) -> SectionAnatomy;

    /// Rebuild this section's controls from a fresh sample.
    ///
    /// Each section takes what it needs from the one sample and keeps what is
    /// the user's — its cursor, clamped into the new content, and any overlay
    /// that survives a refresh.
    fn adopt(&mut self, model: &SwitchboardModel);

    /// How many items the primary column's scrollable list holds. This is the
    /// scroll range's content extent.
    fn item_count(&self) -> usize;

    /// How many places the content cursor has to be in this section.
    ///
    /// For most sections that is exactly its rows, which is the default. A
    /// section with focusable chrome of its own — a header band of filters,
    /// a footer of controls — spans those too, so every one of its controls
    /// is reachable by the same Up/Down the rows are, and stays reachable
    /// when a filter leaves no rows at all. It never changes what
    /// [`item_count`](Self::item_count) means, so the scroll model is the
    /// rows' alone.
    fn focus_span(&self) -> usize {
        self.item_count()
    }

    /// Which scrollable row, if any, the content cursor at `index`
    /// corresponds to.
    ///
    /// This is how the screen keeps the one "scroll the focused thing into
    /// view" arithmetic while a section's cursor spans things that are not
    /// rows: a cursor on the header or the footer answers [`None`] and the
    /// offset is left where the reader put it, because neither scrolls.
    fn focus_row(&self, index: usize) -> Option<usize> {
        (index < self.item_count()).then_some(index)
    }

    /// Where the primary column's list draws, how tall one of its items is,
    /// and how many it holds.
    ///
    /// The rectangle is the section's own: most seat the list in the whole
    /// `primary` region, but the System section seats it inside its panel,
    /// below the resource block, so the viewport the scrollbar is ranged over
    /// has to come from the section rather than from `primary`'s height.
    fn list_info(&self, frame: &SectionFrame, scale: Scale, theme: &Theme) -> ListInfo;

    /// How many inline action buttons one of this section's rows carries, for
    /// the anchored action column beside the list. Zero when its items are
    /// cards, which draw their own footer actions inside themselves.
    fn row_buttons(&self) -> u32;

    /// How many actions the *focused* item carries — the bound the screen
    /// clamps the within-row action cursor to. Not always
    /// [`row_buttons`](Self::row_buttons): a card's footer length is its own,
    /// and a display-only row carries none.
    fn focused_action_count(&self) -> usize;

    /// The content cursor: which item of the primary column the keyboard is
    /// on.
    fn content_focus(&self) -> usize;

    /// Move the content cursor. The caller has already clamped it into the
    /// list.
    fn set_content_focus(&mut self, index: usize);

    /// The within-row action cursor: which of the focused item's actions the
    /// keyboard is on.
    fn row_action(&self) -> usize;

    /// Move the within-row action cursor. The caller has already clamped it
    /// against [`focused_action_count`](Self::focused_action_count).
    fn set_row_action(&mut self, index: usize);

    /// Feed an activation key to the focused item's action-focused control. A
    /// disabled or denied control refuses the key itself, so a refused
    /// activation produces nothing.
    fn activate_focused(&mut self, key: Key) -> Option<SectionOutcome>;

    /// Paint the section into its own regions.
    fn render(&self, surface: &mut Surface, ctx: SectionCtx<'_>);

    /// Paint the summary this section asked the location band to seat
    /// ([`SectionAnatomy::band_summary`]), into the rectangle the band
    /// resolved for it.
    ///
    /// Nothing by default: a section with no census in its anatomy is never
    /// given a rectangle to paint into, so the two can never disagree.
    fn render_band(
        &self,
        surface: &mut Surface,
        rect: Rect,
        scale: Scale,
        theme: &Theme,
        font: BitmapFont,
    ) {
        let _ = (surface, rect, scale, theme, font);
    }

    /// Route a pointer event to the section's items.
    fn on_pointer(&mut self, event: &InputEvent, ctx: SectionCtx<'_>) -> Option<SectionOutcome>;

    /// Mark this section's focus rings and Focus Field membership for a
    /// content region that is (or is not) `focused`.
    ///
    /// Every section is told, not just the one on show, so the rings of a
    /// section the reader has navigated away from are cleared rather than
    /// left lit under content nobody is looking at.
    fn apply_focus_marks(&mut self, focused: bool);

    /// Whether this section holds the keyboard: an open popup or an in-flight
    /// inline edit of its own takes every key before the Tab-cycled regions
    /// see it.
    fn holds_keyboard(&self) -> bool {
        false
    }

    /// Whether this section holds the pointer: an open popup is modal over
    /// the whole composition, so a press outside it dismisses it rather than
    /// falling through to whatever sits beneath. An inline edit does *not*
    /// hold the pointer — it is a control inside the list, and the rest of
    /// the surface stays reachable while it is open.
    fn holds_pointer(&self) -> bool {
        false
    }

    /// Paint this section's overlay, above every other region including the
    /// scrollbar and grabber.
    fn render_overlay(&self, _surface: &mut Surface, _ctx: SectionCtx<'_>) {}

    /// Route a pointer event to this section's overlay while it
    /// [`holds_pointer`](Self::holds_pointer).
    fn overlay_on_pointer(
        &mut self,
        _event: &InputEvent,
        _ctx: SectionCtx<'_>,
    ) -> Option<SectionOutcome> {
        None
    }

    /// Route a key to this section's overlay while it
    /// [`holds_keyboard`](Self::holds_keyboard).
    fn overlay_on_key(&mut self, _key: Key) -> Option<SectionOutcome> {
        None
    }

    /// Drop this section's overlay because the section is no longer shown.
    ///
    /// Both a popup and an inline edit name a row the reader has navigated
    /// away from, so neither may outlive the section that opened it: an
    /// overlay left standing would keep taking keys for content that is not
    /// on screen.
    fn dismiss_overlay(&mut self) {}
}

/// This application's Switchboard screen (`plans/NEW-SWITCHBOARD.md`).
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
    /// The six sections, each owning its own view models, controls, cursor and
    /// overlays. The screen reaches the one on show through
    /// [`active`](Self::active)/[`active_mut`](Self::active_mut), never by
    /// naming a section's own state here.
    tasks: TasksSection,
    jobs: JobsSection,
    pressure: PressureSection,
    activities: ActivitiesSection,
    recovery: RecoverySection,
    system: SystemSection,
    section: Section,
    offsets: [u64; 6],
    focus: FocusRegion,
    /// The last pointer position, kept so a press can be resolved against the
    /// coordinate the pointer actually reached — hit-testing input, never a
    /// drawn property.
    pointer: RenderInvariant<Point>,
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
    pub fn new(model: &SwitchboardModel) -> Self {
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
            tasks: TasksSection::new(),
            jobs: JobsSection::new(),
            pressure: PressureSection::new(),
            activities: ActivitiesSection::new(),
            recovery: RecoverySection::new(),
            system: SystemSection::new(),
            section: Section::Tasks,
            offsets: [0; 6],
            focus: FocusRegion::Content,
            pointer: RenderInvariant::new(Point::ORIGIN),
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
    pub fn set_model(&mut self, model: &SwitchboardModel) {
        self.adopt(model);
        self.set_scroll_range(
            self.active().item_count(),
            self.scroll.model().range().viewport_extent(),
        );
    }

    /// Derive every model-shaped part of the composition from `model` — the
    /// window furniture and title, and every section's own controls — then
    /// re-assert the keyboard focus onto the controls that replaced the old
    /// ones.
    ///
    /// This is the one model-to-controls derivation. Both
    /// [`new`](Switchboard::new) and [`set_model`](Switchboard::set_model) run
    /// it, so a refreshed Switchboard holds exactly the controls a freshly
    /// built one would, marked exactly the same way. Every section is handed
    /// the sample, not just the one on show, so switching to a section never
    /// shows a reading from a sample ago; each clamps its own cursor into its
    /// own new content, so no cursor can address a row its model no longer
    /// has.
    ///
    /// An open section list survives: its rows are the closed [`Section`] set,
    /// so no sample can make it stale, and closing it would snatch a menu out
    /// from under the reader mid-gesture. What each section does with its own
    /// overlay is that section's own business.
    fn adopt(&mut self, model: &SwitchboardModel) {
        let furniture = model.furniture;
        self.frame.set_furniture(furniture);
        self.frame.title_bar_mut().set_title(&model.title);

        let active = furniture.activation != WindowActivationState::Inactive;
        self.grabber
            .set_enabled(furniture.resizable && furniture.size == WindowSizeState::Restored);
        self.grabber.set_active_frame(active);
        self.corner.set_active_frame(active);

        for section in Section::ALL {
            self.section_mut(section).adopt(model);
        }
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
        self.activities.submitted_name()
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

    /// The overlay rectangle for `menu`: its preferred size, placed below
    /// `anchor` (or above it when there is no room below), clamped inside
    /// `bounds` so it never draws outside the window.
    ///
    /// The section list and a section's own popup place themselves the same
    /// way, so there is one placement rule rather than one per menu.
    pub(super) fn popup_rect(
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
/// The laid-out regions of a Switchboard for one outer bounds.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
struct SbLayout {
    /// The window frame's laid-out rectangles.
    frame: FrameLayout,
    /// The location band along the top of the client: the trail, the active
    /// section's own band summary, and the trailing section-list command,
    /// split by [`resolve_band`].
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
    /// The list of `count` list-row items filling `rect`: one control plus a
    /// gap per item.
    ///
    /// Every section whose primary column is rows builds its metrics here, so
    /// the row pitch is one fact rather than one per section.
    pub(super) fn rows(rect: Rect, count: usize, scale: Scale, theme: &Theme) -> Self {
        Self {
            list_rect: rect,
            item_h: Switchboard::row_item_height(scale, theme),
            count,
        }
    }

    /// The list of `count` card items filling `rect`, the taller pitch a
    /// [`Card`](tairix_controls::Card) with a body and a footer needs.
    pub(super) fn cards(rect: Rect, count: usize, scale: Scale, theme: &Theme) -> Self {
        Self {
            list_rect: rect,
            item_h: Switchboard::card_item_height(scale, theme),
            count,
        }
    }

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
    /// The location band claims one control height immediately below the title
    /// bar and the content and scrollbar take what is left, clipped so a
    /// window too short for the full anatomy still lays out in bounds
    /// (fail closed, never negative or overlapping).
    fn compute_layout(&self, bounds: Rect, scale: Scale, theme: &Theme) -> SbLayout {
        let frame = self.frame.layout(bounds, scale, theme);
        let client = frame.client;

        // The band is shared chrome, but how tall it is belongs to the
        // section on show: one that seats a census there needs more than the
        // resting control height, and one that does not must not pay for it.
        let location_h = self
            .active()
            .anatomy()
            .band_height(scale, theme)
            .min(client.height);
        let location = Rect::new(client.left(), client.top(), client.width, location_h);

        let below_top = client.top() + to_i32(location_h);
        let below_h = client.height.saturating_sub(location_h);
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
            location,
            content,
            scroll,
            corner,
        }
    }

    /// The location band's rectangles for the section on show, resolved
    /// through the one [`resolve_band`] every paint and hit test reads.
    fn band(&self, location: Rect, theme: &Theme, scale: Scale) -> BandLayout {
        resolve_band(location, self.active().anatomy().band_summary, scale, theme)
    }

    /// The section on show, for everything the screen asks a section that
    /// needs no mutation: its anatomy, its counts, its cursor, and its
    /// painting.
    ///
    /// This and [`active_mut`](Self::active_mut) are the only places a
    /// [`Section`] is turned back into the state behind it, so there is one
    /// route to a section rather than one per question.
    fn active(&self) -> &dyn SectionView {
        match self.section {
            Section::Tasks => &self.tasks,
            Section::Jobs => &self.jobs,
            Section::Pressure => &self.pressure,
            Section::Activities => &self.activities,
            Section::Recovery => &self.recovery,
            Section::System => &self.system,
        }
    }

    /// The section on show, for everything that moves its cursor, feeds it
    /// input, or re-derives it from a sample.
    fn active_mut(&mut self) -> &mut dyn SectionView {
        self.section_mut(self.section)
    }

    /// One named section, however it was named — the one place a [`Section`]
    /// becomes the state behind it, so a refresh and a focus sweep can visit
    /// every section without a second copy of this mapping.
    fn section_mut(&mut self, section: Section) -> &mut dyn SectionView {
        match section {
            Section::Tasks => &mut self.tasks,
            Section::Jobs => &mut self.jobs,
            Section::Pressure => &mut self.pressure,
            Section::Activities => &mut self.activities,
            Section::Recovery => &mut self.recovery,
            Section::System => &mut self.system,
        }
    }

    /// The active section's frame and everything else it needs for one
    /// repaint or event, resolved once from the section's own anatomy.
    fn section_ctx<'a>(
        &self,
        layout: &SbLayout,
        bounds: Rect,
        scale: Scale,
        theme: &'a Theme,
        font: BitmapFont,
    ) -> SectionCtx<'a> {
        SectionCtx {
            frame: resolve_section_frame(layout.content, self.active().anatomy(), scale, theme),
            bounds,
            start: usize::try_from(self.offsets[self.section.index()]).unwrap_or(0),
            pointer: *self.pointer,
            scale,
            theme,
            font,
        }
    }

    /// The scrollable list metrics for the active section.
    fn list_info(&self, layout: &SbLayout, scale: Scale, theme: &Theme) -> ListInfo {
        let frame = resolve_section_frame(layout.content, self.active().anatomy(), scale, theme);
        self.active().list_info(&frame, scale, theme)
    }

    /// The anchored action column of the active section: the strip the rows'
    /// inline action buttons stand in, spanning the whole visible list.
    ///
    /// Every row lays its actions against the same trailing edge, so the
    /// column is one rectangle rather than a per-row fact; it is derived from
    /// the same [`Switchboard::split_row`] geometry the buttons themselves
    /// are laid out with, so the column cannot drift away from its contents.
    /// [`None`] when the section's items carry no inline actions.
    fn action_column(info: ListInfo, buttons: u32, scale: Scale, theme: &Theme) -> Option<Rect> {
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
        let aw = action_button_width(scale, theme);
        let total = row_commands_width(buttons, scale, theme);
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
    fn sync_scroll(&mut self, bounds: Rect, scale: Scale, theme: &Theme) {
        let layout = self.compute_layout(bounds, scale, theme);
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
        self.sync_scroll(bounds, scale, theme);
        let layout = self.compute_layout(bounds, scale, theme);
        let ctx = self.section_ctx(&layout, bounds, scale, theme, font);

        self.frame.render(surface, bounds, scale, theme, font);
        self.render_location(surface, layout.location, scale, theme, font);
        self.render_section(surface, ctx);

        // The scrollbar and its junction/resize corner, drawn last so they sit
        // above the content and the corner never overlaps the thumb.
        self.scroll.render(surface, layout.scroll, scale, theme);
        self.corner.render(surface, layout.corner, scale, theme);
        self.grabber.render(surface, layout.corner, scale, theme);

        // The popups, painted last of all so they sit above every other
        // region, including the scrollbar and grabber. Only one can be open:
        // each is modal over the whole composition while it is, so no input
        // can reach the control that would open the other.
        self.active().render_overlay(surface, ctx);
        if let Some(menu) = &self.section_menu {
            let rect = Self::popup_rect(menu, layout.location, bounds, scale, theme, font);
            menu.render(surface, rect, scale, theme, font);
        }
    }

    /// Paint the location band: the trail naming where the reader is, the
    /// section's own summary beside it, then the command that opens the
    /// section list, over the one [`Switchboard::band`] the hit test reads.
    fn render_location(
        &self,
        surface: &mut Surface,
        location: Rect,
        scale: Scale,
        theme: &Theme,
        font: BitmapFont,
    ) {
        let band = self.band(location, theme, scale);
        self.trail.render(surface, band.trail, scale, theme, font);
        if let Some(summary) = band.summary {
            self.active()
                .render_band(surface, summary, scale, theme, font);
        }
        self.section_list
            .render(surface, band.command, scale, theme, font, None);
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
    fn render_section(&self, surface: &mut Surface, ctx: SectionCtx<'_>) {
        let section = self.active();
        section.render(surface, ctx);
        let info = section.list_info(&ctx.frame, ctx.scale, ctx.theme);
        if let Some(column) = Self::action_column(info, section.row_buttons(), ctx.scale, ctx.theme)
        {
            let scrolled = self.offsets[self.section.index()] != 0;
            ActionRail::new(Vec::new())
                .with_edge_wake(scrolled)
                .render(surface, column, ctx.scale, ctx.theme, ctx.font);
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
        if self.active().holds_pointer() {
            let layout = self.compute_layout(bounds, scale, theme);
            let ctx = self.section_ctx(&layout, bounds, scale, theme, font);
            let outcome = self.active_mut().overlay_on_pointer(event, ctx);
            return outcome.and_then(|outcome| self.resolve_outcome(outcome));
        }
        if self.section_menu.is_some() {
            return self.section_menu_on_pointer(event, bounds, scale, theme, font);
        }

        self.sync_scroll(bounds, scale, theme);
        let layout = self.compute_layout(bounds, scale, theme);

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
        let band = self.band(layout.location, theme, scale);
        let (trail, command) = (band.trail, band.command);
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
        let ctx = self.section_ctx(&layout, bounds, scale, theme, font);
        let outcome = self.active_mut().on_pointer(event, ctx);
        outcome.and_then(|outcome| self.resolve_outcome(outcome))
    }

    /// Turn what a section reported into the action a host sees, running the
    /// composition-wide transitions a section may ask for but never perform
    /// itself.
    fn resolve_outcome(&mut self, outcome: SectionOutcome) -> Option<SwitchboardAction> {
        match outcome {
            SectionOutcome::Action(action) => Some(action),
            SectionOutcome::ShowTask { task } => self.show_task(task),
        }
    }

    /// Show [`Section::Tasks`] with `task` focused (clamped into the list;
    /// `None` focuses the first task) and report the transition.
    ///
    /// This runs the one section transition and the one focus arithmetic every
    /// other route runs, so the Pressure section's "Show tasks" relief cannot
    /// leave the trail, the content and the offsets disagreeing.
    fn show_task(&mut self, task: Option<usize>) -> Option<SwitchboardAction> {
        let action = self.select_section_index(Section::Tasks.index());
        let last = self.tasks.item_count().saturating_sub(1);
        let row = task.unwrap_or(0).min(last);
        let focus = self.tasks.focus_index_for_row(row);
        self.tasks.set_content_focus(focus);
        self.tasks.set_row_action(0);
        self.ensure_focus_visible();
        self.apply_focus_marks();
        action
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
        let layout = self.compute_layout(bounds, scale, theme);
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

    /// Feed one key event, returning the typed action it produced (if any).
    ///
    /// The active section's own overlay — an in-flight inline edit or an open
    /// popup — takes every key first: it is modal over the composition, so no
    /// key reaches the regions beneath it until it commits, cancels, or
    /// dismisses. Otherwise Tab cycles keyboard focus between the location
    /// band, the content list, the scrollbar, and the title-bar command group;
    /// keys are then routed to the focused region's control.
    ///
    /// Sections are reachable without a pointer: with focus on the location
    /// band, Space or Enter opens the section list, Up/Down walk it, and Enter
    /// shows the section under the cursor — the [`Menu`]'s own keys, with
    /// Escape closing it and leaving the section as it was.
    pub fn on_key(&mut self, key: Key) -> Option<SwitchboardAction> {
        if self.active().holds_keyboard() {
            let outcome = self.active_mut().overlay_on_key(key);
            return outcome.and_then(|outcome| self.resolve_outcome(outcome));
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
        let count = self.active().focus_span();
        if count == 0 {
            return None;
        }
        match key {
            Key::Named(NamedKey::Down) => {
                let next = (self.active().content_focus() + 1).min(count - 1);
                self.move_content_focus(next);
                None
            }
            Key::Named(NamedKey::Up) => {
                let next = self.active().content_focus().saturating_sub(1);
                self.move_content_focus(next);
                None
            }
            Key::Named(NamedKey::Right) => {
                let last = self.active().focused_action_count().saturating_sub(1);
                let next = (self.active().row_action() + 1).min(last);
                self.active_mut().set_row_action(next);
                self.apply_focus_marks();
                None
            }
            Key::Named(NamedKey::Left) => {
                let next = self.active().row_action().saturating_sub(1);
                self.active_mut().set_row_action(next);
                self.apply_focus_marks();
                None
            }
            _ => {
                let outcome = self.active_mut().activate_focused(key)?;
                self.resolve_outcome(outcome)
            }
        }
    }

    /// Put the content cursor on `index` of the active section: the action
    /// cursor returns to the item's first action, the item is scrolled into
    /// view, and the focus marks are re-applied.
    ///
    /// Both arrow keys move the cursor through here, so "keep the focused item
    /// visible" is one definition rather than one per direction or per
    /// section.
    fn move_content_focus(&mut self, index: usize) {
        self.active_mut().set_content_focus(index);
        self.active_mut().set_row_action(0);
        self.ensure_focus_visible();
        self.apply_focus_marks();
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
        // Every overlay names the section that opened it — a popup anchors on
        // one of its rows, an inline edit sits in one of them, and the section
        // list marks the section on show — so a section change drops them
        // rather than leaving one standing over, lying about, or still taking
        // keys for content the reader has navigated away from.
        self.active_mut().dismiss_overlay();
        self.section_menu = None;
        self.section = section;
        self.trail = Self::build_trail(section);
        self.active_mut().set_content_focus(0);
        self.active_mut().set_row_action(0);
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
        // A cursor on a section's own header or footer names no row, so
        // there is nothing to scroll to and the reader's offset stands.
        let Some(row) = self.active().focus_row(self.active().content_focus()) else {
            return;
        };
        let idx = u64::try_from(row).unwrap_or(0);
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

        // Every section is told, so the one on show lights its focused item
        // and the five behind it are cleared rather than left glowing under
        // content nobody is looking at.
        let content = self.focus == FocusRegion::Content;
        let active = self.section;
        for section in Section::ALL {
            self.section_mut(section)
                .apply_focus_marks(content && section == active);
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
