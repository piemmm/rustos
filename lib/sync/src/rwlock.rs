//! Writer-preference reader/writer lock.
//!
//! [`RwLock<T>`] allows many concurrent readers or a single writer. The
//! lock is **writer-preference**: once a writer registers its intent, no
//! new readers may acquire the lock until the writer has been served.
//! This avoids the classic writer-starvation problem at the cost of
//! slightly higher reader latency under heavy write contention.
//!
//! # When to use
//!
//! - Data structure is read frequently and written occasionally.
//! - Critical sections are short and never block.
//!
//! # When *not* to use
//!
//! - Inside an interrupt handler — there is no `IrqSafe` variant; if an
//!   interrupt may touch the data, wrap an
//!   [`IrqSafeSpinLock`](crate::spinlock::IrqSafeSpinLock) around it
//!   or use a [`SeqLock`](crate::seqlock::SeqLock) for read-mostly data.
//! - For read-mostly data where readers must never block: use
//!   [`SeqLock`](crate::seqlock::SeqLock) instead.
//!
//! # Recursive acquisition is forbidden
//!
//! Because the lock is writer-preference, acquiring a second
//! [`read`](RwLock::read) on a thread that already holds a
//! [`RwLockReadGuard`] for the *same* lock is a self-deadlock as soon as
//! any other CPU registers a pending writer: the writer's intent blocks
//! every new reader (including the recursive one), and the writer itself
//! then waits forever for the first, still-held read guard to drop. There
//! is no reentrant variant. Callers must never call `read`/`write` again
//! on a lock they are already holding a guard for; restructure the code
//! to take the guard once and pass the reference down instead.
//!
//! # Ordering guarantees
//!
//! - A successful [`read`](RwLock::read) performs an [`Acquire`] read on
//!   the state word; the matching writer release [`Release`]s.
//! - A successful [`write`](RwLock::write) performs an [`Acquire`] CAS
//!   and the guard's `Drop` performs a [`Release`].
//!
//! # Fairness invariant
//!
//! Once `pending_writers > 0`, no reader observes a successful
//! [`try_read`](RwLock::try_read) until the next writer has completed.
//! This is what makes the lock writer-preference and is exercised by the
//! property test in `tests/rwlock_fairness.rs`.
//!
//! # IRQ level
//!
//! Process / kernel-thread context only. Never from an interrupt handler.
//!
//! # Lock diagnostics
//!
//! With the `lock-diagnostics` feature, `read`/`write` (and their
//! non-spinning `try_*` counterparts) report their acquire/hold/release
//! lifecycle to the `lockwatch` seam that same feature compiles in,
//! exactly like [`SpinLock`](crate::spinlock::SpinLock). A reader or
//! writer spinning here is otherwise invisible to a lockup watchdog that
//! only samples IRQ-masking spinlocks, so a CPU wedged in `read`/`write`
//! needs to be nameable too. With the feature off this instrumentation,
//! and the `#[track_caller]` shim it needs, compile away entirely and a
//! production lock is the bare atomics below.
//!
//! [`Acquire`]: core::sync::atomic::Ordering::Acquire
//! [`Release`]: core::sync::atomic::Ordering::Release
//! [`IrqSafeSpinLock`]: crate::IrqSafeSpinLock

use core::fmt;
use core::marker::PhantomData;
use core::ops::{Deref, DerefMut};

use crate::loom_compat::{AtomicUsize, Ordering, SyncUnsafeCell};
use crate::spinwait::{spin_until, spin_wait};

