//! Cross-cache integration tests for the reclaimable-memory services
//! (`plans/SMARTRAM.md` SMART10).
//!
//! Every earlier suite proves one cache against its own gauge; these
//! tests prove the *system* behaviour the plan binds: one shared
//! pressure gauge driving the filesystem cache (`CachedFs`, clean file
//! data + metadata) and the semantic launch cache (`LaunchCache`)
//! together through the documented band order, the `ramzip` handoff
//! computed over the caches' combined residue, the reserve floor, the
//! post-drain invalidation correctness, the thrash scenario (band
//! flapping cannot churn rebuilds), and the work-avoided benchmark
//! evidence behind the retention policy.

use crate::fs::memfs::RwMockFs;
use crate::fs::CachedFs;
use crate::launch_cache::LaunchCache;
use crate::test_bundle::verified_app;
use crate::test_pressure::{free_for, pressured, TestSource};
use crate::test_sink::TestSink;

use alloc::boxed::Box;

use tairix_abi::driver::filesystem::{FilesystemRead, FilesystemWrite, NodeId, NodeKind};
use tairix_kernel_mem::{escalation, ramzip_handoff, EscalationStep, RamzipHandoff};
use tairix_reclaim::{CacheBudget, MemoryPressure, PressureBand, ReclaimClass, ReclaimOwner};

extern crate std;

/// A generous budget both caches' test entries fit under.
fn budget() -> CacheBudget {
    CacheBudget::from_backing(16 * 1024 * 1024)
}

/// A leaked capturing sink for the caches' audit records.
fn sink() -> &'static TestSink {
    Box::leak(Box::new(TestSink::new()))
}

/// Both production caches over **one** shared gauge, warmed: the
/// filesystem cache holds clean file data and metadata for
/// `/dir/file.txt`, the launch cache holds one verified system bundle.
fn warmed_pair(
    contents: &[u8],
) -> (
    &'static TestSource,
    &'static MemoryPressure,
    CachedFs<RwMockFs>,
    LaunchCache,
) {
    let (source, pressure) = pressured(free_for(PressureBand::Normal));

    let mut fs = RwMockFs::new();
    let root = fs.root();
    fs.create(root, b"dir", NodeKind::Directory).expect("mkdir");
    let dir = fs.lookup(root, b"dir").expect("dir resolves");
    fs.create(dir, b"file.txt", NodeKind::RegularFile)
        .expect("create");
    fs.write_at(dir, b"file.txt", 0, contents).expect("seed");
    let mut fs_cache = CachedFs::new(
        fs,
        budget(),
        ReclaimOwner::FilesystemVolume { volume: 1 },
        pressure,
        sink(),
    );
    let file = file_of(&mut fs_cache);
    let mut buf = [0u8; 64];
    fs_cache.read_at(file, 0, &mut buf).expect("warm data");
    fs_cache.node_info(file).expect("warm stat");
    assert!(
        fs_cache
            .accounting()
            .class_bytes(ReclaimClass::CleanFileData)
            > 0
    );
    assert!(fs_cache.accounting().class_bytes(ReclaimClass::FsMetadata) > 0);

    let mut launch = LaunchCache::new(budget(), pressure, sink());
    launch.insert("/System/Apps/ps.app", &verified_app());
    assert!(
        launch
            .accounting()
            .class_bytes(ReclaimClass::SemanticAppCache)
            > 0
    );

    (source, pressure, fs_cache, launch)
}

fn file_of(cache: &mut CachedFs<RwMockFs>) -> NodeId {
    let root = cache.root();
    let dir = cache.lookup(root, b"dir").expect("dir resolves");
    cache.lookup(dir, b"file.txt").expect("file resolves")
}

