//! [`LruMap`] — a keyed map whose recency order is maintained in constant
//! time.

use core::borrow::Borrow;
use core::fmt;
use core::hash::{BuildHasher, Hash};
use core::iter::{FusedIterator, Rev};

use alloc::vec::Vec;

use tairix_inline::intrusive::{self, IntrusiveList, Link, LinkStore};

use crate::raw::{RawTable, Slot};
use crate::TryReserveError;

/// One arena node: the entry, the hash it was filed under, and the links
/// whichever of the map's two lists currently holds it.
///
/// The hash is stored rather than recomputed because eviction reaches an
/// entry from the recency order rather than from a key, and a table rebuild
/// then moves entries without hashing any of them.
struct Node<K, V> {
    link: Link,
    hash: u64,
    entry: Option<(K, V)>,
}

/// The map's node arena, and the [`LinkStore`] both its lists thread through.
///
/// Nodes are addressed by index and the arena never shrinks while an entry is
/// live, so a growth that moves the backing allocation leaves every link and
/// every index the hash table holds valid.
struct Arena<K, V> {
    nodes: Vec<Node<K, V>>,
}

impl<K, V> Arena<K, V> {
    const fn new() -> Self {
        Self { nodes: Vec::new() }
    }

    fn len(&self) -> usize {
        self.nodes.len()
    }

    fn allocated_bytes(&self) -> usize {
        self.nodes.capacity() * size_of::<Node<K, V>>()
    }

    fn key(&self, node: usize) -> Option<&K> {
        self.nodes.get(node)?.entry.as_ref().map(|(key, _)| key)
    }

    fn value(&self, node: usize) -> Option<&V> {
        self.nodes.get(node)?.entry.as_ref().map(|(_, value)| value)
    }

    fn value_mut(&mut self, node: usize) -> Option<&mut V> {
        self.nodes
            .get_mut(node)?
            .entry
            .as_mut()
            .map(|(_, value)| value)
    }

    fn pair(&self, node: usize) -> Option<(&K, &V)> {
        let (key, value) = self.nodes.get(node)?.entry.as_ref()?;
        Some((key, value))
    }

    /// The hash a node's entry was filed under. A node the table names is
    /// always in range, so the absent arm is a fail-closed misfiling rather
    /// than a read of a node that is not there.
    fn hash(&self, node: usize) -> u64 {
        self.nodes.get(node).map_or(0, |node| node.hash)
    }

    /// Put an entry into a vacant node.
    fn occupy(&mut self, node: usize, hash: u64, key: K, value: V) {
        if let Some(node) = self.nodes.get_mut(node) {
            node.hash = hash;
            node.entry = Some((key, value));
        }
    }

    /// Take the entry out of a node, leaving it vacant.
    fn vacate(&mut self, node: usize) -> Option<(K, V)> {
        self.nodes.get_mut(node)?.entry.take()
    }

    /// Overwrite a live node's value, reporting the one it replaced.
    fn set_value(&mut self, node: usize, value: V) -> Option<V> {
        let held = self.value_mut(node)?;
        Some(core::mem::replace(held, value))
    }

    /// Append `extra` vacant nodes, reporting the index of each so the caller
    /// can file them.
    ///
    /// The reservation is exact and the amortising is the caller's, so the
    /// arena's footprint is what the map asked for rather than whatever
    /// headroom a speculative growth policy chose — which is what lets a
    /// budget-holder read [`LruMap::allocated_bytes`] as a real figure.
    ///
    /// # Errors
    ///
    /// [`TryReserveError::AllocFailed`] when the arena cannot grow. Nothing is
    /// appended in that case.
    fn try_grow(&mut self, extra: usize) -> Result<core::ops::Range<usize>, TryReserveError> {
        self.nodes
            .try_reserve_exact(extra)
            .map_err(|_| TryReserveError::AllocFailed)?;
        let first = self.nodes.len();
        for _ in 0..extra {
            self.nodes.push(Node {
                link: Link::UNLINKED,
                hash: 0,
                entry: None,
            });
        }
        Ok(first..self.nodes.len())
    }
}

