use tairix_wintersun_net::client::{Intent, IntentKind, ItemOp};
use tairix_wintersun_net::value::{
    ActionId, Aim, Direction, EntityId, EntityKind, PlayEvent, SlotIndex, SpellId, TickInstant,
    TickPhase, WorldPoint,
};

use super::Zone;
use crate::bounds::{INTENT_LOOKBEHIND_TICKS, MAX_INTENTS_PER_TICK};
use crate::clock::TickRate;
use crate::damage::{Blow, School};
use crate::entity::SpawnSpec;
use crate::error::{Refusal, ZoneError};
use crate::stat::Stats;
use crate::status::{Status, StatusKind};
use crate::terrain::SyntheticTerrain;

const EAST: i16 = 32_767;

fn ground() -> SyntheticTerrain {
    SyntheticTerrain::open()
}

fn spec(at: WorldPoint, agility: u16) -> SpawnSpec {
    let stats = Stats::new(100, agility, 100, 100, 100).expect("inside the domain");
    SpawnSpec::new(EntityKind(1), at, stats, 0, 256).expect("a legal body")
}

fn zone_with(bodies: &[(i32, i32)]) -> (Zone, alloc::vec::Vec<EntityId>) {
    let mut zone = Zone::new(TickRate::default_rate());
    let ids = bodies
        .iter()
        .map(|&(x, y)| {
            zone.spawn(spec(WorldPoint { x, y }, 500))
                .expect("room for a body")
        })
        .collect();
    (zone, ids)
}

fn move_intent(sequence: u64, tick: u64, x: i16, y: i16) -> Intent {
    Intent {
        sequence,
        sampled: TickInstant {
            tick,
            phase: TickPhase(0),
        },
        kind: IntentKind::Move(Direction::new(x, y).expect("a unit vector")),
    }
}

#[test]
fn a_new_zone_is_empty_at_tick_zero() {
    let zone = Zone::new(TickRate::default_rate());
    assert_eq!(zone.tick(), 0);
    assert_eq!(zone.population(), 0);
    assert_eq!(zone.rate(), TickRate::default_rate());
    assert!(zone.events().is_empty() && zone.refusals().is_empty());
}

#[test]
fn identities_are_minted_in_order_and_the_table_stays_searchable() {
    let (mut zone, ids) = zone_with(&[(0, 0), (4_096, 0), (8_192, 0)]);
    assert_eq!(ids, [EntityId(1), EntityId(2), EntityId(3)]);
    for id in &ids {
        assert_eq!(zone.entity(*id).expect("present").id(), *id);
    }
    assert!(zone.entity(EntityId(99)).is_none());
    assert!(zone.despawn(ids[1]));
    assert!(!zone.despawn(ids[1]));
    assert_eq!(zone.population(), 2);
    assert_eq!(zone.entity(ids[2]).expect("still there").id(), ids[2]);
}

#[test]
fn an_admitted_move_carries_the_body() {
    let (mut zone, ids) = zone_with(&[(512, 512)]);
    zone.submit(ids[0], &move_intent(1, 0, EAST, 0))
        .expect("admitted");
    zone.step(&ground()).expect("stepped");
    assert_eq!(zone.tick(), 1);
    assert!(zone.entity(ids[0]).expect("present").at().x > 512);
    assert!(zone.refusals().is_empty());
}

#[test]
fn a_replayed_sequence_is_refused() {
    let (mut zone, ids) = zone_with(&[(512, 512)]);
    zone.submit(ids[0], &move_intent(5, 0, EAST, 0))
        .expect("admitted");
    assert_eq!(
        zone.submit(ids[0], &move_intent(5, 0, EAST, 0)),
        Err(Refusal::Replayed)
    );
    assert_eq!(
        zone.submit(ids[0], &move_intent(4, 0, EAST, 0)),
        Err(Refusal::Replayed),
        "an older sequence is a replay too"
    );
    assert!(zone.submit(ids[0], &move_intent(6, 0, EAST, 0)).is_ok());
}

