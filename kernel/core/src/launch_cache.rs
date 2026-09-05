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
//! and classifies it through the `tairix_reclaim` admission gate.
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
//! ([`tairix_reclaim::PressureGauge::growth_permitted`]). Inserts over the
//! hard limit
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

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;

use tairix_appload::LoadedApp;
use tairix_collections::LruMap;
use tairix_hash::BuildSipHash13;
use tairix_log::Sink;
use tairix_reclaim::{
    log_cache_poisoned, log_cache_refused, shrink_target, CacheAccounting, CacheBudget,
    CacheCandidate, CacheLedger, CachePolicy, InvalidationSource, MemoryPressure, RebuildCost,
    ReclaimClass, ReclaimOwner, ReclaimRule, Sensitivity,
};

use crate::cache_control::{CacheClass, CacheControl, CACHE_CONTROL};

/// Approximate per-entry bookkeeping cost (the index bucket, the recency
/// node, the key copy's own allocation, the `Arc` control block) charged on
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

/// The reclaimable semantic launch cache the app store installs once
/// the `/System` mount is published. See the module docs.
pub struct LaunchCache {
    budget: CacheBudget,
    /// The live cache-admission control (the operator's `cache.semantic`
    /// / `cache.all` switch). Sampled at the head of every operation: when
    /// the semantic class is disabled the cache admits nothing and drops
    /// what it holds, a real bypass. Defaults to the process-global
    /// [`CACHE_CONTROL`]; a test binds its own.
    control: &'static CacheControl,
    /// The system memory-pressure gauge, sampled at the head of every
    /// operation: the band's forced-shrink target is applied before the
    /// cache is read or grown, and admission is refused outside normal
    /// pressure or when growth would dip into the reserve.
    pressure: &'static MemoryPressure,
    /// The audit sink a classification refusal or detected ledger
    /// defect reports through (`tairix_reclaim::audit`).
    sink: &'static (dyn Sink + Sync),
    accounting: Arc<CacheAccounting>,
    /// The classified admission policy; `None` when classification
    /// refused, which poisons the cache from birth.
    policy: Option<CachePolicy>,
    /// The cache admits nothing further (a classification refusal, an
    /// unkeyable index, or a detected ledger defect): every lookup misses and
    /// every launch runs the full load gate (fail closed).
    poisoned: bool,
    /// Retained verification results, keyed by the bundle root path, with the
    /// recency order eviction takes maintained alongside them.
    entries: LruMap<String, Arc<LoadedApp>, BuildSipHash13>,
}

