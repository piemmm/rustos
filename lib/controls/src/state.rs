//! The typed control-state vocabulary.
//!
//! Reactive Alloy models a control as a small set of *composed* typed
//! fields, never one giant enum and never an unstructured key/value bag. A
//! disabled destructive recovery button and a focused non-destructive
//! primary button are different *combinations* of small typed states, not
//! unrelated code paths. This module is the one definition of that
//! vocabulary, shared by every control renderer and by the window manager's
//! furniture.
//!
//! # What lives here
//!
//! - [`ControlKind`] and [`ControlRole`] — what a control *is* and the
//!   intent it carries.
//! - [`ControlState`] — the composed run-time state of one control, built
//!   from [`FocusState`], [`PointerState`], [`SelectionState`],
//!   [`ValidationState`], [`AuthorityState`], [`ActivityState`],
//!   [`PressureState`], and [`RecoveryState`].
//! - [`ControlDisposition`] — the *derived* authority/interaction taxonomy a
//!   renderer switches on, so a permission denial is never collapsed into a
//!   plain disabled look.
//! - The window-furniture states ([`WindowControlKind`],
//!   [`WindowActivationState`], [`WindowSizeState`], [`WindowFurnitureState`],
//!   and [`SizeAction`]) the window manager paints its frame from.
//!
//! The scroll-range vocabulary the state model refers to
//! ([`ScrollOrientation`](crate::ScrollOrientation),
//! [`ScrollRange`](crate::ScrollRange), [`ScrollModel`](crate::ScrollModel))
//! already lives in [`crate::scroll`]; it is not restated here.
//!
//! # Illegal states are unrepresentable
//!
//! Mutually exclusive facts are enums (a control is hovered *or* pressed,
//! never both); orthogonal facts are separate fields. Known progress carries
//! a validated [`ProgressValue`] that can never exceed full, so a renderer
//! never has to defend against an out-of-range percentage.

/// What a control fundamentally is.
///
/// The kind selects a control's anatomy and default behaviour; its live
/// appearance still comes from the composed [`ControlState`] and the active
/// theme, not from the kind alone.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub enum ControlKind {
    /// A labelled action plate.
    Button,
    /// An action plate whose content is a single glyph.
    IconButton,
    /// A primary action region plus a disclosure region sharing one plate.
    SplitButton,
    /// A two-state powered contact.
    Toggle,
    /// A boolean selector with a shape mark (checked / mixed).
    Checkbox,
    /// A one-of-many selector with a centre bead.
    Radio,
    /// A measured value control with a rail, track, and thumb.
    Slider,
    /// An instrument trace of known or indeterminate work.
    Progress,
    /// A single-line text entry.
    TextField,
    /// A text entry specialised for queries.
    SearchField,
    /// A field plus a disclosure over a choice list.
    ComboBox,
    /// One row of a menu.
    MenuItem,
    /// One tab in a tab strip.
    Tab,
    /// One selectable/inspectable row of a list or table.
    ListRow,
    /// One cell of a table.
    TableCell,
    /// A grouped state-and-actions surface.
    Card,
    /// A stable-layout container.
    Panel,
    /// One action within a decision dialog.
    DialogAction,
    /// The window-manager-owned boundary around a client viewport.
    WindowFrame,
    /// The window-manager-owned title bar.
    TitleBar,
    /// One window-command furniture button (see [`WindowControlKind`]).
    WindowControl,
    /// The explicit corner resize affordance.
    ResizeGrabber,
    /// A scrollbar in either orientation.
    ScrollBar,
    /// A taskbar entry for one application/window.
    TaskbarItem,
    /// A compact live status capsule in the notification area.
    TraySignal,
    /// A card-shaped transient message.
    Notification,
}

/// The intent a control carries, which drives its default emphasis.
///
/// A role never grants authority: a [`ControlRole::Primary`]
/// or [`ControlRole::Recommended`] action can still be refused by the backing
/// service after activation.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub enum ControlRole {
    /// An ordinary action with no special emphasis.
    Neutral,
    /// The main action of its surface.
    Primary,
    /// The safe action the model recommends (Action Warmth).
    Recommended,
    /// An action that destroys data or is otherwise hard to undo.
    Destructive,
    /// An action that repairs, restarts, or recovers hung work.
    Recovery,
    /// An action that changes location or selection rather than state.
    Navigation,
    /// A system/session-level action (lock, shut down).
    System,
}

/// Whether a control holds keyboard focus and whether it is part of a
/// grouped focus field.
///
/// The two facts are orthogonal — a control can be focused, be a member of a
/// highlighted focus field, both, or neither — so they are separate booleans
/// rather than a four-way enum that would let one imply the other.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash, Default)]
pub struct FocusState {
    /// The control currently holds keyboard focus and draws a focus ring.
    pub focused: bool,
    /// The control belongs to a group whose Focus Field is highlighted.
    pub in_focus_field: bool,
}

