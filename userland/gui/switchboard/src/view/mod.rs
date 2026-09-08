//! The Switchboard window's screen (`plans/NEW-SWITCHBOARD.md`).
//!
//! This is the surface behind the always-right-most Switchboard taskbar icon:
//! the live task, resource-device and recovery state this service samples,
//! laid out for a reader. It is
//! assembled **purely from the shared Reactive Alloy controls** (spec §17) —
//! a [`Tabs`] navigation rail, the collection controls
//! ([`ListRow`](tairix_controls::ListRow), [`Card`](tairix_controls::Card)),
//! action [`Button`](tairix_controls::Button)s, and one shared [`ScrollBar`].
//! The window manager decorates the window server-side, so the application
//! draws no chrome of its own and carries no second copy of any control's
//! behaviour.
//!
//! The one thing it composes rather than consumes is its titled block: the
//! boards frame every readings block the same way, and
//! [`Panel`](tairix_controls::Panel) is a different anatomy — a header band at
//! control height with a dominant rail, a signal bead and an actions row — that
//! the terminal, the taskbar and the file manager share, so matching the boards
//! by retuning it would retune those three.
//!
//! # Package layout
//!
//! This module is the shared skeleton every section draws into: the
//! [`Switchboard`] retained widget tree, [`SwitchboardModel`], [`Section`],
//! [`SwitchboardAction`], input dispatch, the scroll model, and the list
//! geometry every section's primary column reuses.
//! Each section is a struct in its own sibling module — [`mod@tasks`],
//! [`mod@resources`] and [`mod@recovery`] — owning its own view models,
//! controls and cursor behind one internal section dispatch, and every type
//! a host names is re-exported here.
//!
//! # What it composes
//!
//! - The window manager decorates the window server-side, so the whole
//!   application region is the client content described here.
//! - Down the leading edge sits the **navigation rail**: one vertical [`Tabs`]
//!   strip listing every subject the surface can show — the task list, each
//!   resource device under its group heading, and the recovery list — each
//!   entry carrying its own reading and trace. It is the whole switcher, and
//!   is never shed, because it is the only route between subjects. The host
//!   chooses which subject the panel opens on — Recovery when the user
//!   reached for a flagged capsule, the processor otherwise — with
//!   [`Switchboard::select_section`], never by feeding synthetic input.
//! - Each section lays itself out into the one
//!   [`SectionFrame`] anatomy resolved from what that
//!   section asked for, and its primary column is a vertical list drawn from
//!   the shared collection controls; when the list exceeds the viewport the
//!   standard vertical [`ScrollBar`] governs it (mouse wheel, thumb drag, end
//!   buttons, track paging, and keyboard, all from the one shared scroll
//!   engine).
//!
//! # Data in, typed actions out
//!
//! The caller builds a [`SwitchboardModel`] of typed view models
//! ([`TaskSummary`], [`RecoveryItem`], [`ResourceReport`]); Switchboard
//! turns it into controls.
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
//! sample. The pointer's own highlight survives too, because a refresh moves
//! neither the pointer nor the slots it is over; a half-finished press does
//! not, because it names an object the slot may no longer hold.

use alloc::string::String;
use alloc::vec::Vec;

use tairix_font::BitmapFont;
use tairix_geometry::{to_i32, Point, Rect, Region, Scale};
use tairix_input::{InputEvent, Key, NamedKey};
use tairix_raster::{Color, Surface};
use tairix_theme::Theme;

use tairix_controls::{
    damage, ActionRail, AuthorityState, CardAction, Chart, ControlState, PressureKind,
    RenderInvariant, ScrollAction, ScrollBar, ScrollModel, ScrollOrientation, ScrollRange, Tab,
    TabGroupAbsence, Tabs, TabsAction, TabsOrientation,
};
use tairix_icon::{IconArtwork, IconKind, IconRequest};

mod block;
pub mod frame;
pub mod reading;
pub mod recovery;
mod refresh;
pub mod resources;
pub mod tasks;

pub use reading::{
    absence_statement, reading_text, selection_prompt, HealthSeverity, Reading, ReadingFact,
    Unmeasured,
};
pub use recovery::{CrashSnapshot, FaultImpact, FaultMark, RecoveryControl, RecoveryItem};
pub use resources::{
    BlockBody, BlockSpan, CompositionPart, ConsumerRow, CoreCell, DeviceAction, DeviceId,
    HeroInstrument, PaneBlock, PaneHero, PressureBanner, RailGroup, ResourceControl,
    ResourceDevice, ResourceReport, TaskCostColumn,
};
pub use tasks::{TaskAuthority, TaskControl, TaskOwner, TaskSummary};

use frame::{
    action_button_width, resolve_section_frame, row_commands_width, SectionAnatomy, SectionFrame,
};
use recovery::RecoverySection;
use resources::ResourcesSection;
use tasks::TasksSection;

#[cfg(test)]
mod test_support;

#[cfg(test)]
#[path = "mod_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "rail_tests.rs"]
mod rail_tests;

/// The navigation rail's logical width: wide enough for the longest subject
/// name beside its reading at the reference density, and narrow enough that
/// the rail, the pane and a section's own action column all still seat in the
/// smallest window the panel allows.
pub(crate) const RAIL_WIDTH: u32 = 168;

/// One of Switchboard's three top-level sections
/// (`plans/NEW-SWITCHBOARD.md` S4) — one per question a reader arrives
/// with: what is running, what is this machine doing, what broke.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub enum Section {
    /// Live application/task list.
    Tasks,
    /// One pane per resource device: what this machine is doing.
    Resources,
    /// Hung objects and their recovery actions.
    Recovery,
}

impl Section {
    /// The sections in tab order.
    pub const ALL: [Section; 3] = [Section::Tasks, Section::Resources, Section::Recovery];

    /// The section's zero-based tab index.
    #[must_use]
    pub const fn index(self) -> usize {
        match self {
            Section::Tasks => 0,
            Section::Resources => 1,
            Section::Recovery => 2,
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
            Section::Resources => "Resources",
            Section::Recovery => "Recovery",
        }
    }
}

/// One subject the navigation rail can show.
///
/// The rail is the surface's whole navigator, so its entries span more than
/// the resource devices: what is running and what broke are subjects in their
/// own right, listed either side of the machine's devices.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum RailSubject {
    /// The task list.
    Tasks,
    /// One resource device's pane.
    Device(DeviceId),
    /// The recovery list.
    Recovery,
}

