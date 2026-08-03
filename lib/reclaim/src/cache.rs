//! The one bounded, classified, pressure-governed cache
//! (`plans/SMARTRAM.md` sections 5 and 7).
//!
//! Every reclaimable cache in TAIRiX owes the same obligations: it is
//! classified through the [`classification gate`](crate::CacheCandidate::classify)
//! before it holds a single byte, it is bounded by a
//! [`CacheBudget`] derived from its backing rather than a hand-picked
//! ceiling, it shrinks to the [`shrink_target`] its class and the live
//! [`PressureGauge`] band dictate, it charges every entry to a
//! [`CacheAccounting`] ledger with checked arithmetic, it invalidates
//! precisely on a generation change, it wipes anything that was not
//! public before releasing it, and it fails closed — poisoning itself
//! and serving uncached — the moment its books stop balancing.
//!
//! [`ReclaimCache`] is that obligation, implemented once. A consumer
//! supplies only what is genuinely its own: the key it looks up by, the
//! value it retains, the generation that invalidates the lot, and how
//! to build a value on a miss.
//!
//! # Generations
//!
//! A *generation* is whatever the consumer decides invalidates every
//! entry at once — the active display scale combined with the active
//! theme, a mount generation, a policy epoch.
//! [`get_or_build`](ReclaimCache::get_or_build) drops the whole cache
//! the moment the generation differs from the one the entries were
//! built at, so a stale entry cannot be served: this is the
//! [`InvalidationSource::GenerationToken`](crate::InvalidationSource::GenerationToken)
//! contract, not a best-effort sweep.
//!
//! # Never required for correctness
//!
//! A reclaimable cache is an accelerator. Every refusal path here — a
//! poisoned cache, a band that forbids growth, a value larger than the
//! whole budget, a ledger that would overflow — still hands the caller
//! the value it asked for, as [`Served::Uncached`]. The caller never
//! has to decide what to do when caching is unavailable, and no path
//! renders twice.
//!
//! # Hot path
//!
//! A hit is one generation comparison, one gauge sample, one `BTreeMap`
//! lookup, and one recency touch — all O(log n), never a linear scan of
//! the entries. Eviction pops the oldest entry from a `(tick -> key)`
//! index, so a forced shrink visits only the entries it actually
//! releases rather than sweeping the whole cache to find them.

use alloc::collections::BTreeMap;
use alloc::sync::Arc;
use core::ops::Deref;

use tairix_log::Sink;

use crate::audit::{log_cache_poisoned, log_cache_refused};
use crate::ledger::CacheLedger;
use crate::model::{
    AccountingError, CacheAccounting, CacheBudget, CacheCandidate, CachePolicy, ReclaimClass,
    ReclaimOwner, Sensitivity,
};
use crate::pressure::{shrink_target, PressureGauge};

/// What a cached value costs and how it is destroyed.
///
/// Implemented by the value type a consumer retains. Both methods are
/// obligations, not conveniences: a cache cannot bound what it cannot
/// measure, and it cannot honour the zero-on-release rule for
/// non-public data without a way to overwrite the bytes.
pub trait CachedBytes {
    /// Bytes of heap payload this value retains, excluding the cache's
    /// own per-entry bookkeeping (which the cache charges separately
    /// from its declared
    /// [`entry_metadata_bytes`](crate::CacheCandidate::entry_metadata_bytes)).
    ///
    /// Must be stable for the lifetime of the value: the cache charges
    /// this figure on admission and discharges the same figure on
    /// release, so a value that silently changes size unbalances the
    /// ledger and poisons the cache.
    fn payload_bytes(&self) -> usize;

    /// Overwrite every retained byte, so nothing readable survives in
    /// the freed allocation.
    ///
    /// Called before release for any cache whose declared
    /// [`Sensitivity`] is not [`Sensitivity::Public`]. A value that
    /// genuinely holds nothing worth overwriting still implements this
    /// — with the truthful minimum, not an empty body kept for the
    /// signature's sake.
    fn wipe(&mut self);
}

