//! Host tests for the semantic application-launch cache
//! (`plans/SMARTRAM.md` SMART4): classification and ownership, hit/miss
//! accounting, LRU eviction with hysteresis, replacement, pressure-band
//! enforcement, bounded fail-closed admission, and the guarantee that
//! reclaim never makes an app unlaunchable.

use super::*;

use core::sync::atomic::{AtomicUsize, Ordering};

use alloc::boxed::Box;

use rustos_kernel_mem::{FreeMemorySource, PressureBand};

use crate::test_bundle::{composed_bundle, MemFs};
use crate::test_sink::TestSink;

extern crate std;

/// The test gauge's backing size (1 GiB), so the band watermarks land
/// on readable byte counts.
const TEST_TOTAL: usize = 1 << 30;

/// A controllable memory reading backing a test pressure gauge.
struct TestSource {
    free: AtomicUsize,
}

impl FreeMemorySource for TestSource {
    fn free_bytes(&self) -> usize {
        self.free.load(Ordering::Relaxed)
    }

    fn total_bytes(&self) -> usize {
        TEST_TOTAL
    }
}

/// A gauge plus its adjustable source, starting with `free` bytes free.
fn pressured(free: usize) -> (&'static TestSource, &'static MemoryPressure) {
    let source: &'static TestSource = Box::leak(Box::new(TestSource {
        free: AtomicUsize::new(free),
    }));
    (source, Box::leak(Box::new(MemoryPressure::over(source))))
}

/// A gauge pinned at plentiful free memory: normal pressure.
fn unpressured() -> &'static MemoryPressure {
    pressured(TEST_TOTAL / 2).1
}

/// A free reading that folds to `band` from any shallower state.
fn free_for(band: PressureBand) -> usize {
    match band {
        PressureBand::Normal => TEST_TOTAL / 2,
        PressureBand::Mild => TEST_TOTAL / 5 - 4096,
        PressureBand::Moderate => TEST_TOTAL / 10 - 4096,
        PressureBand::Severe => TEST_TOTAL / 16 - 4096,
        PressureBand::Critical => TEST_TOTAL / 32 - 4096,
    }
}

/// A verified [`LoadedApp`] straight from the shared load gate, over the
/// composed in-memory test bundle.
fn verified_app() -> Arc<LoadedApp> {
    let (fs, anchor, _run) = composed_bundle(alloc::vec![]);
    Arc::new(gate_load(&fs, anchor).expect("the composed bundle verifies"))
}

/// Run the full `rustos_appload` gate over `fs`, exactly as the spawn
/// path does.
fn gate_load(fs: &MemFs, anchor: [u8; 32]) -> Result<LoadedApp, rustos_appload::AppError> {
    let sink: &'static TestSink = Box::leak(Box::new(TestSink::new()));
    let store = crate::appspawn::FsBundleStore::new(fs, 1000, &NoCaps);
    let verifier = crate::appspawn::AnchorVerifier::new(anchor);
    let loader = rustos_appload::AppLoader::new(rustos_appload::AppLoaderConfig {
        accepted_abi_version: rustos_abi::ABI_VERSION_CURRENT,
        syscall_table_hash: rustos_kernel_syscall::SYSCALL_TABLE_HASH,
        store: &store,
        verifier: &verifier,
        sink,
    });
    loader.load(
        "/System/Apps/ps.app",
        &rustos_caps::CapabilitySet::from_words([u64::MAX; 4]),
    )
}

/// A `CapabilityQuery` granting nothing — the mock filesystem enforces
/// no permissions.
struct NoCaps;
impl rustos_abi::CapabilityQuery for NoCaps {
    fn holds(&self, _cap: rustos_abi::CapabilityId) -> bool {
        false
    }
}

/// A generous budget every test entry fits under.
fn budget() -> CacheBudget {
    CacheBudget::from_backing(1 << 20)
}

/// The accounted cost of one cached test entry under `key`: learned from
/// the ledger itself, so tests need no private constant.
fn entry_cost(key: &str) -> usize {
    let mut cache = LaunchCache::new(budget(), unpressured());
    cache.insert(key, &verified_app());
    cache.accounting().total_bytes()
}