impl<K, V> LinkStore for Arena<K, V> {
    fn link(&self, index: usize) -> Option<&Link> {
        self.nodes.get(index).map(|node| &node.link)
    }

    fn link_mut(&mut self, index: usize) -> Option<&mut Link> {
        self.nodes.get_mut(index).map(|node| &mut node.link)
    }
}

/// A map that also remembers the order its entries were last used in, so the
/// least-recently-used one is found and dropped without a search.
///
/// A cache is the shape this exists for: it holds what fits and drops what has
/// gone coldest, and both halves of that must be constant time or the cache
/// costs more than the misses it saves. Lookup, insertion, the recency touch a
/// hit performs, and eviction are each expected O(1) — a hash probe over
/// control-byte groups (the same table [`HashMap`](crate::HashMap) is built
/// on) plus a splice of three links in an index-addressed intrusive list. A
/// recency index keyed by a monotonic tick, which is what every hand-rolled
/// cache in the tree reached for, is O(log n) on all three.
///
/// # Choosing `S`
///
/// As [`HashMap`](crate::HashMap): there is no default hasher, because a map
/// over keys an attacker can choose or influence must be built over
/// [`BuildSipHash13::keyed`](tairix_hash::BuildSipHash13::keyed), and a
/// defaulted hasher is one nothing forces the use site to think about.
///
/// # Recency
///
/// [`get`](Self::get) and [`get_mut`](Self::get_mut) are uses and refresh the
/// entry; [`peek`](Self::peek) and the iterators are observations and do not.
/// [`try_insert`](Self::try_insert) files a new or replaced entry as the most
/// recently used. Nothing here evicts on its own: a caller bounds the map by
/// its own budget — entries, bytes, or a pressure band — and calls
/// [`pop_lru`](Self::pop_lru) until it is met, which is what lets one map serve
/// a fixed-entry index and a byte-budgeted cache alike.
///
/// # Allocation
///
/// Lookup, the touch, iteration, removal, and eviction allocate nothing: an
/// evicted node returns to a free list and the next insertion reuses it, so a
/// map at its steady state never returns to the allocator. Growth is fallible
/// ([`try_insert`](Self::try_insert), [`try_reserve`](Self::try_reserve)) and
/// [`clear`](Self::clear) releases both allocations, because a cache drained
/// under memory pressure has to give its memory back.
///
/// # Secrets
///
/// The map does not scrub a node it frees: reuse inside one address space is
/// not a security boundary. A holder of a key, credential, or capability token
/// stores a value type that zeroes itself on drop.
///
/// # Detected corruption
///
/// Every link the map splices names a node from its own arena, so a refused
/// splice means the map's own bookkeeping has been corrupted by something
/// outside it. Such a refusal is fail-closed — the operation reports nothing
/// found or nothing inserted rather than proceeding on links it does not
/// trust — and is a debug assertion, since no input can reach it.
pub struct LruMap<K, V, S> {
    arena: Arena<K, V>,
    /// Key index: each live bucket holds the arena node its entry lives in, so
    /// a key is stored once however many indexes reach it.
    index: RawTable<usize>,
    /// Live nodes, most recently used at the front.
    recency: IntrusiveList,
    /// Vacant nodes awaiting reuse.
    free: IntrusiveList,
    hasher: S,
}

impl<K, V, S> LruMap<K, V, S> {
    /// An empty map that has not allocated.
    #[must_use]
    pub const fn with_hasher(hasher: S) -> Self {
        Self {
            arena: Arena::new(),
            index: RawTable::new(),
            recency: IntrusiveList::new(),
            free: IntrusiveList::new(),
            hasher,
        }
    }

