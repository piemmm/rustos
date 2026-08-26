//! A scheduler-blocking mutual-exclusion lock (a *sleeping* mutex).
//!
//! Every lock in `lib/sync` ([`SpinLock`](tairix_sync::SpinLock),
//! [`RwLock`](tairix_sync::RwLock), [`McsLock`](tairix_sync::McsLock), …)
//! *spins* on contention. That is correct only for a short critical section
//! whose holder never gives up the CPU. A `SleepLock` is the opposite: its
//! critical section may **park** — most importantly it may be held across a
//! block-device completion-IRQ wait (`Block::read_blocks` parks the calling
//! task on the controller interrupt). A spin lock held across such a park is
//! a defect: a second contender on the same CPU deadlocks, and on another CPU
//! it busy-spins on a holder that is asleep (forbidden busy-waiting). A
//! `SleepLock` instead **parks the contender off the run queue** and wakes it
//! when the holder releases — no spinning while a holder sleeps.
//!
//! This is the per-mount serialisation primitive the userland filesystem
//! path needs: each `fs_*` operation runs in the calling task's own context
//! and takes the mount's `SleepLock` for the duration of one operation
//! (including the device park), so operations on *different* mounts proceed
//! fully in parallel while operations on one mount are serialised without a
//! single global server task.
//!
//! # Why this lives in `kernel/core`, not `lib/sync`
//!
//! Parking and waking a task is the scheduler's job, and the layering forbids
//! a `lib/*` crate from depending on the kernel. So, unlike the spinning
//! primitives, a sleeping lock cannot live in `lib/sync`: it reaches the
//! scheduler through the installed [`WaitQueueArch`](crate::WaitQueueArch)
//! hook (for the current CPU, the current task, and `unpark`) and the
//! kernel's `reschedule_current` park primitive (to park the caller),
//! exactly as the console-read and process-wait blocking backings do.
//!
//! # No lost wake-ups
//!
//! The acquire path closes the release/park race with the same discipline
//! the other kernel waiters use: the contender **registers on the wait queue
//! before it re-tests** the lock, so a release in the window between its
//! failed fast-path attempt and its park cannot be missed — the releaser's
//! wake finds the registered task, and the scheduler's wake-pending token
//! turns an `unpark` that races a not-yet-committed park into a re-ready
//! rather than a lost wake-up. Each woken contender re-tests and either
//! acquires or parks again, so a wake meant for another contender is a
//! harmless spurious wake.
//!
//! # Fairness
//!
//! Waiters retain FIFO registration order. Release hands ownership directly
//! to the oldest task while keeping the lock closed to fresh contenders, then
//! wakes only that task. This avoids both a thundering herd and barging: a
//! long-waiting disk operation cannot be perpetually displaced by newer work.
//!
//! # The uncontended path is two atomics
//!
//! Acquire and release are one compare-exchange each when nobody is waiting,
//! and the wait queue is not touched at all. That matters because this lock
//! serialises *every* block-device operation on a shared disk
//! (`crate::shared_block`), so a filesystem read walking a file pays one
//! acquire/release per device operation — and a device operation served from
//! the block cache above the disk is a memcpy, not a park.
//!
//! What makes it possible is that contention lives **in the lock word**: a
//! contender sets a `CONTENDED` bit there before it parks, so the releaser's
//! single `LOCKED -> 0` compare-exchange fails precisely when a wake is
//! owed. Flag and lock bit share one location, so their modification order is
//! total and no store/load fence is needed: a contender that publishes before
//! the release makes that release take the wake path, and one that publishes
//! after it observes the lock already free and never parks. Keeping the
//! "is anyone waiting?" answer in a separate structure would have needed the
//! wait-queue lock (and a `BTreeMap` lookup) on every release to learn that
//! nobody was.

use core::cell::UnsafeCell;
use core::ops::{Deref, DerefMut};
use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use crate::dispatch_slot::RescheduleAction;
use crate::kthread::reschedule_current;
use crate::waitq::{wait_arch, WaitQueue, NO_DEADLINE};

/// Lock-word bit: the lock is held.
const LOCKED: u32 = 1 << 0;

/// Lock-word bit: a contender registered on the wait queue while the lock
/// was held, so the release owes it a wake.
///
/// Set by the contender *before* it parks and cleared only by a release
/// that finds the queue genuinely empty. It may therefore linger over a
/// handoff, or over a contender that found the lock free after publishing
/// it — each costs one release that consults the wait queue and finds
/// nothing, and clears the bit. Clearing it speculatively instead would
/// race a second contender's flag and lose its wake.
const CONTENDED: u32 = 1 << 1;

