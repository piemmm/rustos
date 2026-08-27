//! Host tests for the semantic application-launch cache
//! (`plans/SMARTRAM.md` SMART4): classification and ownership, hit/miss
//! accounting, LRU eviction with hysteresis, replacement, pressure-band
//! enforcement, bounded fail-closed admission, and the guarantee that
//! reclaim never makes an app unlaunchable.

use super::*;

use alloc::boxed::Box;

use tairix_reclaim::{FreeMemorySource, PressureBand};

use crate::test_bundle::{composed_bundle, gate_load, verified_app};
use crate::test_pressure::{free_for, pressured, unpressured, TEST_TOTAL};
use crate::test_sink::TestSink;

extern crate std;

/// A generous budget every test entry fits under.
fn budget() -> CacheBudget {
    CacheBudget::from_backing(1 << 20)
}

/// A leaked capturing sink for the cache's audit records.
fn sink() -> &'static TestSink {
    Box::leak(Box::new(TestSink::new()))
}

/// The accounted cost of one cached test entry under `key`: learned from
/// the ledger itself, so tests need no private constant.
fn entry_cost(key: &str) -> usize {
    let mut cache = LaunchCache::new(budget(), unpressured(), sink());
    cache.insert(key, &verified_app());
    cache.accounting().total_bytes()
}

#[test]
fn the_cache_classifies_and_is_charged_to_the_app_store() {
    let cache = LaunchCache::new(budget(), unpressured(), sink());
    assert_eq!(
        cache.owner(),
        Some(ReclaimOwner::KernelSubsystem("app_store"))
    );
    assert_eq!(cache.accounting().total_bytes(), 0);
}

#[test]
fn a_miss_then_insert_then_hit_are_each_accounted() {
    let mut cache = LaunchCache::new(budget(), unpressured(), sink());
    let app = verified_app();
    assert!(cache.lookup("/System/Commands/ps.app").is_none());
    assert_eq!(cache.accounting().misses(), 1);

    cache.insert("/System/Commands/ps.app", &app);
    assert_eq!(cache.accounting().insertions(), 1);
    let charged = cache.accounting().total_bytes();
    assert!(
        charged > app.run_image().len(),
        "the ledger charges the image plus bookkeeping"
    );

    let hit = cache.lookup("/System/Commands/ps.app").expect("a hit");
    assert_eq!(cache.accounting().hits(), 1);
    // The hit is the very object the gate verified.
    assert_eq!(hit.as_ref(), app.as_ref());
}

#[test]
fn contains_peeks_residency_without_disturbing_the_cache() {
    let mut cache = LaunchCache::new(budget(), unpressured(), sink());
    let app = verified_app();
    assert!(
        !cache.contains("/System/Commands/ps.app"),
        "an absent bundle is not resident"
    );
    // A peek at an absent key records no miss (unlike `lookup`).
    assert_eq!(cache.accounting().misses(), 0);

    cache.insert("/System/Commands/ps.app", &app);
    assert!(
        cache.contains("/System/Commands/ps.app"),
        "a resident bundle is reported present"
    );
    // The peek restamped no LRU tick and recorded no hit.
    assert_eq!(cache.accounting().hits(), 0);
    assert_eq!(cache.resident(), alloc::vec!["/System/Commands/ps.app"]);
}

#[test]
fn a_hit_and_a_miss_produce_identical_load_decisions() {
    let (fs, anchor, _run) = composed_bundle(alloc::vec![]);
    let fresh = gate_load(&fs, anchor).expect("gate accepts");
    let mut cache = LaunchCache::new(budget(), unpressured(), sink());
    cache.insert(
        "/System/Commands/ps.app",
        &Arc::new(gate_load(&fs, anchor).expect("gate accepts")),
    );
    let cached = cache.lookup("/System/Commands/ps.app").expect("a hit");
    // Identity, image, capability ceiling, and library policy decisions
    // agree exactly between the cached and the freshly gated result.
    assert_eq!(cached.as_ref(), &fresh);
}

#[test]
fn reclaim_cannot_make_an_app_unlaunchable() {
    let (fs, anchor, _run) = composed_bundle(alloc::vec![]);
    let (source, pressure) = pressured(free_for(PressureBand::Normal));
    let mut cache = LaunchCache::new(budget(), pressure, sink());
    cache.insert(
        "/System/Commands/ps.app",
        &Arc::new(gate_load(&fs, anchor).expect("gate accepts")),
    );
    // Severe pressure drains the entry.
    source.set_free(free_for(PressureBand::Severe));
    assert!(cache.lookup("/System/Commands/ps.app").is_none());
    // The source bundle is intact, so the full gate still accepts it and
    // returns the same decision the cache would have served.
    let reloaded = gate_load(&fs, anchor).expect("the gate re-verifies the intact bundle");
    assert_eq!(reloaded.id(), "os.tairix.ps");
}

