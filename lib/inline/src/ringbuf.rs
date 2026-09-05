//! A fixed-capacity circular queue, constant time at both ends.

use core::fmt;
use core::iter::Chain;
use core::mem::MaybeUninit;
use core::ops::{Deref, Range};
use core::ptr;
use core::slice;
use core::sync::atomic::{compiler_fence, Ordering};

use crate::CapacityError;

/// Reduce a logical index known to be below `2 * cap` into `0..cap`.
///
/// One subtraction rather than a division: every caller here adds at most a
/// capacity to an index already inside the ring.
const fn wrap(index: usize, cap: usize) -> usize {
    if index >= cap {
        index - cap
    } else {
        index
    }
}

/// A queue of at most `N` elements over an inline slot array, allocating
/// nothing.
///
/// `N` is the caller's own bound, chosen at the use site: a type-ahead buffer
/// whose size follows what a human can type, a diagnostic tail whose whole
/// purpose is "the last `N` records", a driver hand-off ring sized to a
/// hardware burst. A bound rather than an unbounded queue is what stops a
/// wedged consumer letting a producer grow kernel memory without limit.
///
/// Push and pop are constant time at either end, and so is
/// [`get`](Self::get) by offset from the front. Nothing panics:
/// [`try_push_back`](Self::try_push_back) hands a refused value back and every
/// positional read answers with [`Option`].
///
/// # Elements that carried a secret
///
/// A container does not scrub the slots it frees — reuse inside one address
/// space is not a security boundary. A ring a credential *transits* is a
/// different case, and it has its own type: [`SecretRing`].
pub struct RingBuf<T, const N: usize> {
    /// The live elements occupy `(head + i) % N` for `i` in `0..len`; every
    /// other slot holds no live element.
    slots: [MaybeUninit<T>; N],
    /// Slot index of the front element, below `N` whenever `N` is non-zero.
    head: usize,
    len: usize,
}

