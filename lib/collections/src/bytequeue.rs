//! A bounded byte FIFO over one contiguous arena.
//!
//! Bytes are appended at the tail and taken from the head, and what is queued
//! is always one contiguous run, so a holder can parse, decrypt, or write it
//! in place. That is what `alloc::VecDeque` cannot offer: its contents wrap,
//! and a record that straddles the wrap has to be rotated into one piece
//! before it can be read as a slice.
//!
//! The consumed prefix is reclaimed by compacting on a threshold, never by
//! shifting the whole queue on every take, so a relay that moves every byte of
//! a stream through the queue pays an amortised constant per byte.
//!
//! Storage the queue gives back is wiped first, on growth and on drop, so no
//! byte it ever held reaches the allocator. That is the one scrub a container
//! here performs: a byte stream is exactly what a credential or a session key
//! transits, and a reallocation would otherwise leave a copy in freed memory.
//! Consumed bytes inside live storage are not wiped as they go; they are
//! overwritten or wiped when the storage is released.

use alloc::vec::Vec;

use zeroize::Zeroize;

use crate::TryReserveError;

/// Why bytes could not be appended.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum QueueError {
    /// The bytes would take the queue past its bound.
    Full,
    /// The allocator could not supply storage below the bound.
    Alloc(TryReserveError),
}

/// A byte FIFO holding at most `bound` bytes in one contiguous arena.
///
/// The bound is the holder's containment policy, fixed at construction; the
/// queue never holds more whatever it is offered. Storage behind the bound is
/// committed either up front ([`Self::committed`], so no later operation
/// allocates and the cost is known at admission) or on demand ([`Self::new`],
/// so a queue that only ever carries a little costs a little). Either way
/// growth is fallible and a steady state allocates nothing.
pub struct ByteQueue {
    // Its length is all the storage committed and never shrinks, so wiping
    // the slice wipes every byte the queue has held.
    arena: Vec<u8>,
    head: usize,
    tail: usize,
    bound: usize,
}

impl ByteQueue {
    /// An empty queue that may hold up to `bound` bytes, committing storage
    /// only as bytes arrive.
    #[must_use]
    pub const fn new(bound: usize) -> Self {
        Self {
            arena: Vec::new(),
            head: 0,
            tail: 0,
            bound,
        }
    }

    /// An empty queue with all `bound` bytes of storage committed now.
    ///
    /// # Errors
    ///
    /// [`TryReserveError::AllocFailed`] when the allocator refuses the arena.
    pub fn committed(bound: usize) -> Result<Self, TryReserveError> {
        let mut queue = Self::new(bound);
        queue.grow_to(bound)?;
        Ok(queue)
    }

    /// The most bytes the queue will ever hold.
    #[must_use]
    pub const fn bound(&self) -> usize {
        self.bound
    }

