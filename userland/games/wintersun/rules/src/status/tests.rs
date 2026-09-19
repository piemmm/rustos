use tairix_wintersun_net::value::EntityId;

use super::{Application, Magnitude, Status, StatusKind, StatusSet};
use crate::bounds::{
    DIMINISH_IMMUNE_AT, MAX_HARMFUL_STATUS, MAX_HELPFUL_STATUS, MAX_PERMILLE_MAGNITUDE,
    MAX_STATUS_TICKS,
};
use crate::error::RuleError;

const WINDOW: u32 = 450;
const SOURCE: EntityId = EntityId(7);

fn status(kind: StatusKind, magnitude: u16, ticks: u32) -> Status {
    Status::new(kind, magnitude, ticks, SOURCE).expect("a legal status")
}

fn from(kind: StatusKind, magnitude: u16, ticks: u32, source: u64) -> Status {
    Status::new(kind, magnitude, ticks, EntityId(source)).expect("a legal status")
}

#[test]
fn the_vocabulary_is_closed_and_indexed_without_collision() {
    assert_eq!(StatusKind::ALL.len(), StatusKind::COUNT);
    for (position, kind) in StatusKind::ALL.iter().enumerate() {
        assert_eq!(kind.index(), position, "{kind:?} indexes its own slot");
    }
}

#[test]
fn a_binary_kind_refuses_a_magnitude() {
    for kind in StatusKind::ALL.iter().copied() {
        let ceiling = kind.magnitude_ceiling();
        assert!(Status::new(kind, ceiling, 1, SOURCE).is_ok());
        if let Some(over) = ceiling.checked_add(1) {
            assert_eq!(
                Status::new(kind, over, 1, SOURCE),
                Err(RuleError::StatusMagnitude),
                "{kind:?} must refuse a magnitude past its ceiling"
            );
        }
        if kind.magnitude() == Magnitude::None {
            assert_eq!(ceiling, 0, "{kind:?} is binary and admits no magnitude");
        }
    }
}

#[test]
fn a_zero_or_over_long_duration_is_refused() {
    assert_eq!(
        Status::new(StatusKind::Stun, 0, 0, SOURCE),
        Err(RuleError::StatusDuration)
    );
    assert_eq!(
        Status::new(StatusKind::Stun, 0, MAX_STATUS_TICKS + 1, SOURCE),
        Err(RuleError::StatusDuration)
    );
    assert!(Status::new(StatusKind::Stun, 0, MAX_STATUS_TICKS, SOURCE).is_ok());
}

#[test]
fn proportional_effects_take_the_strongest_rather_than_the_sum() {
    let mut set = StatusSet::new();
    set.apply(from(StatusKind::Slow, 400, 100, 1));
    set.apply(from(StatusKind::Slow, 700, 100, 2));
    assert_eq!(
        set.speed_permille(),
        300,
        "two slows that summed would stop the body dead"
    );

    let mut set = StatusSet::new();
    set.apply(from(StatusKind::Fortify, MAX_PERMILLE_MAGNITUDE, 100, 1));
    set.apply(from(StatusKind::Fortify, MAX_PERMILLE_MAGNITUDE, 100, 2));
    assert!(
        set.damage_taken_permille() > 0,
        "two mitigations that summed would negate a blow entirely"
    );
}

#[test]
fn resource_shaped_effects_do_sum() {
    let mut set = StatusSet::new();
    set.apply(from(StatusKind::Shield, 100, 100, 1));
    set.apply(from(StatusKind::Shield, 150, 100, 2));
    assert_eq!(set.absorb_available(), 250);

    let mut set = StatusSet::new();
    set.apply(from(StatusKind::Bleed, 5, 100, 1));
    set.apply(from(StatusKind::Bleed, 3, 100, 2));
    set.apply(from(StatusKind::Regenerate, 2, 100, 3));
    assert_eq!(set.periodic_health(), -6);
}