#[test]
fn a_sample_from_the_future_is_refused() {
    let (mut zone, ids) = zone_with(&[(512, 512)]);
    assert_eq!(
        zone.submit(ids[0], &move_intent(1, 1, EAST, 0)),
        Err(Refusal::SampledInTheFuture)
    );
}

#[test]
fn a_back_dated_sample_gains_no_further_priority() {
    let (mut zone, ids) = zone_with(&[(512, 512)]);
    for _ in 0..100 {
        zone.step(&ground()).expect("stepped");
    }
    let tick = zone.tick();
    let edge = zone
        .submit(
            ids[0],
            &move_intent(1, tick - INTENT_LOOKBEHIND_TICKS - 1, EAST, 0),
        )
        .expect("clamped, not refused");
    let far = zone
        .submit(ids[0], &move_intent(2, 0, EAST, 0))
        .expect("clamped, not refused");
    assert_eq!(edge, far, "every over-old sample lands on one edge");
}

#[test]
fn a_client_cannot_spend_more_than_its_tick_budget() {
    let (mut zone, ids) = zone_with(&[(512, 512)]);
    for sequence in 1..=u64::from(MAX_INTENTS_PER_TICK) {
        zone.submit(ids[0], &move_intent(sequence, 0, EAST, 0))
            .expect("inside the budget");
    }
    assert_eq!(
        zone.submit(
            ids[0],
            &move_intent(u64::from(MAX_INTENTS_PER_TICK) + 1, 0, EAST, 0)
        ),
        Err(Refusal::TooManyThisTick)
    );
    zone.step(&ground()).expect("stepped");
    assert!(
        zone.submit(
            ids[0],
            &move_intent(u64::from(MAX_INTENTS_PER_TICK) + 1, 1, EAST, 0)
        )
        .is_ok(),
        "the budget refills each tick"
    );
}

#[test]
fn the_submission_queue_is_reserved_for_the_whole_population() {
    // `submit` pushes without reserving, which is only safe because
    // spawning grew the queue to the worst case first. A reservation that
    // covered one body's budget rather than the population's would leave
    // the push to grow the vector — and an allocation failure there aborts
    // instead of failing closed.
    let mut zone = Zone::new(TickRate::default_rate());
    for index in 0..12_i32 {
        zone.spawn(spec(
            WorldPoint {
                x: 512 + index * 8_192,
                y: 512,
            },
            100,
        ))
        .expect("room");
        assert!(
            zone.pending.capacity() >= zone.population() * usize::from(MAX_INTENTS_PER_TICK),
            "the queue must hold every body's whole budget at once"
        );
    }
}

#[test]
fn every_body_can_spend_its_whole_budget_in_one_tick() {
    let mut zone = Zone::new(TickRate::default_rate());
    let mut ids = alloc::vec::Vec::new();
    for index in 0..12_i32 {
        ids.push(
            zone.spawn(spec(
                WorldPoint {
                    x: 512 + index * 8_192,
                    y: 512,
                },
                100,
            ))
            .expect("room"),
        );
    }
    for id in &ids {
        for sequence in 1..=u64::from(MAX_INTENTS_PER_TICK) {
            zone.submit(*id, &move_intent(sequence, 0, EAST, 0))
                .expect("inside the budget");
        }
    }
    assert_eq!(
        zone.pending.len(),
        ids.len() * usize::from(MAX_INTENTS_PER_TICK)
    );
    zone.step(&ground()).expect("stepped");
    assert!(zone.pending.is_empty(), "the step consumed every one");
}

#[test]
fn an_intent_for_a_body_the_zone_does_not_hold_is_refused() {
    let (mut zone, _) = zone_with(&[(512, 512)]);
    assert_eq!(
        zone.submit(EntityId(404), &move_intent(1, 0, EAST, 0)),
        Err(Refusal::NoSuchEntity(EntityId(404)))
    );
}