impl FocusState {
    /// Neither focused nor within a highlighted focus field.
    pub const UNFOCUSED: Self = Self {
        focused: false,
        in_focus_field: false,
    };

    /// Holds keyboard focus (and therefore draws a focus ring).
    pub const FOCUSED: Self = Self {
        focused: true,
        in_focus_field: false,
    };
}

/// The pointer's relationship to a control.
///
/// These are mutually exclusive: a control cannot be both hovered and the
/// source of a drag at once, so they are a single enum.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash, Default)]
pub enum PointerState {
    /// The pointer is not over the control.
    #[default]
    None,
    /// The pointer is over the control.
    Hover,
    /// A pointer button is held down on the control.
    Pressed,
    /// The control is the source of an in-flight drag.
    DragSource,
    /// The control is a valid drop target for the in-flight drag.
    DragTarget,
}

/// Whether a control (typically a row, cell, or choice) is selected.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash, Default)]
pub enum SelectionState {
    /// Not selected.
    #[default]
    Unselected,
    /// Selected.
    Selected,
    /// A tri-state selection that is partially on (mixed children).
    Mixed,
    /// The current item within a set (the caret/cursor row), which may be
    /// distinct from being selected.
    Current,
}

/// The validation status of a control's value.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash, Default)]
pub enum ValidationState {
    /// The value is valid.
    #[default]
    Valid,
    /// The value is usable but carries a caution.
    Warning,
    /// The value is invalid and blocks the action.
    Invalid,
    /// The value is awaiting verification by a backing service.
    Pending,
}

/// Whether the caller may perform a control's action, and if not, why.
///
/// A denial is rendered distinctly from a plain disabled control
/// (spec §13): the control never silently collapses "you
/// lack authority" into "this is inactive". Security-sensitive reasons are
/// conveyed as concise user-facing text by the renderer, never as secrets or
/// capability tokens.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash, Default)]
pub enum AuthorityState {
    /// The action is permitted.
    #[default]
    Allowed,
    /// The action is possible but consequential and must be confirmed.
    NeedsConfirmation,
    /// The caller lacks a required capability.
    NeedsCapability,
    /// The action is refused by policy or authority.
    Denied,
    /// The action was attempted and the backing service refused it safely.
    FailedClosed,
}

/// A known-progress value as a validated fraction in permille (0..=1000).
///
/// Constructed through [`ProgressValue::new`], which clamps out-of-range
/// input, so a renderer can never receive a fraction beyond full and never
/// has to defend against one (fail closed).
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub struct ProgressValue {
    permille: u16,
}

impl ProgressValue {
    /// Full progress (1000 permille).
    pub const FULL: Self = Self { permille: 1000 };
    /// No progress (0 permille).
    pub const EMPTY: Self = Self { permille: 0 };

    /// A progress value in permille, clamped into `0..=1000`.
    #[must_use]
    pub const fn new(permille: u16) -> Self {
        Self {
            permille: if permille > 1000 { 1000 } else { permille },
        }
    }

    /// The value in permille (0..=1000).
    #[must_use]
    pub const fn permille(self) -> u16 {
        self.permille
    }

    /// Whether the value is full.
    #[must_use]
    pub const fn is_complete(self) -> bool {
        self.permille >= 1000
    }
}

/// What work a control (or its linked object) is doing.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash, Default)]
pub enum ActivityState {
    /// No work in progress.
    #[default]
    Idle,
    /// Work is in progress but its extent is not yet measurable.
    Working,
    /// Work is in progress with a known, measurable fraction complete.
    Progress(ProgressValue),
    /// Work is in progress with no measurable fraction (bounded moving
    /// trace); reduced motion renders it statically.
    Indeterminate,
    /// Work finished successfully.
    Complete,
}

/// Which resource a [`PressureState`] refers to.
///
/// Each kind maps to a distinct semantic signal role in the theme *and* a
/// distinct shape fallback (spec §15), so pressure is legible without colour.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub enum PressureKind {
    /// Compute saturation.
    Cpu,
    /// Memory pressure.
    Memory,
    /// Storage throughput.
    Disk,
    /// Network transfer / remote I/O.
    Network,
    /// Power / battery pressure.
    Power,
    /// Thermal pressure.
    Thermal,
}

/// Whether a control is under a resource pressure, and which.
///
/// A control surfaces at most one dominant pressure; a container that must
/// show several composes several controls, each with its own rail.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash, Default)]
pub enum PressureState {
    /// No pressure to indicate.
    #[default]
    None,
    /// Under the given resource pressure.
    Under(PressureKind),
}