impl LaunchCache {
    /// Build a cache bounded by `budget` and governed by the system
    /// `pressure` gauge, charged to the kernel app-store subsystem.
    ///
    /// The candidate declaration passes the `tairix_reclaim`
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
        // The keys are caller-chosen bundle paths, so the index is keyed under
        // the per-boot hash key. A boot that never got one starts the cache
        // poisoned rather than filing them under a predictable one: every
        // launch then runs the full load gate, which is the slower answer and
        // never the wrong one.
        let Ok(hasher) = BuildSipHash13::keyed() else {
            log_cache_poisoned(sink, CACHE_LABEL, Some(owner), "unkeyed_index");
            return Self {
                budget,
                control: &CACHE_CONTROL,
                pressure,
                sink,
                accounting: Arc::new(CacheAccounting::new()),
                poisoned: true,
                policy,
                entries: LruMap::with_hasher(BuildSipHash13::UNKEYED),
            };
        };
        Self {
            budget,
            control: &CACHE_CONTROL,
            pressure,
            sink,
            accounting: Arc::new(CacheAccounting::new()),
            poisoned: policy.is_none(),
            policy,
            entries: LruMap::with_hasher(hasher),
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

    /// Remove the entry under `key`, discharging its cost. `false` when no
    /// such entry exists.
    fn remove_key(&mut self, key: &str) -> bool {
        let Some(app) = self.entries.remove(key) else {
            return false;
        };
        self.discharge(key, &app);
        true
    }

    /// Discharge exactly what an entry was charged, poisoning the cache if the
    /// ledger will not balance.
    fn discharge(&mut self, key: &str, app: &Arc<LoadedApp>) {
        let (payload, metadata) = Self::cost_of(key, app);
        if self
            .accounting
            .discharge(ReclaimClass::SemanticAppCache, payload, metadata)
            .is_err()
        {
            self.poison("ledger_imbalance");
        }
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
        self.discard_all();
    }

    /// Drop every entry, counting the drain as a teardown and rebalancing
    /// the ledger to empty. Shared by [`Self::poison`] and the disabled-
    /// class drain so the whole-cache drop has one definition.
    fn discard_all(&mut self) {
        self.accounting
            .record_teardown(ReclaimClass::SemanticAppCache);
        self.entries.clear();
        self.accounting.zero_ledger();
    }

    /// Evict least-recently-used entries until the ledger total is at
    /// most `target`.
    fn evict_until(&mut self, target: usize) {
        while self.accounting.total_bytes() > target {
            let Some((key, app)) = self.entries.pop_lru() else {
                // Bytes charged with nothing left to release is a ledger
                // defect; fail closed rather than loop.
                self.poison("orphan_index_slot");
                return;
            };
            self.discharge(&key, &app);
            self.accounting.record_eviction();
            if self.poisoned {
                return;
            }
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
        // The operator disabled the launch cache (`cache.semantic` or the
        // master `cache.all` off): drop everything and admit nothing
        // further — a real bypass, every launch runs the full load gate.
        // Re-enabling lets the next verification refill it.
        if !self.control.admits(CacheClass::Semantic) {
            if self.accounting.total_bytes() > 0 {
                self.discard_all();
            }
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
        let Some(app) = self.entries.get(bundle).map(Arc::clone) else {
            self.accounting.record_miss(ReclaimClass::SemanticAppCache);
            return None;
        };
        self.accounting.record_hit(ReclaimClass::SemanticAppCache);
        Some(app)
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
        if !self.control.admits(CacheClass::Semantic) {
            self.accounting
                .record_refusal(ReclaimClass::SemanticAppCache);
            return;
        }
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
        let class = ReclaimClass::SemanticAppCache;
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
        // The key copy is fallible: heap exhaustion refuses the entry
        // instead of aborting.
        let mut key = String::new();
        if key.try_reserve_exact(bundle.len()).is_err() {
            self.accounting
                .record_refusal(ReclaimClass::SemanticAppCache);
            return;
        }
        key.push_str(bundle);
        // Room for the entry before the ledger is touched, so a refusal
        // discharges nothing it never charged.
        if self.entries.try_reserve(1).is_err() {
            self.accounting
                .record_refusal(ReclaimClass::SemanticAppCache);
            return;
        }
        if self
            .accounting
            .charge(ReclaimClass::SemanticAppCache, payload, metadata)
            .is_err()
        {
            self.accounting
                .record_refusal(ReclaimClass::SemanticAppCache);
            return;
        }
        if self.entries.try_insert(key, Arc::clone(app)).is_err() {
            debug_assert!(false, "a reservation left room for the entry");
            self.accounting
                .record_refusal(ReclaimClass::SemanticAppCache);
        }
    }

    /// Whether `bundle` is currently resident, without disturbing the
    /// cache — no LRU restamp, no hit/miss accounting, no pressure
    /// enforcement (all of which [`Self::lookup`] performs).
    ///
    /// This is an advisory *existence* peek for the spawn path's
    /// synchronous store-bundle probe: a resident entry is proof the load
    /// gate accepted this bundle's bytes from the immutable read-only
    /// store earlier this boot, so the bundle certainly exists on disk and
    /// the probe can skip its filesystem lookup. It grants no authority and
    /// serves no image — the launch still re-authorises the caller's read
    /// and re-runs the gate on a miss (`plans/FIX-DESKTOP.md` §2.1). Because
    /// it does not enforce pressure, a `true` may momentarily outlive an
    /// entry a concurrent reclaim would drop; that only costs the probe one
    /// redundant lookup avoided, never a wrong launch decision. A poisoned
    /// cache reports nothing resident.
    #[must_use]
    pub fn contains(&self, bundle: &str) -> bool {
        !self.poisoned && self.entries.contains_key(bundle)
    }

    /// The keys currently resident, oldest first — test and diagnostics
    /// visibility only; never an authority or serving path.
    #[must_use]
    pub fn resident(&self) -> Vec<&str> {
        self.entries
            .iter_lru()
            .map(|(key, _)| key.as_str())
            .collect()
    }
}

#[cfg(test)]
#[path = "launch_cache_tests.rs"]
mod tests;
