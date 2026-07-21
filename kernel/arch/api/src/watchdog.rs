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
    /// A directed non-maskable attention interrupt was raised on the
    /// target (a hard lockup: the port asked the wedged core to dump its
    /// live state and, where possible, abandon the offending task).
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
    use super::{CpuId, RecoveryOutcome, WatchdogArch, WatchdogKind};

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
        use super::super::{RecoveryOutcome, StuckInterrupt, WatchdogArch, WatchdogKind};
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
}
