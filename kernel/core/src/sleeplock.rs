//! A scheduler-blocking mutual-exclusion lock (a *sleeping* mutex).
//!
//! Every lock in `lib/sync` ([`SpinLock`](rustos_sync::SpinLock),
//! [`RwLock`](rustos_sync::RwLock), [`McsLock`](rustos_sync::McsLock), …)
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
//! Release wakes *every* parked contender; they re-contend and exactly one
//! wins while the rest re-park. For the low-contention per-mount use this is
//! simplest and strands no waiter. A future fair hand-off can wake one
//! waiter without changing the public contract.

use core::cell::UnsafeCell;
use core::ops::{Deref, DerefMut};
use core::sync::atomic::{AtomicBool, Ordering};

use crate::dispatch_slot::RescheduleAction;
use crate::kthread::reschedule_current;
use crate::waitq::{wait_arch, WaitQueue, NO_DEADLINE};

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
    /// `true` while the lock is held. The single point of mutual exclusion;
    /// every acquire is a `compare_exchange` against it.
    locked: AtomicBool,
    /// Contenders parked waiting for the holder to release. Reuses the one
    /// kernel wait-queue definition (its register/wake/unpark bookkeeping is
    /// tested in `crate::waitq`); this lock adds only the acquire/release
    /// policy on top.
    waiters: WaitQueue,
    /// The protected value. Access is guarded by `locked`: a live
    /// [`SleepGuard`] is proof of exclusive ownership.
    data: UnsafeCell<T>,
}

// SAFETY: `SleepLock` is a mutual-exclusion boundary: `lock`/`try_lock` hand
// out a `SleepGuard` only after a successful `compare_exchange` on `locked`,
// so at most one thread ever holds `&mut`-equivalent access to `data` at a
// time, and ownership is transferred (not shared) on release. It is therefore
// safe to send the lock (and the value it guards) between threads when `T:
// Send`, and to share `&SleepLock` across threads when `T: Send` (sharing the
// reference only ever yields serialised, exclusive access to `T`). `T` need
// not be `Sync` because the guard never hands out concurrent `&T`.
unsafe impl<T: ?Sized + Send> Send for SleepLock<T> {}
// SAFETY: as for `Send` above — `&SleepLock` only ever yields exclusive
// access to `T` through the `locked` gate, never concurrent shared access.
unsafe impl<T: ?Sized + Send> Sync for SleepLock<T> {}

impl<T> SleepLock<T> {
    /// A new unlocked `SleepLock` guarding `value`.
    ///
    /// `const` so a lock may be placed in a `static` or built in a `const`
    /// context, like the spinning primitives.
    #[must_use]
    pub const fn new(value: T) -> Self {
        Self {
            locked: AtomicBool::new(false),
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
    #[must_use]
    pub fn try_lock(&self) -> Option<SleepGuard<'_, T>> {
        if self
            .locked
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_ok()
        {
            Some(SleepGuard { lock: self })
        } else {
            None
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
            self.park_until_released();
        }
    }

    /// One park attempt on a contended acquire: register the current caller,
    /// re-test, and park off the run queue until the holder releases.
    ///
    /// Returns after a wake (or without parking, if the lock freed in the
    /// register window); the [`lock`](Self::lock) loop then re-attempts the
    /// fast-path acquire. Registering **before** the re-test is what makes
    /// the release/park race lossless (see the module docs).
    fn park_until_released(&self) {
        // The scheduler hook supplies the current CPU, the current task to
        // register, and the `unpark` the releaser uses. Without it (before
        // the boot path installs the hook, or in a host test of an unrelated
        // path) no task can be parked — and no genuine contention can exist
        // either, since parking needs a live scheduler — so retry. This is
        // not a steady-state busy-poll: it is reachable only when there is no
        // scheduler to contend on.
        let Some(hook) = wait_arch() else {
            core::hint::spin_loop();
            return;
        };
        let Some(cpu) = hook.current_cpu() else {
            core::hint::spin_loop();
            return;
        };
        let Some(task) = hook.current_task(cpu) else {
            core::hint::spin_loop();
            return;
        };
        // Register before the re-test so a release between the failed
        // fast-path attempt and the park is never missed: the releaser's
        // `wake_all` finds this task, and the scheduler's wake-pending token
        // converts an `unpark` racing a not-yet-committed park into a
        // re-ready.
        self.waiters.register(task, NO_DEADLINE);
        // Re-test under the registration: if the holder released in the
        // window, do not park — drop the registration and let the caller
        // re-attempt the fast path.
        if !self.locked.load(Ordering::Acquire) {
            self.waiters.deregister(task);
            return;
        }
        // Park off the run queue. A `false` return means the caller is not a
        // resumable user kthread (a host test, or a non-task context): there
        // is then no scheduler to park on and no real contention, so drop the
        // registration and retry rather than park into the void.
        if !reschedule_current(cpu, RescheduleAction::Park) {
            self.waiters.deregister(task);
            core::hint::spin_loop();
            return;
        }
        // Woken: stop waiting and re-attempt the acquire.
        self.waiters.deregister(task);
    }

    /// Release the lock and wake every parked contender.
    ///
    /// Called only by [`SleepGuard`]'s `Drop`. The `Release` store publishes
    /// the critical section's writes to the next holder's `Acquire`; the wake
    /// then re-readies the parked contenders, each of which re-tests and
    /// either acquires or re-parks. A fail-safe no-op wake before the arch
    /// hook is installed (no task can be parked then).
    fn release(&self) {
        self.locked.store(false, Ordering::Release);
        if let Some(hook) = wait_arch() {
            self.waiters.wake_all(hook);
        }
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
    }
}
