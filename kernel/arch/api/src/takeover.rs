//! Machine-takeover surface of the Arch HAL — the irreducibly
//! per-architecture mechanism that hands the *whole* machine over to a
//! one-way, destructive whole-RAM test (the pre-boot Supervisor's
//! `memtest full`, `plans/NEW-SUPERVISOR.md` §9).
//!
//! # Why this is a HAL slice
//!
//! The in-system RAM test can only test frames it explicitly owns — never
//! the RAM the kernel image, heap, page tables, or stacks occupy — because
//! corrupting the live map would destroy the running kernel. Testing *all*
//! of RAM therefore requires owning the whole machine: stopping every other
//! CPU, masking interrupts, stopping the lockup watchdog
//! (`plans/WATCHDOG.md`), and relocating/flattening paging so a small
//! self-contained test routine can address physical RAM directly — exactly
//! what memtest86 does. Every one of those steps is architecture-specific
//! silicon work (the SMP quiesce channel, the interrupt controller, the
//! MMU/cache regime), so the *mechanism* lives behind this one neutral
//! vocabulary while its bodies stay genuinely per-port. The parallel
//! per-arch implementations of this trait are the deliberate shape of
//! modularity, never collapsed behind `cfg`.
//!
//! The *pattern algorithm* the test runs (moving-inversions,
//! address-in-address) is **not** here: it is arch-neutral and already
//! lives in `tairix_kernel_mem::ramtest::run_destructive`. This slice is
//! only the takeover *mechanism*.
//!
//! # The contract (a strict two-step sequence)
//!
//! The caller drives exactly one ordered sequence, then never returns to
//! normal kernel execution (the only exits are reset/power-off):
//!
//! 1. [`MachineTakeover::quiesce_secondaries`] — stop every *other* logical
//!    CPU into a bounded, controlled halt. It is a legitimate *bounded
//!    handshake* (the machine is being deliberately torn down): the
//!    secondaries spin-halt under a bounded budget, and a CPU that does not
//!    acknowledge within the budget makes the whole takeover **fail closed**
//!    ([`TakeoverError::CpuQuiesceTimeout`]) — it never spins forever. On a
//!    single-CPU machine there is nothing to quiesce and the call succeeds
//!    immediately.
//! 2. [`MachineTakeover::prepare_takeover`] — with the caller now the only
//!    running CPU, mask interrupts, stop the lockup watchdog, relocate the
//!    test routine into a small reserved arena and flatten/identity-map
//!    paging so it can address physical RAM, and perform the cache
//!    maintenance destructive writes require. On success the machine is
//!    ready for the destructive sweep; on a port-specific failure it
//!    **fails closed** ([`TakeoverError::PrepareFailed`]) so the caller can
//!    report and stay in the REPL rather than half-tear-down and wedge.
//!
//! A port that cannot take the machine over (no quiesce/relocate primitive
//! wired) simply is not installed on [`crate`]-side glue at all
//! (`KernelArch::machine_takeover` returns `None`), or, if partially wired,
//! reports [`TakeoverError::NotSupported`] fail-safe. It never panics
//! (`plans/NEW-SUPERVISOR.md` §9.1) and never half-completes.
//!
//! # Why the host conformance vertical proves only the neutral vocabulary
//!
//! Unlike [`crate::smp`], a takeover has **no harmless input**: there is no
//! argument that makes [`MachineTakeover::prepare_takeover`] a no-op, so it
//! cannot be run against a *supported* real port (or the host) without
//! flattening paging and destroying execution. The host [`conformance`]
//! vertical therefore proves the observable, side-effect-free half of the
//! contract against an **unsupported** double — the calls are object-safe,
//! total (never panic), and **fail closed** with
//! [`TakeoverError::NotSupported`] — exactly the behaviour `wasm32` and the
//! mock ports exhibit. The real per-port takeover is proven end-to-end by
//! the destructive-memtest QEMU vertical (`plans/NEW-SUPERVISOR.md` §9
//! Stage E), whose guest ends in a reset rather than resuming boot.

use crate::CpuId;

/// Why a [`MachineTakeover`] step was refused or failed.
///
/// The neutral failure surface every port maps its native error onto, so
/// the architecture-neutral caller handles one set of outcomes and always
/// **fails closed** (stays in the Supervisor REPL, changes nothing). A
/// port's richer detail is preserved in the payloads for the audit log.
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
#[non_exhaustive]
pub enum TakeoverError {
    /// A secondary CPU did not acknowledge the quiesce request within the
    /// bounded handshake budget. The takeover is abandoned before any
    /// destructive step, so the machine is left running and the operator is
    /// told which core could not be stopped (fail closed).
    CpuQuiesceTimeout {
        /// The logical CPU that failed to halt within the budget.
        cpu: CpuId,
    },
    /// This port has no takeover mechanism wired (`wasm32`, the mock ports,
    /// or a bare-metal port before its takeover slice lands). Surfaced
    /// fail-safe: the caller reports "not supported" and stays in the REPL.
    NotSupported,
    /// The port could not complete [`MachineTakeover::prepare_takeover`]
    /// (it could not relocate/flatten paging, stop the watchdog, or perform
    /// the required cache maintenance). Carries the port's raw status for
    /// the audit log (`0` where the mechanism reports only failure). The
    /// caller fails closed rather than running the test on a half-prepared
    /// machine.
    PrepareFailed(i64),
}