/// The clean-file plus transform residue the `ramzip` handoff gate is
/// fed: every byte of those classes still resident across the caches
/// sharing the gauge (the launch cache holds neither class; its ledger
/// keeps the sum honest rather than assumed).
fn clean_and_transform(fs_cache: &CachedFs<RwMockFs>, launch: &LaunchCache) -> usize {
    fs_cache
        .accounting()
        .class_bytes(ReclaimClass::CleanFileData)
        + fs_cache
            .accounting()
            .class_bytes(ReclaimClass::TransformCache)
        + launch.accounting().class_bytes(ReclaimClass::CleanFileData)
        + launch
            .accounting()
            .class_bytes(ReclaimClass::TransformCache)
}

/// Touch both caches so each applies the current band's forced-shrink
/// targets on its own next operation, exactly as production consumers
/// sample the gauge. The root stat is never pre-warmed, so under
/// pressure every touch also drives one refused (and counted)
/// admission attempt.
fn touch(fs_cache: &mut CachedFs<RwMockFs>, launch: &mut LaunchCache) {
    let root = fs_cache.root();
    let _ = fs_cache.lookup(root, b"dir");
    let _ = fs_cache.node_info(root);
    let _ = launch.lookup("/System/Apps/ps.app");
}

#[test]
fn one_gauge_drives_both_caches_through_the_documented_band_order() {
    let (source, pressure, mut fs_cache, mut launch) = warmed_pair(b"ordered reclaim");

    // Mild: the semantic class shrinks toward the low watermark and no
    // cache grows, but nothing here exceeds the watermark, so the
    // resident entries survive; clean file data and metadata are held.
    source.set_free(free_for(PressureBand::Mild));
    touch(&mut fs_cache, &mut launch);
    assert_eq!(pressure.band(), PressureBand::Mild);
    assert!(
        fs_cache
            .accounting()
            .class_bytes(ReclaimClass::CleanFileData)
            > 0,
        "clean file data is not drained at mild pressure"
    );
    assert!(!pressure.growth_permitted(4096), "no growth outside normal");

    // Moderate: the launch cache and the clean file data finish
    // reclaim; hot metadata alone is preserved (to the low watermark).
    source.set_free(free_for(PressureBand::Moderate));
    touch(&mut fs_cache, &mut launch);
    assert_eq!(pressure.band(), PressureBand::Moderate);
    assert_eq!(
        fs_cache
            .accounting()
            .class_bytes(ReclaimClass::CleanFileData),
        0,
        "clean file data finishes reclaim at moderate pressure"
    );
    assert_eq!(
        launch
            .accounting()
            .class_bytes(ReclaimClass::SemanticAppCache),
        0,
        "the semantic launch cache drains at moderate pressure"
    );
    assert!(
        fs_cache.accounting().class_bytes(ReclaimClass::FsMetadata) > 0,
        "hot metadata is preserved at moderate pressure"
    );

    // Severe: every class across both caches is forced to zero.
    source.set_free(free_for(PressureBand::Severe));
    touch(&mut fs_cache, &mut launch);
    assert_eq!(fs_cache.accounting().total_bytes(), 0);
    assert_eq!(launch.accounting().total_bytes(), 0);

    // Both caches still serve correctly straight from their backings.
    let file = file_of(&mut fs_cache);
    let mut buf = [0u8; 64];
    let n = fs_cache.read_at(file, 0, &mut buf).expect("still served");
    assert_eq!(&buf[..n], b"ordered reclaim");
}