    /// Live entries.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.recency.len()
    }

    /// `true` if the map holds no entries.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.recency.is_empty()
    }

    /// Entries the map can hold before it must grow.
    #[must_use]
    pub fn capacity(&self) -> usize {
        self.index.capacity().min(self.arena.len())
    }

    /// Bytes of heap the map holds — its resident footprint, for a caller that
    /// budgets memory.
    #[must_use]
    pub fn allocated_bytes(&self) -> usize {
        self.index.allocated_bytes() + self.arena.allocated_bytes()
    }

    /// The map's hasher.
    #[must_use]
    pub const fn hasher(&self) -> &S {
        &self.hasher
    }

    /// Drop every entry and release both allocations.
    ///
    /// A truncate that kept the capacity would be the wrong default here: the
    /// callers that clear a whole map are caches being drained by memory
    /// pressure or by a generation change, and holding their peak footprint
    /// afterwards is the memory the drain was for.
    pub fn clear(&mut self) {
        self.arena = Arena::new();
        self.index = RawTable::new();
        self.recency = IntrusiveList::new();
        self.free = IntrusiveList::new();
    }

    /// The least-recently-used entry, without refreshing it.
    #[must_use]
    pub fn peek_lru(&self) -> Option<(&K, &V)> {
        self.arena.pair(self.recency.back()?)
    }

    /// The most-recently-used entry, without refreshing it.
    #[must_use]
    pub fn peek_mru(&self) -> Option<(&K, &V)> {
        self.arena.pair(self.recency.front()?)
    }

    /// The entries from least to most recently used, refreshing none of them.
    ///
    /// This is the order eviction follows, so it is what a caller reads to
    /// choose a victim by more than age, and what a diagnostic reports.
    /// [`Iterator::rev`] walks it the other way.
    pub fn iter_lru(&self) -> IterLru<'_, K, V> {
        IterLru {
            nodes: self.recency.iter(&self.arena).rev(),
            arena: &self.arena,
        }
    }

    /// Move a live node to the front of the recency order.
    fn promote(&mut self, node: usize) {
        if self.recency.move_to_front(&mut self.arena, node).is_err() {
            debug_assert!(false, "a live entry is on the recency list");
        }
    }

    /// Take a live node's entry, unlink it, and return the node for reuse.
    fn detach(&mut self, node: usize) -> Option<(K, V)> {
        let entry = self.arena.vacate(node);
        if self.recency.unlink(&mut self.arena, node).is_err() {
            debug_assert!(false, "a live entry is on the recency list");
            return entry;
        }
        if self.free.push_front(&mut self.arena, node).is_err() {
            debug_assert!(false, "an unlinked node joins the free list");
        }
        entry
    }
}