// State word layout (in a single `AtomicUsize`):
//   bit 0           : WRITER_BIT     — set while a writer holds the lock
//   bits 1..HALF    : reader count
//   bits HALF..LAST : pending-writer count
// Every bit is spoken for, so no field may be incremented without first
// checking it is not already at its maximum: a wrap would carry into the
// neighbouring field (or off the top) and hand out a state that lies about
// who holds the lock.
const WRITER_BIT: usize = 1;
const READER_SHIFT: u32 = 1;
const PENDING_SHIFT: u32 = usize::BITS / 2;
const READER_ONE: usize = 1 << READER_SHIFT;
const PENDING_ONE: usize = 1 << PENDING_SHIFT;
const READER_MASK: usize = ((1usize << (PENDING_SHIFT - READER_SHIFT)) - 1) << READER_SHIFT;
const PENDING_MASK: usize = !((1usize << PENDING_SHIFT) - 1);

/// Most concurrent readers the count field can hold.
const MAX_READERS: usize = reader_count(READER_MASK);
/// Most simultaneously registered writer intents the count field can hold.
const MAX_PENDING: usize = pending_writers(PENDING_MASK);

#[inline]
const fn reader_count(state: usize) -> usize {
    (state & READER_MASK) >> READER_SHIFT
}

#[inline]
const fn pending_writers(state: usize) -> usize {
    (state & PENDING_MASK) >> PENDING_SHIFT
}

#[inline]
const fn writer_held(state: usize) -> bool {
    (state & WRITER_BIT) != 0
}

// The three state transitions, as total functions of the state word. Keeping
// them pure is what lets the saturation boundaries — otherwise reachable only
// with billions of live guards — be exercised directly, and it puts the
// "check before you increment" ordering in one place instead of once per
// caller.

/// The state a reader acquisition moves to, or [`None`] if it must wait: a
/// writer holds or is pending (writer preference), or the reader field is
/// full.
#[inline]
const fn reader_acquired(state: usize) -> Option<usize> {
    if writer_held(state) || pending_writers(state) > 0 || reader_count(state) == MAX_READERS {
        return None;
    }
    Some(state + READER_ONE)
}

/// The state a writer's intent registration moves to, or [`None`] if the
/// pending field is full.
#[inline]
const fn pending_registered(state: usize) -> Option<usize> {
    if pending_writers(state) == MAX_PENDING {
        return None;
    }
    Some(state + PENDING_ONE)
}

/// The state a *registered* writer moves to when it takes the lock: it
/// withdraws its own intent and sets the writer bit in one transition, so no
/// window shows the lock both held and still awaited by its holder.
///
/// [`None`] while a reader or another writer holds it, and — fail closed —
/// for an unregistered caller: withdrawing an intent never lodged wraps the
/// pending count to its maximum, and since readers defer to a pending writer
/// the lock would then refuse every reader for ever.
#[inline]
const fn writer_acquired(state: usize) -> Option<usize> {
    if reader_count(state) != 0 || writer_held(state) || pending_writers(state) == 0 {
        return None;
    }
    Some((state - PENDING_ONE) | WRITER_BIT)
}

/// Writer-preference reader/writer lock.
pub struct RwLock<T: ?Sized> {
    state: AtomicUsize,
    data: SyncUnsafeCell<T>,
}

// SAFETY: Mutual exclusion is enforced by `state`; the only paths that
// expose `&T`/`&mut T` are guards that hold the appropriate count.
unsafe impl<T: ?Sized + Send> Send for RwLock<T> {}
// SAFETY: Readers see `&T` so `T: Sync` is required; the writer sees
// `&mut T` so `T: Send` is required.
unsafe impl<T: ?Sized + Send + Sync> Sync for RwLock<T> {}

impl<T> RwLock<T> {
    /// Create a new reader/writer lock wrapping `value`.
    #[cfg(not(loom))]
    #[must_use]
    pub const fn new(value: T) -> Self {
        Self {
            state: AtomicUsize::new(0),
            data: SyncUnsafeCell::new(value),
        }
    }

    /// Create a new reader/writer lock wrapping `value` (non-`const` under `loom`).
    #[cfg(loom)]
    #[must_use]
    pub fn new(value: T) -> Self {
        Self {
            state: AtomicUsize::new(0),
            data: SyncUnsafeCell::new(value),
        }
    }