impl TakeoverError {
    /// Stable cause string for audit records (never carries a payload
    /// value — the numeric detail is logged as a separate field).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::CpuQuiesceTimeout { .. } => "takeover_cpu_quiesce_timeout",
            Self::NotSupported => "takeover_not_supported",
            Self::PrepareFailed(_) => "takeover_prepare_failed",
        }
    }
}

/// Hand the whole machine over to a one-way destructive whole-RAM test.
///
/// Implemented once per port (the SMP quiesce channel, the interrupt
/// controller, the MMU/cache regime are all per-architecture) and held by
/// the architecture-neutral caller behind a `&dyn MachineTakeover`. The two
/// methods are driven in order — [`Self::quiesce_secondaries`] then
/// [`Self::prepare_takeover`] — and the caller never returns to normal
/// execution afterwards.
///
/// # Required semantics
///
/// * Both methods **fail closed** and **never panic** for any state: an
///   unwired port returns [`TakeoverError::NotSupported`]; a quiesce that
///   times out returns [`TakeoverError::CpuQuiesceTimeout`]; a preparation
///   that cannot complete returns [`TakeoverError::PrepareFailed`].
/// * On any error the machine must be left **running and recoverable** — no
///   destructive step taken, no half-torn-down state that wedges the
///   caller. A port must not begin flattening paging until it can complete
///   it.
/// * [`Self::quiesce_secondaries`] is a *bounded* handshake, never an
///   unbounded spin: it succeeds only once every other CPU is halted (or
///   there are none), and otherwise times out fail-closed within a bounded
///   budget.
pub trait MachineTakeover {
    /// Stop every logical CPU other than the caller into a bounded,
    /// controlled halt, so the caller becomes the machine's only running
    /// CPU.
    ///
    /// # Errors
    ///
    /// Returns [`TakeoverError::CpuQuiesceTimeout`] naming the first CPU
    /// that did not halt within the bounded budget, or
    /// [`TakeoverError::NotSupported`] if the port has no quiesce channel.
    /// On either error no destructive state change has occurred.
    ///
    /// # Safety
    ///
    /// The caller must guarantee the machine is being deliberately torn
    /// down (the confirmed, audited `memtest full` path): after this call
    /// the halted secondaries no longer make progress, so any kernel state
    /// they held (locks, in-flight work) is abandoned. It must be the last
    /// SMP operation before [`Self::prepare_takeover`].
    unsafe fn quiesce_secondaries(&self) -> Result<(), TakeoverError>;

    /// With the caller the only running CPU, mask interrupts, stop the
    /// lockup watchdog, relocate the test routine into a reserved arena and
    /// flatten paging so it can address physical RAM, and perform the cache
    /// maintenance destructive writes require.
    ///
    /// # Errors
    ///
    /// Returns [`TakeoverError::NotSupported`] if the port has no relocate
    /// primitive, or [`TakeoverError::PrepareFailed`] carrying the port's
    /// raw status if the preparation could not complete. On either error
    /// the machine is left in a running, recoverable state.
    ///
    /// # Safety
    ///
    /// The caller must have already driven [`Self::quiesce_secondaries`] to
    /// success, so no other CPU is running. On success the machine is no
    /// longer safe for normal kernel execution — only the destructive test
    /// routine over its reserved arena may run, and the only exits are
    /// reset/power-off.
    unsafe fn prepare_takeover(&self) -> Result<(), TakeoverError>;
}

/// The machine-takeover conformance vertical.
///
/// Like [`crate::smp::conformance`] it names only the trait and runs on the
/// host. Because a takeover has no harmless input (see the module docs), it
/// proves the neutral vocabulary against an **unsupported** handle: both
/// steps must be object-safe, total (never panic), and **fail closed** with
/// [`TakeoverError::NotSupported`] — the behaviour every not-yet-wired port
/// exhibits. The real per-port takeover is proven by the destructive-memtest
/// QEMU vertical (`plans/NEW-SUPERVISOR.md` §9 Stage E).
pub mod conformance {
    use super::{MachineTakeover, TakeoverError};

