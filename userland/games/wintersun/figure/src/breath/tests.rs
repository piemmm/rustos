//! What breathing moves, and how far.

use core::f64::consts::TAU;

use tairix_util::mathf;

use super::Breath;
use crate::error::FigureError;
use crate::pose::{Param, Pose};
use crate::socket::Side;

const SLACK: f64 = 1e-9;

fn close(a: f64, b: f64) -> bool {
    mathf::fabs(a - b) <= SLACK
}

fn breath() -> Breath {
    Breath::new(4.0, 0.05).expect("a real breath")
}

#[test]
fn an_unreal_period_or_depth_is_refused() {
    for period in [0.0, -1.0, f64::NAN, f64::INFINITY] {
        assert_eq!(
            Breath::new(period, 0.05).map(|_| ()),
            Err(FigureError::DurationUnreal),
            "a period of {period} must be refused"
        );
    }
    for depth in [-0.01, 1.01, f64::NAN] {
        assert_eq!(
            Breath::new(4.0, depth).map(|_| ()),
            Err(FigureError::ParamOutsideRange),
            "a depth of {depth} must be refused"
        );
    }
}

#[test]
fn an_unreal_step_is_refused() {
    let mut breath = breath();
    for seconds in [-0.1, f64::NAN, f64::INFINITY] {
        assert_eq!(breath.advance(seconds), Err(FigureError::ElapsedUnreal));
    }
    assert!(close(breath.phase(), 0.0));
}

#[test]
fn the_cycle_wraps_and_returns_to_where_it_started() {
    let mut breath = breath();
    breath.advance(4.0).expect("it breathes");
    assert!(close(breath.phase(), 0.0));
    breath.advance(1.0).expect("it breathes");
    assert!(close(breath.phase(), 0.25));
    breath.advance(40.0).expect("it breathes");
    assert!(
        close(breath.phase(), 0.25),
        "ten whole cycles change nothing"
    );
}

/// The point of the layer: an idle figure is never completely still, so it
/// never reads as a paused game.
#[test]
fn an_idle_figure_is_never_completely_still() {
    let mut breath = breath();
    let mut moved = false;
    for _ in 0..240 {
        breath.advance(1.0 / 60.0).expect("it breathes");
        moved |= !breath.overlay().expect("an overlay").written().is_empty();
    }
    assert!(moved, "breathing must actually move something");
}

/// Small enough to read as life rather than as an animation: nothing it does
/// exceeds the depth it was asked for.
#[test]
fn nothing_it_moves_exceeds_its_depth() {
    let mut breath = Breath::new(3.0, 0.08).expect("a real breath");
    for _ in 0..600 {
        breath.advance(1.0 / 120.0).expect("it breathes");
        let overlay = breath.overlay().expect("an overlay");
        for param in Param::ALL {
            assert!(
                mathf::fabs(overlay.get(param)) <= breath.depth() + SLACK,
                "{param:?} moved {} past a depth of {}",
                overlay.get(param),
                breath.depth()
            );
        }
    }
}

/// A chest that fills lifts the spine and carries the arms out with it; a
/// spine moving on its own would read as a bow.
#[test]
fn the_chest_and_the_shoulders_move_together() {
    let mut breath = breath();
    breath.advance(1.0).expect("it breathes");
    let overlay = breath.overlay().expect("an overlay");
    let rise = breath.depth() * mathf::sin(TAU * 0.25);
    assert!(close(overlay.get(Param::SpineBend), -rise));
    for side in Side::BOTH {
        assert!(overlay.get(Param::ShoulderSplay(side)) > 0.0);
    }
}

#[test]
fn it_leaves_a_pose_inside_its_ranges() {
    let mut breath = Breath::new(2.0, 1.0).expect("a real breath");
    let extreme = Pose::REST
        .with(Param::SpineBend, 1.0)
        .expect("a real pose")
        .with(Param::ShoulderSplay(Side::Left), 1.0)
        .expect("a real pose");
    for _ in 0..200 {
        breath.advance(1.0 / 60.0).expect("it breathes");
        let breathed = breath
            .overlay()
            .expect("an overlay")
            .applied(&extreme)
            .expect("it applies");
        for param in Param::ALL {
            assert!(param.range().holds(breathed.get(param)));
        }
    }
}
