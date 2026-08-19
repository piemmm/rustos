//! Futex-backed synchronisation for a multi-threaded program: [`Mutex`] and
//! [`Condvar`] (`plans/THREADS.md` decision 5).
//!
//! Both are built on the single generic blocking primitive the kernel offers —
//! `futex_wait` / `futex_wake` over a 32-bit word in the process's own memory —
//! so an **uncontended** lock is a pair of atomic operations and never enters
//! the kernel at all, while a thread that must genuinely wait gives the CPU up
//! and is woken by the release. Nothing here spins.
//!
//! # No poisoning
//!
//! A TAIRiX program has no unwinder: a panic writes its reason to `stderr` and
//! ends the *process* (`crate::entry!`'s handler), so a lock can never be left
//! held by a thread that unwound out of its critical section. There is
//! therefore no poison state and [`Mutex::lock`] is infallible — unlike the
//! hosted `std::sync::Mutex`, whose `Result` exists only to report a poisoned
//! lock.

use core::cell::UnsafeCell;
use core::marker::PhantomData;
use core::ops::{Deref, DerefMut};
use core::sync::atomic::{AtomicU32, Ordering};

use tairix_abi::{Duration64, Errno};

/// `futex_wait`'s "no timeout" spelling.
const NO_TIMEOUT: u64 = u64::MAX;

/// The longest finite timeout the futex ABI can express: one nanosecond short
/// of the "no timeout" sentinel, so a caller asking for an absurd span still
/// gets a *timeout* rather than an indefinite wait.
const MAX_TIMEOUT_NS: u64 = u64::MAX - 1;

/// [`Mutex::state`]: free.
const UNLOCKED: u32 = 0;
/// [`Mutex::state`]: held, with no thread known to be waiting.
const LOCKED: u32 = 1;
/// [`Mutex::state`]: held, with at least one thread waiting — so the release
/// owes a wake.
const CONTENDED: u32 = 2;

/// Relative nanoseconds a `Duration64` names, clamped so it can never be
/// mistaken for [`NO_TIMEOUT`]. A negative span is already elapsed and is
/// expressed as zero.
fn timeout_nanos(timeout: Duration64) -> u64 {
    timeout.saturating_total_nanos().min(MAX_TIMEOUT_NS)
}

/// The raw `-errno` a futex wait returned, or [`None`] when it completed.
fn futex_error(ret: i64) -> Option<Errno> {
    if ret >= 0 {
        return None;
    }
    // The kernel encodes a refusal as `-errno`; a discriminant this build does
    // not know reads as the generic refusal rather than as success.
    let code = i32::try_from(-ret).unwrap_or(i32::MAX);
    Errno::from_i32(code).or(Some(Errno::OutOfRange))
}

/// Park until the word at `addr` is woken, unless it no longer holds `expected`.
///
/// # Safety
///
/// `addr` must be the address of a live, naturally aligned [`AtomicU32`] this
/// process owns.
unsafe fn wait_on(addr: u64, expected: u32, timeout_ns: u64) -> Option<Errno> {
    // SAFETY: the caller guarantees `addr` names a live aligned word of this
    // process; the kernel validates the alignment and resolves the address
    // against this process's own space before reading it.
    futex_error(unsafe { crate::futex_wait(addr, expected, timeout_ns) })
}

/// Wake up to `count` threads parked on the word at `addr`.
///
/// # Safety
///
/// As [`wait_on`].
unsafe fn wake_on(addr: u64, count: u32) {
    // SAFETY: as `wait_on`. The kernel dereferences nothing: the wait key is
    // `(this process, addr)`, so a word with no waiters simply wakes no one.
    let _ = unsafe { crate::futex_wake(addr, count) };
}

/// The address of an atomic word, in the form the futex syscalls take.
fn word_addr(word: &AtomicU32) -> u64 {
    core::ptr::from_ref(word) as usize as u64
}

/// A mutual-exclusion lock guarding a `T`, whose uncontended acquire and
/// release are pure user-space atomics.
///
/// The lock word carries three states — free, held, and held-with-waiters — so
/// a release pays for a `futex_wake` syscall **only** when a thread is actually
/// parked on it. Contention parks rather than spins, so a long critical section
/// never pegs a core.
pub struct Mutex<T> {
    /// [`UNLOCKED`] / [`LOCKED`] / [`CONTENDED`]. This is the futex word, so it
    /// must stay a naturally aligned `u32`.
    state: AtomicU32,
    value: UnsafeCell<T>,
}

