//! Clean, rebuildable filesystem cache (`plans/SMARTRAM.md` section 6.1).
//!
//! [`CachedFs`] wraps a mounted volume's filesystem driver **below** the
//! VFS policy layer: every permission check (capability gate, ACL, mode
//! bits, mount flags) still runs in the secured VFS on every operation,
//! so a cache hit can never bypass authorisation — the cache only spares
//! the driver a repeated structural read of bytes the caller was just
//! authorised to see.
//!
//! # What is cached
//!
//! * File **data**, in page-sized chunks ([`ReclaimClass::CleanFileData`]).
//! * **Metadata** ([`ReclaimClass::FsMetadata`]): stat records
//!   (`node_info`), security records (`security`), name resolution
//!   (`lookup`), and directory entries (`read_dir`).
//!
//! Only *clean* state is cached: writes go straight to the driver
//! (write-through) and invalidate what they touch, so the cache never
//! holds dirty data and dropping any entry is always safe.
//!
//! # Coherence: one volume, one writer
//!
//! Every mutation of the volume flows through this wrapper — the `fs_*`
//! syscalls and the account-administration engine share the single
//! registered driver instance behind one `SleepLock`
//! (`LateFilesystem::register`). There is no second window onto the
//! device, so precise invalidation here is complete: `write_at` /
//! `truncate` drop the file's data and stat, `create` / `remove` /
//! `rename` drop the affected lookups, directory entries, and directory
//! stats, and `set_security` drops the node's security record. When a
//! mutation's target cannot be identified (an unexpected driver error
//! while resolving it), the **whole cache is purged** — fail closed,
//! never a stale entry.
//!
//! # Classification, bounds, eviction, and accounting
//!
//! At construction the cache declares its two [`CacheCandidate`]s —
//! clean file data and filesystem metadata, owned by the wrapped
//! volume, holding decrypted user data, precisely invalidated by the
//! volume's single writer, droppable on demand, with bounded per-entry
//! bookkeeping — and classifies them through the `kernel/mem::reclaim`
//! admission gate. A refusal starts the cache poisoned: every
//! operation is served straight from the driver (fail closed, never an
//! unclassified cache).
//!
//! The cache is bounded by a [`CacheBudget`] derived from the kernel
//! heap size and accounted per class in a [`CacheAccounting`] ledger
//! (`kernel/mem::reclaim`). An insert that would exceed the hard limit
//! first evicts least-recently-used entries down to the low watermark
//! (hysteresis), evicting file data before metadata
//! ([`ReclaimClass::reclaim_priority`]). Oversized entries (a name over
//! the component bound, a read larger than the bypass limit) are refused
//! or bypassed, never admitted unbounded. Every payload buffer the cache
//! copies is allocated fallibly (`try_reserve`): allocation failure
//! refuses the entry and the operation is served straight from the
//! driver. The remaining map-node allocations are small, fixed-size, and
//! bounded by the budget's entry-overhead charge.
//!
//! # Secret hygiene
//!
//! The volumes this wraps are encrypted at rest, so cached file bytes
//! and names are decrypted user data: every buffer is zeroed before its
//! entry is released — on invalidation, eviction, purge, and teardown —
//! so reclaim never leaves plaintext in reusable heap memory.
//!
//! # Concurrency
//!
//! `CachedFs` lives inside the per-mount `SleepLock`, so every operation
//! holds `&mut self`: lookup racing reclaim, invalidation racing
//! rebuild, and teardown racing reclaim are impossible by construction
//! rather than by locking discipline.

use alloc::collections::BTreeMap;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::mem::size_of;

use tairix_abi::driver::filesystem::{
    DirEntry, FilesystemAttrs, FilesystemAttrsFs, FilesystemAttrsProvider, FilesystemRead,
    FilesystemSecurity, FilesystemStats, FilesystemWrite, NodeId, NodeInfo, NodeKind, NodeSecurity,
    VolumeStats,
};
use tairix_abi::DriverError;
use tairix_kernel_mem::{
    log_cache_poisoned, log_cache_refused, shrink_target, CacheAccounting, CacheBudget,
    CacheCandidate, CachePolicy, InvalidationSource, MemoryPressure, RebuildCost, ReclaimClass,
    ReclaimOwner, ReclaimRule, Sensitivity, PAGE_SIZE,
};
use tairix_log::Sink;
use zeroize::Zeroize;

use super::path::MAX_COMPONENT_LEN;

/// A cached file-data chunk covers exactly one page-aligned window.
const CHUNK: usize = PAGE_SIZE;

/// Reads larger than this bypass the cache entirely: a bulk sequential
/// read (a bundle load) is served in one driver call and must not evict
/// the hot small-read working set.
const READ_BYPASS_LIMIT: usize = 4 * CHUNK;

/// Approximate per-entry bookkeeping cost (map nodes, key copies, the
/// LRU index) charged on top of an entry's payload so the ledger tracks
/// real heap footprint, not just payload bytes.
const ENTRY_OVERHEAD: usize = 96;

/// Which cache pool a key lives in, for the LRU index.
#[derive(Clone, Debug, Eq, PartialEq)]
enum KeyRef {
    /// `stat` pool: node id.
    Stat(u64),
    /// `sec` pool: node id.
    Sec(u64),
    /// `lookup` pool: (directory id, child name).
    Lookup(u64, Vec<u8>),
    /// `dirent` pool: (directory id, cursor).
    Dirent(u64, u64),
    /// `data` pool: (file id, chunk base offset).
    Data(u64, u64),
}

