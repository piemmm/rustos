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
//! # The payload is stored as atomics, not as a `T`
//!
//! A reader deliberately copies while a writer may be mid-update, so the two
//! accesses genuinely race — that is the whole design. C gets away with it by
//! convention; in Rust a racing non-atomic access is undefined behaviour
//! outright, and a `T` materialised from half-written bytes can be an invalid
//! value (an out-of-range enum, a zero in a niche) before any check can
//! discard it. So the payload lives in a fixed array of [`AtomicUsize`], is
//! copied word-at-a-time with `Relaxed` accesses, and only becomes a `T` once
//! the sequence has certified the snapshot. A relaxed race is defined: it
//! yields *some* value, and the sequence check is what rejects the mixed ones.
//!
//! The array is a fixed [`MAX_PAYLOAD_WORDS`] words, checked against `T` at
//! compile time. That is a containment bound rather than a capacity: this
//! primitive exists for a small value copied whole on every read, and a
//! payload that outgrows the bound wants a lock, not a retry loop.
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

use core::marker::PhantomData;
use core::mem::{size_of, transmute_copy, MaybeUninit};

use crate::loom_compat::{fence, AtomicUsize, Ordering};
use crate::spinwait::spin_wait;

/// Payload words a [`SeqLock`] reserves. Sized for the small, read-mostly
/// values the primitive is for — a time pair, a counter set, a geometry —
/// and checked against `T` when the lock is constructed.
pub const MAX_PAYLOAD_WORDS: usize = 8;

/// A sequence lock protecting a `T: Copy` payload.
pub struct SeqLock<T: Copy> {
    seq: AtomicUsize,
    words: [AtomicUsize; MAX_PAYLOAD_WORDS],
    _payload: PhantomData<T>,
}

// SAFETY: a reader hands back its own copy of `T` and no reference into the
// lock ever escapes, so sharing one across threads moves `T` values between
// them and needs exactly `T: Send`. The payload itself is only ever touched
// through the atomics above.
unsafe impl<T: Copy + Send> Send for SeqLock<T> {}
// SAFETY: as above.
unsafe impl<T: Copy + Send> Sync for SeqLock<T> {}

impl<T: Copy> SeqLock<T> {
    /// Compile-time proof that `T` fits the reserved words. Evaluated by
    /// every constructor, so an oversized payload is a build failure at the
    /// point of use rather than a truncation at runtime.
    const PAYLOAD_FITS: () = assert!(
        size_of::<T>() <= MAX_PAYLOAD_WORDS * size_of::<usize>(),
        "SeqLock payload exceeds MAX_PAYLOAD_WORDS; use a lock rather than a retry loop"
    );

    /// Words actually spanned by `T`, so a small payload does not pay for the
    /// whole reservation on every read.
    const WORDS: usize = size_of::<T>().div_ceil(size_of::<usize>());

    /// Construct a new sequence lock initialised to `value`.
    #[cfg(not(loom))]
    #[must_use]
    pub const fn new(value: T) -> Self {
        let () = Self::PAYLOAD_FITS;
        let raw = Self::encode(&value);
        let mut words = [const { AtomicUsize::new(0) }; MAX_PAYLOAD_WORDS];
        let mut i = 0;
        while i < MAX_PAYLOAD_WORDS {
            words[i] = AtomicUsize::new(raw[i]);
            i += 1;
        }
        Self {
            seq: AtomicUsize::new(0),
            words,
            _payload: PhantomData,
        }
    }

    /// Construct a new sequence lock initialised to `value` (non-`const`
    /// under `loom`, whose atomics have no `const` constructor).
    #[cfg(loom)]
    #[must_use]
    pub fn new(value: T) -> Self {
        let () = Self::PAYLOAD_FITS;
        let this = Self {
            seq: AtomicUsize::new(0),
            words: core::array::from_fn(|_| AtomicUsize::new(0)),
            _payload: PhantomData,
        };
        this.store(&value);
        this
    }

    /// `value`'s bytes as whole words, zero-padded.
    ///
    /// Zeroed rather than left uninitialised because `T` need not be a whole
    /// number of words: the tail of the last word would otherwise be
    /// undefined bytes on their way into an atomic store, and would publish
    /// whatever the previous payload left there.
    const fn encode(value: &T) -> [usize; MAX_PAYLOAD_WORDS] {
        let mut raw = [0usize; MAX_PAYLOAD_WORDS];
        // SAFETY: `T: Copy` so the source needs no ownership transfer, the
        // destination is at least `size_of::<T>()` bytes by `PAYLOAD_FITS`,
        // the two cannot overlap (`raw` is a fresh local), and the byte-wise
        // copy imposes no alignment requirement on either side.
        unsafe {
            core::ptr::copy_nonoverlapping(
                core::ptr::from_ref(value).cast::<u8>(),
                raw.as_mut_ptr().cast::<u8>(),
                size_of::<T>(),
            );
        }
        raw
    }

    /// Publish `value`'s words. Relaxed: the sequence counter carries the
    /// ordering, and a reader that sees a half-published payload is rejected
    /// by the sequence check rather than by the payload's own ordering.
    fn store(&self, value: &T) {
        let raw = Self::encode(value);
        for (slot, word) in self.words.iter().zip(raw.iter()).take(Self::WORDS) {
            slot.store(*word, Ordering::Relaxed);
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
            let mut raw = [MaybeUninit::<usize>::uninit(); MAX_PAYLOAD_WORDS];
            for (slot, word) in raw.iter_mut().zip(self.words.iter()).take(Self::WORDS) {
                slot.write(word.load(Ordering::Relaxed));
            }
            // Re-sample the sequence; an acquire fence forces the loads
            // above to be ordered before this read.
            fence(Ordering::Acquire);
            let s2 = self.seq.load(Ordering::Relaxed);
            if s1 == s2 {
                // SAFETY: the sequence was even before the copy and unchanged
                // after it, so no writer overlapped and these are exactly the
                // bytes some writer committed — a valid `T`. `transmute_copy`
                // reads `size_of::<T>()` bytes, which `PAYLOAD_FITS` bounds
                // inside the words just initialised, and drops to an unaligned
                // read where `T` is more strictly aligned than a word.
                return unsafe { transmute_copy::<_, T>(&raw) };
            }
            // Raced; retry.
            spin_wait();
        }
    }

    /// Returns the current sequence value (informational only).
    pub fn sequence(&self) -> usize {
        self.seq.load(Ordering::Relaxed)
    }

    /// Replace the payload.
    ///
    /// Whole-value rather than a `&mut T` mutator: the payload is a set of
    /// atomic words, so there is no `T` in the lock to lend out, and a
    /// read-modify-write that spanned the odd-sequence window would be a
    /// second writer's worth of exposure for no gain. Read, adjust, write.
    ///
    /// **At most one writer at a time** may invoke `write`. The caller
    /// is responsible for serialising writers, e.g. with an
    /// [`IrqSafeSpinLock`](crate::IrqSafeSpinLock).
    ///
    /// # Safety
    ///
    /// Caller must guarantee writer-uniqueness as described above. Two
    /// concurrent writers are not undefined — every access is atomic — but
    /// they interleave their words and commit a value neither wrote.
    pub unsafe fn write(&self, value: T) {
        // Mark "writer in progress" by setting LSB.
        let prev = self.seq.fetch_add(1, Ordering::Release);
        debug_assert!(prev & 1 == 0, "SeqLock concurrent writers detected");
        self.store(&value);
        // Mark "writer done"; release publishes the payload writes.
        self.seq.fetch_add(1, Ordering::Release);
    }
}
