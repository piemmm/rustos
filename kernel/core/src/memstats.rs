//! Late-installed registry of the live memory-statistics sources the
//! System Information introspection feed exports
//! (`plans/STRESSTEST.md` ST1).
//!
//! The figures the `MEMORY_PRESSURE`, `RECLAIM_STATS`, and
//! `RAMZIP_STATS` queries report live in places that only exist once the
//! boot path has created them: the system pressure gauge is built over
//! the frame allocator, each reclaimable cache's ledger is born with the
//! cache (per mounted volume, per installed launch cache), and the
//! `ramzip` tier is the one process-global compressed-memory pool the
//! boot path installs once the CSPRNG is seeded (its stats feed is
//! registered by [`install_global_ramzip_stats`]). This registry is the
//! one arch-neutral rendezvous
//! between those producers and the read-only export in
//! [`crate::introspect_source`] — the same late-install pattern as the
//! unlock-published user database.
//!
//! Everything here is observation-only: registering a ledger grants the
//! export nothing but lock-free reads of saturating diagnostics, and the
//! registry never mutates a producer's state (the one deliberate
//! exception: reading the pressure gauge takes a fresh sample, which is
//! how every consumer of the gauge reads it).

use alloc::boxed::Box;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicUsize, Ordering};

use tairix_abi::sysinfo::RamzipStats;
use tairix_kernel_mem::pressure::FreeMemorySource;
use tairix_kernel_mem::reclaim::{CacheAccounting, ReclaimClass, ReclaimClassStats};
use tairix_kernel_mem::MemoryPressure;
use tairix_sync::RwLock;

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
    /// Every registered live cache ledger. Growable (one per mounted
    /// volume cache plus the boot singletons), never a fixed ceiling; a
    /// ledger registered here lives for the boot (the `Arc` keeps a
    /// torn-down cache's final, zeroed books readable).
    ledgers: RwLock<Vec<Arc<CacheAccounting>>>,
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
        let gauge: &'static MemoryPressure = Box::leak(Box::new(MemoryPressure::over(backing)));
        *slot = Some(gauge);
        gauge
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

    /// Register a live cache ledger for the reclaim export.
    ///
    /// Called by the production construction sites (a mounted volume's
    /// filesystem cache, the block/transform caches, the launch cache);
    /// unit-test caches simply never register.
    pub fn register_ledger(&self, ledger: Arc<CacheAccounting>) {
        self.ledgers.write().push(ledger);
    }

    /// Install the live `ramzip` tier's stats feed. First install wins.
    pub fn install_ramzip(&self, source: &'static (dyn RamzipStatsSource + 'static)) {
        let mut slot = self.ramzip.write();
        if slot.is_none() {
            *slot = Some(source);
        }
    }

    /// Aggregate `class`'s figures across every registered ledger.
    ///
    /// An empty registry truthfully reports zeros — nothing is cached
    /// yet — never an error a gated client would mistake for a refusal.
    #[must_use]
    pub fn reclaim_class_stats(&self, class: ReclaimClass) -> ReclaimClassStats {
        let ledgers = self.ledgers.read();
        let mut total = ReclaimClassStats::default();
        for ledger in ledgers.iter() {
            let stats = ledger.class_stats(class);
            total.payload_bytes = total.payload_bytes.saturating_add(stats.payload_bytes);
            total.metadata_bytes = total.metadata_bytes.saturating_add(stats.metadata_bytes);
            total.entries = total.entries.saturating_add(stats.entries);
            total.refusals = total.refusals.saturating_add(stats.refusals);
            total.pressure_shrinks = total
                .pressure_shrinks
                .saturating_add(stats.pressure_shrinks);
            total.teardowns = total.teardowns.saturating_add(stats.teardowns);
            total.failures = total.failures.saturating_add(stats.failures);
        }
        total
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
    use core::sync::atomic::{AtomicUsize, Ordering};

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
        stats.register_ledger(a.clone());
        stats.register_ledger(b.clone());

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

        let ledger = Arc::new(CacheAccounting::new());
        stats.register_ledger(ledger.clone());
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
