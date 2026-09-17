//! Locomotion tests: the leg cycle, the jump arc, the tail, and the ears.

use super::{
    advance_phase, animate, burrow_crouch, ear_flop, jump_height, jump_progress, tail_sway,
    BURROW_SECONDS, JUMP_HEIGHT, JUMP_SECONDS, RUN_SPEED, WALK_SPEED,
};
use crate::cinder::Pose;
use tairix_util::mathf;

/// Whether two floats are equal to within the last bit or two of a `f64`.
///
/// The values compared here are exact by construction — a clamp that returned
/// its bound, a parabola at its root — so this is a statement that no
/// arithmetic crept in, not a tolerance for one.
fn exact(a: f64, b: f64) -> bool {
    mathf::fabs(a - b) <= f64::EPSILON * 8.0
}

#[test]
fn a_faster_walk_takes_quicker_steps() {
    let slow = advance_phase(0.0, WALK_SPEED, 0.1);
    let fast = advance_phase(0.0, RUN_SPEED, 0.1);
    assert!(fast > slow, "the legs must not skate at speed");
}

#[test]
fn standing_still_does_not_advance_the_legs() {
    assert!(exact(advance_phase(1.0, 0.0, 0.1), 1.0));
}

#[test]
fn the_phase_stays_bounded_however_long_the_walk() {
    let mut phase = 0.0;
    for _ in 0..10_000 {
        phase = advance_phase(phase, RUN_SPEED, 0.016);
        assert!(
            (0.0..core::f64::consts::TAU).contains(&phase),
            "a creature left walking for a week must not lose precision in its legs"
        );
    }
}

#[test]
fn a_jump_leaves_the_floor_peaks_in_the_middle_and_lands() {
    assert!(jump_height(0.0).is_some_and(|h| exact(h, 0.0)));
    let apex = jump_height(JUMP_SECONDS / 2.0).expect("mid-leap");
    assert!(
        mathf::fabs(apex - JUMP_HEIGHT) < 1e-9,
        "the arc must reach the stated height"
    );
    assert_eq!(jump_height(JUMP_SECONDS), None, "and land");
    assert_eq!(
        jump_height(-0.1),
        None,
        "a jump that has not begun is not in the air"
    );
}

#[test]
fn jump_progress_runs_from_nothing_to_all_and_stops() {
    assert!(exact(jump_progress(0.0), 0.0));
    assert!(exact(jump_progress(JUMP_SECONDS / 2.0), 0.5));
    assert!(exact(jump_progress(JUMP_SECONDS), 1.0));
    assert!(exact(jump_progress(JUMP_SECONDS * 10.0), 1.0));
}

#[test]
fn the_tail_swings_against_a_turn() {
    let left = tail_sway(WALK_SPEED, 2.0, 0.0);
    let right = tail_sway(WALK_SPEED, -2.0, 0.0);
    assert!(
        left < right,
        "the tail counterweights whichever way he turns"
    );
}

#[test]
fn the_tail_still_moves_when_he_is_standing_still() {
    let a = tail_sway(0.0, 0.0, 0.0);
    let b = tail_sway(0.0, 0.0, 0.9);
    assert!(!exact(a, b), "a stiff tail reads as a statue");
}

#[test]
fn the_ears_sweep_back_at_speed_and_prick_at_rest() {
    assert!(exact(ear_flop(0.0), 0.0));
    assert!(ear_flop(RUN_SPEED) > ear_flop(WALK_SPEED));
    assert!(
        ear_flop(RUN_SPEED * 10.0) <= 0.8,
        "the ears must not fold through the head however fast he runs"
    );
}

#[test]
fn animating_sets_every_derived_parameter_together() {
    let mut pose = Pose::default();
    animate(&mut pose, RUN_SPEED, 1.0, 0.1, 0.4);
    assert!(!exact(pose.gait_phase, 0.0));
    assert!(!exact(pose.tail_sway, 0.0));
    assert!(!exact(pose.ear_flop, 0.0));
}

#[test]
fn a_burrow_flattens_over_time_and_no_further() {
    assert!(exact(burrow_crouch(0.0), 0.0));
    assert!(burrow_crouch(BURROW_SECONDS / 2.0) > 0.0);
    assert!(exact(burrow_crouch(BURROW_SECONDS), 1.0));
    assert!(
        exact(burrow_crouch(BURROW_SECONDS * 5.0), 1.0),
        "flat is as flat as he gets"
    );
}