#[test]
fn the_cache_classifies_and_is_charged_to_the_app_store() {
    let cache = LaunchCache::new(budget(), unpressured());
    assert_eq!(
        cache.owner(),
        Some(ReclaimOwner::KernelSubsystem("app_store"))
    );
    assert_eq!(cache.accounting().total_bytes(), 0);
}

#[test]
fn a_miss_then_insert_then_hit_are_each_accounted() {
    let mut cache = LaunchCache::new(budget(), unpressured());
    let app = verified_app();
    assert!(cache.lookup("/System/Apps/ps.app").is_none());
    assert_eq!(cache.accounting().misses(), 1);

    cache.insert("/System/Apps/ps.app", &app);
    assert_eq!(cache.accounting().insertions(), 1);
    let charged = cache.accounting().total_bytes();
    assert!(
        charged > app.run_image().len(),
        "the ledger charges the image plus bookkeeping"
    );

    let hit = cache.lookup("/System/Apps/ps.app").expect("a hit");
    assert_eq!(cache.accounting().hits(), 1);
    // The hit is the very object the gate verified.
    assert_eq!(hit.as_ref(), app.as_ref());
}

#[test]
fn a_hit_and_a_miss_produce_identical_load_decisions() {
    let (fs, anchor, _run) = composed_bundle(alloc::vec![]);
    let fresh = gate_load(&fs, anchor).expect("gate accepts");
    let mut cache = LaunchCache::new(budget(), unpressured());
    cache.insert(
        "/System/Apps/ps.app",
        &Arc::new(gate_load(&fs, anchor).expect("gate accepts")),
    );
    let cached = cache.lookup("/System/Apps/ps.app").expect("a hit");
    // Identity, image, capability ceiling, and library policy decisions
    // agree exactly between the cached and the freshly gated result.
    assert_eq!(cached.as_ref(), &fresh);
}

#[test]
fn reclaim_cannot_make_an_app_unlaunchable() {
    let (fs, anchor, _run) = composed_bundle(alloc::vec![]);
    let (source, pressure) = pressured(free_for(PressureBand::Normal));
    let mut cache = LaunchCache::new(budget(), pressure);
    cache.insert(
        "/System/Apps/ps.app",
        &Arc::new(gate_load(&fs, anchor).expect("gate accepts")),
    );
    // Severe pressure drains the entry.
    source
        .free
        .store(free_for(PressureBand::Severe), Ordering::Relaxed);
    assert!(cache.lookup("/System/Apps/ps.app").is_none());
    // The source bundle is intact, so the full gate still accepts it and
    // returns the same decision the cache would have served.
    let reloaded = gate_load(&fs, anchor).expect("the gate re-verifies the intact bundle");
    assert_eq!(reloaded.id(), "os.rustos.ps");
}

#[test]
fn replacement_never_shadows_and_the_ledger_stays_balanced() {
    let mut cache = LaunchCache::new(budget(), unpressured());
    let app = verified_app();
    cache.insert("/System/Apps/a.app", &app);
    let one = cache.accounting().total_bytes();
    cache.insert("/System/Apps/a.app", &app);
    assert_eq!(
        cache.accounting().total_bytes(),
        one,
        "re-inserting the same bundle replaces its entry"
    );
    assert_eq!(cache.accounting().invalidations(), 1);
    assert_eq!(cache.resident(), ["/System/Apps/a.app"]);
}

