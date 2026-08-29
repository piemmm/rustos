//! The `ARXFS` transform cache: reclaimable decompressed-cluster
//! plaintext (`plans/SMARTRAM.md` SMART3, section 6.2).
//!
//! [`TransformClusterCache`] is the production implementation of the
//! [`ClusterCache`] seam the `ARXFS` driver exposes: it retains the
//! verified, decrypted, decompressed plaintext of compressed clusters
//! between reads, so the driver pays the transform pipeline (per-block
//! AEAD + integrity checks, then a whole-frame decompression) once per
//! cluster instead of once per read. It complements the clean file
//! cache (`kernel/core::fs::CachedFs`, `plans/SMARTRAM.md` section 6.1)
//! rather than duplicating it: `CachedFs` retains page-sized chunks of
//! *served* plaintext for small reads, while this cache sits below the
//! driver's own read path and also covers the large sequential reads
//! (bundle and driver-store loads) that bypass `CachedFs` entirely and
//! `CachedFs`'s misses, both of which otherwise re-run the whole
//! transform per call.
//!
//! # Classification, budget, pressure
//!
//! At construction the cache declares its [`CacheCandidate`] — class
//! [`ReclaimClass::TransformCache`], owned by the mounted volume,
//! expensive to rebuild, decrypted user data, precisely invalidated by
//! the volume's single writer, droppable on demand — and classifies it
//! through the `kernel/mem::reclaim` admission gate. A refusal starts
//! the cache poisoned: every lookup misses, every offer is refused, and
//! the driver keeps serving through the full pipeline (fail closed).
//!
//! The cache is bounded by a [`CacheBudget`] and accounted in a
//! [`CacheAccounting`] ledger. Every operation first applies the
//! current pressure band's forced-shrink target for the transform-cache
//! class ([`shrink_target`]): the class is preserved at mild pressure
//! and drained to zero from moderate pressure on, before anonymous
//! pages are handed to `ramzip` (`plans/SWAPSWAPSWAP.md` section 6).
//! Growth is admitted only at normal pressure and never into the
//! reserve ([`tairix_reclaim::PressureGauge::growth_permitted`]). Inserts
//! over the
//! hard limit first evict least-recently-used entries down to the low
//! watermark (hysteresis).
//!
//! # Coherence and secret hygiene
//!
//! Entries are keyed by the cluster's first stored physical block; the
//! driver invalidates on every block free and purges on transaction
//! rollback (the `tairix_drv_fs_arxfs::ClusterCache` contract), so an
//! entry can never outlive the bytes it was derived from. The plaintext
//! is decrypted user data from an encrypted-at-rest volume: every
//! buffer is volatilely wiped before its entry is released — on
//! invalidation, eviction, purge, replacement, and teardown.

use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::sync::Arc;
use alloc::vec::Vec;

use tairix_drv_fs_arxfs::{ClusterCache, MAX_CLUSTER_PLAINTEXT};
use tairix_kernel_core::{CacheClass, CacheControl, CACHE_CONTROL};
use tairix_log::Sink;
use tairix_reclaim::{
    log_cache_poisoned, log_cache_refused, shrink_target, CacheAccounting, CacheBudget,
    CacheCandidate, CacheLedger, CachePolicy, InvalidationSource, MemoryPressure, RebuildCost,
    ReclaimClass, ReclaimOwner, ReclaimRule, Sensitivity, MAP_ENTRY_OVERHEAD,
};
use zeroize::Zeroize;

/// The fixed `cache` label this cache's audit records carry.
const CACHE_LABEL: &str = "transform";

/// One retained cluster: its stored-run length in physical blocks (for
/// run-covering invalidation), its plaintext, and its recency tick.
struct Entry {
    stored: u64,
    plain: Vec<u8>,
    tick: u64,
}

