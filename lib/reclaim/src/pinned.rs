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
//! # One machine, many pools
//!
//! A pool's own budget is a share of the machine's RAM, so the *sum* of the
//! budgets of every pinned pool on a machine is a multiple of what the
//! machine has: eight mounted volumes each entitled to a sixteenth of RAM
//! would between them pin half of it in memory nothing can reclaim. Pools
//! that must not do that draw on one [`PinnedShare`] instead, which carries
//! the live total across them and how many are drawing, so each can bound
//! itself by its share of one machine-wide ceiling rather than by its own
//! slice of RAM.
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

/// The machine-wide total several pinned pools draw on, and how many of them
/// are drawing.
///
/// It carries no ceiling: what the machine may pin in total is derived from
/// discovered RAM by whoever owns the policy (for a filesystem's dirty set,
/// the same [`CacheBudget`](crate::CacheBudget) derivation a reclaimable
/// cache uses). What a shared figure has to supply is the two things a pool
/// cannot know alone — what its siblings are holding, and how many of them
/// there are — so one instance per machine, installed by the host on every
/// pool that shares the ceiling.
///
/// Updated as a side effect of each pool's own [`PinnedAccounting::set`], so
/// there is no second total to keep in step: a pool publishes its footprint
/// exactly once and the share follows.
#[derive(Debug, Default)]
pub struct PinnedShare {
    bytes: AtomicUsize,
    peak_bytes: AtomicUsize,
    drawing: AtomicUsize,
}

impl PinnedShare {
    /// A share nothing is drawing on yet.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            bytes: AtomicUsize::new(0),
            peak_bytes: AtomicUsize::new(0),
            drawing: AtomicUsize::new(0),
        }
    }

    /// Bytes pinned across every pool drawing on the share.
    #[must_use]
    pub fn bytes(&self) -> usize {
        self.bytes.load(Ordering::Relaxed)
    }

    /// The most the machine has held across every pool at once.
    ///
    /// A sum of the pools' own high-water marks would over-count peaks they
    /// never reached together, so the figure a machine-wide bound is judged
    /// by has to be taken here, as the total moves.
    #[must_use]
    pub fn peak_bytes(&self) -> usize {
        self.peak_bytes.load(Ordering::Relaxed)
    }

    /// Pools currently holding pinned bytes.
    #[must_use]
    pub fn drawing_pools(&self) -> usize {
        self.drawing.load(Ordering::Relaxed)
    }

    /// Fold one pool's move from `was` to `now` bytes into the share.
    fn fold(&self, was: usize, now: usize) {
        let total = if now >= was {
            add(&self.bytes, now - was)
        } else {
            take(&self.bytes, was - now)
        };
        self.peak_bytes.fetch_max(total, Ordering::Relaxed);
        match (was, now) {
            (0, 1..) => {
                add(&self.drawing, 1);
            }
            (1.., 0) => {
                take(&self.drawing, 1);
            }
            _ => {}
        }
    }
}

/// Saturating add on a counter several pools update, returning the new value.
/// Saturating rather than wrapping because an implausible total that wrapped
/// into a small one would admit unbounded growth.
fn add(counter: &AtomicUsize, by: usize) -> usize {
    held(
        counter.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |read| {
            Some(read.saturating_add(by))
        }),
    )
    .saturating_add(by)
}

/// Saturating subtract, for the same reason.
fn take(counter: &AtomicUsize, by: usize) -> usize {
    held(
        counter.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |read| {
            Some(read.saturating_sub(by))
        }),
    )
    .saturating_sub(by)
}

/// The value an always-`Some` update read. The refusal arm cannot arise, and
/// carries the same reading, so both are the value.
const fn held(update: Result<usize, usize>) -> usize {
    match update {
        Ok(read) | Err(read) => read,
    }
}

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
    /// The machine-wide total this pool draws on, where it shares one with
    /// its siblings. `None` for a pool that is the only claim on its
    /// ceiling (a host tool, a unit test).
    share: Option<&'static PinnedShare>,
}