#[test]
fn the_ramzip_handoff_opens_only_after_the_caches_drain() {
    let (source, pressure, mut fs_cache, mut launch) = warmed_pair(b"handoff ordering");

    // At moderate pressure with clean cache still resident, compression
    // is held and the escalation order says: reclaim caches first.
    source.set_free(free_for(PressureBand::Moderate));
    let band = pressure.sample();
    assert_eq!(band, PressureBand::Moderate);
    let residue = clean_and_transform(&fs_cache, &launch);
    assert!(residue > 0, "the warmed caches hold clean residue");
    assert_eq!(
        ramzip_handoff(band, residue),
        RamzipHandoff::HoldCompression
    );
    assert_eq!(escalation(band, residue), EscalationStep::ReclaimCaches);

    // The consumers' own next operations apply the band targets; with
    // the clean and transform residue at zero the handoff gate opens.
    touch(&mut fs_cache, &mut launch);
    let drained = clean_and_transform(&fs_cache, &launch);
    assert_eq!(drained, 0, "moderate pressure drains clean and transform");
    assert_eq!(
        ramzip_handoff(band, drained),
        RamzipHandoff::CompressColdAnonymous
    );
    assert_eq!(escalation(band, drained), EscalationStep::HandOffToRamzip);

    // Critical pressure never starts speculative compression: the
    // escalation belongs to the VM policy.
    source.set_free(free_for(PressureBand::Critical));
    let band = pressure.sample();
    assert_eq!(band, PressureBand::Critical);
    assert_eq!(ramzip_handoff(band, 0), RamzipHandoff::HoldCompression);
    assert_eq!(escalation(band, 0), EscalationStep::VmPolicy);
}

#[test]
fn the_reserve_floor_is_shared_and_admits_nothing() {
    let (source, pressure, mut fs_cache, mut launch) = warmed_pair(b"reserve preserved");

    // A reading inside the reserve is critical for every consumer of
    // the shared gauge: both caches drain and neither re-admits.
    let reserve = pressure.thresholds().reserve();
    source.set_free(reserve);
    touch(&mut fs_cache, &mut launch);
    assert_eq!(pressure.band(), PressureBand::Critical);
    assert_eq!(fs_cache.accounting().total_bytes(), 0);
    assert_eq!(launch.accounting().total_bytes(), 0);
    assert!(
        !pressure.growth_permitted(0),
        "growth is never permitted from inside the reserve"
    );
}

#[test]
fn a_mutation_during_forced_drain_is_never_served_stale() {
    let (source, _pressure, mut fs_cache, mut launch) = warmed_pair(b"before mutation");
    let file = file_of(&mut fs_cache);

    // Drain everything at severe pressure, then mutate the file while
    // the caches are empty.
    source.set_free(free_for(PressureBand::Severe));
    touch(&mut fs_cache, &mut launch);
    let dir = {
        let root = fs_cache.root();
        fs_cache.lookup(root, b"dir").expect("dir resolves")
    };
    fs_cache
        .write_at(dir, b"file.txt", 0, b"after mutation!")
        .expect("mutate under pressure");

    // Recovery back to normal: hysteresis walks the band up one step
    // per sample, after which the caches repopulate — with the mutated
    // bytes, never the drained generation's.
    source.set_free(free_for(PressureBand::Normal));
    for _ in 0..PressureBand::ALL.len() {
        let _ = fs_cache.node_info(file);
    }
    let mut buf = [0u8; 64];
    let n = fs_cache.read_at(file, 0, &mut buf).expect("served");
    assert_eq!(&buf[..n], b"after mutation!");
    let m = fs_cache.read_at(file, 0, &mut buf).expect("served warm");
    assert_eq!(&buf[..m], b"after mutation!", "the warm copy is current");
    assert!(fs_cache.accounting().total_bytes() > 0, "the cache rebuilt");
}