// SAFETY: the lock word serialises every access to `value`: a thread reaches
// the interior only while it holds the lock (through a `MutexGuard`, which
// releases it on drop), so at most one `&mut T` exists at a time. Sending the
// lock (and thus the `T`) to another thread requires `T: Send`, and sharing it
// requires nothing more, because the lock itself provides the exclusion.
unsafe impl<T: Send> Send for Mutex<T> {}
// SAFETY: as above.
unsafe impl<T: Send> Sync for Mutex<T> {}

impl<T> Mutex<T> {
    /// A new unlocked mutex holding `value`.
    #[must_use]
    pub const fn new(value: T) -> Self {
        Self {
            state: AtomicU32::new(UNLOCKED),
            value: UnsafeCell::new(value),
        }
    }

    /// Acquire the lock, blocking until it is free.
    ///
    /// The fast path is one compare-exchange. On contention the caller
    /// announces itself in the lock word and parks on it, so it consumes no CPU
    /// while it waits and the release wakes exactly one thread.
    pub fn lock(&self) -> MutexGuard<'_, T> {
        if self
            .state
            .compare_exchange(UNLOCKED, LOCKED, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            self.lock_contended();
        }
        MutexGuard {
            mutex: self,
            _not_send: PhantomData,
        }
    }

    /// Acquire the lock only if it is free right now.
    ///
    /// Returns [`None`] rather than blocking, so a caller with other work can
    /// do it instead of waiting.
    pub fn try_lock(&self) -> Option<MutexGuard<'_, T>> {
        self.state
            .compare_exchange(UNLOCKED, LOCKED, Ordering::Acquire, Ordering::Relaxed)
            .ok()
            .map(|_| MutexGuard {
                mutex: self,
                _not_send: PhantomData,
            })
    }

    /// Borrow the interior directly, given exclusive access to the lock itself.
    ///
    /// No locking happens: `&mut self` already proves no other reference exists.
    pub fn get_mut(&mut self) -> &mut T {
        self.value.get_mut()
    }

    /// Consume the lock and return the value it guarded.
    #[must_use]
    pub fn into_inner(self) -> T {
        self.value.into_inner()
    }

    /// The blocking half of [`Self::lock`], kept out of line so the
    /// uncontended path stays a single inlined compare-exchange.
    ///
    /// Marking the word [`CONTENDED`] *before* parking is what makes the wait
    /// race-free: the release reads the word, so a holder releasing between the
    /// mark and the park sees the mark and issues the wake, and the kernel's
    /// own compare then finds the word changed and returns without parking.
    fn lock_contended(&self) {
        // A holder with no recorded waiter must learn there is one now.
        let mut seen = self.state.swap(CONTENDED, Ordering::Acquire);
        while seen != UNLOCKED {
            // SAFETY: `state` is a live, naturally aligned `AtomicU32` field of
            // `self`, which outlives this call.
            let _ = unsafe { wait_on(word_addr(&self.state), CONTENDED, NO_TIMEOUT) };
            seen = self.state.swap(CONTENDED, Ordering::Acquire);
        }
    }

    /// Release the lock, waking one waiter if the word says any is parked.
    fn unlock(&self) {
        if self.state.swap(UNLOCKED, Ordering::Release) == CONTENDED {
            // Wake exactly one: a `wake_all` here would be a thundering herd
            // where all but one thread immediately re-park.
            // SAFETY: as `lock_contended`.
            unsafe { wake_on(word_addr(&self.state), 1) };
        }
    }
}

impl<T: Default> Default for Mutex<T> {
    fn default() -> Self {
        Self::new(T::default())
    }
}

/// Proof that the holder owns a [`Mutex`]'s interior, releasing it on drop.
///
/// Deliberately not [`Send`]: the exclusion a holder reasons about is scoped to
/// the thread that acquired the lock, so a guard handed to another thread would
/// let two threads believe they hold it.
pub struct MutexGuard<'a, T> {
    mutex: &'a Mutex<T>,
    _not_send: PhantomData<*const ()>,
}

