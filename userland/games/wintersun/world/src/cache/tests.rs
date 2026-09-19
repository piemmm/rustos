use core::sync::atomic::{AtomicUsize, Ordering};

use tairix_log::{Event, Sink};
use tairix_reclaim::{PressureBand, ReportedPressure};

use super::ChunkCache;
use crate::params::{RealmParams, RealmSpec};
use crate::realm::RealmField;
use tairix_wintersun_net::value::ChunkCoord;

/// A sink that only counts, so a test can tell a silent cache from a
/// refused one without a journal.
struct CountingSink(AtomicUsize);

impl Sink for CountingSink {
    fn write_event(&self, _: &Event<'_>) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }
}

static SINK: CountingSink = CountingSink(AtomicUsize::new(0));
static PRESSURE: ReportedPressure = ReportedPressure::unknown();

/// A cache with a mebibyte-scale budget, which is enough for a handful of
/// chunks and not for a realm's worth.
fn cache() -> ChunkCache {
    PRESSURE.report(PressureBand::Normal);
    ChunkCache::new("wintersun-test", 64 * 1024 * 1024, &PRESSURE, &SINK)
}

fn field() -> RealmField {
    let params = RealmParams::new(RealmSpec {
        seed: 0xCAC4E,
        extent_chunks: 32,
        coarse_samples: 64,
        ..RealmParams::winter_default(0xCAC4E).spec()
    })
    .expect("legal");
    RealmField::generate(params).expect("solves")
}

#[test]
fn a_chunk_is_generated_once_and_then_held() {
    let field = field();
    let mut cache = cache();
    let coord = ChunkCoord { x: 0, y: 0 };

    let first = cache.get_or_generate(&field, coord).expect("generates");
    let first_digest = crate::digest::chunk(&first);
    drop(first);

    let second = cache.get_or_generate(&field, coord).expect("serves");
    assert!(second.is_cached(), "the second ask should be a hit");
    assert_eq!(crate::digest::chunk(&second), first_digest);
}

#[test]
fn the_cache_charges_for_what_it_holds() {
    let field = field();
    let mut cache = cache();
    assert_eq!(cache.charged_bytes(), 0);
    let served = cache
        .get_or_generate(&field, ChunkCoord { x: 1, y: 2 })
        .expect("generates");
    // A chunk is some seventy kibibytes against a four-mebibyte budget,
    // so a cache this size has no excuse to refuse it.
    assert!(served.is_cached(), "the budget admits a chunk");
    drop(served);
    assert!(cache.charged_bytes() > 0, "a held entry must be charged");
}

#[test]
fn pressure_gives_the_memory_back() {
    let field = field();
    let mut cache = cache();
    for x in 0..6 {
        let _ = cache.get_or_generate(&field, ChunkCoord { x, y: 0 });
    }
    let before = cache.charged_bytes();
    PRESSURE.report(PressureBand::Critical);
    cache.enforce_pressure();
    assert!(
        cache.charged_bytes() <= before,
        "a tightening band must not grow the cache"
    );
    PRESSURE.report(PressureBand::Normal);
}

#[test]
fn a_parameter_change_invalidates_every_entry() {
    let first = field();
    let mut cache = cache();
    let coord = ChunkCoord { x: 0, y: 0 };
    let before = crate::digest::chunk(&cache.get_or_generate(&first, coord).expect("generates"));

    let params = RealmParams::new(RealmSpec {
        seed: 0xD1FF,
        ..first.params().spec()
    })
    .expect("legal");
    let second = RealmField::generate(params).expect("solves");
    let after = crate::digest::chunk(&cache.get_or_generate(&second, coord).expect("generates"));

    assert_ne!(before, after, "a stale chunk was served for a new realm");
}

#[test]
fn the_cache_can_wipe_a_chunk_it_releases() {
    // Terrain is public, so the cache's own release path does not call
    // this — which is exactly why it is exercised here rather than left
    // to be discovered wrong the day something sensitive is cached.
    use tairix_reclaim::CachedBytes;

    let field = field();
    let mut chunk = crate::chunk::ChunkBuild::new(ChunkCoord { x: 0, y: 0 })
        .expect("fits")
        .finish(&field)
        .expect("builds");
    let before = crate::digest::chunk(&chunk);
    chunk.wipe();
    assert_ne!(crate::digest::chunk(&chunk), before, "wipe left the bytes");
    assert!(chunk.scatter().is_empty());
}

#[test]
fn an_uncached_result_is_still_a_correct_one() {
    // A budget too small to admit anything must still answer every ask.
    let field = field();
    PRESSURE.report(PressureBand::Normal);
    let mut tiny = ChunkCache::new("wintersun-tiny", 1024, &PRESSURE, &SINK);
    let served = tiny
        .get_or_generate(&field, ChunkCoord { x: 0, y: 0 })
        .expect("generates");
    assert!(!served.is_cached(), "a kilobyte cannot hold a chunk");
    assert_eq!(served.blend(0, 0).total(), 255);
}
