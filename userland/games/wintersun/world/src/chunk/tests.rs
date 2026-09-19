use super::{Chunk, ChunkBuild, Phase, Surface, SHORE_CELLS, WORK_CELLS};
use crate::biome::WEIGHT_TOTAL;
use crate::digest;
use crate::geom::CHUNK_CELLS;
use crate::params::{RealmParams, RealmSpec};
use crate::realm::RealmField;
use tairix_wintersun_net::value::ChunkCoord;

fn field(seed: u64) -> RealmField {
    let params = RealmParams::new(RealmSpec {
        seed,
        extent_chunks: 32,
        coarse_samples: 64,
        ..RealmParams::winter_default(seed).spec()
    })
    .expect("legal");
    RealmField::generate(params).expect("solves")
}

fn built(field: &RealmField, coord: ChunkCoord) -> Chunk {
    ChunkBuild::new(coord)
        .expect("fits")
        .finish(field)
        .expect("builds")
}

#[test]
fn the_phase_sequence_terminates_at_done() {
    let mut phase = Phase::Relief;
    for _ in 0..16 {
        phase = phase.next();
    }
    assert_eq!(phase, Phase::Done);
    assert_eq!(Phase::Done.next(), Phase::Done);
}

#[test]
fn a_build_walks_every_phase_in_order() {
    let field = field(0xC0DE);
    let mut build = ChunkBuild::new(ChunkCoord { x: 0, y: 0 }).expect("fits");
    let expected = [
        Phase::Water,
        Phase::Structures,
        Phase::Climate,
        Phase::Biome,
        Phase::Scatter,
        Phase::Done,
    ];
    assert_eq!(build.phase(), Phase::Relief);
    for want in expected {
        assert_eq!(build.step(&field).expect("steps"), want);
    }
    assert_eq!(build.step(&field).expect("steps"), Phase::Done);
}

#[test]
fn a_partial_build_is_readable_at_every_phase() {
    let field = field(0xC0DE);
    let mut build = ChunkBuild::new(ChunkCoord { x: 1, y: 1 }).expect("fits");
    loop {
        // Nothing here panics or reads uninitialised state: a client draws
        // whatever is ready rather than waiting.
        let partial = build.partial();
        assert_eq!(partial.coord(), ChunkCoord { x: 1, y: 1 });
        let _ = partial.elevation(0, 0);
        let _ = partial.blend(31, 31);
        if build.step(&field).expect("steps") == Phase::Done {
            break;
        }
    }
}

#[test]
fn interrupting_a_build_changes_nothing() {
    let field = field(0xC0DE);
    let coord = ChunkCoord { x: -2, y: 3 };

    let straight_through = built(&field, coord);

    // Stop after every phase, do unrelated work, and resume.
    let mut piecewise = ChunkBuild::new(coord).expect("fits");
    while piecewise.phase() != Phase::Done {
        piecewise.step(&field).expect("steps");
        let _ = built(&field, ChunkCoord { x: 9, y: 9 });
    }
    let resumed = piecewise.finish(&field).expect("finishes");

    assert_eq!(digest::chunk(&straight_through), digest::chunk(&resumed));
}

#[test]
fn a_chunk_is_the_same_alone_as_among_its_neighbours() {
    let field = field(0xB0A7);
    let coord = ChunkCoord { x: 4, y: -5 };
    let alone = digest::chunk(&built(&field, coord));

    for dy in -1..=1 {
        for dx in -1..=1 {
            let _ = built(
                &field,
                ChunkCoord {
                    x: coord.x + dx,
                    y: coord.y + dy,
                },
            );
        }
    }
    assert_eq!(alone, digest::chunk(&built(&field, coord)));
}

#[test]
fn the_shore_transform_measures_the_four_neighbour_distance() {
    // The transform is the one quantity a cell's neighbours can change,
    // so it is checked against hand-computed distances rather than
    // against itself. One water cell at the working grid's corner: every
    // other cell's distance is then its Manhattan offset from it.
    let mut build = ChunkBuild::new(ChunkCoord { x: 0, y: 0 }).expect("fits");
    build.work_shore.fill(u16::MAX);
    build.work_shore[ChunkBuild::work_index(0, 0)] = 0;
    build.measure_shore();

    for wy in 0..WORK_CELLS {
        for wx in 0..WORK_CELLS {
            let measured = build.work_shore[ChunkBuild::work_index(wx, wy)];
            let expected = u16::try_from(wx + wy).expect("inside the grid");
            assert_eq!(measured, expected, "at ({wx}, {wy})");
        }
    }

    // With no water at all nothing is reachable, and the transform says
    // so rather than inventing a distance.
    build.work_shore.fill(u16::MAX);
    build.measure_shore();
    assert!(build.work_shore.iter().all(|&d| d == u16::MAX));
}