impl<'a, T> MutexGuard<'a, T> {
    /// The lock this guard holds — what [`Condvar::wait`] re-acquires after its
    /// park.
    #[must_use]
    pub fn mutex(&self) -> &'a Mutex<T> {
        self.mutex
    }
}

impl<T> Deref for MutexGuard<'_, T> {
    type Target = T;

    fn deref(&self) -> &T {
        // SAFETY: holding this guard means holding the lock, so no other
        // reference to the interior exists.
        unsafe { &*self.mutex.value.get() }
    }
}

impl<T> DerefMut for MutexGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        // SAFETY: as `deref`; `&mut self` additionally proves this guard is not
        // aliased.
        unsafe { &mut *self.mutex.value.get() }
    }
}

impl<T> Drop for MutexGuard<'_, T> {
    fn drop(&mut self) {
        self.mutex.unlock();
    }
}

/// Whether a [`Condvar::wait_timeout`] returned because its deadline elapsed.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct WaitTimeoutResult(bool);

impl WaitTimeoutResult {
    /// Whether the wait ended on its timeout rather than on a notification.
    ///
    /// A `false` is **not** proof the predicate holds: a wait may return
    /// spuriously, so the caller re-tests its own condition either way.
    #[must_use]
    pub const fn timed_out(self) -> bool {
        self.0
    }
}

/// A condition variable: a rendezvous where threads wait for a predicate that
/// another thread makes true under the same [`Mutex`].
///
/// It holds a monotonic notification counter rather than a waiter list. Reading
/// the counter *before* releasing the mutex is what closes the lost-wake-up
/// race: a notification landing in the window between the release and the park
/// bumps the counter, and the kernel's own compare then declines to park.
///
/// A wait may return without a notification (the futex contract permits a
/// spurious wake), so every caller re-tests its predicate in a loop — the same
/// discipline every POSIX and Rust condition variable requires.
pub struct Condvar {
    /// Bumped by every notification. The futex word, so a naturally aligned
    /// `u32`; wrapping is harmless because waiters compare for *inequality*.
    notifications: AtomicU32,
}

impl Condvar {
    /// A new condition variable with no notifications yet.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            notifications: AtomicU32::new(0),
        }
    }

    /// Release `guard`, park until notified, then re-acquire and return it.
    pub fn wait<'a, T>(&self, guard: MutexGuard<'a, T>) -> MutexGuard<'a, T> {
        self.park(guard, NO_TIMEOUT).0
    }

    /// As [`Self::wait`], but give up after `timeout`.
    ///
    /// The returned [`WaitTimeoutResult`] says whether the deadline elapsed;
    /// the caller still re-tests its predicate, because a wait can also return
    /// spuriously.
    pub fn wait_timeout<'a, T>(
        &self,
        guard: MutexGuard<'a, T>,
        timeout: Duration64,
    ) -> (MutexGuard<'a, T>, WaitTimeoutResult) {
        self.park(guard, timeout_nanos(timeout))
    }

    /// Wake one waiting thread.
    ///
    /// Waiters are released oldest-first by the kernel, so repeated
    /// notification cannot leave an older waiter behind newer arrivals.
    pub fn notify_one(&self) {
        self.bump();
        // SAFETY: `notifications` is a live, naturally aligned `AtomicU32`
        // field of `self`, which outlives this call.
        unsafe { wake_on(word_addr(&self.notifications), 1) };
    }

    /// Wake every waiting thread.
    pub fn notify_all(&self) {
        self.bump();
        // SAFETY: as `notify_one`.
        unsafe { wake_on(word_addr(&self.notifications), u32::MAX) };
    }

    /// Publish a notification. `Release` pairs with the `Acquire` load a waiter
    /// performs before it releases the mutex, so the state the notifier wrote
    /// under the lock is visible to the woken waiter.
    fn bump(&self) {
        self.notifications.fetch_add(1, Ordering::Release);
    }

    /// The one wait implementation both public forms share.
    fn park<'a, T>(
        &self,
        guard: MutexGuard<'a, T>,
        timeout_ns: u64,
    ) -> (MutexGuard<'a, T>, WaitTimeoutResult) {
        // Sample the counter while the mutex is still held: any notification
        // from here on changes it, so the park below cannot miss one.
        let observed = self.notifications.load(Ordering::Acquire);
        let mutex = guard.mutex();
        drop(guard);
        // SAFETY: `notifications` is a live, naturally aligned `AtomicU32`
        // field of `self`, which outlives this call.
        let outcome = unsafe { wait_on(word_addr(&self.notifications), observed, timeout_ns) };
        // Only `TimedOut` is a genuine deadline expiry. `WouldBlock` means a
        // notification already landed, and any other refusal is reported to the
        // caller as a plain (spurious) wake — it re-tests its predicate anyway,
        // so claiming a timeout that did not happen would be the only way to
        // mislead it.
        let timed_out = outcome == Some(Errno::TimedOut);
        (mutex.lock(), WaitTimeoutResult(timed_out))
    }
}

