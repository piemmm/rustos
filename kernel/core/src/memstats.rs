//! Late-installed registry of the live memory-statistics sources the
//! System Information introspection feed exports
//! (`plans/STRESSTEST.md` ST1).
//!
//! The figures the `MEMORY_PRESSURE`, `CACHE_LEDGERS`, and
//! `RAMZIP_STATS` queries report live in places that only exist once
//! the boot path has created them: the system pressure gauge is built
//! over the frame allocator, each reclaimable cache's ledger is born
//! with the cache (per mounted volume, per installed launch cache), and
//! the `ramzip` tier is the one process-global compressed-memory pool
//! the boot path installs once the CSPRNG is seeded (its stats feed is
//! registered by [`install_global_ramzip_stats`]). This registry is the
//! one arch-neutral rendezvous
//! between those producers and the read-only export in
//! [`crate::introspect_source`] — the same late-install pattern as the
//! unlock-published user database.
//!
//! Every registered ledger carries its cache's identity as well as its
//! counters, so the export is one row *per cache* — plus one row per
//! registered **pinned** pool ([`MemStats::register_pinned_ledger`]),
//! memory this kernel measures but can never reclaim, kept out of the
//! per-class reclaim totals so no reclaim decision can mistake it for
//! headroom
//! ([`MemStats::cache_ledger_records`]) — the whole of what this kernel
//! publishes about reclaimable caches. Folding those rows into per-class
//! totals for display belongs to the client that also holds the caches
//! processes report about themselves, which is a sum this kernel cannot
//! measure. The one per-class total kept here
//! ([`MemStats::reclaim_class_stats`]) is therefore not an export at all:
//! it is the kernel's own input to [`MemStats::ramzip_reclaimable_residue`],
//! the reclaim decision over when `ramzip` may start compressing anonymous
//! pages out.
//!
//! Only *this kernel's own* caches ever land here — a userland process's
//! self-reported figures for its own reclaimable caches live in a
//! separate, userland-owned registry, never in this one, so a process can
//! never present its own numbers as kernel-measured or steer that reclaim
//! decision with them.
//!
//! Everything here is observation-only: registering a ledger grants the
//! export nothing but lock-free reads of saturating diagnostics, and the
//! registry never mutates a producer's state (the one deliberate
//! exception: reading the pressure gauge takes a fresh sample, which is
//! how every consumer of the gauge reads it).

use alloc::boxed::Box;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicUsize, Ordering};

use tairix_abi::sysinfo::{CacheLedgerOrigin, CacheLedgerRecord, RamzipStats};
use tairix_reclaim::{
    BandObserver, CacheLedger, FreeMemorySource, MemoryPressure, PinnedLedger, PressureBand,
    ReclaimClass, ReclaimClassStats,
};
use tairix_sync::RwLock;

/// Turns a published band change into a deferred wake of every task
/// parked on a memory-pressure wait-set member.
///
/// The gauge samples itself from wherever memory is being spent, so this
/// hook can fire inside the frame allocator, a demand fault, or a
/// direct-reclaim sweep — contexts that may already hold the very locks a
/// wake would need. It therefore only flags the wait-queue; the real
/// unpark happens at the next dispatcher-context drain.
struct PressureBandWake;

impl BandObserver for PressureBandWake {
    fn band_changed(&self, band: PressureBand) {
        let _ = band;
        crate::waitq::pressure_wake();
    }
}

/// The one observer installed on the system gauge.
static PRESSURE_BAND_WAKE: PressureBandWake = PressureBandWake;