/// A cached `node_info` record.
struct StatEntry {
    info: NodeInfo,
    tick: u64,
}

/// A cached `security` record.
struct SecEntry {
    sec: NodeSecurity,
    tick: u64,
}

/// A cached positive `lookup` result.
struct LookupEntry {
    node: u64,
    tick: u64,
}

/// A cached `read_dir` entry: the fixed record plus its name bytes.
struct DirentEntry {
    entry: DirEntry,
    name: Vec<u8>,
    tick: u64,
}

/// A cached file-data chunk. `bytes.len() < CHUNK` marks end-of-file at
/// `base + bytes.len()` — authoritative because every write to the
/// volume invalidates the file's chunks before the next read.
struct DataEntry {
    bytes: Vec<u8>,
    tick: u64,
}

/// The clean, rebuildable filesystem cache wrapping one volume's driver.
///
/// See the module docs for the design; construct with [`CachedFs::new`]
/// at driver registration time.
pub struct CachedFs<F> {
    inner: F,
    budget: CacheBudget,
    /// The system memory-pressure gauge, sampled at the head of every
    /// operation: the band's forced-shrink targets are applied before
    /// the cache is read or grown, and admission is refused outside
    /// normal pressure or when growth would dip into the reserve.
    pressure: &'static MemoryPressure,
    /// The audit sink a classification refusal or detected ledger
    /// defect reports through (`kernel/mem::reclaim_audit`).
    sink: &'static (dyn Sink + Sync),
    accounting: Arc<CacheAccounting>,
    /// The classified admission policies (file data, metadata); `None`
    /// when classification refused, which poisons the cache from birth.
    policies: Option<(CachePolicy, CachePolicy)>,
    /// Monotonic recency counter; every touch assigns a fresh tick, so
    /// ticks are unique and the LRU maps are keyed by them.
    tick: u64,
    /// Books no longer balance (a ledger defect was detected): the
    /// cache has been purged and admits nothing further — every
    /// operation is served straight from the driver (fail closed).
    poisoned: bool,
    stat: BTreeMap<u64, StatEntry>,
    sec: BTreeMap<u64, SecEntry>,
    /// Positive lookup results, nested per directory so a hit borrows
    /// the queried name instead of allocating a tuple key.
    lookup: BTreeMap<u64, BTreeMap<Vec<u8>, LookupEntry>>,
    dirent: BTreeMap<(u64, u64), DirentEntry>,
    data: BTreeMap<(u64, u64), DataEntry>,
    /// LRU index of the data pool, keyed by tick (oldest first).
    lru_data: BTreeMap<u64, KeyRef>,
    /// LRU index of the metadata pools, keyed by tick (oldest first).
    lru_meta: BTreeMap<u64, KeyRef>,
}

/// The fixed `cache` label this cache's audit records carry.
const CACHE_LABEL: &str = "clean_fs";

impl<F> CachedFs<F> {
    /// The cache's declared candidates: clean file data and filesystem
    /// metadata for the volume `owner`, both decrypted user data (the
    /// volumes are encrypted at rest), both precisely invalidated by
    /// the volume's single writer, both droppable on demand. The
    /// metadata pool's worst-case per-entry bookkeeping carries a name
    /// component copy on top of the fixed overhead.
    fn candidates(owner: ReclaimOwner) -> (CacheCandidate, CacheCandidate) {
        let data = CacheCandidate {
            class: Some(ReclaimClass::CleanFileData),
            owner: Some(owner),
            rebuild_cost: RebuildCost::Cheap,
            sensitivity: Some(Sensitivity::UserData),
            invalidation: Some(InvalidationSource::SourceMutation),
            rule: Some(ReclaimRule::Drop),
            entry_metadata_bytes: ENTRY_OVERHEAD,
        };
        let metadata = CacheCandidate {
            class: Some(ReclaimClass::FsMetadata),
            rebuild_cost: RebuildCost::Moderate,
            entry_metadata_bytes: ENTRY_OVERHEAD + MAX_COMPONENT_LEN,
            ..data
        };
        (data, metadata)
    }

    /// Wrap `inner` with an empty cache bounded by `budget`, charged to
    /// the volume `owner` and governed by the system `pressure` gauge.
    ///
    /// Both candidate declarations pass the `kernel/mem::reclaim`
    /// classification gate; a refusal poisons the cache from birth, so
    /// every operation is served straight from the driver — fail
    /// closed, the volume still works.
    #[must_use]
    pub fn new(
        inner: F,
        budget: CacheBudget,
        owner: ReclaimOwner,
        pressure: &'static MemoryPressure,
        sink: &'static (dyn Sink + Sync),
    ) -> Self {
        let (data, metadata) = Self::candidates(owner);
        let policies = match (data.classify(), metadata.classify()) {
            (Ok(data), Ok(metadata)) => Some((data, metadata)),
            (data, metadata) => {
                for refusal in [data.err(), metadata.err()].into_iter().flatten() {
                    log_cache_refused(sink, CACHE_LABEL, Some(owner), refusal);
                }
                None
            }
        };
        Self {
            inner,
            budget,
            pressure,
            sink,
            accounting: Arc::new(CacheAccounting::new()),
            policies,
            tick: 0,
            poisoned: policies.is_none(),
            stat: BTreeMap::new(),
            sec: BTreeMap::new(),
            lookup: BTreeMap::new(),
            dirent: BTreeMap::new(),
            data: BTreeMap::new(),
            lru_data: BTreeMap::new(),
            lru_meta: BTreeMap::new(),
        }
    }