impl RailSubject {
    /// Which section shows this subject.
    #[must_use]
    pub const fn section(self) -> Section {
        match self {
            RailSubject::Tasks => Section::Tasks,
            RailSubject::Device(_) => Section::Resources,
            RailSubject::Recovery => Section::Recovery,
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
    /// The home root of the account this service runs as, if it has one.
    ///
    /// A run-scoped fact of the process rather than anything sampled, like the
    /// title beside it: it is what lets a task loaded from this user's own
    /// program store draw that bundle's icon. An absent home simply searches
    /// the system stores, so nothing is guessed.
    pub home: Option<String>,
    /// The live tasks.
    pub tasks: Vec<TaskSummary>,
    /// The hung/recoverable objects.
    pub recovery: Vec<RecoveryItem>,
    /// How many faults have cleared since the service started watching.
    ///
    /// Only something that folds one sample into the next can see a fault
    /// disappear, so this is counted where the samples meet and carried
    /// here — never re-derived by the screen, which sees one model at a
    /// time and would count differently depending on what it saw before.
    pub recovery_resolved: usize,
    /// The Tasks rail entry's own trace: the process population over the
    /// window, read against the largest this session has seen.
    pub tasks_trend: RailTrace,
    /// The Recovery rail entry's own trace: the share of the population that
    /// was stopped.
    pub recovery_trend: RailTrace,
    /// Everything the Resources section shows: one device per pane, in rail
    /// order.
    pub resources: ResourceReport,
}

/// One rail entry's trace: the readings, and what the top of their box means.
///
/// A device's trace is a permille share of that device's own capacity, so its
/// box needs no stated ceiling. The two subjects that are not devices count
/// things instead, and a count has no capacity — so it carries the denominator
/// its trace is drawn against rather than leaving the reader to assume one.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct RailTrace {
    /// The readings, oldest first.
    pub points: Vec<u16>,
    /// What the top of the box means. Zero leaves the permille default.
    pub full_scale: u16,
}

impl SwitchboardModel {
    /// An empty model with the given title and no data yet.
    #[must_use]
    pub fn new(title: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            home: None,
            tasks: Vec::new(),
            recovery: Vec::new(),
            recovery_resolved: 0,
            tasks_trend: RailTrace::default(),
            recovery_trend: RailTrace::default(),
            resources: ResourceReport::default(),
        }
    }

    /// This model with `home` as the session's own account root.
    #[must_use]
    pub fn with_home(mut self, home: Option<String>) -> Self {
        self.home = home;
        self
    }
}

/// The typed outcome of interacting with a [`Switchboard`].
///
/// Switchboard never performs an operation itself: it reports the intent and
/// the hosting service authorises, validates, and applies it, then feeds the
/// updated model back (a refusal fails closed rather than acting).
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum SwitchboardAction {
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
    /// A recovery action was invoked.
    Recovery {
        /// The object's index within the model.
        index: usize,
        /// Which recovery action.
        control: RecoveryControl,
    },
    /// A command was invoked on the selected resource device.
    Resource {
        /// The device's index within the report, in rail order.
        index: usize,
        /// Which device command.
        control: ResourceControl,
    },
    /// The active section was scrolled to `offset` (in item units).
    Scrolled {
        /// The new first-visible item index.
        offset: u64,
    },
}

/// The text every surface shows in place of a figure the service did not
/// measure.
///
/// One word, spelled once, so a resource meter and a table cell can never
/// say "no reading" two different ways. It is deliberately a word rather
/// than a dash or a zero: a reader must be able to tell "nothing measured
/// this" from "measured, and it was nothing".
pub const UNMEASURED_READING: &str = "unknown";

/// The picture one process's row draws.
///
/// An application the desktop launched resolves its *own* icon from the bundle
/// it was launched from — the session attests that, so it is the better
/// identity where it exists. Every other process resolves its icon from its
/// **name**, which the kernel attests from the store path it loaded the image
/// from and which no process can set for itself; the fixed program-store order
/// turns that name into the one bundle it could have come from. So a service,
/// a driver or a command draws its own picture too, not one generic mark for
/// everything the desktop did not launch.
///
/// `home` is this session's own account root, whose two program stores are
/// searched after every system one — so a user's own command app draws its own
/// icon, and no user-writable store can shadow a system program's.
///
/// Either way the fall-back ladder is the shared one: the bundle's declared
/// icon, then the class artwork, then the built-in glyph. A name that resolves
/// to no bundle simply reaches the class tier, so nothing is ever handed a
/// picture it has no claim to.
///
/// The one statement of that rule, read by the task table and by every
/// device's top-consumers block, so a process cannot be drawn as one thing in
/// one place and another elsewhere.
fn task_icon<'a>(bundle: Option<&'a str>, name: &'a str, home: Option<&'a str>) -> IconRequest<'a> {
    match bundle {
        Some(dir) => IconRequest::bundle(task_icon_kind(bundle), dir),
        None => IconRequest::program(task_icon_kind(bundle), name, home),
    }
}

/// The class a process's icon falls back to when nothing it names resolves.
///
/// Independent of where the picture is searched for, so a caller that needs
/// only the class — reserving a tile's icon slot — asks for it without a home
/// or a name it would have no use for.
const fn task_icon_kind(bundle: Option<&str>) -> IconKind {
    match bundle {
        Some(_) => IconKind::AppBundle,
        None => IconKind::Executable,
    }
}

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
    /// The navigation rail, which chooses what the surface shows.
    Rail,
    /// The active section's content list.
    Content,
    /// The vertical scrollbar.
    Scrollbar,
}

