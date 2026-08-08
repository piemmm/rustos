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
    /// One whole view giving way to another — the login screen stepping from
    /// the account chooser to the chosen account's password prompt.
    StageTransition,
    /// The thing an authority refused, shaken to say so.
    AttemptRejected,
    /// A whole session's screen appearing or leaving: the login screen fading
    /// to black once a secret is accepted, and the desktop fading in over it.
    SessionFade,
}

impl MotionInteraction {
    /// Every interaction, in the order a [`MotionTheme`]'s duration table
    /// holds them: the table is indexed by the variant, so this order is the
    /// meaning of [`MotionTheme::new`]'s argument.
    pub const ALL: [Self; 15] = [
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
        Self::StageTransition,
        Self::AttemptRejected,
        Self::SessionFade,
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

/// Nanoseconds in one millisecond, the unit a duration is authored in and the
/// unit a monotonic clock is read in.
const NANOS_PER_MS: u64 = 1_000_000;

/// One animation in flight: when it began, and how long it runs for.
///
/// The consumer's counterpart to [`MotionTheme::duration`], and the single
/// definition of how a duration becomes frames. Every animated surface — a
/// selection mark moving between items, one view giving way to another, a
/// refused attempt shaking, a session fading to black — starts a timeline
/// from the theme's duration for its interaction, asks it how far through it
/// is when it paints, and asks it when to wake next. A second such state
/// machine would be a second place for an animation to stall or to leave a
/// timer armed.
///
/// It reads no clock of its own: the embedder passes the monotonic instant it
/// already holds. That is what lets a surface be animated on the host with no
/// kernel, and keeps a surface from acquiring a clock it has no other use
/// for.
///
/// A settled timeline is *complete*, not pending: [`progress`](Self::progress)
/// answers [`u8::MAX`] and [`next_frame_in`](Self::next_frame_in) asks for no
/// wake, so a reduced-motion theme's zero duration renders the finished state
/// immediately and arms nothing.
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq, Hash)]
pub struct Timeline {
    started_ns: u64,
    duration_ns: u64,
}

impl Timeline {
    /// Nothing animating: complete, and asking for no wake.
    pub const SETTLED: Self = Self {
        started_ns: 0,
        duration_ns: 0,
    };

    /// The shortest gap between two animation frames.
    ///
    /// A sixtieth of a second, shared by every animation so they are all
    /// equally smooth: no shipped display refreshes faster, so a finer step
    /// would compose frames nobody ever sees.
    pub const FRAME_NS: u64 = 1_000_000_000 / 60;

    /// A timeline of `duration_ms` beginning at `now_ns`.
    ///
    /// A zero duration — what a reduced-motion theme answers for every
    /// interaction — is [`SETTLED`](Self::SETTLED): the state change lands
    /// immediately and nothing is left running to wake for.
    #[must_use]
    pub fn start(now_ns: u64, duration_ms: u16) -> Self {
        if duration_ms == 0 {
            return Self::SETTLED;
        }
        Self {
            started_ns: now_ns,
            duration_ns: u64::from(duration_ms).saturating_mul(NANOS_PER_MS),
        }
    }

    /// Whether this timeline still has a span to run over. A settled one does
    /// not; one whose span has merely elapsed still does, until a consumer
    /// [`settle`](Self::settle)s it.
    #[must_use]
    pub const fn running(self) -> bool {
        self.duration_ns != 0
    }

    /// How far through the span `now_ns` is: `0` at the start, [`u8::MAX`] at
    /// the end.
    ///
    /// A settled timeline, an instant at or past the end, and an instant
    /// *before* the start are all complete. The last of those is a clock that
    /// jumped backwards, which settles an animation rather than stalling it on
    /// its first frame.
    #[must_use]
    pub fn progress(self, now_ns: u64) -> u8 {
        if self.duration_ns == 0 {
            return u8::MAX;
        }
        let Some(elapsed) = now_ns.checked_sub(self.started_ns) else {
            return u8::MAX;
        };
        if elapsed >= self.duration_ns {
            return u8::MAX;
        }
        let scaled = elapsed.saturating_mul(u64::from(u8::MAX)) / self.duration_ns;
        u8::try_from(scaled).unwrap_or(u8::MAX)
    }

    /// [`progress`](Self::progress) shaped so the animation starts and ends
    /// gently instead of beginning and stopping at full speed.
    ///
    /// The smoothstep of the linear progress, which is what makes a travelling
    /// element read as having weight; a fade whose *strength* is what matters
    /// takes the linear progress instead. One definition, so two animations
    /// cannot ease differently by accident.
    #[must_use]
    pub fn eased(self, now_ns: u64) -> u8 {
        let t = u32::from(self.progress(now_ns));
        let max = u32::from(u8::MAX);
        // 3t² − 2t³ over the byte range, widest term well inside 32 bits.
        let shaped = t * t * (3 * max - 2 * t) / (max * max);
        u8::try_from(shaped.min(max)).unwrap_or(u8::MAX)
    }

    /// Whether `now_ns` has reached the end of the span, so a consumer holding
    /// this timeline has nothing left to animate and should
    /// [`settle`](Self::settle) it.
    #[must_use]
    pub fn finished(self, now_ns: u64) -> bool {
        self.progress(now_ns) == u8::MAX
    }

    /// Stop running: this timeline becomes [`SETTLED`](Self::SETTLED).
    pub const fn settle(&mut self) {
        *self = Self::SETTLED;
    }

    /// Nanoseconds until the next frame of this animation is worth drawing, or
    /// `None` when there is nothing left to draw.
    ///
    /// The nearer of the frame cadence and what remains of the span, so the
    /// last wake lands on the end rather than past it, and an animation that
    /// is settled or already over arms no timer at all.
    #[must_use]
    pub fn next_frame_in(self, now_ns: u64) -> Option<u64> {
        if self.duration_ns == 0 {
            return None;
        }
        let elapsed = now_ns.checked_sub(self.started_ns)?;
        let remaining = self.duration_ns.checked_sub(elapsed)?;
        (remaining > 0).then(|| remaining.min(Self::FRAME_NS))
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
