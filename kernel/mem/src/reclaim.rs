//! Reclaimable-memory classification, budget, and accounting
//! (`plans/SMARTRAM.md`).
//!
//! A reclaimable cache holds *derived* state — data that can always be
//! rebuilt from its canonical source — so the memory it occupies is a
//! loan the VM can call in at any time. This module is the one
//! definition of how such a cache is classed, bounded, and accounted:
//! the consumer (today the filesystem cache in `kernel/core::fs`)
//! charges every entry here, and reclaim decisions read these numbers
//! rather than re-deriving their own.
//!
//! # Classes
//!
//! Each entry belongs to exactly one [`ReclaimClass`]. Classes order
//! reclaim: under pressure the cheaper-to-rebuild class is evicted
//! first (`plans/SMARTRAM.md` section 7, matching
//! `plans/SWAPSWAPSWAP.md` section 6 — clean file cache is reclaimed
//! before anything more expensive).
//!
//! # Budgets and hysteresis
//!
//! A [`CacheBudget`] is derived from the size of the backing resource
//! (the kernel heap arena), never a free-standing magic number. Growth
//! and shrink use two watermarks so a cache does not oscillate on one
//! threshold: an insert that would push usage past the *hard* limit
//! forces eviction down to the *low* watermark.
//!
//! # Fail-closed accounting
//!
//! [`CacheAccounting`] refuses overflow and underflow with a typed
//! [`AccountingError`] instead of wrapping or saturating: a cache whose
//! books stop balancing is a defect, and its caller drops the entry
//! rather than corrupting the ledger.

/// The reclaim class of a cached entry (`plans/SMARTRAM.md` section 5).
///
/// Only classes with a live in-tree consumer exist; further classes are
/// added with the stage that consumes them, never ahead of it.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum ReclaimClass {
    /// Clean, rebuildable file *data* re-readable from the volume.
    /// Cheapest to rebuild (one bounded device read), so it is evicted
    /// first.
    CleanFileData,
    /// Filesystem *metadata* — stat records, lookup results, directory
    /// entries, security records. Small, hot, and rebuilt by a
    /// multi-step tree walk, so it outlives file data under pressure.
    FsMetadata,
}

impl ReclaimClass {
    /// Eviction order under pressure: lower is reclaimed first.
    ///
    /// Deterministic for equal inputs: the ordering is a pure function
    /// of the class.
    #[must_use]
    pub const fn reclaim_priority(self) -> u8 {
        match self {
            Self::CleanFileData => 0,
            Self::FsMetadata => 1,
        }
    }
}

/// Why a cache refused to admit or account an entry.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum AccountingError {
    /// Charging the entry would overflow the ledger.
    Overflow,
    /// Discharging the entry would underflow the ledger — the books no
    /// longer balance, which is a caller defect surfaced loudly.
    Underflow,
}

/// The grow/shrink bounds of one bounded cache, in bytes.
///
/// `hard` is the ceiling an insert may never push usage past; `low` is
/// the watermark a forced shrink evicts down to. Keeping them apart is
/// the hysteresis `plans/SMARTRAM.md` section 7 requires: growth up to
/// `hard`, shrink down to `low`, never both on one threshold.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct CacheBudget {
    hard: usize,
    low: usize,
}

/// The backing-resource fraction a filesystem cache may occupy.
///
/// One volume's cache is capped at 1/16 of the kernel heap arena: with
/// the fixed 64 MiB heap this is 4 MiB per volume, so the two boot
/// volumes together stay under 1/8 of the heap and cache growth can
/// never be the cause of kernel-heap exhaustion (`plans/SMARTRAM.md`
/// section 7 — reserves are preserved by construction).
const BACKING_DIVISOR: usize = 16;

/// The shrink watermark as a fraction of the hard limit: a forced
/// shrink evicts down to 3/4 of `hard`, so post-shrink inserts have
/// real headroom before the next eviction pass.
const LOW_NUMERATOR: usize = 3;
const LOW_DIVISOR: usize = 4;

