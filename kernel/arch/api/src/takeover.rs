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
//! # The contract (one irreversible operation that never returns)
//!
//! A machine takeover cannot be expressed as a "prepare, then let the caller
//! sweep on its own stack" pair: the destructive sweep overwrites **all** of
//! RAM, including whatever stack the caller was running on, so the moment the
//! sweep touches that stack the caller's return address and locals are gone.
//! The Supervisor REPL (and therefore `memtest full`) runs on a kernel-service
//! kthread stack allocated from *usable* RAM, so a driver that swept RAM and
//! then called `reboot()` on that same stack would corrupt its own execution
//! mid-sweep and crash rather than reset. The takeover is therefore a
//! **single** operation the port owns end to end:
//!
//! [`MachineTakeover::take_over`] performs, in order and without ever handing
//! control back to normal kernel code:
//!
//! 1. **Quiesce every other logical CPU** into a bounded, controlled halt. It
//!    is a legitimate *bounded handshake* (the machine is being deliberately
//!    torn down): the secondaries spin-halt under a bounded budget, and a CPU
//!    that does not acknowledge within the budget makes the whole takeover
//!    **fail closed** ([`TakeoverError::CpuQuiesceTimeout`]) — it never spins
//!    forever. On a single-CPU machine there is nothing to quiesce and this
//!    step succeeds immediately.
//! 2. **Mask interrupts and stop the lockup watchdog** (`plans/WATCHDOG.md`),
//!    so nothing preempts or resets the now-solitary CPU during the sweep.
//! 3. **Flatten paging** so physical RAM is addressed directly and no
//!    page-table walk depends on RAM the sweep is about to destroy (riscv64
//!    `satp = 0` bare mode; aarch64 `SCTLR_EL1.M = 0`; x86_64 an
//!    identity page table rooted in a reserved arena), and perform the cache
//!    maintenance destructive writes require.
//! 4. **Switch onto a reserved stack the sweep will never overwrite** and run
//!    the caller-supplied `sweep` — the architecture-neutral phase that
//!    destructively tests every *usable* frame and renders progress. The port
//!    guarantees the sweep executes only from memory the sweep does not
//!    destroy (its code lives in the reserved kernel image; its stack and all
//!    state it reads are in reserved memory — see the `sweep` safety
//!    contract on [`MachineTakeover::take_over`]).
//! 5. **Test the region the sweep executed from** — the kernel image and the
//!    stack it ran on, which `sweep` necessarily could not touch — with a
//!    small self-contained, relocated per-port stub that never touches the
//!    firmware region (overwriting it would break the reset path), then
//!    **reset** the machine. This is the memtest86-complete "all of RAM"
//!    coverage: only the tiny relocated stub arena and the firmware are
//!    excluded, and both are unavoidable.
//!
//! On any pre-destructive refusal — no takeover mechanism wired, a quiesce
//! timeout, or a preparation the port could not complete — `take_over`
//! **returns** the [`TakeoverError`] with the machine left running and
//! recoverable, so the caller reports it and stays in the REPL. It never
//! panics (`plans/NEW-SUPERVISOR.md` §9.1) and never half-completes.
//!
//! A port that has no takeover mechanism is simply not installed on
//! [`crate`]-side glue (`KernelArch::machine_takeover` returns `None`) or
//! returns [`TakeoverError::NotSupported`].
//!
//! # Why the host conformance vertical proves only the neutral vocabulary
//!
//! Unlike [`crate::smp`], a takeover has **no harmless input**: there is no
//! argument that makes [`MachineTakeover::take_over`] a no-op, so it cannot be
//! run against a *supported* real port (or the host) without flattening paging
//! and destroying execution. The host [`conformance`] vertical therefore
//! proves the observable, side-effect-free half of the contract against an
//! **unsupported** double — the call is object-safe, total (never panics), and
//! **fails closed** with [`TakeoverError::NotSupported`] *without ever running
//! the sweep* — exactly the behaviour `wasm32` and the not-yet-wired ports
//! exhibit. The real per-port takeover is proven end-to-end by the
//! destructive-memtest QEMU vertical (`plans/NEW-SUPERVISOR.md` §9 Stage E),
//! whose guest ends in a reset rather than resuming boot.

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
    /// The port could not complete the pre-sweep preparation inside
    /// [`MachineTakeover::take_over`] (it could not flatten/identity-map
    /// paging, stop the watchdog, or perform the required cache maintenance).
    /// Carries the port's raw status for the audit log (`0` where the
    /// mechanism reports only failure). The caller fails closed rather than
    /// running the test on a half-prepared machine.
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
/// controller, the MMU/cache regime, and the relocation/reset path are all
/// per-architecture) and held by the architecture-neutral caller behind a
/// `&dyn MachineTakeover`. The single [`Self::take_over`] operation owns the
/// entire irreversible sequence; on success it never returns.
///
/// # Required semantics
///
/// * [`Self::take_over`] **fails closed** and **never panics** for any state:
///   an unwired port returns [`TakeoverError::NotSupported`]; a quiesce that
///   times out returns [`TakeoverError::CpuQuiesceTimeout`]; a preparation
///   that cannot complete returns [`TakeoverError::PrepareFailed`].
/// * On any refusal the machine must be left **running and recoverable** — no
///   destructive step taken, no half-torn-down state that wedges the caller,
///   and `sweep` must **not** have been called. A port must not begin
///   flattening paging until it can complete the whole sequence.
/// * The quiesce is a *bounded* handshake, never an unbounded spin: it
///   succeeds only once every other CPU is halted (or there are none), and
///   otherwise times out fail-closed within a bounded budget.
pub trait MachineTakeover {
    /// Take the whole machine over and run the one-way destructive whole-RAM
    /// test to completion, then reset. **Never returns on success.**
    ///
    /// The port drives, in order and without handing control back to normal
    /// kernel code: quiesce every other CPU (bounded, fail-closed), mask
    /// interrupts, stop the lockup watchdog, flatten paging so physical RAM
    /// is addressed directly, switch onto a reserved stack the sweep cannot
    /// overwrite, call `sweep` (the architecture-neutral phase that
    /// destructively tests every *usable* frame and renders progress), then
    /// test the region the sweep executed from — the kernel image and the
    /// stack it ran on — with a small relocated per-port stub that never
    /// touches the firmware, and finally reset. See the module docs for the
    /// full contract.
    ///
    /// # Returns / errors
    ///
    /// Returns **only** when the takeover did not proceed, carrying the
    /// reason: [`TakeoverError::NotSupported`] (no mechanism wired),
    /// [`TakeoverError::CpuQuiesceTimeout`] (a secondary would not halt), or
    /// [`TakeoverError::PrepareFailed`] (the port could not complete the
    /// pre-sweep preparation). On every such return the machine is unchanged
    /// and `sweep` was not called. On success it never returns (the machine
    /// resets).
    ///
    /// # Safety
    ///
    /// The caller must guarantee this is the confirmed, audited `memtest
    /// full` path — the operator has decided to tear the machine down, so
    /// stopping every other CPU, flattening paging, and overwriting all of
    /// RAM are the intended, deliberate actions.
    ///
    /// The caller must further guarantee that `sweep` — its code, the closure
    /// environment behind the `&mut dyn FnMut()`, and every datum it reads or
    /// writes-through (the boot memory map, the physical-map descriptor, the
    /// console state it renders progress to) — resides in memory the sweep
    /// does **not** destroy. In practice that means the reserved kernel image
    /// and boot heap, never a frame handed out by the frame allocator: a
    /// closure environment left on the usable-RAM kthread stack would be
    /// overwritten mid-sweep. The port satisfies its half by switching onto a
    /// reserved stack before the call; the caller satisfies its half by
    /// constructing a `sweep` whose captured state is `'static` and reserved.
    unsafe fn take_over(&self, sweep: &mut dyn FnMut()) -> TakeoverError;
}