impl<K, V, S> LruMap<K, V, S>
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

    /// Make room for `additional` further entries, in both the key index and
    /// the node arena.
    ///
    /// A caller that must not lose a value to an allocation failure reserves
    /// first; the insertion that follows cannot need to grow.
    ///
    /// # Errors
    ///
    /// [`TryReserveError`] when an allocation fails or the capacity cannot be
    /// represented. The map is left untouched.
    pub fn try_reserve(&mut self, additional: usize) -> Result<(), TryReserveError> {
        let arena = &self.arena;
        self.index
            .try_reserve(additional, |&node| arena.hash(node))?;
        let Some(extra) = additional.checked_sub(self.free.len()) else {
            return Ok(());
        };
        if extra == 0 {
            return Ok(());
        }
        // Double, or take the whole ask when it is larger: growing by exactly
        // one node per insertion would make a run of them quadratic.
        let grow = extra.max(self.arena.len());
        for node in self.arena.try_grow(grow)? {
            if self.free.push_front(&mut self.arena, node).is_err() {
                debug_assert!(false, "a fresh node joins the free list");
                return Err(TryReserveError::AllocFailed);
            }
        }
        Ok(())
    }

    /// The value for `key`, refreshing its recency.
    pub fn get<Q>(&mut self, key: &Q) -> Option<&V>
    where
        K: Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        let node = self.locate(key)?.1;
        self.promote(node);
        self.arena.value(node)
    }

    /// The value for `key`, mutably, refreshing its recency.
    pub fn get_mut<Q>(&mut self, key: &Q) -> Option<&mut V>
    where
        K: Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        let node = self.locate(key)?.1;
        self.promote(node);
        self.arena.value_mut(node)
    }

    /// The value for `key` without refreshing its recency — an observation
    /// rather than a use.
    pub fn peek<Q>(&self, key: &Q) -> Option<&V>
    where
        K: Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        self.arena.value(self.locate(key)?.1)
    }

    /// The value for `key`, mutably, without refreshing its recency.
    ///
    /// For an update that is not this holder's own use of the entry — a
    /// neighbour cache folding in a peer's unsolicited reply, a counter the
    /// entry carries for someone else — where refreshing recency would let
    /// another party decide what this holder evicts.
    pub fn peek_mut<Q>(&mut self, key: &Q) -> Option<&mut V>
    where
        K: Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        let node = self.locate(key)?.1;
        self.arena.value_mut(node)
    }

    /// `true` if `key` is present, without refreshing its recency.
    pub fn contains_key<Q>(&self, key: &Q) -> bool
    where
        K: Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        self.locate(key).is_some()
    }

    /// Insert `value` for `key` as the most recently used entry, returning the
    /// value it replaced.
    ///
    /// A replacement keeps the stored key and refreshes the entry's recency.
    /// Nothing is evicted to make room: the map grows, and the caller drops
    /// what its own budget will not hold.
    ///
    /// # Errors
    ///
    /// [`TryReserveError`] when the map must grow and an allocation fails.
    /// `key` and `value` are dropped in that case; a caller that must keep
    /// them calls [`try_reserve`](Self::try_reserve) first.
    pub fn try_insert(&mut self, key: K, value: V) -> Result<Option<V>, TryReserveError> {
        let hash = self.hasher.hash_one(&key);
        if let Some((_, node)) = self.probe_for(hash, &key) {
            let previous = self.arena.set_value(node, value);
            self.promote(node);
            return Ok(previous);
        }

        // Both indexes grow before anything is written, so a refusal leaves
        // the map exactly as it was.
        self.try_reserve(1)?;
        let Some(Slot::Vacant(bucket)) = self.index.probe(hash, |_| false).slot else {
            // A reservation puts a free lane on every chain, so this cannot
            // happen; refusing the insertion is the fail-closed answer if it
            // ever does.
            debug_assert!(false, "a reserved table offered no vacancy");
            return Err(TryReserveError::AllocFailed);
        };
        let Some(node) = self.free.pop_front(&mut self.arena) else {
            debug_assert!(false, "a reservation left a free node");
            return Err(TryReserveError::AllocFailed);
        };
        if self.recency.push_front(&mut self.arena, node).is_err() {
            debug_assert!(false, "a free node joins the recency list");
            if self.free.push_front(&mut self.arena, node).is_err() {
                debug_assert!(false, "an unlinked node rejoins the free list");
            }
            return Err(TryReserveError::AllocFailed);
        }
        self.arena.occupy(node, hash, key, value);
        // SAFETY: `probe` reports a vacant bucket of this table, and the
        // reservation above left growth to spend on it.
        unsafe { self.index.fill(bucket, hash, node) };
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
        let (bucket, node) = self.locate(key)?;
        // SAFETY: `locate` reports an occupied bucket of this table.
        unsafe { self.index.take(bucket) };
        self.detach(node)
    }

    /// Drop the least-recently-used entry and return it — the eviction the
    /// caller's budget drives.
    pub fn pop_lru(&mut self) -> Option<(K, V)> {
        let node = self.recency.back()?;
        let hash = self.arena.hash(node);
        // The stored node index identifies the bucket outright, so eviction
        // costs no key hash and no key comparison.
        let Some(Slot::Occupied(bucket)) = self.index.probe(hash, |&held| held == node).slot else {
            debug_assert!(false, "a live entry has a bucket");
            return None;
        };
        // SAFETY: `probe` reports an occupied bucket of this table.
        unsafe { self.index.take(bucket) };
        self.detach(node)
    }

    /// Keep only the entries `keep` accepts, leaving the recency order of the
    /// survivors unchanged.
    pub fn retain(&mut self, mut keep: impl FnMut(&K, &mut V) -> bool) {
        for node in 0..self.arena.len() {
            let Some(entry) = self
                .arena
                .nodes
                .get_mut(node)
                .and_then(|node| node.entry.as_mut())
            else {
                continue;
            };
            if keep(&entry.0, &mut entry.1) {
                continue;
            }
            let hash = self.arena.hash(node);
            let Some(Slot::Occupied(bucket)) = self.index.probe(hash, |&held| held == node).slot
            else {
                debug_assert!(false, "a live entry has a bucket");
                continue;
            };
            // SAFETY: `probe` reports an occupied bucket of this table.
            unsafe { self.index.take(bucket) };
            self.detach(node);
        }
    }

    /// Control groups examined to find `key`, or `None` if it is absent.
    ///
    /// The map's deterministic work counter, as
    /// [`HashMap::probe_groups`](crate::HashMap::probe_groups): probe depth is
    /// reproducible on any machine where an elapsed time is not, so it is what
    /// the crate's performance gates assert on.
    pub fn probe_groups<Q>(&self, key: &Q) -> Option<usize>
    where
        K: Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        let hash = self.hasher.hash_one(key);
        let arena = &self.arena;
        let probe = self
            .index
            .probe(hash, |&node| Self::holds(arena, node, key));
        match probe.slot {
            Some(Slot::Occupied(_)) => Some(probe.groups),
            _ => None,
        }
    }

    /// The bucket and arena node holding `key`, if it is live.
    fn locate<Q>(&self, key: &Q) -> Option<(usize, usize)>
    where
        K: Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        self.probe_for(self.hasher.hash_one(key), key)
    }

    /// [`Self::locate`] against a hash the caller already has.
    fn probe_for<Q>(&self, hash: u64, key: &Q) -> Option<(usize, usize)>
    where
        K: Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        let arena = &self.arena;
        match self
            .index
            .probe(hash, |&node| Self::holds(arena, node, key))
            .slot
        {
            // SAFETY: `probe` reports an occupied bucket of this table.
            Some(Slot::Occupied(bucket)) => Some((bucket, unsafe { *self.index.entry(bucket) })),
            _ => None,
        }
    }

    /// Whether the arena node holds `key`. A node the index names is always
    /// live, so a vacant one answers no rather than matching by accident.
    fn holds<Q>(arena: &Arena<K, V>, node: usize, key: &Q) -> bool
    where
        K: Borrow<Q>,
        Q: Eq + ?Sized,
    {
        arena.key(node).is_some_and(|found| found.borrow() == key)
    }
}