    /// The cache's byte ledger and event counters.
    #[must_use]
    pub fn accounting(&self) -> &CacheAccounting {
        &self.accounting
    }

    /// A shared handle to this cache's ledger, for registration with the
    /// System Information memory-statistics registry. Observation-only:
    /// the holder gets lock-free reads of the same saturating
    /// diagnostics this cache keeps.
    #[must_use]
    pub fn accounting_shared(&self) -> Arc<CacheAccounting> {
        Arc::clone(&self.accounting)
    }

    /// The cache's grow/shrink bounds.
    #[must_use]
    pub fn budget(&self) -> CacheBudget {
        self.budget
    }

    /// The owner the cache's memory is charged to, or `None` when
    /// classification refused the cache (it is then poisoned and
    /// admits nothing).
    #[must_use]
    pub fn owner(&self) -> Option<ReclaimOwner> {
        self.policies.map(|(data, _)| data.owner())
    }

    /// The wrapped driver, for the host tests' call counting.
    #[cfg(test)]
    pub(crate) fn inner_driver(&self) -> &F {
        &self.inner
    }
}

impl<F> CachedFs<F> {
    /// The next unique recency tick.
    fn next_tick(&mut self) -> u64 {
        self.tick += 1;
        self.tick
    }

    /// Copy `bytes` into a fresh exact-capacity buffer, fallibly: an
    /// allocation failure yields `None` and the caller refuses the
    /// entry instead of aborting on heap exhaustion.
    fn try_copy(bytes: &[u8]) -> Option<Vec<u8>> {
        let mut out = Vec::new();
        out.try_reserve_exact(bytes.len()).ok()?;
        out.extend_from_slice(bytes);
        Some(out)
    }

    /// The ledger class an LRU key belongs to.
    fn class_of(key: &KeyRef) -> ReclaimClass {
        match key {
            KeyRef::Data(..) => ReclaimClass::CleanFileData,
            _ => ReclaimClass::FsMetadata,
        }
    }

    /// The accounted `(payload, metadata)` byte cost of an LRU key's
    /// entry: the cached content, and the per-entry bookkeeping (the
    /// fixed overhead plus any key-copy bytes) on top of it.
    fn cost_of(&self, key: &KeyRef) -> (usize, usize) {
        match key {
            KeyRef::Stat(_) => (size_of::<NodeInfo>(), ENTRY_OVERHEAD),
            KeyRef::Sec(_) => (size_of::<NodeSecurity>(), ENTRY_OVERHEAD),
            // The cached content is the resolved node id; the name is
            // carried twice as bookkeeping (map key and LRU key copy).
            KeyRef::Lookup(_, name) => (
                8,
                ENTRY_OVERHEAD.saturating_add(name.len().saturating_mul(2)),
            ),
            KeyRef::Dirent(dir, cursor) => (
                self.dirent
                    .get(&(*dir, *cursor))
                    .map_or(0, |e| e.name.len().saturating_add(size_of::<DirEntry>())),
                ENTRY_OVERHEAD,
            ),
            KeyRef::Data(file, base) => (
                self.data.get(&(*file, *base)).map_or(0, |e| e.bytes.len()),
                ENTRY_OVERHEAD,
            ),
        }
    }

    /// Remove the entry `key` names, zeroing its buffers, dropping its
    /// LRU index slot, and discharging its cost. Returns the removed
    /// entry's tick, or `None` when it was already gone.
    fn remove_entry(&mut self, key: &KeyRef) -> Option<u64> {
        let (payload, metadata) = self.cost_of(key);
        let tick = match key {
            KeyRef::Stat(node) => self.stat.remove(node).map(|e| e.tick),
            KeyRef::Sec(node) => self.sec.remove(node).map(|e| e.tick),
            KeyRef::Lookup(dir, name) => {
                let removed = self.lookup.get_mut(dir).and_then(|names| {
                    names
                        .remove_entry(name.as_slice())
                        .map(|(mut stored_name, entry)| {
                            stored_name.as_mut_slice().zeroize();
                            entry.tick
                        })
                });
                if self.lookup.get(dir).is_some_and(BTreeMap::is_empty) {
                    self.lookup.remove(dir);
                }
                removed
            }
            KeyRef::Dirent(dir, cursor) => self.dirent.remove(&(*dir, *cursor)).map(|mut e| {
                e.name.as_mut_slice().zeroize();
                e.tick
            }),
            KeyRef::Data(file, base) => self.data.remove(&(*file, *base)).map(|mut e| {
                e.bytes.as_mut_slice().zeroize();
                e.tick
            }),
        }?;
        self.lru_data.remove(&tick);
        self.lru_meta.remove(&tick);
        if self
            .accounting
            .discharge(Self::class_of(key), payload, metadata)
            .is_err()
        {
            self.poison("ledger_imbalance");
        }
        Some(tick)
    }

    /// Drop every cached entry (zeroed) and admit nothing further: the
    /// fail-closed response to the internal defect named by `cause`.
    /// The driver keeps serving every operation; only the cache is
    /// disabled. The defect is counted and reported once through the
    /// audit sink; a cache already poisoned (including from birth)
    /// does not report again.
    fn poison(&mut self, cause: &'static str) {
        if !self.poisoned {
            // The poison disables the whole cache, so the failure hits
            // both classes it serves.
            self.accounting.record_failure(ReclaimClass::CleanFileData);
            self.accounting.record_failure(ReclaimClass::FsMetadata);
            log_cache_poisoned(self.sink, CACHE_LABEL, self.owner(), cause);
        }
        self.poisoned = true;
        self.purge();
    }