/// Discovered physical-RAM size, in bytes, published once at boot — the
/// backing every reclaimable cache derives its byte budget from
/// (`CacheBudget::from_backing`). `0` means "not yet published".
///
/// The kernel heap is now growable, so `tairix_kalloc::HEAP_BYTES` is only
/// the *bootstrap* size, no longer the memory a cache should size itself
/// against; a cache sized to the bootstrap slab would be a fixed ceiling
/// that wastes RAM on a large machine and over-commits a small one. This
/// value lets every cache scale its budget with the RAM the machine
/// actually has (a proportional policy from one derivation), the
/// growable-capacity discipline the charter requires.
static CACHE_BACKING_BYTES: AtomicUsize = AtomicUsize::new(0);

/// Publish the discovered physical-RAM size the cache budgets scale
/// against. Called once from the boot path after the frame allocator
/// exists, with `usable_frames * PAGE_SIZE`.
pub fn set_cache_backing_bytes(bytes: usize) {
    CACHE_BACKING_BYTES.store(bytes, Ordering::Release);
}

/// The byte size a reclaimable cache derives its budget from
/// (`CacheBudget::from_backing`): the discovered physical RAM once the
/// boot path has published it, falling back to the bootstrap heap size
/// before then (host tests, and the window before
/// [`set_cache_backing_bytes`] runs) so a cache built early still gets a
/// sane, non-zero budget.
#[must_use]
pub fn cache_backing_bytes() -> usize {
    match CACHE_BACKING_BYTES.load(Ordering::Acquire) {
        0 => tairix_kalloc::HEAP_BYTES,
        bytes => bytes,
    }
}

/// A live `ramzip` tier's stats feed.
///
/// The production source ([`install_global_ramzip_stats`]) reads the one
/// process-global tier, so `RAMZIP_STATS` reports its real figures the
/// moment the boot path brings the tier online (and an idle all-zero
/// snapshot before then). Counters only — never page contents or key
/// material.
pub trait RamzipStatsSource: Sync {
    /// Snapshot the tier's exported counters.
    fn stats(&self) -> RamzipStats;
}

/// The registry of live memory-statistics sources.
///
/// A plain instance type (the production kernel uses the one
/// [`MEM_STATS`] static; tests build their own instance) holding the
/// system pressure gauge slot, the registered cache ledgers, and the
/// `ramzip` stats slot.
pub struct MemStats {
    /// The one system memory-pressure gauge, created on first request
    /// over the frame allocator and shared by every consumer (the
    /// volume caches, the launch cache, and the export) so band
    /// hysteresis and transition counters have a single history.
    pressure: RwLock<Option<&'static MemoryPressure>>,
    /// Every registered live cache's identity plus a shared handle to its
    /// ledger. Growable (one per mounted volume cache plus the boot
    /// singletons), never a fixed ceiling; a ledger registered here lives
    /// for the boot (the cloned [`CacheLedger`] keeps a torn-down cache's
    /// final, zeroed books readable). Registration order is preserved, so
    /// [`Self::cache_ledger_records`] pages a stable list.
    ledgers: RwLock<Vec<CacheLedger>>,
    /// Every registered **pinned** pool: memory the reclaim model
    /// measures but can never take (a mounted volume's unwritten blocks).
    /// Held apart from [`Self::ledgers`] because it must never reach
    /// [`Self::reclaim_class_stats`] — a reclaim decision that counted
    /// unwritable-away bytes as reclaimable headroom would stall reclaim
    /// waiting for memory nothing can free. Growable, registration order
    /// preserved, and appended after the cache rows so the paged list is
    /// stable.
    pinned: RwLock<Vec<PinnedLedger>>,
    /// The live `ramzip` tier, once one registers.
    ramzip: RwLock<Option<&'static (dyn RamzipStatsSource + 'static)>>,
}

/// The production registry the boot path and the introspection export
/// share.
pub static MEM_STATS: MemStats = MemStats::new();

