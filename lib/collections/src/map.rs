//! [`HashMap`] — an unordered map with expected constant-time lookup.

use core::borrow::Borrow;
use core::fmt;
use core::hash::{BuildHasher, Hash};
use core::iter::FusedIterator;
use core::marker::PhantomData;

use crate::raw::{Probe, RawIter, RawTable, Slot};
use crate::TryReserveError;

/// A hash map: expected O(1) lookup, insertion, and removal, costing one
/// control byte and one `(K, V)` slot per bucket and no per-entry node.
///
/// # Choosing `S`
///
/// `S` is how the map hashes, and it is a security decision the map will not
/// make for you — there is deliberately no default construction. A map whose
/// keys an attacker can choose or influence is built over
/// [`BuildSipHash13::keyed`](tairix_hash::BuildSipHash13::keyed), which refuses
/// to hand out a hasher until the per-boot key exists: hashing such keys under
/// a predictable key lets an attacker pick a set that all collide and turns
/// every lookup into a linear scan. A map over keys the kernel assigns itself
/// may use the faster, unkeyed
/// [`BuildFastHash`](tairix_hash::BuildFastHash) — and the use site then says
/// so in plain sight.
///
/// # Iteration order
///
/// Unspecified. It varies with the hash key, the insertion history, and the
/// capacity, so anything whose output is compared, logged, paged, or
/// reproduced wants an ordered container instead.
///
/// # Allocation
///
/// Lookup, iteration, and removal never allocate. Every operation that can
/// allocate is fallible — [`try_insert`](Self::try_insert) and
/// [`try_reserve`](Self::try_reserve) return a [`TryReserveError`] — and there
/// is no `Index` implementation, because a subscript that panics on a missing
/// key has no place in a kernel.
///
/// # Secrets
///
/// The map does not scrub a slot it frees: reuse inside one address space is
/// not a security boundary. A holder of a key, credential, or capability token
/// stores a value type that zeroes itself on drop.
pub struct HashMap<K, V, S> {
    table: RawTable<(K, V)>,
    hasher: S,
}

impl<K, V, S> HashMap<K, V, S> {
    /// An empty map that has not allocated.
    #[must_use]
    pub const fn with_hasher(hasher: S) -> Self {
        Self {
            table: RawTable::new(),
            hasher,
        }
    }

    /// Live entries.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.table.len()
    }

    /// `true` if the map holds no entries.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.table.len() == 0
    }

    /// Entries the map can hold before it must grow.
    #[must_use]
    pub const fn capacity(&self) -> usize {
        self.table.capacity()
    }

    /// Bytes of heap the map holds — its resident footprint, for a caller that
    /// budgets memory.
    #[must_use]
    pub fn allocated_bytes(&self) -> usize {
        self.table.allocated_bytes()
    }

    /// The map's hasher.
    #[must_use]
    pub const fn hasher(&self) -> &S {
        &self.hasher
    }

    /// Drop every entry, keeping the allocation.
    pub fn clear(&mut self) {
        self.table.clear();
    }

    /// The entries, in unspecified order.
    pub fn iter(&self) -> Iter<'_, K, V> {
        Iter {
            raw: self.table.iter(),
            marker: PhantomData,
        }
    }

    /// The entries, in unspecified order, with mutable values.
    pub fn iter_mut(&mut self) -> IterMut<'_, K, V> {
        IterMut {
            raw: self.table.iter(),
            marker: PhantomData,
        }
    }

    /// The keys, in unspecified order.
    pub fn keys(&self) -> Keys<'_, K, V> {
        Keys { inner: self.iter() }
    }

    /// The values, in unspecified order.
    pub fn values(&self) -> Values<'_, K, V> {
        Values { inner: self.iter() }
    }

    /// The values, in unspecified order, mutably.
    pub fn values_mut(&mut self) -> ValuesMut<'_, K, V> {
        ValuesMut {
            inner: self.iter_mut(),
        }
    }

    /// Keep only the entries `keep` accepts.
    pub fn retain(&mut self, mut keep: impl FnMut(&K, &mut V) -> bool) {
        for index in 0..self.table.buckets() {
            // SAFETY: `index` is below the bucket count.
            if !unsafe { self.table.is_occupied(index) } {
                continue;
            }
            // SAFETY: the slot is occupied, and `&mut self` makes the borrow
            // unique.
            let (key, value) = unsafe { self.table.entry_mut(index) };
            if keep(key, value) {
                continue;
            }
            // Removing rewrites this entry's own control byte and nothing
            // else, so it cannot disturb a slot the walk has yet to reach.
            // SAFETY: the slot is occupied.
            drop(unsafe { self.table.take(index) });
        }
    }
}

