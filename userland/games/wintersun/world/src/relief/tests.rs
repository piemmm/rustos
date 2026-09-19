use super::{sea_level, solve};
use crate::params::{RealmParams, RealmSpec};
use crate::realm::{try_filled, CoarseSample};
use crate::seed::SeedKey;
use crate::uplift::Plates;

fn field(spec: RealmSpec) -> (RealmParams, alloc::vec::Vec<CoarseSample>) {
    let params = RealmParams::new(spec).expect("legal");
    let side = params.coarse_samples() as usize;
    let mut samples = try_filled(side * side, CoarseSample::default()).expect("fits");
    solve(
        params,
        SeedKey::new(params.seed()),
        Plates::new(params),
        &mut samples,
    )
    .expect("solves");
    (params, samples)
}

fn small(ocean_permille: u16) -> RealmSpec {
    RealmSpec {
        extent_chunks: 32,
        coarse_samples: 64,
        ocean_permille,
        ..RealmParams::winter_default(0x00C0_FFEE).spec()
    }
}

#[test]
fn the_requested_fraction_of_the_realm_is_submerged() {
    for permille in [0_u16, 200, 500, 800, 1000] {
        let (_, samples) = field(small(permille));
        let submerged = samples
            .iter()
            .filter(|sample| sample.elevation.is_submerged())
            .count();
        let fraction = submerged * 1000 / samples.len();
        assert!(
            fraction.abs_diff(usize::from(permille)) <= 30,
            "asked for {permille} per mille of sea, got {fraction}"
        );
    }
}

#[test]
fn elevations_stay_inside_the_declared_relief() {
    let (params, samples) = field(small(400));
    let relief = f64::from(params.relief_units());
    for sample in &samples {
        assert!(sample.elevation.units() <= relief + 1.0);
        assert!(sample.elevation.units() >= -relief - 1.0);
        assert!(sample.water >= sample.elevation || sample.elevation.is_submerged());
    }
}

#[test]
fn belt_strength_is_recorded_for_every_sample() {
    let (_, samples) = field(small(400));
    assert!(
        samples.iter().any(|sample| sample.belt > 0),
        "a realm with a dozen plates has convergent boundaries somewhere"
    );
}

#[test]
fn relief_is_a_pure_function_of_the_parameters() {
    let (_, first) = field(small(400));
    let (_, second) = field(small(400));
    assert_eq!(first, second);
}

#[test]
fn different_seeds_give_different_ground() {
    let (_, first) = field(small(400));
    let (_, second) = field(RealmSpec {
        seed: 0xD1FF,
        ..small(400)
    });
    assert_ne!(first, second);
}

#[test]
fn the_quantile_cut_is_ordered_and_total() {
    let raw = [0.4_f64, -0.9, 0.1, -0.2, 0.7];
    let none = sea_level(&raw, 0).expect("fits");
    assert!(none <= -0.9);
    let all = sea_level(&raw, 1000).expect("fits");
    assert!(all >= 0.7 - 1.0e-6);
    let half = sea_level(&raw, 500).expect("fits");
    assert!(half > -0.9 && half < 0.7);
    // An empty field has no quantile, and asking for one is not a crash.
    assert!(sea_level(&[], 500).expect("fits") <= f64::MIN);
}
