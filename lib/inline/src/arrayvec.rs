//! A vector whose capacity is a compile-time bound the caller chooses.

use core::fmt;
use core::hash::{Hash, Hasher};
use core::mem::MaybeUninit;
use core::ops::{Deref, DerefMut};
use core::ptr;
use core::slice;

use crate::CapacityError;

/// A vector of at most `N` elements held inline, allocating nothing.
///
/// `N` is the caller's own bound, chosen at the use site, so this is one of the
/// container shapes a fixed capacity is legitimate for: a path that must not
/// reach an allocator (an interrupt handler, early boot), or a buffer whose
/// ceiling is dictated by what it accumulates rather than by the machine.
/// Anything whose size should follow the hardware wants `alloc`'s `Vec`, or
/// `tairix_collections::SmallVec` when the common case is small.
///
/// Nothing here panics. [`try_push`](Self::try_push) hands a rejected value
/// back rather than dropping it, and every positional operation answers with
/// [`Option`] instead of asserting a bound. The deref to `[T]` supplies
/// iteration, search, and sorting.
pub struct ArrayVec<T, const N: usize> {
    /// `slots[..len]` hold live elements; the rest are uninitialised.
    slots: [MaybeUninit<T>; N],
    len: usize,
}

impl<T, const N: usize> ArrayVec<T, N> {
    /// An empty vector. `const`, so one can back a `static`.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            slots: [const { MaybeUninit::uninit() }; N],
            len: 0,
        }
    }

    /// The fixed capacity, `N`.
    #[must_use]
    pub const fn capacity(&self) -> usize {
        N
    }

    /// Live element count.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.len
    }

    /// Whether no element is held.
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

    /// The live elements, oldest index first.
    #[must_use]
    pub const fn as_slice(&self) -> &[T] {
        // SAFETY: the first `len` slots are initialised, and `MaybeUninit<T>`
        // shares `T`'s layout, so the prefix is a valid `[T]` of that length.
        unsafe { slice::from_raw_parts(self.slots.as_ptr().cast::<T>(), self.len) }
    }

    /// The live elements, mutably.
    #[must_use]
    pub const fn as_mut_slice(&mut self) -> &mut [T] {
        // SAFETY: as `as_slice`, and `&mut self` makes this the only borrow.
        unsafe { slice::from_raw_parts_mut(self.slots.as_mut_ptr().cast::<T>(), self.len) }
    }

    /// Append `value`.
    ///
    /// # Errors
    ///
    /// [`CapacityError`] carrying `value` back when the vector is full.
    pub fn try_push(&mut self, value: T) -> Result<(), CapacityError<T>> {
        if self.len == N {
            return Err(CapacityError::new(value));
        }
        self.slots[self.len].write(value);
        self.len += 1;
        Ok(())
    }

    /// Remove and return the last element.
    pub fn pop(&mut self) -> Option<T> {
        if self.len == 0 {
            return None;
        }
        self.len -= 1;
        // SAFETY: the slot was initialised while live and now sits outside the
        // live prefix, so this read consumes it exactly once.
        Some(unsafe { self.slots[self.len].assume_init_read() })
    }

    /// Remove the element at `index`, shifting the tail one place left.
    pub fn remove(&mut self, index: usize) -> Option<T> {
        if index >= self.len {
            return None;
        }
        // SAFETY: `index < len`, so the slot is initialised and the read
        // consumes it. The tail shifts down over the vacated slot and the
        // length shrinks by one, so the slot the shift duplicated at the top
        // now sits outside the live prefix and is never read again.
        let taken = unsafe {
            let base = self.slots.as_mut_ptr().cast::<T>();
            let taken = ptr::read(base.add(index));
            ptr::copy(base.add(index + 1), base.add(index), self.len - index - 1);
            taken
        };
        self.len -= 1;
        Some(taken)
    }

    /// Remove the element at `index`, moving the last element into its place.
    ///
    /// Constant-time, and the only removal that does not preserve order.
    pub fn swap_remove(&mut self, index: usize) -> Option<T> {
        if index >= self.len {
            return None;
        }
        let last = self.len - 1;
        self.as_mut_slice().swap(index, last);
        self.pop()
    }

    /// Drop every element past `len`. A `len` at or above the current length
    /// changes nothing.
    pub fn truncate(&mut self, len: usize) {
        if len >= self.len {
            return;
        }
        let dropped = self.len - len;
        self.len = len;
        // SAFETY: the slots `len..len + dropped` were initialised and now sit
        // outside the live prefix — the length was lowered first, so an
        // unwinding element drop cannot make this run twice over them.
        unsafe {
            let base = self.slots.as_mut_ptr().cast::<T>().add(len);
            ptr::drop_in_place(ptr::slice_from_raw_parts_mut(base, dropped));
        }
    }

    /// Drop every element.
    pub fn clear(&mut self) {
        self.truncate(0);
    }

    /// Keep only the elements `keep` accepts, preserving their order.
    pub fn retain<F>(&mut self, mut keep: F)
    where
        F: FnMut(&T) -> bool,
    {
        /// Restores a correct length however the sweep ends, shifting the tail
        /// it never reached down over the gap the rejections left. A predicate
        /// that unwinds therefore loses exactly the elements already rejected
        /// and keeps every other, rather than leaking the untested tail or
        /// leaving the vector describing a slot twice.
        struct Backshift<'a, T, const N: usize> {
            vec: &'a mut ArrayVec<T, N>,
            /// Elements whose verdict is known.
            processed: usize,
            /// Of those, how many were rejected and dropped.
            deleted: usize,
            original_len: usize,
        }

        impl<T, const N: usize> Drop for Backshift<'_, T, N> {
            fn drop(&mut self) {
                let tail = self.original_len - self.processed;
                if self.deleted > 0 && tail > 0 {
                    // SAFETY: `slots[processed..original_len]` still hold live
                    // elements the length does not yet claim, and
                    // `deleted <= processed`, so the destination lies below the
                    // source and inside the array.
                    unsafe {
                        let base = self.vec.slots.as_mut_ptr();
                        ptr::copy(
                            base.add(self.processed),
                            base.add(self.processed - self.deleted),
                            tail,
                        );
                    }
                }
                self.vec.len = self.original_len - self.deleted;
            }
        }

        // The length is the guard's to restore for the whole sweep, so no
        // early exit can leave it describing a slot that has moved.
        let original_len = core::mem::replace(&mut self.len, 0);
        let mut guard = Backshift {
            vec: self,
            processed: 0,
            deleted: 0,
            original_len,
        };
        for index in 0..original_len {
            // SAFETY: `index < original_len` and no earlier iteration touched
            // this slot — a survivor moves strictly *down* and a rejection is
            // dropped in place — so it is initialised and live.
            let verdict = keep(unsafe { guard.vec.slots[index].assume_init_ref() });
            guard.processed = index + 1;
            if verdict {
                if guard.deleted > 0 {
                    let destination = index - guard.deleted;
                    // SAFETY: both indices are below `original_len <= N`, and
                    // the destination is a slot a rejection vacated, so no live
                    // element is overwritten.
                    unsafe {
                        let base = guard.vec.slots.as_mut_ptr();
                        ptr::copy_nonoverlapping(base.add(index), base.add(destination), 1);
                    }
                }
            } else {
                guard.deleted += 1;
                // SAFETY: the slot is initialised, and counting it deleted
                // first means nothing reads or drops it again — including the
                // guard, if this drop unwinds.
                unsafe { guard.vec.slots[index].assume_init_drop() };
            }
        }
    }
}