impl<K, V, S> HashMap<K, V, S>
where
    K: Eq + Hash,
    S: BuildHasher,
{
    /// An empty map with room for `capacity` entries.
    ///
    /// # Errors
    ///
    /// [`TryReserveError`] when the allocation fails or the capacity cannot be
    /// represented.
    pub fn try_with_capacity_and_hasher(
        capacity: usize,
        hasher: S,
    ) -> Result<Self, TryReserveError> {
        let mut map = Self::with_hasher(hasher);
        map.try_reserve(capacity)?;
        Ok(map)
    }

    /// Make room for `additional` further entries.
    ///
    /// A caller that must not lose a value to an allocation failure reserves
    /// first; the insertion that follows cannot need to grow.
    ///
    /// # Errors
    ///
    /// [`TryReserveError`] when the allocation fails or the capacity cannot be
    /// represented. The map is left untouched.
    pub fn try_reserve(&mut self, additional: usize) -> Result<(), TryReserveError> {
        let hasher = &self.hasher;
        self.table
            .try_reserve(additional, |(key, _)| hasher.hash_one(key))
    }

    /// The value for `key`.
    pub fn get<Q>(&self, key: &Q) -> Option<&V>
    where
        K: Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        self.get_key_value(key).map(|(_, value)| value)
    }

    /// The stored key and its value.
    pub fn get_key_value<Q>(&self, key: &Q) -> Option<(&K, &V)>
    where
        K: Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        let index = self.find(key)?;
        // SAFETY: `find` reports an occupied slot of this table.
        let (found, value) = unsafe { self.table.entry(index) };
        Some((found, value))
    }

    /// The value for `key`, mutably.
    pub fn get_mut<Q>(&mut self, key: &Q) -> Option<&mut V>
    where
        K: Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        let index = self.find(key)?;
        // SAFETY: `find` reports an occupied slot of this table, and
        // `&mut self` makes the borrow unique.
        let (_, value) = unsafe { self.table.entry_mut(index) };
        Some(value)
    }

    /// `true` if `key` is present.
    pub fn contains_key<Q>(&self, key: &Q) -> bool
    where
        K: Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        self.find(key).is_some()
    }

    /// Insert `value` for `key`, returning the value it replaced.
    ///
    /// # Errors
    ///
    /// [`TryReserveError`] when the map must grow and the allocation fails.
    /// `key` and `value` are dropped in that case; a caller that must keep
    /// them calls [`try_reserve`](Self::try_reserve) first.
    pub fn try_insert(&mut self, key: K, value: V) -> Result<Option<V>, TryReserveError> {
        let hash = self.hasher.hash_one(&key);
        match self.table.probe(hash, |(found, _)| *found == key).slot {
            Some(Slot::Occupied(index)) => {
                // SAFETY: `probe` reports an occupied slot of this table.
                let slot = unsafe { self.table.entry_mut(index) };
                return Ok(Some(core::mem::replace(&mut slot.1, value)));
            }
            Some(Slot::Vacant(index)) if self.table.growth_left() > 0 => {
                // SAFETY: `probe` reports a vacant slot of this table, and the
                // table has growth left to spend on it.
                unsafe { self.table.fill(index, hash, (key, value)) };
                return Ok(None);
            }
            _ => {}
        }

        self.try_reserve(1)?;
        let Some(Slot::Vacant(index)) = self.table.probe(hash, |(found, _)| *found == key).slot
        else {
            // A reservation puts a free lane on every chain, so this cannot
            // happen; refusing the insertion is the fail-closed answer if it
            // ever does.
            debug_assert!(false, "a reserved table offered no vacancy");
            return Err(TryReserveError::AllocFailed);
        };
        // SAFETY: `probe` reports a vacant slot of this table, and the
        // reservation above left growth to spend on it.
        unsafe { self.table.fill(index, hash, (key, value)) };
        Ok(None)
    }

    /// Remove `key`, returning its value.
    pub fn remove<Q>(&mut self, key: &Q) -> Option<V>
    where
        K: Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        self.remove_entry(key).map(|(_, value)| value)
    }

    /// Remove `key`, returning it with its value.
    pub fn remove_entry<Q>(&mut self, key: &Q) -> Option<(K, V)>
    where
        K: Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        let index = self.find(key)?;
        // SAFETY: `find` reports an occupied slot of this table.
        Some(unsafe { self.table.take(index) })
    }

    /// Slot index of `key`, if it is live.
    fn find<Q>(&self, key: &Q) -> Option<usize>
    where
        K: Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        match self.probe(key).slot {
            Some(Slot::Occupied(index)) => Some(index),
            _ => None,
        }
    }

    /// Probe for `key`, hashing it through this map's hasher.
    fn probe<Q>(&self, key: &Q) -> Probe
    where
        K: Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        let hash = self.hasher.hash_one(key);
        self.table.probe(hash, |(found, _)| found.borrow() == key)
    }

    /// Control groups examined to find `key`, or `None` if it is absent.
    ///
    /// The map's deterministic work counter: probe depth is what a hash
    /// table's performance actually is, and unlike an elapsed time it is
    /// reproducible on any machine, so it — not a stopwatch — is what the
    /// crate's performance gates assert on and what a health report would
    /// read to spot a table that has degraded.
    pub fn probe_groups<Q>(&self, key: &Q) -> Option<usize>
    where
        K: Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        let probe = self.probe(key);
        match probe.slot {
            Some(Slot::Occupied(_)) => Some(probe.groups),
            _ => None,
        }
    }
}