impl FocusRegion {
    /// The regions in Tab-cycle order.
    const ORDER: [FocusRegion; 3] = [
        FocusRegion::Rail,
        FocusRegion::Content,
        FocusRegion::Scrollbar,
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
    /// Show [`Section::Tasks`] ordered by what a device costs, so a busy
    /// device is traced to the tasks sitting on it.
    ShowTasksBy {
        /// Which cost to order the table by.
        column: resources::TaskCostColumn,
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
/// per question. Everything *shared* deliberately stays out: the location
/// band, the scroll model, the section transition, and the keyboard policy
/// (Tab cycles the regions, the arrows move the content cursor, and the
/// focused item is scrolled into view) all live in the screen, so a section
/// reports its counts and holds its own cursor rather than re-deriving any of
/// that for itself.
trait SectionView {
    /// The regions this section asks the frame to seat.
    fn anatomy(&self) -> SectionAnatomy;

    /// Rebuild this section's controls from a fresh sample, reporting the
    /// rectangles the rebuild actually repaints into `sweep`.
    ///
    /// Each section takes what it needs from the one sample and keeps what is
    /// the user's — its cursor, clamped into the new content, and any overlay
    /// that survives a refresh.
    ///
    /// A section on show sweeps with the frame it will next be drawn in, so it
    /// reports the instruments whose readings moved and the rows whose cells
    /// moved; a section that is not on show has no frame, draws nothing, and
    /// reports nothing. Reporting is what makes a fresh sample cost the
    /// readings that changed instead of the whole client, so a reading that
    /// moved and was not reported leaves a stale pixel: over-report where the
    /// two pull against each other (a re-ordered list reports its whole list).
    fn adopt(&mut self, model: &SwitchboardModel, sweep: &mut Sweep<'_, '_>);

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
    ///
    /// A section whose cursor *is* its selection — a rail entry naming the
    /// pane beside it, a card naming the detail beside it — selects here, so
    /// it reports the regions that selection re-derives into `sweep`. A
    /// section whose cursor only moves a ring reports nothing but its marks,
    /// which [`apply_focus_marks`](Self::apply_focus_marks) states.
    fn set_content_focus(&mut self, index: usize, sweep: &mut Sweep<'_, '_>);

    /// The within-row action cursor: which of the focused item's actions the
    /// keyboard is on.
    fn row_action(&self) -> usize;

    /// Move the within-row action cursor. The caller has already clamped it
    /// against [`focused_action_count`](Self::focused_action_count).
    fn set_row_action(&mut self, index: usize, sweep: &mut Sweep<'_, '_>);

    /// Feed an activation key to the focused item's action-focused control,
    /// reporting every control whose drawn state the key changed into
    /// `damage`. A disabled or denied control refuses the key itself, so a
    /// refused activation produces nothing.
    fn activate_focused(
        &mut self,
        key: Key,
        ctx: SectionCtx<'_>,
        damage: &mut Region,
    ) -> Option<SectionOutcome>;

    /// Re-lay-out anything whose shape depends on the width it will be
    /// drawn at, before the scroll model is ranged over it.
    ///
    /// Nothing by default. A section whose content re-wraps with its own
    /// width — the Resources pane's per-core grid — recompiles here, once
    /// per resize rather than per paint, so the scroll range always
    /// describes the layout that is actually on screen.
    fn relayout(&mut self, frame: &SectionFrame, scale: Scale, theme: &Theme) {
        let _ = (frame, scale, theme);
    }

    /// Paint the section into its own regions.
    ///
    /// `artwork` resolves every icon the section draws — an application's own
    /// picture where one is attested, its class's shipped artwork otherwise —
    /// so no draw site rasterises a glyph itself. It is passed to the render
    /// paths rather than carried on [`SectionCtx`] because only they need it:
    /// the context is `Copy` and reaches the input paths too, and a mutable
    /// borrow on it would have to be threaded through every one of them.
    fn render(&self, surface: &mut Surface, ctx: SectionCtx<'_>, artwork: &mut dyn IconArtwork);

    /// Route a pointer event to the section's items, reporting every control
    /// whose drawn state the event changed into `damage`.
    fn on_pointer(
        &mut self,
        event: &InputEvent,
        ctx: SectionCtx<'_>,
        damage: &mut Region,
    ) -> Option<SectionOutcome>;

    /// Mark this section's focus rings and Focus Field membership for a
    /// content region that is (or is not) `focused`.
    ///
    /// Every section is told, not just the one on show, so the rings of a
    /// section the reader has navigated away from are cleared rather than
    /// left lit under content nobody is looking at.
    fn apply_focus_marks(&mut self, focused: bool, sweep: &mut Sweep<'_, '_>);

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
    /// scrollbar.
    fn render_overlay(
        &self,
        _surface: &mut Surface,
        _ctx: SectionCtx<'_>,
        _artwork: &mut dyn IconArtwork,
    ) {
    }

    /// Route a pointer event to this section's overlay while it
    /// [`holds_pointer`](Self::holds_pointer), reporting every control whose
    /// drawn state the event changed into `damage`.
    fn overlay_on_pointer(
        &mut self,
        _event: &InputEvent,
        _ctx: SectionCtx<'_>,
        _damage: &mut Region,
    ) -> Option<SectionOutcome> {
        None
    }

    /// Route a key to this section's overlay while it
    /// [`holds_keyboard`](Self::holds_keyboard), reporting every control whose
    /// drawn state the key changed into `damage`.
    fn overlay_on_key(
        &mut self,
        _key: Key,
        _ctx: SectionCtx<'_>,
        _damage: &mut Region,
    ) -> Option<SectionOutcome> {
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

/// What one pass over the composition marks its controls against: the frame
/// the round holds, if it holds one, and the sink it reports into.
///
/// An interactive path — a key, a pointer outcome — holds the frame it just
/// laid out, and so does a fresh sample adopted into the section on show, so
/// every mark and every re-derived control reports the rectangle it repaints.
/// A caller with no frame to resolve a rectangle against — [`Switchboard::new`]
/// before a window exists, and the two sections that are *not* on show and
/// therefore draw nothing — sweeps with no `ctx`: each mark is adopted and
/// nothing is reported.
struct Sweep<'a, 'b> {
    ctx: Option<SectionCtx<'a>>,
    damage: &'b mut Region,
}

impl<'a, 'b> Sweep<'a, 'b> {
    /// A sweep from a path that holds the frame it laid out.
    fn reporting(ctx: SectionCtx<'a>, damage: &'b mut Region) -> Self {
        Self {
            ctx: Some(ctx),
            damage,
        }
    }

    /// A sweep from a caller with no frame, whose marks are adopted in silence.
    fn adopting(sink: &'b mut Region) -> Self {
        Self {
            ctx: None,
            damage: sink,
        }
    }

    /// The frame this sweep resolves rectangles against, or `None` when it
    /// reports nothing.
    const fn ctx(&self) -> Option<SectionCtx<'a>> {
        self.ctx
    }

    /// Report `rect` as repainted. A sweep with no frame reports nothing, so a
    /// section that resolved the rectangle from a frame it does not have
    /// cannot report against a made-up one.
    fn report(&mut self, rect: Rect) {
        if self.ctx.is_some() {
            self.damage.add(rect);
        }
    }

    /// Report the whole client, for a transition that re-lays every region at
    /// once — a section change re-spells the band, the content, and the
    /// scrollbar together, and drops whatever popup was over them. A sweep
    /// with no frame already presents whole and has nothing to report.
    fn client(&mut self) {
        if let Some(ctx) = self.ctx {
            self.damage.add(ctx.bounds);
        }
    }

    /// Mark an action rail's focused command.
    ///
    /// `rect` is the rail's own content rectangle, resolved from this sweep's
    /// frame by the section that seats it, or [`None`] when the frame is too
    /// narrow to seat the rail at all — it is then drawn nowhere and the
    /// empty rectangle reports nothing. A sweep with no frame is a rebuild
    /// that presents the composition whole, so it adopts the mark instead.
    fn rail(&mut self, rail: &mut ActionRail, index: Option<usize>, rect: Option<Rect>) {
        match self.ctx {
            Some(_) => rail.set_focus(index, rect.unwrap_or(Rect::EMPTY), self.damage),
            None => rail.adopt_focus(index),
        }
    }
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
/// — the shared chrome, the section on show and its own rows, cards and
/// meters, the scroll offsets, hover and press highlights, the focus rings,
/// the open Group popup, and the in-flight rename.
///
/// Two things deliberately do not. The last pointer coordinate is pure
/// hit-testing input that no render path reads, so a sample that crosses no
/// control does not force a repaint of an unchanged surface; the exclusion
/// lives in the field's type — a crate-internal wrapper that always compares
/// equal. And the *five* sections that are not on show draw nothing, so their
/// contents cannot change the picture: only the section [`section`] names
/// takes part, and which section that is is compared first, so a section
/// switch still compares unequal and still repaints. Comparing all six would
/// repaint the whole window every time any hidden section's readings moved —
/// which, with a per-frame compositor reading among them, is every frame.
///
/// The relation is still conservative in the safe direction only. Unequal
/// compositions *may* draw identically (a focus index that moves while focus
/// rests elsewhere), which costs one needless repaint; equal ones never differ
/// on screen. `PartialEq` is written out rather than derived, so it
/// destructures `Self` exhaustively: a field added later fails to compile
/// until it is either compared or deliberately excluded here.
///
/// [`section`]: Switchboard::section
#[derive(Clone, Debug, Eq)]
pub struct Switchboard {
    /// The navigation rail: every subject the surface can show — the task
    /// list, each resource device, and the recovery list — in one grouped
    /// list, each entry carrying its own reading and trace.
    rail: Tabs,
    /// The first rail subject the rail's window shows, so a machine with more
    /// subjects than the column seats scrolls rather than drawing past itself.
    rail_offset: usize,
    /// The subjects the rail listed when it was last built, so a chosen entry
    /// names the subject the reader actually pressed rather than whatever the
    /// next sample put in that row.
    rail_subjects: Vec<RailSubject>,
    scroll: ScrollBar,
    /// The three sections, each owning its own view models, controls, cursor
    /// and overlays. The screen reaches the one on show through
    /// [`active`](Self::active)/[`active_mut`](Self::active_mut), never by
    /// naming a section's own state here.
    tasks: TasksSection,
    resources: ResourcesSection,
    recovery: RecoverySection,
    section: Section,
    offsets: [u64; Section::ALL.len()],
    focus: FocusRegion,
    /// The last pointer position, kept so a press can be resolved against the
    /// coordinate the pointer actually reached — hit-testing input, never a
    /// drawn property.
    pointer: RenderInvariant<Point>,
}

impl PartialEq for Switchboard {
    fn eq(&self, other: &Self) -> bool {
        let Self {
            rail,
            rail_offset,
            rail_subjects,
            scroll,
            tasks,
            resources,
            recovery,
            section,
            offsets,
            focus,
            pointer,
        } = self;
        *section == other.section
            && *rail == other.rail
            && *rail_offset == other.rail_offset
            && *rail_subjects == other.rail_subjects
            && *scroll == other.scroll
            && *offsets == other.offsets
            && *focus == other.focus
            && *pointer == other.pointer
            && match section {
                Section::Tasks => *tasks == other.tasks,
                Section::Resources => *resources == other.resources,
                Section::Recovery => *recovery == other.recovery,
            }
    }
}

impl Switchboard {
    /// Build a Switchboard from a typed model, turning each view model into
    /// its shared control.
    ///
    /// It opens on [`Section::Resources`] with the processor selected — what
    /// this machine is doing is the question a monitor is opened to answer. A
    /// host that wants another section calls
    /// [`select_section`](Switchboard::select_section), and one that samples
    /// live state feeds each new reading to
    /// [`set_model`](Switchboard::set_model) rather than building again.
    #[must_use]
    pub fn new(model: &SwitchboardModel) -> Self {
        let mut switchboard = Self {
            rail: Tabs::new(Vec::new()).with_orientation(TabsOrientation::Vertical),
            rail_offset: 0,
            rail_subjects: Vec::new(),
            scroll: ScrollBar::new(
                ScrollOrientation::Vertical,
                ScrollModel::new(ScrollRange::EMPTY, 1, 1),
            ),
            tasks: TasksSection::new(),
            resources: ResourcesSection::new(),
            recovery: RecoverySection::new(),
            section: Section::Resources,
            offsets: [0; Section::ALL.len()],
            focus: FocusRegion::Content,
            pointer: RenderInvariant::new(Point::ORIGIN),
        };
        switchboard.adopt(model, &mut Sweep::adopting(&mut damage::sink()));
        switchboard
    }

    /// Adopt `model` with no frame to report against, for a window whose
    /// pixels the session has released: nothing partial can stand on a region
    /// that holds none of them, so the host draws the client whole instead of
    /// resolving rectangles against geometry the window does not have.
    pub fn adopt_unshown(&mut self, model: &SwitchboardModel) {
        self.adopt(model, &mut Sweep::adopting(&mut damage::sink()));
        self.set_scroll_range(
            self.active().item_count(),
            self.scroll.model().range().viewport_extent(),
        );
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
    /// an open section list, and any scroll-thumb drag in flight.
    ///
    /// **Kept, because the pointer has not moved:** the hover highlight. A row's
    /// rectangle belongs to its slot and a refresh does not move the slots, so
    /// the pointer really is still over the control at the same slot; leaving
    /// the re-derived controls resting would publish the opposite and would
    /// publish it afresh every sample. Every section carries it over the one
    /// shared way: a control the refresh derived unchanged is kept whole, and
    /// one it changed takes the hover alone.
    ///
    /// **Dropped, because it names a row that may now be a different object:**
    /// row selection and any half-finished press. A row index survives a
    /// refresh only as a *position* in the list, never as an identity, so a
    /// press begun on one task can never complete against whatever task now
    /// occupies that slot.
    ///
    /// The list position the keyboard focus names is clamped into the new
    /// content, and the active section's scroll offset is re-ranged through
    /// the same clamp a section switch uses, so a list that shrank leaves
    /// neither past its end. An emptied section leaves a valid, renderable
    /// state with nothing to activate.
    ///
    /// The reading is adopted against the frame the composition will next be
    /// drawn in, so the section on show reports the instruments and cells that
    /// actually moved into `damage` and the host presents those instead of the
    /// client. The band's own summary is the host's to report, because the band
    /// is shared chrome rather than any section's region.
    pub fn set_model(
        &mut self,
        model: &SwitchboardModel,
        bounds: Rect,
        scale: Scale,
        theme: &Theme,
        font: BitmapFont,
        damage: &mut Region,
    ) {
        let layout = Self::compute_layout(bounds, scale, theme);
        let ctx = self.section_ctx(&layout, bounds, scale, theme, font);
        let was = self.active().item_count();
        self.adopt(model, &mut Sweep::reporting(ctx, damage));
        // A sample that added or removed an item moved the thumb, and the bar
        // is no section's region to report.
        self.report_scroll_range(was, bounds, scale, theme, damage);
        self.set_scroll_range(
            self.active().item_count(),
            self.scroll.model().range().viewport_extent(),
        );
    }

    /// Derive every model-shaped part of the composition from `model` — every
    /// section's own controls — then re-assert the keyboard focus onto the
    /// controls that replaced the old ones.
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
    ///
    /// Only the section on show is handed `sweep`'s frame: the other two draw
    /// no pixel, so a rectangle resolved against a frame that is not theirs
    /// would name someone else's region.
    fn adopt(&mut self, model: &SwitchboardModel, sweep: &mut Sweep<'_, '_>) {
        let shown = self.section;
        for section in Section::ALL {
            match sweep.ctx().filter(|_| section == shown) {
                Some(ctx) => self
                    .section_mut(section)
                    .adopt(model, &mut Sweep::reporting(ctx, sweep.damage)),
                None => self
                    .section_mut(section)
                    .adopt(model, &mut Sweep::adopting(&mut damage::sink())),
            }
        }
        // Rebuilt from the adopted model, after the sections, so a subject that
        // appeared or went away moves the rail's entries with it. Restated
        // rather than replaced: the strip holds where the pointer is, which
        // entry it rests on and which one a press is waiting for, and a fresh
        // strip would know none of them — so a sample landing between a
        // reader's motion and their press would swallow the click and drop the
        // lift from under the pointer.
        self.rail_subjects = Self::subjects(model);
        let rebuilt = self.build_rail(model);
        let rail_moved = self.rail.restate(rebuilt);
        if rail_moved {
            if let Some(ctx) = sweep.ctx() {
                let layout = Self::compute_layout(ctx.bounds, ctx.scale, ctx.theme);
                sweep.damage.add(layout.rail);
            }
        }
        match sweep.ctx() {
            Some(ctx) => self.apply_focus_marks(&mut Sweep::reporting(ctx, sweep.damage)),
            None => self.apply_focus_marks(&mut Sweep::adopting(&mut damage::sink())),
        }
    }

    /// Every subject the rail lists, in rail order: what is running, then the
    /// machine's devices, then what broke.
    ///
    /// Derived from the model rather than held, so a device that appears or
    /// goes away between samples moves the rail's entries with it and no
    /// second list can disagree about what the surface can show.
    fn subjects(model: &SwitchboardModel) -> Vec<RailSubject> {
        let mut subjects = alloc::vec![RailSubject::Tasks];
        subjects.extend(
            model
                .resources
                .devices
                .iter()
                .map(|device| RailSubject::Device(device.id)),
        );
        subjects.push(RailSubject::Recovery);
        subjects
    }

    /// The rail's entries: a window of the subjects from
    /// [`rail_offset`](Self::rail_offset), each carrying its own reading, its
    /// group heading where it starts one, and its trace.
    fn build_rail(&self, model: &SwitchboardModel) -> Tabs {
        let mut tabs = Vec::new();
        let mut previous: Option<RailGroup> = None;
        for (index, subject) in Self::subjects(model).into_iter().enumerate() {
            let (name, group, reading, trace, kind) = match subject {
                RailSubject::Tasks => (
                    String::from("Tasks"),
                    RailGroup::Tasks,
                    alloc::format!("{}", model.tasks.len()),
                    &model.tasks_trend,
                    PressureKind::Cpu,
                ),
                RailSubject::Recovery => (
                    String::from("Recovery"),
                    RailGroup::Recovery,
                    alloc::format!("{}", model.recovery.len()),
                    &model.recovery_trend,
                    PressureKind::Thermal,
                ),
                RailSubject::Device(id) => {
                    let Some(device) = model.resources.devices.iter().find(|d| d.id == id) else {
                        continue;
                    };
                    let mut tab = Tab::new(device.name.clone())
                        .with_reading(reading::reading_text(&device.reading));
                    if previous != Some(device.group) {
                        tab = tab.with_group(device.group.heading());
                    }
                    previous = Some(device.group);
                    if !device.trend.is_empty() {
                        tab = tab.with_trend(
                            Chart::new(device.kind).with_samples(device.trend.iter().copied()),
                        );
                    }
                    if index >= self.rail_offset {
                        tabs.push(tab);
                    }
                    continue;
                }
            };
            let mut tab = Tab::new(name).with_reading(reading);
            if previous != Some(group) {
                tab = tab.with_group(group.heading());
            }
            previous = Some(group);
            if !trace.points.is_empty() {
                tab = tab.with_trend(
                    Chart::new(kind)
                        .with_samples(trace.points.iter().copied())
                        .with_full_scale(trace.full_scale),
                );
            }
            if index >= self.rail_offset {
                tabs.push(tab);
            }
        }
        let mut rail = Tabs::new(tabs)
            .with_orientation(TabsOrientation::Vertical)
            .with_absences(self.rail_absences(model));
        if let Some(position) = self
            .selected_position(model)
            .and_then(|index| index.checked_sub(self.rail_offset))
        {
            rail.adopt_selected(position);
        }
        rail
    }

    /// The empty groups the rail states, in rail order.
    ///
    /// Only `Storage` and `Network` can be empty: the task list, the
    /// processor, the machine's memory, the display path and the machine's
    /// own facts always answer, and so does recovery. Each is stated whether
    /// the query was refused or simply found nothing — the two read
    /// differently, and silence reads as neither.
    fn rail_absences(&self, model: &SwitchboardModel) -> Vec<TabGroupAbsence> {
        let report = &model.resources;
        [
            (RailGroup::Storage, report.storage_absent, "storage device"),
            (
                RailGroup::Network,
                report.interfaces_absent,
                "managed interface",
            ),
        ]
        .into_iter()
        .filter(|(group, _, _)| !report.devices.iter().any(|device| device.group == *group))
        .map(|(group, refusal, subject)| {
            let statement = match refusal {
                Some(reason) => reading::absence_statement(subject, reason),
                None => alloc::format!("No {subject} is present."),
            };
            TabGroupAbsence::new(group.heading(), statement, self.group_start(model, group))
        })
        .collect()
    }

    /// The rail position an empty `group` would have started at, within the
    /// window from [`rail_offset`](Self::rail_offset): before the first seated
    /// subject of a later group, or last where no later group has one.
    fn group_start(&self, model: &SwitchboardModel, group: RailGroup) -> usize {
        let seated: Vec<RailGroup> = Self::subjects(model)
            .into_iter()
            .skip(self.rail_offset)
            .map(|subject| match subject {
                RailSubject::Tasks => RailGroup::Tasks,
                RailSubject::Recovery => RailGroup::Recovery,
                RailSubject::Device(id) => model
                    .resources
                    .devices
                    .iter()
                    .find(|d| d.id == id)
                    .map_or(RailGroup::Machine, |d| d.group),
            })
            .collect();
        seated
            .iter()
            .position(|seated| *seated > group)
            .unwrap_or(seated.len())
    }

    /// Move the rail's lit entry to the subject now on show.
    ///
    /// Read from the subjects the rail was last built over rather than from a
    /// model, because a section change carries none: without this the rail
    /// would keep lighting the previous subject until the next sample rebuilt
    /// it, which is a whole sampling interval of pointing at the wrong pane.
    fn mark_rail_selection(&mut self) {
        let shown = match self.section {
            Section::Tasks => Some(RailSubject::Tasks),
            Section::Recovery => Some(RailSubject::Recovery),
            Section::Resources => self.resources.selected.map(RailSubject::Device),
        };
        let position = shown
            .and_then(|shown| self.rail_subjects.iter().position(|s| *s == shown))
            .and_then(|index| index.checked_sub(self.rail_offset));
        if let Some(index) = position {
            self.rail.adopt_selected(index);
        }
    }

    /// Where the subject on show sits in the rail's whole list.
    fn selected_position(&self, model: &SwitchboardModel) -> Option<usize> {
        let shown = self.shown_subject(model)?;
        Self::subjects(model)
            .into_iter()
            .position(|subject| subject == shown)
    }

    /// The subject the surface is showing: the section, and for Resources the
    /// device its pane is drawn from.
    fn shown_subject(&self, model: &SwitchboardModel) -> Option<RailSubject> {
        match self.section {
            Section::Tasks => Some(RailSubject::Tasks),
            Section::Recovery => Some(RailSubject::Recovery),
            Section::Resources => self
                .resources
                .selected
                .or_else(|| model.resources.devices.first().map(|device| device.id))
                .map(RailSubject::Device),
        }
    }

    /// Show the rail entry at `index`, switching section and — for a device —
    /// the pane beside it, through the one section transition every other
    /// route runs.
    ///
    /// An out-of-range entry changes nothing (fail closed).
    fn select_rail_entry(
        &mut self,
        index: usize,
        ctx: SectionCtx<'_>,
        damage: &mut Region,
    ) -> Option<SwitchboardAction> {
        let subject = *self
            .rail_subjects
            .get(index.saturating_add(self.rail_offset))?;
        let mut sweep = Sweep::reporting(ctx, damage);
        if let RailSubject::Device(id) = subject {
            self.resources.select_device(id, &mut sweep);
        }
        let action = self.select_section_index(subject.section().index(), &mut sweep);
        self.mark_rail_selection();
        sweep.client();
        action
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
        self.select_section_index(section.index(), &mut Sweep::adopting(&mut damage::sink()))
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
}
/// The laid-out regions of a Switchboard for one outer bounds.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
struct SbLayout {
    /// The navigation rail down the leading edge: every subject the surface
    /// can show, in one list.
    rail: Rect,
    /// The section content area (excludes the rail and the scrollbar gutter).
    content: Rect,
    /// The vertical scrollbar track.
    scroll: Rect,
}

/// The scrollable list of the active section: where it draws, one item's
/// height, and how many items it holds.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
struct ListInfo {
    /// The rectangle the item list occupies.
    pub(super) list_rect: Rect,
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
    /// The window manager carves out the client area server-side, so the
    /// bounds handed in are the client itself. The rail claims its width down
    /// the leading edge and the content and scrollbar take what is left,
    /// clipped so a window too small for the full anatomy still lays out in
    /// bounds (fail closed, never negative or overlapping).
    ///
    /// The rail is carved here rather than inside a section's frame because it
    /// is the only route between sections: a drop order that could shed it
    /// would strand the reader wherever they happened to be.
    fn compute_layout(bounds: Rect, scale: Scale, theme: &Theme) -> SbLayout {
        let client = bounds;

        let rail_w = scale.scale_length(RAIL_WIDTH).min(client.width);
        let rail = Rect::new(client.left(), client.top(), rail_w, client.height);

        let beside_left = client.left() + to_i32(rail_w);
        let beside_w = client.width.saturating_sub(rail_w);
        let gutter = scale
            .scale_length(theme.metrics().scrollbar_breadth)
            .max(1)
            .min(beside_w);
        let content_w = beside_w.saturating_sub(gutter);
        let content = Rect::new(beside_left, client.top(), content_w, client.height);

        let gutter_left = beside_left + to_i32(content_w);
        let scroll = Rect::new(gutter_left, client.top(), gutter, client.height);

        SbLayout {
            rail,
            content,
            scroll,
        }
    }

    /// Lay the theme's base surface tint over the whole client area.
    fn fill_client(surface: &mut Surface, bounds: Rect, theme: &Theme) {
        let (Ok(x), Ok(y)) = (u32::try_from(bounds.left()), u32::try_from(bounds.top())) else {
            return;
        };
        surface.fill_rect(
            x,
            y,
            bounds.width,
            bounds.height,
            Color::from(theme.palette().surface),
        );
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
            Section::Resources => &self.resources,
            Section::Recovery => &self.recovery,
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
            Section::Resources => &mut self.resources,
            Section::Recovery => &mut self.recovery,
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
            scale,
            theme,
            font,
        }
    }

    /// The scrollable list metrics for the active section.
    fn list_info(&self, layout: &SbLayout, scale: Scale, theme: &Theme) -> ListInfo {
        let frame = self.section_frame(layout, scale, theme);
        self.active().list_info(&frame, scale, theme)
    }

    /// The active section's resolved regions for this layout.
    fn section_frame(&self, layout: &SbLayout, scale: Scale, theme: &Theme) -> SectionFrame {
        resolve_section_frame(layout.content, self.active().anatomy(), scale, theme)
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
        let layout = Self::compute_layout(bounds, scale, theme);
        let frame = self.section_frame(&layout, scale, theme);
        self.active_mut().relayout(&frame, scale, theme);
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
        artwork: &mut dyn IconArtwork,
    ) {
        self.sync_scroll(bounds, scale, theme);
        let layout = Self::compute_layout(bounds, scale, theme);
        let ctx = self.section_ctx(&layout, bounds, scale, theme, font);

        // The window manager decorates the window, but its content pixels are
        // the client's own: without laying the surface tint down first, every
        // pixel no control covers keeps whatever the shared frame region held
        // before, which reads as a transparent window.
        Self::fill_client(surface, bounds, theme);
        self.rail.render(surface, layout.rail, scale, theme);
        self.render_section(surface, ctx, artwork);

        // The scrollbar, drawn after the content so its thumb sits above it.
        self.scroll.render(surface, layout.scroll, scale, theme);

        // The section's own overlay last of all, so it sits above every other
        // region including the scrollbar.
        self.active().render_overlay(surface, ctx, artwork);
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
        ctx: SectionCtx<'_>,
        artwork: &mut dyn IconArtwork,
    ) {
        let section = self.active();
        section.render(surface, ctx, artwork);
        let info = section.list_info(&ctx.frame, ctx.scale, ctx.theme);
        if let Some(column) = Self::action_column(info, section.row_buttons(), ctx.scale, ctx.theme)
        {
            let scrolled = self.offsets[self.section.index()] != 0;
            ActionRail::new(Vec::new())
                .with_edge_wake(scrolled)
                .render(surface, column, ctx.scale, ctx.theme);
        }
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
        damage: &mut Region,
    ) -> Option<SwitchboardAction> {
        let was = self.active().item_count();
        let action = self.route_pointer(event, bounds, scale, theme, font, damage);
        self.report_scroll_range(was, bounds, scale, theme, damage);
        action
    }

    /// Route one pointer event to whichever region owns it.
    ///
    /// The scrollbar is reported by [`on_pointer`](Self::on_pointer) once this
    /// has returned, so every route through here may change how many items the
    /// section holds without each one having to account for a bar that is not
    /// its own.
    fn route_pointer(
        &mut self,
        event: &InputEvent,
        bounds: Rect,
        scale: Scale,
        theme: &Theme,
        font: BitmapFont,
        damage: &mut Region,
    ) -> Option<SwitchboardAction> {
        if let InputEvent::PointerMoved { to } = event {
            *self.pointer = *to;
        }

        // An open popup is modal over the rest of the composition: every event
        // routes to it first, and a primary press outside its bounds dismisses
        // it rather than falling through to whatever sits beneath.
        if self.active().holds_pointer() {
            let layout = Self::compute_layout(bounds, scale, theme);
            let ctx = self.section_ctx(&layout, bounds, scale, theme, font);
            let outcome = self.active_mut().overlay_on_pointer(event, ctx, damage);
            return outcome.and_then(|outcome| self.resolve_outcome(outcome, ctx, damage));
        }
        self.sync_scroll(bounds, scale, theme);
        let layout = Self::compute_layout(bounds, scale, theme);

        // The mouse wheel scrolls the active section (spec §17 / no deferral).
        if let InputEvent::PointerScrolled { dx, dy } = event {
            if let Some(ScrollAction::ScrollTo { offset }) =
                self.scroll.wheel(*dx, *dy, layout.scroll, damage)
            {
                self.scrolled_to(offset, layout.content, damage);
                return Some(SwitchboardAction::Scrolled { offset });
            }
            return None;
        }

        // The scrollbar.
        if let Some(ScrollAction::ScrollTo { offset }) =
            self.scroll
                .on_pointer(event, layout.scroll, scale, theme, damage)
        {
            self.scrolled_to(offset, layout.content, damage);
            return Some(SwitchboardAction::Scrolled { offset });
        }

        // The navigation rail: choosing a subject is what switches section.
        if let Some(TabsAction::Selected { index }) =
            self.rail
                .on_pointer(event, layout.rail, scale, theme, damage)
        {
            let ctx = self.section_ctx(&layout, bounds, scale, theme, font);
            return self.select_rail_entry(index, ctx, damage);
        }

        // The active section's content.
        let ctx = self.section_ctx(&layout, bounds, scale, theme, font);
        let outcome = self.active_mut().on_pointer(event, ctx, damage);
        outcome.and_then(|outcome| self.resolve_outcome(outcome, ctx, damage))
    }

    /// Turn what a section reported into the action a host sees, running the
    /// composition-wide transitions a section may ask for but never perform
    /// itself.
    fn resolve_outcome(
        &mut self,
        outcome: SectionOutcome,
        ctx: SectionCtx<'_>,
        damage: &mut Region,
    ) -> Option<SwitchboardAction> {
        match outcome {
            SectionOutcome::Action(action) => Some(action),
            SectionOutcome::ShowTasksBy { column } => {
                let action = self.show_tasks(ctx, damage);
                self.tasks.sort_by_cost(column, ctx, damage);
                action
            }
        }
    }

    /// Show [`Section::Tasks`] with its first row focused, and report the
    /// transition.
    ///
    /// This runs the one section transition and the one focus arithmetic
    /// every other route runs, so a resource pane's "sort tasks by" command
    /// cannot leave the trail, the content and the offsets disagreeing.
    fn show_tasks(
        &mut self,
        ctx: SectionCtx<'_>,
        damage: &mut Region,
    ) -> Option<SwitchboardAction> {
        let mut sweep = Sweep::reporting(ctx, damage);
        let action = self.select_section_index(Section::Tasks.index(), &mut sweep);
        let focus = self.tasks.focus_index_for_row(0);
        self.tasks.set_content_focus(focus, &mut sweep);
        self.tasks.set_row_action(0, &mut sweep);
        self.ensure_focus_visible(&mut sweep);
        self.apply_focus_marks(&mut sweep);
        action
    }

    /// Open the section list on the section currently shown, so the reader
    /// starts from where they are, reporting the pixels it will cover.
    ///
    /// Report the scrollbar when a round changed *how many* items the section
    /// holds, `was` being the count it started with.
    ///
    /// Selecting a device whose pane is a different length, or a sample that
    /// added or removed a row, moves the thumb — and the controls the round
    /// routed through know nothing about a bar that is not theirs. The bar is
    /// re-ranged by the next paint, which is too late to report but exactly in
    /// time to be drawn: the present renders inside the reported rectangle, so
    /// naming it here is the whole of what the round owes. The count either
    /// side of the round is what decides it, never the range the bar happens
    /// to be carrying, which no round is responsible for having synced.
    fn report_scroll_range(
        &self,
        was: usize,
        bounds: Rect,
        scale: Scale,
        theme: &Theme,
        damage: &mut Region,
    ) {
        if self.active().item_count() != was {
            damage.add(Self::compute_layout(bounds, scale, theme).scroll);
        }
    }

    /// Adopt `offset` as the active section's scroll offset, reporting
    /// `content` when it moved: every item in the list is drawn somewhere new,
    /// which is more than the scrollbar's own report describes.
    fn scrolled_to(&mut self, offset: u64, content: Rect, damage: &mut Region) {
        if self.offsets[self.section.index()] == offset {
            return;
        }
        self.offsets[self.section.index()] = offset;
        damage.add(content);
    }

    /// Feed one key event, returning the typed action it produced (if any).
    ///
    /// The active section's own overlay — an in-flight inline edit or an open
    /// popup — takes every key first: it is modal over the composition, so no
    /// key reaches the regions beneath it until it commits, cancels, or
    /// dismisses. Otherwise Tab cycles keyboard focus between the navigation
    /// rail, the content list, and the scrollbar; keys are then routed to the
    /// focused region's control.
    ///
    /// Subjects are reachable without a pointer: with focus on the rail,
    /// Up/Down walk its entries and Home/End jump to either end. On a rail the
    /// cursor is the choice, so walking onto an entry shows its pane.
    pub fn on_key(
        &mut self,
        key: Key,
        bounds: Rect,
        scale: Scale,
        theme: &Theme,
        font: BitmapFont,
        damage: &mut Region,
    ) -> Option<SwitchboardAction> {
        let was = self.active().item_count();
        let action = self.route_key(key, bounds, scale, theme, font, damage);
        self.report_scroll_range(was, bounds, scale, theme, damage);
        action
    }

    /// Route one key to whichever region holds the keyboard, on the same
    /// terms as [`route_pointer`](Self::route_pointer) — the scrollbar is
    /// [`on_key`](Self::on_key)'s to report.
    fn route_key(
        &mut self,
        key: Key,
        bounds: Rect,
        scale: Scale,
        theme: &Theme,
        font: BitmapFont,
        damage: &mut Region,
    ) -> Option<SwitchboardAction> {
        let layout = Self::compute_layout(bounds, scale, theme);
        let ctx = self.section_ctx(&layout, bounds, scale, theme, font);

        if self.active().holds_keyboard() {
            let outcome = self.active_mut().overlay_on_key(key, ctx, damage);
            return outcome.and_then(|outcome| self.resolve_outcome(outcome, ctx, damage));
        }
        if key == Key::Named(NamedKey::Tab) {
            self.focus = self.focus.next();
            self.apply_focus_marks(&mut Sweep::reporting(ctx, damage));
            return None;
        }
        match self.focus {
            // The rail is the keyboard's route between subjects: moving its
            // cursor and committing a choice are the control's own keys.
            FocusRegion::Rail => {
                let was = self.rail.current();
                if let Some(TabsAction::Selected { index }) =
                    self.rail.on_key(key, layout.rail, scale, theme, damage)
                {
                    return self.select_rail_entry(index, ctx, damage);
                }
                // Moving the cursor *is* choosing: a rail entry names the pane
                // the reader is reading, so browsing the rail shows what it
                // names rather than waiting for a second key to confirm.
                match self.rail.current() {
                    Some(index) if Some(index) != was => self.select_rail_entry(index, ctx, damage),
                    _ => None,
                }
            }
            FocusRegion::Scrollbar => match self.scroll.on_key(key, layout.scroll, damage) {
                Some(ScrollAction::ScrollTo { offset }) => {
                    self.scrolled_to(offset, layout.content, damage);
                    Some(SwitchboardAction::Scrolled { offset })
                }
                None => None,
            },
            FocusRegion::Content => self.content_on_key(key, ctx, damage),
        }
    }

    /// Route a key to the focused content item: Up/Down move the row focus
    /// (resetting the action focus to the row's first button), Left/Right
    /// move the action focus along the row's buttons, and Enter/Space
    /// activate the action-focused button.
    fn content_on_key(
        &mut self,
        key: Key,
        ctx: SectionCtx<'_>,
        damage: &mut Region,
    ) -> Option<SwitchboardAction> {
        let count = self.active().focus_span();
        if count == 0 {
            return None;
        }
        match key {
            Key::Named(NamedKey::Down) => {
                let next = (self.active().content_focus() + 1).min(count - 1);
                self.move_content_focus(next, ctx, damage);
                None
            }
            Key::Named(NamedKey::Up) => {
                let next = self.active().content_focus().saturating_sub(1);
                self.move_content_focus(next, ctx, damage);
                None
            }
            Key::Named(NamedKey::Right) => {
                let last = self.active().focused_action_count().saturating_sub(1);
                let next = (self.active().row_action() + 1).min(last);
                self.move_row_action(next, ctx, damage);
                None
            }
            Key::Named(NamedKey::Left) => {
                let next = self.active().row_action().saturating_sub(1);
                self.move_row_action(next, ctx, damage);
                None
            }
            _ => {
                let outcome = self.active_mut().activate_focused(key, ctx, damage)?;
                self.resolve_outcome(outcome, ctx, damage)
            }
        }
    }

    /// Put the within-row action cursor on `index` of the active section and
    /// re-apply the focus marks.
    fn move_row_action(&mut self, index: usize, ctx: SectionCtx<'_>, damage: &mut Region) {
        let mut sweep = Sweep::reporting(ctx, damage);
        self.active_mut().set_row_action(index, &mut sweep);
        self.apply_focus_marks(&mut sweep);
    }

    /// Put the content cursor on `index` of the active section: the action
    /// cursor returns to the item's first action, the item is scrolled into
    /// view, and the focus marks are re-applied.
    ///
    /// Both arrow keys move the cursor through here, so "keep the focused item
    /// visible" is one definition rather than one per direction or per
    /// section.
    fn move_content_focus(&mut self, index: usize, ctx: SectionCtx<'_>, damage: &mut Region) {
        let mut sweep = Sweep::reporting(ctx, damage);
        self.active_mut().set_content_focus(index, &mut sweep);
        self.active_mut().set_row_action(0, &mut sweep);
        self.ensure_focus_visible(&mut sweep);
        self.apply_focus_marks(&mut sweep);
    }

    /// The one section transition: every path that changes the shown section —
    /// the section list, the keyboard, and
    /// [`select_section`](Switchboard::select_section) — runs this, so the
    /// location trail, the content, and the per-section scroll offset can never
    /// disagree.
    ///
    /// It shows the section and puts keyboard focus back on its first item;
    /// the offset stays each section's own and is re-clamped against the new
    /// content by the next scroll sync. Re-selecting the shown section is a
    /// no-op, and an out-of-range index changes nothing (fail closed); both
    /// report no change.
    fn select_section_index(
        &mut self,
        index: usize,
        sweep: &mut Sweep<'_, '_>,
    ) -> Option<SwitchboardAction> {
        let section = Section::from_index(index)?;
        if section == self.section {
            return None;
        }
        // Every overlay names the section that opened it — a popup anchors on
        // one of its rows, an inline edit sits in one of them — so a section
        // change drops them rather than leaving one standing over, lying
        // about, or still taking keys for content the reader has navigated
        // away from.
        self.active_mut().dismiss_overlay();
        self.section = section;
        self.mark_rail_selection();
        self.active_mut().set_content_focus(0, sweep);
        self.active_mut().set_row_action(0, sweep);
        self.apply_focus_marks(sweep);
        sweep.client();
        Some(SwitchboardAction::SectionChanged { section })
    }

    /// Nudge the active section's offset so the focused content item stays
    /// visible, using the last-synced viewport extent, reporting the whole
    /// client through `sweep` when it moved: every item is then drawn
    /// somewhere new.
    fn ensure_focus_visible(&mut self, sweep: &mut Sweep<'_, '_>) {
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
        let was = self.offsets[self.section.index()];
        let mut offset = was;
        if idx < offset {
            offset = idx;
        } else if idx >= offset + viewport {
            offset = idx + 1 - viewport;
        }
        self.scroll.set_model(self.scroll.model().scroll_to(offset));
        let now = self.scroll.model().offset();
        self.offsets[self.section.index()] = now;
        if now != was {
            sweep.client();
        }
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
    fn apply_focus_marks(&mut self, sweep: &mut Sweep<'_, '_>) {
        // The rail shows its keyboard cursor only while it holds focus: the
        // selected subject stays lit either way, so an unfocused rail states
        // where the reader is without also claiming their keys.
        let cursor = (self.focus == FocusRegion::Rail)
            .then(|| self.rail.selected().unwrap_or(0))
            .or(None);
        match sweep.ctx {
            Some(ctx) => {
                let layout = Self::compute_layout(ctx.bounds, ctx.scale, ctx.theme);
                self.rail
                    .set_current(cursor, layout.rail, ctx.scale, ctx.theme, sweep.damage);
            }
            None => self.rail.adopt_current(cursor),
        }
        self.scroll
            .set_focused(self.focus == FocusRegion::Scrollbar);

        // Every section is told, so the one on show lights its focused item
        // and the five behind it are cleared rather than left glowing under
        // content nobody is looking at. Only the one on show is drawn, so only
        // its marks have a rectangle to report.
        let content = self.focus == FocusRegion::Content;
        let active = self.section;
        for section in Section::ALL {
            if section == active {
                self.section_mut(section).apply_focus_marks(content, sweep);
            } else {
                self.section_mut(section)
                    .apply_focus_marks(false, &mut Sweep::adopting(&mut damage::sink()));
            }
        }
    }
}