    /// Consume the lock and return the protected value.
    pub fn into_inner(self) -> T {
        let this = core::mem::ManuallyDrop::new(self);
        // SAFETY: `self` is consumed and held in `ManuallyDrop`, so the
        // inner cell is not dropped and we may move the value out.
        this.data.with(|p| unsafe { core::ptr::read(p) })
    }
}

impl<T: ?Sized> RwLock<T> {
    /// The uninstrumented reader-acquire attempt, shared by the public
    /// [`Self::try_read`] and [`Self::read`] so the lock-diagnostics site
    /// note is emitted exactly once per acquisition (never doubled by
    /// `read` delegating to `try_read`).
    #[inline]
    fn raw_try_read(&self) -> Option<RwLockReadGuard<'_, T>> {
        let mut cur = self.state.load(Ordering::Relaxed);
        loop {
            let next = reader_acquired(cur)?;
            match self
                .state
                .compare_exchange_weak(cur, next, Ordering::Acquire, Ordering::Relaxed)
            {
                Ok(_) => {
                    return Some(RwLockReadGuard {
                        lock: self,
                        _cpu_bound: PhantomData,
                    })
                }
                Err(actual) => cur = actual,
            }
        }
    }

    /// Register a writer's intent, which is what makes new readers back off.
    /// `false` when the pending field is full, so the caller waits or refuses
    /// rather than wrapping the count.
    ///
    /// A `fetch_add` cannot express this: it would publish the wrapped state
    /// before the caller could inspect it, and for the window until the
    /// caller undid it every reader would see a queue of writers as empty.
    #[inline]
    fn register_pending(&self) -> bool {
        let mut cur = self.state.load(Ordering::Relaxed);
        loop {
            let Some(next) = pending_registered(cur) else {
                return false;
            };
            match self
                .state
                .compare_exchange_weak(cur, next, Ordering::Relaxed, Ordering::Relaxed)
            {
                Ok(_) => return true,
                Err(actual) => cur = actual,
            }
        }
    }

    /// Try to acquire a shared (reader) lock without spinning.
    ///
    /// Fails (`None`) if a writer holds the lock *or* one is pending.
    #[cfg_attr(feature = "lock-diagnostics", track_caller)]
    pub fn try_read(&self) -> Option<RwLockReadGuard<'_, T>> {
        let guard = self.raw_try_read()?;
        // Record the successful non-spinning acquire against the caller's
        // source site so a wedge while holding this guard names it.
        #[cfg(feature = "lock-diagnostics")]
        crate::lockwatch::note(
            crate::lockwatch::LockEvent::TryAcquired,
            core::panic::Location::caller(),
        );
        Some(guard)
    }

    /// Acquire a shared (reader) lock, spinning until it is granted.
    #[cfg_attr(feature = "lock-diagnostics", track_caller)]
    pub fn read(&self) -> RwLockReadGuard<'_, T> {
        // Publish the acquiring site *before* spinning, so a CPU that
        // wedges spinning for a lock a writer never releases has its
        // report name the contended lock (marked `acquiring`); the
        // successful-acquire note below then promotes it to `held`.
        #[cfg(feature = "lock-diagnostics")]
        let site = core::panic::Location::caller();
        #[cfg(feature = "lock-diagnostics")]
        crate::lockwatch::note(crate::lockwatch::LockEvent::Acquiring, site);
        loop {
            if let Some(g) = self.raw_try_read() {
                #[cfg(feature = "lock-diagnostics")]
                crate::lockwatch::note(crate::lockwatch::LockEvent::Acquired, site);
                return g;
            }
            // Spin until both writer and pending-writer flags clear. A
            // reader refused because the count is momentarily full has no
            // writer to watch for, which is why the shared loop serves its
            // round before consulting the condition at all.
            spin_until(|| {
                let s = self.state.load(Ordering::Relaxed);
                !writer_held(s) && pending_writers(s) == 0
            });
        }
    }

    /// Try to acquire the exclusive (writer) lock without spinning.
    #[cfg_attr(feature = "lock-diagnostics", track_caller)]
    pub fn try_write(&self) -> Option<RwLockWriteGuard<'_, T>> {
        // First register intent so concurrent readers back off.
        if !self.register_pending() {
            return None;
        }
        // Now attempt to take the lock, withdrawing that intent in the same
        // transition. One attempt only: `try_write` never spins.
        let cur = self.state.load(Ordering::Relaxed);
        let taken = writer_acquired(cur).is_some_and(|next| {
            self.state
                .compare_exchange(cur, next, Ordering::Acquire, Ordering::Relaxed)
                .is_ok()
        });
        if !taken {
            self.withdraw_pending();
            return None;
        }
        // Record the successful non-spinning acquire against the caller's
        // source site so a wedge while holding this guard names it. There was
        // no spin phase, so this is the only note this acquisition emits.
        #[cfg(feature = "lock-diagnostics")]
        crate::lockwatch::note(
            crate::lockwatch::LockEvent::TryAcquired,
            core::panic::Location::caller(),
        );
        Some(RwLockWriteGuard {
            lock: self,
            _cpu_bound: PhantomData,
        })
    }

    /// Withdraw an intent this caller registered and did not convert into the
    /// lock. Unconditional: the count cannot underflow because only a
    /// successful [`Self::register_pending`] reaches here.
    #[inline]
    fn withdraw_pending(&self) {
        self.state.fetch_sub(PENDING_ONE, Ordering::Relaxed);
    }

    /// Acquire the exclusive (writer) lock, spinning until granted.
    #[cfg_attr(feature = "lock-diagnostics", track_caller)]
    pub fn write(&self) -> RwLockWriteGuard<'_, T> {
        // Publish the acquiring site *before* spinning, so a CPU that
        // wedges spinning for a lock it can never take has its report
        // name the contended lock (marked `acquiring`); the
        // successful-acquire note below then promotes it to `held`.
        #[cfg(feature = "lock-diagnostics")]
        let site = core::panic::Location::caller();
        #[cfg(feature = "lock-diagnostics")]
        crate::lockwatch::note(crate::lockwatch::LockEvent::Acquiring, site);
        // Step 1: register pending-writer intent. This blocks new readers,
        // achieving writer preference. A full pending field is a wait, not a
        // refusal — `write` has no way to decline — so spin until a departing
        // writer frees a slot.
        while !self.register_pending() {
            spin_wait();
        }
        loop {
            let cur = self.state.load(Ordering::Relaxed);
            if let Some(next) = writer_acquired(cur) {
                if self
                    .state
                    .compare_exchange_weak(cur, next, Ordering::Acquire, Ordering::Relaxed)
                    .is_ok()
                {
                    #[cfg(feature = "lock-diagnostics")]
                    crate::lockwatch::note(crate::lockwatch::LockEvent::Acquired, site);
                    return RwLockWriteGuard {
                        lock: self,
                        _cpu_bound: PhantomData,
                    };
                }
            }
            // Reached on a lost CAS as well as on an unavailable lock. A weak
            // compare-exchange may fail spuriously on the LL/SC targets, so
            // omitting the round here would let a writer spin hot without
            // hinting the core or serving its peers.
            spin_wait();
        }
    }

    /// Returns `true` if any writer (held or pending) is registered.
    pub fn is_write_pending(&self) -> bool {
        let s = self.state.load(Ordering::Relaxed);
        writer_held(s) || pending_writers(s) > 0
    }

    /// Returns the current number of active readers (informational only).
    pub fn reader_count(&self) -> usize {
        reader_count(self.state.load(Ordering::Relaxed))
    }

    /// Get a mutable reference to the protected value.
    pub fn get_mut(&mut self) -> &mut T {
        // SAFETY: `&mut self` guarantees no concurrent access.
        self.data.with_mut(|p| unsafe { &mut *p })
    }
}