impl CacheBudget {
    /// Derive the budget for one cache from the byte size of the
    /// resource backing it (the kernel heap arena), per the documented
    /// policy fractions. A tiny backing yields a tiny budget; zero
    /// yields zero, which admits nothing (fail closed, never a panic).
    #[must_use]
    pub const fn from_backing(backing_bytes: usize) -> Self {
        let hard = backing_bytes / BACKING_DIVISOR;
        Self {
            hard,
            low: hard / LOW_DIVISOR * LOW_NUMERATOR,
        }
    }

    /// The ceiling an insert may never push usage past.
    #[must_use]
    pub const fn hard(self) -> usize {
        self.hard
    }

    /// The watermark a forced shrink evicts down to.
    #[must_use]
    pub const fn low(self) -> usize {
        self.low
    }
}

/// The running byte ledger and event counters of one bounded cache.
///
/// Bytes are kept per [`ReclaimClass`]; every mutation is
/// checked-arithmetic and fails closed with [`AccountingError`] rather
/// than wrapping. Event counters saturate: they are diagnostics, and a
/// saturated diagnostic is still truthful about "a very large number".
#[derive(Debug, Default)]
pub struct CacheAccounting {
    data_bytes: usize,
    metadata_bytes: usize,
    hits: u64,
    misses: u64,
    insertions: u64,
    invalidations: u64,
    evictions: u64,
    refusals: u64,
}