/// The recovery posture of a control's linked object.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash, Default)]
pub enum RecoveryState {
    /// Nothing to recover.
    #[default]
    None,
    /// The object can be recovered by an ordinary action.
    Recoverable,
    /// The object is hung / not responding.
    Hung,
    /// A restart is recommended.
    RestartRecommended,
    /// Only a deliberate, high-impact force action remains.
    ForceAction,
}

/// The composed run-time state of one control.
///
/// Every field is a small typed state; the whole is assembled with the
/// builder methods rather than by naming a bespoke variant per combination.
/// A renderer reads the individual fields it cares about and derives its
/// overall interaction taxonomy from [`disposition`](ControlState::disposition).
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct ControlState {
    /// Whether the control is enabled at all. A disabled control performs no
    /// action regardless of its other fields.
    pub enabled: bool,
    /// Keyboard focus / focus-field membership.
    pub focus: FocusState,
    /// The pointer's relationship to the control.
    pub pointer: PointerState,
    /// Selection status.
    pub selection: SelectionState,
    /// Validation status of the control's value.
    pub validation: ValidationState,
    /// Whether the caller may act, and if not, why.
    pub authority: AuthorityState,
    /// What work the control or its linked object is doing.
    pub activity: ActivityState,
    /// Resource pressure to indicate, if any.
    pub pressure: PressureState,
    /// Recovery posture of the linked object.
    pub recovery: RecoveryState,
}

impl Default for ControlState {
    fn default() -> Self {
        Self::idle()
    }
}

impl ControlState {
    /// An enabled, idle, unfocused, allowed control — the resting state.
    #[must_use]
    pub const fn idle() -> Self {
        Self {
            enabled: true,
            focus: FocusState::UNFOCUSED,
            pointer: PointerState::None,
            selection: SelectionState::Unselected,
            validation: ValidationState::Valid,
            authority: AuthorityState::Allowed,
            activity: ActivityState::Idle,
            pressure: PressureState::None,
            recovery: RecoveryState::None,
        }
    }

    /// A disabled control (no action, no matter the other fields).
    #[must_use]
    pub const fn disabled() -> Self {
        Self {
            enabled: false,
            ..Self::idle()
        }
    }

    /// This state with the given enabled flag.
    #[must_use]
    pub const fn with_enabled(mut self, enabled: bool) -> Self {
        self.enabled = enabled;
        self
    }

    /// This state with the given focus.
    #[must_use]
    pub const fn with_focus(mut self, focus: FocusState) -> Self {
        self.focus = focus;
        self
    }

    /// This state with the given pointer relationship.
    #[must_use]
    pub const fn with_pointer(mut self, pointer: PointerState) -> Self {
        self.pointer = pointer;
        self
    }

    /// This state with the given selection.
    #[must_use]
    pub const fn with_selection(mut self, selection: SelectionState) -> Self {
        self.selection = selection;
        self
    }

    /// This state with the given validation.
    #[must_use]
    pub const fn with_validation(mut self, validation: ValidationState) -> Self {
        self.validation = validation;
        self
    }

    /// This state with the given authority.
    #[must_use]
    pub const fn with_authority(mut self, authority: AuthorityState) -> Self {
        self.authority = authority;
        self
    }

    /// This state with the given activity.
    #[must_use]
    pub const fn with_activity(mut self, activity: ActivityState) -> Self {
        self.activity = activity;
        self
    }

    /// This state with the given pressure.
    #[must_use]
    pub const fn with_pressure(mut self, pressure: PressureState) -> Self {
        self.pressure = pressure;
        self
    }

    /// This state with the given recovery posture.
    #[must_use]
    pub const fn with_recovery(mut self, recovery: RecoveryState) -> Self {
        self.recovery = recovery;
        self
    }

    /// The derived interaction/authority taxonomy a renderer switches on.
    ///
    /// This is the one place the spec §13 distinction is computed, so no
    /// renderer re-derives it and none accidentally paints an authority
    /// denial as a plain disabled control. Precedence, highest first:
    ///
    /// 1. `!enabled` → [`ControlDisposition::DisabledByState`].
    /// 2. authority [`Denied`](AuthorityState::Denied) /
    ///    [`NeedsCapability`](AuthorityState::NeedsCapability) →
    ///    [`ControlDisposition::DeniedByAuthority`].
    /// 3. authority [`FailedClosed`](AuthorityState::FailedClosed) →
    ///    [`ControlDisposition::FailedClosed`].
    /// 4. authority [`NeedsConfirmation`](AuthorityState::NeedsConfirmation)
    ///    → [`ControlDisposition::NeedsConfirmation`].
    /// 5. validation [`Pending`](ValidationState::Pending) →
    ///    [`ControlDisposition::PendingCheck`].
    /// 6. otherwise → [`ControlDisposition::Interactive`].
    #[must_use]
    pub const fn disposition(self) -> ControlDisposition {
        if !self.enabled {
            return ControlDisposition::DisabledByState;
        }
        match self.authority {
            AuthorityState::Denied | AuthorityState::NeedsCapability => {
                ControlDisposition::DeniedByAuthority
            }
            AuthorityState::FailedClosed => ControlDisposition::FailedClosed,
            AuthorityState::NeedsConfirmation => ControlDisposition::NeedsConfirmation,
            AuthorityState::Allowed => match self.validation {
                ValidationState::Pending => ControlDisposition::PendingCheck,
                _ => ControlDisposition::Interactive,
            },
        }
    }