    /// Bytes queued and not yet consumed.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.tail - self.head
    }

    /// Whether nothing is queued.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.tail == self.head
    }

    /// Bytes that may still be appended before the bound is reached.
    #[must_use]
    pub const fn room(&self) -> usize {
        self.bound - self.len()
    }

    /// Bytes of storage committed so far — what the queue costs its holder
    /// now, which never exceeds [`Self::bound`].
    #[must_use]
    pub const fn storage(&self) -> usize {
        self.arena.len()
    }

    /// The queued bytes, oldest first.
    #[must_use]
    pub fn pending(&self) -> &[u8] {
        &self.arena[self.head..self.tail]
    }

    /// The queued bytes, oldest first, for a holder that transforms them in
    /// place.
    pub fn pending_mut(&mut self) -> &mut [u8] {
        &mut self.arena[self.head..self.tail]
    }

    /// Make `additional` bytes appendable without allocating again.
    ///
    /// # Errors
    ///
    /// [`QueueError::Full`] when `additional` exceeds [`Self::room`], or
    /// [`QueueError::Alloc`] when the storage cannot be committed. Nothing is
    /// queued or lost either way.
    pub fn reserve(&mut self, additional: usize) -> Result<(), QueueError> {
        if additional > self.room() {
            return Err(QueueError::Full);
        }
        if self.arena.len() - self.tail >= additional {
            return Ok(());
        }
        let needed = self.len() + additional;
        if needed <= self.arena.len() {
            self.compact();
            return Ok(());
        }
        // Doubling keeps a queue that grows by small appends from paying an
        // allocation per append; the bound caps it.
        let target = needed
            .max(self.arena.len().saturating_mul(2))
            .min(self.bound);
        self.grow_to(target).map_err(QueueError::Alloc)
    }

    /// Claim exactly `n` bytes at the tail for the caller to fill, committing
    /// storage if it must. All or nothing, so a holder writing a record can
    /// never leave half of one queued.
    ///
    /// # Errors
    ///
    /// As [`Self::reserve`].
    pub fn append_slot(&mut self, n: usize) -> Result<&mut [u8], QueueError> {
        self.reserve(n)?;
        let start = self.tail;
        self.tail += n;
        Ok(&mut self.arena[start..self.tail])
    }

    /// Append as much of `bytes` as the bound admits, returning how many were
    /// taken. A short append is back-pressure: the holder offers the rest
    /// once it has consumed something.
    ///
    /// # Errors
    ///
    /// [`TryReserveError::AllocFailed`] when the storage cannot be committed;
    /// nothing is appended.
    pub fn push_slice(&mut self, bytes: &[u8]) -> Result<usize, TryReserveError> {
        let take = bytes.len().min(self.room());
        match self.append_slot(take) {
            Ok(slot) => {
                slot.copy_from_slice(&bytes[..take]);
                Ok(take)
            }
            Err(QueueError::Alloc(err)) => Err(err),
            // `take` never exceeds the room.
            Err(QueueError::Full) => Ok(0),
        }
    }

    /// Hand the committed free window at the tail to `read`, keeping as many
    /// bytes as it reports. Nothing is allocated, so a holder that wants a
    /// larger window reserves it first.
    ///
    /// # Errors
    ///
    /// Whatever `read` reports; nothing is appended then.
    pub fn fill<E>(
        &mut self,
        read: impl FnOnce(&mut [u8]) -> Result<usize, E>,
    ) -> Result<usize, E> {
        if self.head > 0 && self.tail == self.arena.len() {
            self.compact();
        }
        let window = &mut self.arena[self.tail..];
        let read = read(window)?.min(window.len());
        self.tail += read;
        Ok(read)
    }

    /// Drop the first `n` queued bytes.
    pub fn consume(&mut self, n: usize) {
        self.head += n.min(self.len());
        if self.head == self.tail {
            // Fully drained: reset rather than move anything, which is the
            // ordinary case for a holder that drains what it queues.
            self.head = 0;
            self.tail = 0;
        } else if self.head >= self.arena.len() / 2 {
            // The consumed prefix has reached half the arena, so the move is
            // paid for by at least that many consumed bytes.
            self.compact();
        }
    }

    /// Keep only the oldest `len` queued bytes, dropping what was appended
    /// after them — how a holder abandons a record it could not finish.
    pub fn truncate(&mut self, len: usize) {
        self.tail = self.head + len.min(self.len());
    }

    /// Drop everything queued. Committed storage is kept.
    pub fn clear(&mut self) {
        self.head = 0;
        self.tail = 0;
    }

    /// Move the queued bytes to the start of the arena.
    fn compact(&mut self) {
        if self.head == 0 {
            return;
        }
        self.arena.copy_within(self.head..self.tail, 0);
        self.tail -= self.head;
        self.head = 0;
    }

    /// Commit storage up to `size` bytes, compacting as it goes.
    ///
    /// A fresh arena rather than a reallocation, so the old one can be wiped
    /// before it is freed.
    fn grow_to(&mut self, size: usize) -> Result<(), TryReserveError> {
        if size <= self.arena.len() {
            return Ok(());
        }
        let mut fresh = Vec::new();
        fresh
            .try_reserve_exact(size)
            .map_err(|_| TryReserveError::AllocFailed)?;
        fresh.extend_from_slice(&self.arena[self.head..self.tail]);
        fresh.resize(size, 0);
        let mut old = core::mem::replace(&mut self.arena, fresh);
        old.as_mut_slice().zeroize();
        self.tail -= self.head;
        self.head = 0;
        Ok(())
    }

    /// Erase all committed storage, consumed and queued bytes alike.
    fn wipe(&mut self) {
        self.arena.as_mut_slice().zeroize();
    }
}

impl Drop for ByteQueue {
    fn drop(&mut self) {
        self.wipe();
    }
}

impl core::fmt::Debug for ByteQueue {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        // The queued bytes are the holder's data; the shape is enough.
        f.debug_struct("ByteQueue")
            .field("len", &self.len())
            .field("bound", &self.bound)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
#[path = "bytequeue_tests.rs"]
mod tests;