/// A mutual-exclusion lock whose contenders **park** off the run queue
/// instead of spinning, so its critical section may be held across a task
/// park (e.g. a block-device completion-IRQ wait).
///
/// Construct one with [`SleepLock::new`] and acquire it with
/// [`lock`](SleepLock::lock) (blocking) or [`try_lock`](SleepLock::try_lock)
/// (non-blocking). The returned [`SleepGuard`] dereferences to the protected
/// value and releases the lock — waking a parked contender — when dropped.
///
/// Acquiring this lock may park the caller, so it must be taken only from a
/// context that can be rescheduled (a task / kthread), never from an
/// interrupt handler.
pub struct SleepLock<T: ?Sized> {
    /// [`LOCKED`] while held, plus [`CONTENDED`] once a contender has
    /// registered to park. The single point of mutual exclusion; every
    /// acquire is a `compare_exchange` against it.
    state: AtomicU32,
    /// FIFO ownership handed directly to one parked task. Zero means no
    /// handoff is outstanding; scheduler task ids never use zero.
    handoff: AtomicU64,
    /// Contenders parked waiting for the holder to release. Reuses the one
    /// kernel wait-queue definition (its register/wake/unpark bookkeeping is
    /// tested in `crate::waitq`); this lock adds only the acquire/release
    /// policy on top.
    waiters: WaitQueue,
    /// The protected value. Access is guarded by [`LOCKED`]: a live
    /// [`SleepGuard`] is proof of exclusive ownership.
    data: UnsafeCell<T>,
}

// SAFETY: `SleepLock` is a mutual-exclusion boundary: `lock`/`try_lock` hand
// out a `SleepGuard` only after a successful `compare_exchange` that sets
// `LOCKED`, so at most one thread ever holds `&mut`-equivalent access to
// `data` at a time, and ownership is transferred (not shared) on release. It
// is therefore safe to send the lock (and the value it guards) between threads
// when `T: Send`, and to share `&SleepLock` across threads when `T: Send`
// (sharing the reference only ever yields serialised, exclusive access to
// `T`). `T` need not be `Sync` because the guard never hands out concurrent
// `&T`.
unsafe impl<T: ?Sized + Send> Send for SleepLock<T> {}
// SAFETY: as for `Send` above — `&SleepLock` only ever yields exclusive
// access to `T` through the `LOCKED` gate, never concurrent shared access.
unsafe impl<T: ?Sized + Send> Sync for SleepLock<T> {}

impl<T> SleepLock<T> {
    /// A new unlocked `SleepLock` guarding `value`.
    ///
    /// `const` so a lock may be placed in a `static` or built in a `const`
    /// context, like the spinning primitives.
    #[must_use]
    pub const fn new(value: T) -> Self {
        Self {
            state: AtomicU32::new(0),
            handoff: AtomicU64::new(0),
            waiters: WaitQueue::new(),
            data: UnsafeCell::new(value),
        }
    }

    /// Consume the lock and return the guarded value.
    ///
    /// Takes `self` by value, so no other reference can exist and no locking
    /// is required.
    pub fn into_inner(self) -> T {
        self.data.into_inner()
    }
}

