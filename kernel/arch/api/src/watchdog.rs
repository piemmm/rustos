//! Lockup-watchdog surface of the Arch HAL — the non-maskable liveness
//! sample and best-effort cross-CPU recovery a port exposes so the
//! architecture-neutral detector in `kernel/core` can catch, diagnose,
//! and try to break both *soft* and *hard* CPU lockups.
//!
//! # Why this is a HAL slice
//!
//! A soft lockup (a CPU that keeps taking interrupts but stops returning
//! to the scheduler) is observable from the CPU's own timer path. A
//! **hard** lockup — a CPU that has stopped taking maskable interrupts
//! entirely (a lock spun with IRQs masked, a wedged device access, an
//! interrupt storm) — is *not*: the victim never runs its own tick, so
//! only **another** CPU can observe it, and only if that observation
//! rides a channel the victim cannot mask. That channel is inherently
//! per-architecture — a pseudo-NMI: the aarch64 FIQ (group 0), the
//! x86_64 NMI/LAPIC-deadline, the riscv64 higher-privilege timer. The
//! non-maskable *sample* and the cross-CPU *recovery signal* therefore
//! live here, behind one architecture-neutral vocabulary, while their
//! bodies stay genuinely per-port (the parallel per-arch implementations
//! of this one trait are the deliberate shape of modularity, never
//! collapsed behind `cfg`).
//!
//! # The contract
//!
//! * The port arms a per-CPU non-maskable cadence timer (~[`CADENCE_NS`]).
//!   On every fire it builds a [`WatchdogSample`] from the interrupted
//!   frame and hands it to `kernel/core`'s
//!   `watchdog::on_watchdog_tick`, which stamps this CPU's liveness
//!   heartbeat, records the sample as the CPU's last-known context, and
//!   runs the cross-CPU scan.
//! * When the scan finds another CPU locked up, `kernel/core` renders the
//!   diagnosis and asks this port to try to break it through
//!   [`WatchdogArch::request_recovery`]. The port raises the appropriate
//!   cross-CPU signal (a reschedule IPI for a soft lockup; a directed
//!   non-maskable attention interrupt for a hard one) and reports what it
//!   was able to do with a [`RecoveryOutcome`]. Recovery is **best
//!   effort**: a genuinely wedged core may be unrecoverable, in which
//!   case the honest answer is [`RecoveryOutcome::Unrecoverable`] and the
//!   detector has already made the failure loud.
//!
//! A port with no pseudo-NMI channel yet simply never installs a
//! [`WatchdogArch`] and never calls `on_watchdog_tick`; the soft detector
//! still works from the ordinary timer path, and hard-lockup detection is
//! inert rather than wrong (fail closed).

use crate::CpuId;

/// The non-maskable watchdog cadence, in nanoseconds (1 second).
///
/// One definition every port arms its per-CPU pseudo-NMI cadence timer to
/// and the architecture-neutral detector sizes its thresholds against, so
/// the sample rate and the detection thresholds can never drift apart. A
/// one-second cadence keeps the liveness heartbeat fresh enough that a
/// multi-second lockup threshold has several samples of margin, while the
/// per-fire cost (one heartbeat store plus a bounded scan) is negligible
/// against a whole second of CPU time — the watchdog does not perturb
/// normal execution.
pub const CADENCE_NS: u64 = 1_000_000_000;

/// The two kinds of CPU lockup the watchdog distinguishes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WatchdogKind {
    /// The CPU is still taking its non-maskable watchdog sample (alive at
    /// the trap level) but has stopped returning to the scheduler for far
    /// longer than any bounded operation should take — a runaway in-kernel
    /// loop or a task that never yields. Often recoverable by forcing a
    /// reschedule of the offending CPU.
    Soft,
    /// The CPU has stopped taking even the non-maskable watchdog sample
    /// while it is running work — wedged with maskable interrupts off, an
    /// interrupt storm, or a dead core. Only another CPU can observe it,
    /// and recovery is best-effort (a directed attention interrupt).
    Hard,
}

impl WatchdogKind {
    /// A short, fixed lowercase tag for the diagnosis (`"soft"`/`"hard"`).
    #[must_use]
    pub const fn tag(self) -> &'static str {
        match self {
            Self::Soft => "soft",
            Self::Hard => "hard",
        }
    }
}

/// What a port was able to do when asked to recover a locked-up CPU.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecoveryOutcome {
    /// A reschedule was forced on the target (a soft lockup: the offending
    /// CPU will re-enter the scheduler at its next safe point).
    Rescheduled,
    /// A directed attention interrupt was raised on the target (a hard
    /// lockup: the port asked the wedged core to dump its live state and,
    /// where possible, abandon the offending task).
    ///
    /// Only as forceful as the port's interrupt controller allows, and
    /// never a claim that the target recovered. A port with a genuine
    /// non-maskable channel can reach a core that has masked interrupts;
    /// one without — GICv2 non-secure, where the vector is an ordinary
    /// maskable Group-1 SGI — cannot, and there the loud cross-CPU report
    /// is the whole answer.
    AttentionRaised,
    /// The target could not be recovered; the failure has been made loud
    /// and the caller must treat the CPU as lost.
    Unrecoverable,
    /// This port has no recovery channel for this kind of lockup.
    Unsupported,
}