impl Default for Condvar {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The lock-word protocol is what a reviewer must be able to check by
    /// inspection: the uncontended acquire/release pair never leaves the word
    /// marked contended, so it never pays for a wake syscall.
    #[test]
    fn an_uncontended_lock_cycle_leaves_the_word_free() {
        let mutex = Mutex::new(7u32);
        {
            let guard = mutex.lock();
            assert_eq!(*guard, 7);
            assert_eq!(mutex.state.load(Ordering::Relaxed), LOCKED);
        }
        assert_eq!(mutex.state.load(Ordering::Relaxed), UNLOCKED);
    }

    #[test]
    fn try_lock_takes_a_free_lock_and_declines_a_held_one() {
        let mutex = Mutex::new(0u8);
        let held = mutex.try_lock().expect("a fresh lock is free");
        assert!(
            mutex.try_lock().is_none(),
            "a held lock is declined, never waited on"
        );
        drop(held);
        assert!(mutex.try_lock().is_some());
    }

    #[test]
    fn a_guard_mutates_the_interior_in_place() {
        let mutex = Mutex::new(1u32);
        *mutex.lock() += 41;
        assert_eq!(mutex.into_inner(), 42);
    }

    #[test]
    fn get_mut_reaches_the_interior_without_locking() {
        let mut mutex = Mutex::new(5u32);
        *mutex.get_mut() = 6;
        assert_eq!(mutex.state.load(Ordering::Relaxed), UNLOCKED);
        assert_eq!(mutex.into_inner(), 6);
    }

    /// Every notification changes the word a waiter compares against, which is
    /// what makes a notification landing between the mutex release and the park
    /// impossible to lose.
    #[test]
    fn every_notification_changes_the_word_waiters_compare() {
        let cv = Condvar::new();
        let start = cv.notifications.load(Ordering::Relaxed);
        cv.bump();
        assert_ne!(cv.notifications.load(Ordering::Relaxed), start);
        let after_one = cv.notifications.load(Ordering::Relaxed);
        cv.bump();
        assert_ne!(cv.notifications.load(Ordering::Relaxed), after_one);
    }

    #[test]
    fn a_timeout_is_clamped_so_it_can_never_read_as_no_timeout() {
        assert_eq!(timeout_nanos(Duration64::from_nanos(0)), 0);
        assert_eq!(timeout_nanos(Duration64::from_nanos(1_500)), 1_500);
        // A span whose nanosecond count saturates must still name a *finite*
        // timeout, never the "no timeout" sentinel.
        let huge = timeout_nanos(Duration64::from_secs(i64::MAX));
        assert_eq!(huge, MAX_TIMEOUT_NS);
        assert_ne!(huge, NO_TIMEOUT);
        // A negative span is already elapsed.
        assert_eq!(timeout_nanos(Duration64::from_secs(-1)), 0);
    }

    #[test]
    fn a_futex_result_decodes_success_and_every_refusal() {
        assert_eq!(futex_error(0), None);
        assert_eq!(
            futex_error(-i64::from(Errno::TimedOut.as_i32())),
            Some(Errno::TimedOut)
        );
        assert_eq!(
            futex_error(-i64::from(Errno::WouldBlock.as_i32())),
            Some(Errno::WouldBlock)
        );
        // A discriminant this build does not know is still reported as a
        // refusal, never silently read as success.
        assert!(futex_error(-9_999).is_some());
    }

    #[test]
    fn only_a_real_deadline_expiry_reads_as_timed_out() {
        assert!(WaitTimeoutResult(true).timed_out());
        assert!(!WaitTimeoutResult(false).timed_out());
    }
}