impl<K: fmt::Debug, V: fmt::Debug, S> fmt::Debug for LruMap<K, V, S> {
    /// Renders the entries least-recently-used first, which is the order
    /// eviction takes them in.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_map().entries(self.iter_lru()).finish()
    }
}

/// A walk over a map's entries from least to most recently used.
pub struct IterLru<'a, K, V> {
    nodes: Rev<intrusive::Iter<'a, Arena<K, V>>>,
    arena: &'a Arena<K, V>,
}

impl<'a, K, V> Iterator for IterLru<'a, K, V> {
    type Item = (&'a K, &'a V);

    fn next(&mut self) -> Option<(&'a K, &'a V)> {
        loop {
            let node = self.nodes.next()?;
            if let Some(pair) = self.arena.pair(node) {
                return Some(pair);
            }
            debug_assert!(false, "the recency list holds only live entries");
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.nodes.size_hint()
    }
}

impl<'a, K, V> DoubleEndedIterator for IterLru<'a, K, V> {
    fn next_back(&mut self) -> Option<(&'a K, &'a V)> {
        loop {
            let node = self.nodes.next_back()?;
            if let Some(pair) = self.arena.pair(node) {
                return Some(pair);
            }
            debug_assert!(false, "the recency list holds only live entries");
        }
    }
}

impl<K, V> FusedIterator for IterLru<'_, K, V> {}

#[cfg(test)]
#[path = "lru_tests.rs"]
mod tests;
