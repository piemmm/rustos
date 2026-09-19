use alloc::vec::Vec;

use super::{solve, step_of, water_distance, wind_order, LAPSE_RATE};
use crate::geom::{Elevation, Moisture};
use crate::hydrology;
use crate::params::{RealmParams, RealmSpec};
use crate::realm::{try_filled, CoarseSample};
use crate::relief;
use crate::seed::SeedKey;
use crate::uplift::Plates;
use tairix_wintersun_net::value::Facing;

fn solved(wind: Facing) -> (RealmParams, Vec<CoarseSample>) {
    let spec = RealmSpec {
        extent_chunks: 32,
        coarse_samples: 64,
        wind,
        ..RealmParams::winter_default(0x5170_2000).spec()
    };
    let params = RealmParams::new(spec).expect("legal");
    let side = params.coarse_samples() as usize;
    let mut samples = try_filled(side * side, CoarseSample::default()).expect("fits");
    let key = SeedKey::new(params.seed());
    relief::solve(params, key, Plates::new(params), &mut samples).expect("solves");
    hydrology::solve(params, &mut samples).expect("solves");
    solve(params, key, &mut samples).expect("solves");
    (params, samples)
}

#[test]
fn a_wind_always_has_an_upwind_neighbour() {
    for turn in 0..64_u32 {
        let facing = Facing(u16::try_from(turn * 1024).expect("below 65536"));
        let (wx, wy) = facing.unit_vector();
        let offset = (step_of(wx), step_of(wy));
        assert_ne!(offset, (0, 0), "a unit vector has a dominant axis");
    }
}

#[test]
fn the_wind_order_visits_every_cell_upwind_first() {
    let side = 8_u32;
    let area = (side * side) as usize;
    let (wx, wy) = Facing(0x0800).unit_vector();
    let order = wind_order(area, side, wx, wy).expect("fits");
    assert_eq!(order.len(), area);
    let mut seen = try_filled(area, false).expect("fits");
    let mut previous = f64::MIN;
    for raw in order {
        let index = raw as usize;
        assert!(!seen[index], "a cell was visited twice");
        seen[index] = true;
        let column = u32::try_from(index % side as usize).expect("in grid");
        let row = u32::try_from(index / side as usize).expect("in grid");
        let projection = f64::from(column) * wx + f64::from(row) * wy;
        assert!(projection >= previous - 1.0, "the sweep went backwards");
        previous = projection;
    }
    assert!(seen.iter().all(|&visited| visited));
}

#[test]
fn distance_to_water_is_zero_at_water_and_finite_everywhere_reachable() {
    let side = 4_u32;
    let mut samples = try_filled((side * side) as usize, CoarseSample::default()).expect("fits");
    for sample in &mut samples {
        sample.elevation = Elevation::from_units(10.0);
        sample.water = sample.elevation;
    }
    samples[0].elevation = Elevation::from_units(-1.0);
    samples[0].water = Elevation::SEA_LEVEL;

    let distance = water_distance(&samples, side).expect("fits");
    assert_eq!(distance[0], 0);
    assert_eq!(distance[1], 1);
    assert_eq!(distance[(side + 1) as usize], 2);
    assert!(distance.iter().all(|&d| d < u32::MAX));
}

#[test]
fn a_realm_with_no_water_is_uniformly_continental() {
    let side = 3_u32;
    let mut samples = try_filled((side * side) as usize, CoarseSample::default()).expect("fits");
    for sample in &mut samples {
        sample.elevation = Elevation::from_units(50.0);
        sample.water = sample.elevation;
    }
    let distance = water_distance(&samples, side).expect("fits");
    assert!(
        distance.iter().all(|&d| d == u32::MAX),
        "no source, no flood"
    );
}

#[test]
fn standing_water_is_saturated_and_everywhere_is_in_range() {
    let (_, samples) = solved(Facing(0x0800));
    for sample in &samples {
        if sample.is_water() {
            assert_eq!(sample.moisture, Moisture(u16::MAX));
        }
        assert!((0.0..=1.0).contains(&sample.moisture.fraction()));
    }
}

#[test]
fn altitude_cools_the_air() {
    let (_, samples) = solved(Facing(0x0800));
    // Compare samples in the same row, so latitude is held constant.
    let side = 64_usize;
    for row in 0..side {
        let band = &samples[row * side..(row + 1) * side];
        let Some(low) = band
            .iter()
            .filter(|s| !s.elevation.is_submerged())
            .min_by_key(|s| s.elevation)
        else {
            continue;
        };
        let Some(high) = band.iter().max_by_key(|s| s.elevation) else {
            continue;
        };
        let rise = high.elevation.units() - low.elevation.units();
        if rise < 400.0 {
            continue;
        }
        assert!(
            high.temperature < low.temperature,
            "a peak {rise} units above its valley was not colder"
        );
    }
}

#[test]
fn the_lapse_rate_is_the_environmental_one() {
    // 6.5 K per thousand units, which is 6.5 K/km at a unit to the metre.
    assert!((LAPSE_RATE * 1000.0 - 6.5).abs() < 1.0e-9);
}

#[test]
fn a_range_casts_a_rain_shadow() {
    // With an easterly-blowing wind, the lee of a ridge is its east side.
    let (_, samples) = solved(Facing(0));
    let side = 64_usize;
    let mut shadowed = 0;
    let mut compared = 0;
    for row in 8..side - 8 {
        for column in 8..side - 9 {
            let index = row * side + column;
            let crest = samples[index];
            let windward = samples[index - 1];
            let lee = samples[index + 1];
            if crest.is_water() || windward.is_water() || lee.is_water() {
                continue;
            }
            let rise = crest.elevation.units() - windward.elevation.units();
            let fall = crest.elevation.units() - lee.elevation.units();
            if rise < 120.0 || fall < 120.0 {
                continue;
            }
            compared += 1;
            if lee.moisture < windward.moisture {
                shadowed += 1;
            }
        }
    }
    assert!(compared > 0, "the realm has ridges across the wind");
    assert!(
        shadowed * 4 >= compared * 3,
        "only {shadowed} of {compared} ridges cast a shadow"
    );
}

#[test]
fn the_solve_is_a_pure_function_of_its_input() {
    let (_, first) = solved(Facing(0x0800));
    let (_, second) = solved(Facing(0x0800));
    assert_eq!(first, second);
}
