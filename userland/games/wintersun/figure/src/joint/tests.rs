//! What a joint limit admits, and what it refuses.

use core::f64::consts::TAU;

use super::{JointId, Limit, Limits};
use crate::error::FigureError;
use crate::frame::Rotation;

#[test]
fn a_symmetric_limit_spans_either_side_of_rest() {
    let limit = Limit::symmetric(0.5).expect("a half radian either way is real");
    assert!(limit.holds(0.0));
    assert!(limit.holds(0.5));
    assert!(limit.holds(-0.5));
    assert!(!limit.holds(0.51));
    assert!(!limit.holds(-0.51));
}

#[test]
fn a_limit_holds_its_own_bounds() {
    // Closed, not open: a clip driving a joint exactly to its stop is
    // authored on purpose and must not be refused.
    let limit = Limit::new(-0.25, 1.5).expect("real bounds");
    assert!(limit.holds(limit.min()));
    assert!(limit.holds(limit.max()));
}

#[test]
fn an_inverted_limit_is_refused() {
    assert_eq!(Limit::new(0.4, -0.4), Err(FigureError::LimitInverted));
    assert_eq!(Limit::symmetric(-0.4), Err(FigureError::LimitInverted));
}

#[test]
fn a_limit_that_excludes_rest_is_refused() {
    // Either way round: a joint whose own neutral pose is illegal could
    // never be placed at all.
    assert_eq!(
        Limit::new(0.2, 0.8),
        Err(FigureError::LimitExcludesRest),
        "a limit entirely above rest"
    );
    assert_eq!(
        Limit::new(-0.8, -0.2),
        Err(FigureError::LimitExcludesRest),
        "a limit entirely below rest"
    );
}

#[test]
fn an_unreal_or_over_wound_limit_is_refused() {
    assert_eq!(Limit::new(f64::NAN, 1.0), Err(FigureError::LimitUnreal));
    assert_eq!(
        Limit::new(-1.0, f64::INFINITY),
        Err(FigureError::LimitUnreal)
    );
    assert_eq!(Limit::new(-TAU - 0.1, 0.0), Err(FigureError::LimitUnreal));
    assert_eq!(Limit::new(0.0, TAU + 0.1), Err(FigureError::LimitUnreal));
}

#[test]
fn a_limit_refuses_an_unreal_angle() {
    // Otherwise a NaN reaches the depth sort as a position no pixel is at.
    let limit = Limit::symmetric(1.0).expect("real");
    assert!(!limit.holds(f64::NAN));
    assert!(!limit.holds(f64::INFINITY));
}

#[test]
fn a_fixed_axis_holds_only_rest() {
    assert!(Limit::FIXED.holds(0.0));
    assert!(!Limit::FIXED.holds(0.000_1));
    assert!(!Limit::FIXED.holds(-0.000_1));
}

#[test]
fn a_hinge_fixes_everything_but_its_swing() {
    let limits = Limits::hinge(0.9).expect("real");
    assert!(limits.holds(Rotation::new(0.9, 0.0, 0.0)));
    assert!(!limits.holds(Rotation::new(0.0, 0.1, 0.0)));
    assert!(!limits.holds(Rotation::new(0.0, 0.0, 0.1)));
}

#[test]
fn fixed_limits_hold_rest_and_nothing_else() {
    assert!(Limits::FIXED.holds(Rotation::REST));
    assert!(!Limits::FIXED.holds(Rotation::new(0.01, 0.0, 0.0)));
}

#[test]
fn a_joint_id_names_its_own_position() {
    for index in 0..u8::MAX {
        assert_eq!(JointId::new(index).index(), usize::from(index));
    }
}