impl<K: fmt::Debug, V: fmt::Debug, S> fmt::Debug for HashMap<K, V, S> {
    /// Renders the entries in the map's unspecified iteration order, so the
    /// text is a snapshot for a human, never something to compare.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_map().entries(self.iter()).finish()
    }
}

/// Shared iterator over a map's entries.
pub struct Iter<'a, K, V> {
    raw: RawIter<(K, V)>,
    marker: PhantomData<(&'a K, &'a V)>,
}

impl<'a, K, V> Iterator for Iter<'a, K, V> {
    type Item = (&'a K, &'a V);

    fn next(&mut self) -> Option<(&'a K, &'a V)> {
        let slot = self.raw.next()?;
        // SAFETY: the raw walk yields the address of a live entry of the table
        // this iterator borrows, which outlives `'a`.
        let (key, value) = unsafe { slot.as_ref() };
        Some((key, value))
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.raw.size_hint()
    }
}

impl<K, V> ExactSizeIterator for Iter<'_, K, V> {
    fn len(&self) -> usize {
        self.raw.len()
    }
}

impl<K, V> FusedIterator for Iter<'_, K, V> {}

/// Iterator over a map's entries with mutable values.
pub struct IterMut<'a, K, V> {
    raw: RawIter<(K, V)>,
    marker: PhantomData<(&'a K, &'a mut V)>,
}

impl<'a, K, V> Iterator for IterMut<'a, K, V> {
    type Item = (&'a K, &'a mut V);

    fn next(&mut self) -> Option<(&'a K, &'a mut V)> {
        let mut slot = self.raw.next()?;
        // SAFETY: the raw walk yields each live entry's address exactly once,
        // so no two items alias, and the table is uniquely borrowed for `'a`.
        let (key, value) = unsafe { slot.as_mut() };
        Some((&*key, value))
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.raw.size_hint()
    }
}