#[test]
fn replacement_never_shadows_and_the_ledger_stays_balanced() {
    let mut cache = LaunchCache::new(budget(), unpressured(), sink());
    let app = verified_app();
    cache.insert("/System/Commands/a.app", &app);
    let one = cache.accounting().total_bytes();
    cache.insert("/System/Commands/a.app", &app);
    assert_eq!(
        cache.accounting().total_bytes(),
        one,
        "re-inserting the same bundle replaces its entry"
    );
    assert_eq!(cache.accounting().invalidations(), 1);
    assert_eq!(cache.resident(), ["/System/Commands/a.app"]);
}

#[test]
fn eviction_is_least_recently_used_with_hysteresis() {
    let cost = entry_cost("/System/Commands/a.app");
    // A budget whose hard limit holds exactly two entries; the low
    // watermark (3/4 of hard) then holds exactly one.
    let budget = CacheBudget::from_backing((2 * cost + cost / 2) * 16);
    assert!(budget.hard() >= 2 * cost && budget.hard() < 3 * cost);
    let mut cache = LaunchCache::new(budget, unpressured(), sink());
    let app = verified_app();

    cache.insert("/System/Commands/a.app", &app);
    cache.insert("/System/Commands/b.app", &app);
    // Refresh `a` so `b` becomes least recently used.
    assert!(cache.lookup("/System/Commands/a.app").is_some());
    cache.insert("/System/Commands/c.app", &app);
    assert!(
        cache.lookup("/System/Commands/b.app").is_none(),
        "the least recently used entry is evicted first"
    );
    assert!(cache.lookup("/System/Commands/a.app").is_some());
    assert!(cache.lookup("/System/Commands/c.app").is_some());
    assert!(cache.accounting().evictions() >= 1);
    // The ledger never exceeds the hard limit.
    assert!(cache.accounting().total_bytes() <= budget.hard());
}

#[test]
fn an_entry_larger_than_the_hard_limit_is_refused() {
    let cost = entry_cost("/System/Commands/a.app");
    let budget = CacheBudget::from_backing((cost - 1) * 16);
    let mut cache = LaunchCache::new(budget, unpressured(), sink());
    cache.insert("/System/Commands/a.app", &verified_app());
    assert!(cache.lookup("/System/Commands/a.app").is_none());
    assert_eq!(cache.accounting().refusals(), 1);
    assert_eq!(cache.accounting().total_bytes(), 0);
}

#[test]
fn an_over_long_bundle_key_is_refused() {
    let mut cache = LaunchCache::new(budget(), unpressured(), sink());
    let long = alloc::format!("/System/Commands/{}.app", "x".repeat(300));
    cache.insert(&long, &verified_app());
    assert_eq!(cache.accounting().refusals(), 1);
    assert!(cache.lookup(&long).is_none());
}

#[test]
fn admission_follows_the_band_ceiling_and_recovers() {
    let (source, pressure) = pressured(free_for(PressureBand::Mild));
    let mut cache = LaunchCache::new(budget(), pressure, sink());
    let app = verified_app();
    // Mild pressure shrinks the semantic class toward its low watermark
    // rather than closing it: an entry that fits under that watermark is
    // still admitted, which is what keeps the cache useful while the band
    // holds.
    cache.insert("/System/Commands/a.app", &app);
    assert_eq!(cache.accounting().refusals(), 0);
    assert!(cache.lookup("/System/Commands/a.app").is_some());
    assert!(cache.accounting().total_bytes() <= budget().low());

    // Moderate finishes that reclaim: the class is emptied and admits
    // nothing.
    source.set_free(free_for(PressureBand::Moderate));
    cache.insert("/System/Commands/b.app", &app);
    assert!(cache.lookup("/System/Commands/b.app").is_none());
    assert_eq!(cache.accounting().total_bytes(), 0);

    // Back at normal pressure (above the mild exit watermark), admission
    // resumes.
    source.set_free(free_for(PressureBand::Normal));
    cache.insert("/System/Commands/a.app", &app);
    assert!(cache.lookup("/System/Commands/a.app").is_some());
}

#[test]
fn a_reading_inside_the_reserve_admits_nothing() {
    // Free memory just above the reserve floor folds to critical, so
    // admission is refused outright — cache growth can never be the
    // cause of reserve exhaustion. (The arithmetic of the reserve
    // clause itself is proven by the pressure module's own tests.)
    let (_, pressure) = pressured(TEST_TOTAL / 64 + 4096);
    let mut cache = LaunchCache::new(budget(), pressure, sink());
    cache.insert("/System/Commands/a.app", &verified_app());
    assert_eq!(cache.accounting().total_bytes(), 0);
    assert_eq!(cache.accounting().refusals(), 1);
}

