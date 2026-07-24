//! The semantic application-launch cache (`plans/SMARTRAM.md` SMART4,
//! section 6.3).
//!
//! [`LaunchCache`] retains the [`LoadedApp`] the shared `tairix_appload`
//! load gate accepted for a bundle on the immutable read-only system
//! stores: the parsed signed manifest, the content-hash and
//! syscall-interface-hash verdicts, the dynamic-loader library policy
//! decisions, and the validated `rxe` entry-point image. A later launch
//! of the same bundle serves that result instead of re-reading,
//! re-hashing, and re-verifying the whole bundle tree — launch latency
//! is a designed hot path — while the *caller's* authority is still
//! checked per launch by the spawn path (the cache stores no
//! caller-dependent decision; see below).
//!
//! # Classification, budget, pressure
//!
//! At construction the cache declares its [`CacheCandidate`] — class
//! [`ReclaimClass::SemanticAppCache`], owned by the kernel app-store
//! subsystem, expensive to rebuild (a whole-tree hash plus an Ed25519
//! verification), system data, invalidated by generation (the read-only
//! store is immutable for the life of the boot, so the boot *is* the
//! generation and a system update is a new boot), droppable on demand —
//! and classifies it through the `kernel/mem::reclaim` admission gate.
//! A refusal starts the cache poisoned: every lookup misses, every
//! insert is refused, and every launch runs the full load gate (fail
//! closed).
//!
//! The cache is bounded by a [`CacheBudget`] and accounted in a
//! [`CacheAccounting`] ledger. Every operation first applies the
//! current pressure band's forced-shrink target for the semantic class
//! ([`shrink_target`]): shrunk to the low watermark at mild pressure and
//! drained to zero from moderate pressure on — before anonymous pages
//! are handed to `ramzip` (`plans/SWAPSWAPSWAP.md` section 6). Growth is
//! admitted only at normal pressure and never into the reserve
//! ([`MemoryPressure::growth_permitted`]). Inserts over the hard limit
//! first evict least-recently-used entries down to the low watermark
//! (hysteresis). Reclaim can never make an app unlaunchable: a miss
//! simply re-runs the load gate against the intact on-disk bundle.
//!
//! # What a hit does and does not prove
//!
//! A hit is proof the load gate accepted exactly these immutable bytes
//! earlier this boot. It carries **no** caller-dependent authority: the
//! cached capability ceiling is the manifest request itself (the spawn
//! path verifies it against the full-word intersection identity before
//! inserting), and the per-caller intersection with the spawning
//! credential's user ceiling — plus the caller's re-authorised read of
//! the entry point through the secured VFS — happens on every launch,
//! hit or miss. A policy or grant change therefore can never be served
//! stale from this cache, and a hit and a miss produce identical load
//! decisions.
//!
//! Entries are shared [`Arc`]s to signed, public system code
//! ([`Sensitivity::SystemData`]) — no credential, key, or user plaintext
//! — so eviction drops the cache's reference without wiping: a launched
//! process legitimately holds the same image, and the bytes are the
//! world-readable store's own contents.

use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;

use tairix_appload::LoadedApp;
use tairix_kernel_mem::{
    log_cache_poisoned, log_cache_refused, shrink_target, CacheAccounting, CacheBudget,
    CacheCandidate, CachePolicy, InvalidationSource, MemoryPressure, RebuildCost, ReclaimClass,
    ReclaimOwner, ReclaimRule, Sensitivity,
};
use tairix_log::Sink;

/// Approximate per-entry bookkeeping cost (map nodes, the LRU index
/// slot, the fixed entry fields, the `Arc` control block) charged on
/// top of an entry's payload so the ledger tracks real heap footprint.
const ENTRY_OVERHEAD: usize = 128;

/// Longest bundle-root key the cache admits. A validation bound, not a
/// capacity: every cacheable bundle lives directly under one of the two
/// system stores, whose paths are far shorter, and bounding the key is
/// what keeps the declared per-entry metadata honest. A longer key is
/// refused (served uncached), never truncated.
const MAX_BUNDLE_KEY: usize = 256;

/// The fixed `cache` label this cache's audit records carry.
const CACHE_LABEL: &str = "launch";

/// One cached verification result: the accepted [`LoadedApp`] and the
/// recency tick of its last use.
struct Entry {
    app: Arc<LoadedApp>,
    tick: u64,
}

