use super::{
    ParamsError, RealmParams, RealmSpec, MAX_COARSE_SAMPLES, MAX_EDGE_CELSIUS, MAX_EXTENT_CHUNKS,
    MAX_PLATES, MAX_RELIEF_UNITS, MIN_COARSE_SAMPLES, MIN_EDGE_CELSIUS, MIN_EXTENT_CHUNKS,
    MIN_PLATES,
};
use crate::geom::CHUNK_CELLS;
use tairix_wintersun_net::value::Facing;

fn legal() -> RealmSpec {
    RealmParams::winter_default(1).spec()
}

#[test]
fn the_shipped_default_validates() {
    let params = RealmParams::new(legal()).expect("the shipped default is legal");
    assert_eq!(params.extent_chunks(), legal().extent_chunks);
    assert_eq!(params.extent_cells(), params.extent_chunks() * CHUNK_CELLS);
    assert_eq!(params.seed(), legal().seed);
    assert_eq!(params.plates(), legal().plates);
    assert_eq!(params.coarse_samples(), legal().coarse_samples);
    assert_eq!(params.ocean_permille(), legal().ocean_permille);
    assert_eq!(params.relief_units(), legal().relief_units);
    assert_eq!(params.wind(), legal().wind);
}

#[test]
fn an_extent_outside_its_bounds_is_refused() {
    for extent in [0, 1, MIN_EXTENT_CHUNKS - 1, MAX_EXTENT_CHUNKS * 2] {
        let spec = RealmSpec {
            extent_chunks: extent,
            ..legal()
        };
        assert_eq!(RealmParams::new(spec), Err(ParamsError::Extent));
    }
}

#[test]
fn an_extent_that_is_not_a_power_of_two_is_refused() {
    let spec = RealmSpec {
        extent_chunks: 200,
        ..legal()
    };
    assert_eq!(RealmParams::new(spec), Err(ParamsError::Extent));
}

#[test]
fn a_coarse_resolution_outside_its_bounds_is_refused() {
    for samples in [0, MIN_COARSE_SAMPLES / 2, MAX_COARSE_SAMPLES * 2, 100] {
        let spec = RealmSpec {
            coarse_samples: samples,
            ..legal()
        };
        assert_eq!(RealmParams::new(spec), Err(ParamsError::CoarseResolution));
    }
}

#[test]
fn a_resolution_finer_than_the_cell_grid_is_refused() {
    // Four chunks is 256 cells; 512 samples would have to sample between
    // cells, which is not a thing the generator can answer.
    let spec = RealmSpec {
        extent_chunks: 4,
        coarse_samples: 512,
        ..legal()
    };
    assert_eq!(RealmParams::new(spec), Err(ParamsError::CoarseResolution));
}

#[test]
fn a_plate_count_outside_its_bounds_is_refused() {
    for plates in [0, MIN_PLATES - 1, MAX_PLATES + 1] {
        let spec = RealmSpec { plates, ..legal() };
        assert_eq!(RealmParams::new(spec), Err(ParamsError::PlateCount));
    }
}

#[test]
fn an_impossible_ocean_fraction_is_refused() {
    let spec = RealmSpec {
        ocean_permille: 1001,
        ..legal()
    };
    assert_eq!(RealmParams::new(spec), Err(ParamsError::OceanFraction));
    let all_sea = RealmSpec {
        ocean_permille: 1000,
        ..legal()
    };
    assert!(RealmParams::new(all_sea).is_ok(), "a water world is legal");
}

#[test]
fn zero_or_excessive_relief_is_refused() {
    for relief in [0, MAX_RELIEF_UNITS + 1, u16::MAX] {
        let spec = RealmSpec {
            relief_units: relief,
            ..legal()
        };
        assert_eq!(RealmParams::new(spec), Err(ParamsError::Relief));
    }
}

#[test]
fn a_temperature_outside_the_band_is_refused_at_either_edge() {
    for celsius in [MIN_EDGE_CELSIUS - 1, MAX_EDGE_CELSIUS + 1] {
        let north = RealmSpec {
            north_celsius: celsius,
            ..legal()
        };
        assert_eq!(RealmParams::new(north), Err(ParamsError::Temperature));
        let south = RealmSpec {
            south_celsius: celsius,
            ..legal()
        };
        assert_eq!(RealmParams::new(south), Err(ParamsError::Temperature));
    }
}

#[test]
fn every_heading_is_a_legal_wind() {
    for turn in [0_u16, 1, 0x4000, 0x8000, 0xC000, u16::MAX] {
        let spec = RealmSpec {
            wind: Facing(turn),
            ..legal()
        };
        assert!(RealmParams::new(spec).is_ok());
    }
}

#[test]
fn the_coarse_step_divides_the_cell_grid_exactly() {
    for extent in [4_u32, 16, 256, 4096] {
        for samples in [32_u32, 64, 256, 512] {
            let spec = RealmSpec {
                extent_chunks: extent,
                coarse_samples: samples,
                ..legal()
            };
            let Ok(params) = RealmParams::new(spec) else {
                continue;
            };
            let step = params.cells_per_coarse();
            assert!(step >= 1);
            assert_eq!(step * samples, extent * CHUNK_CELLS);
        }
    }
}

#[test]
fn the_realm_is_centred_on_the_origin() {
    let params = RealmParams::winter_default(3);
    assert_eq!(params.min_chunk(), -params.max_chunk());
    assert!(params.holds_chunk(0, 0));
    assert!(params.holds_chunk(params.min_chunk(), params.min_chunk()));
    assert!(!params.holds_chunk(params.max_chunk(), 0));
    assert!(!params.holds_chunk(0, params.min_chunk() - 1));
}

#[test]
fn the_largest_realm_stays_inside_a_coordinate() {
    let spec = RealmSpec {
        extent_chunks: MAX_EXTENT_CHUNKS,
        coarse_samples: MAX_COARSE_SAMPLES,
        ..legal()
    };
    let params = RealmParams::new(spec).expect("the ceiling is legal");
    let half_cells = i64::from(params.half_extent_chunks()) * i64::from(CHUNK_CELLS);
    let half_sub_units = half_cells * i64::from(crate::geom::CELL_SUB_UNITS);
    assert!(
        half_sub_units < i64::from(i32::MAX),
        "a position in the largest legal realm must fit a wire coordinate"
    );
}
