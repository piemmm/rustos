//! A vector that stays inline while it is small and spills to the heap.

use alloc::vec::Vec;
use core::fmt;
use core::hash::{Hash, Hasher};
use core::mem;
use core::ops::{Deref, DerefMut};
use core::slice;

use crate::{ArrayVec, TryReserveError};

/// Whether `count` elements of `T` have a representable allocation.
fn layout_fits<T>(count: usize) -> Result<(), TryReserveError> {
    let bytes = count
        .checked_mul(mem::size_of::<T>())
        .ok_or(TryReserveError::CapacityOverflow)?;
    let ceiling = usize::try_from(isize::MAX).map_err(|_| TryReserveError::CapacityOverflow)?;
    if bytes > ceiling {
        return Err(TryReserveError::CapacityOverflow);
    }
    Ok(())
}

/// Where a [`SmallVec`]'s elements currently live.
enum Storage<T, const N: usize> {
    /// Held inline, allocating nothing.
    Inline(ArrayVec<T, N>),
    /// Spilled to the heap, which the vector stays on for the rest of its life.
    Spilled(Vec<T>),
}

/// A vector holding up to `N` elements inline and spilling to the heap beyond
/// that.
///
/// For the hot paths that carry one to a handful of elements and pay a heap
/// allocation for the privilege. `N` is the caller's own bound, chosen at the
/// use site from what the common case actually holds; exceeding it costs one
/// allocation and correct behaviour, never a refusal, so the bound is a
/// performance choice rather than a ceiling.
///
/// A vector never returns inline once it has spilled: re-inlining would trade
/// a branch on every subsequent operation for a saving the growth pattern that
/// caused the spill is unlikely to want.
///
/// Nothing here panics. Every allocating operation is fallible, including the
/// copy — [`try_clone`](Self::try_clone) rather than `Clone`, since an
/// infallible clone would be an allocation with nowhere to report failure.
pub struct SmallVec<T, const N: usize>(Storage<T, N>);

impl<T, const N: usize> SmallVec<T, N> {
    /// An empty vector, inline. `const`, so one can back a `static`.
    #[must_use]
    pub const fn new() -> Self {
        Self(Storage::Inline(ArrayVec::new()))
    }

    /// An empty vector with room for `capacity` elements, spilling immediately
    /// when that exceeds `N`.
    ///
    /// # Errors
    ///
    /// [`TryReserveError`] when the heap cannot supply the block.
    pub fn try_with_capacity(capacity: usize) -> Result<Self, TryReserveError> {
        let mut out = Self::new();
        out.try_reserve(capacity)?;
        Ok(out)
    }

    /// Live element count.
    #[must_use]
    pub fn len(&self) -> usize {
        match &self.0 {
            Storage::Inline(inline) => inline.len(),
            Storage::Spilled(heap) => heap.len(),
        }
    }

    /// Whether no element is held.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Elements that fit without further growth.
    #[must_use]
    pub fn capacity(&self) -> usize {
        match &self.0 {
            Storage::Inline(_) => N,
            Storage::Spilled(heap) => heap.capacity(),
        }
    }

    /// Whether the elements have moved to the heap.
    #[must_use]
    pub fn spilled(&self) -> bool {
        matches!(self.0, Storage::Spilled(_))
    }

    /// The live elements, in order.
    #[must_use]
    pub fn as_slice(&self) -> &[T] {
        match &self.0 {
            Storage::Inline(inline) => inline.as_slice(),
            Storage::Spilled(heap) => heap.as_slice(),
        }
    }

    /// The live elements, mutably.
    #[must_use]
    pub fn as_mut_slice(&mut self) -> &mut [T] {
        match &mut self.0 {
            Storage::Inline(inline) => inline.as_mut_slice(),
            Storage::Spilled(heap) => heap.as_mut_slice(),
        }
    }