#[test]
fn no_amount_of_claimed_elapsed_time_moves_a_body_further() {
    // The realm computes distance from its own speed, so a client sending
    // its whole tick budget covers exactly the same ground as one sending a
    // single intent.
    let (mut greedy, greedy_ids) = zone_with(&[(512, 512)]);
    let (mut honest, honest_ids) = zone_with(&[(512, 512)]);
    for sequence in 1..=u64::from(MAX_INTENTS_PER_TICK) {
        greedy
            .submit(greedy_ids[0], &move_intent(sequence, 0, EAST, 0))
            .expect("inside the budget");
    }
    honest
        .submit(honest_ids[0], &move_intent(1, 0, EAST, 0))
        .expect("admitted");
    greedy.step(&ground()).expect("stepped");
    honest.step(&ground()).expect("stepped");
    assert_eq!(
        greedy.entity(greedy_ids[0]).expect("present").at(),
        honest.entity(honest_ids[0]).expect("present").at()
    );
}

#[test]
fn a_root_stops_a_body_without_losing_its_input() {
    let (mut zone, ids) = zone_with(&[(512, 512)]);
    zone.apply_status(
        ids[0],
        Status::new(StatusKind::Root, 0, 2, EntityId(1)).expect("a legal root"),
    )
    .expect("applied");

    zone.submit(ids[0], &move_intent(1, 0, EAST, 0))
        .expect("admitted");
    zone.step(&ground()).expect("stepped");
    assert_eq!(
        zone.entity(ids[0]).expect("present").at().x,
        512,
        "a rooted body does not move"
    );
    assert!(
        zone.refusals().is_empty(),
        "and its input is honoured rather than refused"
    );

    // Change direction while rooted; when it lifts, the new direction is
    // what takes effect.
    zone.submit(ids[0], &move_intent(2, 1, 0, EAST))
        .expect("admitted");
    zone.step(&ground()).expect("stepped");
    zone.step(&ground()).expect("stepped");
    let body = zone.entity(ids[0]).expect("present");
    assert!(body.at().y > 512, "it went the way it was last told");
    assert_eq!(body.at().x, 512);
}

#[test]
fn a_stun_refuses_a_discrete_action_and_a_silence_only_a_cast() {
    let (mut zone, ids) = zone_with(&[(512, 512)]);
    let aim = Aim {
        target: None,
        at: WorldPoint { x: 0, y: 0 },
        viewed: TickInstant {
            tick: 0,
            phase: TickPhase(0),
        },
    };
    zone.apply_status(
        ids[0],
        Status::new(StatusKind::Stun, 0, 5, EntityId(1)).expect("a legal stun"),
    )
    .expect("applied");
    zone.submit(
        ids[0],
        &Intent {
            sequence: 1,
            sampled: TickInstant {
                tick: 0,
                phase: TickPhase(0),
            },
            kind: IntentKind::Action {
                action: ActionId(1),
                aim,
            },
        },
    )
    .expect("admitted");
    zone.step(&ground()).expect("stepped");
    assert_eq!(
        zone.refusals().first().map(|r| r.reason),
        Some(Refusal::Stunned)
    );

    let (mut zone, ids) = zone_with(&[(512, 512)]);
    zone.apply_status(
        ids[0],
        Status::new(StatusKind::Silence, 0, 5, EntityId(1)).expect("a legal silence"),
    )
    .expect("applied");
    zone.submit(
        ids[0],
        &Intent {
            sequence: 1,
            sampled: TickInstant {
                tick: 0,
                phase: TickPhase(0),
            },
            kind: IntentKind::Cast {
                spell: SpellId(1),
                aim,
            },
        },
    )
    .expect("admitted");
    zone.step(&ground()).expect("stepped");
    assert_eq!(
        zone.refusals().first().map(|r| r.reason),
        Some(Refusal::Silenced)
    );
}