#[test]
fn a_control_effect_halves_each_application_then_refuses() {
    let mut set = StatusSet::new();
    assert_eq!(
        set.apply(status(StatusKind::Stun, 0, 64)),
        Application::Applied
    );
    set.clear_kind(StatusKind::Stun);
    assert_eq!(
        set.apply(status(StatusKind::Stun, 0, 64)),
        Application::Diminished(32)
    );
    set.clear_kind(StatusKind::Stun);
    assert_eq!(
        set.apply(status(StatusKind::Stun, 0, 64)),
        Application::Diminished(16)
    );
    set.clear_kind(StatusKind::Stun);
    assert_eq!(
        set.apply(status(StatusKind::Stun, 0, 64)),
        Application::Immune,
        "the chain is {DIMINISH_IMMUNE_AT} long, then immune"
    );
}

#[test]
fn a_diminished_duration_never_rounds_away_to_nothing() {
    let mut set = StatusSet::new();
    set.apply(status(StatusKind::Root, 0, 1));
    set.clear_kind(StatusKind::Root);
    set.apply(status(StatusKind::Root, 0, 1));
    assert!(
        set.has(StatusKind::Root),
        "a one-tick control effect halved is still a control effect"
    );
}

#[test]
fn applying_into_immunity_holds_the_window_open() {
    let mut set = StatusSet::new();
    for _ in 0..DIMINISH_IMMUNE_AT {
        set.apply(status(StatusKind::Stun, 0, 8));
        set.clear_kind(StatusKind::Stun);
    }
    // Age most of the way to the reset, then apply into the immunity: the
    // contact must restart the clock, or an attacker could never be timed
    // out of it.
    for _ in 0..(WINDOW - 1) {
        set.advance(WINDOW);
    }
    assert_eq!(
        set.apply(status(StatusKind::Stun, 0, 8)),
        Application::Immune
    );
    set.advance(WINDOW);
    assert_eq!(
        set.apply(status(StatusKind::Stun, 0, 8)),
        Application::Immune,
        "the refused application reset the window rather than letting it lapse"
    );
}

#[test]
fn the_window_resets_after_quiet() {
    let mut set = StatusSet::new();
    set.apply(status(StatusKind::Stun, 0, 8));
    set.clear_kind(StatusKind::Stun);
    for _ in 0..WINDOW {
        set.advance(WINDOW);
    }
    assert_eq!(
        set.apply(status(StatusKind::Stun, 0, 64)),
        Application::Applied,
        "a quiet window starts the chain again"
    );
}

#[test]
fn damage_shaped_effects_do_not_diminish() {
    let mut set = StatusSet::new();
    for _ in 0..6 {
        assert_eq!(
            set.apply(status(StatusKind::Bleed, 5, 40)),
            Application::Applied,
            "a bleed is damage, not a loss of control"
        );
    }
    assert!(!StatusKind::Bleed.diminishes());
}

#[test]
fn a_refresh_takes_the_better_of_each_rather_than_the_newer() {
    let mut set = StatusSet::new();
    set.apply(from(StatusKind::Haste, 300, 100, 1));
    set.apply(from(StatusKind::Haste, 100, 10, 1));
    let held = set.held().first().expect("one status");
    assert_eq!(
        held.magnitude(),
        300,
        "a weaker re-application cannot weaken it"
    );
    assert_eq!(held.remaining(), 100, "nor shorten it");
}

#[test]
fn buffs_cannot_crowd_out_debuffs() {
    let mut set = StatusSet::new();
    for source in 0..(MAX_HELPFUL_STATUS as u64 + 4) {
        set.apply(from(StatusKind::Regenerate, 1, 10_000, source));
    }
    assert_eq!(
        set.apply(from(StatusKind::Stun, 0, 50, 99)),
        Application::Applied,
        "a full helpful partition must not confer debuff immunity"
    );
}