/// An architecture-neutral snapshot of what a CPU was doing when its
/// non-maskable watchdog sample fired — the raw material of the "why".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WatchdogSample {
    /// The interrupted program counter (the port's return address for the
    /// sample: `ELR_EL1` on aarch64). `0` when the port cannot supply one.
    pub pc: u64,
    /// The id of the task running on the CPU when the sample fired, or
    /// [`WatchdogSample::NO_TASK`] when the CPU was in the kernel/idle with
    /// no user task switched in.
    pub task: u64,
    /// One port-defined auxiliary word carrying the interrupted processor
    /// state the diagnosis decodes (aarch64 `SPSR_EL1` — exception level
    /// and the `DAIF` mask bits, which reveal *why* a hard-locked CPU was
    /// not taking interrupts). `0` when the port supplies none.
    pub aux: u64,
    /// Whether the sample interrupted **kernel** code (a privileged
    /// exception level) rather than a user task. The detector uses it to
    /// tell a CPU wedged *in the kernel* (a genuine lockup even when it is
    /// the only runnable task) apart from a CPU legitimately running a
    /// lone, preemptible user task (which owes the scheduler nothing and
    /// must never be flagged).
    pub in_kernel: bool,
}

impl WatchdogSample {
    /// Sentinel [`Self::task`] meaning "no user task was running".
    pub const NO_TASK: u64 = u64::MAX;

    /// A sample with no context (a port that cannot introspect the frame).
    pub const EMPTY: Self = Self {
        pc: 0,
        task: Self::NO_TASK,
        aux: 0,
        in_kernel: false,
    };
}

/// A device interrupt found *stuck* — and still able to reach a CPU — in
/// the shared interrupt controller by the watchdog observer, with the fact
/// that turns a bare line id into a verdict: whether it is **active** (a
/// live storm) or merely **pending**.
///
/// A hard lockup's own sample is stale, so the observer reads the
/// controller's globally-shared state live to name the offending line. Only
/// a line that could actually be delivered is reported: a masked line
/// cannot reach a CPU, so it can never be the wedge, and the observer skips
/// it rather than blaming an innocent line. That leaves two cases the
/// `active` flag distinguishes — a core wedged mid-handler (`active`) or an
/// enabled line asserted but not yet taken (`pending`) — so the diagnosis
/// is decisive without a second boot to guess.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StuckInterrupt {
    /// The shared interrupt id (aarch64 GICv2 SPI, id >= 32).
    pub intid: u32,
    /// `true` when the line is **active** — acknowledged but not yet
    /// completed, i.e. a handler is in flight (or re-firing faster than it
    /// completes): the signature of a live storm. `false` when the line is
    /// only **pending** — enabled and asserted, but no handler yet running.
    /// A masked line is never reported at all (it cannot reach a CPU), so
    /// this is only ever a live, deliverable suspect.
    pub active: bool,
}