#[test]
fn an_intent_no_table_resolves_is_refused_with_its_reason() {
    let (mut zone, ids) = zone_with(&[(512, 512)]);
    zone.submit(
        ids[0],
        &Intent {
            sequence: 1,
            sampled: TickInstant {
                tick: 0,
                phase: TickPhase(0),
            },
            kind: IntentKind::Item {
                op: ItemOp::Drop,
                slot: SlotIndex(0),
                target_slot: None,
            },
        },
    )
    .expect("admitted");
    zone.step(&ground()).expect("stepped");
    let refused = zone.refusals().first().copied().expect("one refusal");
    assert_eq!(refused.reason, Refusal::Unresolvable);
    assert_eq!(refused.sequence, 1);
    assert_eq!(refused.entity, ids[0]);
}

#[test]
fn notifications_and_refusals_are_cleared_by_the_next_step() {
    let (mut zone, ids) = zone_with(&[(512, 512)]);
    zone.apply_blow(
        None,
        ids[0],
        &Blow::new(School::Physical, 10, 0).expect("a legal blow"),
    )
    .expect("landed");
    assert_eq!(zone.events().len(), 1);
    zone.step(&ground()).expect("stepped");
    assert!(zone.events().is_empty(), "each step reports its own tick");
}

#[test]
fn a_blow_runs_the_pipeline_and_a_death_is_reaped_with_its_notice() {
    let (mut zone, ids) = zone_with(&[(512, 512), (100_000, 0)]);
    let fatal = Blow::new(School::Physical, 900_000, 1000).expect("a legal blow");
    let landed = zone
        .apply_blow(Some(ids[1]), ids[0], &fatal)
        .expect("landed");
    assert!(landed.to_health > 0);
    assert!(!zone.entity(ids[0]).expect("present").is_alive());

    zone.step(&ground()).expect("stepped");
    assert!(zone.entity(ids[0]).is_none(), "the dead leave");
    assert_eq!(zone.population(), 1);
    assert!(zone
        .events()
        .iter()
        .any(|event| matches!(event.event, PlayEvent::Death { entity } if entity == ids[0])));
}

#[test]
fn a_blow_or_a_heal_for_a_missing_body_is_refused() {
    let (mut zone, ids) = zone_with(&[(512, 512)]);
    let blow = Blow::new(School::Physical, 10, 0).expect("a legal blow");
    assert_eq!(
        zone.apply_blow(None, EntityId(404), &blow),
        Err(ZoneError::Refused(Refusal::NoSuchEntity(EntityId(404))))
    );
    assert_eq!(
        zone.apply_blow(Some(EntityId(404)), ids[0], &blow),
        Err(ZoneError::Refused(Refusal::NoSuchEntity(EntityId(404)))),
        "an absent attacker is refused too"
    );
    assert_eq!(
        zone.apply_heal(None, EntityId(404), 10),
        Err(ZoneError::Refused(Refusal::NoSuchEntity(EntityId(404))))
    );
}

#[test]
fn a_heal_is_bounded_by_the_pool_it_fills() {
    let (mut zone, ids) = zone_with(&[(512, 512)]);
    assert_eq!(zone.apply_heal(None, ids[0], 500), Ok(0), "already full");
    zone.apply_blow(
        None,
        ids[0],
        &Blow::new(School::Physical, 100, 0).expect("a legal blow"),
    )
    .expect("landed");
    let restored = zone.apply_heal(None, ids[0], 500).expect("healed");
    assert!(restored > 0 && restored <= 100);
}

#[test]
fn a_periodic_status_ticks_with_the_step() {
    let (mut zone, ids) = zone_with(&[(512, 512)]);
    let full = zone.entity(ids[0]).expect("present").health().current();
    zone.apply_status(
        ids[0],
        Status::new(StatusKind::Bleed, 9, 3, EntityId(1)).expect("a legal bleed"),
    )
    .expect("applied");
    for expected in 1..=3 {
        zone.step(&ground()).expect("stepped");
        assert_eq!(
            zone.entity(ids[0]).expect("present").health().current(),
            full - 9 * expected
        );
    }
    zone.step(&ground()).expect("stepped");
    assert_eq!(
        zone.entity(ids[0]).expect("present").health().current(),
        full - 27,
        "it stopped when it expired"
    );
}