/// The reclaimable decompressed-cluster cache one mounted volume
/// installs into its `ARXFS` driver. See the module docs.
pub struct TransformClusterCache {
    budget: CacheBudget,
    /// The live cache-admission control (the operator's `cache.transform`
    /// / `cache.all` switch). Sampled at the head of every operation: when
    /// the transform class is disabled the cache admits nothing and drops
    /// (wiping) what it holds, a real bypass. Defaults to the process-
    /// global [`CACHE_CONTROL`]; a test binds its own.
    control: &'static CacheControl,
    /// The system memory-pressure gauge, sampled at the head of every
    /// operation: the band's forced-shrink target is applied before the
    /// cache is read or grown, and admission is refused outside normal
    /// pressure or when growth would dip into the reserve.
    pressure: &'static MemoryPressure,
    /// The audit sink a classification refusal or detected ledger
    /// defect reports through (`kernel/mem::reclaim_audit`).
    sink: &'static (dyn Sink + Sync),
    accounting: Arc<CacheAccounting>,
    /// The classified admission policy; `None` when classification
    /// refused, which poisons the cache from birth.
    policy: Option<CachePolicy>,
    /// The cache admits nothing further (a classification refusal or a
    /// detected ledger defect): every lookup misses and the driver
    /// serves through the full transform pipeline (fail closed).
    poisoned: bool,
    /// Monotonic recency counter; every touch assigns a fresh tick, so
    /// ticks are unique and the LRU index is keyed by them.
    tick: u64,
    /// Retained clusters, keyed by the run's first stored block.
    entries: BTreeMap<u64, Entry>,
    /// LRU index: tick (oldest first) to entry key.
    lru: BTreeMap<u64, u64>,
}

impl TransformClusterCache {
    /// Build a cache bounded by `budget`, charged to `owner`, and
    /// governed by the system `pressure` gauge.
    ///
    /// The candidate declaration passes the `kernel/mem::reclaim`
    /// classification gate; a refusal poisons the cache from birth, so
    /// the volume still works and every read runs the full pipeline —
    /// fail closed, never an unclassified cache.
    #[must_use]
    pub fn new(
        budget: CacheBudget,
        owner: ReclaimOwner,
        pressure: &'static MemoryPressure,
        sink: &'static (dyn Sink + Sync),
    ) -> Self {
        let candidate = CacheCandidate {
            class: Some(ReclaimClass::TransformCache),
            owner: Some(owner),
            rebuild_cost: RebuildCost::Expensive,
            sensitivity: Some(Sensitivity::UserData),
            invalidation: Some(InvalidationSource::SourceMutation),
            rule: Some(ReclaimRule::Drop),
            entry_metadata_bytes: MAP_ENTRY_OVERHEAD,
        };
        let policy = match candidate.classify() {
            Ok(policy) => Some(policy),
            Err(refusal) => {
                log_cache_refused(sink, CACHE_LABEL, Some(owner), refusal);
                None
            }
        };
        Self {
            budget,
            control: &CACHE_CONTROL,
            pressure,
            sink,
            accounting: Arc::new(CacheAccounting::new()),
            poisoned: policy.is_none(),
            policy,
            tick: 0,
            entries: BTreeMap::new(),
            lru: BTreeMap::new(),
        }
    }

    /// Bind a specific [`CacheControl`] instead of the process-global
    /// [`CACHE_CONTROL`] this cache consults by default. Production uses
    /// the shared global the unlock path applies the operator's
    /// configuration to; a host test drives the disable path against its
    /// own control through this builder.
    #[must_use]
    pub fn with_cache_control(mut self, control: &'static CacheControl) -> Self {
        self.control = control;
        self
    }

    /// The boxed cache a mounted volume installs into its driver,
    /// budgeted from the kernel heap arena exactly like the volume's
    /// clean filesystem cache and charged to the volume identified by
    /// its stable per-boot mount handle.
    #[must_use]
    pub fn for_volume(
        volume: u64,
        pressure: &'static MemoryPressure,
        sink: &'static (dyn Sink + Sync),
    ) -> Box<dyn ClusterCache> {
        let cache = Self::new(
            // Budget from discovered physical RAM (the growable kernel heap's
            // bootstrap size is no longer the memory to size a cache against);
            // falls back to the bootstrap size before RAM is published.
            CacheBudget::from_backing(tairix_kernel_core::memstats::cache_backing_bytes()),
            ReclaimOwner::FilesystemVolume { volume },
            pressure,
            sink,
        );
        // Every production transform cache registers its ledger with the
        // System Information memory-statistics registry (observation-only).
        // A `None` means classification refused the cache at birth (it is
        // then poisoned and admits nothing), so there is nothing to
        // register — the refusal is already in the audit log.
        if let Some(ledger) = cache.ledger() {
            tairix_kernel_core::memstats::MEM_STATS.register_ledger(ledger);
        }
        Box::new(cache)
    }