impl<T: Default> Default for RwLock<T> {
    fn default() -> Self {
        Self::new(T::default())
    }
}

impl<T: ?Sized + fmt::Debug> fmt::Debug for RwLock<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.try_read() {
            Some(g) => f.debug_struct("RwLock").field("data", &&*g).finish(),
            None => f
                .debug_struct("RwLock")
                .field("data", &format_args!("<locked>"))
                .finish(),
        }
    }
}

/// Shared-access RAII guard returned by [`RwLock::read`].
#[must_use = "if unused the read lock is immediately released"]
pub struct RwLockReadGuard<'a, T: ?Sized> {
    lock: &'a RwLock<T>,
    /// Pins the guard to the CPU that acquired it: releasing a reader slot
    /// from a core that never took one, and under lock diagnostics popping a
    /// per-CPU record it never pushed, is not something a caller should be
    /// able to spell. A raw pointer is how a type opts out of `Send`.
    _cpu_bound: PhantomData<*const ()>,
}

// SAFETY: a shared `&RwLockReadGuard` hands out `&T` through `Deref`, so
// `T: Sync` is the requirement; `Send` stays blocked by the marker above.
unsafe impl<T: ?Sized + Sync> Sync for RwLockReadGuard<'_, T> {}

impl<T: ?Sized> Deref for RwLockReadGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        // SAFETY: The guard holds a reader slot; no writer can be active.
        self.lock.data.with(|p| unsafe { &*p })
    }
}

