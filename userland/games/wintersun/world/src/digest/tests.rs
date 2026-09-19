use super::{chunk, realm, reference_params, world, REFERENCE_DIGEST};
use crate::chunk::ChunkBuild;
use crate::params::{RealmParams, RealmSpec};
use crate::realm::RealmField;
use tairix_wintersun_net::value::ChunkCoord;

#[test]
fn the_reference_realm_validates() {
    // `reference_params` falls back to the shipped default if its own
    // constants ever stop validating; this is what would catch that.
    let spec = reference_params().spec();
    assert!(RealmParams::new(spec).is_ok());
    assert_eq!(spec.seed, 0x5748_4954_4552_534E);
    assert_eq!(reference_params(), RealmParams::new(spec).expect("legal"));
}

#[test]
fn the_reference_digest_is_what_every_target_must_produce() {
    // The one constant the cross-architecture claim is staked on. A change
    // here is a change to every realm ever generated, and is written down
    // deliberately rather than pasted from a failure.
    assert_eq!(
        world(reference_params()).expect("solves"),
        REFERENCE_DIGEST,
        "the generator's output moved"
    );
}

#[test]
fn the_digest_is_stable_across_runs() {
    let params = reference_params();
    assert_eq!(
        world(params).expect("solves"),
        world(params).expect("solves")
    );
}

#[test]
fn a_different_seed_gives_a_different_digest() {
    let mut spec = reference_params().spec();
    spec.seed ^= 1;
    let other = RealmParams::new(spec).expect("legal");
    assert_ne!(world(other).expect("solves"), REFERENCE_DIGEST);
}

#[test]
fn every_parameter_moves_the_digest() {
    let base = reference_params().spec();
    let variations = [
        RealmSpec {
            extent_chunks: 64,
            ..base
        },
        RealmSpec {
            coarse_samples: 128,
            ..base
        },
        RealmSpec { plates: 10, ..base },
        RealmSpec {
            ocean_permille: 421,
            ..base
        },
        RealmSpec {
            relief_units: 1501,
            ..base
        },
        RealmSpec {
            north_celsius: -19,
            ..base
        },
        RealmSpec {
            south_celsius: 12,
            ..base
        },
        RealmSpec {
            wind: tairix_wintersun_net::value::Facing(0x2000),
            ..base
        },
    ];
    for spec in variations {
        let params = RealmParams::new(spec).expect("legal");
        assert_ne!(
            world(params).expect("solves"),
            REFERENCE_DIGEST,
            "a parameter change left the world untouched"
        );
    }
}

#[test]
fn the_realm_and_chunk_digests_are_pure() {
    let field = RealmField::generate(reference_params()).expect("solves");
    assert_eq!(realm(&field), realm(&field));

    let coord = ChunkCoord { x: 0, y: 0 };
    let first = ChunkBuild::new(coord)
        .expect("fits")
        .finish(&field)
        .expect("builds");
    let second = ChunkBuild::new(coord)
        .expect("fits")
        .finish(&field)
        .expect("builds");
    assert_eq!(chunk(&first), chunk(&second));
}

#[test]
fn neighbouring_chunks_digest_differently() {
    let field = RealmField::generate(reference_params()).expect("solves");
    let a = ChunkBuild::new(ChunkCoord { x: 0, y: 0 })
        .expect("fits")
        .finish(&field)
        .expect("builds");
    let b = ChunkBuild::new(ChunkCoord { x: 1, y: 0 })
        .expect("fits")
        .finish(&field)
        .expect("builds");
    assert_ne!(chunk(&a), chunk(&b));
}
