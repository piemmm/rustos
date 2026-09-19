use tairix_util::mathf;

use super::{isqrt_round, Plates};
use crate::params::RealmParams;

fn plates(count: u32) -> Plates {
    let mut spec = RealmParams::winter_default(0xBEEF).spec();
    spec.plates = count;
    Plates::new(RealmParams::new(spec).expect("legal"))
}

#[test]
fn the_rounded_integer_square_root_is_exact() {
    for value in 0_u32..2000 {
        let root = isqrt_round(value);
        let low = f64::from(value).sqrt() - 0.5;
        let high = f64::from(value).sqrt() + 0.5;
        assert!(f64::from(root) >= low - 1.0e-9 && f64::from(root) <= high + 1.0e-9);
    }
    assert_eq!(isqrt_round(0), 0);
    assert_eq!(isqrt_round(1), 1);
    assert_eq!(isqrt_round(2), 1);
    assert_eq!(isqrt_round(3), 2);
    assert_eq!(isqrt_round(u32::MAX), 65536);
}

#[test]
fn the_grid_never_collapses_below_two_cells() {
    for count in [4_u32, 9, 12, 64] {
        assert!(plates(count).grid() >= 2);
    }
}

#[test]
fn a_plate_is_a_pure_function_of_its_cell() {
    let field = plates(12);
    for cx in -4..4 {
        for cy in -4..4 {
            assert_eq!(field.plate(cx, cy), field.plate(cx, cy));
        }
    }
}

#[test]
fn a_seed_site_stays_inside_its_own_cell() {
    let field = plates(16);
    for cx in 0..4 {
        for cy in 0..4 {
            let plate = field.plate(cx, cy);
            assert!((plate.site.0 - f64::from(cx) - 0.5).abs() < 0.5);
            assert!((plate.site.1 - f64::from(cy) - 0.5).abs() < 0.5);
            assert!((0.0..=1.0).contains(&plate.buoyancy));
        }
    }
}

#[test]
fn the_nearest_plate_is_genuinely_the_nearest() {
    let field = plates(25);
    let grid = f64::from(field.grid());
    for step in 0..200 {
        let x = f64::from(step) * 0.031 * grid;
        let y = f64::from(step) * 0.017 * grid;
        let (near, far) = field.nearest_two(x, y);
        let d_near = (near.site.0 - x).powi(2) + (near.site.1 - y).powi(2);
        let d_far = (far.site.0 - x).powi(2) + (far.site.1 - y).powi(2);
        assert!(d_near <= d_far, "nearest_two must order its answer");
        assert_ne!(near.cell, far.cell, "two distinct plates meet everywhere");
        // Nothing in the surrounding ring is closer than the answer.
        for dx in -2..=2 {
            for dy in -2..=2 {
                let floor = |value: f64| mathf::round_i32(mathf::floor(value));
                let other = field.plate(floor(x) + dx, floor(y) + dy);
                if other.cell == near.cell {
                    continue;
                }
                let d = (other.site.0 - x).powi(2) + (other.site.1 - y).powi(2);
                assert!(d_near <= d + 1.0e-12, "a nearer seed was missed");
            }
        }
    }
}

#[test]
fn tectonics_stay_inside_their_stated_ranges() {
    let field = plates(12);
    let grid = f64::from(field.grid());
    for sx in 0..80 {
        for sy in 0..80 {
            let x = f64::from(sx) / 80.0 * grid;
            let y = f64::from(sy) / 80.0 * grid;
            let tectonics = field.tectonics(x, y);
            assert!((-1.0..=1.0).contains(&tectonics.uplift));
            assert!((0.0..=1.0).contains(&tectonics.belt));
            assert!((0.0..=1.0).contains(&tectonics.buoyancy));
            let boundary = field.boundary(x, y);
            assert!(boundary.distance >= 0.0);
            assert!((0.0..=1.0).contains(&boundary.buoyancy));
        }
    }
}

#[test]
fn a_belt_only_appears_where_plates_converge() {
    let field = plates(16);
    let grid = f64::from(field.grid());
    for sx in 0..60 {
        for sy in 0..60 {
            let x = f64::from(sx) / 60.0 * grid;
            let y = f64::from(sy) / 60.0 * grid;
            if field.tectonics(x, y).belt > 0.0 {
                assert!(field.boundary(x, y).convergence > 0.0);
            }
        }
    }
}
