use tairix_wintersun_net::value::{ChunkCoord, WorldPoint};
use tairix_wintersun_world::chunk::{Chunk, ChunkBuild};
use tairix_wintersun_world::geom::{CellCoord, Elevation, CELL_SUB_UNITS};
use tairix_wintersun_world::params::RealmParams;
use tairix_wintersun_world::realm::RealmField;

use super::{
    cell_at, occupiable, rise_legal, ChunkTerrain, SyntheticTerrain, Terrain, TerrainCell,
    MAX_STEP_RISE_SUB_UNITS, WADE_DEPTH_SUB_UNITS,
};
use crate::error::RuleError;

fn wet(ground: i16, depth: i16) -> TerrainCell {
    TerrainCell {
        ground: Elevation(ground),
        water: Elevation(ground + depth),
    }
}

#[test]
fn depth_is_never_negative() {
    assert_eq!(TerrainCell::dry(Elevation(40)).depth(), 0);
    assert_eq!(wet(40, 3).depth(), 3);
    // Ground above the water surface is dry, not negatively wet.
    let inverted = TerrainCell {
        ground: Elevation(40),
        water: Elevation(10),
    };
    assert_eq!(inverted.depth(), 0);
}

#[test]
fn depth_cannot_overflow_at_the_field_extremes() {
    let extreme = TerrainCell {
        ground: Elevation(i16::MIN),
        water: Elevation(i16::MAX),
    };
    assert_eq!(
        extreme.depth(),
        i32::from(i16::MAX) - i32::from(i16::MIN),
        "the difference is formed in a wider type"
    );
}

#[test]
fn shallow_water_is_wadeable_and_deep_water_is_not() {
    let wade = i16::try_from(WADE_DEPTH_SUB_UNITS).expect("a small bound");
    assert!(
        occupiable(Some(wet(0, wade))),
        "exactly waist deep is passable"
    );
    assert!(!occupiable(Some(wet(0, wade + 1))));
    assert!(occupiable(Some(TerrainCell::dry(Elevation(0)))));
}

#[test]
fn unknown_ground_is_never_walkable() {
    assert!(!occupiable(None), "absent ground fails closed");
    assert!(!rise_legal(None, Some(TerrainCell::dry(Elevation(0)))));
    assert!(!rise_legal(Some(TerrainCell::dry(Elevation(0))), None));
    assert!(!rise_legal(None, None));
}

#[test]
fn a_rise_is_bounded_and_a_drop_is_not() {
    let rise = i16::try_from(MAX_STEP_RISE_SUB_UNITS).expect("a small bound");
    let floor = TerrainCell::dry(Elevation(0));
    assert!(rise_legal(
        Some(floor),
        Some(TerrainCell::dry(Elevation(rise)))
    ));
    assert!(!rise_legal(
        Some(floor),
        Some(TerrainCell::dry(Elevation(rise + 1)))
    ));
    assert!(
        rise_legal(Some(floor), Some(TerrainCell::dry(Elevation(i16::MIN)))),
        "walking off a ledge is allowed"
    );
}

#[test]
fn a_point_maps_to_its_cell_across_the_origin() {
    assert_eq!(cell_at(WorldPoint { x: 0, y: 0 }), CellCoord::new(0, 0));
    assert_eq!(
        cell_at(WorldPoint {
            x: CELL_SUB_UNITS - 1,
            y: 0
        }),
        CellCoord::new(0, 0)
    );
    assert_eq!(
        cell_at(WorldPoint { x: -1, y: -1 }),
        CellCoord::new(-1, -1),
        "flooring, so there is no double-width cell at the origin"
    );
    assert_eq!(
        cell_at(WorldPoint {
            x: -CELL_SUB_UNITS,
            y: CELL_SUB_UNITS
        }),
        CellCoord::new(-1, 1)
    );
}

#[test]
fn open_synthetic_ground_is_passable_everywhere() {
    let ground = SyntheticTerrain::open();
    for x in -20..20 {
        for y in -20..20 {
            let cell = ground.cell(CellCoord::new(x, y));
            assert!(occupiable(cell), "({x}, {y}) must be open");
        }
    }
}

#[test]
fn the_synthetic_lattice_puts_a_pillar_and_a_pool_where_it_says() {
    let ground = SyntheticTerrain::lattice(5, 3);
    let pillar = ground.cell(CellCoord::new(0, 0)).expect("ground");
    let plain = ground.cell(CellCoord::new(1, 0)).expect("ground");
    assert!(
        !rise_legal(Some(plain), Some(pillar)),
        "a pillar is unclimbable from beside it"
    );
    assert!(occupiable(Some(pillar)), "but standing on one is fine");

    let pool = ground.cell(CellCoord::new(1, 1)).expect("ground");
    assert!(!occupiable(Some(pool)), "a pool is too deep to wade");
    // The lattices are offset, so neither hides the other.
    assert!(occupiable(Some(
        ground.cell(CellCoord::new(5, 5)).expect("ground")
    )));
}

#[test]
fn the_synthetic_lattice_repeats_across_the_origin() {
    let ground = SyntheticTerrain::lattice(5, 0);
    let at_origin = ground.cell(CellCoord::new(0, 0)).expect("ground");
    let wrapped = ground.cell(CellCoord::new(-5, -5)).expect("ground");
    assert_eq!(
        at_origin, wrapped,
        "the pattern is flooring, not truncating"
    );
}

fn window() -> (RealmField, [Chunk; 2]) {
    let field = RealmField::generate(RealmParams::winter_default(0x51EE))
        .expect("the default realm solves");
    let first = ChunkBuild::new(ChunkCoord { x: 0, y: 0 })
        .and_then(|build| build.finish(&field))
        .expect("a chunk");
    let second = ChunkBuild::new(ChunkCoord { x: 1, y: 0 })
        .and_then(|build| build.finish(&field))
        .expect("a chunk");
    (field, [first, second])
}

#[test]
fn a_chunk_window_answers_for_the_chunks_it_holds_and_no_others() {
    let (_field, chunks) = window();
    let sorted = [&chunks[0], &chunks[1]];
    let terrain = ChunkTerrain::new(&sorted).expect("sorted");

    let inside = terrain.cell(CellCoord::new(3, 3)).expect("held");
    assert_eq!(inside.ground, chunks[0].elevation(3, 3));
    assert_eq!(inside.water, chunks[0].water(3, 3));
    assert!(
        terrain.cell(CellCoord::new(70, 3)).is_some(),
        "the second chunk"
    );
    assert!(
        terrain.cell(CellCoord::new(3, 200)).is_none(),
        "a chunk the window does not hold has no ground"
    );
}

#[test]
fn an_unsorted_or_duplicated_window_is_refused() {
    let (_field, chunks) = window();
    let reversed = [&chunks[1], &chunks[0]];
    assert_eq!(
        ChunkTerrain::new(&reversed).err(),
        Some(RuleError::TerrainWindow)
    );
    let duplicated = [&chunks[0], &chunks[0]];
    assert_eq!(
        ChunkTerrain::new(&duplicated).err(),
        Some(RuleError::TerrainWindow),
        "a duplicate coordinate makes the lookup's answer arbitrary"
    );
    let empty: [&Chunk; 0] = [];
    assert!(ChunkTerrain::new(&empty).is_ok());
}