/// A cross-core program-counter sample of a wedged CPU, read from a
/// **non-maskable, halt-free** external-debug channel (the aarch64
/// CoreSight external-debug PC Sample Register, `EDPCSR`).
///
/// A hard lockup's own last-known sample is stale: the victim went silent
/// with maskable interrupts off, so its recorded `pc` names the innocent
/// code it last returned to, not the instruction wedging it now
/// ([`WatchdogArch::stuck_interrupt`] gives the *device* "why"; this gives
/// the *code* "why"). The one observation that survives such a core is a
/// read of its PC by *another* master over a channel the victim cannot
/// mask and that does not halt it — on ARMv8 the memory-mapped external
/// debug interface's `EDPCSR`. This enum is the honest outcome of that
/// read, held to the same fail-closed discipline as the rest of the HAL:
/// a port that cannot make the observation says so rather than fabricating
/// a PC.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemotePcSample {
    /// The observer read a live PC of the target without halting it — a
    /// **fresh** sample, unlike the target's stale pre-silence one, that
    /// names the instruction the wedged core is actually stuck on.
    /// `context` is one port-defined auxiliary word describing the sampled
    /// execution context (aarch64 `EDVIDSR`: security state / exception
    /// level / mode), `0` when the port supplies none.
    Sampled {
        /// The sampled program counter.
        pc: u64,
        /// A port-defined auxiliary context word (`0` = none).
        context: u64,
    },
    /// The channel exists and is reachable, but could not produce a valid
    /// sample this read — the target PE is in a low-power or reset state,
    /// or the sample register was not valid. An honest transient, never a
    /// claim that the observation is impossible.
    Unavailable(&'static str),
    /// This port has no external-debug PC-sampling channel wired for the
    /// target: no debug component was discovered for it, the feature is not
    /// implemented by the silicon, or the port exposes no such surface at
    /// all. Fail closed — the caller keeps the stale sample rather than a
    /// fabricated one.
    Unsupported(&'static str),
}

impl RemotePcSample {
    /// The fresh PC when this is a [`Self::Sampled`] reading, else `None`.
    /// A convenience for a caller that only wants the address and treats
    /// both non-sampled variants identically (keep the stale sample).
    #[must_use]
    pub const fn pc(self) -> Option<u64> {
        match self {
            Self::Sampled { pc, .. } => Some(pc),
            Self::Unavailable(_) | Self::Unsupported(_) => None,
        }
    }
}

/// The interrupt a CPU acknowledged and has **not yet completed** — the
/// one observation a wedged core can only make about *itself*.
///
/// [`WatchdogArch::stuck_interrupt`] reads the controller's globally-shared
/// state, so it sees shared device lines only: per-CPU **banked** lines
/// (aarch64 GICv2 SGIs and PPIs) are invisible to an observer, which reads
/// its own banked bits rather than the victim's. Yet a banked line whose
/// end-of-interrupt never runs is precisely what leaves a CPU interface's
/// running priority raised, blocking every later interrupt on that core —
/// its preemption timer included — while the shared scan innocently falls
/// through to the first enabled-and-pending device line.
///
/// The gap closes from the other side: each CPU publishes what it
/// acknowledged into its own per-CPU slot and clears it at the matching
/// end-of-interrupt, so the observer reads back the victim's own record.
/// Purely observational — publishing it changes no acknowledge/complete
/// ordering.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InFlightInterrupt {
    /// The target has no acknowledged-but-uncompleted interrupt: it is not
    /// inside a handler, so no missed completion is wedging its interface.
    Idle,
    /// The target acknowledged `intid` and has not completed it. On a core
    /// that has gone silent this names the interrupt it is still inside —
    /// including a banked SGI or PPI no observer could otherwise see.
    Acknowledged {
        /// The acknowledged interrupt id, with any port-specific
        /// acknowledge cookie (an aarch64 SGI's source-CPU field) already
        /// stripped: an id, never a raw register value.
        intid: u32,
    },
    /// This port does not publish an in-flight interrupt for `target`.
    /// Fail closed — the caller renders nothing rather than implying the
    /// target is idle.
    Unsupported(&'static str),
}

impl InFlightInterrupt {
    /// The acknowledged id when one is in flight, else `None`. A
    /// convenience for a caller that treats [`Self::Idle`] and
    /// [`Self::Unsupported`] alike (nothing to name).
    #[must_use]
    pub const fn intid(self) -> Option<u32> {
        match self {
            Self::Acknowledged { intid } => Some(intid),
            Self::Idle | Self::Unsupported(_) => None,
        }
    }
}

/// Per-CPU publication of the interrupt each CPU has acknowledged but not
/// yet completed, so an observer can read back what a *silent* core is
/// still inside.
///
/// The bookkeeping is identical on every port — a per-CPU slot written by
/// its own CPU at interrupt entry and restored at completion — so it lives
/// here once and each port supplies only its two call sites and its
/// acknowledge/complete decode. Reads are plain loads of published state:
/// no lock, no block, safe from a non-maskable sample path.
///
/// Publication is a *diagnostic* capability: a build that never registers
/// backing storage never records anything and [`in_flight::read`] answers
/// [`InFlightInterrupt::Unsupported`], so a shippable image pays nothing
/// and claims nothing.
pub mod in_flight {
    use core::sync::atomic::{AtomicBool, AtomicPtr, AtomicU32, AtomicUsize, Ordering};

    use super::{CpuId, InFlightInterrupt};

    /// Slot value meaning "this CPU has no interrupt in flight". No
    /// architecture numbers an interrupt `u32::MAX` (a GICv2 id is at most
    /// 1019, an x86 vector at most 255), so the sentinel cannot collide
    /// with a real acknowledgement.
    pub const NO_IN_FLIGHT: u32 = u32::MAX;

    // Compile-time guard for the invariant NO_IN_FLIGHT documents: a GICv2
    // id is at most 1019 and an x86 vector at most 255.
    const _: () = assert!(NO_IN_FLIGHT > 1023);

    /// The slot value an interrupt entry displaced, which its matching
    /// completion must put back so a *nested* interrupt cannot erase the
    /// record of the interrupt it interrupted.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    #[must_use = "an interrupt entry's displaced record must be restored at its completion"]
    pub struct DisplacedInFlight(u32);

    /// Why per-CPU in-flight publication could not be registered.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum RegisterError {
        /// Backing storage was already published this boot (set-once).
        AlreadyRegistered,
        /// The supplied slice covers no CPU, so nothing could be recorded.
        Empty,
    }

    /// Latches on the first [`register_slots`], so the live per-CPU slice
    /// can never be re-pointed underneath a CPU that is mid-interrupt.
    static REGISTERED: AtomicBool = AtomicBool::new(false);

    /// Base of the registered per-CPU slice, null until [`register_slots`].
    static SLOTS: AtomicPtr<AtomicU32> = AtomicPtr::new(core::ptr::null_mut());

    /// Length of the registered per-CPU slice, published before the base so
    /// a reader that sees a non-null base also sees the matching length.
    static SLOT_COUNT: AtomicUsize = AtomicUsize::new(0);

    /// Publish caller-leaked per-CPU in-flight slots, one per discovered
    /// CPU, and return how many were registered.
    ///
    /// Every slot is reset to [`NO_IN_FLIGHT`] before publication, so a
    /// caller may hand in plainly-zeroed storage. Set-once per boot, and
    /// called before any CPU takes an interrupt through the publishing
    /// path.
    ///
    /// # Errors
    ///
    /// * [`RegisterError::Empty`] when `slots` is empty.
    /// * [`RegisterError::AlreadyRegistered`] on a second publish; nothing
    ///   is re-pointed.
    pub fn register_slots(slots: &'static [AtomicU32]) -> Result<usize, RegisterError> {
        if slots.is_empty() {
            return Err(RegisterError::Empty);
        }
        if REGISTERED
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return Err(RegisterError::AlreadyRegistered);
        }
        for slot in slots {
            slot.store(NO_IN_FLIGHT, Ordering::Relaxed);
        }
        SLOT_COUNT.store(slots.len(), Ordering::Release);
        SLOTS.store(slots.as_ptr().cast_mut(), Ordering::Release);
        Ok(slots.len())
    }

    /// The slot for `cpu`, or `None` before registration or for a `cpu`
    /// beyond the registered count (fail closed rather than index outside
    /// the published slice).
    fn slot(cpu: CpuId) -> Option<&'static AtomicU32> {
        let base = SLOTS.load(Ordering::Acquire);
        if base.is_null() {
            return None;
        }
        let index = cpu as usize;
        if index >= SLOT_COUNT.load(Ordering::Acquire) {
            return None;
        }
        // SAFETY: a non-null base is published only from a `&'static
        // [AtomicU32]` whose length was stored (release) before the base,
        // and the index was just bounds-checked against that length, so
        // this element is in bounds and lives for `'static`.
        Some(unsafe { &*base.add(index) })
    }

    /// Record that `cpu` has acknowledged `intid` and owes it a completion,
    /// returning the record it displaced for the matching [`restore`].
    ///
    /// Called by the port from its own CPU's interrupt entry, after the
    /// acknowledge that named `intid`. A no-op before registration.
    pub fn record(cpu: CpuId, intid: u32) -> DisplacedInFlight {
        match slot(cpu) {
            Some(slot) => {
                // Only this CPU writes its own slot (an observer only
                // reads), so the read-then-write needs no atomic swap.
                let displaced = slot.load(Ordering::Relaxed);
                slot.store(intid, Ordering::Relaxed);
                DisplacedInFlight(displaced)
            }
            None => DisplacedInFlight(NO_IN_FLIGHT),
        }
    }

    /// Put back the record `displaced` by the matching [`record`], at the
    /// point `cpu` completes the interrupt it acknowledged. A no-op before
    /// registration.
    pub fn restore(cpu: CpuId, displaced: DisplacedInFlight) {
        if let Some(slot) = slot(cpu) {
            slot.store(displaced.0, Ordering::Relaxed);
        }
    }

    /// Read back what `cpu` published — the observation a wedged core's own
    /// stale sample cannot give, and the only way to see a banked line.
    #[must_use]
    pub fn read(cpu: CpuId) -> InFlightInterrupt {
        let Some(slot) = slot(cpu) else {
            return InFlightInterrupt::Unsupported("no in-flight interrupt publication registered");
        };
        match slot.load(Ordering::Relaxed) {
            NO_IN_FLIGHT => InFlightInterrupt::Idle,
            intid => InFlightInterrupt::Acknowledged { intid },
        }
    }

    #[cfg(test)]
    mod tests {
        use super::{
            read, record, register_slots, restore, AtomicU32, InFlightInterrupt, RegisterError,
        };

        /// The one process-wide registration every test in this module
        /// shares (registration is set-once, so it cannot be per-test).
        /// Zeroed on purpose: `register_slots` must reset each slot to the
        /// idle sentinel itself.
        static SLOTS: [AtomicU32; 4] = [
            AtomicU32::new(0),
            AtomicU32::new(0),
            AtomicU32::new(0),
            AtomicU32::new(0),
        ];

        fn registered() -> usize {
            match register_slots(&SLOTS) {
                Ok(count) => count,
                Err(RegisterError::AlreadyRegistered) => SLOTS.len(),
                Err(RegisterError::Empty) => 0,
            }
        }

        #[test]
        fn an_acknowledged_line_is_published_and_cleared_at_completion() {
            assert_eq!(registered(), 4);
            // A device line, a banked timer PPI and a banked SGI all
            // publish and clear identically — the banked ones being the
            // whole point (no observer can read them).
            for intid in [77_u32, 30, 27, 0] {
                let displaced = record(1, intid);
                assert_eq!(read(1), InFlightInterrupt::Acknowledged { intid });
                restore(1, displaced);
                assert_eq!(read(1), InFlightInterrupt::Idle);
            }
        }

        #[test]
        fn a_nested_interrupt_does_not_erase_the_one_it_interrupted() {
            assert_eq!(registered(), 4);
            let outer = record(2, 77);
            let inner = record(2, 27);
            assert_eq!(read(2), InFlightInterrupt::Acknowledged { intid: 27 });
            restore(2, inner);
            assert_eq!(read(2), InFlightInterrupt::Acknowledged { intid: 77 });
            restore(2, outer);
            assert_eq!(read(2), InFlightInterrupt::Idle);
        }

        #[test]
        fn a_cpu_beyond_the_registered_count_reads_unsupported() {
            assert_eq!(registered(), 4);
            // Fail closed: an out-of-range CPU is never claimed idle, and
            // recording for it writes nothing.
            let displaced = record(9, 77);
            assert!(matches!(read(9), InFlightInterrupt::Unsupported(_)));
            restore(9, displaced);
        }

        #[test]
        fn re_registration_is_refused() {
            assert_eq!(registered(), 4);
            assert_eq!(
                register_slots(&SLOTS),
                Err(RegisterError::AlreadyRegistered)
            );
        }
    }
}

