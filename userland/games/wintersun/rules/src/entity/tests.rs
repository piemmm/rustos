use tairix_wintersun_net::value::{Direction, EntityId, EntityKind, Facing, WorldPoint};

use super::{Entity, SpawnSpec, RESIDUE_SCALE};
use crate::bounds::{MAX_ARMOUR, MAX_BODY_RADIUS_SUB_UNITS};
use crate::error::RuleError;
use crate::stat::Stats;
use crate::status::{Status, StatusKind};

fn stats() -> Stats {
    Stats::new(10, 20, 30, 40, 50).expect("inside the domain")
}

fn spec() -> SpawnSpec {
    SpawnSpec::new(
        EntityKind(3),
        WorldPoint { x: 100, y: 200 },
        stats(),
        250,
        512,
    )
    .expect("a legal body")
}

fn body() -> Entity {
    Entity::spawn(EntityId(11), spec())
}

#[test]
fn a_body_outside_its_bounds_is_refused() {
    let at = WorldPoint { x: 0, y: 0 };
    assert_eq!(
        SpawnSpec::new(EntityKind(1), at, stats(), 0, 0),
        Err(RuleError::BodyRadius)
    );
    let too_wide = u16::try_from(MAX_BODY_RADIUS_SUB_UNITS + 1).expect("inside u16");
    assert_eq!(
        SpawnSpec::new(EntityKind(1), at, stats(), 0, too_wide),
        Err(RuleError::BodyRadius)
    );
    assert_eq!(
        SpawnSpec::new(EntityKind(1), at, stats(), MAX_ARMOUR + 1, 100),
        Err(RuleError::Armour)
    );
    assert!(SpawnSpec::new(EntityKind(1), at, stats(), MAX_ARMOUR, 100).is_ok());
}

#[test]
fn a_spawned_body_starts_full_and_still() {
    let body = body();
    assert_eq!(body.id(), EntityId(11));
    assert_eq!(body.kind(), EntityKind(3));
    assert_eq!(body.at(), WorldPoint { x: 100, y: 200 });
    assert_eq!(body.health().current(), stats().max_health());
    assert_eq!(body.resource().current(), stats().max_resource());
    assert_eq!(body.residue(), (0, 0));
    assert_eq!(body.acknowledged(), 0);
    assert_eq!(body.held(), Direction::still());
    assert!(body.is_alive());
    assert!(body.status().held().is_empty());
}

#[test]
fn the_wire_view_carries_what_a_client_draws() {
    let state = body().state();
    assert_eq!(state.id, EntityId(11));
    assert_eq!(state.kind, EntityKind(3));
    assert_eq!(state.at, WorldPoint { x: 100, y: 200 });
}

#[test]
fn holding_a_direction_turns_the_body_to_it() {
    let mut body = body();
    body.hold(Direction::new(32_767, 0).expect("east"));
    assert_eq!(body.facing(), Facing(0));
    body.hold(Direction::new(0, 32_767).expect("south"));
    assert_eq!(body.facing(), Facing(0x4000));
    body.hold(Direction::new(-32_767, 0).expect("west"));
    assert_eq!(body.facing(), Facing(0x8000));
    body.hold(Direction::new(0, -32_767).expect("north"));
    assert_eq!(body.facing(), Facing(0xC000));
}

#[test]
fn releasing_the_input_keeps_the_heading() {
    let mut body = body();
    body.hold(Direction::new(0, 32_767).expect("south"));
    body.hold(Direction::still());
    assert_eq!(
        body.facing(),
        Facing(0x4000),
        "facing nowhere is not a direction to snap to"
    );
    assert_eq!(body.held(), Direction::still());
}

#[test]
fn a_body_that_cannot_move_records_its_input_without_turning() {
    let mut body = body();
    body.hold(Direction::new(32_767, 0).expect("east"));
    body.status_mut()
        .apply(Status::new(StatusKind::Root, 0, 30, EntityId(1)).expect("a legal root"));

    let south = Direction::new(0, 32_767).expect("south");
    body.hold(south);
    assert_eq!(
        body.held(),
        south,
        "the input must stay current, or the body resumes the old way when the root lifts"
    );
    assert_eq!(body.facing(), Facing(0), "but a rooted body does not turn");
}

#[test]
fn the_intent_budget_counts_and_resets() {
    let mut body = body();
    assert_eq!(body.admitted_this_tick(), 0);
    body.acknowledge(4);
    body.acknowledge(5);
    assert_eq!(body.admitted_this_tick(), 2);
    assert_eq!(body.acknowledged(), 5);
    body.reset_tick_budget();
    assert_eq!(body.admitted_this_tick(), 0);
    assert_eq!(
        body.acknowledged(),
        5,
        "the sequence is not a per-tick thing"
    );
}

#[test]
fn the_residue_scale_is_the_wire_s_own_fixed_point() {
    assert_eq!(
        RESIDUE_SCALE,
        i64::from(i16::MAX) + 1,
        "a held direction is Q1.15, so the carried remainder shares its scale"
    );
}

#[test]
fn a_body_with_no_health_is_not_alive() {
    let mut body = body();
    body.health_mut().drain(u32::MAX);
    assert!(!body.is_alive());
}