    /// Whether the control will dispatch its action when activated.
    ///
    /// True only when the control is [`Interactive`](ControlDisposition::Interactive)
    /// or awaiting confirmation; any disabled, denied, pending, or
    /// failed-closed disposition returns false (fail closed).
    #[must_use]
    pub const fn is_actionable(self) -> bool {
        matches!(
            self.disposition(),
            ControlDisposition::Interactive | ControlDisposition::NeedsConfirmation
        )
    }
}

/// The interaction/authority taxonomy a renderer paints, derived from a
/// [`ControlState`] by [`ControlState::disposition`].
///
/// These are the spec §13 cases. They are deliberately distinct so a user
/// can tell *why* a control will not act: because the object's state makes it
/// invalid, because they lack authority, because it needs confirmation,
/// because a check is pending, or because an attempt was refused.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub enum ControlDisposition {
    /// The control acts normally.
    Interactive,
    /// The object state makes the action invalid (muted plate and label).
    DisabledByState,
    /// The caller lacks authority (Authority Mark plus reason).
    DeniedByAuthority,
    /// The action is possible but consequential (deliberate confirmation).
    NeedsConfirmation,
    /// Awaiting a backing-service response (Heat Seam / verification mark).
    PendingCheck,
    /// The action was refused safely (warning / recovery with typed reason).
    FailedClosed,
}

/// The exact window-manager command a furniture button represents.
///
/// A theme may reorder or reposition the command group, but never change
/// what a button *means*; that meaning is this typed kind, not a position.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub enum WindowControlKind {
    /// Cooperative close request (never force termination, spec §11.19).
    Close,
    /// Remove the window from the workspace, keeping it alive.
    Minimize,
    /// Send the window to the bottom of the stack, keeping it visible.
    PutToBack,
    /// Toggle between restored and maximized.
    SizeToggle,
}

/// Whether a window frame is active, inactive, or requesting attention.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash, Default)]
pub enum WindowActivationState {
    /// The frame is inactive but structurally complete.
    #[default]
    Inactive,
    /// The frame is the active window (strongest Frame Rim and title).
    Active,
    /// The frame requests attention without stealing focus (bounded bead).
    AttentionRequested,
}

/// Whether a window is restored or maximized.
///
/// Fullscreen is a separate application/session mode and is *not* a size
/// state of the size-toggle control (spec §5).
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash, Default)]
pub enum WindowSizeState {
    /// The window occupies its saved logical rectangle.
    #[default]
    Restored,
    /// The window fills the session work area (not the physical display).
    Maximized,
}

impl WindowSizeState {
    /// The action the size-toggle control will perform *next* from this
    /// state — the label and glyph a [`WindowControlKind::SizeToggle`] shows.
    ///
    /// A restored window offers [`SizeAction::Maximize`]; a maximized window
    /// offers [`SizeAction::Restore`] (spec §11.22).
    #[must_use]
    pub const fn next_size_action(self) -> SizeAction {
        match self {
            WindowSizeState::Restored => SizeAction::Maximize,
            WindowSizeState::Maximized => SizeAction::Restore,
        }
    }
}

/// The next action a size-toggle control will perform, used for its glyph and
/// accessible name.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub enum SizeAction {
    /// Fill the work area.
    Maximize,
    /// Return to the saved logical rectangle.
    Restore,
}

/// The composed state of a window's furniture.
///
/// Activation and size are typed states; movability and resizability are
/// per-window capabilities the window manager derives from the client's
/// declared support and from session/stacking/work-area policy. A control
/// whose capability is absent renders disabled with a reason rather than
/// vanishing (spec §11.17–§11.23).
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash, Default)]
pub struct WindowFurnitureState {
    /// Whether the frame is active, inactive, or requesting attention.
    pub activation: WindowActivationState,
    /// Whether the window is restored or maximized.
    pub size: WindowSizeState,
    /// Whether the window may be moved by its title bar.
    pub movable: bool,
    /// Whether the window may be resized (grabber/size-toggle enabled).
    pub resizable: bool,
}

impl WindowFurnitureState {
    /// The next action a size-toggle control shows for this window.
    #[must_use]
    pub const fn size_action(self) -> SizeAction {
        self.size.next_size_action()
    }
}
