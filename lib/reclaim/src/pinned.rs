//! Accounting for a **pinned** pool: bytes the reclaim model measures but
//! can never take back (`plans/SMARTRAM.md` section 6.1).
//!
//! Everything else in this crate describes *reclaimable* memory — derived
//! state a cache can drop because its canonical source will rebuild it.
//! Some bounded pools are not like that. A filesystem's dirty block set
//! holds the only copy of bytes the medium does not have yet, so it can be
//! *written out* but never dropped; the same is true of a removable
//! volume's uncommitted-write journal. Such a pool is deliberately not
//! admitted through [`CacheCandidate::classify`](crate::CacheCandidate),
//! whose whole contract is droppability, and it obeys the reserve floor
//! ([`GrowthAllowance::permits_reserve`](crate::GrowthAllowance)) rather
//! than a class ceiling.
//!
//! It still has to be *visible*. TAIRiX has no `/proc/meminfo`, so the
//! System Information cache-ledger export is the only channel through
//! which an operator can see that some of the machine's RAM is held by a
//! filesystem's unwritten data — and a figure nobody can read is a figure
//! nobody can act on. [`PinnedLedger`] therefore samples into the same
//! [`CacheLedgerRecord`] a reclaimable cache does, under the
//! [`CACHE_CLASS_PINNED`] class id, which the per-class reclaim totals
//! drop by construction so unreclaimable bytes can never be counted as
//! headroom.
//!
//! # Why a gauge and not a charge/discharge ledger
//!
//! [`CacheAccounting`](crate::CacheAccounting) is a running, checked
//! ledger because a cache admits and evicts entries one at a time and its
//! books balancing is the property worth failing closed on. A pinned pool
//! instead *knows* its total: the owner holds every byte and can state the
//! figure outright. Adding a second running total to keep in step with the
//! first would be bookkeeping that can drift; a gauge cannot.

use alloc::sync::Arc;
use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use tairix_abi::sysinfo::{CacheLedgerRecord, CACHE_CLASS_PINNED};
use tairix_abi::Errno;

use crate::model::ReclaimOwner;

/// The live figures of one pinned pool.
///
/// Interior-atomic so one instance can be shared
/// ([`Arc`]) with the read-only System Information export while the owning
/// pool keeps updating it. **Mutation is single-writer**: the owner
/// serialises its own updates (it is the only thing that can change the
/// pool), while readers take lock-free per-field snapshots.
#[derive(Debug, Default)]
pub struct PinnedAccounting {
    bytes: AtomicUsize,
    entries: AtomicU64,
    peak_bytes: AtomicUsize,
    released: AtomicU64,
    refusals: AtomicU64,
}

impl PinnedAccounting {
    /// An idle pool.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            bytes: AtomicUsize::new(0),
            entries: AtomicU64::new(0),
            peak_bytes: AtomicUsize::new(0),
            released: AtomicU64::new(0),
            refusals: AtomicU64::new(0),
        }
    }

    /// Publish the pool's current footprint, tracking its high-water mark.
    pub fn set(&self, bytes: usize, entries: u64) {
        self.bytes.store(bytes, Ordering::Relaxed);
        self.entries.store(entries, Ordering::Relaxed);
        if self.peak_bytes.load(Ordering::Relaxed) < bytes {
            self.peak_bytes.store(bytes, Ordering::Relaxed);
        }
    }

    /// Note one pass that wrote the pool out and returned its bytes — the
    /// pinned equivalent of a pressure-forced shrink, and the only way a
    /// pinned pool ever gives memory back.
    pub fn note_released(&self) {
        bump(&self.released);
    }

    /// Note one admission the pool's bound refused, so the owner had to
    /// make room before it could take the bytes.
    pub fn note_refusal(&self) {
        bump(&self.refusals);
    }

    /// Bytes currently pinned.
    #[must_use]
    pub fn bytes(&self) -> usize {
        self.bytes.load(Ordering::Relaxed)
    }

    /// Entries currently pinned.
    #[must_use]
    pub fn entries(&self) -> u64 {
        self.entries.load(Ordering::Relaxed)
    }

    /// The largest footprint the pool has held since it was built.
    #[must_use]
    pub fn peak_bytes(&self) -> usize {
        self.peak_bytes.load(Ordering::Relaxed)
    }

    /// Passes that wrote the pool out and returned its bytes.
    #[must_use]
    pub fn released(&self) -> u64 {
        self.released.load(Ordering::Relaxed)
    }

    /// Admissions the bound refused.
    #[must_use]
    pub fn refusals(&self) -> u64 {
        self.refusals.load(Ordering::Relaxed)
    }
}

/// Saturating increment of one diagnostic counter (single-writer: the
/// owning pool serialises its own updates).
fn bump(counter: &AtomicU64) {
    let value = counter.load(Ordering::Relaxed);
    counter.store(value.saturating_add(1), Ordering::Relaxed);
}

/// One pinned pool's identity plus a shared, read-only handle to its
/// figures — the [`crate::CacheLedger`] of the non-reclaimable side.
///
/// Cloning is cheap and shares the figures: a registry holds a clone while
/// the owning pool keeps updating them.
#[derive(Clone)]
pub struct PinnedLedger {
    label: &'static str,
    owner: ReclaimOwner,
    accounting: Arc<PinnedAccounting>,
}

impl core::fmt::Debug for PinnedLedger {
    /// The identity and the footprint, never what is held: a pinned pool's
    /// bytes are user data, and a ledger describes the pool rather than
    /// revealing its contents.
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("PinnedLedger")
            .field("label", &self.label)
            .field("owner", &self.owner)
            .field("pinned_bytes", &self.accounting.bytes())
            .finish()
    }
}