#[test]
fn band_flapping_cannot_churn_cache_rebuilds() {
    let (source, pressure, mut fs_cache, mut launch) = warmed_pair(b"no thrash");

    // Enter mild pressure once.
    source.set_free(free_for(PressureBand::Mild));
    touch(&mut fs_cache, &mut launch);
    assert_eq!(pressure.band(), PressureBand::Mild);
    let insertions_fs = fs_cache.accounting().insertions();
    let insertions_launch = launch.accounting().insertions();
    let mild_entries = pressure.band_entries(PressureBand::Mild);

    // Flap the free reading between the mild enter watermark and the
    // hysteresis window below the exit watermark. The band holds, so
    // the caches never oscillate between rebuild and reclaim: admission
    // stays refused (the churn is *detected* as counted refusals, and
    // *reduced* to zero rebuilds).
    let held = free_for(PressureBand::Mild) + 8192;
    for _ in 0..8 {
        source.set_free(held);
        touch(&mut fs_cache, &mut launch);
        assert_eq!(pressure.band(), PressureBand::Mild, "hysteresis holds");
        source.set_free(free_for(PressureBand::Mild));
        touch(&mut fs_cache, &mut launch);
    }
    assert_eq!(
        pressure.band_entries(PressureBand::Mild),
        mild_entries,
        "the flap never re-enters the band"
    );
    assert_eq!(
        fs_cache.accounting().insertions(),
        insertions_fs,
        "no filesystem-cache rebuild churn under the flap"
    );
    assert_eq!(
        launch.accounting().insertions(),
        insertions_launch,
        "no launch-cache rebuild churn under the flap"
    );
    assert!(
        fs_cache.accounting().refusals() > 0,
        "the refused re-admissions are counted"
    );

    // A genuine recovery above the exit watermark rebuilds exactly once
    // per touched entry, not once per flap.
    source.set_free(free_for(PressureBand::Normal));
    touch(&mut fs_cache, &mut launch);
    assert_eq!(pressure.band(), PressureBand::Normal);
    touch(&mut fs_cache, &mut launch);
    assert!(fs_cache.accounting().insertions() > insertions_fs);
}

/// Work-avoided and latency evidence behind the retention policy
/// (`plans/SMARTRAM.md` section 14): the deterministic assertions prove
/// the *work avoided* (a warm pass performs zero driver reads and zero
/// load-gate runs); the printed timings are estimates for threshold
/// tuning, never assertions.
#[test]
fn bench_evidence_warm_passes_avoid_the_rebuild_work() {
    const PASSES: u32 = 32;

    // Filesystem cache: a cold pass misses, every warm pass hits.
    let (_source, _pressure, mut fs_cache, mut launch) = warmed_pair(b"bench evidence");
    let file = file_of(&mut fs_cache);
    let mut buf = [0u8; 64];
    let misses_after_warm = fs_cache.accounting().misses();
    let cold_hits = fs_cache.accounting().hits();

    let started = std::time::Instant::now();
    for _ in 0..PASSES {
        fs_cache.read_at(file, 0, &mut buf).expect("warm read");
    }
    let warm_read = started.elapsed();

    assert_eq!(
        fs_cache.accounting().misses(),
        misses_after_warm,
        "warm reads never fall back to the driver"
    );
    assert!(fs_cache.accounting().hits() >= cold_hits + u64::from(PASSES));

    // Launch cache: every warm lookup serves the verified app without
    // re-running the load gate (a lookup miss is the only path that
    // would force one).
    let lookup_misses = launch.accounting().misses();
    let started = std::time::Instant::now();
    for _ in 0..PASSES {
        assert!(launch.lookup("/System/Apps/ps.app").is_some());
    }
    let warm_launch = started.elapsed();
    assert_eq!(
        launch.accounting().misses(),
        lookup_misses,
        "warm launches never re-run the load gate"
    );

    // The cold costs, measured over fresh backings for the estimate.
    let started = std::time::Instant::now();
    for _ in 0..PASSES {
        let (_s, _p, mut cold_fs, _l) = warmed_pair(b"bench evidence");
        let cold_file = file_of(&mut cold_fs);
        cold_fs.read_at(cold_file, 0, &mut buf).expect("cold read");
    }
    let cold_setup = started.elapsed();

    std::eprintln!(
        "bench estimate (not a guarantee): {PASSES} warm fs reads {warm_read:?}, \
         {PASSES} warm launch lookups {warm_launch:?}, \
         {PASSES} cold warm-ups (full gate + driver) {cold_setup:?}"
    );
}
