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
use core::ops::{Deref, DerefMut};

use crate::loom_compat::{spin_loop, AtomicUsize, Ordering, SyncUnsafeCell};

// State word layout (in a single `AtomicUsize`):
//   bit 0           : WRITER_BIT     — set while a writer holds the lock
//   bits 1..HALF    : reader count
//   bits HALF..LAST : pending-writer count
//   The top bit is unused so saturating arithmetic stays well-defined.
const WRITER_BIT: usize = 1;
const READER_SHIFT: u32 = 1;
const PENDING_SHIFT: u32 = usize::BITS / 2;
const READER_ONE: usize = 1 << READER_SHIFT;
const PENDING_ONE: usize = 1 << PENDING_SHIFT;
const READER_MASK: usize = ((1usize << (PENDING_SHIFT - READER_SHIFT)) - 1) << READER_SHIFT;
const PENDING_MASK: usize = !((1usize << PENDING_SHIFT) - 1);

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
            if writer_held(cur) || pending_writers(cur) > 0 {
                return None;
            }
            if reader_count(cur) == reader_count(READER_MASK) {
                // Reader count saturated — refuse rather than overflow.
                return None;
            }
            match self.state.compare_exchange_weak(
                cur,
                cur + READER_ONE,
                Ordering::Acquire,
                Ordering::Relaxed,
            ) {
                Ok(_) => return Some(RwLockReadGuard { lock: self }),
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
            // Spin until both writer and pending-writer flags clear.
            while {
                let s = self.state.load(Ordering::Relaxed);
                writer_held(s) || pending_writers(s) > 0
            } {
                spin_loop();
            }
        }
    }

    /// Try to acquire the exclusive (writer) lock without spinning.
    #[cfg_attr(feature = "lock-diagnostics", track_caller)]
    pub fn try_write(&self) -> Option<RwLockWriteGuard<'_, T>> {
        // First register intent so concurrent readers back off.
        let prev = self.state.fetch_add(PENDING_ONE, Ordering::Relaxed);
        if pending_writers(prev) == pending_writers(PENDING_MASK) {
            // Pending-writer count saturated; undo and refuse.
            self.state.fetch_sub(PENDING_ONE, Ordering::Relaxed);
            return None;
        }
        // Now attempt to flip WRITER_BIT, but only if there are no
        // readers and no other writer.
        let cur = self.state.load(Ordering::Relaxed);
        if reader_count(cur) == 0 && !writer_held(cur) {
            // PENDING was already incremented; flip WRITER_BIT and drop
            // our pending bump in one CAS.
            if self
                .state
                .compare_exchange(
                    cur,
                    (cur - PENDING_ONE) | WRITER_BIT,
                    Ordering::Acquire,
                    Ordering::Relaxed,
                )
                .is_ok()
            {
                // Record the successful non-spinning acquire against the
                // caller's source site so a wedge while holding this
                // guard names it. There was no spin phase, so this is the
                // only note this acquisition emits.
                #[cfg(feature = "lock-diagnostics")]
                crate::lockwatch::note(
                    crate::lockwatch::LockEvent::TryAcquired,
                    core::panic::Location::caller(),
                );
                return Some(RwLockWriteGuard { lock: self });
            }
            self.state.fetch_sub(PENDING_ONE, Ordering::Relaxed);
            return None;
        }
        self.state.fetch_sub(PENDING_ONE, Ordering::Relaxed);
        None
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
        // Step 1: register pending-writer intent. This blocks new
        // readers, achieving writer preference.
        self.state.fetch_add(PENDING_ONE, Ordering::Relaxed);
        loop {
            let cur = self.state.load(Ordering::Relaxed);
            if reader_count(cur) == 0 && !writer_held(cur) {
                if self
                    .state
                    .compare_exchange_weak(
                        cur,
                        (cur - PENDING_ONE) | WRITER_BIT,
                        Ordering::Acquire,
                        Ordering::Relaxed,
                    )
                    .is_ok()
                {
                    #[cfg(feature = "lock-diagnostics")]
                    crate::lockwatch::note(crate::lockwatch::LockEvent::Acquired, site);
                    return RwLockWriteGuard { lock: self };
                }
            } else {
                spin_loop();
            }
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
}

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
}

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