    /// Run the [`MachineTakeover`] conformance suite against an
    /// **unsupported** `takeover` handle.
    ///
    /// Asserts that both steps fail closed with
    /// [`TakeoverError::NotSupported`] without panicking, both directly and
    /// behind the object-safe erasure the kernel holds the handle through.
    ///
    /// It must only be given a handle whose takeover is *not* wired (the
    /// `wasm32`/mock case): a supported port's methods destroy the machine
    /// and cannot be conformance-tested this way — that is what the Stage E
    /// QEMU vertical is for.
    ///
    /// # Panics
    ///
    /// Panics (failing the conformance test) if either step returns
    /// anything other than [`TakeoverError::NotSupported`].
    pub fn run_unsupported<T: MachineTakeover + ?Sized>(takeover: &T) {
        // SAFETY: the handle is an unsupported port, so both methods take no
        // platform action and merely report `NotSupported`; the tear-down
        // preconditions are vacuously satisfied.
        let quiesced = unsafe { takeover.quiesce_secondaries() };
        assert_eq!(
            quiesced,
            Err(TakeoverError::NotSupported),
            "an unsupported port must fail closed from quiesce_secondaries",
        );
        // SAFETY: as above — no other CPU was actually stopped and the
        // method reports `NotSupported` without touching paging.
        let prepared = unsafe { takeover.prepare_takeover() };
        assert_eq!(
            prepared,
            Err(TakeoverError::NotSupported),
            "an unsupported port must fail closed from prepare_takeover",
        );
    }

    #[cfg(test)]
    mod tests {
        use super::super::{MachineTakeover, TakeoverError};
        use super::run_unsupported;
        use crate::CpuId;

        /// The honest unsupported double: both steps fail closed without
        /// touching any hardware, exactly as `wasm32`/mock ports do.
        struct UnsupportedTakeover;

        impl MachineTakeover for UnsupportedTakeover {
            unsafe fn quiesce_secondaries(&self) -> Result<(), TakeoverError> {
                Err(TakeoverError::NotSupported)
            }
            unsafe fn prepare_takeover(&self) -> Result<(), TakeoverError> {
                Err(TakeoverError::NotSupported)
            }
        }

        #[test]
        fn suite_requires_fail_closed_when_unsupported() {
            run_unsupported(&UnsupportedTakeover);
            // And over the object-safe erasure the kernel holds it behind.
            let erased: &dyn MachineTakeover = &UnsupportedTakeover;
            run_unsupported(erased);
        }

        /// A faithful *supported* double, used only to prove the neutral
        /// vocabulary maps a port's outcomes without panicking (it takes no
        /// real platform action). It cannot be fed to `run_unsupported` —
        /// that helper is only for the unsupported path.
        #[derive(Default)]
        struct FakeTakeover {
            /// Which CPU, if any, refuses to quiesce.
            stuck_cpu: Option<CpuId>,
        }

        impl MachineTakeover for FakeTakeover {
            unsafe fn quiesce_secondaries(&self) -> Result<(), TakeoverError> {
                match self.stuck_cpu {
                    Some(cpu) => Err(TakeoverError::CpuQuiesceTimeout { cpu }),
                    None => Ok(()),
                }
            }
            unsafe fn prepare_takeover(&self) -> Result<(), TakeoverError> {
                Ok(())
            }
        }

        #[test]
        fn error_causes_are_stable_and_distinct() {
            assert_eq!(
                TakeoverError::NotSupported.as_str(),
                "takeover_not_supported"
            );
            assert_eq!(
                TakeoverError::CpuQuiesceTimeout { cpu: 3 }.as_str(),
                "takeover_cpu_quiesce_timeout",
            );
            assert_eq!(
                TakeoverError::PrepareFailed(-7).as_str(),
                "takeover_prepare_failed",
            );
            // The quiesce-timeout cause does not depend on which CPU stuck.
            assert_eq!(
                TakeoverError::CpuQuiesceTimeout { cpu: 3 }.as_str(),
                TakeoverError::CpuQuiesceTimeout { cpu: 9 }.as_str(),
            );
        }

        #[test]
        fn faithful_double_maps_outcomes_without_panic() {
            let ok = FakeTakeover::default();
            // SAFETY: the double takes no platform action.
            assert_eq!(unsafe { ok.quiesce_secondaries() }, Ok(()));
            // SAFETY: as above.
            assert_eq!(unsafe { ok.prepare_takeover() }, Ok(()));

            let stuck = FakeTakeover { stuck_cpu: Some(2) };
            // SAFETY: as above — reports a timeout, takes no action.
            assert_eq!(
                unsafe { stuck.quiesce_secondaries() },
                Err(TakeoverError::CpuQuiesceTimeout { cpu: 2 }),
            );
        }
    }
}