impl<T: ?Sized> SleepLock<T> {
    /// Try to acquire the lock without blocking.
    ///
    /// Returns the [`SleepGuard`] on success, or [`None`] if the lock is
    /// currently held — never parks, so it is safe from any context.
    ///
    /// The contention bit is carried through an acquire rather than cleared:
    /// waiters may still be queued, and only a release that has looked at
    /// the queue may say otherwise.
    #[must_use]
    pub fn try_lock(&self) -> Option<SleepGuard<'_, T>> {
        let mut observed = self.state.load(Ordering::Relaxed);
        loop {
            if observed & LOCKED != 0 {
                return None;
            }
            match self.state.compare_exchange_weak(
                observed,
                observed | LOCKED,
                Ordering::Acquire,
                Ordering::Relaxed,
            ) {
                Ok(_) => return Some(SleepGuard { lock: self }),
                Err(current) => observed = current,
            }
        }
    }

    /// Acquire the lock, parking the caller off the run queue while it is
    /// held by someone else.
    ///
    /// Blocks until the lock is acquired; the returned [`SleepGuard`]
    /// releases it on drop. Must be called from a reschedulable context (a
    /// task / kthread), never an interrupt handler.
    pub fn lock(&self) -> SleepGuard<'_, T> {
        loop {
            if let Some(guard) = self.try_lock() {
                return guard;
            }
            if let Some(task) = self.park_until_released() {
                if self.claim_handoff(task) {
                    return SleepGuard { lock: self };
                }
            }
        }
    }

    /// Claim direct FIFO ownership granted to `task` by the prior holder.
    fn claim_handoff(&self, task: u64) -> bool {
        self.handoff
            .compare_exchange(task, 0, Ordering::Acquire, Ordering::Relaxed)
            .is_ok()
    }

    /// One park attempt on a contended acquire: register the current caller,
    /// re-test, and park off the run queue until the holder releases.
    ///
    /// Returns after a wake (or without parking, if the lock freed in the
    /// register window); the [`lock`](Self::lock) loop then re-attempts the
    /// fast-path acquire. Registering **before** the re-test is what makes
    /// the release/park race lossless (see the module docs).
    fn park_until_released(&self) -> Option<u64> {
        // The scheduler hook supplies the current CPU, the current task to
        // register, and the `unpark` the releaser uses. Without it (before
        // the boot path installs the hook, or in a host test of an unrelated
        // path) no task can be parked — and no genuine contention can exist
        // either, since parking needs a live scheduler — so retry. This is
        // not a steady-state busy-poll: it is reachable only when there is no
        // scheduler to contend on.
        let Some(hook) = wait_arch() else {
            core::hint::spin_loop();
            return None;
        };
        let Some(cpu) = hook.current_cpu() else {
            core::hint::spin_loop();
            return None;
        };
        let Some(task) = hook.current_task(cpu) else {
            core::hint::spin_loop();
            return None;
        };
        // Register before the re-test so a release between the failed
        // fast-path attempt and the park is never missed: the releaser's
        // wake finds this task, and the scheduler's wake-pending token
        // converts an `unpark` racing a not-yet-committed park into a
        // re-ready.
        self.waiters.register(task, NO_DEADLINE);
        // Publish contention *in the lock word*, then re-test it in the same
        // operation. Because the flag and the lock bit are one location, the
        // releaser's single-CAS fast path cannot complete without observing
        // this, so no fence is needed here: either the flag lands first and
        // the release takes the wake path, or the release lands first and the
        // observed value has `LOCKED` clear, in which case the holder is gone
        // — do not park, drop the registration, and let the caller re-attempt
        // the fast path.
        if self.state.fetch_or(CONTENDED, Ordering::AcqRel) & LOCKED == 0 {
            self.waiters.deregister(task);
            return None;
        }
        // Park off the run queue. Every dispatched kthread — a user task in
        // its syscall trap and a kernel service kthread body alike — has a
        // published resume handle, so this parks any real contender. A
        // `false` return means the caller is not a dispatched kthread at all
        // (a host test, or the pre-dispatch boot flow): there is then no
        // scheduler to park on and no real contention, so drop the
        // registration and retry rather than park into the void.
        if !reschedule_current(cpu, RescheduleAction::Park) {
            self.waiters.deregister(task);
            core::hint::spin_loop();
            return None;
        }
        // Woken: stop waiting and let `lock` claim a direct handoff when this
        // task was the designated FIFO successor. A spurious wake has no
        // handoff and simply re-enters the normal acquire/park loop.
        self.waiters.deregister(task);
        Some(task)
    }

    /// Release the lock and wake the oldest parked contender.
    ///
    /// Called only by [`SleepGuard`]'s `Drop`. Uncontended — no contender has
    /// published [`CONTENDED`] — this is one compare-exchange and the wait
    /// queue is never consulted. Otherwise ownership is published directly to
    /// the oldest task and [`LOCKED`] remains set, so a fresh contender cannot
    /// barge before the wake runs; the designated waiter's Acquire claim
    /// observes the prior holder's critical-section writes.
    fn release(&self) {
        if self
            .state
            .compare_exchange(LOCKED, 0, Ordering::Release, Ordering::Relaxed)
            .is_ok()
        {
            return;
        }
        self.release_contended(wait_arch());
    }

    /// The release path a published [`CONTENDED`] flag selects, factored so
    /// host tests can drive the direct handoff state machine without
    /// installing the process-global boot hook.
    ///
    /// A stale flag (a handoff's successor, or a contender that found the lock
    /// already free) reaches here with an empty queue and simply clears the
    /// word, which is what stops the bit lingering.
    fn release_contended(&self, hook: Option<&dyn crate::waitq::WaitQueueArch>) {
        if let Some(hook) = hook {
            // A waiter that vanished between observation and wake is passed
            // over for the next-oldest, never taken as licence to unlock with
            // the queue still occupied: that would strand every remaining
            // contender, since the word is cleared with it and no later
            // release would owe them a wake. Each pass-over removes a
            // candidate that is already absent, so this ends at the first
            // live waiter or an empty queue.
            while let Some(task) = self.waiters.oldest_task() {
                // Keep `LOCKED` set while ownership is in flight, preventing a
                // fresh contender from barging ahead of the FIFO waiter. The
                // waiter's Acquire claim publishes the prior holder's
                // critical-section writes before it receives the guard.
                self.handoff.store(task, Ordering::Release);
                if self.waiters.wake_task(hook, task) {
                    return;
                }
                self.handoff.store(0, Ordering::Relaxed);
            }
        }
        self.state.store(0, Ordering::Release);
    }
}