/// The per-architecture non-maskable-recovery handle the watchdog reaches
/// through.
///
/// Installed once per boot (see `kernel/core`'s
/// `watchdog::install_recovery`). Implementations must be [`Send`] +
/// [`Sync`] — the detector reaches the handle from whichever CPU's
/// watchdog sample runs the scan — and must **never panic** and **never
/// take a lock** reachable from ordinary code, because a recovery request
/// runs from the non-maskable sample path, potentially while the target
/// CPU holds arbitrary locks.
pub trait WatchdogArch: Send + Sync {
    /// Try, best-effort, to break `target` out of a `kind` lockup.
    ///
    /// Called by the detector after it has already made the lockup loud,
    /// so a return of [`RecoveryOutcome::Unrecoverable`] or
    /// [`RecoveryOutcome::Unsupported`] is honest, never silent. The
    /// implementation must be non-blocking: it raises a cross-CPU signal
    /// and returns, never spinning to wait for the target to react.
    fn request_recovery(&self, target: CpuId, kind: WatchdogKind) -> RecoveryOutcome;

    /// The interrupt id currently *stuck* in the shared interrupt
    /// controller — active (acknowledged but not yet completed) or pending
    /// — that most likely explains a hard lockup, or `None` when the port
    /// exposes no globally-observable interrupt state.
    ///
    /// A hard lockup's own last-known sample is stale (it is, by
    /// definition, the sample taken *before* the CPU went silent), so it
    /// names the innocent code the CPU last returned to, not the storm or
    /// stuck line wedging it now. This lets the **observer** CPU read the
    /// controller's globally-visible state instead — on a shared
    /// distributor a device line stuck active because its handler never
    /// completes (an interrupt storm, or a line whose real servicing is
    /// deferred) shows up here — and name the offending line in the
    /// diagnosis, the decisive "why" the stale sample cannot give.
    ///
    /// Only globally-shared lines are observable this way (aarch64 GICv2
    /// SPIs, id >= 32); per-CPU banked lines (SGIs/PPIs) are not, since the
    /// observer cannot read another CPU's banked state. It is a pure read
    /// of shared state with no side effects, safe to call from the
    /// non-maskable sample path. The default is `None`: a port without such
    /// introspection wired reports nothing rather than guessing (fail
    /// closed), exactly as one without a recovery channel simply never
    /// installs a handle.
    ///
    /// Only a line that can still be *delivered* is reported — a masked
    /// line cannot reach a CPU, so it can never be the wedge and is skipped
    /// rather than blamed. The returned [`StuckInterrupt`] carries the line
    /// id and whether it is actively storming (`active`) or merely
    /// enabled-and-pending, so a reader can tell a live wedge from an
    /// asserted-but-untaken line without a second boot.
    fn stuck_interrupt(&self) -> Option<StuckInterrupt> {
        None
    }

