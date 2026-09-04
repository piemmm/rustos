//! Sequence lock (`SeqLock`).
//!
//! A [`SeqLock<T>`] is a lock-free reader, single-writer primitive used
//! for **read-mostly** data where readers must never block (interrupt
//! handlers, statistics counters, the time-of-day vDSO, …). Readers
//! validate their snapshot by sampling a monotonically-increasing
//! sequence counter on either side of the read; a writer increments the
//! counter to an odd value before mutating and back to even on commit,
//! so a reader can detect that it raced with a write and retry.
//!
//! # When to use
//!
//! - Data is read very frequently and written rarely.
//! - Readers must remain non-blocking and tolerant of retry.
//! - The payload is small enough to copy on every read
//!   (typically `T: Copy`).
//!
//! # When *not* to use
//!
//! - There are multiple concurrent writers — [`SeqLock`] permits exactly
//!   one. Wrap in another lock for the writer side, or use [`RwLock`].
//! - The payload is large; the reader copy is expensive.
//! - Readers cannot tolerate observing torn writes-in-progress (the
//!   retry loop hides them, but the *latency* spikes).
//!
//! # Ordering guarantees
//!
//! - A successful [`read`](SeqLock::read) sees only "committed" values:
//!   it samples the sequence with [`Acquire`] before *and* after copying
//!   the payload and discards the result if either sample is odd or the
//!   two disagree.
//! - A writer increments the sequence with [`Release`] semantics around
//!   the mutation, publishing every write to the payload.
//!
//! # IRQ level
//!
//! Readers are safe at any IRQ level. Writers must serialise themselves
//! (typically using an [`IrqSafeSpinLock`](crate::IrqSafeSpinLock) wrapping
//! a `SeqLock` write handle, or by being the only writer by construction).
//!
//! [`Acquire`]: core::sync::atomic::Ordering::Acquire
//! [`Release`]: core::sync::atomic::Ordering::Release
//! [`RwLock`]: crate::RwLock

use crate::loom_compat::{fence, AtomicUsize, Ordering, SyncUnsafeCell};
use crate::spinwait::spin_wait;

/// A sequence lock protecting a `T: Copy` payload.
pub struct SeqLock<T: Copy> {
    seq: AtomicUsize,
    data: SyncUnsafeCell<T>,
}

// SAFETY: Readers never form a reference into the cell — they copy via
// `ptr::read_volatile`. Writers serialise externally.
unsafe impl<T: Copy + Send> Send for SeqLock<T> {}
// SAFETY: As above.
unsafe impl<T: Copy + Send> Sync for SeqLock<T> {}

impl<T: Copy> SeqLock<T> {
    /// Construct a new sequence lock initialised to `value`.
    #[cfg(not(loom))]
    #[must_use]
    pub const fn new(value: T) -> Self {
        Self {
            seq: AtomicUsize::new(0),
            data: SyncUnsafeCell::new(value),
        }
    }

    /// Construct a new sequence lock initialised to `value` (non-`const` under `loom`).
    #[cfg(loom)]
    #[must_use]
    pub fn new(value: T) -> Self {
        Self {
            seq: AtomicUsize::new(0),
            data: SyncUnsafeCell::new(value),
        }
    }

    /// Read a consistent snapshot of the payload, retrying if a writer
    /// raced with us.
    pub fn read(&self) -> T {
        loop {
            let s1 = self.seq.load(Ordering::Acquire);
            if s1 & 1 != 0 {
                // A writer is mid-update; spin until they finish.
                spin_wait();
                continue;
            }
            // Read the payload.
            // SAFETY: `T: Copy` and we read via `ptr::read_volatile` so a
            // partially-written payload merely produces a value we then
            // discard.
            let value = self.data.with(|p| unsafe { core::ptr::read_volatile(p) });
            // Re-sample the sequence; an acquire fence forces the load
            // above to be ordered before this read.
            fence(Ordering::Acquire);
            let s2 = self.seq.load(Ordering::Relaxed);
            if s1 == s2 {
                return value;
            }
            // Raced; retry.
            spin_wait();
        }
    }

    /// Returns the current sequence value (informational only).
    pub fn sequence(&self) -> usize {
        self.seq.load(Ordering::Relaxed)
    }

    /// Mutate the payload.
    ///
    /// **At most one writer at a time** may invoke `write`. The caller
    /// is responsible for serialising writers, e.g. with an
    /// [`IrqSafeSpinLock`](crate::IrqSafeSpinLock).
    ///
    /// # Safety
    ///
    /// Caller must guarantee writer-uniqueness as described above.
    pub unsafe fn write<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&mut T) -> R,
    {
        // Mark "writer in progress" by setting LSB.
        let prev = self.seq.fetch_add(1, Ordering::Release);
        debug_assert!(prev & 1 == 0, "SeqLock concurrent writers detected");
        // SAFETY: Writer-uniqueness is a precondition of this method, so
        // no other thread (writer or reader) can be inside `data` as a
        // mutable reference. Readers only ever read the bytes via
        // `read_volatile`.
        let r = self.data.with_mut(|p| unsafe { f(&mut *p) });
        // Mark "writer done"; release publishes the payload writes.
        self.seq.fetch_add(1, Ordering::Release);
        r
    }
}