    /// Drop every cached entry, zeroing all buffers and rebalancing the
    /// ledger to empty. Every whole-cache drain is counted as a
    /// teardown.
    fn purge(&mut self) {
        // A whole-cache drain hits both classes this cache serves.
        self.accounting.record_teardown(ReclaimClass::CleanFileData);
        self.accounting.record_teardown(ReclaimClass::FsMetadata);
        for entry in self.data.values_mut() {
            entry.bytes.as_mut_slice().zeroize();
        }
        for entry in self.dirent.values_mut() {
            entry.name.as_mut_slice().zeroize();
        }
        while let Some((_, mut names)) = self.lookup.pop_first() {
            while let Some((mut name, _)) = names.pop_first() {
                name.as_mut_slice().zeroize();
            }
        }
        self.stat.clear();
        self.sec.clear();
        self.dirent.clear();
        self.data.clear();
        self.lru_data.clear();
        self.lru_meta.clear();
        self.accounting.zero_ledger();
    }

    /// Evict least-recently-used entries until the ledger total is at
    /// most `target`, taking file data before metadata.
    fn evict_until(&mut self, target: usize) {
        while self.accounting.total_bytes() > target {
            let key = match self.lru_data.first_key_value() {
                Some((_, key)) => key.clone(),
                None => match self.lru_meta.first_key_value() {
                    Some((_, key)) => key.clone(),
                    None => return,
                },
            };
            if self.remove_entry(&key).is_none() {
                // An index entry with no backing entry is a ledger
                // defect; fail closed rather than loop.
                self.poison("orphan_index_slot");
                return;
            }
            self.accounting.record_eviction();
        }
    }

    /// Apply the current pressure band's forced-shrink targets, called
    /// at the head of every cache-touching operation before the cache
    /// is read or grown (`plans/SMARTRAM.md` section 7). Eviction takes file data
    /// before metadata, so the combined ceiling — resident metadata
    /// capped at its own class target plus file data capped at its
    /// own — shrinks each class exactly to its band target: at mild
    /// pressure clean file data drops to the low watermark, at moderate
    /// it drains fully while metadata is capped at the low watermark, and
    /// at severe or critical pressure everything goes. Every evicted
    /// buffer is zeroed on the way out, exactly as ordinary eviction.
    fn enforce_pressure(&mut self) {
        if self.poisoned {
            return;
        }
        let band = self.pressure.sample();
        let data_target = shrink_target(band, ReclaimClass::CleanFileData, self.budget);
        let meta_target = shrink_target(band, ReclaimClass::FsMetadata, self.budget);
        let data_bytes = self.accounting.class_bytes(ReclaimClass::CleanFileData);
        let meta_bytes = self.accounting.class_bytes(ReclaimClass::FsMetadata);
        let target = meta_bytes
            .min(meta_target)
            .saturating_add(data_bytes.min(data_target));
        if self.accounting.total_bytes() > target {
            // Attribute the pass to each class whose footprint exceeds
            // its own band target — the classes the shrink will hit.
            if data_bytes > data_target {
                self.accounting
                    .record_pressure_shrink(ReclaimClass::CleanFileData);
            }
            if meta_bytes > meta_target {
                self.accounting
                    .record_pressure_shrink(ReclaimClass::FsMetadata);
            }
            self.evict_until(target);
        }
    }

    /// Admit an entry of `payload` cached-content bytes plus `metadata`
    /// bookkeeping bytes under `key`, evicting to make room. Returns
    /// the recency tick to store in the entry, or `None` when the entry
    /// is refused (over budget, poisoned, growth is forbidden by the
    /// pressure band or would dip into the reserve, or the ledger
    /// cannot account it) — the caller then serves without caching.
    fn admit(&mut self, key: KeyRef, payload: usize, metadata: usize) -> Option<u64> {
        let class = Self::class_of(&key);
        let cost = payload.saturating_add(metadata);
        if self.poisoned || cost > self.budget.hard() {
            self.accounting.record_refusal(class);
            return None;
        }
        if !self.pressure.growth_permitted(cost) {
            self.accounting.record_refusal(class);
            return None;
        }
        if self.accounting.total_bytes().saturating_add(cost) > self.budget.hard() {
            let headroom = self.budget.low().min(self.budget.hard() - cost);
            self.evict_until(headroom);
            if self.poisoned {
                self.accounting.record_refusal(class);
                return None;
            }
        }
        if self.accounting.charge(class, payload, metadata).is_err() {
            self.accounting.record_refusal(class);
            return None;
        }
        let tick = self.next_tick();
        match class {
            ReclaimClass::CleanFileData => self.lru_data.insert(tick, key),
            _ => self.lru_meta.insert(tick, key),
        };
        Some(tick)
    }

    /// Refresh `key`'s recency: move its LRU slot from `old_tick` to a
    /// fresh tick, returning the new tick for the entry to store.
    fn touch(&mut self, old_tick: u64) -> u64 {
        let tick = self.next_tick();
        if let Some(key) = self.lru_data.remove(&old_tick) {
            self.lru_data.insert(tick, key);
        } else if let Some(key) = self.lru_meta.remove(&old_tick) {
            self.lru_meta.insert(tick, key);
        }
        tick
    }

