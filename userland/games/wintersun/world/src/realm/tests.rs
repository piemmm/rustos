use super::{clamped_index, try_filled, RealmField};
use crate::error::WorldError;
use crate::geom::{signed, CellCoord, CHUNK_CELLS};
use crate::params::{RealmParams, RealmSpec};
use tairix_wintersun_net::value::ChunkCoord;

fn small(seed: u64) -> RealmParams {
    RealmParams::new(RealmSpec {
        seed,
        extent_chunks: 32,
        coarse_samples: 64,
        ..RealmParams::winter_default(seed).spec()
    })
    .expect("legal")
}

#[test]
fn a_grid_index_clamps_rather_than_wrapping() {
    assert_eq!(clamped_index(0, 0, 8), 0);
    assert_eq!(clamped_index(7, 7, 8), 63);
    assert_eq!(clamped_index(-5, -5, 8), 0);
    assert_eq!(clamped_index(99, 99, 8), 63);
    assert_eq!(clamped_index(-1, 3, 8), 24);
}

#[test]
fn a_fallible_allocation_yields_a_refusal_rather_than_an_abort() {
    let refused: Result<alloc::vec::Vec<u64>, WorldError> = try_filled(usize::MAX / 8, 0);
    assert_eq!(refused, Err(WorldError::OutOfMemory));
}

#[test]
fn a_realm_solves_and_every_sample_is_coherent() {
    let field = RealmField::generate(small(0x7E57)).expect("solves");
    assert_eq!(field.side(), 64);
    assert_eq!(field.samples().len(), 64 * 64);
    for sample in field.samples() {
        assert!(sample.water >= sample.elevation || sample.elevation.is_submerged());
        assert!(sample.discharge >= 1);
    }
}

#[test]
fn generation_is_a_pure_function_of_its_parameters() {
    let first = RealmField::generate(small(0x7E57)).expect("solves");
    let second = RealmField::generate(small(0x7E57)).expect("solves");
    assert_eq!(first.samples(), second.samples());
    assert_eq!(first.sites(), second.sites());
    assert_eq!(first.roads(), second.roads());
    assert_eq!(first.landmarks(), second.landmarks());
}

#[test]
fn different_seeds_give_different_realms() {
    let first = RealmField::generate(small(1)).expect("solves");
    let second = RealmField::generate(small(2)).expect("solves");
    assert_ne!(first.samples(), second.samples());
}

#[test]
fn a_sample_query_clamps_to_the_realm() {
    let field = RealmField::generate(small(11)).expect("solves");
    let edge = field.sample(0, 0);
    assert_eq!(field.sample(-100, -100), edge);
    let far = field.sample(63, 63);
    assert_eq!(field.sample(9999, 9999), far);
}

#[test]
fn a_cell_on_a_sample_interpolates_to_that_sample_exactly() {
    let field = RealmField::generate(small(12)).expect("solves");
    let params = field.params();
    let origin = params.min_chunk() * signed(CHUNK_CELLS);
    let step = signed(params.cells_per_coarse());
    for sx in [0_i32, 1, 17, 40] {
        for sy in [0_i32, 3, 22, 61] {
            let cell = CellCoord::new(origin + sx * step, origin + sy * step);
            let (gx, gy) = field.grid_position(cell);
            assert!((gx - f64::from(sx)).abs() < 1.0e-9);
            assert!((gy - f64::from(sy)).abs() < 1.0e-9);
            let sample = field.sample(sx, sy);
            assert!(
                (field.elevation_units_at(gx, gy) - sample.elevation.units()).abs() < 1.0e-9,
                "interpolation must reproduce the sample it lands on"
            );
        }
    }
}

#[test]
fn a_chunks_grid_position_is_its_north_west_corner() {
    let field = RealmField::generate(small(13)).expect("solves");
    for chunk in [ChunkCoord { x: 0, y: 0 }, ChunkCoord { x: -4, y: 6 }] {
        let (gx, gy) = field.chunk_grid_position(chunk);
        let corner = field.grid_position(crate::geom::chunk_origin(chunk));
        assert!((gx - corner.0).abs() < f64::EPSILON);
        assert!((gy - corner.1).abs() < f64::EPSILON);
    }
}

#[test]
fn interpolated_fields_stay_inside_their_samples_range() {
    let field = RealmField::generate(small(14)).expect("solves");
    let lowest = field
        .samples()
        .iter()
        .map(|s| s.elevation)
        .min()
        .expect("non-empty");
    let highest = field
        .samples()
        .iter()
        .map(|s| s.elevation)
        .max()
        .expect("non-empty");
    for step in 0..200 {
        let g = f64::from(step) * 0.31;
        let height = field.elevation_units_at(g, g * 0.7);
        assert!(height >= lowest.units() - 1.0e-6);
        assert!(height <= highest.units() + 1.0e-6);
        assert!((0.0..=1.0).contains(&field.belt_at(g, g * 0.7)));
        assert!((0.0..=1.0).contains(&field.moisture_at(g, g * 0.7)));
    }
}