impl MemStats {
    /// An empty registry.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            pressure: RwLock::new(None),
            ledgers: RwLock::new(Vec::new()),
            pinned: RwLock::new(Vec::new()),
            ramzip: RwLock::new(None),
        }
    }

    /// The one system pressure gauge over `backing`, created on first
    /// call and returned unchanged on every later one — whichever boot
    /// or export path asks first, exactly one gauge ever exists, so its
    /// hysteresis state and per-band transition counters are the single
    /// system history.
    pub fn system_pressure(
        &self,
        backing: &'static (dyn FreeMemorySource + 'static),
    ) -> &'static MemoryPressure {
        if let Some(gauge) = *self.pressure.read() {
            return gauge;
        }
        let mut slot = self.pressure.write();
        if let Some(gauge) = *slot {
            return gauge;
        }
        // One boot-lifetime leak, deliberate: the gauge is shared as
        // `&'static` by every cache for the life of the kernel.
        let gauge: &'static MemoryPressure = Box::leak(Box::new(
            MemoryPressure::over(backing).observed_by(&PRESSURE_BAND_WAKE),
        ));
        *slot = Some(gauge);
        gauge
    }

    /// The band the gauge has published, without taking a reading, or
    /// [`PressureBand::Normal`] before boot brings the gauge online.
    ///
    /// A peek, deliberately: it is read on the wait-set readiness scan
    /// and by the ungated band query, neither of which should be able to
    /// make an unprivileged caller drive a free-memory reading. The band
    /// is refreshed by whoever actually spends memory (a cache
    /// operation, a demand fault, a reclaim sweep), which is the only
    /// moment it can meaningfully have moved.
    ///
    /// Before the gauge exists there is no measured state to report and
    /// nothing has yet been able to consume memory through it, so the
    /// shallowest band is the truthful answer rather than a guess.
    #[must_use]
    pub fn published_band(&self) -> PressureBand {
        self.current_pressure()
            .map_or(PressureBand::Normal, MemoryPressure::band)
    }

    /// The system pressure gauge if one has been created, or `None`
    /// before the first [`Self::system_pressure`] call brings it online.
    ///
    /// Unlike [`Self::system_pressure`] this never *creates* the gauge —
    /// it has no backing to derive thresholds from — so a reader on a
    /// path that has no gauge to hand (the demand-fault direct-reclaim
    /// step) can consult the shared gauge if one exists and simply do
    /// nothing before boot wires it (fail closed, never a fabricated
    /// gauge over a guessed backing).
    #[must_use]
    pub fn current_pressure(&self) -> Option<&'static MemoryPressure> {
        *self.pressure.read()
    }

    /// The reclaimable-cache residue the `ramzip` compress-out handoff
    /// waits to drain first: the clean file-data plus transform-cache
    /// bytes (payload and per-entry metadata) still resident across every
    /// registered ledger.
    ///
    /// This is the `clean_and_transform_bytes` figure
    /// [`tairix_kernel_mem::ramzip_handoff`] gates on — reconstructable
    /// clean cache and expensive-transform cache are always cheaper to
    /// drop than encrypted compressed anonymous storage, so compression
    /// holds until they are gone. Summed saturating (the figures are
    /// live gauges); an empty registry truthfully reports zero.
    #[must_use]
    pub fn ramzip_reclaimable_residue(&self) -> usize {
        let clean = self.reclaim_class_stats(ReclaimClass::CleanFileData);
        let transform = self.reclaim_class_stats(ReclaimClass::TransformCache);
        clean
            .payload_bytes
            .saturating_add(clean.metadata_bytes)
            .saturating_add(transform.payload_bytes)
            .saturating_add(transform.metadata_bytes)
            .try_into()
            .unwrap_or(usize::MAX)
    }

    /// Register a live cache's ledger for the reclaim and per-cache
    /// exports.
    ///
    /// Called by the production construction sites (a mounted volume's
    /// filesystem cache, the block/transform caches, the launch cache);
    /// unit-test caches simply never register.
    pub fn register_ledger(&self, ledger: CacheLedger) {
        self.ledgers.write().push(ledger);
    }

    /// Install the live `ramzip` tier's stats feed. First install wins.
    pub fn install_ramzip(&self, source: &'static (dyn RamzipStatsSource + 'static)) {
        let mut slot = self.ramzip.write();
        if slot.is_none() {
            *slot = Some(source);
        }
    }

    /// Aggregate `class`'s figures across every ledger **this kernel
    /// measures itself**.
    ///
    /// This is why the aggregate still exists once the per-cache export
    /// below exists: [`Self::ramzip_reclaimable_residue`] feeds it into a
    /// real kernel reclaim decision (when `ramzip` may compress out
    /// anonymous pages), and a decision like that must never be steerable
    /// by a figure a process reported about itself. Summing only
    /// registered ledgers — every one of them kernel-measured, since
    /// nothing in this crate registers a self-reported one — keeps that
    /// true by construction; a self-reported registry, wherever it lives,
    /// must stay out of this sum.
    ///
    /// Each ledger contributes its **own** class only, and that is exactly
    /// the identity: a pool charges nothing but the class its ledger
    /// declares, so selecting a ledger's own class loses none of its
    /// figures. What the selection does rule out is double counting. A
    /// cache holding several classified pools registers one ledger per
    /// pool over one *shared*
    /// [`CacheAccounting`](tairix_reclaim::CacheAccounting), so reading
    /// every ledger's figures for every class would read that one shared
    /// ledger once per sibling pool and inflate the residue the reclaim
    /// decision gates on.
    ///
    /// An empty registry truthfully reports zeros — nothing is cached
    /// yet — never an error a gated client would mistake for a refusal.
    #[must_use]
    pub fn reclaim_class_stats(&self, class: ReclaimClass) -> ReclaimClassStats {
        let ledgers = self.ledgers.read();
        let mut total = ReclaimClassStats::default();
        for ledger in ledgers.iter().filter(|ledger| ledger.class() == class) {
            let stats = ledger.accounting().class_stats(class);
            total.payload_bytes = total.payload_bytes.saturating_add(stats.payload_bytes);
            total.metadata_bytes = total.metadata_bytes.saturating_add(stats.metadata_bytes);
            total.entries = total.entries.saturating_add(stats.entries);
            total.refusals = total.refusals.saturating_add(stats.refusals);
            total.pressure_shrinks = total
                .pressure_shrinks
                .saturating_add(stats.pressure_shrinks);
            total.teardowns = total.teardowns.saturating_add(stats.teardowns);
            total.failures = total.failures.saturating_add(stats.failures);
            total.hits = total.hits.saturating_add(stats.hits);
            total.misses = total.misses.saturating_add(stats.misses);
        }
        total
    }

    /// Register a live pinned pool's ledger for the per-pool export.
    ///
    /// Called by the production construction sites (a writable volume's
    /// write-back dirty set); unit-test pools simply never register.
    pub fn register_pinned_ledger(&self, ledger: PinnedLedger) {
        self.pinned.write().push(ledger);
    }

    /// One [`CacheLedgerRecord`] per registered ledger — the reclaimable
    /// caches first, then the pinned pools — in registration order, stable
    /// across the paged `CACHE_LEDGERS` reads exactly as every other list
    /// domain requires.
    ///
    /// Every record is stamped [`CacheLedgerOrigin::Kernel`] with
    /// `reporter_pid = 0`: everything registered here is a cache this
    /// kernel measures directly, never a process's claim about itself.
    /// A ledger whose [`CacheLedger::to_record`] refuses — an unrenderable
    /// label, which is a defect in the crate that built the cache, not a
    /// transient condition — is skipped rather than aborting the whole
    /// query: one misbehaving cache must not blind the export to every
    /// other registered ledger, and the label defect is exactly what
    /// `to_record`'s own refusal already exists to catch.
    #[must_use]
    pub fn cache_ledger_records(&self) -> Vec<CacheLedgerRecord> {
        let mut records = Vec::new();
        // One registry lock at a time: the two lists are independent, so
        // taking both at once would create a lock order for no reason.
        for ledger in self.ledgers.read().iter() {
            if let Ok(mut record) = ledger.to_record() {
                record.origin = CacheLedgerOrigin::Kernel;
                record.reporter_pid = 0;
                records.push(record);
            }
        }
        for ledger in self.pinned.read().iter() {
            if let Ok(mut record) = ledger.to_record() {
                record.origin = CacheLedgerOrigin::Kernel;
                record.reporter_pid = 0;
                records.push(record);
            }
        }
        records
    }

    /// The `ramzip` tier's exported counters: the live tier's snapshot
    /// once one has registered, and the truthful idle-tier zeros (the
    /// tier stores nothing) until then.
    #[must_use]
    pub fn ramzip_stats(&self) -> RamzipStats {
        match *self.ramzip.read() {
            Some(source) => source.stats(),
            None => RamzipStats::default(),
        }
    }
}