    /// Read a **fresh** program-counter sample of the hard-locked `target`
    /// over a non-maskable, halt-free external-debug channel, or say why it
    /// could not.
    ///
    /// This is the *code*-side counterpart of [`Self::stuck_interrupt`]'s
    /// *device*-side "why". A hard-locked core's own recorded sample is
    /// stale (taken before it went silent), so it cannot name the
    /// instruction wedging it. Where the silicon exposes a memory-mapped
    /// external-debug PC sample (ARMv8 `EDPCSR`), another CPU can read the
    /// victim's PC *without halting it and over a channel the victim cannot
    /// mask* — the one live observation that survives a `DAIF.I`-masked
    /// wedge on a GIC whose non-maskable interrupt belongs to the secure
    /// world. Called by the detector on a hard lockup, from the observer's
    /// non-maskable sample path, so the implementation must be non-blocking,
    /// take no ordinary-code lock, and never panic — exactly as
    /// [`Self::request_recovery`].
    ///
    /// The default is [`RemotePcSample::Unsupported`]: a port with no such
    /// channel (or none discovered for `target`) reports so, and the caller
    /// keeps the stale sample rather than a fabricated one (fail closed),
    /// exactly as a port without shared-interrupt introspection defaults
    /// [`Self::stuck_interrupt`] to `None`.
    fn remote_pc_sample(&self, target: CpuId) -> RemotePcSample {
        let _ = target;
        RemotePcSample::Unsupported("this port exposes no external-debug PC sampling")
    }

