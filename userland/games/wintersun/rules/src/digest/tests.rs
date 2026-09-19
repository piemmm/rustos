use tairix_wintersun_net::value::{EntityId, WorldPoint};

use tairix_hash::FastHash;

use super::{
    entity, reference_body, reference_run, reference_terrain, scripted, zone as zone_digest,
    REFERENCE_BODIES, REFERENCE_DIGEST, REFERENCE_TICKS,
};
use crate::bounds::MAX_STAT;
use crate::clock::TickRate;
use crate::damage::{Blow, School};
use crate::entity::SpawnSpec;
use crate::stat::{Stat, Stats};
use crate::terrain::{occupiable, Terrain};
use crate::zone::Zone;

#[test]
fn the_scripted_run_agrees_with_the_reference_constant() {
    assert_eq!(
        reference_run().expect("the run completes"),
        REFERENCE_DIGEST,
        "the constant records what the simulation does; moving it is a deliberate act"
    );
}

#[test]
fn the_scripted_run_is_reproducible() {
    let first = reference_run().expect("the run completes");
    let second = reference_run().expect("the run completes");
    assert_eq!(first, second, "nothing in the run may depend on history");
}

#[test]
fn every_reference_body_is_inside_the_bounds_it_is_validated_against() {
    for index in 0..REFERENCE_BODIES {
        let spec = reference_body(index).expect("a legal body");
        for &stat in Stat::ALL {
            assert!(
                spec.stats().get(stat) <= MAX_STAT,
                "body {index} stat {stat:?}"
            );
        }
    }
}

#[test]
fn no_reference_body_begins_inside_an_obstacle() {
    let ground = reference_terrain();
    for index in 0..REFERENCE_BODIES {
        let spec = reference_body(index).expect("a legal body");
        let cell = crate::terrain::cell_at(spec.at());
        assert!(
            occupiable(ground.cell(cell)),
            "body {index} spawns inside an obstacle at {cell:?}"
        );
    }
}

#[test]
fn the_scripted_run_does_what_the_script_says() {
    // A digest is only worth asserting if the session behind it did
    // something: without this, a run that silently stopped simulating would
    // still produce a stable number.
    const {
        assert!(
            REFERENCE_TICKS > 180,
            "the script's last act is at tick 180"
        );
    };
    let mut hasher = FastHash::new();
    let zone = scripted(&mut hasher).expect("the run completes");

    assert_eq!(
        zone.population(),
        REFERENCE_BODIES - 1,
        "the script's last act kills one body, and the reap must have taken it"
    );
    assert_eq!(zone.tick(), REFERENCE_TICKS);
    assert!(
        zone.entities()
            .iter()
            .any(|body| body.at() != reference_body(0).expect("a legal body").at()),
        "the bodies must have moved"
    );
    assert!(
        zone.entities()
            .iter()
            .any(|body| body.health().current() < body.health().max()),
        "and taken damage that was not healed away"
    );
}

#[test]
fn the_digest_moves_when_the_state_does() {
    let mut zone = Zone::new(TickRate::default_rate());
    let id = zone
        .spawn(reference_body(0).expect("a legal body"))
        .expect("room");
    let before = zone_digest(&zone);
    zone.apply_blow(
        None,
        id,
        &Blow::new(School::Physical, 40, 0).expect("a legal blow"),
    )
    .expect("landed");
    assert_ne!(
        before,
        zone_digest(&zone),
        "a health change must be visible"
    );
}

#[test]
fn the_digest_distinguishes_two_bodies_that_differ_only_by_position() {
    let stats = Stats::new(10, 10, 10, 10, 10).expect("inside the domain");
    let mut west = Zone::new(TickRate::default_rate());
    let mut east = Zone::new(TickRate::default_rate());
    west.spawn(
        SpawnSpec::new(
            tairix_wintersun_net::value::EntityKind(1),
            WorldPoint { x: 0, y: 0 },
            stats,
            0,
            256,
        )
        .expect("a legal body"),
    )
    .expect("room");
    east.spawn(
        SpawnSpec::new(
            tairix_wintersun_net::value::EntityKind(1),
            WorldPoint { x: 1, y: 0 },
            stats,
            0,
            256,
        )
        .expect("a legal body"),
    )
    .expect("room");
    assert_ne!(zone_digest(&west), zone_digest(&east));
}

#[test]
fn a_body_hashes_to_something_a_desync_can_be_bisected_by() {
    let mut zone = Zone::new(TickRate::default_rate());
    let first = zone
        .spawn(reference_body(0).expect("a legal body"))
        .expect("room");
    let second = zone
        .spawn(reference_body(1).expect("a legal body"))
        .expect("room");
    let a = entity(zone.entity(first).expect("present"));
    let b = entity(zone.entity(second).expect("present"));
    assert_ne!(a, b, "two different bodies must not share a hash");
    assert_eq!(
        a,
        entity(zone.entity(first).expect("present")),
        "and one body's hash must be stable"
    );
    assert_eq!(first, EntityId(1));
    assert_eq!(second, EntityId(2));
}