impl<T, const N: usize> RingBuf<T, N> {
    /// An empty ring. `const`, so one can back a `static`.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            slots: [const { MaybeUninit::uninit() }; N],
            head: 0,
            len: 0,
        }
    }

    /// The fixed capacity, `N`.
    #[must_use]
    pub const fn capacity(&self) -> usize {
        N
    }

    /// Queued element count.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.len
    }

    /// Whether nothing is queued.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Whether the capacity is reached, so the next push is refused.
    #[must_use]
    pub const fn is_full(&self) -> bool {
        self.len == N
    }

    /// Elements that can still be pushed.
    #[must_use]
    pub const fn remaining_capacity(&self) -> usize {
        N - self.len
    }

    /// Slot index of the element `offset` places behind the front.
    const fn slot_of(&self, offset: usize) -> usize {
        wrap(self.head + offset, N)
    }

    /// The queued elements as at most two contiguous runs, front run first.
    fn runs(&self) -> (&[T], &[T]) {
        if self.len == 0 {
            return (&[], &[]);
        }
        let start = self.head;
        let first = self.len.min(N - start);
        // SAFETY: the whole live window is initialised and `MaybeUninit<T>`
        // shares `T`'s layout, so the run at `start` and the wrapped remainder
        // at zero are each a valid `[T]` of the length computed here.
        unsafe {
            let base = self.slots.as_ptr().cast::<T>();
            (
                slice::from_raw_parts(base.add(start), first),
                slice::from_raw_parts(base, self.len - first),
            )
        }
    }

    /// The queued elements, front to back.
    pub fn iter(&self) -> Chain<slice::Iter<'_, T>, slice::Iter<'_, T>> {
        let (front, back) = self.runs();
        front.iter().chain(back.iter())
    }

    /// The element `offset` places behind the front.
    #[must_use]
    pub fn get(&self, offset: usize) -> Option<&T> {
        if offset >= self.len {
            return None;
        }
        let slot = self.slot_of(offset);
        // SAFETY: `offset < len`, so the slot is inside the live window and
        // therefore initialised.
        Some(unsafe { self.slots[slot].assume_init_ref() })
    }

    /// The element `offset` places behind the front, mutably.
    #[must_use]
    pub fn get_mut(&mut self, offset: usize) -> Option<&mut T> {
        if offset >= self.len {
            return None;
        }
        let slot = self.slot_of(offset);
        // SAFETY: as `get`, and `&mut self` makes this the only borrow.
        Some(unsafe { self.slots[slot].assume_init_mut() })
    }

    /// The front element, the next one [`pop_front`](Self::pop_front) returns.
    #[must_use]
    pub fn front(&self) -> Option<&T> {
        self.get(0)
    }

    /// The back element, the most recently pushed.
    #[must_use]
    pub fn back(&self) -> Option<&T> {
        self.get(self.len.checked_sub(1)?)
    }

    /// Append `value` at the back.
    ///
    /// # Errors
    ///
    /// [`CapacityError`] carrying `value` back when the ring is full.
    pub fn try_push_back(&mut self, value: T) -> Result<(), CapacityError<T>> {
        if self.len == N {
            return Err(CapacityError::new(value));
        }
        let slot = self.slot_of(self.len);
        self.slots[slot].write(value);
        self.len += 1;
        Ok(())
    }

    /// Prepend `value` at the front.
    ///
    /// # Errors
    ///
    /// [`CapacityError`] carrying `value` back when the ring is full.
    pub fn try_push_front(&mut self, value: T) -> Result<(), CapacityError<T>> {
        if self.len == N {
            return Err(CapacityError::new(value));
        }
        // `N` is non-zero here, since a zero-capacity ring is always full.
        self.head = if self.head == 0 { N - 1 } else { self.head - 1 };
        self.slots[self.head].write(value);
        self.len += 1;
        Ok(())
    }

    /// Append `value` at the back, dropping the front element to make room
    /// when the ring is full and returning whatever was displaced.
    ///
    /// The push a bounded tail wants: recent history is the point, so losing
    /// the oldest record is the behaviour rather than a failure.
    pub fn push_back_overwrite(&mut self, value: T) -> Option<T> {
        if N == 0 {
            // Nowhere to store anything, so `value` is dropped here and
            // nothing was displaced.
            return None;
        }
        let evicted = if self.len == N {
            self.pop_front()
        } else {
            None
        };
        let slot = self.slot_of(self.len);
        self.slots[slot].write(value);
        self.len += 1;
        evicted
    }

    /// Remove and return the front element.
    pub fn pop_front(&mut self) -> Option<T> {
        if self.len == 0 {
            return None;
        }
        let slot = self.head;
        self.head = wrap(slot + 1, N);
        self.len -= 1;
        // SAFETY: the slot was inside the live window and the window has moved
        // past it, so this read consumes the element exactly once.
        Some(unsafe { self.slots[slot].assume_init_read() })
    }

    /// Remove and return the back element.
    pub fn pop_back(&mut self) -> Option<T> {
        if self.len == 0 {
            return None;
        }
        self.len -= 1;
        let slot = self.slot_of(self.len);
        // SAFETY: as `pop_front` — the shortened window no longer covers the
        // slot, so the element is read out exactly once.
        Some(unsafe { self.slots[slot].assume_init_read() })
    }

    /// Drop up to `count` elements from the front, returning how many went.
    pub fn discard_front(&mut self, count: usize) -> usize {
        let taken = count.min(self.len);
        if taken == 0 {
            return 0;
        }
        let start = self.head;
        let first = taken.min(N - start);
        self.head = wrap(start + taken, N);
        self.len -= taken;
        // SAFETY: the `taken` elements from the old front were live and the
        // window was narrowed past them first, so each is dropped exactly once
        // even if one of the drops unwinds.
        unsafe {
            let base = self.slots.as_mut_ptr().cast::<T>();
            ptr::drop_in_place(ptr::slice_from_raw_parts_mut(base.add(start), first));
            ptr::drop_in_place(ptr::slice_from_raw_parts_mut(base, taken - first));
        }
        taken
    }

    /// Drop every queued element.
    pub fn clear(&mut self) {
        self.discard_front(self.len);
    }
}