#[test]
fn the_halo_is_as_wide_as_the_band_it_has_to_resolve() {
    // A distance the halo cannot measure exactly is one the consumer
    // cannot distinguish: dryness divides by SHORE_CELLS and clamps, so
    // anything past the halo's reach reads as "away from water" whatever
    // a wider window would have said. That equality is the whole argument
    // for this halo width, so it is asserted rather than left to the
    // reader.
    assert_eq!(WORK_CELLS, CHUNK_CELLS + 2 * SHORE_CELLS);

    let field = field(0xB0A7);
    let mut build = ChunkBuild::new(ChunkCoord { x: 0, y: 0 }).expect("fits");
    build.step(&field).expect("relief");
    build.step(&field).expect("water");
    for cy in 0..CHUNK_CELLS {
        for cx in 0..CHUNK_CELLS {
            let shore =
                build.work_shore[ChunkBuild::work_index(cx + SHORE_CELLS, cy + SHORE_CELLS)];
            if u32::from(shore) > SHORE_CELLS {
                let conditions = build.conditions(&field, cx, cy);
                assert!(
                    (conditions.dryness - 1.0).abs() < f64::EPSILON,
                    "an unresolvable distance must saturate"
                );
            }
        }
    }
}

#[test]
fn every_cell_is_coherent() {
    let field = field(0xB0A7);
    let chunk = built(&field, ChunkCoord { x: 0, y: 0 });
    for cy in 0..CHUNK_CELLS {
        for cx in 0..CHUNK_CELLS {
            assert_eq!(chunk.blend(cx, cy).total(), WEIGHT_TOTAL);
            let surface = chunk.surface(cx, cy);
            assert!(
                chunk.water(cx, cy) >= chunk.elevation(cx, cy),
                "a water surface below its bed at ({cx}, {cy})"
            );
            if surface.is_water() {
                assert!(!surface.is_cleared(), "a road was laid across a river");
            }
        }
    }
}

#[test]
fn a_cell_index_wraps_onto_the_chunk() {
    let field = field(1);
    let chunk = built(&field, ChunkCoord { x: 0, y: 0 });
    assert_eq!(
        chunk.elevation(0, 0),
        chunk.elevation(CHUNK_CELLS, CHUNK_CELLS)
    );
}

#[test]
fn the_surface_flags_are_independent() {
    let plain = Surface::default();
    assert_eq!(plain.bits(), 0);
    for flag in [
        Surface::ROAD,
        Surface::SETTLEMENT,
        Surface::CHANNEL,
        Surface::LAKE,
        Surface::SEA,
    ] {
        let only = plain.with(flag);
        let set = [
            only.is_road(),
            only.is_settlement(),
            only.is_channel(),
            only.is_lake(),
            only.is_sea(),
        ];
        assert_eq!(
            set.iter().filter(|held| **held).count(),
            1,
            "one flag, one reader"
        );
        assert_eq!(only.is_cleared(), only.is_road() || only.is_settlement());
        assert_eq!(
            only.is_water(),
            only.is_channel() || only.is_lake() || only.is_sea()
        );
    }
    // A road along a lake shore carries both, and each reader sees its own.
    let both = plain.with(Surface::ROAD).with(Surface::LAKE);
    assert!(both.is_road() && both.is_lake() && both.is_cleared() && both.is_water());
    assert!(!both.is_sea() && !both.is_channel() && !both.is_settlement());
}

#[test]
fn a_chunk_reports_the_bytes_it_holds() {
    let field = field(1);
    let chunk = built(&field, ChunkCoord { x: 0, y: 0 });
    let area = (CHUNK_CELLS as usize).pow(2);
    assert!(
        chunk.payload_bytes() >= area * 8,
        "the arrays are accounted for"
    );
}

#[test]
fn scrubbing_leaves_nothing_readable() {
    let field = field(1);
    let mut chunk = built(&field, ChunkCoord { x: 0, y: 0 });
    chunk.scrub();
    assert!(chunk.scatter().is_empty());
    for cy in 0..CHUNK_CELLS {
        for cx in 0..CHUNK_CELLS {
            assert_eq!(chunk.elevation(cx, cy), crate::geom::Elevation::SEA_LEVEL);
            assert_eq!(chunk.surface(cx, cy), Surface::default());
        }
    }
}
