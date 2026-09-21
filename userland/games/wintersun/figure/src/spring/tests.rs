//! What a spring settles to, and what it refuses.

use tairix_util::mathf;

use super::{Motion, Spring};
use crate::error::FigureError;

const SLACK: f64 = 1e-9;

fn close(a: f64, b: f64) -> bool {
    mathf::fabs(a - b) <= SLACK
}

fn spring(rate: f64, damping: f64) -> Spring {
    Spring::new(rate, damping).expect("a real spring")
}

fn displaced(value: f64) -> Motion {
    Motion::new(value, 0.0).expect("a real motion")
}

#[test]
fn an_unreal_spring_is_refused() {
    for (rate, damping) in [
        (0.0, 1.0),
        (-1.0, 1.0),
        (f64::NAN, 1.0),
        (f64::INFINITY, 1.0),
        (1.0, -0.1),
        (1.0, f64::NAN),
    ] {
        assert_eq!(
            Spring::new(rate, damping).map(|_| ()),
            Err(FigureError::SpringUnreal),
            "rate {rate} damping {damping} must be refused"
        );
    }
}

#[test]
fn an_unreal_motion_impulse_or_target_is_refused() {
    assert_eq!(
        Motion::new(f64::NAN, 0.0).map(|_| ()),
        Err(FigureError::MotionUnreal)
    );
    assert_eq!(
        Motion::new(0.0, f64::INFINITY).map(|_| ()),
        Err(FigureError::MotionUnreal)
    );
    assert_eq!(
        displaced(1.0).struck(f64::NAN).map(|_| ()),
        Err(FigureError::MotionUnreal)
    );
    assert_eq!(
        spring(10.0, 1.0)
            .step(Motion::REST, f64::NAN, 0.1)
            .map(|_| ()),
        Err(FigureError::MotionUnreal)
    );
}

#[test]
fn an_unreal_step_is_refused() {
    for seconds in [-0.1, f64::NAN, f64::INFINITY] {
        assert_eq!(
            spring(10.0, 1.0)
                .step(displaced(1.0), 0.0, seconds)
                .map(|_| ()),
            Err(FigureError::ElapsedUnreal),
            "a step of {seconds} must be refused"
        );
    }
}

#[test]
fn a_zero_step_changes_nothing() {
    let state = Motion::new(0.4, -2.0).expect("a real motion");
    let after = spring(9.0, 0.5).step(state, 0.0, 0.0).expect("it steps");
    assert_eq!(after, state);
}

/// The whole reason a recoil is an impulse rather than a displacement: the
/// hand has not moved yet on the frame the blow lands.
#[test]
fn an_impulse_adds_speed_without_moving_anything() {
    let struck = displaced(0.25).struck(-3.0).expect("a real impulse");
    assert!(close(struck.value, 0.25));
    assert!(close(struck.velocity, -3.0));
}

/// The property that makes every layer above this one safe to reason about:
/// whatever the step, the value stays inside the envelope its own start set.
#[test]
fn no_step_of_any_length_can_make_a_spring_diverge() {
    for damping in [0.0, 0.2, 0.7, 1.0, 1.4, 4.0] {
        let spring = spring(12.0, damping);
        for step in [1e-4, 0.001, 1.0 / 60.0, 0.25, 1.0, 7.5, 600.0] {
            let mut state = Motion::new(1.0, 30.0).expect("a real motion");
            let start = mathf::fabs(state.value) + mathf::fabs(state.velocity) / spring.rate();
            for _ in 0..200 {
                state = spring.step(state, 0.0, step).expect("it steps");
                assert!(
                    state.value.is_finite() && state.velocity.is_finite(),
                    "damping {damping} step {step} must stay a number"
                );
                assert!(
                    mathf::fabs(state.value) <= start + SLACK,
                    "damping {damping} step {step} must not gain amplitude"
                );
            }
        }
    }
}

/// Split a step in two and the answer must be the same, because the closed
/// form is the true solution rather than an approximation of one.
#[test]
fn stepping_twice_matches_stepping_once_over_the_same_span() {
    for damping in [0.3, 1.0, 2.5] {
        let spring = spring(8.0, damping);
        let start = Motion::new(0.6, -1.5).expect("a real motion");
        let once = spring.step(start, 0.1, 0.4).expect("it steps");
        let split = spring
            .step(spring.step(start, 0.1, 0.15).expect("it steps"), 0.1, 0.25)
            .expect("it steps");
        assert!(
            close(once.value, split.value) && close(once.velocity, split.velocity),
            "damping {damping} must be path-independent"
        );
    }
}

#[test]
fn every_damping_regime_settles_on_its_target() {
    for damping in [0.1, 0.999_99, 1.0, 1.000_01, 3.0] {
        let spring = spring(20.0, damping);
        let mut state = Motion::new(1.0, 5.0).expect("a real motion");
        for _ in 0..600 {
            state = spring.step(state, -0.25, 1.0 / 60.0).expect("it steps");
        }
        assert!(
            state.settled(-0.25, 1e-6),
            "damping {damping} settled to {} moving {}",
            state.value,
            state.velocity
        );
    }
}

/// The overshoot is the follow-through, so it must actually be there below
/// critical and actually be absent at and above it.
#[test]
fn only_an_underdamped_spring_overshoots() {
    for (damping, wanted) in [(0.2, true), (1.0, false), (2.0, false)] {
        let spring = spring(15.0, damping);
        let mut state = displaced(1.0);
        let mut crossed = false;
        for _ in 0..400 {
            state = spring.step(state, 0.0, 1.0 / 120.0).expect("it steps");
            crossed |= state.value < -SLACK;
        }
        assert_eq!(crossed, wanted, "damping {damping} overshoot");
    }
}

/// A spring already sitting on its target with no speed has nothing to do,
/// which is what lets a settled layer cost nothing per frame.
#[test]
fn a_settled_spring_stays_put() {
    let after = spring(10.0, 1.0)
        .step(Motion::REST, 0.0, 1.0 / 60.0)
        .expect("it steps");
    assert_eq!(after, Motion::REST);
}