impl<T: Copy, const N: usize> RingBuf<T, N> {
    /// Append as much of `items` as fits at the back, returning how many were
    /// taken.
    ///
    /// A short push, so a producer applies back-pressure by retrying the
    /// remainder rather than overrunning the ring or blocking.
    pub fn push_slice(&mut self, items: &[T]) -> usize {
        let take = items.len().min(self.remaining_capacity());
        if take == 0 {
            return 0;
        }
        let start = self.slot_of(self.len);
        let first = take.min(N - start);
        // SAFETY: `MaybeUninit<T>` shares `T`'s layout, so an initialised
        // element is a valid `MaybeUninit<T>` to copy out of.
        let src: &[MaybeUninit<T>] = unsafe { slice::from_raw_parts(items.as_ptr().cast(), take) };
        let (head_src, tail_src) = src.split_at(first);
        self.slots[start..start + first].copy_from_slice(head_src);
        self.slots[..take - first].copy_from_slice(tail_src);
        self.len += take;
        take
    }

    /// Append the whole of `items` at the back.
    ///
    /// # Errors
    ///
    /// [`CapacityError`] when `items` is longer than the remaining capacity;
    /// nothing is stored, so the ring never holds a partial record.
    pub fn try_push_slice(&mut self, items: &[T]) -> Result<(), CapacityError> {
        if items.len() > self.remaining_capacity() {
            return Err(CapacityError::new(()));
        }
        self.push_slice(items);
        Ok(())
    }

    /// Copy up to `out.len()` elements starting `offset` places behind the
    /// front into `out`, leaving the ring untouched. Returns how many were
    /// copied.
    ///
    /// What a variable-length frame needs: read the header, decide, and only
    /// then consume — so a caller that cannot accept the record leaves it
    /// queued.
    pub fn peek_slice(&self, offset: usize, out: &mut [T]) -> usize {
        let Some(available) = self.len.checked_sub(offset) else {
            return 0;
        };
        let want = out.len().min(available);
        let (front, back) = self.runs();
        let mut written = 0;
        let mut skip = offset;
        for run in [front, back] {
            let start = skip.min(run.len());
            skip -= start;
            let take = (run.len() - start).min(want - written);
            out[written..written + take].copy_from_slice(&run[start..start + take]);
            written += take;
        }
        written
    }

    /// Remove up to `out.len()` elements from the front into `out`, returning
    /// how many were taken.
    pub fn pop_slice(&mut self, out: &mut [T]) -> usize {
        let taken = self.peek_slice(0, out);
        self.discard_front(taken);
        taken
    }
}

impl<T, const N: usize> Drop for RingBuf<T, N> {
    fn drop(&mut self) {
        self.clear();
    }
}

/// A [`RingBuf`] that scrubs every slot it vacates, for a queue a credential
/// merely passes through.
///
/// A typed password crosses a console's type-ahead queue between the keyboard
/// driver and the login that reads it; a key event carrying it crosses the
/// desktop's input channel. Without a scrub the cleartext would sit in a
/// long-lived kernel buffer for the rest of the boot, well after its reader
/// took it — so this queue writes a blank over each slot as the element leaves,
/// over the whole store when its holder changes, and over the whole store again
/// as it is dropped, which is zero-on-free for memory that held a credential.
///
/// Each scrub is a **volatile** store followed by a compiler fence, so the
/// optimiser cannot discard a write to memory it can prove nothing reads again.
/// A plain assignment would be exactly such a write, and dropping it would
/// leave the cleartext in place — the scrub has to be un-elidable to be real.
///
/// The discipline is the type's, not the caller's: there is no `DerefMut`, so
/// the plain ring's non-scrubbing pops are simply out of reach and no edit can
/// bypass the scrub by accident. Reads reach through [`Deref`] unchanged.
///
/// # Invariant
///
/// Every slot is initialised, from construction onward: the constructor writes
/// the blank into all of them, every mutation writes either an element or the
/// blank, and a [`Copy`] element has no way to un-initialise a slot. That is
/// what makes [`backing_store`](Self::backing_store) — the whole store, live
/// window and vacated slots alike — safe to hand out, so a holder can prove
/// its scrubs left nothing behind.
pub struct SecretRing<T: Copy, const N: usize> {
    ring: RingBuf<T, N>,
    /// Written over every slot the ring vacates, and over all of them at
    /// construction, so the store is uniformly initialised.
    blank: T,
}

impl<T: Copy, const N: usize> SecretRing<T, N> {
    /// An empty ring with every slot already holding `blank`. `const`, so one
    /// can back a `static`.
    #[must_use]
    pub const fn new(blank: T) -> Self {
        Self {
            ring: RingBuf {
                slots: [MaybeUninit::new(blank); N],
                head: 0,
                len: 0,
            },
            blank,
        }
    }