impl CacheAccounting {
    /// An empty ledger.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            data_bytes: 0,
            metadata_bytes: 0,
            hits: 0,
            misses: 0,
            insertions: 0,
            invalidations: 0,
            evictions: 0,
            refusals: 0,
        }
    }

    /// Total bytes currently charged across all classes.
    #[must_use]
    pub const fn total_bytes(&self) -> usize {
        // Per-class charges are individually checked, and both fit the
        // budget ceiling, so their sum cannot overflow in practice;
        // saturating keeps the diagnostic truthful even if it could.
        self.data_bytes.saturating_add(self.metadata_bytes)
    }

    /// Bytes currently charged to `class`.
    #[must_use]
    pub const fn class_bytes(&self, class: ReclaimClass) -> usize {
        match class {
            ReclaimClass::CleanFileData => self.data_bytes,
            ReclaimClass::FsMetadata => self.metadata_bytes,
        }
    }

    /// Charge `bytes` to `class` for an admitted entry.
    ///
    /// # Errors
    ///
    /// [`AccountingError::Overflow`] if the ledger cannot represent the
    /// new total; nothing is charged.
    pub fn charge(&mut self, class: ReclaimClass, bytes: usize) -> Result<(), AccountingError> {
        let slot = match class {
            ReclaimClass::CleanFileData => &mut self.data_bytes,
            ReclaimClass::FsMetadata => &mut self.metadata_bytes,
        };
        *slot = slot.checked_add(bytes).ok_or(AccountingError::Overflow)?;
        self.insertions = self.insertions.saturating_add(1);
        Ok(())
    }

    /// Discharge `bytes` from `class` for a removed entry.
    ///
    /// # Errors
    ///
    /// [`AccountingError::Underflow`] if more is discharged than was
    /// ever charged — the books no longer balance; nothing is changed.
    pub fn discharge(&mut self, class: ReclaimClass, bytes: usize) -> Result<(), AccountingError> {
        let slot = match class {
            ReclaimClass::CleanFileData => &mut self.data_bytes,
            ReclaimClass::FsMetadata => &mut self.metadata_bytes,
        };
        *slot = slot.checked_sub(bytes).ok_or(AccountingError::Underflow)?;
        Ok(())
    }

    /// Reset the byte ledger to empty, keeping the event counters.
    ///
    /// This is the fail-closed companion of a whole-cache purge: after
    /// every entry has been dropped the ledger is empty by definition,
    /// and on the poison path (a detected charge/discharge imbalance)
    /// it is the only truthful value left. Never a substitute for
    /// per-entry [`discharge`](Self::discharge) in normal operation.
    pub fn zero_ledger(&mut self) {
        self.data_bytes = 0;
        self.metadata_bytes = 0;
    }

    /// Record a lookup served from the cache.
    pub fn record_hit(&mut self) {
        self.hits = self.hits.saturating_add(1);
    }

    /// Record a lookup that fell through to the canonical source.
    pub fn record_miss(&mut self) {
        self.misses = self.misses.saturating_add(1);
    }

    /// Record an entry dropped because its source changed.
    pub fn record_invalidation(&mut self) {
        self.invalidations = self.invalidations.saturating_add(1);
    }

    /// Record an entry evicted for space.
    pub fn record_eviction(&mut self) {
        self.evictions = self.evictions.saturating_add(1);
    }

    /// Record an entry refused admission (over-bound, unaccountable, or
    /// allocation failure).
    pub fn record_refusal(&mut self) {
        self.refusals = self.refusals.saturating_add(1);
    }

    /// Lookups served from the cache.
    #[must_use]
    pub const fn hits(&self) -> u64 {
        self.hits
    }

    /// Lookups that fell through to the canonical source.
    #[must_use]
    pub const fn misses(&self) -> u64 {
        self.misses
    }

    /// Entries admitted.
    #[must_use]
    pub const fn insertions(&self) -> u64 {
        self.insertions
    }

    /// Entries dropped because their source changed.
    #[must_use]
    pub const fn invalidations(&self) -> u64 {
        self.invalidations
    }

    /// Entries evicted for space.
    #[must_use]
    pub const fn evictions(&self) -> u64 {
        self.evictions
    }

    /// Entries refused admission.
    #[must_use]
    pub const fn refusals(&self) -> u64 {
        self.refusals
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_class_maps_to_a_distinct_reclaim_priority() {
        assert!(
            ReclaimClass::CleanFileData.reclaim_priority()
                < ReclaimClass::FsMetadata.reclaim_priority()
        );
    }

    #[test]
    fn budget_is_derived_from_the_backing_with_hysteresis() {
        let budget = CacheBudget::from_backing(64 * 1024 * 1024);
        assert_eq!(budget.hard(), 4 * 1024 * 1024);
        assert_eq!(budget.low(), 3 * 1024 * 1024);
        assert!(budget.low() < budget.hard());
    }

    #[test]
    fn zero_backing_admits_nothing() {
        let budget = CacheBudget::from_backing(0);
        assert_eq!(budget.hard(), 0);
        assert_eq!(budget.low(), 0);
    }

    #[test]
    fn charge_and_discharge_balance_per_class() {
        let mut acct = CacheAccounting::new();
        acct.charge(ReclaimClass::CleanFileData, 4096)
            .expect("charges");
        acct.charge(ReclaimClass::FsMetadata, 128).expect("charges");
        assert_eq!(acct.class_bytes(ReclaimClass::CleanFileData), 4096);
        assert_eq!(acct.class_bytes(ReclaimClass::FsMetadata), 128);
        assert_eq!(acct.total_bytes(), 4224);
        acct.discharge(ReclaimClass::CleanFileData, 4096)
            .expect("discharges");
        assert_eq!(acct.total_bytes(), 128);
    }

    #[test]
    fn overflow_is_refused_and_charges_nothing() {
        let mut acct = CacheAccounting::new();
        acct.charge(ReclaimClass::FsMetadata, usize::MAX)
            .expect("charges");
        assert_eq!(
            acct.charge(ReclaimClass::FsMetadata, 1),
            Err(AccountingError::Overflow)
        );
        assert_eq!(acct.class_bytes(ReclaimClass::FsMetadata), usize::MAX);
    }

    #[test]
    fn underflow_is_refused_and_discharges_nothing() {
        let mut acct = CacheAccounting::new();
        acct.charge(ReclaimClass::CleanFileData, 10)
            .expect("charges");
        assert_eq!(
            acct.discharge(ReclaimClass::CleanFileData, 11),
            Err(AccountingError::Underflow)
        );
        assert_eq!(acct.class_bytes(ReclaimClass::CleanFileData), 10);
    }

    #[test]
    fn event_counters_track_each_path() {
        let mut acct = CacheAccounting::new();
        acct.record_hit();
        acct.record_miss();
        acct.record_invalidation();
        acct.record_eviction();
        acct.record_refusal();
        assert_eq!(acct.hits(), 1);
        assert_eq!(acct.misses(), 1);
        assert_eq!(acct.invalidations(), 1);
        assert_eq!(acct.evictions(), 1);
        assert_eq!(acct.refusals(), 1);
        assert_eq!(acct.insertions(), 0);
    }
}