    /// The byte offset of `pos` within its containing chunk. The
    /// remainder of a division by [`CHUNK`] always fits `usize`.
    #[allow(clippy::cast_possible_truncation)]
    fn offset_in_chunk(pos: u64, base: u64) -> usize {
        (pos - base) as usize
    }
}

impl<F: FilesystemRead> CachedFs<F> {
    /// Resolve the node `dir/name` currently names, for invalidation,
    /// preferring the cache over a driver read.
    ///
    /// `Ok(None)` when no such child exists; `Err(())` when the driver
    /// failed unexpectedly — the caller must then purge the whole cache
    /// rather than leave a possibly-affected entry standing.
    fn resolve_for_invalidation(&mut self, dir: NodeId, name: &[u8]) -> Result<Option<u64>, ()> {
        if let Some(entry) = self
            .lookup
            .get(&dir.raw())
            .and_then(|names| names.get(name))
        {
            return Ok(Some(entry.node));
        }
        match self.inner.lookup(dir, name) {
            Ok(node) => Ok(Some(node.raw())),
            Err(DriverError::NotFound) => Ok(None),
            Err(_) => Err(()),
        }
    }

    /// Drop the cached lookup for `dir/name`, if present.
    fn invalidate_lookup(&mut self, dir: u64, name: &[u8]) {
        // The key copy could not be allocated: the entry cannot be
        // addressed individually, so fail closed on the whole cache.
        let Some(name) = Self::try_copy(name) else {
            self.purge();
            return;
        };
        if self.remove_entry(&KeyRef::Lookup(dir, name)).is_some() {
            self.accounting.record_invalidation();
        }
    }

    /// Drop every cached lookup under `dir`.
    ///
    /// Used when a mutation changes the directory's name bindings
    /// (`create` / `remove` / `rename`): name matching policy belongs to
    /// the driver and may fold case, so an exact-byte removal could
    /// leave a differently-spelled alias of the same binding standing —
    /// the whole directory's lookups go instead (fail closed).
    fn invalidate_lookups(&mut self, dir: u64) {
        loop {
            let Some(key) = self
                .lookup
                .get(&dir)
                .and_then(|names| names.first_key_value())
                .map(|(name, _)| KeyRef::Lookup(dir, name.clone()))
            else {
                return;
            };
            if self.remove_entry(&key).is_some() {
                self.accounting.record_invalidation();
            }
        }
    }

    /// Drop the cached stat record for `node`, if present.
    fn invalidate_stat(&mut self, node: u64) {
        if self.remove_entry(&KeyRef::Stat(node)).is_some() {
            self.accounting.record_invalidation();
        }
    }

    /// Drop the cached security record for `node`, if present.
    fn invalidate_sec(&mut self, node: u64) {
        if self.remove_entry(&KeyRef::Sec(node)).is_some() {
            self.accounting.record_invalidation();
        }
    }

    /// Drop every cached data chunk of `node`.
    fn invalidate_data(&mut self, node: u64) {
        loop {
            let Some(key) = self
                .data
                .range((node, 0)..=(node, u64::MAX))
                .next()
                .map(|((file, base), _)| KeyRef::Data(*file, *base))
            else {
                return;
            };
            if self.remove_entry(&key).is_some() {
                self.accounting.record_invalidation();
            }
        }
    }

    /// Drop every cached directory entry of `dir` — a mutation makes
    /// every retained cursor's remainder unspecified, and each entry
    /// embeds a child's metadata that may just have changed.
    fn invalidate_dirents(&mut self, dir: u64) {
        loop {
            let Some(key) = self
                .dirent
                .range((dir, 0)..=(dir, u64::MAX))
                .next()
                .map(|((d, cursor), _)| KeyRef::Dirent(*d, *cursor))
            else {
                return;
            };
            if self.remove_entry(&key).is_some() {
                self.accounting.record_invalidation();
            }
        }
    }

    /// Drop everything cached about `node`: stat, security, and data.
    fn invalidate_node(&mut self, node: u64) {
        self.invalidate_stat(node);
        self.invalidate_sec(node);
        self.invalidate_data(node);
    }

    /// Serve one chunk-aligned slice of a file read: copy into `out`
    /// from the cached chunk at `base`, fetching and admitting the
    /// chunk on a miss. Returns `(bytes copied, chunk length)`; a chunk
    /// shorter than [`CHUNK`] marks end-of-file within it.
    fn chunk_read(
        &mut self,
        file: NodeId,
        base: u64,
        in_off: usize,
        out: &mut [u8],
    ) -> Result<(usize, usize), DriverError> {
        let raw = file.raw();
        if let Some(entry) = self.data.get(&(raw, base)) {
            let len = entry.bytes.len();
            let n = out.len().min(len.saturating_sub(in_off));
            if n > 0 {
                out[..n].copy_from_slice(&entry.bytes[in_off..in_off + n]);
            }
            let old_tick = entry.tick;
            let tick = self.touch(old_tick);
            if let Some(entry) = self.data.get_mut(&(raw, base)) {
                entry.tick = tick;
            }
            self.accounting.record_hit(ReclaimClass::CleanFileData);
            return Ok((n, len));
        }
        self.accounting.record_miss(ReclaimClass::CleanFileData);
        let Some(mut chunk) = Self::try_zeroed(CHUNK) else {
            // No memory for a chunk buffer: serve the caller's slice
            // straight from the driver without caching. A short read
            // here means EOF within this window, reported as a short
            // chunk so the outer loop stops.
            self.accounting.record_refusal(ReclaimClass::CleanFileData);
            let n = self.inner.read_at(file, base + in_off as u64, out)?;
            let len = if n < out.len() { in_off + n } else { CHUNK };
            return Ok((n, len));
        };
        let read = self.inner.read_at(file, base, chunk.as_mut_slice())?;
        chunk[read..].zeroize();
        chunk.truncate(read);
        let n = out.len().min(read.saturating_sub(in_off));
        if n > 0 {
            out[..n].copy_from_slice(&chunk[in_off..in_off + n]);
        }
        match self.admit(KeyRef::Data(raw, base), read, ENTRY_OVERHEAD) {
            Some(tick) => {
                self.data
                    .insert((raw, base), DataEntry { bytes: chunk, tick });
            }
            None => chunk.as_mut_slice().zeroize(),
        }
        Ok((n, read))
    }