#[test]
fn eviction_is_least_recently_used_with_hysteresis() {
    let cost = entry_cost("/System/Apps/a.app");
    // A budget whose hard limit holds exactly two entries; the low
    // watermark (3/4 of hard) then holds exactly one.
    let budget = CacheBudget::from_backing((2 * cost + cost / 2) * 16);
    assert!(budget.hard() >= 2 * cost && budget.hard() < 3 * cost);
    let mut cache = LaunchCache::new(budget, unpressured());
    let app = verified_app();

    cache.insert("/System/Apps/a.app", &app);
    cache.insert("/System/Apps/b.app", &app);
    // Refresh `a` so `b` becomes least recently used.
    assert!(cache.lookup("/System/Apps/a.app").is_some());
    cache.insert("/System/Apps/c.app", &app);
    assert!(
        cache.lookup("/System/Apps/b.app").is_none(),
        "the least recently used entry is evicted first"
    );
    assert!(cache.lookup("/System/Apps/a.app").is_some());
    assert!(cache.lookup("/System/Apps/c.app").is_some());
    assert!(cache.accounting().evictions() >= 1);
    // The ledger never exceeds the hard limit.
    assert!(cache.accounting().total_bytes() <= budget.hard());
}

#[test]
fn an_entry_larger_than_the_hard_limit_is_refused() {
    let cost = entry_cost("/System/Apps/a.app");
    let budget = CacheBudget::from_backing((cost - 1) * 16);
    let mut cache = LaunchCache::new(budget, unpressured());
    cache.insert("/System/Apps/a.app", &verified_app());
    assert!(cache.lookup("/System/Apps/a.app").is_none());
    assert_eq!(cache.accounting().refusals(), 1);
    assert_eq!(cache.accounting().total_bytes(), 0);
}

#[test]
fn an_over_long_bundle_key_is_refused() {
    let mut cache = LaunchCache::new(budget(), unpressured());
    let long = alloc::format!("/System/Apps/{}.app", "x".repeat(300));
    cache.insert(&long, &verified_app());
    assert_eq!(cache.accounting().refusals(), 1);
    assert!(cache.lookup(&long).is_none());
}

#[test]
fn admission_is_refused_outside_normal_pressure_and_recovers() {
    let (source, pressure) = pressured(free_for(PressureBand::Mild));
    let mut cache = LaunchCache::new(budget(), pressure);
    let app = verified_app();
    cache.insert("/System/Apps/a.app", &app);
    assert_eq!(cache.accounting().refusals(), 1);
    assert_eq!(cache.accounting().total_bytes(), 0);
    // Back at normal pressure (above the mild exit watermark), admission
    // resumes.
    source
        .free
        .store(free_for(PressureBand::Normal), Ordering::Relaxed);
    cache.insert("/System/Apps/a.app", &app);
    assert!(cache.lookup("/System/Apps/a.app").is_some());
}

#[test]
fn a_reading_inside_the_reserve_admits_nothing() {
    // Free memory just above the reserve floor folds to critical, so
    // admission is refused outright — cache growth can never be the
    // cause of reserve exhaustion. (The arithmetic of the reserve
    // clause itself is proven by the pressure module's own tests.)
    let (_, pressure) = pressured(TEST_TOTAL / 64 + 4096);
    let mut cache = LaunchCache::new(budget(), pressure);
    cache.insert("/System/Apps/a.app", &verified_app());
    assert_eq!(cache.accounting().total_bytes(), 0);
    assert_eq!(cache.accounting().refusals(), 1);
}

#[test]
fn mild_pressure_shrinks_to_the_low_watermark() {
    let cost = entry_cost("/System/Apps/a.app");
    // Hard limit holds three entries; the low watermark holds two.
    let budget = CacheBudget::from_backing((3 * cost + cost / 4) * 16);
    assert!(budget.hard() >= 3 * cost && budget.low() >= 2 * cost && budget.low() < 3 * cost);
    let (source, pressure) = pressured(free_for(PressureBand::Normal));
    let mut cache = LaunchCache::new(budget, pressure);
    let app = verified_app();
    cache.insert("/System/Apps/a.app", &app);
    cache.insert("/System/Apps/b.app", &app);
    cache.insert("/System/Apps/c.app", &app);
    assert_eq!(cache.resident().len(), 3);

    source
        .free
        .store(free_for(PressureBand::Mild), Ordering::Relaxed);
    // Any operation applies the band's shrink target first.
    assert!(cache.lookup("/System/Apps/c.app").is_some());
    assert!(
        cache.accounting().total_bytes() <= budget.low(),
        "mild pressure shrinks the semantic class to the low watermark"
    );
    assert_eq!(cache.resident().len(), 2, "the oldest entry was reclaimed");
}