impl<T: Clone, const N: usize> ArrayVec<T, N> {
    /// Append a clone of every element of `items`.
    ///
    /// All or nothing: a slice that does not fit is refused whole, so the
    /// vector never holds a partial copy.
    ///
    /// # Errors
    ///
    /// [`CapacityError`] when `items` is longer than the remaining capacity.
    pub fn try_extend_from_slice(&mut self, items: &[T]) -> Result<(), CapacityError> {
        if items.len() > self.remaining_capacity() {
            return Err(CapacityError::new(()));
        }
        for item in items {
            self.slots[self.len].write(item.clone());
            self.len += 1;
        }
        Ok(())
    }
}

impl<T, const N: usize> Drop for ArrayVec<T, N> {
    fn drop(&mut self) {
        self.clear();
    }
}

impl<T, const N: usize> Default for ArrayVec<T, N> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T, const N: usize> Deref for ArrayVec<T, N> {
    type Target = [T];

    fn deref(&self) -> &[T] {
        self.as_slice()
    }
}

impl<T, const N: usize> DerefMut for ArrayVec<T, N> {
    fn deref_mut(&mut self) -> &mut [T] {
        self.as_mut_slice()
    }
}

impl<T: Clone, const N: usize> Clone for ArrayVec<T, N> {
    fn clone(&self) -> Self {
        let mut out = Self::new();
        for item in self.as_slice() {
            out.slots[out.len].write(item.clone());
            out.len += 1;
        }
        out
    }
}