    /// The cache's byte ledger and event counters.
    #[must_use]
    pub fn accounting(&self) -> &CacheAccounting {
        &self.accounting
    }

    /// The owner the cache's memory is charged to, or `None` when
    /// classification refused the cache (it is then poisoned and admits
    /// nothing).
    #[must_use]
    pub fn owner(&self) -> Option<ReclaimOwner> {
        self.policy.map(CachePolicy::owner)
    }

    /// This cache described for the System Information memory-statistics
    /// registry: its label, owner, and class, plus a shared handle to the
    /// ledger above.
    ///
    /// `None` when classification refused the cache (it is then poisoned
    /// and admits nothing, so there is no footprint to attribute — the
    /// refusal is already in the audit log with its reason).
    #[must_use]
    pub fn ledger(&self) -> Option<CacheLedger> {
        let policy = self.policy?;
        Some(CacheLedger::new(
            CACHE_LABEL,
            policy.owner(),
            policy.class(),
            Arc::clone(&self.accounting),
        ))
    }

    /// The next unique recency tick.
    fn next_tick(&mut self) -> u64 {
        self.tick += 1;
        self.tick
    }

    /// The accounted `(payload, metadata)` byte cost of an entry
    /// holding `payload` plaintext bytes plus the fixed per-entry
    /// bookkeeping.
    const fn cost_of(payload: usize) -> (usize, usize) {
        (payload, MAP_ENTRY_OVERHEAD)
    }

    /// Volatilely wipe a plaintext buffer: decrypted user data must not
    /// linger in reusable heap memory once its entry is released.
    fn wipe(plain: &mut Vec<u8>) {
        plain.as_mut_slice().zeroize();
    }

    /// Remove (and wipe) the entry keyed by `start`, dropping its LRU
    /// slot and discharging its cost. `false` when no such entry
    /// exists.
    fn remove_start(&mut self, start: u64) -> bool {
        let Some(mut entry) = self.entries.remove(&start) else {
            return false;
        };
        Self::wipe(&mut entry.plain);
        self.lru.remove(&entry.tick);
        let (payload, metadata) = Self::cost_of(entry.plain.len());
        if self
            .accounting
            .discharge(ReclaimClass::TransformCache, payload, metadata)
            .is_err()
        {
            self.poison("ledger_imbalance");
        }
        true
    }

    /// Drop every entry (wiped) and admit nothing further: the
    /// fail-closed response to the internal defect named by `cause`.
    /// The driver keeps serving; only the cache is disabled. The
    /// defect is counted and reported once through the audit sink.
    fn poison(&mut self, cause: &'static str) {
        if !self.poisoned {
            self.accounting.record_failure(ReclaimClass::TransformCache);
            log_cache_poisoned(self.sink, CACHE_LABEL, self.owner(), cause);
        }
        self.poisoned = true;
        self.drop_all();
    }

    /// Drop every entry, wiping all plaintext and rebalancing the
    /// ledger to empty. Every whole-cache drain is counted as a
    /// teardown.
    fn drop_all(&mut self) {
        self.accounting
            .record_teardown(ReclaimClass::TransformCache);
        for entry in self.entries.values_mut() {
            Self::wipe(&mut entry.plain);
        }
        self.entries.clear();
        self.lru.clear();
        self.accounting.zero_ledger();
    }

    /// Evict least-recently-used entries until the ledger total is at
    /// most `target`.
    fn evict_until(&mut self, target: usize) {
        while self.accounting.total_bytes() > target {
            let Some((_, &start)) = self.lru.first_key_value() else {
                return;
            };
            if !self.remove_start(start) {
                // An index slot with no backing entry is a ledger
                // defect; fail closed rather than loop.
                self.poison("orphan_index_slot");
                return;
            }
            self.accounting.record_eviction();
        }
    }

    /// Apply the current pressure band's forced-shrink target for the
    /// transform-cache class, called at the head of every operation
    /// before the cache is read or grown (`plans/SMARTRAM.md` section
    /// 7): preserved at mild pressure, drained to zero from moderate
    /// on. Every evicted buffer is wiped on the way out.
    fn enforce_pressure(&mut self) {
        if self.poisoned {
            return;
        }
        // The operator disabled the transform cache (`cache.transform` or
        // the master `cache.all` off): drop (wiping) everything and admit
        // nothing further — a real bypass, every read runs the full
        // transform pipeline. Re-enabling lets the next read refill it.
        if !self.control.admits(CacheClass::Transform) {
            if self.accounting.total_bytes() > 0 {
                self.drop_all();
            }
            return;
        }
        let band = self.pressure.sample();
        let target = shrink_target(band, ReclaimClass::TransformCache, self.budget);
        if self.accounting.total_bytes() > target {
            self.accounting
                .record_pressure_shrink(ReclaimClass::TransformCache);
            self.evict_until(target);
        }
    }
}

