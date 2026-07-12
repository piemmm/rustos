//! Late-installed registry of the live memory-statistics sources the
//! System Information introspection feed exports
//! (`plans/STRESSTEST.md` ST1).
//!
//! The figures the `MEMORY_PRESSURE`, `RECLAIM_STATS`, and
//! `RAMZIP_STATS` queries report live in places that only exist once the
//! boot path has created them: the system pressure gauge is built over
//! the frame allocator, each reclaimable cache's ledger is born with the
//! cache (per mounted volume, per installed launch cache), and the
//! `ramzip` tier has no live instance until the restartable-user-fault
//! prerequisite lands. This registry is the one arch-neutral rendezvous
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

use rustos_abi::sysinfo::RamzipStats;
use rustos_kernel_mem::pressure::FreeMemorySource;
use rustos_kernel_mem::reclaim::{CacheAccounting, ReclaimClass, ReclaimClassStats};
use rustos_kernel_mem::MemoryPressure;
use rustos_sync::RwLock;

/// A live `ramzip` tier's stats feed.
///
/// No production tier registers yet (its restartable-user-fault
/// prerequisite is staged in `PLAN.md`); when one comes up it installs
/// itself here and the `RAMZIP_STATS` query starts reporting its real
/// figures. Counters only — never page contents or key material.
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