/// The reclaimable semantic launch cache the app store installs once
/// the `/System` mount is published. See the module docs.
pub struct LaunchCache {
    budget: CacheBudget,
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
    /// detected ledger defect): every lookup misses and every launch
    /// runs the full load gate (fail closed).
    poisoned: bool,
    /// Monotonic recency counter; every touch assigns a fresh tick, so
    /// ticks are unique and the LRU index is keyed by them.
    tick: u64,
    /// Retained verification results, keyed by the bundle root path.
    entries: BTreeMap<String, Entry>,
    /// LRU index: tick (oldest first) to entry key.
    lru: BTreeMap<u64, String>,
}

impl LaunchCache {
    /// Build a cache bounded by `budget` and governed by the system
    /// `pressure` gauge, charged to the kernel app-store subsystem.
    ///
    /// The candidate declaration passes the `kernel/mem::reclaim`
    /// classification gate; a refusal poisons the cache from birth, so
    /// every launch still works through the full load gate — fail
    /// closed, never an unclassified cache.
    #[must_use]
    pub fn new(
        budget: CacheBudget,
        pressure: &'static MemoryPressure,
        sink: &'static (dyn Sink + Sync),
    ) -> Self {
        let owner = ReclaimOwner::KernelSubsystem("app_store");
        let candidate = CacheCandidate {
            class: Some(ReclaimClass::SemanticAppCache),
            owner: Some(owner),
            rebuild_cost: RebuildCost::Expensive,
            sensitivity: Some(Sensitivity::SystemData),
            invalidation: Some(InvalidationSource::GenerationToken),
            rule: Some(ReclaimRule::Drop),
            entry_metadata_bytes: ENTRY_OVERHEAD + MAX_BUNDLE_KEY,
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

    /// The owner the cache's memory is charged to, or `None` when
    /// classification refused the cache (it is then poisoned and admits
    /// nothing).
    #[must_use]
    pub fn owner(&self) -> Option<ReclaimOwner> {
        self.policy.map(CachePolicy::owner)
    }

    /// The next unique recency tick.
    fn next_tick(&mut self) -> u64 {
        self.tick += 1;
        self.tick
    }

    /// The accounted `(payload, metadata)` byte cost of caching `app`
    /// under `key`: the validated image, the owned manifest strings,
    /// and the resolved-library references as payload; the key copy,
    /// the per-library node overhead, and the fixed overhead as
    /// per-entry bookkeeping.
    fn cost_of(key: &str, app: &LoadedApp) -> (usize, usize) {
        let strings = app
            .id()
            .len()
            .saturating_add(app.name().len())
            .saturating_add(app.version().len())
            .saturating_add(app.run_path().len());
        let references: usize = app
            .libraries()
            .iter()
            .map(|library| library.reference.len())
            .sum();
        let payload = app
            .run_image()
            .len()
            .saturating_add(strings)
            .saturating_add(references);
        let metadata = ENTRY_OVERHEAD
            .saturating_add(key.len())
            .saturating_add(app.libraries().len().saturating_mul(ENTRY_OVERHEAD));
        (payload, metadata)
    }

    /// Remove the entry under `key`, dropping its LRU slot and
    /// discharging its cost. `false` when no such entry exists.
    fn remove_key(&mut self, key: &str) -> bool {
        let Some(entry) = self.entries.remove(key) else {
            return false;
        };
        self.lru.remove(&entry.tick);
        let (payload, metadata) = Self::cost_of(key, &entry.app);
        if self
            .accounting
            .discharge(ReclaimClass::SemanticAppCache, payload, metadata)
            .is_err()
        {
            self.poison("ledger_imbalance");
        }
        true
    }

    /// Drop every entry and admit nothing further: the fail-closed
    /// response to the internal defect named by `cause`. Launches keep
    /// working; only the cache is disabled. The defect is counted and
    /// reported once through the audit sink, and the whole-cache drain
    /// is counted as a teardown.
    fn poison(&mut self, cause: &'static str) {
        if !self.poisoned {
            self.accounting
                .record_failure(ReclaimClass::SemanticAppCache);
            log_cache_poisoned(self.sink, CACHE_LABEL, self.owner(), cause);
        }
        self.poisoned = true;
        self.accounting
            .record_teardown(ReclaimClass::SemanticAppCache);
        self.entries.clear();
        self.lru.clear();
        self.accounting.zero_ledger();
    }

    /// Evict least-recently-used entries until the ledger total is at
    /// most `target`.
    fn evict_until(&mut self, target: usize) {
        while self.accounting.total_bytes() > target {
            let Some((_, key)) = self.lru.first_key_value() else {
                return;
            };
            let key = key.clone();
            if !self.remove_key(&key) {
                // An index slot with no backing entry is a ledger
                // defect; fail closed rather than loop.
                self.poison("orphan_index_slot");
                return;
            }
            self.accounting.record_eviction();
        }
    }

    /// Apply the current pressure band's forced-shrink target for the
    /// semantic-app-cache class, called at the head of every operation
    /// before the cache is read or grown (`plans/SMARTRAM.md` section
    /// 7): shrunk to the low watermark at mild pressure, drained to
    /// zero from moderate pressure on.
    fn enforce_pressure(&mut self) {
        if self.poisoned {
            return;
        }
        let band = self.pressure.sample();
        let target = shrink_target(band, ReclaimClass::SemanticAppCache, self.budget);
        if self.accounting.total_bytes() > target {
            self.accounting
                .record_pressure_shrink(ReclaimClass::SemanticAppCache);
            self.evict_until(target);
        }
    }

    /// The cached verification result for `bundle`, refreshing its LRU
    /// stamp, or `None` when the bundle has not been verified this boot
    /// (or the entry was reclaimed — the caller then re-runs the load
    /// gate, so reclaim can never make an app unlaunchable).
    pub fn lookup(&mut self, bundle: &str) -> Option<Arc<LoadedApp>> {
        if self.poisoned {
            return None;
        }
        self.enforce_pressure();
        if !self.entries.contains_key(bundle) {
            self.accounting.record_miss(ReclaimClass::SemanticAppCache);
            return None;
        }
        let tick = self.next_tick();
        if let Some(entry) = self.entries.get_mut(bundle) {
            self.lru.remove(&entry.tick);
            self.lru.insert(tick, String::from(bundle));
            entry.tick = tick;
        }
        self.accounting.record_hit(ReclaimClass::SemanticAppCache);
        self.entries.get(bundle).map(|entry| Arc::clone(&entry.app))
    }

    /// Record `app` as the verified result for `bundle`.
    ///
    /// Admission is refused — the launch is served, just not cached —
    /// when the key exceeds its bound, the entry alone exceeds the hard
    /// limit, pressure is not normal, or growth would dip into the
    /// reserve. An insert over the hard limit first evicts
    /// least-recently-used entries down to the low watermark; a
    /// re-verification of an already cached bundle replaces its entry.
    pub fn insert(&mut self, bundle: &str, app: &Arc<LoadedApp>) {
        if self.poisoned {
            return;
        }
        self.enforce_pressure();
        if bundle.len() > MAX_BUNDLE_KEY {
            self.accounting
                .record_refusal(ReclaimClass::SemanticAppCache);
            return;
        }
        // Replace, never shadow: a stale entry under the same key would
        // desynchronise the ledger.
        if self.remove_key(bundle) {
            self.accounting.record_invalidation();
        }
        if self.poisoned {
            return;
        }
        let (payload, metadata) = Self::cost_of(bundle, app);
        let cost = payload.saturating_add(metadata);
        if cost > self.budget.hard() || !self.pressure.growth_permitted(cost) {
            self.accounting
                .record_refusal(ReclaimClass::SemanticAppCache);
            return;
        }
        if self.accounting.total_bytes().saturating_add(cost) > self.budget.hard() {
            let headroom = self.budget.low().min(self.budget.hard() - cost);
            self.evict_until(headroom);
            if self.poisoned {
                return;
            }
        }
        // The key copy is fallible: heap exhaustion refuses the entry
        // instead of aborting.
        let mut key = String::new();
        if key.try_reserve_exact(bundle.len()).is_err() {
            self.accounting
                .record_refusal(ReclaimClass::SemanticAppCache);
            return;
        }
        key.push_str(bundle);
        if self
            .accounting
            .charge(ReclaimClass::SemanticAppCache, payload, metadata)
            .is_err()
        {
            self.accounting
                .record_refusal(ReclaimClass::SemanticAppCache);
            return;
        }
        let tick = self.next_tick();
        self.lru.insert(tick, key.clone());
        self.entries.insert(
            key,
            Entry {
                app: Arc::clone(app),
                tick,
            },
        );
    }

    /// The keys currently resident, oldest first — test and diagnostics
    /// visibility only; never an authority or serving path.
    #[must_use]
    pub fn resident(&self) -> Vec<&str> {
        self.lru.values().map(String::as_str).collect()
    }
}

#[cfg(test)]
#[path = "launch_cache_tests.rs"]
mod tests;