impl ClusterCache for TransformClusterCache {
    fn get(&mut self, phys: u64) -> Option<&[u8]> {
        if self.poisoned {
            return None;
        }
        self.enforce_pressure();
        if !self.entries.contains_key(&phys) {
            self.accounting.record_miss(ReclaimClass::TransformCache);
            return None;
        }
        let tick = self.next_tick();
        if let Some(entry) = self.entries.get_mut(&phys) {
            self.lru.remove(&entry.tick);
            self.lru.insert(tick, phys);
            entry.tick = tick;
        }
        self.accounting.record_hit(ReclaimClass::TransformCache);
        self.entries.get(&phys).map(|entry| entry.plain.as_slice())
    }

    fn put(&mut self, phys: u64, stored: u64, plaintext: &[u8]) {
        if self.poisoned {
            return;
        }
        self.enforce_pressure();
        if !self.control.admits(CacheClass::Transform) {
            self.accounting.record_refusal(ReclaimClass::TransformCache);
            return;
        }
        // A shapeless offer (no run to invalidate against, an empty or
        // over-bound plaintext) is refused: entries stay individually
        // bounded and coherently invalidatable.
        if stored == 0 || plaintext.is_empty() || plaintext.len() > MAX_CLUSTER_PLAINTEXT {
            self.accounting.record_refusal(ReclaimClass::TransformCache);
            return;
        }
        // Replace, never shadow: a stale entry under the same key would
        // desynchronise the ledger.
        if self.remove_start(phys) {
            self.accounting.record_invalidation();
        }
        if self.poisoned {
            return;
        }
        let (payload, metadata) = Self::cost_of(plaintext.len());
        let cost = payload.saturating_add(metadata);
        let class = ReclaimClass::TransformCache;
        // One reading: the ceiling and the reserve draw must agree on the
        // band, so both come from the same fold.
        let mut allowance = self.pressure.growth_allowance();
        let ceiling = shrink_target(allowance.band(), class, self.budget);
        if !allowance.take(class, self.budget, cost) {
            self.accounting.record_refusal(class);
            return;
        }
        if self.accounting.total_bytes().saturating_add(cost) > ceiling {
            let headroom = self.budget.low().min(ceiling - cost);
            self.evict_until(headroom);
            if self.poisoned {
                return;
            }
        }
        // The copy is fallible: heap exhaustion refuses the entry
        // instead of aborting.
        let mut plain = Vec::new();
        if plain.try_reserve_exact(plaintext.len()).is_err() {
            self.accounting.record_refusal(ReclaimClass::TransformCache);
            return;
        }
        plain.extend_from_slice(plaintext);
        if self
            .accounting
            .charge(ReclaimClass::TransformCache, payload, metadata)
            .is_err()
        {
            Self::wipe(&mut plain);
            self.accounting.record_refusal(ReclaimClass::TransformCache);
            return;
        }
        let tick = self.next_tick();
        self.lru.insert(tick, phys);
        self.entries.insert(
            phys,
            Entry {
                stored,
                plain,
                tick,
            },
        );
    }

    fn invalidate(&mut self, phys: u64) {
        let covering = self
            .entries
            .range(..=phys)
            .next_back()
            .filter(|(start, entry)| phys - *start < entry.stored)
            .map(|(start, _)| *start);
        if let Some(start) = covering {
            if self.remove_start(start) {
                self.accounting.record_invalidation();
            }
        }
    }

    fn purge(&mut self) {
        self.drop_all();
    }
}

impl Drop for TransformClusterCache {
    /// Teardown wipes every retained buffer: the entries hold decrypted
    /// user data, which must not outlive their owner in reusable heap
    /// memory.
    fn drop(&mut self) {
        for entry in self.entries.values_mut() {
            Self::wipe(&mut entry.plain);
        }
    }
}

#[cfg(test)]
#[path = "transform_cache_tests.rs"]
mod tests;