    /// The interrupt `target` acknowledged and has not yet completed, as
    /// `target` itself published it (see [`InFlightInterrupt`]).
    ///
    /// This is the *banked*-line counterpart of [`Self::stuck_interrupt`]'s
    /// shared-line "why". A missed completion leaves the target's interface
    /// running priority raised and blocks every later interrupt on that
    /// core, but when the culprit is a per-CPU banked line the observer
    /// cannot read it — so the victim publishes it and the observer reads
    /// it back here. A plain load of already-published state: non-blocking,
    /// lock-free, never panicking, safe from the sample path exactly as
    /// [`Self::request_recovery`].
    ///
    /// The default is [`InFlightInterrupt::Unsupported`]: a port that does
    /// not publish the cookie says so rather than claiming the target is
    /// idle (fail closed).
    fn in_flight_interrupt(&self, target: CpuId) -> InFlightInterrupt {
        let _ = target;
        InFlightInterrupt::Unsupported("this port publishes no in-flight interrupt")
    }
}

/// Host-run conformance vertical every port drives over its
/// [`WatchdogArch`] implementation.
///
/// Kept minimal and behavioural: the trait carries no arithmetic of its
/// own, so the contract worth pinning is that a handle answers every
/// `(target, kind)` request with a well-formed [`RecoveryOutcome`] and
/// never panics. A port with no recovery channel legitimately answers
/// [`RecoveryOutcome::Unsupported`]; the check accepts that.
pub mod conformance {
    use super::{
        CpuId, InFlightInterrupt, RecoveryOutcome, RemotePcSample, WatchdogArch, WatchdogKind,
    };