#[test]
fn overlapping_bodies_are_pushed_apart() {
    let (mut zone, ids) = zone_with(&[(4_096, 4_096), (4_196, 4_096)]);
    zone.step(&ground()).expect("stepped");
    let a = zone.entity(ids[0]).expect("present").at();
    let b = zone.entity(ids[1]).expect("present").at();
    let gap = i64::from(b.x) - i64::from(a.x);
    assert!(gap > 100, "they were 100 apart inside a 512 reach");
    assert!(a.x < 4_096 && b.x > 4_196, "both moved, symmetrically");
}

#[test]
fn separation_is_not_biased_by_the_order_bodies_were_spawned() {
    let (mut west_first, west_ids) = zone_with(&[(4_096, 4_096), (4_196, 4_096)]);
    let (mut east_first, east_ids) = zone_with(&[(4_196, 4_096), (4_096, 4_096)]);
    west_first.step(&ground()).expect("stepped");
    east_first.step(&ground()).expect("stepped");
    let west_gap = i64::from(west_first.entity(west_ids[1]).expect("present").at().x)
        - i64::from(west_first.entity(west_ids[0]).expect("present").at().x);
    let east_gap = i64::from(east_first.entity(east_ids[0]).expect("present").at().x)
        - i64::from(east_first.entity(east_ids[1]).expect("present").at().x);
    assert_eq!(west_gap, east_gap, "corrections are gathered, then applied");
}

#[test]
fn a_push_cannot_shove_a_body_through_a_wall() {
    // Two bodies overlapping beside a pillar: the push must not put either
    // inside it.
    let terrain = SyntheticTerrain::lattice(4, 0);
    let mut zone = Zone::new(TickRate::default_rate());
    let a = zone
        .spawn(spec(WorldPoint { x: 5_632, y: 5_632 }, 0))
        .expect("room");
    let b = zone
        .spawn(spec(
            WorldPoint {
                x: 5_632 + 100,
                y: 5_632,
            },
            0,
        ))
        .expect("room");
    for _ in 0..8 {
        zone.step(&terrain).expect("stepped");
    }
    for id in [a, b] {
        let at = zone.entity(id).expect("present").at();
        let cell = crate::terrain::cell_at(at);
        assert!(
            !(cell.x.rem_euclid(4) == 0 && cell.y.rem_euclid(4) == 0),
            "{id:?} was pushed into a pillar at {cell:?}"
        );
    }
}

#[test]
fn a_zone_of_many_bodies_steps_without_refusing_anything() {
    let mut zone = Zone::new(TickRate::default_rate());
    let mut ids = alloc::vec::Vec::new();
    for index in 0..64_i32 {
        ids.push(
            zone.spawn(spec(
                WorldPoint {
                    x: 512 + index * 3_000,
                    y: 512 + (index % 8) * 3_000,
                },
                200 + u16::try_from(index).unwrap_or(0),
            ))
            .expect("room"),
        );
    }
    for tick in 0..30 {
        for (index, id) in ids.iter().enumerate() {
            let turn = i16::try_from(index % 4).unwrap_or(0);
            zone.submit(*id, &move_intent(tick + 1, tick, EAST - turn, turn))
                .expect("admitted");
        }
        zone.step(&ground()).expect("stepped");
        assert!(
            zone.refusals().is_empty(),
            "nothing was refused at tick {tick}"
        );
    }
    assert_eq!(zone.population(), 64);
}

#[test]
fn spending_a_resource_is_all_or_nothing() {
    let (mut zone, ids) = zone_with(&[(512, 512)]);
    let full = zone.entity(ids[0]).expect("present").resource().current();
    assert_eq!(zone.spend_resource(ids[0], full + 1), Ok(false));
    assert_eq!(
        zone.entity(ids[0]).expect("present").resource().current(),
        full
    );
    assert_eq!(zone.spend_resource(ids[0], full), Ok(true));
    assert!(zone.entity(ids[0]).expect("present").resource().is_empty());
    assert_eq!(
        zone.spend_resource(EntityId(404), 1),
        Err(ZoneError::Refused(Refusal::NoSuchEntity(EntityId(404))))
    );
}