/// An RAII proof of exclusive ownership of a [`SleepLock`]'s value.
///
/// Dereferences to the guarded `T` and releases the lock — waking a parked
/// contender — when dropped.
pub struct SleepGuard<'a, T: ?Sized> {
    lock: &'a SleepLock<T>,
}

impl<T: ?Sized> Deref for SleepGuard<'_, T> {
    type Target = T;

    fn deref(&self) -> &T {
        // SAFETY: a live guard is proof the lock is held, so this is the only
        // reference to `data`; no other guard can exist concurrently.
        unsafe { &*self.lock.data.get() }
    }
}

impl<T: ?Sized> DerefMut for SleepGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        // SAFETY: a live guard is proof of *exclusive* ownership of the lock,
        // so this `&mut` is unique — no other guard or reference exists.
        unsafe { &mut *self.lock.data.get() }
    }
}

impl<T: ?Sized> Drop for SleepGuard<'_, T> {
    fn drop(&mut self) {
        self.lock.release();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use alloc::vec::Vec;
    use tairix_kernel_sched_api::TaskId;
    use tairix_sync::SpinLock;

    struct RecordingWake {
        tasks: SpinLock<Vec<TaskId>>,
    }

    impl RecordingWake {
        const fn new() -> Self {
            Self {
                tasks: SpinLock::new(Vec::new()),
            }
        }
    }

    impl crate::waitq::WaitQueueArch for RecordingWake {
        fn unpark(&self, id: TaskId) {
            self.tasks.lock().push(id);
        }

        fn now_ns(&self) -> u64 {
            0
        }

        fn set_wakeup(&self, _deadline_ns: Option<u64>) {}
    }

    #[test]
    fn an_uncontended_lock_grants_and_releases() {
        let lock = SleepLock::new(0u32);
        {
            let mut guard = lock.lock();
            *guard = 7;
        }
        // The release made the value visible and the lock re-acquirable.
        assert_eq!(*lock.lock(), 7);
    }

    #[test]
    fn try_lock_succeeds_when_free_and_fails_while_held() {
        let lock = SleepLock::new(());
        let held = lock.try_lock().expect("free lock is acquirable");
        // A second attempt fails closed while the first guard is alive.
        assert!(lock.try_lock().is_none(), "a held lock refuses try_lock");
        drop(held);
        // Once released it is acquirable again.
        assert!(lock.try_lock().is_some(), "a released lock is acquirable");
    }

    #[test]
    fn the_guard_mutates_the_protected_value_in_place() {
        let lock = SleepLock::new(Vec::<u8>::new());
        lock.lock().push(1);
        lock.lock().push(2);
        let guard = lock.lock();
        assert_eq!(&*guard, &[1, 2]);
    }

    #[test]
    fn into_inner_returns_the_value() {
        let lock = SleepLock::new(99u64);
        assert_eq!(lock.into_inner(), 99);
    }

    #[test]
    fn a_released_lock_leaves_no_parked_waiter() {
        // With no scheduler hook installed nothing ever parks, so the
        // wait-queue stays empty across uncontended acquire/release — the
        // release path's wake is a safe no-op.
        let lock = SleepLock::new(0u8);
        {
            let _guard = lock.lock();
        }
        assert!(lock.waiters.is_empty(), "no contender was ever registered");
        assert_eq!(lock.state.load(Ordering::Acquire), 0);
    }

    #[test]
    fn an_uncontended_release_never_consults_the_wait_queue() {
        // The regression: this lock serialises every block-device operation
        // on a shared disk, and release took the wait-queue spin lock (and a
        // `BTreeMap` lookup) on *every* one just to learn that nobody was
        // waiting. The fast path is selected by the lock word alone, so a
        // registration nothing published `CONTENDED` for is not looked at —
        // which is sound because a real contender always sets that flag
        // before it parks (`park_until_released`).
        let lock = SleepLock::new(());
        let wake = RecordingWake::new();
        lock.waiters.register(44, NO_DEADLINE);
        {
            let _guard = lock.try_lock().expect("free lock is acquirable");
            assert_eq!(lock.state.load(Ordering::Acquire), LOCKED);
        }
        assert_eq!(
            lock.state.load(Ordering::Acquire),
            0,
            "the fast path unlocked"
        );
        assert_eq!(
            lock.handoff.load(Ordering::Acquire),
            0,
            "nothing handed off"
        );
        assert!(
            wake.tasks.lock().is_empty(),
            "an unpublished registration is never woken"
        );

        // Publishing the flag is what selects the wake path, and the same
        // registration is then handed the lock.
        let _guard = lock.try_lock().expect("free lock is acquirable");
        assert_eq!(lock.state.fetch_or(CONTENDED, Ordering::AcqRel), LOCKED);
        lock.release_contended(Some(&wake));
        assert_eq!(wake.tasks.lock().as_slice(), &[44]);
        assert_eq!(lock.handoff.load(Ordering::Acquire), 44);
    }

    #[test]
    fn a_stale_contention_flag_clears_itself_on_the_next_release() {
        // A contender that published the flag and then found the lock free
        // leaves it set with an empty queue. That costs one release which
        // consults the queue, finds nothing, and clears the word — never a
        // permanently slow lock and never a lost wake.
        let lock = SleepLock::new(());
        let wake = RecordingWake::new();
        let guard = lock.try_lock().expect("free lock is acquirable");
        lock.state.fetch_or(CONTENDED, Ordering::AcqRel);
        drop(guard);
        assert_eq!(lock.state.load(Ordering::Acquire), 0, "the flag is cleared");
        assert!(wake.tasks.lock().is_empty());
        // And the lock is acquirable again through the plain fast path.
        assert!(lock.try_lock().is_some());
    }

    #[test]
    fn a_vanished_waiter_is_passed_over_rather_than_stranding_the_rest() {
        // A wake that finds its target already gone must not unlock with the
        // queue still occupied: the word is cleared with it, so no later
        // release would owe the remaining contenders a wake and they would
        // park for good.
        let lock = SleepLock::new(());
        let wake = RecordingWake::new();
        lock.state.store(LOCKED | CONTENDED, Ordering::Relaxed);
        lock.waiters.register(51, NO_DEADLINE);
        lock.waiters.register(52, NO_DEADLINE);
        // 51 leaves the queue after registering, exactly the window the
        // pass-over exists for.
        lock.waiters.deregister(51);

        lock.release_contended(Some(&wake));

        assert_eq!(wake.tasks.lock().as_slice(), &[52], "the next-oldest woke");
        assert_eq!(lock.handoff.load(Ordering::Acquire), 52);
        assert_ne!(
            lock.state.load(Ordering::Acquire) & LOCKED,
            0,
            "ownership is in flight, so the lock stays closed"
        );
    }

    #[test]
    fn release_hands_ownership_to_the_oldest_waiter_without_barging() {
        let lock = SleepLock::new(());
        let wake = RecordingWake::new();
        lock.state.store(LOCKED | CONTENDED, Ordering::Relaxed);
        lock.waiters.register(11, NO_DEADLINE);
        lock.waiters.register(22, NO_DEADLINE);

        lock.release_contended(Some(&wake));

        assert_ne!(lock.state.load(Ordering::Acquire) & LOCKED, 0);
        assert_eq!(lock.handoff.load(Ordering::Acquire), 11);
        assert_eq!(wake.tasks.lock().as_slice(), &[11]);
        assert!(
            lock.try_lock().is_none(),
            "a fresh contender cannot barge ahead of the FIFO handoff"
        );
        assert!(!lock.claim_handoff(22));
        assert!(lock.claim_handoff(11));
    }

    #[test]
    fn repeated_release_handoffs_follow_fifo_order() {
        let lock = SleepLock::new(());
        let wake = RecordingWake::new();
        lock.state.store(LOCKED | CONTENDED, Ordering::Relaxed);
        for task in [31, 32, 33] {
            lock.waiters.register(task, NO_DEADLINE);
        }

        for task in [31, 32, 33] {
            lock.release_contended(Some(&wake));
            assert_eq!(lock.handoff.load(Ordering::Acquire), task);
            lock.waiters.deregister(task);
            assert!(lock.claim_handoff(task));
        }
        assert_eq!(wake.tasks.lock().as_slice(), &[31, 32, 33]);

        lock.release_contended(Some(&wake));
        assert_eq!(lock.state.load(Ordering::Acquire), 0);
        assert!(lock.try_lock().is_some());
    }
}