    /// Make room for `additional` further elements, spilling if they do not fit
    /// inline.
    ///
    /// # Errors
    ///
    /// [`TryReserveError`] when the heap cannot supply the block, or when the
    /// requested capacity has no representable layout.
    pub fn try_reserve(&mut self, additional: usize) -> Result<(), TryReserveError> {
        let inline = match &mut self.0 {
            Storage::Spilled(heap) => {
                return heap
                    .try_reserve(additional)
                    .map_err(|_| TryReserveError::AllocFailed)
            }
            Storage::Inline(inline) => inline,
        };
        let want = inline
            .len()
            .checked_add(additional)
            .ok_or(TryReserveError::CapacityOverflow)?;
        if want <= N {
            return Ok(());
        }
        layout_fits::<T>(want)?;
        let mut heap = Vec::new();
        heap.try_reserve_exact(want)
            .map_err(|_| TryReserveError::AllocFailed)?;
        // Reserved exactly, so moving the inline elements across cannot
        // reallocate and therefore cannot fail.
        heap.extend(mem::take(inline));
        self.0 = Storage::Spilled(heap);
        Ok(())
    }

    /// Append `value`, spilling to the heap if it does not fit inline.
    ///
    /// # Errors
    ///
    /// [`TryReserveError`] when the heap cannot supply the growth. The value is
    /// dropped, as it is for every allocating insertion in this crate.
    pub fn try_push(&mut self, value: T) -> Result<(), TryReserveError> {
        self.try_reserve(1)?;
        match &mut self.0 {
            // Unreachable after the reserve above; mapped rather than asserted
            // so no path here can panic.
            Storage::Inline(inline) => inline
                .try_push(value)
                .map_err(|_| TryReserveError::AllocFailed),
            Storage::Spilled(heap) => {
                heap.push(value);
                Ok(())
            }
        }
    }

    /// Remove and return the last element.
    pub fn pop(&mut self) -> Option<T> {
        match &mut self.0 {
            Storage::Inline(inline) => inline.pop(),
            Storage::Spilled(heap) => heap.pop(),
        }
    }

    /// Remove the element at `index`, shifting the tail one place left.
    pub fn remove(&mut self, index: usize) -> Option<T> {
        match &mut self.0 {
            Storage::Inline(inline) => inline.remove(index),
            Storage::Spilled(heap) => (index < heap.len()).then(|| heap.remove(index)),
        }
    }

    /// Remove the element at `index`, moving the last element into its place.
    pub fn swap_remove(&mut self, index: usize) -> Option<T> {
        match &mut self.0 {
            Storage::Inline(inline) => inline.swap_remove(index),
            Storage::Spilled(heap) => (index < heap.len()).then(|| heap.swap_remove(index)),
        }
    }

    /// Drop every element past `len`.
    pub fn truncate(&mut self, len: usize) {
        match &mut self.0 {
            Storage::Inline(inline) => inline.truncate(len),
            Storage::Spilled(heap) => heap.truncate(len),
        }
    }

    /// Drop every element, keeping the capacity already acquired.
    pub fn clear(&mut self) {
        self.truncate(0);
    }

    /// Keep only the elements `keep` accepts, preserving their order.
    pub fn retain<F>(&mut self, mut keep: F)
    where
        F: FnMut(&T) -> bool,
    {
        match &mut self.0 {
            Storage::Inline(inline) => inline.retain(keep),
            Storage::Spilled(heap) => heap.retain(|item| keep(item)),
        }
    }
}

impl<T: Clone, const N: usize> SmallVec<T, N> {
    /// Append a clone of every element of `items`.
    ///
    /// # Errors
    ///
    /// [`TryReserveError`] when the heap cannot supply the growth. A failure
    /// leaves the vector unchanged, because the whole reservation is taken
    /// before the first clone.
    pub fn try_extend_from_slice(&mut self, items: &[T]) -> Result<(), TryReserveError> {
        self.try_reserve(items.len())?;
        for item in items {
            self.try_push(item.clone())?;
        }
        Ok(())
    }

    /// Copy the vector, inline when it fits.
    ///
    /// # Errors
    ///
    /// [`TryReserveError`] when the heap cannot supply the copy.
    pub fn try_clone(&self) -> Result<Self, TryReserveError> {
        let mut out = Self::new();
        out.try_extend_from_slice(self.as_slice())?;
        Ok(out)
    }
}

