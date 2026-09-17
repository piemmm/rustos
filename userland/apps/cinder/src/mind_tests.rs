//! Mind tests: the needs' drift, the intent choices, and their determinism
//! under a seeded generator.

use super::{Activity, Intent, Mind, Need, Needs, NOTICE_RANGE, POUNCE_RANGE, RECONSIDER_SECONDS};
use tairix_rng::{FastRng, RandU64};

/// A generator seeded so the same run produces the same creature.
fn seeded(seed: u64) -> FastRng {
    FastRng::seed_from_u64(seed)
}

/// Run `mind` for `seconds` in `dt` steps, answering the last intent.
fn run<R: RandU64>(mind: &mut Mind<R>, seconds: f64, dt: f64, pointer: Option<f64>) -> Intent {
    let mut intent = mind.intent();
    let mut elapsed = 0.0;
    while elapsed < seconds {
        intent = mind.tick(dt, pointer, false);
        elapsed += dt;
    }
    intent
}

#[test]
fn a_seeded_mind_makes_the_same_decisions_every_run() {
    let first = run(&mut Mind::new(seeded(7)), 60.0, 0.1, None);
    let second = run(&mut Mind::new(seeded(7)), 60.0, 0.1, None);
    assert_eq!(
        first, second,
        "behaviour must be assertable rather than merely watchable"
    );
}

#[test]
fn a_different_seed_is_a_different_creature() {
    let mut a = Mind::new(seeded(1));
    let mut b = Mind::new(seeded(2));
    let mut differed = false;
    for _ in 0..200 {
        if a.tick(0.2, None, false) != b.tick(0.2, None, false) {
            differed = true;
            break;
        }
    }
    assert!(differed, "two seeds must not walk in lockstep");
}

#[test]
fn resting_drains_play_and_exerting_restores_it() {
    let mut needs = Needs::default();
    let before = needs.play;
    needs.tick(10.0, Activity::Resting);
    assert!(needs.play < before, "doing nothing gets boring");
    needs.tick(10.0, Activity::Exerting);
    assert!(needs.play > 0.0);
}

#[test]
fn sleeping_restores_energy_and_exerting_spends_it() {
    let mut needs = Needs {
        energy: 0.5,
        ..Needs::default()
    };
    needs.tick(5.0, Activity::Exerting);
    assert!(needs.energy < 0.5);
    let spent = needs.energy;
    needs.tick(5.0, Activity::Sleeping);
    assert!(needs.energy > spent);
}

#[test]
fn a_need_never_leaves_its_range_however_long_it_drifts() {
    let mut needs = Needs::default();
    for _ in 0..10_000 {
        needs.tick(1.0, Activity::Exerting);
    }
    for level in [needs.energy, needs.play, needs.affection] {
        assert!((0.0..=1.0).contains(&level), "a need must stay a fraction");
    }
}

#[test]
fn the_most_pressing_need_is_the_lowest_one_below_the_threshold() {
    let contented = Needs::default();
    assert_eq!(contented.most_pressing(), None);
    let tired = Needs {
        energy: 0.1,
        play: 0.9,
        affection: 0.9,
    };
    assert_eq!(tired.most_pressing(), Some(Need::Rest));
    let lonely = Needs {
        energy: 0.9,
        play: 0.25,
        affection: 0.05,
    };
    assert_eq!(lonely.most_pressing(), Some(Need::Company));
}

#[test]
fn a_pointer_within_pouncing_range_is_pounced_on_at_once() {
    let mut mind = Mind::new(seeded(3));
    // Not after the reconsider timer: at once, or the creature looks as if it
    // had not noticed.
    assert_eq!(
        mind.tick(0.016, Some(POUNCE_RANGE - 1.0), false),
        Intent::Pounce
    );
}

#[test]
fn an_exhausted_creature_sleeps_rather_than_pouncing() {
    let mut mind = Mind::resuming(
        seeded(4),
        Needs {
            energy: 0.0,
            play: 1.0,
            affection: 1.0,
        },
    );
    let intent = run(&mut mind, RECONSIDER_SECONDS * 2.0, 0.1, Some(10.0));
    assert_eq!(intent, Intent::Nap, "a spent creature must read as tired");
}

#[test]
fn a_pointer_out_of_notice_range_is_ignored() {
    let mut mind = Mind::new(seeded(5));
    let intent = run(&mut mind, 30.0, 0.1, Some(NOTICE_RANGE * 2.0));
    assert_ne!(intent, Intent::Chase);
    assert_ne!(intent, Intent::Pounce);
}

#[test]
fn something_in_the_way_is_dealt_with_rather_than_walked_into() {
    let mut mind = Mind::new(seeded(6));
    let mut saw_crossing = false;
    for _ in 0..200 {
        let intent = mind.tick(0.2, None, true);
        if matches!(intent, Intent::Climb | Intent::Burrow) {
            saw_crossing = true;
            break;
        }
    }
    assert!(saw_crossing, "a blocked creature must choose over or under");
}

#[test]
fn both_ways_past_an_obstacle_are_reachable() {
    // The choice is a draw, so over many draws both must appear — a creature
    // that always went the same way would read as scripted.
    let mut climbed = false;
    let mut burrowed = false;
    for seed in 0..40u64 {
        let mut mind = Mind::new(seeded(seed));
        for _ in 0..20 {
            match mind.tick(0.3, None, true) {
                Intent::Climb => climbed = true,
                Intent::Burrow => burrowed = true,
                _ => {}
            }
        }
    }
    assert!(climbed && burrowed);
}

#[test]
fn petting_restores_affection_and_settles_him() {
    let mut mind = Mind::resuming(
        seeded(8),
        Needs {
            energy: 0.8,
            play: 0.8,
            affection: 0.1,
        },
    );
    let before = mind.needs().affection;
    mind.petted();
    assert!(mind.needs().affection > before);
    assert_eq!(
        mind.intent(),
        Intent::Sit,
        "a creature that ignored a hand would not read as a pet"
    );
}

#[test]
fn every_intent_states_a_pace_and_an_activity() {
    for intent in [
        Intent::Wander,
        Intent::Sit,
        Intent::Groom,
        Intent::Nap,
        Intent::Chase,
        Intent::Pounce,
        Intent::Climb,
        Intent::Burrow,
        Intent::ComeHome,
    ] {
        assert!((0.0..=1.0).contains(&intent.pace()));
        let _ = intent.activity();
    }
}

#[test]
fn a_wander_draw_is_a_real_heading_and_a_sane_distance() {
    let mut mind = Mind::new(seeded(9));
    for _ in 0..100 {
        let heading = mind.draw_heading();
        assert!((0.0..core::f64::consts::TAU).contains(&heading));
        let distance = mind.draw_wander_distance();
        assert!((60.0..=260.0).contains(&distance));
    }
}
