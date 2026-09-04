//! The single spin round every primitive in this crate waits through, and
//! the port-installed work a spinning CPU owes its peers.
//!
//! # Why a spin round is a service point
//!
//! A spinning CPU is by definition waiting on another CPU to make progress.
//! Where the *other* CPU is simultaneously waiting on this one, the wait has
//! to be broken by this CPU doing the peer's work, because nothing else can.
//! The concrete case is a cross-CPU TLB shootdown whose targets must
//! acknowledge in software (x86_64 has no broadcast invalidation): the
//! initiator cannot return until every target acknowledges, and a target
//! inside [`IrqSafeSpinLock::lock`](crate::IrqSafeSpinLock::lock) has masked
//! its own interrupts — so the acknowledge cannot arrive by interrupt. If the
//! lock it is spinning for is one the initiator holds, both spin for ever.
//!
//! `spin_wait` is therefore the one place a spin round is spelled, and it runs
//! the installed service before hinting the CPU. Every primitive in this crate
//! spins through it, which is what makes the property total rather than a list
//! of audited locks: it holds for a primitive added tomorrow and for a caller
//! no registry names.
//!
//! # Per-port cost
//!
//! A port whose invalidation is a hardware broadcast (aarch64 `tlbi …is`) or
//! firmware-served (the riscv64 SBI RFENCE) needs no acknowledge, installs
//! nothing, and pays one load and a branch per spin round.

use core::sync::atomic::{AtomicUsize, Ordering};

use crate::loom_compat::spin_loop;

/// Installed service (`fn()` as a `usize`; `0` = none), doubling as the
/// set-once claim.
///
/// A `core` atomic rather than the `loom_compat` shim on purpose: it is
/// written once during boot and never during a model run, so letting `loom`
/// explore it would multiply every interleaving for a location that cannot
/// race.
static SPIN_SERVICE: AtomicUsize = AtomicUsize::new(0);

/// Install the work a spinning CPU discharges on behalf of its peers.
///
/// The port installs this once during boot, before interrupts are first
/// enabled and before any secondary CPU is started. The service runs on the
/// *calling* CPU from an arbitrary spin round, so it must be reentrant
/// against its own interrupt handler, must take no lock, and must not spin.
///
/// **Set-once.** A later call is refused: swapping a live service could leave
/// a CPU mid-spin calling one that no longer describes the work owed.
pub fn install_service(service: fn()) {
    let _ =
        SPIN_SERVICE.compare_exchange(0, service as usize, Ordering::Release, Ordering::Relaxed);
}

/// One round of a spin-wait: discharge whatever a peer is waiting on this
/// CPU for, then hint the CPU that it is spinning.
#[inline]
pub(crate) fn spin_wait() {
    let raw = SPIN_SERVICE.load(Ordering::Acquire);
    if raw != 0 {
        // SAFETY: a non-zero slot only ever holds a `fn()` pointer
        // round-tripped through `install_service`, published with `Release`
        // against this `Acquire` load.
        let service = unsafe { core::mem::transmute::<usize, fn()>(raw) };
        service();
    }
    spin_loop();
}

#[cfg(all(test, not(loom)))]
mod tests {
    use super::{install_service, spin_wait, Ordering, SPIN_SERVICE};
    use core::sync::atomic::AtomicUsize;

    /// Rounds that reached the installed service. Counted rather than
    /// latched because every other spinning test in this binary runs the
    /// service too once it is installed.
    static SERVED: AtomicUsize = AtomicUsize::new(0);

    fn count_one_service() {
        SERVED.fetch_add(1, Ordering::Relaxed);
    }

    fn never_installed() {
        panic!("the refused service was called");
    }

    /// One test rather than three: the slot is a process-wide set-once
    /// static, so separate `#[test]`s would race for the single claim.
    #[test]
    fn a_spin_round_runs_the_installed_service_and_the_claim_is_set_once() {
        // Before any install a spin round runs nothing. This test is the
        // crate's only installer, so nothing else can have claimed the slot.
        assert_eq!(SPIN_SERVICE.load(Ordering::Relaxed), 0);
        spin_wait();
        assert_eq!(
            SERVED.load(Ordering::Relaxed),
            0,
            "no service installed: a spin round only hints the CPU"
        );

        install_service(count_one_service);
        let before = SERVED.load(Ordering::Relaxed);
        spin_wait();
        spin_wait();
        assert!(
            SERVED.load(Ordering::Relaxed) >= before + 2,
            "each spin round discharges the installed service"
        );

        // Set-once: a later install is refused rather than swapping a live
        // service out from under a CPU that is mid-spin calling it.
        // `never_installed` panics if it is ever reached.
        install_service(never_installed);
        let after = SERVED.load(Ordering::Relaxed);
        spin_wait();
        assert!(
            SERVED.load(Ordering::Relaxed) > after,
            "the originally installed service is still the one in force"
        );
    }
}
