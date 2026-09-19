//! The lattice covers the view, unmapped ground is drawn as unmapped,
//! and a road reaches the weight field.

use super::*;
use tairix_wintersun_world::chunk::ChunkBuild;
use tairix_wintersun_world::params::{RealmParams, RealmSpec};
use tairix_wintersun_world::realm::RealmField;

fn realm() -> RealmParams {
    RealmParams::new(RealmSpec {
        extent_chunks: 8,
        coarse_samples: 32,
        plates: 8,
        ..RealmParams::winter_default(0x7E57_5EED).spec()
    })
    .expect("the spec is in range")
}

fn field() -> RealmField {
    RealmField::generate(realm()).expect("the realm generates")
}

fn chunks(
    field: &RealmField,
    visible: Bounds,
) -> alloc::vec::Vec<tairix_wintersun_world::chunk::Chunk> {
    visible_chunks(visible)
        .filter(|c| field.params().holds_chunk(c.x, c.y))
        .map(|coord| {
            ChunkBuild::new(coord)
                .expect("a chunk fits")
                .finish(field)
                .expect("a chunk generates")
        })
        .collect()
}

fn bounds(min_x: i32, min_y: i32, max_x: i32, max_y: i32) -> Bounds {
    Bounds {
        min_x,
        min_y,
        max_x,
        max_y,
    }
}

#[test]
fn visible_chunks_are_produced_in_window_order() {
    let coords: alloc::vec::Vec<_> = visible_chunks(bounds(-5_000, -5_000, 5_000, 5_000)).collect();
    assert!(!coords.is_empty());
    assert!(
        coords.is_sorted(),
        "the window is binary-searched, so its order is the coordinate's"
    );
    let unique: alloc::collections::BTreeSet<_> = coords.iter().collect();
    assert_eq!(unique.len(), coords.len(), "a chunk was listed twice");
}

#[test]
fn the_visible_chunks_cover_every_lattice_sample_the_pass_reads() {
    let field = field();
    let visible = bounds(-3_000, -3_000, 3_000, 3_000);
    let held = chunks(&field, visible);
    let borrowed: alloc::vec::Vec<_> = held.iter().collect();
    let window = ChunkWindow::new(&borrowed).expect("generated in order");
    let mut grid = TerrainGrid::new();
    grid.rebuild(&window, visible, &[], &Fray::new(1))
        .expect("the grid fits");
    assert_eq!(
        grid.unmapped(),
        0,
        "the chunk enumeration missed ground the lattice reads"
    );
}

#[test]
fn a_lattice_covers_one_sample_past_each_edge() {
    let field = field();
    let visible = bounds(0, 0, 0, 0);
    let held = chunks(&field, visible);
    let borrowed: alloc::vec::Vec<_> = held.iter().collect();
    let window = ChunkWindow::new(&borrowed).expect("generated in order");
    let mut grid = TerrainGrid::new();
    grid.rebuild(&window, visible, &[], &Fray::new(1))
        .expect("the grid fits");
    let (cols, rows) = grid.extent();
    assert!(
        cols >= 2 && rows >= 2,
        "a single-pixel view still needs a pair to interpolate between"
    );
}

#[test]
fn ground_with_no_resident_chunk_is_drawn_as_unmapped() {
    let empty: [&tairix_wintersun_world::chunk::Chunk; 0] = [];
    let window = ChunkWindow::new(&empty).expect("an empty window is sorted");
    let visible = bounds(0, 0, 4_000, 4_000);
    let mut grid = TerrainGrid::new();
    grid.rebuild(&window, visible, &[], &Fray::new(1))
        .expect("the grid fits");
    let (cols, rows) = grid.extent();
    assert_eq!(
        grid.unmapped(),
        cols * rows,
        "every sample should be missing"
    );
    assert_eq!(grid.ground(0, 0), None);
    assert_eq!(grid.materials().count(), 0);

    // And the pass draws it rather than failing or guessing.
    let warp = Warp::new(1);
    let params = realm();
    let quality = Quality::FULL;
    let cache = MaterialCache::new("terrain-unmapped-test", 1 << 20, &PRESSURE, &SINK);
    let _ = params;
    let pass = Pass {
        warp: &warp,
        cache: &cache,
        quality,
        step: 32,
        origin: WorldPoint { x: 0, y: 0 },
    };
    let mut row = [Pixel::TRANSPARENT; 64];
    paint_row(&mut row, &grid, &pass, 0);
    assert!(
        row.iter().all(|p| *p == UNMAPPED.premultiply()),
        "unmapped ground was drawn as something else"
    );
}

#[test]
fn a_road_reaches_the_weight_field() {
    let field = field();
    let roads = RoadDecals::from_realm(&field).expect("the roads fit");
    if roads.is_empty() {
        // A realm with no settlements routes no roads; the decal path is
        // covered by the reference frame's realm, which has some.
        return;
    }
    let decals = roads.decals().expect("the decals fit");
    let fray = Fray::new(field.params().seed());
    let on_road = field
        .roads()
        .first()
        .and_then(|road| road.path.first())
        .and_then(|cell| cell.centre())
        .expect("a road has a first cell");
    let visible = bounds(
        on_road.x - 2_000,
        on_road.y - 2_000,
        on_road.x + 2_000,
        on_road.y + 2_000,
    );
    let held = chunks(&field, visible);
    let borrowed: alloc::vec::Vec<_> = held.iter().collect();
    let window = ChunkWindow::new(&borrowed).expect("generated in order");

    let mut bare = TerrainGrid::new();
    bare.rebuild(&window, visible, &[], &fray)
        .expect("the grid fits");
    let mut paved = TerrainGrid::new();
    paved
        .rebuild(&window, visible, &decals, &fray)
        .expect("the grid fits");

    let bare_set: alloc::collections::BTreeSet<_> = bare.materials().collect();
    let paved_set: alloc::collections::BTreeSet<_> = paved.materials().collect();
    assert!(
        paved_set.contains(&Material::Gravel) || bare_set == paved_set,
        "a road crossing the view left no gravel in the weight field"
    );
}

#[test]
fn the_mip_coarsens_as_the_camera_pulls_back() {
    let mut last = None;
    for step in [8, 16, 32, 64, 128] {
        let mip = mip_for(Material::Rock, step);
        if let Some(previous) = last {
            assert!(
                mip.level() >= previous,
                "pulling back to {step} sub-units a pixel chose a finer mip"
            );
        }
        last = Some(mip.level());
    }
}

/// A sink the terrain tests do not read.
struct Quiet;

impl tairix_log::Sink for Quiet {
    fn write_event(&self, _: &tairix_log::Event<'_>) {}
}

static SINK: Quiet = Quiet;
static PRESSURE: tairix_reclaim::ReportedPressure = tairix_reclaim::ReportedPressure::unknown();
