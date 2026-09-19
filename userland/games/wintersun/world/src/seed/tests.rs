use alloc::vec::Vec;

use super::{SeedKey, Stage};

const STAGES: [Stage; 11] = [
    Stage::Plates,
    Stage::Continent,
    Stage::Ridge,
    Stage::Warp,
    Stage::Detail,
    Stage::Dune,
    Stage::Climate,
    Stage::Biome,
    Stage::Scatter,
    Stage::Settlement,
    Stage::Landmark,
];

#[test]
fn a_lattice_value_is_a_pure_function_of_its_inputs() {
    let key = SeedKey::new(0x1234_5678_9ABC_DEF0);
    for stage in STAGES {
        for x in -3..3 {
            for y in -3..3 {
                assert_eq!(key.lattice(stage, x, y), key.lattice(stage, x, y));
            }
        }
    }
}

#[test]
fn stages_are_domain_separated() {
    let key = SeedKey::new(7);
    let mut seen = Vec::new();
    for stage in STAGES {
        let value = key.lattice(stage, 11, -4);
        assert!(!seen.contains(&value), "two stages share a lattice value");
        seen.push(value);
    }
}

#[test]
fn neighbouring_coordinates_are_uncorrelated() {
    let key = SeedKey::new(99);
    let a = key.lattice(Stage::Detail, 0, 0);
    assert_ne!(a, key.lattice(Stage::Detail, 1, 0));
    assert_ne!(a, key.lattice(Stage::Detail, 0, 1));
    assert_ne!(a, key.lattice(Stage::Detail, -1, 0));
}

#[test]
fn seeds_give_different_worlds() {
    assert_ne!(
        SeedKey::new(1).lattice(Stage::Continent, 5, 5),
        SeedKey::new(2).lattice(Stage::Continent, 5, 5)
    );
}

#[test]
fn unit_draws_stay_in_range_and_signed_draws_straddle_zero() {
    let key = SeedKey::new(0xFEED);
    let mut negative = 0;
    let mut positive = 0;
    for x in 0..2000 {
        let unit = key.unit(Stage::Biome, x, 0);
        assert!((0.0..1.0).contains(&unit));
        let signed = key.signed(Stage::Biome, x, 0);
        assert!((-1.0..1.0).contains(&signed));
        if signed < 0.0 {
            negative += 1;
        } else {
            positive += 1;
        }
    }
    assert!(negative > 800 && positive > 800, "signed draws are skewed");
}

#[test]
fn a_stream_is_reproducible_and_bounded() {
    let key = SeedKey::new(4242);
    let mut first = key.stream(Stage::Settlement, 3, 9);
    let mut second = key.stream(Stage::Settlement, 3, 9);
    for _ in 0..64 {
        let a = first.unit();
        assert!((0.0..1.0).contains(&a));
        assert!((a - second.unit()).abs() < f64::EPSILON);
    }
}

#[test]
fn range_draws_stay_inside_their_bounds() {
    let key = SeedKey::new(5);
    let mut stream = key.stream(Stage::Scatter, 0, 0);
    for _ in 0..500 {
        let value = stream.range_u32(7, 11);
        assert!((7..=11).contains(&value));
    }
    // An empty range yields its low bound rather than dividing by zero.
    assert_eq!(stream.range_u32(9, 3), 9);
    // The whole range is legal, and the width does not overflow.
    let _ = stream.range_u32(0, u32::MAX);
}