    /// A zeroed buffer of `len` bytes, or `None` on allocation failure.
    fn try_zeroed(len: usize) -> Option<Vec<u8>> {
        let mut out = Vec::new();
        out.try_reserve_exact(len).ok()?;
        out.resize(len, 0);
        Some(out)
    }
}

impl<F: FilesystemRead> FilesystemRead for CachedFs<F> {
    fn root(&self) -> NodeId {
        self.inner.root()
    }

    fn node_info(&mut self, node: NodeId) -> Result<NodeInfo, DriverError> {
        self.enforce_pressure();
        let raw = node.raw();
        if let Some(entry) = self.stat.get(&raw) {
            let info = entry.info;
            let old_tick = entry.tick;
            let tick = self.touch(old_tick);
            if let Some(entry) = self.stat.get_mut(&raw) {
                entry.tick = tick;
            }
            self.accounting.record_hit(ReclaimClass::FsMetadata);
            return Ok(info);
        }
        self.accounting.record_miss(ReclaimClass::FsMetadata);
        let info = self.inner.node_info(node)?;
        if let Some(tick) = self.admit(KeyRef::Stat(raw), size_of::<NodeInfo>(), ENTRY_OVERHEAD) {
            self.stat.insert(raw, StatEntry { info, tick });
        }
        Ok(info)
    }

    fn lookup(&mut self, dir: NodeId, name: &[u8]) -> Result<NodeId, DriverError> {
        self.enforce_pressure();
        let dir_raw = dir.raw();
        if let Some(entry) = self.lookup.get(&dir_raw).and_then(|names| names.get(name)) {
            let node = entry.node;
            let old_tick = entry.tick;
            let tick = self.touch(old_tick);
            if let Some(entry) = self
                .lookup
                .get_mut(&dir_raw)
                .and_then(|names| names.get_mut(name))
            {
                entry.tick = tick;
            }
            self.accounting.record_hit(ReclaimClass::FsMetadata);
            return Ok(NodeId::from_raw(node));
        }
        self.accounting.record_miss(ReclaimClass::FsMetadata);
        let node = self.inner.lookup(dir, name)?;
        // A name over the VFS component bound is unbounded input from
        // the cache's point of view and is served uncached.
        if name.len() > MAX_COMPONENT_LEN {
            self.accounting.record_refusal(ReclaimClass::FsMetadata);
            return Ok(node);
        }
        let (Some(key_name), Some(entry_name)) = (Self::try_copy(name), Self::try_copy(name))
        else {
            self.accounting.record_refusal(ReclaimClass::FsMetadata);
            return Ok(node);
        };
        let metadata = ENTRY_OVERHEAD.saturating_add(name.len().saturating_mul(2));
        if let Some(tick) = self.admit(KeyRef::Lookup(dir_raw, key_name), 8, metadata) {
            self.lookup.entry(dir_raw).or_default().insert(
                entry_name,
                LookupEntry {
                    node: node.raw(),
                    tick,
                },
            );
        } else {
            let mut entry_name = entry_name;
            entry_name.as_mut_slice().zeroize();
        }
        Ok(node)
    }

    fn read_at(&mut self, file: NodeId, offset: u64, buf: &mut [u8]) -> Result<usize, DriverError> {
        self.enforce_pressure();
        if buf.is_empty() || buf.len() > READ_BYPASS_LIMIT || self.poisoned {
            return self.inner.read_at(file, offset, buf);
        }
        let chunk_len = CHUNK as u64;
        let mut total = 0usize;
        while total < buf.len() {
            let Some(pos) = offset.checked_add(total as u64) else {
                break;
            };
            let base = pos - (pos % chunk_len);
            let in_off = Self::offset_in_chunk(pos, base);
            let want = (buf.len() - total).min(CHUNK - in_off);
            let (copied, len) =
                self.chunk_read(file, base, in_off, &mut buf[total..total + want])?;
            total += copied;
            if len < CHUNK || copied < want {
                break;
            }
        }
        Ok(total)
    }

