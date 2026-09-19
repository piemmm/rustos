use core::sync::atomic::{AtomicUsize, Ordering};

use tairix_log::{Event, Sink};
use tairix_reclaim::{PressureBand, ReportedPressure};
use tairix_wintersun_world::biome::Material;

use super::{MaterialCache, TileKey};
use crate::material::{Mip, Quality};

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

fn cache(backing_bytes: usize) -> MaterialCache {
    PRESSURE.report(PressureBand::Normal);
    MaterialCache::new("wintersun-art-test", backing_bytes, &PRESSURE, &SINK)
}

fn key(material: Material, level: u32) -> TileKey {
    TileKey {
        material,
        mip: Mip::new(level).expect("level is in the chain"),
    }
}

#[test]
fn a_tile_is_synthesised_once_and_then_held() {
    let mut cache = cache(256 * 1024 * 1024);
    let tile = key(Material::Gravel, 1);

    assert!(cache.ensure(Quality::FULL, tile));
    let first: alloc::vec::Vec<_> = cache
        .peek(Quality::FULL, &tile)
        .expect("just ensured")
        .texels()
        .to_vec();

    assert!(cache.ensure(Quality::FULL, tile));
    assert_eq!(
        cache
            .peek(Quality::FULL, &tile)
            .expect("still held")
            .texels(),
        first.as_slice(),
    );
    assert_eq!(cache.len(), 1);
}

#[test]
fn an_empty_cache_holds_nothing_and_peeks_nothing() {
    let cache = cache(256 * 1024 * 1024);
    assert!(cache.is_empty());
    assert_eq!(cache.charged_bytes(), 0);
    assert!(cache.peek(Quality::FULL, &key(Material::Rock, 0)).is_none());
}

#[test]
fn the_cache_charges_for_what_it_holds() {
    let mut cache = cache(256 * 1024 * 1024);
    assert!(cache.ensure(Quality::FULL, key(Material::Rock, 2)));
    let charged = cache.charged_bytes();
    let payload = cache
        .peek(Quality::FULL, &key(Material::Rock, 2))
        .expect("held")
        .payload_bytes();
    assert!(charged >= payload, "charged {charged} for {payload} bytes");
}

#[test]
fn several_materials_and_mips_coexist() {
    let mut cache = cache(256 * 1024 * 1024);
    let keys = [
        key(Material::Rock, 0),
        key(Material::Rock, 3),
        key(Material::Sand, 0),
        key(Material::Water, 2),
    ];
    for &tile in &keys {
        assert!(cache.ensure(Quality::FULL, tile));
    }
    assert_eq!(cache.len(), keys.len());
    for &tile in &keys {
        let held = cache.peek(Quality::FULL, &tile).expect("held");
        assert_eq!(held.material(), tile.material);
        assert_eq!(held.mip(), tile.mip);
    }
}

#[test]
fn a_quality_change_stales_every_tile() {
    // The generation token's whole job: a tile synthesised at one octave
    // count must not be served for another.
    let mut cache = cache(256 * 1024 * 1024);
    let tile = key(Material::Moor, 1);
    assert!(cache.ensure(Quality::FULL, tile));
    assert!(cache.peek(Quality::FULL, &tile).is_some());
    assert!(cache.peek(Quality::new(1), &tile).is_none());
}

#[test]
fn a_budget_too_small_for_a_tile_refuses_rather_than_fails() {
    // A machine that cannot hold the tile still gets a `false`, never an
    // error and never a panic — the caller's answer is a coarser mip.
    let mut cache = cache(64 * 1024);
    assert!(!cache.ensure(Quality::FULL, key(Material::Rock, 0)));
    assert!(cache.peek(Quality::FULL, &key(Material::Rock, 0)).is_none());
}

#[test]
fn pressure_gives_the_tiles_back() {
    let mut cache = cache(256 * 1024 * 1024);
    for level in 0..4 {
        assert!(cache.ensure(Quality::FULL, key(Material::Gravel, level)));
    }
    assert!(!cache.is_empty());

    PRESSURE.report(PressureBand::Critical);
    let released = cache.enforce_pressure();
    PRESSURE.report(PressureBand::Normal);

    assert!(released > 0, "critical pressure released nothing");
    assert!(cache.charged_bytes() < 256 * 1024 * 1024);
}