/// The machine-takeover conformance vertical.
///
/// Like [`crate::smp::conformance`] it names only the trait and runs on the
/// host. Because a takeover has no harmless input (see the module docs), it
/// proves the neutral vocabulary against an **unsupported** handle:
/// [`MachineTakeover::take_over`] must be object-safe, total (never panic),
/// and **fail closed** with [`TakeoverError::NotSupported`] *without ever
/// running the sweep* — the behaviour every not-yet-wired port exhibits. The
/// real per-port takeover is proven by the destructive-memtest QEMU vertical
/// (`plans/NEW-SUPERVISOR.md` §9 Stage E).
pub mod conformance {
    use super::{MachineTakeover, TakeoverError};

    /// Run the [`MachineTakeover`] conformance suite against an
    /// **unsupported** `takeover` handle.
    ///
    /// Asserts that [`MachineTakeover::take_over`] fails closed with
    /// [`TakeoverError::NotSupported`] without panicking and **without
    /// invoking the sweep** — both directly and behind the object-safe
    /// erasure the kernel holds the handle through. A sweep that ran on an
    /// unsupported port would mean the machine was torn down without the
    /// destructive mechanism actually being ready, so the double asserts the
    /// sweep is never called.
    ///
    /// It must only be given a handle whose takeover is *not* wired (the
    /// `wasm32`/mock case): a supported port destroys the machine and cannot
    /// be conformance-tested this way — that is what the Stage E QEMU vertical
    /// is for.
    ///
    /// # Panics
    ///
    /// Panics (failing the conformance test) if `take_over` returns anything
    /// other than [`TakeoverError::NotSupported`], or if it invokes `sweep`.
    pub fn run_unsupported<T: MachineTakeover + ?Sized>(takeover: &T) {
        let mut swept = false;
        // SAFETY: the handle is an unsupported port, so `take_over` takes no
        // platform action and merely reports `NotSupported`; the tear-down
        // preconditions are vacuously satisfied and the sweep is never run.
        let outcome = unsafe { takeover.take_over(&mut || swept = true) };
        assert_eq!(
            outcome,
            TakeoverError::NotSupported,
            "an unsupported port must fail closed from take_over",
        );
        assert!(
            !swept,
            "an unsupported port must not run the destructive sweep",
        );
    }

    #[cfg(test)]
    mod tests {
        use super::super::{MachineTakeover, TakeoverError};
        use super::run_unsupported;

        /// The honest unsupported double: `take_over` fails closed without
        /// touching any hardware or running the sweep, exactly as
        /// `wasm32`/mock ports do.
        struct UnsupportedTakeover;

        impl MachineTakeover for UnsupportedTakeover {
            unsafe fn take_over(&self, _sweep: &mut dyn FnMut()) -> TakeoverError {
                TakeoverError::NotSupported
            }
        }

        #[test]
        fn suite_requires_fail_closed_when_unsupported() {
            run_unsupported(&UnsupportedTakeover);
            // And over the object-safe erasure the kernel holds it behind.
            let erased: &dyn MachineTakeover = &UnsupportedTakeover;
            run_unsupported(erased);
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
    }
}
