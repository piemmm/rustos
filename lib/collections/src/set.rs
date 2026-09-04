//! [`HashSet`] — an unordered set with expected constant-time membership.

use core::borrow::Borrow;
use core::fmt;
use core::hash::{BuildHasher, Hash};
use core::iter::FusedIterator;

use crate::map::{self, HashMap};
use crate::TryReserveError;

/// A hash set, with [`HashMap`]'s guarantees and the same choice of `S`.
///
/// It is that map with a zero-sized value, so it inherits the layout, the
/// probe behaviour, and the fallible-allocation rules rather than repeating
/// them, and costs exactly one control byte and one `T` slot per bucket.
pub struct HashSet<T, S> {
    map: HashMap<T, (), S>,
}

impl<T, S> HashSet<T, S> {
    /// An empty set that has not allocated.
    #[must_use]
    pub const fn with_hasher(hasher: S) -> Self {
        Self {
            map: HashMap::with_hasher(hasher),
        }
    }

    /// Live members.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.map.len()
    }

    /// `true` if the set holds no members.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    /// Members the set can hold before it must grow.
    #[must_use]
    pub const fn capacity(&self) -> usize {
        self.map.capacity()
    }

    /// Bytes of heap the set holds — its resident footprint.
    #[must_use]
    pub fn allocated_bytes(&self) -> usize {
        self.map.allocated_bytes()
    }

    /// The set's hasher.
    #[must_use]
    pub const fn hasher(&self) -> &S {
        self.map.hasher()
    }

    /// Drop every member, keeping the allocation.
    pub fn clear(&mut self) {
        self.map.clear();
    }

    /// The members, in unspecified order.
    pub fn iter(&self) -> Iter<'_, T> {
        Iter {
            inner: self.map.keys(),
        }
    }

    /// Keep only the members `keep` accepts.
    pub fn retain(&mut self, mut keep: impl FnMut(&T) -> bool) {
        self.map.retain(|member, ()| keep(member));
    }
}

impl<T, S> HashSet<T, S>
where
    T: Eq + Hash,
    S: BuildHasher,
{
    /// An empty set with room for `capacity` members.
    ///
    /// # Errors
    ///
    /// [`TryReserveError`] when the allocation fails or the capacity cannot be
    /// represented.
    pub fn try_with_capacity_and_hasher(
        capacity: usize,
        hasher: S,
    ) -> Result<Self, TryReserveError> {
        Ok(Self {
            map: HashMap::try_with_capacity_and_hasher(capacity, hasher)?,
        })
    }

    /// Make room for `additional` further members.
    ///
    /// # Errors
    ///
    /// [`TryReserveError`] when the allocation fails or the capacity cannot be
    /// represented. The set is left untouched.
    pub fn try_reserve(&mut self, additional: usize) -> Result<(), TryReserveError> {
        self.map.try_reserve(additional)
    }

    /// `true` if `value` is a member.
    pub fn contains<Q>(&self, value: &Q) -> bool
    where
        T: Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        self.map.contains_key(value)
    }

    /// The stored member equal to `value`.
    pub fn get<Q>(&self, value: &Q) -> Option<&T>
    where
        T: Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        self.map.get_key_value(value).map(|(member, ())| member)
    }

    /// Insert `value`, reporting whether it was newly added.
    ///
    /// # Errors
    ///
    /// [`TryReserveError`] when the set must grow and the allocation fails.
    /// `value` is dropped in that case; a caller that must keep it calls
    /// [`try_reserve`](Self::try_reserve) first.
    pub fn try_insert(&mut self, value: T) -> Result<bool, TryReserveError> {
        Ok(self.map.try_insert(value, ())?.is_none())
    }

    /// Remove `value`, reporting whether it was a member.
    pub fn remove<Q>(&mut self, value: &Q) -> bool
    where
        T: Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        self.map.remove(value).is_some()
    }

    /// Remove `value`, returning the stored member.
    pub fn take<Q>(&mut self, value: &Q) -> Option<T>
    where
        T: Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        self.map.remove_entry(value).map(|(member, ())| member)
    }
}

impl<T: fmt::Debug, S> fmt::Debug for HashSet<T, S> {
    /// Renders the members in the set's unspecified iteration order, so the
    /// text is a snapshot for a human, never something to compare.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_set().entries(self.iter()).finish()
    }
}

/// Shared iterator over a set's members.
pub struct Iter<'a, T> {
    inner: map::Keys<'a, T, ()>,
}

impl<'a, T> Iterator for Iter<'a, T> {
    type Item = &'a T;

    fn next(&mut self) -> Option<&'a T> {
        self.inner.next()
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.inner.size_hint()
    }
}

impl<T> ExactSizeIterator for Iter<'_, T> {
    fn len(&self) -> usize {
        self.inner.len()
    }
}

impl<T> FusedIterator for Iter<'_, T> {}

/// Owning iterator over a set's members.
pub struct IntoIter<T, S> {
    inner: map::IntoIter<T, (), S>,
}

impl<T, S> Iterator for IntoIter<T, S> {
    type Item = T;

    fn next(&mut self) -> Option<T> {
        self.inner.next().map(|(member, ())| member)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.inner.size_hint()
    }
}

impl<T, S> ExactSizeIterator for IntoIter<T, S> {
    fn len(&self) -> usize {
        self.inner.len()
    }
}

impl<T, S> FusedIterator for IntoIter<T, S> {}

impl<T, S> IntoIterator for HashSet<T, S> {
    type Item = T;
    type IntoIter = IntoIter<T, S>;

    fn into_iter(self) -> IntoIter<T, S> {
        IntoIter {
            inner: self.map.into_iter(),
        }
    }
}

impl<'a, T, S> IntoIterator for &'a HashSet<T, S> {
    type Item = &'a T;
    type IntoIter = Iter<'a, T>;

    fn into_iter(self) -> Iter<'a, T> {
        self.iter()
    }
}