impl Default for MemStats {
    fn default() -> Self {
        Self::new()
    }
}

/// The production `ramzip` stats feed: the one process-global tier
/// installed by the boot path. Its [`RamzipStatsSource::stats`] reads a
/// lock-free counters snapshot of that tier (all zero before boot
/// installs one), so the `RAMZIP_STATS` query reports the real figures
/// the moment the tier goes live.
struct GlobalRamzipStats;

impl RamzipStatsSource for GlobalRamzipStats {
    fn stats(&self) -> RamzipStats {
        tairix_kernel_mem::ramzip::global_stats()
    }
}

static GLOBAL_RAMZIP_STATS: GlobalRamzipStats = GlobalRamzipStats;

/// Register the production `ramzip` stats feed on [`MEM_STATS`], so the
/// System Information `RAMZIP_STATS` query reports the live global
/// tier's counters. Called once from the boot path; first install wins.
pub fn install_global_ramzip_stats() {
    MEM_STATS.install_ramzip(&GLOBAL_RAMZIP_STATS);
}

#[cfg(test)]
mod tests {
    extern crate std;

    use super::*;
    use alloc::sync::Arc;
    use core::sync::atomic::{AtomicUsize, Ordering};
    use tairix_abi::sysinfo::CacheOwnerKind;
    use tairix_reclaim::{CacheAccounting, ReclaimOwner};

