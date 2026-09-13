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

use crate::fnptr::FnCell;
use crate::loom_compat::spin_loop;

/// Installed service, doubling as the set-once claim.
static SPIN_SERVICE: FnCell<fn()> = FnCell::empty();

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
    let _ = SPIN_SERVICE.claim(service);
}

/// One round of a spin-wait: discharge whatever a peer is waiting on this
/// CPU for, then hint the CPU that it is spinning.
#[inline]
pub(crate) fn spin_wait() {
    if let Some(service) = SPIN_SERVICE.load() {
        service();
    }
    spin_loop();
}

/// Spin through [`spin_wait`] until `ready` reports the wait over.
///
/// The round is served *before* the condition is first tested, so a failed
/// acquisition discharges one even when the state it was going to watch has
/// already cleared. Without that, a CPU that keeps losing the race to peers
/// who release in between would never reach a round at all, and so never do
/// the work they are blocked on it for.
///
/// Every acquire path in this crate that has to wait spins here, which is
/// what makes the round-per-failed-attempt property one definition rather
/// than a rule each lock restates.
#[inline]
pub(crate) fn spin_until(mut ready: impl FnMut() -> bool) {
    loop {
        spin_wait();
        if ready() {
            return;
        }
    }
}

#[cfg(all(test, not(loom)))]
mod tests {
    use super::{install_service, spin_until, spin_wait, SPIN_SERVICE};
    use crate::SpinLock;
    use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::Arc;

    /// Rounds that reached the installed service, over every thread.
    /// Counted rather than latched because every other spinning test in
    /// this binary runs the service too once it is installed — which is
    /// also why only the `>=` assertions below read it.
    static SERVED: AtomicUsize = AtomicUsize::new(0);

    std::thread_local! {
        /// Rounds this thread's own spins reached the service. The slot and
        /// its service are process-wide, so [`SERVED`] moves under any
        /// concurrently-spinning test and cannot carry an *exact* count;
        /// a per-thread tally can, because only this thread's `spin_*`
        /// calls ever touch it.
        static SERVED_HERE: core::cell::Cell<usize> = const { core::cell::Cell::new(0) };
    }

    /// This thread's own service-round count.
    fn served_here() -> usize {
        SERVED_HERE.with(core::cell::Cell::get)
    }

    /// Set by the service so a holder can wait for proof that the other
    /// thread is *already spinning*, making a contended acquisition
    /// deterministic instead of a matter of timing.
    static WAITER_SPUN: AtomicBool = AtomicBool::new(false);

    fn count_one_service() {
        SERVED.fetch_add(1, Ordering::Relaxed);
        SERVED_HERE.with(|here| here.set(here.get() + 1));
        WAITER_SPUN.store(true, Ordering::Release);
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
        assert!(!SPIN_SERVICE.is_installed());
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

        a_wait_whose_condition_has_already_cleared_still_serves_one_round();
        a_contended_acquisition_serves_rounds_through_the_lock();
    }

    /// A wait whose condition is already clear still owes its peers one
    /// round.
    ///
    /// This is the property the acquire paths are built on and cannot
    /// demonstrate for themselves: reaching it through `SpinLock::lock`
    /// needs a holder's release to land between the waiter's
    /// compare-exchange and its next load, a window with no hook in it and
    /// one no test can schedule. Asserted here against the shared loop both
    /// paths spin through, it needs no scheduling at all — and the count is
    /// exact, so a loop that tested before serving would fail rather than
    /// coincide. Exactness needs the per-thread tally: the process-wide
    /// [`SERVED`] moves under any other test spinning at the same time.
    fn a_wait_whose_condition_has_already_cleared_still_serves_one_round() {
        let before = served_here();
        spin_until(|| true);
        assert_eq!(
            served_here(),
            before + 1,
            "a wait that never had to spin still discharges one round"
        );
    }

    /// A `SpinLock` acquisition that has to wait discharges the service, so
    /// the round is reached from the lock itself and not only from a direct
    /// `spin_wait` call.
    ///
    /// Contention is established by construction rather than by timing: the
    /// holder keeps the lock until the waiter's own spin rounds report it
    /// spinning, so the waiter cannot have taken the lock uncontended. The
    /// holder's wait is bounded so a broken implementation that serves no
    /// round fails the assertion below instead of hanging the suite.
    fn a_contended_acquisition_serves_rounds_through_the_lock() {
        const HOLDER_SPIN_BUDGET: u32 = 100_000;

        let lock = Arc::new(SpinLock::new(0u32));
        let holder_has_it = Arc::new(AtomicBool::new(false));

        WAITER_SPUN.store(false, Ordering::Release);
        let before = SERVED.load(Ordering::Relaxed);

        let (lock_held, held_flag) = (Arc::clone(&lock), Arc::clone(&holder_has_it));
        let holder = std::thread::spawn(move || {
            let mut guard = lock_held.lock();
            *guard = 7;
            held_flag.store(true, Ordering::Release);
            let mut budget = HOLDER_SPIN_BUDGET;
            while !WAITER_SPUN.load(Ordering::Acquire) && budget > 0 {
                budget -= 1;
                core::hint::spin_loop();
            }
        });

        while !holder_has_it.load(Ordering::Acquire) {
            core::hint::spin_loop();
        }
        let waiter = std::thread::spawn(move || *lock.lock());

        holder.join().expect("holder thread");
        assert_eq!(waiter.join().expect("waiter thread"), 7);
        assert!(
            WAITER_SPUN.load(Ordering::Acquire),
            "a waiter that had to spin never reached the service"
        );
        assert!(
            SERVED.load(Ordering::Relaxed) > before,
            "a contended lock acquisition discharged no spin round"
        );
    }
}