impl<T, const N: usize> Default for SmallVec<T, N> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T, const N: usize> Deref for SmallVec<T, N> {
    type Target = [T];

    fn deref(&self) -> &[T] {
        self.as_slice()
    }
}

impl<T, const N: usize> DerefMut for SmallVec<T, N> {
    fn deref_mut(&mut self) -> &mut [T] {
        self.as_mut_slice()
    }
}

impl<T: fmt::Debug, const N: usize> fmt::Debug for SmallVec<T, N> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_list().entries(self.as_slice()).finish()
    }
}

impl<T: PartialEq, const N: usize> PartialEq for SmallVec<T, N> {
    fn eq(&self, other: &Self) -> bool {
        self.as_slice() == other.as_slice()
    }
}

impl<T: Eq, const N: usize> Eq for SmallVec<T, N> {}

impl<T: PartialEq, const N: usize> PartialEq<[T]> for SmallVec<T, N> {
    fn eq(&self, other: &[T]) -> bool {
        self.as_slice() == other
    }
}

impl<T: Hash, const N: usize> Hash for SmallVec<T, N> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.as_slice().hash(state);
    }
}

impl<T: Clone, const N: usize> TryFrom<&[T]> for SmallVec<T, N> {
    type Error = TryReserveError;

    fn try_from(items: &[T]) -> Result<Self, TryReserveError> {
        let mut out = Self::new();
        out.try_extend_from_slice(items)?;
        Ok(out)
    }
}

impl<'a, T, const N: usize> IntoIterator for &'a SmallVec<T, N> {
    type Item = &'a T;
    type IntoIter = slice::Iter<'a, T>;

    fn into_iter(self) -> slice::Iter<'a, T> {
        self.as_slice().iter()
    }
}

impl<'a, T, const N: usize> IntoIterator for &'a mut SmallVec<T, N> {
    type Item = &'a mut T;
    type IntoIter = slice::IterMut<'a, T>;

    fn into_iter(self) -> slice::IterMut<'a, T> {
        self.as_mut_slice().iter_mut()
    }
}

impl<T, const N: usize> IntoIterator for SmallVec<T, N> {
    type Item = T;
    type IntoIter = IntoIter<T, N>;

    fn into_iter(self) -> IntoIter<T, N> {
        IntoIter(match self.0 {
            Storage::Inline(inline) => IntoIterInner::Inline(inline.into_iter()),
            Storage::Spilled(heap) => IntoIterInner::Spilled(heap.into_iter()),
        })
    }
}

/// Whichever storage the consuming iterator is walking.
enum IntoIterInner<T, const N: usize> {
    Inline(crate::arrayvec::IntoIter<T, N>),
    Spilled(alloc::vec::IntoIter<T>),
}

/// Consuming iterator over a [`SmallVec`], yielding elements front to back.
pub struct IntoIter<T, const N: usize>(IntoIterInner<T, N>);

impl<T, const N: usize> Iterator for IntoIter<T, N> {
    type Item = T;

    fn next(&mut self) -> Option<T> {
        match &mut self.0 {
            IntoIterInner::Inline(inline) => inline.next(),
            IntoIterInner::Spilled(heap) => heap.next(),
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        match &self.0 {
            IntoIterInner::Inline(inline) => inline.size_hint(),
            IntoIterInner::Spilled(heap) => heap.size_hint(),
        }
    }
}

impl<T, const N: usize> DoubleEndedIterator for IntoIter<T, N> {
    fn next_back(&mut self) -> Option<T> {
        match &mut self.0 {
            IntoIterInner::Inline(inline) => inline.next_back(),
            IntoIterInner::Spilled(heap) => heap.next_back(),
        }
    }
}

impl<T, const N: usize> ExactSizeIterator for IntoIter<T, N> {}

#[cfg(test)]
#[path = "smallvec_tests.rs"]
mod tests;
