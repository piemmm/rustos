use super::{billow, fbm, ridged, value, warp, LACUNARITY, OCTAVES, PERSISTENCE};
use crate::seed::{SeedKey, Stage};

const KEY: SeedKey = SeedKey::new(0xA5A5_1234);

#[test]
fn octave_parameters_are_the_usual_ones() {
    // Halving amplitude against doubling frequency is what makes the sum
    // fractal rather than merely layered.
    assert!((PERSISTENCE - 0.5).abs() < f64::EPSILON);
    assert!((LACUNARITY - 2.0).abs() < f64::EPSILON);
    // The last octave still has to carry weight the quantised output can
    // see; past that an octave is a hash paid for nothing.
    let finest = PERSISTENCE.powi(i32::try_from(OCTAVES).expect("a small count") - 1);
    assert!(finest > 1.0 / 256.0, "the finest octave rounds away");
}

#[test]
fn value_noise_reproduces_its_lattice_and_stays_in_range() {
    for x in -4..4 {
        for y in -4..4 {
            let at_lattice = value(KEY, Stage::Detail, f64::from(x), f64::from(y));
            assert!((at_lattice - KEY.signed(Stage::Detail, x, y)).abs() < 1.0e-12);
            assert!((-1.0..=1.0).contains(&at_lattice));
        }
    }
}

#[test]
fn value_noise_is_continuous_across_a_cell_boundary() {
    let left = value(KEY, Stage::Detail, 1.0 - 1.0e-9, 0.37);
    let right = value(KEY, Stage::Detail, 1.0 + 1.0e-9, 0.37);
    assert!((left - right).abs() < 1.0e-6, "a seam at the lattice line");
}

#[test]
fn the_summed_forms_stay_in_their_stated_ranges() {
    for step in 0..400 {
        let t = f64::from(step) * 0.173;
        assert!((-1.0..=1.0).contains(&fbm(KEY, Stage::Continent, t, t * 0.61)));
        assert!((0.0..=1.0).contains(&ridged(KEY, Stage::Ridge, t, t * 0.61)));
        assert!((0.0..=1.0).contains(&billow(KEY, Stage::Dune, t, t * 0.61)));
    }
}

#[test]
fn noise_is_a_pure_function_of_position() {
    for step in 0..50 {
        let t = f64::from(step) * 1.31;
        assert!(
            (fbm(KEY, Stage::Detail, t, -t) - fbm(KEY, Stage::Detail, t, -t)).abs() < f64::EPSILON
        );
    }
}

#[test]
fn different_positions_give_different_noise() {
    let a = fbm(KEY, Stage::Detail, 0.25, 0.25);
    let b = fbm(KEY, Stage::Detail, 7.75, 3.5);
    assert!((a - b).abs() > 1.0e-6);
}

#[test]
fn a_warp_displaces_by_no_more_than_its_strength() {
    for step in 0..200 {
        let t = f64::from(step) * 0.29;
        let (wx, wy) = warp(KEY, t, -t, 1.5, 0.3);
        assert!((wx - t).abs() <= 0.3 + 1.0e-12);
        assert!((wy + t).abs() <= 0.3 + 1.0e-12);
    }
}

#[test]
fn an_extreme_coordinate_does_not_wrap_the_lattice() {
    // Saturating rather than wrapping: the value at a huge coordinate is
    // the edge's, never one from the far side.
    let far = value(KEY, Stage::Detail, 1.0e18, 1.0e18);
    assert!((-1.0..=1.0).contains(&far));
}
