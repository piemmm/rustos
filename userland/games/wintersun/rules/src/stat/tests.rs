use super::{Stat, Stats, BASE_HEALTH, BASE_POWER_PERMILLE, BASE_RESOURCE, BASE_SPEED_SUB_UNITS};
use crate::bounds::{MAX_SPEED_SUB_UNITS_PER_TICK, MAX_STAT};
use crate::error::RuleError;

fn uniform(value: u16) -> Stats {
    Stats::new(value, value, value, value, value).expect("inside the domain")
}

#[test]
fn a_stat_above_the_ceiling_is_refused() {
    for index in 0..5 {
        let mut values = [0_u16; 5];
        values[index] = MAX_STAT + 1;
        assert_eq!(
            Stats::new(values[0], values[1], values[2], values[3], values[4]),
            Err(RuleError::Stat),
            "field {index} must be checked"
        );
    }
    assert!(
        uniform(MAX_STAT).max_health() > 0,
        "the ceiling itself is legal"
    );
}

#[test]
fn every_stat_is_readable_and_independent() {
    let stats = Stats::new(1, 2, 3, 4, 5).expect("inside the domain");
    assert_eq!(stats.get(Stat::Might), 1);
    assert_eq!(stats.get(Stat::Agility), 2);
    assert_eq!(stats.get(Stat::Vitality), 3);
    assert_eq!(stats.get(Stat::Insight), 4);
    assert_eq!(stats.get(Stat::Resolve), 5);
    assert_eq!(Stat::ALL.len(), 5, "the set is closed at five");
}

#[test]
fn the_curves_start_at_their_stated_base() {
    let zero = uniform(0);
    assert_eq!(zero.max_health(), BASE_HEALTH);
    assert_eq!(zero.max_resource(), BASE_RESOURCE);
    assert_eq!(zero.speed_sub_units_per_tick(), BASE_SPEED_SUB_UNITS);
    assert_eq!(zero.power_permille(Stat::Might), BASE_POWER_PERMILLE);
    assert_eq!(zero.resistance_permille(), 0);
}

#[test]
fn the_curves_are_monotone_across_the_whole_domain() {
    let mut previous = (0_u32, 0_u32, i32::MIN, 0_u32, 0_u32);
    for value in 0..=MAX_STAT {
        let stats = uniform(value);
        let now = (
            stats.max_health(),
            stats.max_resource(),
            stats.speed_sub_units_per_tick(),
            stats.power_permille(Stat::Might),
            stats.resistance_permille(),
        );
        assert!(now.0 >= previous.0, "health at {value}");
        assert!(now.1 >= previous.1, "resource at {value}");
        assert!(now.2 >= previous.2, "speed at {value}");
        assert!(now.3 >= previous.3, "power at {value}");
        assert!(now.4 >= previous.4, "resistance at {value}");
        previous = now;
    }
}

#[test]
fn resistance_never_reaches_total_at_any_legal_stat() {
    for value in 0..=MAX_STAT {
        let permille = uniform(value).resistance_permille();
        assert!(
            permille < 1000,
            "resistance {permille} at stat {value} would negate a blow entirely"
        );
    }
}

#[test]
fn resistance_is_half_at_the_stated_half_stat() {
    let half = Stats::new(0, 0, 0, 0, 500).expect("inside the domain");
    assert_eq!(half.resistance_permille(), 500);
}

#[test]
fn no_curve_saturates_inside_the_domain() {
    // Saturation would silently flatten a curve, which the monotonicity
    // test above cannot distinguish from a legitimate plateau.
    let top = uniform(MAX_STAT);
    assert!(top.max_health() < u32::MAX);
    assert!(top.max_resource() < u32::MAX);
    assert!(top.power_permille(Stat::Insight) < u32::MAX);
    assert!(top.speed_sub_units_per_tick() < MAX_SPEED_SUB_UNITS_PER_TICK);
    assert!(
        top.speed_sub_units_per_tick() > BASE_SPEED_SUB_UNITS,
        "agility must actually move the speed curve"
    );
}

#[test]
fn power_reads_the_stat_it_was_asked_for() {
    let stats = Stats::new(MAX_STAT, 0, 0, 0, 0).expect("inside the domain");
    assert!(stats.power_permille(Stat::Might) > stats.power_permille(Stat::Insight));
    assert_eq!(stats.power_permille(Stat::Insight), BASE_POWER_PERMILLE);
}