/// The outcome of a [`get_or_build`](ReclaimCache::get_or_build).
///
/// Both variants carry a usable value, so a caller dereferences the
/// result and never branches on whether caching happened.
#[derive(Debug)]
pub enum Served<'a, V> {
    /// Retained by the cache; the next lookup at this generation is a
    /// hit.
    Cached(&'a V),
    /// Built for this caller alone and not retained, because the cache
    /// could not admit it (pressure forbids growth, the value exceeds
    /// the whole budget, or the cache is poisoned). Correct, just not
    /// accelerated.
    Uncached(V),
}

impl<V> Deref for Served<'_, V> {
    type Target = V;

    fn deref(&self) -> &V {
        match self {
            Self::Cached(value) => value,
            Self::Uncached(value) => value,
        }
    }
}

impl<V> Served<'_, V> {
    /// Whether the value was served from (or admitted to) the cache.
    #[must_use]
    pub const fn is_cached(&self) -> bool {
        matches!(self, Self::Cached(_))
    }
}

/// Why a cache stopped admitting entries and drained itself.
///
/// Both causes are internal-defect conditions, never ordinary
/// operation: an honest cache never reaches them, and one that does
/// serves uncached for the rest of its life rather than trusting books
/// it knows are wrong.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
enum PoisonCause {
    /// A charge would have overflowed the byte ledger.
    LedgerOverflow,
    /// A discharge would have underflowed the byte ledger: more was
    /// removed than was ever charged.
    LedgerUnderflow,
    /// The key index and the recency index disagreed about which
    /// entries exist.
    IndexDivergence,
}

impl PoisonCause {
    /// The fixed audit `cause` label.
    const fn label(self) -> &'static str {
        match self {
            Self::LedgerOverflow => "ledger_overflow",
            Self::LedgerUnderflow => "ledger_underflow",
            Self::IndexDivergence => "index_divergence",
        }
    }

    /// The cause a failed ledger mutation implies.
    const fn of(error: AccountingError) -> Self {
        match error {
            AccountingError::Overflow => Self::LedgerOverflow,
            AccountingError::Underflow => Self::LedgerUnderflow,
        }
    }
}

/// One retained value plus the bookkeeping the cache needs to release
/// it again exactly as it charged it.
struct Entry<V> {
    value: V,
    /// Recency tick; the key of this entry in the LRU index.
    tick: u64,
    /// The payload figure charged on admission, discharged verbatim on
    /// release so the ledger balances even if the value's own
    /// measurement were to drift.
    charged_payload: usize,
}

/// A bounded, classified, generation-invalidated, pressure-governed LRU
/// cache. See the [module docs](self).
///
/// `K` identifies an entry within a generation, `V` is the retained
/// value, and `E` is the generation token whose change empties the
/// cache.
pub struct ReclaimCache<K, V, E> {
    /// Fixed label naming this cache in audit records.
    label: &'static str,
    /// The declared owner and class, kept so the cache can describe itself
    /// to a diagnostics registry. Both are `None` exactly when the
    /// candidate declared none, which is also what poisons the cache.
    owner: Option<ReclaimOwner>,
    declared_class: Option<ReclaimClass>,
    budget: CacheBudget,
    /// The live band the shrink target and admission are decided
    /// against, sampled at the head of every operation — never a mode
    /// frozen at construction.
    pressure: &'static (dyn PressureGauge + 'static),
    sink: &'static (dyn Sink + Sync),
    accounting: Arc<CacheAccounting>,
    /// The classified admission policy; `None` when classification
    /// refused, which poisons the cache from birth.
    policy: Option<CachePolicy>,
    /// Per-entry bookkeeping bytes, charged on top of every payload.
    entry_metadata_bytes: usize,
    /// The cache admits nothing further and holds nothing: a
    /// classification refusal or a detected ledger/index defect. Every
    /// lookup then builds and serves uncached (fail closed).
    poisoned: bool,
    /// Monotonic recency counter; every touch assigns a fresh tick, so
    /// ticks are unique and the LRU index is keyed by them. A `u64` of
    /// ticks outlasts any machine uptime by centuries, so the counter
    /// is never exhausted in practice; it wraps rather than trapping
    /// because a wrapped recency order would still be a correct cache,
    /// merely a badly ordered one.
    tick: u64,
    /// The generation the retained entries were built at.
    generation: Option<E>,
    entries: BTreeMap<K, Entry<V>>,
    /// Recency index: tick (oldest first) to entry key.
    lru: BTreeMap<u64, K>,
}