impl PinnedAccounting {
    /// An idle pool that shares its ceiling with nothing.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            bytes: AtomicUsize::new(0),
            entries: AtomicU64::new(0),
            peak_bytes: AtomicUsize::new(0),
            released: AtomicU64::new(0),
            refusals: AtomicU64::new(0),
            share: None,
        }
    }

    /// An idle pool drawing on `share`, the machine-wide total its siblings
    /// draw on too.
    #[must_use]
    pub const fn within(share: &'static PinnedShare) -> Self {
        Self {
            bytes: AtomicUsize::new(0),
            entries: AtomicU64::new(0),
            peak_bytes: AtomicUsize::new(0),
            released: AtomicU64::new(0),
            refusals: AtomicU64::new(0),
            share: Some(share),
        }
    }

    /// Publish the pool's current footprint, tracking its high-water mark and
    /// folding the move into the machine-wide share.
    pub fn set(&self, bytes: usize, entries: u64) {
        let was = self.bytes.swap(bytes, Ordering::Relaxed);
        self.entries.store(entries, Ordering::Relaxed);
        if self.peak_bytes.load(Ordering::Relaxed) < bytes {
            self.peak_bytes.store(bytes, Ordering::Relaxed);
        }
        if let Some(share) = self.share {
            share.fold(was, bytes);
        }
    }

    /// Bytes pinned by every pool *other* than this one that draws on the
    /// same machine-wide share; zero for a pool that shares nothing.
    ///
    /// This is the figure a pool's own ceiling is reduced by, so that what
    /// the machine holds in total stays inside one derived limit however
    /// many pools there are.
    #[must_use]
    pub fn other_bytes(&self) -> usize {
        self.share
            .map_or(0, |share| share.bytes().saturating_sub(self.bytes()))
    }

    /// The divisor a pool's equal share of the machine-wide ceiling is taken
    /// over: the pools currently holding bytes, counting this one whether it
    /// holds any yet or not, and never zero.
    ///
    /// A pool about to take its first bytes has to count itself, or the last
    /// pool to wake would compute a share the pools before it have already
    /// spent. A pool holding nothing counts for nothing, so a machine whose
    /// other pools are empty leaves the whole ceiling to the one that is
    /// filling.
    #[must_use]
    pub fn drawing_pools(&self) -> usize {
        let Some(share) = self.share else {
            return 1;
        };
        let drawing = share.drawing_pools();
        if self.bytes() > 0 {
            drawing.max(1)
        } else {
            drawing.saturating_add(1)
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
    fn a_share_carries_the_total_across_its_pools_and_who_is_drawing() {
        static SHARE: PinnedShare = PinnedShare::new();
        let one = PinnedAccounting::within(&SHARE);
        let two = PinnedAccounting::within(&SHARE);
        assert_eq!((SHARE.bytes(), SHARE.drawing_pools()), (0, 0));

        one.set(4096, 1);
        two.set(1024, 1);
        assert_eq!((SHARE.bytes(), SHARE.drawing_pools()), (5120, 2));
        assert_eq!(one.other_bytes(), 1024, "what the sibling holds");
        assert_eq!(two.other_bytes(), 4096);

        // The peak is the machine's, taken as the total moves: a sum of the
        // pools' own peaks would count a moment they never shared.
        one.set(0, 0);
        two.set(2048, 2);
        assert_eq!((SHARE.bytes(), SHARE.drawing_pools()), (2048, 1));
        assert_eq!(SHARE.peak_bytes(), 5120);
        assert_eq!(one.peak_bytes(), 4096);

        two.set(0, 0);
        assert_eq!((SHARE.bytes(), SHARE.drawing_pools()), (0, 0));
    }

    #[test]
    fn the_share_divisor_counts_a_pool_about_to_draw() {
        // A pool taking its first bytes has to count itself, or the last pool
        // to wake would compute a share the others have already spent.
        static SHARE: PinnedShare = PinnedShare::new();
        let idle = PinnedAccounting::within(&SHARE);
        let busy = PinnedAccounting::within(&SHARE);
        assert_eq!(idle.drawing_pools(), 1, "alone, and holding nothing");

        busy.set(512, 1);
        assert_eq!(busy.drawing_pools(), 1, "the only one holding anything");
        assert_eq!(idle.drawing_pools(), 2, "itself and the one already there");

        // A pool that shares nothing is its own whole machine.
        let alone = PinnedAccounting::new();
        alone.set(512, 1);
        assert_eq!((alone.drawing_pools(), alone.other_bytes()), (1, 0));
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
