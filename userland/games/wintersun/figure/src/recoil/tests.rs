//! What a recoil throws, and what it comes back to.

use tairix_util::mathf;

use super::Recoil;
use crate::error::FigureError;
use crate::pose::{Param, Pose};
use crate::socket::Side;
use crate::spring::Spring;

const ARM: Param = Param::ShoulderSwing(Side::Right);
const ELBOW: Param = Param::ElbowBend(Side::Right);

fn recoil() -> Recoil {
    Recoil::new(Spring::new(26.0, 0.35).expect("a real spring"))
}

#[test]
fn an_unreal_impulse_or_step_is_refused() {
    let mut recoil = recoil();
    assert_eq!(recoil.strike(ARM, f64::NAN), Err(FigureError::MotionUnreal));
    recoil.strike(ARM, -3.0).expect("a real impulse");
    for seconds in [-0.1, f64::NAN] {
        assert_eq!(recoil.advance(seconds), Err(FigureError::ElapsedUnreal));
    }
}

#[test]
fn an_unstruck_recoil_does_nothing_at_all() {
    let mut recoil = recoil();
    for _ in 0..100 {
        recoil.advance(1.0 / 60.0).expect("it settles");
    }
    assert!(recoil.settled());
    assert!(recoil.overlay().expect("an overlay").is_empty());
}

/// An impulse is a speed, not a displacement: the limb has not moved yet on
/// the frame the blow lands, which is what makes the kick read as an impact
/// rather than as a teleport.
#[test]
fn a_strike_moves_nothing_until_time_passes() {
    let mut recoil = recoil();
    recoil.strike(ARM, -4.0).expect("a real impulse");
    assert!(recoil.overlay().expect("an overlay").is_empty());
    recoil.advance(1.0 / 60.0).expect("it settles");
    assert!(recoil.overlay().expect("an overlay").get(ARM) < 0.0);
}

/// The recoil's target is the animation itself, so it must vanish completely
/// rather than leaving the limb parked somewhere new.
#[test]
fn a_recoil_settles_back_onto_the_clip() {
    let mut recoil = recoil();
    recoil.strike(ARM, -6.0).expect("a real impulse");
    recoil.strike(ELBOW, 2.5).expect("a real impulse");
    for _ in 0..600 {
        recoil.advance(1.0 / 60.0).expect("it settles");
    }
    assert!(recoil.settled(), "a recoil must run out");
    let overlay = recoil.overlay().expect("an overlay");
    for param in Param::ALL {
        assert!(
            mathf::fabs(overlay.get(param)) < 1e-3,
            "{param:?} was left at {}",
            overlay.get(param)
        );
    }
}

/// The overshoot is the follow-through — the thing whose absence makes an
/// attack feel weightless.
#[test]
fn a_recoil_overshoots_on_its_way_back() {
    let mut recoil = recoil();
    recoil.strike(ARM, -6.0).expect("a real impulse");
    let mut went_back = false;
    let mut came_past = false;
    for _ in 0..400 {
        recoil.advance(1.0 / 120.0).expect("it settles");
        let value = recoil.overlay().expect("an overlay").get(ARM);
        went_back |= value < -0.01;
        came_past |= went_back && value > 0.01;
    }
    assert!(went_back, "the strike must throw the limb");
    assert!(came_past, "and it must overshoot coming back");
}

/// Only what was struck moves; a blow on one arm does not shrug the whole
/// body.
#[test]
fn a_strike_touches_only_what_it_struck() {
    let mut recoil = recoil();
    recoil.strike(ARM, -5.0).expect("a real impulse");
    recoil.advance(0.1).expect("it settles");
    let written = recoil.overlay().expect("an overlay").written();
    assert!(written.holds(ARM));
    assert_eq!(written.len(), 1, "nothing else may move");
}

#[test]
fn a_recoil_leaves_a_pose_inside_its_ranges() {
    let mut recoil = Recoil::new(Spring::new(30.0, 0.2).expect("a real spring"));
    recoil.strike(ELBOW, 40.0).expect("a real impulse");
    recoil.strike(ARM, -40.0).expect("a real impulse");
    let extreme = Pose::REST.with(ELBOW, 1.0).expect("a real pose");
    for _ in 0..400 {
        recoil.advance(1.0 / 60.0).expect("it settles");
        let struck = recoil
            .overlay()
            .expect("an overlay")
            .applied(&extreme)
            .expect("it applies");
        for param in Param::ALL {
            assert!(
                param.range().holds(struck.get(param)),
                "{param:?} left its range at {}",
                struck.get(param)
            );
        }
    }
}

#[test]
fn a_recoil_reads_back_the_spring_it_was_made_from() {
    let spring = Spring::new(18.0, 0.4).expect("a real spring");
    assert_eq!(Recoil::new(spring).spring(), spring);
}