    struct Fake {
        free: AtomicUsize,
        total: usize,
    }

    impl FreeMemorySource for Fake {
        fn free_bytes(&self) -> usize {
            self.free.load(Ordering::Relaxed)
        }
        fn total_bytes(&self) -> usize {
            self.total
        }
    }

    fn leaked_fake() -> &'static Fake {
        Box::leak(Box::new(Fake {
            free: AtomicUsize::new(1 << 30),
            total: 1 << 30,
        }))
    }

    #[test]
    fn system_pressure_is_created_once_and_shared() {
        let stats = MemStats::new();
        let backing = leaked_fake();
        let first = stats.system_pressure(backing);
        let second = stats.system_pressure(leaked_fake());
        assert!(core::ptr::eq(first, second), "one gauge, one history");
    }

    #[test]
    fn reclaim_stats_aggregate_registered_ledgers_and_move() {
        let stats = MemStats::new();
        let class = ReclaimClass::CleanFileData;
        // Empty registry: truthful zeros.
        assert_eq!(
            stats.reclaim_class_stats(class),
            ReclaimClassStats::default()
        );

        let a = Arc::new(CacheAccounting::new());
        let b = Arc::new(CacheAccounting::new());
        stats.register_ledger(CacheLedger::new(
            "cache-a",
            ReclaimOwner::KernelSubsystem("mem"),
            class,
            a.clone(),
        ));
        stats.register_ledger(CacheLedger::new(
            "cache-b",
            ReclaimOwner::KernelSubsystem("mem"),
            class,
            b.clone(),
        ));

        a.charge(class, 4096, 64).expect("charges");
        b.charge(class, 1024, 32).expect("charges");
        b.record_refusal(class);
        let total = stats.reclaim_class_stats(class);
        assert_eq!(total.payload_bytes, 5120);
        assert_eq!(total.metadata_bytes, 96);
        assert_eq!(total.entries, 2);
        assert_eq!(total.refusals, 1);

        // The gauge moves as the underlying ledger moves.
        a.discharge(class, 4096, 64).expect("discharges");
        let after = stats.reclaim_class_stats(class);
        assert_eq!(after.payload_bytes, 1024);
        assert_eq!(after.entries, 1);
    }

    #[test]
    fn a_class_total_is_the_pool_s_own_charges_however_many_pools_share_a_ledger() {
        // A pool charges only the class its ledger declares, so reading a
        // ledger under its own class loses none of its figures — that much
        // is the identity. What it rules out is double counting: the
        // filesystem cache registers a ledger per classified pool over one
        // shared accounting object, and reading every ledger for every
        // class would read that one shared object once per sibling pool and
        // inflate the residue the reclaim decision gates on.
        let pools = [
            ("clean_fs.data", ReclaimClass::CleanFileData, 4096, 64),
            ("clean_fs.metadata", ReclaimClass::FsMetadata, 1024, 32),
        ];
        let owner = ReclaimOwner::FilesystemVolume { volume: 1 };
        // The exported figures are 64-bit; the ledger charges in host words.
        let charge = |accounting: &CacheAccounting, class, payload: u64, metadata: u64| {
            accounting
                .charge(
                    class,
                    usize::try_from(payload).expect("fits a host word"),
                    usize::try_from(metadata).expect("fits a host word"),
                )
                .expect("charges");
        };

        // Two pools over one shared ledger, exactly as that cache builds it.
        let shared = MemStats::new();
        let accounting = Arc::new(CacheAccounting::new());
        for (label, class, payload, metadata) in pools {
            shared.register_ledger(CacheLedger::new(label, owner, class, accounting.clone()));
            charge(&accounting, class, payload, metadata);
        }

        // The same charges spread over a ledger each, as two independent
        // single-pool caches would present them.
        let separate = MemStats::new();
        for (label, class, payload, metadata) in pools {
            let own = Arc::new(CacheAccounting::new());
            charge(&own, class, payload, metadata);
            separate.register_ledger(CacheLedger::new(label, owner, class, own));
        }

        for (_, class, payload, metadata) in pools {
            let total = shared.reclaim_class_stats(class);
            assert_eq!(
                total,
                accounting.class_stats(class),
                "the pool's own charges, read exactly once"
            );
            assert_eq!(
                total,
                separate.reclaim_class_stats(class),
                "sharing one ledger between pools changes no class total"
            );
            assert_eq!(total.payload_bytes, payload);
            assert_eq!(total.metadata_bytes, metadata);
            assert_eq!(total.entries, 1);
        }

        // Both pools still appear as their own row in the per-cache
        // export, each naming the pool it measures.
        let records = shared.cache_ledger_records();
        assert_eq!(records.len(), 2);
        for (record, (label, class, payload, _)) in records.iter().zip(pools) {
            assert_eq!(record.label(), label);
            assert_eq!(usize::from(record.class), class.index());
            assert_eq!(record.payload_bytes, payload);
        }
    }

    #[test]
    fn cache_ledger_records_is_empty_for_an_empty_registry() {
        let stats = MemStats::new();
        assert!(stats.cache_ledger_records().is_empty());
    }

    #[test]
    fn cache_ledger_records_carries_identity_and_figures_in_registration_order() {
        let stats = MemStats::new();
        let first = Arc::new(CacheAccounting::new());
        first
            .charge(ReclaimClass::DisposableUi, 4096, 64)
            .expect("charges");
        stats.register_ledger(CacheLedger::new(
            "wm.cursor",
            ReclaimOwner::DesktopSession { seat: 1 },
            ReclaimClass::DisposableUi,
            first,
        ));
        let second = Arc::new(CacheAccounting::new());
        second
            .charge(ReclaimClass::CleanFileData, 1024, 32)
            .expect("charges");
        stats.register_ledger(CacheLedger::new(
            "arxfs.clean",
            ReclaimOwner::FilesystemVolume { volume: 3 },
            ReclaimClass::CleanFileData,
            second,
        ));

        let records = stats.cache_ledger_records();
        assert_eq!(records.len(), 2, "one row per registered ledger");

        assert_eq!(records[0].label(), "wm.cursor");
        assert_eq!(records[0].owner_kind, CacheOwnerKind::DesktopSession);
        assert_eq!(records[0].owner_id, 1);
        assert_eq!(records[0].origin, CacheLedgerOrigin::Kernel);
        assert_eq!(records[0].reporter_pid, 0);
        assert_eq!(records[0].payload_bytes, 4096);
        assert_eq!(records[0].metadata_bytes, 64);

        assert_eq!(records[1].label(), "arxfs.clean");
        assert_eq!(records[1].owner_kind, CacheOwnerKind::FilesystemVolume);
        assert_eq!(records[1].owner_id, 3);
        assert_eq!(records[1].origin, CacheLedgerOrigin::Kernel);
        assert_eq!(records[1].reporter_pid, 0);
        assert_eq!(records[1].payload_bytes, 1024);
        assert_eq!(records[1].metadata_bytes, 32);
    }

    #[test]
    fn a_pinned_pool_is_exported_as_its_own_row_and_never_as_reclaim_headroom() {
        use tairix_reclaim::{PinnedAccounting, PinnedLedger};

        let stats = MemStats::new();
        let clean = Arc::new(CacheAccounting::new());
        clean
            .charge(ReclaimClass::CleanFileData, 1024, 32)
            .expect("a fresh ledger accepts a charge");
        stats.register_ledger(CacheLedger::new(
            "arxfs.clean",
            ReclaimOwner::FilesystemVolume { volume: 3 },
            ReclaimClass::CleanFileData,
            clean,
        ));
        let pinned = Arc::new(PinnedAccounting::new());
        pinned.set(1 << 20, 512);
        stats.register_pinned_ledger(PinnedLedger::new(
            "arxfs.writeback",
            ReclaimOwner::FilesystemVolume { volume: 3 },
            pinned,
        ));

        let records = stats.cache_ledger_records();
        assert_eq!(records.len(), 2, "the cache row, then the pinned one");
        assert_eq!(records[0].label(), "arxfs.clean");
        assert_eq!(records[1].label(), "arxfs.writeback");
        assert_eq!(records[1].class, tairix_abi::sysinfo::CACHE_CLASS_PINNED);
        assert_eq!(records[1].payload_bytes, 1 << 20);
        assert_eq!(records[1].origin, CacheLedgerOrigin::Kernel);

        // The reclaim decision must see only the reclaimable megabyte's worth:
        // counting a pinned pool as headroom would stall `ramzip` waiting for
        // memory nothing can free.
        assert_eq!(stats.ramzip_reclaimable_residue(), 1024 + 32);
        assert_eq!(
            stats
                .reclaim_class_stats(ReclaimClass::CleanFileData)
                .payload_bytes,
            1024
        );
    }

    #[test]
    fn cache_ledger_records_skips_a_ledger_with_an_unrenderable_label() {
        let stats = MemStats::new();
        // A control character in the label makes `to_record` refuse; the
        // query must still answer with every other registered row rather
        // than failing the whole page over one defective cache.
        stats.register_ledger(CacheLedger::new(
            "broken\u{1b}[2Jlabel",
            ReclaimOwner::KernelSubsystem("mem"),
            ReclaimClass::DisposableUi,
            Arc::new(CacheAccounting::new()),
        ));
        stats.register_ledger(CacheLedger::new(
            "good-cache",
            ReclaimOwner::KernelSubsystem("mem"),
            ReclaimClass::DisposableUi,
            Arc::new(CacheAccounting::new()),
        ));

        let records = stats.cache_ledger_records();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].label(), "good-cache");
    }

    #[test]
    fn current_pressure_is_none_until_the_gauge_is_created_then_shares_it() {
        let stats = MemStats::new();
        // No gauge yet: the fault-path reader gets nothing and does no work.
        assert!(stats.current_pressure().is_none());
        let gauge = stats.system_pressure(leaked_fake());
        // Once created, the reader sees the very same shared gauge.
        let observed = stats.current_pressure().expect("gauge exists");
        assert!(core::ptr::eq(gauge, observed), "one shared gauge");
    }

    #[test]
    fn reclaimable_residue_sums_only_clean_and_transform_bytes() {
        let stats = MemStats::new();
        // Empty registry: nothing to drain before compression.
        assert_eq!(stats.ramzip_reclaimable_residue(), 0);

        // One cache holding a pool of each class, so every class below is
        // genuinely in the registry and the exclusions the residue makes
        // are the formula's own, not an artefact of what was registered.
        let ledger = Arc::new(CacheAccounting::new());
        for class in [
            ReclaimClass::CleanFileData,
            ReclaimClass::TransformCache,
            ReclaimClass::FsMetadata,
            ReclaimClass::DisposableUi,
        ] {
            stats.register_ledger(CacheLedger::new(
                "test-cache",
                ReclaimOwner::KernelSubsystem("mem"),
                class,
                ledger.clone(),
            ));
        }
        // Clean file data and transform cache both count, payload+metadata.
        ledger
            .charge(ReclaimClass::CleanFileData, 4096, 64)
            .expect("charges");
        ledger
            .charge(ReclaimClass::TransformCache, 1024, 32)
            .expect("charges");
        // Metadata and other classes must NOT count toward the handoff
        // residue: only clean+transform are cheaper than compression.
        ledger
            .charge(ReclaimClass::FsMetadata, 8192, 128)
            .expect("charges");
        ledger
            .charge(ReclaimClass::DisposableUi, 2048, 16)
            .expect("charges");
        assert_eq!(
            stats.ramzip_reclaimable_residue(),
            4096 + 64 + 1024 + 32,
            "only clean file data + transform cache, payload and metadata"
        );
        // Draining them leaves nothing for the handoff to wait on.
        ledger
            .discharge(ReclaimClass::CleanFileData, 4096, 64)
            .expect("discharges");
        ledger
            .discharge(ReclaimClass::TransformCache, 1024, 32)
            .expect("discharges");
        assert_eq!(stats.ramzip_reclaimable_residue(), 0);
    }

    #[test]
    fn ramzip_defaults_to_the_idle_tier_until_a_source_registers() {
        struct Live;
        impl RamzipStatsSource for Live {
            fn stats(&self) -> RamzipStats {
                RamzipStats {
                    entries: 3,
                    ..RamzipStats::default()
                }
            }
        }
        static LIVE: Live = Live;
        struct Other;
        impl RamzipStatsSource for Other {
            fn stats(&self) -> RamzipStats {
                RamzipStats {
                    entries: 9,
                    ..RamzipStats::default()
                }
            }
        }
        static OTHER: Other = Other;

        let stats = MemStats::new();
        assert_eq!(stats.ramzip_stats(), RamzipStats::default());
        stats.install_ramzip(&LIVE);
        assert_eq!(stats.ramzip_stats().entries, 3);
        // First install wins; a second is ignored.
        stats.install_ramzip(&OTHER);
        assert_eq!(stats.ramzip_stats().entries, 3);
    }
}