impl<K, V, E> core::fmt::Debug for ReclaimCache<K, V, E>
where
    K: Ord + Clone,
    V: CachedBytes,
    E: PartialEq + Clone,
{
    /// A summary, not a dump: which cache this is, how much it is
    /// holding, and whether it has failed closed.
    ///
    /// The omitted fields are omitted on purpose. The retained entries,
    /// their keys, and the generation are or derive from *user data* — a
    /// rendered cursor, a glyph, a decoded record — and printing them
    /// would spill that into a log or a test failure message; the
    /// classification, budget, gauge, and sink are fixed at construction
    /// and say nothing about the cache's live state. What a reader
    /// actually needs is here, and the byte ledger behind
    /// [`accounting`](ReclaimCache::accounting) carries the rest.
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("ReclaimCache")
            .field("label", &self.label)
            .field("entries", &self.entries.len())
            .field("charged_bytes", &self.charged_bytes())
            .field("poisoned", &self.poisoned)
            .finish_non_exhaustive()
    }
}

impl<K, V, E> ReclaimCache<K, V, E>
where
    K: Ord + Clone,
    V: CachedBytes,
    E: PartialEq + Clone,
{
    /// Build a cache from a declared [`CacheCandidate`], bounded by
    /// `budget` and governed by `pressure`.
    ///
    /// The candidate passes the classification gate here: a refusal is
    /// audited and poisons the cache from birth, so its consumer keeps
    /// working — building every value on demand — rather than holding
    /// unclassified memory.
    #[must_use]
    pub fn new(
        label: &'static str,
        candidate: CacheCandidate,
        budget: CacheBudget,
        pressure: &'static (dyn PressureGauge + 'static),
        sink: &'static (dyn Sink + Sync),
    ) -> Self {
        let owner = candidate.owner;
        let declared_class = candidate.class;
        let entry_metadata_bytes = candidate.entry_metadata_bytes;
        let policy = match candidate.classify() {
            Ok(policy) => Some(policy),
            Err(refusal) => {
                log_cache_refused(sink, label, owner, refusal);
                None
            }
        };
        Self {
            label,
            owner,
            declared_class,
            budget,
            pressure,
            sink,
            accounting: Arc::new(CacheAccounting::new()),
            poisoned: policy.is_none(),
            entry_metadata_bytes,
            policy,
            tick: 0,
            generation: None,
            entries: BTreeMap::new(),
            lru: BTreeMap::new(),
        }
    }

    /// This cache's byte ledger and event counters.
    #[must_use]
    pub fn accounting(&self) -> &CacheAccounting {
        &self.accounting
    }

    /// A shared handle to this cache's ledger, for registration with a
    /// diagnostics registry. Observation-only: the holder gets
    /// lock-free reads of the same counters this cache keeps.
    #[must_use]
    pub fn accounting_shared(&self) -> Arc<CacheAccounting> {
        Arc::clone(&self.accounting)
    }

    /// This cache described for a diagnostics registry: its label, owner,
    /// and class, plus a shared handle to the counters above.
    ///
    /// `None` when the candidate declared no owner or no class. Such a
    /// cache is poisoned from birth and retains nothing, so there is no
    /// footprint to attribute and a row for it would say only that a
    /// misdeclared cache exists — which its classification refusal already
    /// said, in the audit log, with the reason.
    #[must_use]
    pub fn ledger(&self) -> Option<CacheLedger> {
        Some(CacheLedger::new(
            self.label,
            self.owner?,
            self.declared_class?,
            self.accounting_shared(),
        ))
    }

    /// Whether the cache has disabled itself and serves everything
    /// uncached.
    #[must_use]
    pub const fn poisoned(&self) -> bool {
        self.poisoned
    }

    /// Entries currently retained.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether nothing is retained.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Bytes currently charged for this cache, payload plus per-entry
    /// bookkeeping.
    #[must_use]
    pub fn charged_bytes(&self) -> usize {
        match self.policy {
            Some(policy) => self.accounting.class_bytes(policy.class()),
            None => 0,
        }
    }

    /// The generation the retained entries were built at, or `None`
    /// before the first build.
    #[must_use]
    pub const fn generation(&self) -> Option<&E> {
        self.generation.as_ref()
    }

    /// Borrow the retained value for `key` at `generation` without
    /// building it, touching recency, or sampling pressure.
    ///
    /// This is for a caller that must hold *several* entries borrowed at
    /// the same time, which the exclusive borrow
    /// [`get_or_build`](Self::get_or_build) needs cannot express: the
    /// compositor resolves one source row per overlapping window before
    /// it walks the columns of a scanline, so every covering window's
    /// furniture is read at once.
    ///
    /// Recency is not a casualty of the untouched read. Such a caller
    /// first ensures residency through
    /// [`get_or_build`](Self::get_or_build) for every key it is about to
    /// read, in the same operation, so each entry is touched exactly
    /// once per pass and the least-recently-*composited* window is still
    /// the one eviction takes.
    ///
    /// Returns `None` for an absent key, a poisoned cache, or a
    /// generation that differs from the retained one: a stale entry is
    /// never served, and the caller falls back to building the value
    /// itself exactly as it would for a cache that refused it.
    #[must_use]
    pub fn peek(&self, generation: &E, key: &K) -> Option<&V> {
        if self.poisoned || self.generation.as_ref() != Some(generation) {
            return None;
        }
        self.entries.get(key).map(|entry| &entry.value)
    }

    /// Serve `key` at `generation`, building it once if it is absent.
    ///
    /// A generation different from the retained one empties the cache
    /// first — precise invalidation, not a stale read. Within the
    /// generation a present key is returned without calling `build`.
    ///
    /// `build` returning `None` (a degenerate input the consumer cannot
    /// render) retains nothing and yields `None`, so the next call
    /// retries rather than the failure being remembered.
    pub fn get_or_build<F>(&mut self, generation: &E, key: K, build: F) -> Option<Served<'_, V>>
    where
        F: FnOnce() -> Option<V>,
    {
        if self.poisoned {
            return build().map(Served::Uncached);
        }

        if self.generation.as_ref() != Some(generation) {
            let dropped = !self.entries.is_empty();
            self.invalidate_all();
            self.generation = Some(generation.clone());
            if dropped {
                self.accounting.record_invalidation();
            }
        }

        self.enforce_pressure();

        let Some(policy) = self.policy else {
            return build().map(Served::Uncached);
        };

        if self.entries.contains_key(&key) {
            self.accounting.record_hit(policy.class());
            self.touch(&key);
            return self
                .entries
                .get(&key)
                .map(|entry| Served::Cached(&entry.value));
        }

        self.accounting.record_miss(policy.class());
        let value = build()?;
        Some(self.admit(policy, key, value))
    }

    /// Release the entry for `key` if it is retained, wiping it where
    /// the declared sensitivity requires.
    ///
    /// Precise invalidation for a consumer whose source of truth
    /// changed for exactly one key (an asset replaced, a pinned
    /// application removed) — never a whole-cache purge for a
    /// single-entry change.
    pub fn invalidate(&mut self, key: &K) {
        if self.release(key).is_some() {
            self.accounting.record_invalidation();
        }
    }

    /// Apply the band's forced shrink now, returning the bytes
    /// released.
    ///
    /// Called on its own when a consumer learns the band changed
    /// (rather than waiting for its next lookup to notice), and
    /// internally at the head of every lookup. A band whose target is
    /// already met releases nothing and counts nothing.
    pub fn enforce_pressure(&mut self) -> usize {
        let Some(policy) = self.policy else {
            return 0;
        };
        let band = self.pressure.sample();
        let ceiling = shrink_target(band, policy.class(), self.budget);
        let before = self.charged_bytes();
        if before <= ceiling {
            return 0;
        }
        self.evict_to(ceiling);
        let released = before.saturating_sub(self.charged_bytes());
        if released > 0 {
            self.accounting.record_pressure_shrink(policy.class());
        }
        released
    }

    /// Release everything the cache holds because its owner is going
    /// away — a session ending, a seat revoked, a consumer shutting
    /// down.
    ///
    /// Every value is wiped where the declared sensitivity requires, so
    /// a torn-down owner leaves no readable rendered user data behind
    /// in reusable heap.
    pub fn teardown(&mut self) {
        let had_entries = !self.entries.is_empty();
        self.invalidate_all();
        if let Some(policy) = self.policy {
            if had_entries {
                self.accounting.record_teardown(policy.class());
            }
        }
    }

    /// Admit `value` under `key`, or hand it back unretained when the
    /// live policy refuses it.
    fn admit(&mut self, policy: CachePolicy, key: K, value: V) -> Served<'_, V> {
        let payload = value.payload_bytes();
        let cost = payload.saturating_add(self.entry_metadata_bytes);

        if cost > self.budget.hard() || !self.pressure.growth_permitted(cost) {
            self.accounting.record_refusal(policy.class());
            return Served::Uncached(value);
        }

        if self.charged_bytes().saturating_add(cost) > self.budget.hard() {
            self.evict_to(self.budget.low().saturating_sub(cost));
        }
        if self.charged_bytes().saturating_add(cost) > self.budget.hard() {
            self.accounting.record_refusal(policy.class());
            return Served::Uncached(value);
        }

        if let Err(error) =
            self.accounting
                .charge(policy.class(), payload, self.entry_metadata_bytes)
        {
            self.poison(PoisonCause::of(error));
            return Served::Uncached(value);
        }

        self.tick = self.tick.wrapping_add(1);
        let tick = self.tick;
        self.lru.insert(tick, key.clone());
        let admitted = self.entries.entry(key).or_insert(Entry {
            value,
            tick,
            charged_payload: payload,
        });
        Served::Cached(&admitted.value)
    }

    /// Move `key` to the head of the recency order.
    fn touch(&mut self, key: &K) {
        self.tick = self.tick.wrapping_add(1);
        let next = self.tick;
        let Some(entry) = self.entries.get_mut(key) else {
            return;
        };
        let previous = entry.tick;
        entry.tick = next;
        self.lru.remove(&previous);
        self.lru.insert(next, key.clone());
    }

    /// Evict oldest-first until the charged total is at or below
    /// `ceiling`.
    fn evict_to(&mut self, ceiling: usize) {
        while self.charged_bytes() > ceiling {
            let Some((_, key)) = self.lru.iter().next().map(|(t, k)| (*t, k.clone())) else {
                // Bytes remain charged with no entry left to release:
                // the ledger and the index disagree.
                if self.charged_bytes() > 0 {
                    self.poison(PoisonCause::IndexDivergence);
                }
                return;
            };
            if self.release(&key).is_none() {
                self.poison(PoisonCause::IndexDivergence);
                return;
            }
            self.accounting.record_eviction();
            if self.poisoned {
                return;
            }
        }
    }

    /// Remove one entry, discharging exactly what it was charged and
    /// wiping it where the declared sensitivity requires.
    fn release(&mut self, key: &K) -> Option<()> {
        let policy = self.policy?;
        let mut entry = self.entries.remove(key)?;
        self.lru.remove(&entry.tick);
        if wipes_on_release(policy.sensitivity()) {
            entry.value.wipe();
        }
        if let Err(error) = self.accounting.discharge(
            policy.class(),
            entry.charged_payload,
            self.entry_metadata_bytes,
        ) {
            self.poison(PoisonCause::of(error));
        }
        Some(())
    }

    /// Drop every entry, wiping where required, without counting an
    /// eviction or an invalidation per entry.
    fn invalidate_all(&mut self) {
        let wipe = self
            .policy
            .is_some_and(|policy| wipes_on_release(policy.sensitivity()));
        for (_, mut entry) in core::mem::take(&mut self.entries) {
            if wipe {
                entry.value.wipe();
            }
        }
        self.lru.clear();
        self.accounting.zero_ledger();
    }

    /// Disable the cache for the rest of its life: drain it, zero the
    /// ledger, audit the defect once, and serve everything uncached.
    fn poison(&mut self, cause: PoisonCause) {
        if self.poisoned {
            return;
        }
        self.poisoned = true;
        if let Some(policy) = self.policy {
            self.accounting.record_failure(policy.class());
        }
        self.invalidate_all();
        log_cache_poisoned(
            self.sink,
            self.label,
            self.policy.map(CachePolicy::owner),
            cause.label(),
        );
    }
}