impl<K, V> ExactSizeIterator for IterMut<'_, K, V> {
    fn len(&self) -> usize {
        self.raw.len()
    }
}

impl<K, V> FusedIterator for IterMut<'_, K, V> {}

/// Owning iterator over a map's entries.
pub struct IntoIter<K, V, S> {
    map: HashMap<K, V, S>,
    index: usize,
}

impl<K, V, S> Iterator for IntoIter<K, V, S> {
    type Item = (K, V);

    fn next(&mut self) -> Option<(K, V)> {
        while self.index < self.map.table.buckets() {
            let index = self.index;
            self.index += 1;
            // SAFETY: `index` is below the bucket count.
            if unsafe { self.map.table.is_occupied(index) } {
                // SAFETY: the slot is occupied; taking it marks it free, so no
                // later step reads it again.
                return Some(unsafe { self.map.table.take(index) });
            }
        }
        None
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.map.len(), Some(self.map.len()))
    }
}

impl<K, V, S> ExactSizeIterator for IntoIter<K, V, S> {
    fn len(&self) -> usize {
        self.map.len()
    }
}

impl<K, V, S> FusedIterator for IntoIter<K, V, S> {}

impl<K, V, S> IntoIterator for HashMap<K, V, S> {
    type Item = (K, V);
    type IntoIter = IntoIter<K, V, S>;

    fn into_iter(self) -> IntoIter<K, V, S> {
        IntoIter {
            map: self,
            index: 0,
        }
    }
}

impl<'a, K, V, S> IntoIterator for &'a HashMap<K, V, S> {
    type Item = (&'a K, &'a V);
    type IntoIter = Iter<'a, K, V>;

    fn into_iter(self) -> Iter<'a, K, V> {
        self.iter()
    }
}

impl<'a, K, V, S> IntoIterator for &'a mut HashMap<K, V, S> {
    type Item = (&'a K, &'a mut V);
    type IntoIter = IterMut<'a, K, V>;

    fn into_iter(self) -> IterMut<'a, K, V> {
        self.iter_mut()
    }
}

/// Iterator over a map's keys.
pub struct Keys<'a, K, V> {
    inner: Iter<'a, K, V>,
}

impl<'a, K, V> Iterator for Keys<'a, K, V> {
    type Item = &'a K;

    fn next(&mut self) -> Option<&'a K> {
        self.inner.next().map(|(key, _)| key)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.inner.size_hint()
    }
}

impl<K, V> ExactSizeIterator for Keys<'_, K, V> {
    fn len(&self) -> usize {
        self.inner.len()
    }
}

impl<K, V> FusedIterator for Keys<'_, K, V> {}

/// Iterator over a map's values.
pub struct Values<'a, K, V> {
    inner: Iter<'a, K, V>,
}

impl<'a, K, V> Iterator for Values<'a, K, V> {
    type Item = &'a V;

    fn next(&mut self) -> Option<&'a V> {
        self.inner.next().map(|(_, value)| value)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.inner.size_hint()
    }
}

impl<K, V> ExactSizeIterator for Values<'_, K, V> {
    fn len(&self) -> usize {
        self.inner.len()
    }
}

impl<K, V> FusedIterator for Values<'_, K, V> {}

/// Iterator over a map's values, mutably.
pub struct ValuesMut<'a, K, V> {
    inner: IterMut<'a, K, V>,
}

impl<'a, K, V> Iterator for ValuesMut<'a, K, V> {
    type Item = &'a mut V;

    fn next(&mut self) -> Option<&'a mut V> {
        self.inner.next().map(|(_, value)| value)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.inner.size_hint()
    }
}

impl<K, V> ExactSizeIterator for ValuesMut<'_, K, V> {
    fn len(&self) -> usize {
        self.inner.len()
    }
}

impl<K, V> FusedIterator for ValuesMut<'_, K, V> {}
