use tairix_wintersun_net::value::EntityId;

use super::{
    after_defence, after_status, attacker_scaled, floored, healed, resolve, split_absorb, Blow,
    School,
};
use crate::bounds::{MAX_ARMOUR, MAX_BLOW_BASE, MAX_POWER_SCALE_PERMILLE, MIN_LANDED_DAMAGE};
use crate::error::RuleError;
use crate::stat::{Stat, Stats, BASE_POWER_PERMILLE, POWER_PER_POINT_PERMILLE};
use crate::status::{Status, StatusKind, StatusSet};

fn blow(school: School, base: u32, scale: u16) -> Blow {
    Blow::new(school, base, scale).expect("a legal blow")
}

fn afflicted(kind: StatusKind, magnitude: u16) -> StatusSet {
    let mut set = StatusSet::new();
    set.apply(Status::new(kind, magnitude, 100, EntityId(1)).expect("a legal status"));
    set
}

#[test]
fn a_blow_outside_its_bounds_is_refused() {
    assert_eq!(
        Blow::new(School::Flame, MAX_BLOW_BASE + 1, 1000),
        Err(RuleError::BlowBase)
    );
    assert_eq!(
        Blow::new(School::Flame, 10, MAX_POWER_SCALE_PERMILLE + 1),
        Err(RuleError::PowerScale)
    );
    assert!(Blow::new(School::Flame, MAX_BLOW_BASE, MAX_POWER_SCALE_PERMILLE).is_ok());
}

#[test]
fn a_school_scales_with_the_stated_stat() {
    assert_eq!(School::Physical.scaling_stat(), Stat::Might);
    for school in School::ALL
        .iter()
        .copied()
        .filter(|s| *s != School::Physical)
    {
        assert_eq!(school.scaling_stat(), Stat::Insight, "{school:?}");
    }
    assert_eq!(School::ALL.len(), 6);
}

#[test]
fn step_one_is_the_hand_computed_scaling() {
    // Might 100 gives power 1000 + 100*5 = 1500 permille, so 500 above
    // unmodified. A base of 200 at full scale therefore takes
    // 200 + 200*500*1000/1_000_000 = 200 + 100 = 300.
    let attacker = Stats::new(100, 0, 0, 0, 0).expect("inside the domain");
    assert_eq!(
        attacker.power_permille(Stat::Might),
        BASE_POWER_PERMILLE + 100 * POWER_PER_POINT_PERMILLE
    );
    assert_eq!(
        attacker_scaled(&blow(School::Physical, 200, 1000), attacker),
        300
    );
    // Half scale takes half the stat's contribution, not half the blow.
    assert_eq!(
        attacker_scaled(&blow(School::Physical, 200, 500), attacker),
        250
    );
    // No scale is a blow with no stat in it at all.
    assert_eq!(
        attacker_scaled(&blow(School::Physical, 200, 0), attacker),
        200
    );
}

#[test]
fn a_blow_reads_only_its_own_school_stat() {
    let physical = Stats::new(1000, 0, 0, 0, 0).expect("inside the domain");
    let arcane = Stats::new(0, 0, 0, 1000, 0).expect("inside the domain");
    let swing = blow(School::Physical, 100, 1000);
    let bolt = blow(School::Frost, 100, 1000);
    assert!(attacker_scaled(&swing, physical) > attacker_scaled(&swing, arcane));
    assert!(attacker_scaled(&bolt, arcane) > attacker_scaled(&bolt, physical));
}

#[test]
fn step_two_subtracts_armour_then_scales_by_resistance() {
    // 1000 less 200 armour is 800; a 250-permille resistance passes 750 of
    // it, which is 600.
    assert_eq!(after_defence(1000, 200, 250), 600);
    // Armour alone can take a small blow to nothing — which is what the
    // floor exists for.
    assert_eq!(after_defence(50, 200, 0), 0);
    // Resistance alone cannot, because the curve never reaches a thousand.
    let top = Stats::new(0, 0, 0, 0, 1000).expect("inside the domain");
    assert!(after_defence(1000, 0, top.resistance_permille()) > 0);
}