impl<T: ?Sized> Drop for RwLockReadGuard<'_, T> {
    fn drop(&mut self) {
        // Release pairs with the next writer's Acquire CAS.
        self.lock.state.fetch_sub(READER_ONE, Ordering::Release);
        // Drop the lock-diagnostics record this guard's acquisition pushed.
        // Every `RwLockReadGuard` corresponds to exactly one acquire note
        // (`try_read`/`read`), so the release note balances it one-to-one.
        #[cfg(feature = "lock-diagnostics")]
        crate::lockwatch::note_release();
    }
}

/// Exclusive-access RAII guard returned by [`RwLock::write`].
#[must_use = "if unused the write lock is immediately released"]
pub struct RwLockWriteGuard<'a, T: ?Sized> {
    lock: &'a RwLock<T>,
    /// Pins the guard to the CPU that acquired it, for the same reason as
    /// [`RwLockReadGuard`]'s marker.
    _cpu_bound: PhantomData<*const ()>,
}

// SAFETY: a shared `&RwLockWriteGuard` reaches only `&T` (`DerefMut` needs
// `&mut`), so `T: Sync` is the requirement; `Send` stays blocked above.
unsafe impl<T: ?Sized + Sync> Sync for RwLockWriteGuard<'_, T> {}

impl<T: ?Sized> Deref for RwLockWriteGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        // SAFETY: The guard holds the unique writer slot.
        self.lock.data.with(|p| unsafe { &*p })
    }
}

impl<T: ?Sized> DerefMut for RwLockWriteGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        // SAFETY: The guard holds the unique writer slot.
        self.lock.data.with_mut(|p| unsafe { &mut *p })
    }
}

impl<T: ?Sized> Drop for RwLockWriteGuard<'_, T> {
    fn drop(&mut self) {
        // Clear WRITER_BIT with a Release store-equivalent RMW so readers
        // and the next writer observe our mutations.
        self.lock.state.fetch_and(!WRITER_BIT, Ordering::Release);
        // Drop the lock-diagnostics record this guard's acquisition pushed.
        // Every `RwLockWriteGuard` corresponds to exactly one acquire note
        // (`try_write`/`write`), so the release note balances it one-to-one.
        #[cfg(feature = "lock-diagnostics")]
        crate::lockwatch::note_release();
    }
}

// The state-word transitions are pure, so their saturation and underflow
// refusals are unit-tested directly rather than left to a boundary no live
// workload can reach.
#[cfg(all(test, not(loom)))]
#[path = "rwlock_tests.rs"]
mod tests;