    fn read_dir(
        &mut self,
        dir: NodeId,
        cursor: u64,
        name_out: &mut [u8],
    ) -> Result<Option<DirEntry>, DriverError> {
        self.enforce_pressure();
        let dir_raw = dir.raw();
        if let Some(cached) = self.dirent.get(&(dir_raw, cursor)) {
            // The contract's refusal for an undersized buffer is served
            // from the cached name length exactly as the driver would.
            if name_out.len() < cached.name.len() {
                self.accounting.record_hit(ReclaimClass::FsMetadata);
                return Err(DriverError::BufferTooSmall);
            }
            let mut entry = cached.entry;
            entry.name_len = cached.name.len();
            name_out[..cached.name.len()].copy_from_slice(&cached.name);
            let old_tick = cached.tick;
            let tick = self.touch(old_tick);
            if let Some(cached) = self.dirent.get_mut(&(dir_raw, cursor)) {
                cached.tick = tick;
            }
            self.accounting.record_hit(ReclaimClass::FsMetadata);
            return Ok(Some(entry));
        }
        self.accounting.record_miss(ReclaimClass::FsMetadata);
        let Some(entry) = self.inner.read_dir(dir, cursor, name_out)? else {
            return Ok(None);
        };
        if entry.name_len <= MAX_COMPONENT_LEN && entry.name_len <= name_out.len() {
            if let Some(name) = Self::try_copy(&name_out[..entry.name_len]) {
                let payload = name.len().saturating_add(size_of::<DirEntry>());
                if let Some(tick) =
                    self.admit(KeyRef::Dirent(dir_raw, cursor), payload, ENTRY_OVERHEAD)
                {
                    self.dirent
                        .insert((dir_raw, cursor), DirentEntry { entry, name, tick });
                } else {
                    let mut name = name;
                    name.as_mut_slice().zeroize();
                }
            } else {
                self.accounting.record_refusal(ReclaimClass::FsMetadata);
            }
        } else {
            self.accounting.record_refusal(ReclaimClass::FsMetadata);
        }
        // The entry carries the child's stat record; populate the stat
        // cache so a follow-up `node_info` is a hit.
        let child = entry.node.raw();
        if !self.stat.contains_key(&child) {
            if let Some(tick) =
                self.admit(KeyRef::Stat(child), size_of::<NodeInfo>(), ENTRY_OVERHEAD)
            {
                self.stat.insert(
                    child,
                    StatEntry {
                        info: entry.info,
                        tick,
                    },
                );
            }
        }
        Ok(Some(entry))
    }
}

impl<F: FilesystemRead + FilesystemWrite> FilesystemWrite for CachedFs<F> {
    fn create(&mut self, dir: NodeId, name: &[u8], kind: NodeKind) -> Result<NodeId, DriverError> {
        self.enforce_pressure();
        let result = self.inner.create(dir, name, kind);
        // Invalidate whether or not the driver succeeded: a partially
        // applied refusal on a foreign driver must not leave stale
        // entries standing (ARXFS rolls back, but the cache does not
        // assume it). Name bindings changed, so the directory's whole
        // lookup set goes (driver name matching may fold case).
        let dir_raw = dir.raw();
        self.invalidate_lookups(dir_raw);
        self.invalidate_dirents(dir_raw);
        self.invalidate_stat(dir_raw);
        result
    }

    fn write_at(
        &mut self,
        dir: NodeId,
        name: &[u8],
        offset: u64,
        data: &[u8],
    ) -> Result<usize, DriverError> {
        self.enforce_pressure();
        let target = self.resolve_for_invalidation(dir, name);
        let result = self.inner.write_at(dir, name, offset, data);
        match target {
            Ok(Some(node)) => {
                self.invalidate_stat(node);
                self.invalidate_data(node);
                self.invalidate_dirents(dir.raw());
            }
            Ok(None) => {
                self.invalidate_lookup(dir.raw(), name);
                self.invalidate_dirents(dir.raw());
            }
            Err(()) => self.purge(),
        }
        result
    }

    fn truncate(&mut self, dir: NodeId, name: &[u8], size: u64) -> Result<(), DriverError> {
        self.enforce_pressure();
        let target = self.resolve_for_invalidation(dir, name);
        let result = self.inner.truncate(dir, name, size);
        match target {
            Ok(Some(node)) => {
                self.invalidate_stat(node);
                self.invalidate_data(node);
                self.invalidate_dirents(dir.raw());
            }
            Ok(None) => {
                self.invalidate_lookup(dir.raw(), name);
                self.invalidate_dirents(dir.raw());
            }
            Err(()) => self.purge(),
        }
        result
    }

    fn remove(&mut self, dir: NodeId, name: &[u8]) -> Result<(), DriverError> {
        self.enforce_pressure();
        let target = self.resolve_for_invalidation(dir, name);
        let result = self.inner.remove(dir, name);
        let dir_raw = dir.raw();
        match target {
            Ok(Some(node)) => {
                self.invalidate_lookups(dir_raw);
                self.invalidate_node(node);
                self.invalidate_dirents(dir_raw);
                self.invalidate_stat(dir_raw);
            }
            Ok(None) => {
                self.invalidate_lookups(dir_raw);
                self.invalidate_dirents(dir_raw);
            }
            Err(()) => self.purge(),
        }
        result
    }

    fn rename(
        &mut self,
        src_dir: NodeId,
        src_name: &[u8],
        dst_dir: NodeId,
        dst_name: &[u8],
    ) -> Result<(), DriverError> {
        self.enforce_pressure();
        let overwritten = self.resolve_for_invalidation(dst_dir, dst_name);
        let result = self.inner.rename(src_dir, src_name, dst_dir, dst_name);
        let src_raw = src_dir.raw();
        let dst_raw = dst_dir.raw();
        match overwritten {
            Ok(Some(node)) => self.invalidate_node(node),
            Ok(None) => {}
            Err(()) => {
                self.purge();
                return result;
            }
        }
        // The moved node keeps its identity (its stat, security, and
        // data stay valid); only the name bindings and both directories'
        // listings change.
        self.invalidate_lookups(src_raw);
        self.invalidate_lookups(dst_raw);
        self.invalidate_dirents(src_raw);
        self.invalidate_dirents(dst_raw);
        self.invalidate_stat(src_raw);
        self.invalidate_stat(dst_raw);
        result
    }