impl<T: fmt::Debug, const N: usize> fmt::Debug for ArrayVec<T, N> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_list().entries(self.as_slice()).finish()
    }
}

impl<T: PartialEq, const N: usize> PartialEq for ArrayVec<T, N> {
    fn eq(&self, other: &Self) -> bool {
        self.as_slice() == other.as_slice()
    }
}

impl<T: Eq, const N: usize> Eq for ArrayVec<T, N> {}

impl<T: PartialEq, const N: usize> PartialEq<[T]> for ArrayVec<T, N> {
    fn eq(&self, other: &[T]) -> bool {
        self.as_slice() == other
    }
}

impl<T: Hash, const N: usize> Hash for ArrayVec<T, N> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.as_slice().hash(state);
    }
}

impl<T: Clone, const N: usize> TryFrom<&[T]> for ArrayVec<T, N> {
    type Error = CapacityError;

    fn try_from(items: &[T]) -> Result<Self, CapacityError> {
        let mut out = Self::new();
        out.try_extend_from_slice(items)?;
        Ok(out)
    }
}

impl<'a, T, const N: usize> IntoIterator for &'a ArrayVec<T, N> {
    type Item = &'a T;
    type IntoIter = slice::Iter<'a, T>;

    fn into_iter(self) -> slice::Iter<'a, T> {
        self.as_slice().iter()
    }
}

impl<'a, T, const N: usize> IntoIterator for &'a mut ArrayVec<T, N> {
    type Item = &'a mut T;
    type IntoIter = slice::IterMut<'a, T>;

    fn into_iter(self) -> slice::IterMut<'a, T> {
        self.as_mut_slice().iter_mut()
    }
}

impl<T, const N: usize> IntoIterator for ArrayVec<T, N> {
    type Item = T;
    type IntoIter = IntoIter<T, N>;

    fn into_iter(self) -> IntoIter<T, N> {
        IntoIter { vec: self, next: 0 }
    }
}

/// Consuming iterator over an [`ArrayVec`], yielding elements front to back.
pub struct IntoIter<T, const N: usize> {
    vec: ArrayVec<T, N>,
    /// Index of the next element to yield; `vec.slots[next..vec.len]` are the
    /// elements still owned by the iterator.
    next: usize,
}

impl<T, const N: usize> Iterator for IntoIter<T, N> {
    type Item = T;

    fn next(&mut self) -> Option<T> {
        if self.next == self.vec.len {
            return None;
        }
        let index = self.next;
        self.next += 1;
        // SAFETY: `index < vec.len` and every index below `next` has already
        // been yielded, so the slot is initialised and read exactly once.
        Some(unsafe { self.vec.slots[index].assume_init_read() })
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let remaining = self.vec.len - self.next;
        (remaining, Some(remaining))
    }
}

impl<T, const N: usize> DoubleEndedIterator for IntoIter<T, N> {
    fn next_back(&mut self) -> Option<T> {
        if self.next == self.vec.len {
            return None;
        }
        self.vec.len -= 1;
        // SAFETY: the slot is initialised, sits at or above `next` so it has
        // not been yielded from the front, and the shortened length puts it
        // outside the range anything else reads.
        Some(unsafe { self.vec.slots[self.vec.len].assume_init_read() })
    }
}

impl<T, const N: usize> ExactSizeIterator for IntoIter<T, N> {}

impl<T, const N: usize> Drop for IntoIter<T, N> {
    fn drop(&mut self) {
        let unyielded = self.vec.len - self.next;
        let from = self.next;
        // The vector is emptied before the drop so its own `Drop` cannot reach
        // slots this loop has already dropped or the iterator already yielded.
        self.vec.len = 0;
        // SAFETY: `slots[from..from + unyielded]` are initialised and neither
        // yielded nor dropped, and the vector no longer claims them.
        unsafe {
            let base = self.vec.slots.as_mut_ptr().cast::<T>().add(from);
            ptr::drop_in_place(ptr::slice_from_raw_parts_mut(base, unyielded));
        }
    }
}

#[cfg(test)]
#[path = "arrayvec_tests.rs"]
mod tests;