#[test]
fn moderate_and_deeper_pressure_drain_the_cache() {
    for band in [
        PressureBand::Moderate,
        PressureBand::Severe,
        PressureBand::Critical,
    ] {
        let (source, pressure) = pressured(free_for(PressureBand::Normal));
        let mut cache = LaunchCache::new(budget(), pressure);
        cache.insert("/System/Apps/a.app", &verified_app());
        assert_eq!(cache.resident().len(), 1);
        source.free.store(free_for(band), Ordering::Relaxed);
        assert!(
            cache.lookup("/System/Apps/a.app").is_none(),
            "{band:?} drains the semantic cache before ramzip handoff"
        );
        assert_eq!(cache.accounting().total_bytes(), 0);
        assert_eq!(cache.resident().len(), 0);
    }
}

#[test]
fn a_zero_backing_fails_closed_and_admits_nothing() {
    struct ZeroSource;
    impl FreeMemorySource for ZeroSource {
        fn free_bytes(&self) -> usize {
            0
        }
        fn total_bytes(&self) -> usize {
            0
        }
    }
    let zero: &'static ZeroSource = Box::leak(Box::new(ZeroSource));
    let pressure: &'static MemoryPressure = Box::leak(Box::new(MemoryPressure::over(zero)));
    let mut cache = LaunchCache::new(budget(), pressure);
    cache.insert("/System/Apps/a.app", &verified_app());
    assert_eq!(cache.accounting().total_bytes(), 0);
    assert_eq!(cache.accounting().refusals(), 1);
    assert!(cache.lookup("/System/Apps/a.app").is_none());
}

#[test]
fn the_store_serves_uncached_until_the_reclaim_install() {
    let store = crate::appspawn::AppStore::pending([1u8; 32]);
    let app = verified_app();
    // No cache installed: nothing is cached, nothing is served, and the
    // launch path simply runs the full gate every time.
    store.cache_verified("/System/Apps/a.app", &app);
    assert!(store.cached("/System/Apps/a.app").is_none());
    // After the boot path installs the classified cache, caching works.
    store.install_reclaim(budget(), unpressured());
    store.cache_verified("/System/Apps/a.app", &app);
    assert!(store.cached("/System/Apps/a.app").is_some());
    // A second installation does not replace (and so does not drop) the
    // live cache.
    store.install_reclaim(budget(), unpressured());
    assert!(store.cached("/System/Apps/a.app").is_some());
}

#[test]
fn counters_track_every_event_path() {
    let mut cache = LaunchCache::new(budget(), unpressured());
    let app = verified_app();
    let _ = cache.lookup("/System/Apps/a.app");
    cache.insert("/System/Apps/a.app", &app);
    let _ = cache.lookup("/System/Apps/a.app");
    cache.insert("/System/Apps/a.app", &app);
    let long = "x".repeat(300);
    cache.insert(&long, &app);
    assert_eq!(cache.accounting().misses(), 1);
    assert_eq!(cache.accounting().hits(), 1);
    assert_eq!(cache.accounting().insertions(), 2);
    assert_eq!(cache.accounting().invalidations(), 1);
    assert_eq!(cache.accounting().refusals(), 1);
}

/// The ledger charges the manifest strings and library references, not
/// just the image, so the cost model cannot silently under-account an
/// entry whose metadata dominates.
#[test]
fn the_cost_model_charges_strings_beside_the_image() {
    let mut cache = LaunchCache::new(budget(), unpressured());
    let app = verified_app();
    cache.insert("/System/Apps/ps.app", &app);
    let floor = app.run_image().len()
        + app.id().len()
        + app.name().len()
        + app.version().len()
        + app.run_path().len()
        + "/System/Apps/ps.app".len();
    assert!(cache.accounting().total_bytes() > floor);
}