/// Whether releasing an entry of this sensitivity must overwrite it
/// first.
///
/// Public entries reveal nothing beyond public system state, so the
/// memset is pure cost and is skipped; everything else — a user's
/// rendered data, system data, or anything derived from a secret — is
/// overwritten before the allocation can be reused.
/// [`Sensitivity::CredentialOrKey`] never reaches a live cache (the
/// classification gate refuses it), and is listed so adding a
/// sensitivity forces a decision here rather than silently defaulting.
const fn wipes_on_release(sensitivity: Sensitivity) -> bool {
    match sensitivity {
        Sensitivity::Public => false,
        Sensitivity::UserData
        | Sensitivity::SystemData
        | Sensitivity::SecretDerived
        | Sensitivity::CredentialOrKey => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    extern crate std;

    use crate::model::{InvalidationSource, RebuildCost, ReclaimClass, ReclaimOwner, ReclaimRule};
    use crate::pressure::{PressureBand, ReportedPressure};
    use alloc::boxed::Box;
    use alloc::vec;
    use alloc::vec::Vec;
    use core::sync::atomic::{AtomicUsize, Ordering};
    use tairix_log::{Event, Sink};

    /// Counts audit records. `Sync`, because the cache holds its sink
    /// as `&'static (dyn Sink + Sync)`.
    struct CountingSink {
        records: AtomicUsize,
    }

    impl CountingSink {
        const fn new() -> Self {
            Self {
                records: AtomicUsize::new(0),
            }
        }

        fn count(&self) -> usize {
            self.records.load(Ordering::Relaxed)
        }
    }

    impl Sink for CountingSink {
        fn write_event(&self, _event: &Event<'_>) {
            self.records.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// A retained buffer with a declared size, so a test can drive the
    /// budget precisely.
    #[derive(Debug, Eq, PartialEq)]
    struct Block {
        bytes: Vec<u8>,
    }

    impl Block {
        fn of(len: usize, fill: u8) -> Self {
            Self {
                bytes: vec![fill; len],
            }
        }
    }

    impl CachedBytes for Block {
        fn payload_bytes(&self) -> usize {
            self.bytes.len()
        }

        fn wipe(&mut self) {
            self.bytes.fill(0);
        }
    }

    const METADATA: usize = 32;

    fn candidate(sensitivity: Sensitivity) -> CacheCandidate {
        CacheCandidate {
            class: Some(ReclaimClass::DisposableUi),
            owner: Some(ReclaimOwner::DesktopSession { seat: 1 }),
            rebuild_cost: RebuildCost::Expensive,
            sensitivity: Some(sensitivity),
            invalidation: Some(InvalidationSource::GenerationToken),
            rule: Some(ReclaimRule::Drop),
            entry_metadata_bytes: METADATA,
        }
    }

    fn leak_gauge(band: PressureBand) -> &'static ReportedPressure {
        let gauge: &'static ReportedPressure = Box::leak(Box::new(ReportedPressure::unknown()));
        gauge.report(band);
        gauge
    }

    fn leak_sink() -> &'static CountingSink {
        Box::leak(Box::new(CountingSink::new()))
    }

    /// A cache with a 4 KiB hard budget at the given band.
    fn cache(
        band: PressureBand,
        sensitivity: Sensitivity,
    ) -> (
        ReclaimCache<u32, Block, u64>,
        &'static ReportedPressure,
        &'static CountingSink,
    ) {
        let gauge = leak_gauge(band);
        let sink = leak_sink();
        let cache = ReclaimCache::new(
            "test.cache",
            candidate(sensitivity),
            CacheBudget::from_backing(64 * 1024),
            gauge,
            sink,
        );
        (cache, gauge, sink)
    }

    #[test]
    fn a_hit_is_served_without_rebuilding() {
        let (mut cache, _, _) = cache(PressureBand::Normal, Sensitivity::UserData);
        let first = cache
            .get_or_build(&1, 7, || Some(Block::of(64, 0xAA)))
            .expect("built");
        assert!(first.is_cached());
        let second = cache
            .get_or_build(&1, 7, || panic!("must not rebuild a cached entry"))
            .expect("cached");
        assert!(second.is_cached());
        assert_eq!(cache.accounting().hits(), 1);
        assert_eq!(cache.accounting().misses(), 1);
    }

    #[test]
    fn peek_borrows_a_resident_entry_without_building_or_counting_it() {
        let (mut cache, _, _) = cache(PressureBand::Normal, Sensitivity::UserData);
        let _ = cache.get_or_build(&1, 7, || Some(Block::of(64, 0xAA)));
        let hits = cache.accounting().hits();
        let misses = cache.accounting().misses();
        assert_eq!(cache.peek(&1, &7), Some(&Block::of(64, 0xAA)));
        assert_eq!(cache.peek(&1, &8), None, "an absent key builds nothing");
        assert_eq!(cache.accounting().hits(), hits);
        assert_eq!(cache.accounting().misses(), misses);
        assert_eq!(cache.len(), 1);
    }

    #[test]
    fn peek_refuses_a_generation_the_entries_were_not_built_at() {
        let (mut cache, _, _) = cache(PressureBand::Normal, Sensitivity::UserData);
        let _ = cache.get_or_build(&1, 7, || Some(Block::of(64, 0xAA)));
        assert_eq!(cache.peek(&2, &7), None, "a stale entry is never served");
        assert_eq!(cache.len(), 1, "refusing to peek must not evict");
    }

    #[test]
    fn peek_leaves_recency_alone_so_eviction_still_follows_the_builds() {
        let (mut cache, _, _) = cache(PressureBand::Normal, Sensitivity::UserData);
        for key in 0..3u32 {
            let _ = cache.get_or_build(&1, key, || Some(Block::of(1024, 0xFF)));
        }
        assert_eq!(cache.len(), 3);
        for _ in 0..8 {
            assert!(cache.peek(&1, &0).is_some());
        }
        let _ = cache.get_or_build(&1, 3, || Some(Block::of(1024, 0xFF)));
        assert_eq!(
            cache.peek(&1, &0),
            None,
            "peeking must not refresh recency, so the first build is still the first out"
        );
        assert!(cache.peek(&1, &3).is_some());
    }

    #[test]
    fn a_generation_change_drops_every_entry() {
        let (mut cache, _, _) = cache(PressureBand::Normal, Sensitivity::UserData);
        let _ = cache.get_or_build(&1, 7, || Some(Block::of(64, 1)));
        let _ = cache.get_or_build(&1, 8, || Some(Block::of(64, 2)));
        assert_eq!(cache.len(), 2);
        let _ = cache.get_or_build(&2, 9, || Some(Block::of(64, 3)));
        assert_eq!(cache.len(), 1, "the older generation must not survive");
        assert_eq!(cache.accounting().invalidations(), 1);
    }

    #[test]
    fn the_ledger_balances_across_admit_and_release() {
        let (mut cache, _, _) = cache(PressureBand::Normal, Sensitivity::UserData);
        let _ = cache.get_or_build(&1, 7, || Some(Block::of(100, 1)));
        assert_eq!(cache.charged_bytes(), 100 + METADATA);
        cache.invalidate(&7);
        assert_eq!(cache.charged_bytes(), 0);
        assert_eq!(cache.len(), 0);
    }

    #[test]
    fn the_budget_bounds_the_cache_and_evicts_the_oldest_first() {
        let (mut cache, _, _) = cache(PressureBand::Normal, Sensitivity::UserData);
        let hard = CacheBudget::from_backing(64 * 1024).hard();
        // Each entry costs 1 KiB + metadata; the 4 KiB hard limit
        // therefore admits three before the fourth forces a shrink.
        for key in 0..8u32 {
            let _ = cache.get_or_build(&1, key, || Some(Block::of(1024, 0xFF)));
        }
        assert!(cache.charged_bytes() <= hard, "budget must bound the cache");
        assert!(cache.accounting().evictions() > 0);
        // The oldest keys went first: the most recent admission is
        // still resident.
        assert!(cache
            .get_or_build(&1, 7, || panic!("newest entry must still be cached"))
            .is_some());
    }

    #[test]
    fn a_value_larger_than_the_whole_budget_is_served_uncached() {
        let (mut cache, _, _) = cache(PressureBand::Normal, Sensitivity::UserData);
        let served = cache
            .get_or_build(&1, 7, || Some(Block::of(1 << 20, 9)))
            .expect("built");
        assert!(!served.is_cached(), "an oversized value is never retained");
        assert_eq!(served.payload_bytes(), 1 << 20, "still usable");
        drop(served);
        assert_eq!(cache.len(), 0);
        assert_eq!(cache.accounting().refusals(), 1);
    }

    #[test]
    fn mild_pressure_drops_a_disposable_ui_cache_and_refuses_growth() {
        let (mut cache, gauge, _) = cache(PressureBand::Normal, Sensitivity::UserData);
        let _ = cache.get_or_build(&1, 7, || Some(Block::of(256, 1)));
        assert_eq!(cache.len(), 1);

        gauge.report(PressureBand::Mild);
        assert!(cache.enforce_pressure() > 0, "mild pressure must release");
        assert_eq!(cache.len(), 0);
        assert_eq!(cache.charged_bytes(), 0);
        assert_eq!(cache.accounting().pressure_shrinks(), 1);

        let served = cache
            .get_or_build(&1, 8, || Some(Block::of(256, 2)))
            .expect("built");
        assert!(!served.is_cached(), "no growth under pressure");
    }

    #[test]
    fn an_unknown_band_admits_nothing() {
        let gauge: &'static ReportedPressure = Box::leak(Box::new(ReportedPressure::unknown()));
        let sink = leak_sink();
        let mut cache: ReclaimCache<u32, Block, u64> = ReclaimCache::new(
            "test.cache",
            candidate(Sensitivity::UserData),
            CacheBudget::from_backing(64 * 1024),
            gauge,
            sink,
        );
        let served = cache
            .get_or_build(&1, 7, || Some(Block::of(16, 1)))
            .expect("built");
        assert!(!served.is_cached());
    }

    #[test]
    fn a_refused_classification_poisons_the_cache_and_audits_once() {
        let gauge = leak_gauge(PressureBand::Normal);
        let sink = leak_sink();
        let mut refused = candidate(Sensitivity::UserData);
        refused.sensitivity = Some(Sensitivity::CredentialOrKey);
        let mut cache: ReclaimCache<u32, Block, u64> = ReclaimCache::new(
            "test.cache",
            refused,
            CacheBudget::from_backing(64 * 1024),
            gauge,
            sink,
        );
        assert!(cache.poisoned());
        assert_eq!(sink.count(), 1);
        let served = cache
            .get_or_build(&1, 7, || Some(Block::of(16, 1)))
            .expect("still built");
        assert!(!served.is_cached());
        assert_eq!(sink.count(), 1, "the refusal is audited once");
    }

    #[test]
    fn a_failed_build_is_not_remembered() {
        let (mut cache, _, _) = cache(PressureBand::Normal, Sensitivity::UserData);
        assert!(cache.get_or_build(&1, 7, || None).is_none());
        assert_eq!(cache.len(), 0);
        assert!(cache
            .get_or_build(&1, 7, || Some(Block::of(16, 1)))
            .is_some());
    }

    #[test]
    fn teardown_wipes_non_public_entries() {
        let (mut cache, _, _) = cache(PressureBand::Normal, Sensitivity::UserData);
        let _ = cache.get_or_build(&1, 7, || Some(Block::of(64, 0xAB)));
        cache.teardown();
        assert_eq!(cache.len(), 0);
        assert_eq!(cache.charged_bytes(), 0);
        assert_eq!(cache.accounting().teardowns(), 1);
    }

    #[test]
    fn debug_reports_bookkeeping_without_requiring_the_value_to_be_debug() {
        let (mut cache, _, _) = cache(PressureBand::Normal, Sensitivity::UserData);
        let _ = cache.get_or_build(&1, 7, || Some(Block::of(64, 0xAB)));
        // `Block` does not implement `Debug`, and that absence is the point:
        // a consumer's cached value never has to be `Debug` for the cache
        // itself to report its own bookkeeping.
        let rendered = std::format!("{cache:?}");
        assert!(rendered.contains("ReclaimCache"));
        assert!(rendered.contains("entries: 1"));
    }
}