    /// Run the full [`WatchdogArch`] conformance suite over `arch`.
    ///
    /// Returns `Ok(())` when every request returned a valid outcome, or
    /// `Err(&'static str)` naming the first violation.
    ///
    /// # Errors
    ///
    /// Returns the failing invariant's description when the handle
    /// misbehaves.
    pub fn run_all(arch: &dyn WatchdogArch, self_cpu: CpuId) -> Result<(), &'static str> {
        for &kind in &[WatchdogKind::Soft, WatchdogKind::Hard] {
            // A recovery request must return a well-formed outcome for any
            // target, including the caller's own CPU (a self-request is a
            // benign no-op equivalent to a self-reschedule).
            let _ = arch.request_recovery(self_cpu, kind);
            // A stuck-interrupt query must never panic and must answer with
            // either no line or a well-formed id; a port with no shared
            // interrupt introspection legitimately answers `None`.
            let _ = arch.stuck_interrupt();
            // A remote-PC sample must never panic and must answer with one
            // of the three honest variants for any target; a port with no
            // external-debug channel legitimately answers `Unsupported`. A
            // non-sampled read must carry a non-empty reason (never a bare
            // fail), and a `Sampled` read's convenience accessor must agree
            // it carries a PC, so the variant and the accessor never drift.
            for probe in [self_cpu, self_cpu.wrapping_add(1)] {
                let sample = arch.remote_pc_sample(probe);
                match sample {
                    RemotePcSample::Sampled { pc, .. } => {
                        if sample.pc() != Some(pc) {
                            return Err("remote_pc_sample pc() disagrees with the Sampled reading");
                        }
                    }
                    RemotePcSample::Unavailable(reason) | RemotePcSample::Unsupported(reason) => {
                        if reason.trim().is_empty() {
                            return Err("remote_pc_sample returned an empty reason");
                        }
                        if sample.pc().is_some() {
                            return Err(
                                "remote_pc_sample pc() fabricated a PC for a non-sampled read",
                            );
                        }
                    }
                }
            }
            // An in-flight-interrupt query must never panic and must answer
            // one of the three honest variants for any target; a port that
            // publishes no cookie legitimately answers `Unsupported`, and
            // neither non-acknowledged variant may fabricate an id.
            for probe in [self_cpu, self_cpu.wrapping_add(1)] {
                let in_flight = arch.in_flight_interrupt(probe);
                match in_flight {
                    InFlightInterrupt::Acknowledged { intid } => {
                        if in_flight.intid() != Some(intid) {
                            return Err(
                                "in_flight_interrupt intid() disagrees with the Acknowledged reading",
                            );
                        }
                    }
                    InFlightInterrupt::Unsupported(reason) => {
                        if reason.trim().is_empty() {
                            return Err("in_flight_interrupt returned an empty reason");
                        }
                        if in_flight.intid().is_some() {
                            return Err("in_flight_interrupt intid() fabricated an id");
                        }
                    }
                    InFlightInterrupt::Idle => {
                        if in_flight.intid().is_some() {
                            return Err("in_flight_interrupt intid() fabricated an id");
                        }
                    }
                }
            }
            let outcome = arch.request_recovery(self_cpu.wrapping_add(1), kind);
            match outcome {
                RecoveryOutcome::Rescheduled
                | RecoveryOutcome::AttentionRaised
                | RecoveryOutcome::Unrecoverable
                | RecoveryOutcome::Unsupported => {}
            }
        }
        Ok(())
    }

    #[cfg(test)]
    mod tests {
        use super::super::{
            InFlightInterrupt, RecoveryOutcome, RemotePcSample, StuckInterrupt, WatchdogArch,
            WatchdogKind,
        };
        use super::run_all;
        use crate::CpuId;

        struct StubArch;
        impl WatchdogArch for StubArch {
            fn request_recovery(&self, _target: CpuId, kind: WatchdogKind) -> RecoveryOutcome {
                match kind {
                    WatchdogKind::Soft => RecoveryOutcome::Rescheduled,
                    WatchdogKind::Hard => RecoveryOutcome::AttentionRaised,
                }
            }
        }

        #[test]
        fn a_well_behaved_handle_passes_conformance() {
            assert_eq!(run_all(&StubArch, 0), Ok(()));
        }

        #[test]
        fn an_unsupported_handle_still_passes() {
            struct NoneArch;
            impl WatchdogArch for NoneArch {
                fn request_recovery(&self, _target: CpuId, _kind: WatchdogKind) -> RecoveryOutcome {
                    RecoveryOutcome::Unsupported
                }
            }
            assert_eq!(run_all(&NoneArch, 3), Ok(()));
        }

        #[test]
        fn a_handle_that_names_a_stuck_line_passes_conformance() {
            // A port that can read its shared controller answers a concrete
            // stuck line; conformance accepts that alongside the default.
            struct SpiArch;
            impl WatchdogArch for SpiArch {
                fn request_recovery(&self, _target: CpuId, _kind: WatchdogKind) -> RecoveryOutcome {
                    RecoveryOutcome::AttentionRaised
                }
                fn stuck_interrupt(&self) -> Option<StuckInterrupt> {
                    Some(StuckInterrupt {
                        intid: 37,
                        active: true,
                    })
                }
            }
            assert_eq!(run_all(&SpiArch, 0), Ok(()));
            assert_eq!(
                SpiArch.stuck_interrupt(),
                Some(StuckInterrupt {
                    intid: 37,
                    active: true,
                })
            );
        }

        #[test]
        fn a_handle_that_samples_a_remote_pc_passes_conformance() {
            // A port that can read a target's external-debug PC answers a
            // concrete `Sampled` reading; conformance accepts it (a real
            // sampler may even sample the caller's own CPU) as long as the
            // accessor agrees.
            struct SamplerArch;
            impl WatchdogArch for SamplerArch {
                fn request_recovery(&self, _target: CpuId, _kind: WatchdogKind) -> RecoveryOutcome {
                    RecoveryOutcome::AttentionRaised
                }
                fn remote_pc_sample(&self, target: CpuId) -> RemotePcSample {
                    RemotePcSample::Sampled {
                        pc: 0x1000 + u64::from(target),
                        context: 0,
                    }
                }
            }
            assert_eq!(run_all(&SamplerArch, 0), Ok(()));
            assert_eq!(SamplerArch.remote_pc_sample(2).pc(), Some(0x1002));
        }

        #[test]
        fn a_handle_with_an_empty_sample_reason_fails_conformance() {
            // A non-sampled read must justify itself; an empty reason is the
            // bare-fail this rejects (a caller cannot tell "no channel" from
            // "transiently unavailable" without one).
            struct MuteArch;
            impl WatchdogArch for MuteArch {
                fn request_recovery(&self, _target: CpuId, _kind: WatchdogKind) -> RecoveryOutcome {
                    RecoveryOutcome::Unsupported
                }
                fn remote_pc_sample(&self, _target: CpuId) -> RemotePcSample {
                    RemotePcSample::Unsupported("")
                }
            }
            assert!(run_all(&MuteArch, 0).is_err());
        }

        #[test]
        fn the_default_remote_sample_is_unsupported_with_a_reason() {
            // The trait default (a port that wires no external-debug
            // channel) fails closed with a non-empty reason and no PC.
            let sample = StubArch.remote_pc_sample(1);
            assert!(matches!(sample, RemotePcSample::Unsupported(_)));
            assert_eq!(sample.pc(), None);
            assert!(sample_reason(sample).is_some_and(|r| !r.trim().is_empty()));
        }

        #[test]
        fn a_handle_that_publishes_an_in_flight_line_passes_conformance() {
            // A port whose CPUs publish their acknowledge cookie answers a
            // concrete id; conformance accepts it alongside the default.
            struct CookieArch;
            impl WatchdogArch for CookieArch {
                fn request_recovery(&self, _target: CpuId, _kind: WatchdogKind) -> RecoveryOutcome {
                    RecoveryOutcome::AttentionRaised
                }
                fn in_flight_interrupt(&self, target: CpuId) -> InFlightInterrupt {
                    if target == 0 {
                        InFlightInterrupt::Acknowledged { intid: 27 }
                    } else {
                        InFlightInterrupt::Idle
                    }
                }
            }
            assert_eq!(run_all(&CookieArch, 0), Ok(()));
            assert_eq!(CookieArch.in_flight_interrupt(0).intid(), Some(27));
            assert_eq!(CookieArch.in_flight_interrupt(1).intid(), None);
        }

        #[test]
        fn a_handle_with_an_empty_in_flight_reason_fails_conformance() {
            // A port that publishes nothing must justify itself, exactly as
            // a non-sampled PC read must.
            struct MuteCookieArch;
            impl WatchdogArch for MuteCookieArch {
                fn request_recovery(&self, _target: CpuId, _kind: WatchdogKind) -> RecoveryOutcome {
                    RecoveryOutcome::Unsupported
                }
                fn in_flight_interrupt(&self, _target: CpuId) -> InFlightInterrupt {
                    InFlightInterrupt::Unsupported("")
                }
            }
            assert!(run_all(&MuteCookieArch, 0).is_err());
        }

        fn sample_reason(sample: RemotePcSample) -> Option<&'static str> {
            match sample {
                RemotePcSample::Unavailable(r) | RemotePcSample::Unsupported(r) => Some(r),
                RemotePcSample::Sampled { .. } => None,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kind_tags_are_stable() {
        assert_eq!(WatchdogKind::Soft.tag(), "soft");
        assert_eq!(WatchdogKind::Hard.tag(), "hard");
    }

    #[test]
    fn empty_sample_has_no_task_and_no_context() {
        const _: () = assert!(!WatchdogSample::EMPTY.in_kernel);
        assert_eq!(WatchdogSample::EMPTY.task, WatchdogSample::NO_TASK);
        assert_eq!(WatchdogSample::EMPTY.pc, 0);
        assert_eq!(WatchdogSample::EMPTY.aux, 0);
    }

    #[test]
    fn cadence_is_one_second() {
        assert_eq!(CADENCE_NS, 1_000_000_000);
    }

    #[test]
    fn stuck_interrupt_defaults_to_none() {
        // A port that does not override the query reports no stuck line
        // (fail closed) rather than guessing one.
        struct DefaultArch;
        impl WatchdogArch for DefaultArch {
            fn request_recovery(&self, _target: CpuId, _kind: WatchdogKind) -> RecoveryOutcome {
                RecoveryOutcome::Unsupported
            }
        }
        assert_eq!(DefaultArch.stuck_interrupt(), None);
    }

    #[test]
    fn in_flight_interrupt_defaults_to_unsupported_with_a_reason() {
        // A port that publishes no cookie says so — never `Idle`, which
        // would wrongly clear the target of a missed completion.
        struct DefaultArch;
        impl WatchdogArch for DefaultArch {
            fn request_recovery(&self, _target: CpuId, _kind: WatchdogKind) -> RecoveryOutcome {
                RecoveryOutcome::Unsupported
            }
        }
        let in_flight = DefaultArch.in_flight_interrupt(1);
        assert!(matches!(in_flight, InFlightInterrupt::Unsupported(r) if !r.trim().is_empty()));
        assert_eq!(in_flight.intid(), None);
    }

    #[test]
    fn in_flight_intid_is_only_carried_by_the_acknowledged_reading() {
        assert_eq!(
            InFlightInterrupt::Acknowledged { intid: 30 }.intid(),
            Some(30)
        );
        assert_eq!(InFlightInterrupt::Idle.intid(), None);
        assert_eq!(InFlightInterrupt::Unsupported("no cookie").intid(), None);
    }
}
