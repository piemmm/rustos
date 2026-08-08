//! Motion timings, density, and contrast — the non-colour, non-geometry axes
//! of a Reactive Alloy theme.
//!
//! Reactive Alloy motion is *magnetic, not liquid*: a control may lift,
//! compress, brighten, or expose a seam, driven by a state change, but never
//! runs an idle decorative loop. The durations here are the spec §9 timing
//! targets as theme *data*, so a control never hard-codes a timing. A theme's
//! reduced-motion mode collapses every animated transition to an immediate
//! state change while the state itself still changes visibly (through
//! contrast, rail thickness, shape marks, and labels).
//!
//! Density and contrast are likewise data: they change metrics and emphasis,
//! never the *meaning* of a state (spec §6, §14, §15).

/// One animated interaction, keyed to a spec §9 timing target.
///
/// A control asks the [`MotionTheme`] for the duration of the interaction it
/// is starting; the theme returns the tuned duration, or zero in
/// reduced-motion mode.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub enum MotionInteraction {
    /// Pointer entering a control.
    HoverEnter,
    /// Pointer leaving a control.
    HoverExit,
    /// A press compressing a control.
    PressCompress,
    /// A control settling after release.
    ReleaseSettle,
    /// A panel opening.
    PanelOpen,
    /// A menu opening.
    MenuOpen,
    /// An event-driven job-progress pulse.
    JobProgressPulse,
    /// A recovery latch revealing itself.
    RecoveryLatchReveal,
    /// A window activating or deactivating.
    WindowActivate,
    /// A window minimizing, restoring, or toggling size.
    WindowSizeTransition,
    /// A scrollbar waking or settling.
    ScrollbarWake,
    /// A selection mark arriving on an item, or leaving one.
    SelectionChange,
}

impl MotionInteraction {
    /// Every interaction, in the order a [`MotionTheme`]'s duration table
    /// holds them: the table is indexed by the variant, so this order is the
    /// meaning of [`MotionTheme::new`]'s argument.
    pub const ALL: [Self; 12] = [
        Self::HoverEnter,
        Self::HoverExit,
        Self::PressCompress,
        Self::ReleaseSettle,
        Self::PanelOpen,
        Self::MenuOpen,
        Self::JobProgressPulse,
        Self::RecoveryLatchReveal,
        Self::WindowActivate,
        Self::WindowSizeTransition,
        Self::ScrollbarWake,
        Self::SelectionChange,
    ];

    /// How many durations a [`MotionTheme`] carries.
    pub const COUNT: usize = Self::ALL.len();
}

/// The motion timings a theme provides, in milliseconds, plus the
/// reduced-motion policy.
///
/// Durations are stored as the *tuned* value; [`duration`](MotionTheme::duration)
/// returns `0` for every interaction when [`reduced_motion`](MotionTheme::reduced_motion)
/// is set, which is how a control turns animation into an immediate change
/// without a second code path.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub struct MotionTheme {
    durations: [u16; MotionInteraction::COUNT],
    reduced_motion: bool,
}

impl MotionTheme {
    /// Assemble a motion theme from its per-interaction durations (ms),
    /// indexed by [`MotionInteraction::ALL`].
    ///
    /// A table rather than one parameter per interaction: the durations are
    /// all the same type, so positional arguments could be transposed
    /// silently, and a new interaction would change every call site's
    /// arity.
    ///
    /// `reduced_motion` is off; use [`with_reduced_motion`](Self::with_reduced_motion)
    /// to derive the reduced variant of the same theme.
    #[must_use]
    pub const fn new(durations: [u16; MotionInteraction::COUNT]) -> Self {
        Self {
            durations,
            reduced_motion: false,
        }
    }

    /// This motion theme with reduced motion set to `reduced`.
    #[must_use]
    pub const fn with_reduced_motion(mut self, reduced: bool) -> Self {
        self.reduced_motion = reduced;
        self
    }

    /// Whether animated transitions are suppressed in favour of immediate
    /// state changes.
    #[must_use]
    pub const fn reduced_motion(self) -> bool {
        self.reduced_motion
    }

    /// The duration of an interaction, in milliseconds.
    ///
    /// Returns `0` for every interaction when [`reduced_motion`](Self::reduced_motion)
    /// is set — an immediate state change — so a control needs no separate
    /// reduced-motion branch.
    #[must_use]
    pub const fn duration(self, interaction: MotionInteraction) -> u16 {
        if self.reduced_motion {
            return 0;
        }
        let slot = interaction as usize;
        if slot < self.durations.len() {
            self.durations[slot]
        } else {
            0
        }
    }
}

/// The information density of a theme (spec §14).
///
/// Density changes metrics, never state semantics. It is data on the theme,
/// so a denser layout is a different [`Metrics`](crate::Metrics) table, not a
/// sibling control implementation.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash, Default)]
pub enum Density {
    /// Tables, task lists, sidebars, dense system panels.
    Compact,
    /// Default desktop applications.
    #[default]
    Normal,
    /// Touch-adjacent or distance-viewed surfaces.
    Comfortable,
}

/// The contrast policy of a theme (spec §15).
///
/// High contrast increases rim, rail, and text contrast before adding glow;
/// a monochrome-safe policy additionally requires every semantic role to be
/// distinguished by shape, not colour. Contrast never changes the meaning of
/// a state.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash, Default)]
pub enum Contrast {
    /// Normal contrast.
    #[default]
    Normal,
    /// Increased rim/rail/text contrast.
    High,
    /// Monochrome-safe: semantic roles must be distinguished by shape.
    Monochrome,
}