impl PinnedLedger {
    /// Describe a pinned pool by its label, owner, and shared figures.
    #[must_use]
    pub const fn new(
        label: &'static str,
        owner: ReclaimOwner,
        accounting: Arc<PinnedAccounting>,
    ) -> Self {
        Self {
            label,
            owner,
            accounting,
        }
    }

    /// The pool's stable label.
    #[must_use]
    pub const fn label(&self) -> &'static str {
        self.label
    }

    /// Who is charged for the pool's memory.
    #[must_use]
    pub const fn owner(&self) -> ReclaimOwner {
        self.owner
    }

    /// The shared figures, for a registry that samples them.
    #[must_use]
    pub fn accounting(&self) -> &Arc<PinnedAccounting> {
        &self.accounting
    }

    /// Sample the figures into the wire record the System Information API
    /// carries, under [`CACHE_CLASS_PINNED`].
    ///
    /// The sample is lock-free and per-field, so a record may straddle an
    /// in-flight update; each figure is individually untorn, which is the
    /// sampling semantics every live gauge has. The record's origin is
    /// left unset — whoever publishes it stamps that.
    ///
    /// A pinned pool has no hit ratio to report (nothing looks an entry
    /// up; the owner holds them all), so those columns stay zero rather
    /// than carrying an invented denominator. The release count travels as
    /// `pressure_shrinks`: writing the pool out *is* its shrink pass, and
    /// it is the figure an operator reads to see the bound biting.
    ///
    /// # Errors
    ///
    /// [`Errno::LengthOutOfRange`] or [`Errno::OutOfRange`] if the label
    /// is empty, longer than the wire record admits, or not printable
    /// ASCII — refused here rather than shown as a broken row.
    pub fn to_record(&self) -> Result<CacheLedgerRecord, Errno> {
        let (owner_kind, owner_id) = self.owner.wire();
        let mut record = CacheLedgerRecord::new(
            self.label.as_bytes(),
            owner_kind,
            owner_id,
            CACHE_CLASS_PINNED,
        )?;
        record.payload_bytes = self.accounting.bytes() as u64;
        record.entries = self.accounting.entries();
        record.refusals = self.accounting.refusals();
        record.pressure_shrinks = self.accounting.released();
        Ok(record)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tairix_abi::sysinfo::{
        cache_class_name, fold_cache_ledgers, CacheLedgerOrigin, RECLAIM_CLASS_COUNT,
    };

    fn ledger() -> PinnedLedger {
        PinnedLedger::new(
            "arxfs.dirty",
            ReclaimOwner::FilesystemVolume { volume: 7 },
            Arc::new(PinnedAccounting::new()),
        )
    }

    #[test]
    fn a_record_carries_the_identity_and_the_live_footprint() {
        let entry = ledger();
        entry.accounting().set(8192, 16);
        entry.accounting().note_released();
        entry.accounting().note_refusal();
        entry.accounting().note_refusal();

        let record = entry.to_record().expect("a printable label encodes");
        assert_eq!(record.label(), "arxfs.dirty");
        assert_eq!(record.owner_id, 7);
        assert_eq!(record.class, CACHE_CLASS_PINNED);
        assert_eq!(record.payload_bytes, 8192);
        assert_eq!(record.entries, 16);
        assert_eq!(record.pressure_shrinks, 1);
        assert_eq!(record.refusals, 2);
        assert_eq!(record.origin, CacheLedgerOrigin::Unset);
        assert_eq!(
            (record.hits, record.misses, record.metadata_bytes),
            (0, 0, 0),
            "a pinned pool invents no hit ratio and no bookkeeping split"
        );
    }

    #[test]
    fn the_footprint_is_a_gauge_and_the_peak_a_high_water_mark() {
        let acct = PinnedAccounting::new();
        acct.set(4096, 8);
        acct.set(1024, 2);
        assert_eq!((acct.bytes(), acct.entries()), (1024, 2));
        assert_eq!(acct.peak_bytes(), 4096, "the peak survives the fall");
    }

    #[test]
    fn pinned_bytes_never_enter_a_reclaim_class_total() {
        // The whole reason the pinned class sits past the reclaim classes:
        // a reclaim decision reading these totals must never count memory
        // that can only be written out, never dropped.
        let entry = ledger();
        entry.accounting().set(1 << 20, 4);
        let totals = fold_cache_ledgers(&[entry.to_record().expect("encodes")]);
        assert_eq!(totals.len(), RECLAIM_CLASS_COUNT);
        assert!(
            totals
                .iter()
                .all(|total| total.payload_bytes == 0 && total.entries == 0),
            "a pinned row contributes to no reclaim class"
        );
    }

    #[test]
    fn the_pinned_class_renders_under_its_own_name() {
        assert_eq!(cache_class_name(CACHE_CLASS_PINNED), Some("pinned"));
        assert_eq!(cache_class_name(CACHE_CLASS_PINNED + 1), None);
    }

    #[test]
    fn an_unrenderable_label_is_refused_rather_than_shown_broken() {
        let broken = PinnedLedger::new(
            "arxfs\u{1b}[2Jdirty",
            ReclaimOwner::FilesystemVolume { volume: 1 },
            Arc::new(PinnedAccounting::new()),
        );
        assert_eq!(broken.to_record(), Err(Errno::OutOfRange));
    }
}