#[test]
fn mild_pressure_shrinks_to_the_low_watermark() {
    let cost = entry_cost("/System/Commands/a.app");
    // Hard limit holds three entries; the low watermark holds two.
    let budget = CacheBudget::from_backing((3 * cost + cost / 4) * 16);
    assert!(budget.hard() >= 3 * cost && budget.low() >= 2 * cost && budget.low() < 3 * cost);
    let (source, pressure) = pressured(free_for(PressureBand::Normal));
    let mut cache = LaunchCache::new(budget, pressure, sink());
    let app = verified_app();
    cache.insert("/System/Commands/a.app", &app);
    cache.insert("/System/Commands/b.app", &app);
    cache.insert("/System/Commands/c.app", &app);
    assert_eq!(cache.resident().len(), 3);

    source.set_free(free_for(PressureBand::Mild));
    // Any operation applies the band's shrink target first.
    assert!(cache.lookup("/System/Commands/c.app").is_some());
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
        let mut cache = LaunchCache::new(budget(), pressure, sink());
        cache.insert("/System/Commands/a.app", &verified_app());
        assert_eq!(cache.resident().len(), 1);
        source.set_free(free_for(band));
        assert!(
            cache.lookup("/System/Commands/a.app").is_none(),
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
    let mut cache = LaunchCache::new(budget(), pressure, sink());
    cache.insert("/System/Commands/a.app", &verified_app());
    assert_eq!(cache.accounting().total_bytes(), 0);
    assert_eq!(cache.accounting().refusals(), 1);
    assert!(cache.lookup("/System/Commands/a.app").is_none());
}

#[test]
fn the_store_serves_uncached_until_the_reclaim_install() {
    let store = crate::appspawn::AppStore::pending([1u8; 32]);
    let app = verified_app();
    // No cache installed: nothing is cached, nothing is served, and the
    // launch path simply runs the full gate every time.
    store.cache_verified("/System/Commands/a.app", &app);
    assert!(store.cached("/System/Commands/a.app").is_none());
    // After the boot path installs the classified cache, caching works.
    store.install_reclaim(budget(), unpressured(), sink());
    store.cache_verified("/System/Commands/a.app", &app);
    assert!(store.cached("/System/Commands/a.app").is_some());
    // A second installation does not replace (and so does not drop) the
    // live cache.
    store.install_reclaim(budget(), unpressured(), sink());
    assert!(store.cached("/System/Commands/a.app").is_some());
}

/// The store's `cached_present` existence peek — the spawn probe's
/// cache-hit shortcut — reports residency without serving, and stays
/// `false` while no cache is installed (so a fresh boot always probes the
/// volume rather than fabricating a hit).
#[test]
fn cached_present_reports_residency_without_serving() {
    let store = crate::appspawn::AppStore::pending([1u8; 32]);
    let app = verified_app();
    // No cache installed: the peek reports absent, so the probe falls
    // through to its filesystem lookup rather than claiming a hit.
    assert!(!store.cached_present("/System/Commands/a.app"));
    store.install_reclaim(budget(), unpressured(), sink());
    // An installed but empty cache: still absent.
    assert!(!store.cached_present("/System/Commands/a.app"));
    store.cache_verified("/System/Commands/a.app", &app);
    assert!(
        store.cached_present("/System/Commands/a.app"),
        "a cached bundle is reported present"
    );
    assert!(
        !store.cached_present("/System/Commands/b.app"),
        "an uncached bundle is reported absent"
    );
    // The peek served nothing: the entry is still cached and hittable.
    assert!(store.cached("/System/Commands/a.app").is_some());
}

#[test]
fn counters_track_every_event_path() {
    let mut cache = LaunchCache::new(budget(), unpressured(), sink());
    let app = verified_app();
    let _ = cache.lookup("/System/Commands/a.app");
    cache.insert("/System/Commands/a.app", &app);
    let _ = cache.lookup("/System/Commands/a.app");
    cache.insert("/System/Commands/a.app", &app);
    let long = "x".repeat(300);
    cache.insert(&long, &app);
    assert_eq!(cache.accounting().misses(), 1);
    assert_eq!(cache.accounting().hits(), 1);
    assert_eq!(cache.accounting().insertions(), 2);
    assert_eq!(cache.accounting().invalidations(), 1);
    assert_eq!(cache.accounting().refusals(), 1);
}

#[test]
fn a_forced_pressure_shrink_is_counted() {
    let (source, pressure) = pressured(free_for(PressureBand::Normal));
    let mut cache = LaunchCache::new(budget(), pressure, sink());
    cache.insert("/System/Commands/a.app", &verified_app());
    assert_eq!(cache.accounting().pressure_shrinks(), 0);
    source.set_free(free_for(PressureBand::Moderate));
    assert!(cache.lookup("/System/Commands/a.app").is_none());
    assert_eq!(cache.accounting().pressure_shrinks(), 1);
}

#[test]
fn payload_and_metadata_bytes_are_accounted_separately() {
    let mut cache = LaunchCache::new(budget(), unpressured(), sink());
    cache.insert("/System/Commands/a.app", &verified_app());
    let class = ReclaimClass::SemanticAppCache;
    let acct = cache.accounting();
    assert!(acct.class_payload_bytes(class) > 0);
    assert!(acct.class_metadata_bytes(class) > 0);
    assert_eq!(
        acct.class_bytes(class),
        acct.class_payload_bytes(class) + acct.class_metadata_bytes(class)
    );
}

#[test]
fn a_detected_defect_is_counted_and_reported_once() {
    let captured = sink();
    let mut cache = LaunchCache::new(budget(), unpressured(), captured);
    cache.insert("/System/Commands/a.app", &verified_app());
    cache.poison("ledger_imbalance");
    assert_eq!(cache.accounting().failures(), 1);
    assert!(cache.accounting().teardowns() >= 1);
    assert_eq!(cache.accounting().total_bytes(), 0);
    let events = captured.snapshot();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].id.0, 2001);
    // The record's field shape is closed — fixed labels and numeric
    // handles only, never a bundle path or cached bytes.
    let keys: Vec<&str> = events[0].fields.iter().map(|(k, _)| k.as_str()).collect();
    assert_eq!(keys, ["cache", "owner", "owner_id", "cause"]);
    assert_eq!(events[0].fields[0].1, "launch");
    assert_eq!(events[0].fields[1].1, "app_store");
    assert_eq!(events[0].fields[3].1, "ledger_imbalance");
    // An already-poisoned cache never reports again.
    cache.poison("orphan_index_slot");
    assert_eq!(captured.snapshot().len(), 1);
    assert_eq!(cache.accounting().failures(), 1);
}

