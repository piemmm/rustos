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
    /// A whole session's screen appearing or leaving: the login screen
    /// appearing out of black and fading back to it once a secret is
    /// accepted, and the desktop revealing from that black and dissolving
    /// back into it when the session ends.
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
/// Running and settled are the whole of it. A settled timeline is *complete*,
/// not pending: [`progress`](Self::progress) answers [`u8::MAX`] and
/// [`next_frame_in`](Self::next_frame_in) asks for no wake, so a
/// reduced-motion theme's zero duration renders the finished state immediately
/// and arms nothing. A running one always owes at least one more frame, and
/// stops owing only when its owner [`settle`](Self::settle)s or drops it.
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

    /// Nanoseconds until the next frame is worth drawing, or `None` when this
    /// timeline is settled and owes nothing.
    ///
    /// A running timeline always asks for one: the nearer of the frame cadence
    /// and what remains of the span, and `0` — draw it now — once the span has
    /// run out or the clock has jumped behind the start, because that frame is
    /// the end state and nothing has drawn it yet. The owner draws the frame
    /// it is given and then [`settle`](Self::settle)s or drops the timeline;
    /// that, not the clock, is what ends the asking.
    #[must_use]
    pub fn next_frame_in(self, now_ns: u64) -> Option<u64> {
        if self.duration_ns == 0 {
            return None;
        }
        // A clock behind the start reads as complete, so what is left of the
        // span is nothing rather than all of it.
        let remaining = now_ns
            .checked_sub(self.started_ns)
            .map_or(0, |elapsed| self.duration_ns.saturating_sub(elapsed));
        Some(remaining.min(Self::FRAME_NS))
    }
}

/// One strength ramp in flight: a [`Timeline`] carrying a value from where it
/// started to where it is going.
///
/// A timeline answers *how far through*; a fade answers *what the strength
/// is*. Every surface that dissolves between two strengths — the login
/// screen's veil covering the screen and lifting off it again, a session's
/// screen revealing from black and going back to it — is this one state
/// machine, so the two directions cannot drift apart and a fade that
/// interrupts another resumes from what is actually on screen instead of
/// snapping somewhere it never was.
///
/// The direction is nothing more than the two ends: a ramp to [`u8::MAX`]
/// covers, a ramp to `0` uncovers, and one begun part-way simply names the
/// strength it starts from. The interpolation is linear, like every fade's,
/// because the strength is what the eye reads, not the travel.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub struct Fade {
    timeline: Timeline,
    from: u8,
    to: u8,
}

impl Fade {
    /// A fade from `from` to `to` over `duration_ms`, beginning at `now_ns`.
    ///
    /// A zero duration — a reduced-motion theme's answer for every
    /// interaction — is complete from the first read: the strength is `to`
    /// and no frame is ever owed.
    #[must_use]
    pub fn start(now_ns: u64, duration_ms: u16, from: u8, to: u8) -> Self {
        Self {
            timeline: Timeline::start(now_ns, duration_ms),
            from,
            to,
        }
    }

    /// The strength at `now_ns`: [`from`](Self::start) at the start of the
    /// span, [`to`](Self::target) at its end and ever after.
    #[must_use]
    pub fn strength(self, now_ns: u64) -> u8 {
        let progress = i64::from(self.timeline.progress(now_ns));
        let max = i64::from(u8::MAX);
        let span = i64::from(self.to) - i64::from(self.from);
        let moved = i64::from(self.from) + span * progress / max;
        u8::try_from(moved.clamp(0, max)).unwrap_or(self.to)
    }

    /// The strength this fade ends on, which is also its direction.
    #[must_use]
    pub const fn target(self) -> u8 {
        self.to
    }

    /// Whether this fade still has a span to run over — see
    /// [`Timeline::running`].
    #[must_use]
    pub const fn running(self) -> bool {
        self.timeline.running()
    }

    /// Nanoseconds until the next frame is worth drawing, or `None` when this
    /// fade owes nothing — see [`Timeline::next_frame_in`].
    #[must_use]
    pub fn next_frame_in(self, now_ns: u64) -> Option<u64> {
        self.timeline.next_frame_in(now_ns)
    }

    /// Stop running, having arrived: the strength becomes
    /// [`target`](Self::target) and stays there, and no further frame is
    /// owed. What an owner calls once it has drawn the end state.
    pub const fn settle(&mut self) {
        self.timeline.settle();
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