    /// Append `value` at the back.
    ///
    /// # Errors
    ///
    /// [`CapacityError`] carrying `value` back when the ring is full.
    pub fn try_push_back(&mut self, value: T) -> Result<(), CapacityError<T>> {
        self.ring.try_push_back(value)
    }

    /// Append as much of `items` as fits at the back, returning how many were
    /// taken.
    pub fn push_slice(&mut self, items: &[T]) -> usize {
        self.ring.push_slice(items)
    }

    /// Remove and return the front element, scrubbing the slot it vacated.
    pub fn pop_front(&mut self) -> Option<T> {
        let slot = self.ring.head;
        let value = self.ring.pop_front()?;
        self.scrub(slot..slot + 1);
        Some(value)
    }

    /// Drop up to `count` elements from the front, scrubbing each slot they
    /// vacated. Returns how many went.
    pub fn discard_front(&mut self, count: usize) -> usize {
        let taken = count.min(self.ring.len);
        for _ in 0..taken {
            let slot = self.ring.head;
            self.ring.discard_front(1);
            self.scrub(slot..slot + 1);
        }
        taken
    }

    /// Empty the ring and scrub **every** slot, reaching the residue beyond the
    /// live window as well as what is still queued.
    pub fn purge(&mut self) {
        self.ring.clear();
        self.ring.head = 0;
        self.scrub(0..N);
    }

    /// The whole backing store, live window and vacated slots alike.
    ///
    /// The observation a holder needs to prove its scrubs: after a drain or a
    /// purge, no slot holds anything but the blank.
    #[must_use]
    pub fn backing_store(&self) -> &[T] {
        // SAFETY: every slot is initialised — the constructor blanks all of
        // them and no operation can un-initialise one — and `MaybeUninit<T>`
        // shares `T`'s layout.
        unsafe { slice::from_raw_parts(self.ring.slots.as_ptr().cast::<T>(), N) }
    }

    /// Overwrite the named slots with the blank, un-elidably.
    fn scrub(&mut self, slots: Range<usize>) {
        let blank = self.blank;
        let base = self.ring.slots.as_mut_ptr();
        for slot in slots {
            // SAFETY: `slot < N` for every caller here, and the destination is
            // a whole `MaybeUninit<T>`, which shares `T`'s layout and needs no
            // prior initialisation. The write is volatile so it survives
            // optimisation even though nothing reads the slot again.
            unsafe { ptr::write_volatile(base.add(slot), MaybeUninit::new(blank)) };
        }
        compiler_fence(Ordering::SeqCst);
    }
}

impl<T: Copy, const N: usize> Drop for SecretRing<T, N> {
    fn drop(&mut self) {
        self.purge();
    }
}

impl<T: Copy, const N: usize> Deref for SecretRing<T, N> {
    type Target = RingBuf<T, N>;

    fn deref(&self) -> &RingBuf<T, N> {
        &self.ring
    }
}

impl<T: Copy + fmt::Debug, const N: usize> fmt::Debug for SecretRing<T, N> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // The queued elements only: the residue beyond the window is not this
        // queue's data and a credential-bearing ring should not print it.
        f.debug_list().entries(self.ring.iter()).finish()
    }
}

impl<T, const N: usize> Default for RingBuf<T, N> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T: Clone, const N: usize> Clone for RingBuf<T, N> {
    fn clone(&self) -> Self {
        let mut out = Self::new();
        for item in self {
            let slot = out.slot_of(out.len);
            out.slots[slot].write(item.clone());
            out.len += 1;
        }
        out
    }
}

impl<T: fmt::Debug, const N: usize> fmt::Debug for RingBuf<T, N> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_list().entries(self.iter()).finish()
    }
}

impl<T: PartialEq, const N: usize> PartialEq for RingBuf<T, N> {
    fn eq(&self, other: &Self) -> bool {
        self.len == other.len && self.iter().eq(other.iter())
    }
}

impl<T: Eq, const N: usize> Eq for RingBuf<T, N> {}

impl<'a, T, const N: usize> IntoIterator for &'a RingBuf<T, N> {
    type Item = &'a T;
    type IntoIter = Chain<slice::Iter<'a, T>, slice::Iter<'a, T>>;

    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

#[cfg(test)]
#[path = "ringbuf_tests.rs"]
mod tests;
