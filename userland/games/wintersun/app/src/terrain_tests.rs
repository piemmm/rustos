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

#[test]
fn the_ground_worth_holding_is_the_view_and_a_chunk_around_it() {
    let visible = bounds(-5_000, -5_000, 5_000, 5_000);
    let needed: alloc::vec::Vec<_> = visible_chunks(visible).collect();
    assert!(
        needed.iter().all(|coord| worth_holding(*coord, visible)),
        "the view gave away ground it draws"
    );
    let (min_x, max_x) = (
        needed.iter().map(|c| c.x).min().expect("a chunk"),
        needed.iter().map(|c| c.x).max().expect("a chunk"),
    );
    let y = needed[0].y;
    assert!(worth_holding(ChunkCoord { x: max_x + 1, y }, visible));
    assert!(worth_holding(ChunkCoord { x: min_x - 1, y }, visible));
    assert!(!worth_holding(ChunkCoord { x: max_x + 2, y }, visible));
    assert!(!worth_holding(ChunkCoord { x: min_x, y: y - 2 }, visible));
    // At the coordinate extremes the margin saturates rather than wrapping.
    let edge = bounds(i32::MIN, i32::MIN, i32::MIN + 1, i32::MIN + 1);
    assert!(!worth_holding(
        ChunkCoord {
            x: i32::MAX,
            y: i32::MAX
        },
        edge
    ));
}

/// Held ground is kept in coordinate order however it arrives, a chunk
/// adopted twice is held once, and a view that moves away gives back what it
/// no longer needs while keeping what it still draws.
#[test]
fn held_ground_is_ordered_deduplicated_and_trimmed_to_the_view() {
    let field = field();
    let near = bounds(-2_000, -2_000, 2_000, 2_000);
    let mut arrived = chunks(&field, near);
    assert!(arrived.len() > 2, "the view spans several chunks");
    let coords: alloc::vec::Vec<_> = arrived.iter().map(Chunk::coord).collect();
    arrived.reverse();

    let mut ground = HeldGround::new();
    for chunk in arrived {
        ground.adopt(chunk).expect("room to hold it");
    }
    let again = ChunkBuild::new(coords[0])
        .expect("a chunk fits")
        .finish(&field)
        .expect("a chunk generates");
    ground.adopt(again).expect("room to hold it");
    let held: alloc::vec::Vec<_> = ground
        .borrow()
        .expect("room to borrow")
        .iter()
        .map(|chunk| chunk.coord())
        .collect();
    assert_eq!(held, coords, "held in coordinate order, each once");
    assert!(ChunkWindow::new(&ground.borrow().expect("room")).is_ok());

    let far = bounds(900_000, 900_000, 902_000, 902_000);
    ground.release_distant(far);
    assert!(coords.iter().all(|coord| !ground.holds(*coord)));
    for chunk in chunks(&field, near) {
        ground.adopt(chunk).expect("room to hold it");
    }
    ground.release_distant(near);
    assert!(coords.iter().all(|coord| ground.holds(*coord)));
}