#[test]
fn normal_operation_emits_no_audit_records() {
    let captured = sink();
    let mut cache = LaunchCache::new(budget(), unpressured(), captured);
    cache.insert("/System/Commands/a.app", &verified_app());
    assert!(cache.lookup("/System/Commands/a.app").is_some());
    assert!(cache.lookup("/System/Commands/missing.app").is_none());
    assert!(captured.snapshot().is_empty());
}

/// The ledger charges the manifest strings and library references, not
/// just the image, so the cost model cannot silently under-account an
/// entry whose metadata dominates.
#[test]
fn the_cost_model_charges_strings_beside_the_image() {
    let mut cache = LaunchCache::new(budget(), unpressured(), sink());
    let app = verified_app();
    cache.insert("/System/Commands/ps.app", &app);
    let floor = app.run_image().len()
        + app.id().len()
        + app.name().len()
        + app.version().len()
        + app.run_path().len()
        + "/System/Commands/ps.app".len();
    assert!(cache.accounting().total_bytes() > floor);
}

/// A leaked cache-admission control with the semantic class disabled
/// (`cache.semantic off`), its own instance so it never touches the
/// process-global control other tests rely on.
fn launch_disabled_control() -> &'static CacheControl {
    let control: &'static CacheControl = Box::leak(Box::new(CacheControl::new()));
    control.set(CacheClass::Semantic, crate::CacheMode::Off);
    control
}

#[test]
fn a_disabled_launch_cache_admits_nothing() {
    let mut cache = LaunchCache::new(budget(), unpressured(), sink())
        .with_cache_control(launch_disabled_control());
    cache.insert("/System/Commands/ps.app", &verified_app());
    assert!(
        cache.lookup("/System/Commands/ps.app").is_none(),
        "off is a miss"
    );
    assert_eq!(cache.accounting().total_bytes(), 0, "off admits nothing");
    assert!(cache.accounting().refusals() >= 1);
}

#[test]
fn flipping_the_semantic_switch_off_drops_the_cache() {
    let control: &'static CacheControl = Box::leak(Box::new(CacheControl::new()));
    let mut cache = LaunchCache::new(budget(), unpressured(), sink()).with_cache_control(control);
    cache.insert("/System/Commands/ps.app", &verified_app());
    assert!(cache.accounting().total_bytes() > 0, "the cache filled");

    // The operator disables the class: the next operation drops everything
    // and thereafter every launch runs the full load gate.
    control.set(CacheClass::Semantic, crate::CacheMode::Off);
    assert!(cache.lookup("/System/Commands/ps.app").is_none(), "purged");
    assert_eq!(cache.accounting().total_bytes(), 0);
    assert!(cache.accounting().teardowns() >= 1);
}