    fn flush(&mut self) -> Result<(), DriverError> {
        // Write-through: the cache holds no dirty state to flush.
        self.inner.flush()
    }
}

impl<F: FilesystemRead + FilesystemSecurity> FilesystemSecurity for CachedFs<F> {
    fn security(&mut self, node: NodeId) -> Result<NodeSecurity, DriverError> {
        self.enforce_pressure();
        let raw = node.raw();
        if let Some(entry) = self.sec.get(&raw) {
            let sec = entry.sec;
            let old_tick = entry.tick;
            let tick = self.touch(old_tick);
            if let Some(entry) = self.sec.get_mut(&raw) {
                entry.tick = tick;
            }
            self.accounting.record_hit(ReclaimClass::FsMetadata);
            return Ok(sec);
        }
        self.accounting.record_miss(ReclaimClass::FsMetadata);
        let sec = self.inner.security(node)?;
        if let Some(tick) = self.admit(KeyRef::Sec(raw), size_of::<NodeSecurity>(), ENTRY_OVERHEAD)
        {
            self.sec.insert(raw, SecEntry { sec, tick });
        }
        Ok(sec)
    }

    fn set_security(&mut self, node: NodeId, security: NodeSecurity) -> Result<(), DriverError> {
        self.enforce_pressure();
        let result = self.inner.set_security(node, security);
        // Invalidate on success and failure alike; the next `security`
        // re-reads the stored record.
        self.invalidate_sec(node.raw());
        result
    }
}

impl<F: FilesystemStats> FilesystemStats for CachedFs<F> {
    fn stats(&mut self) -> Result<VolumeStats, DriverError> {
        // Volume accounting is live driver state, never cached.
        self.inner.stats()
    }
}

/// Attribute values are never cached (they are rare, opaque reads), but the
/// calls still route *through* the cache wrapper rather than around it: a
/// mutation may grow or shrink the inode's attribute storage, so the node's
/// cached [`NodeInfo`] is invalidated exactly as a data write's is — a
/// bypass would leave a stale `allocated` behind.
impl<F> FilesystemAttrs for CachedFs<F>
where
    F: FilesystemRead + FilesystemSecurity + FilesystemAttrsProvider,
{
    fn get_attr(
        &mut self,
        node: NodeId,
        key: &[u8],
        value_out: &mut [u8],
    ) -> Result<Option<usize>, DriverError> {
        // Reachable only through `attrs_fs`, which answers `None` when the
        // inner driver stores no attributes; the guard here keeps the
        // failure closed for a caller that ignores the facet.
        let Some(inner) = self.inner.attrs_fs() else {
            return Err(DriverError::Unsupported);
        };
        inner.get_attr(node, key, value_out)
    }

    fn set_attr(&mut self, node: NodeId, key: &[u8], value: &[u8]) -> Result<(), DriverError> {
        let result = match self.inner.attrs_fs() {
            Some(inner) => inner.set_attr(node, key, value),
            None => return Err(DriverError::Unsupported),
        };
        // Invalidate on success and failure alike; the next `node_info`
        // re-reads the stored record (attribute blocks count against the
        // inode's allocation).
        self.invalidate_stat(node.raw());
        result
    }

    fn list_attr(
        &mut self,
        node: NodeId,
        index: u64,
        key_out: &mut [u8],
    ) -> Result<Option<usize>, DriverError> {
        let Some(inner) = self.inner.attrs_fs() else {
            return Err(DriverError::Unsupported);
        };
        inner.list_attr(node, index, key_out)
    }

    fn remove_attr(&mut self, node: NodeId, key: &[u8]) -> Result<(), DriverError> {
        let result = match self.inner.attrs_fs() {
            Some(inner) => inner.remove_attr(node, key),
            None => return Err(DriverError::Unsupported),
        };
        self.invalidate_stat(node.raw());
        result
    }
}

impl<F> FilesystemAttrsProvider for CachedFs<F>
where
    F: FilesystemRead + FilesystemSecurity + FilesystemAttrsProvider,
{
    fn attrs_fs(&mut self) -> Option<&mut dyn FilesystemAttrsFs> {
        // Support is the wrapped driver's fact; the cache adds none. When
        // the inner driver provides attributes the returned view is the
        // cache itself, so resolution reads stay cached and mutations
        // invalidate what they touch.
        if self.inner.attrs_fs().is_some() {
            Some(self)
        } else {
            None
        }
    }
}

#[cfg(test)]
#[path = "fscache_tests.rs"]
mod tests;

impl<F> Drop for CachedFs<F> {
    /// Teardown zeroes every cached buffer: the entries hold decrypted
    /// file bytes and names, which must not outlive their owner in
    /// reusable heap memory.
    fn drop(&mut self) {
        for entry in self.data.values_mut() {
            entry.bytes.as_mut_slice().zeroize();
        }
        for entry in self.dirent.values_mut() {
            entry.name.as_mut_slice().zeroize();
        }
        // Lookup keys carry name bytes; BTreeMap keys are immutable in
        // place, so drain the maps and zeroize each key as it comes out.
        while let Some((_, mut names)) = self.lookup.pop_first() {
            while let Some((mut name, _)) = names.pop_first() {
                name.as_mut_slice().zeroize();
            }
        }
    }
}