#[test]
fn step_three_reads_the_defender_s_statuses() {
    assert_eq!(after_status(1000, &StatusSet::new()), 1000);
    assert_eq!(
        after_status(1000, &afflicted(StatusKind::Vulnerable, 500)),
        1500
    );
    assert_eq!(
        after_status(1000, &afflicted(StatusKind::Fortify, 400)),
        600
    );
}

#[test]
fn step_four_floors_a_landed_blow() {
    assert_eq!(floored(0), MIN_LANDED_DAMAGE);
    assert_eq!(floored(7), 7);
    assert_eq!(
        floored(u64::from(u32::MAX) + 1000),
        u32::MAX,
        "a blow past the type saturates rather than wrapping"
    );
}

#[test]
fn step_five_spends_shields_before_health() {
    assert_eq!(split_absorb(100, 0).to_health, 100);
    let partly = split_absorb(100, 40);
    assert_eq!((partly.absorbed, partly.to_health), (40, 60));
    let wholly = split_absorb(100, 250);
    assert_eq!((wholly.absorbed, wholly.to_health), (100, 0));
    assert_eq!(wholly.total(), 100);
}

#[test]
fn the_whole_pipeline_is_the_five_steps_in_order() {
    let attacker = Stats::new(100, 0, 0, 0, 0).expect("inside the domain");
    let defender = Stats::new(0, 0, 0, 0, 500).expect("inside the domain");
    let status = afflicted(StatusKind::Vulnerable, 500);

    let scaled = attacker_scaled(&blow(School::Physical, 200, 1000), attacker);
    let reduced = after_defence(scaled, 50, defender.resistance_permille());
    let modulated = after_status(reduced, &status);
    let expected = split_absorb(floored(modulated), status.absorb_available());

    assert_eq!(
        resolve(
            &blow(School::Physical, 200, 1000),
            attacker,
            defender,
            50,
            &status
        ),
        expected
    );
    // Hand-computed: 300 scaled, less 50 armour is 250, at 500-permille
    // resistance is 125, at 1500-permille vulnerability is 187.
    assert_eq!(expected.to_health, 187);
    assert_eq!(expected.absorbed, 0);
}

#[test]
fn an_over_defended_blow_still_takes_the_floor() {
    let attacker = Stats::default();
    let defender = Stats::new(0, 0, 0, 0, 1000).expect("inside the domain");
    let landed = resolve(
        &blow(School::Physical, 1, 1000),
        attacker,
        defender,
        MAX_ARMOUR,
        &StatusSet::new(),
    );
    assert_eq!(landed.to_health, MIN_LANDED_DAMAGE);
}

#[test]
fn a_shield_pays_the_whole_blow_when_it_can() {
    let landed = resolve(
        &blow(School::Physical, 10, 0),
        Stats::default(),
        Stats::default(),
        0,
        &afflicted(StatusKind::Shield, 500),
    );
    assert_eq!(landed.to_health, 0);
    assert_eq!(landed.absorbed, 10);
}

#[test]
fn the_widest_blow_cannot_overflow_the_pipeline() {
    let attacker = Stats::new(1000, 1000, 1000, 1000, 1000).expect("inside the domain");
    let landed = resolve(
        &blow(School::Flame, MAX_BLOW_BASE, MAX_POWER_SCALE_PERMILLE),
        attacker,
        Stats::default(),
        0,
        &afflicted(StatusKind::Vulnerable, 900),
    );
    assert!(landed.total() > 0 && landed.total() < u32::MAX);
}

#[test]
fn healing_scales_with_insight_and_is_cut_by_withering() {
    let healer = Stats::new(0, 0, 0, 200, 0).expect("inside the domain");
    // Power 1000 + 200*5 = 2000 permille, so a base of 100 heals 200.
    assert_eq!(healed(100, healer, &StatusSet::new()), 200);
    // A 600-permille wither leaves 400 of that.
    assert_eq!(healed(100, healer, &afflicted(StatusKind::Wither, 600)), 80);
    // There is no floor on healing: a heal that does nothing reads as a heal
    // that was not needed.
    assert_eq!(healed(0, healer, &StatusSet::new()), 0);
}