#[test]
fn a_full_partition_yields_only_to_something_that_outlasts_it() {
    let mut set = StatusSet::new();
    for source in 0..(MAX_HARMFUL_STATUS as u64) {
        set.apply(from(StatusKind::Bleed, 1, 500, source));
    }
    assert_eq!(
        set.apply(from(StatusKind::Bleed, 1, 100, 900)),
        Application::Crowded,
        "a shorter arrival must not displace a longer holder"
    );
    assert_eq!(
        set.apply(from(StatusKind::Bleed, 1, 900, 901)),
        Application::Applied
    );
    assert_eq!(set.held().len(), MAX_HARMFUL_STATUS);
}

#[test]
fn a_refused_crowding_does_not_burn_the_diminishing_chain() {
    // Fill the harmful partition with effects that outlast anything the
    // attacker sends, so every control application is refused for crowding.
    let mut set = StatusSet::new();
    for source in 0..(MAX_HARMFUL_STATUS as u64) {
        set.apply(from(StatusKind::Bleed, 1, 10_000, source));
    }
    for _ in 0..6 {
        assert_eq!(
            set.apply(from(StatusKind::Stun, 0, 50, 99)),
            Application::Crowded
        );
    }

    // With room again, the chain must be untouched: an effect that never
    // landed cannot have walked the target toward immunity.
    set.clear_kind(StatusKind::Bleed);
    assert_eq!(
        set.apply(status(StatusKind::Stun, 0, 64)),
        Application::Applied,
        "applications that were refused must not count against the chain"
    );
}

#[test]
fn a_stun_stops_everything_and_a_silence_only_casting() {
    let mut set = StatusSet::new();
    set.apply(status(StatusKind::Stun, 0, 10));
    assert!(!set.may_act() && !set.may_cast() && !set.may_move());
    assert_eq!(set.speed_permille(), 0);

    let mut set = StatusSet::new();
    set.apply(status(StatusKind::Silence, 0, 10));
    assert!(set.may_act() && set.may_move() && !set.may_cast());

    let mut set = StatusSet::new();
    set.apply(status(StatusKind::Root, 0, 10));
    assert!(set.may_act() && set.may_cast() && !set.may_move());
}

#[test]
fn shields_are_spent_shortest_lived_first() {
    let mut set = StatusSet::new();
    set.apply(from(StatusKind::Shield, 100, 500, 1));
    set.apply(from(StatusKind::Shield, 100, 20, 2));
    assert_eq!(set.consume_absorb(100), 100);
    let left = set.held().iter().find(|s| s.kind() == StatusKind::Shield);
    let left = left.expect("the long shield survives");
    assert_eq!(left.remaining(), 500, "the expiring one paid first");
    assert_eq!(left.magnitude(), 100);
}

#[test]
fn an_exhausted_shield_leaves_the_set() {
    let mut set = StatusSet::new();
    set.apply(status(StatusKind::Shield, 40, 100));
    assert_eq!(set.consume_absorb(250), 40, "it absorbs only what it held");
    assert!(!set.has(StatusKind::Shield));
    assert_eq!(set.absorb_available(), 0);
}

#[test]
fn durations_age_and_expire() {
    let mut set = StatusSet::new();
    set.apply(status(StatusKind::Bleed, 3, 2));
    assert_eq!(set.advance(WINDOW), 0);
    assert_eq!(set.periodic_health(), -3, "it still ticks on its last tick");
    assert_eq!(set.advance(WINDOW), 1);
    assert!(set.held().is_empty());
    assert_eq!(set.periodic_health(), 0);
}

#[test]
fn healing_is_reduced_only_by_a_withering_effect() {
    let mut set = StatusSet::new();
    assert_eq!(set.healing_permille(), 1000);
    set.apply(status(StatusKind::Wither, 600, 100));
    assert_eq!(set.healing_permille(), 400);
    set.apply(status(StatusKind::Vulnerable, 500, 100));
    assert_eq!(
        set.healing_permille(),
        400,
        "vulnerability is not withering"
    );
}
