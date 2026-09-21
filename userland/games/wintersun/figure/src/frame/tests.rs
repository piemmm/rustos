//! The projection's stated properties, each as a number rather than a
//! screenshot.

use core::f64::consts::{FRAC_PI_2, PI};

use tairix_util::mathf;
use tairix_wintersun_net::value::Facing;

use super::{project, screen_turn, Basis, Body, Rotation, FORESHORTEN};

/// Facings in the sense `Facing` itself documents: zero east, advancing south.
const EAST: Facing = Facing(0);
const SOUTH: Facing = Facing(0x4000);
const WEST: Facing = Facing(0x8000);
const NORTH: Facing = Facing(0xC000);

/// Slack for a value that has been through the shared transcendentals, which
/// are accurate to about 1e-9.
const SLACK: f64 = 1e-9;

fn close(a: f64, b: f64) -> bool {
    mathf::fabs(a - b) <= SLACK
}

#[test]
fn foreshortening_is_the_elevation_the_doc_claims() {
    // The doc says "about 35°"; a reader must be able to trust that without
    // reaching for a calculator.
    let degrees = mathf::atan(FORESHORTEN) * 180.0 / PI;
    assert!(
        mathf::fabs(degrees - 35.0) < 0.5,
        "foreshortening is {degrees}°, not the documented ~35°"
    );
}

#[test]
fn facing_east_lays_forward_across_the_screen() {
    let there = project(EAST, Body::FORWARD);
    assert!(close(there.dx, 1.0));
    assert!(close(there.dy, 0.0));
    assert!(close(there.depth, 0.0));
}

#[test]
fn facing_south_lays_forward_into_the_scene_foreshortened() {
    let there = project(SOUTH, Body::FORWARD);
    assert!(close(there.dx, 0.0));
    // South is toward the camera, so a step forward draws lower and nearer.
    assert!(close(there.dy, FORESHORTEN));
    assert!(close(there.depth, 1.0));
}

#[test]
fn facing_north_walks_away_and_sorts_further() {
    let there = project(NORTH, Body::FORWARD);
    assert!(close(there.dy, -FORESHORTEN));
    assert!(
        there.depth < 0.0,
        "walking away must sort further, not nearer"
    );
}

#[test]
fn the_figures_left_is_north_when_it_faces_east() {
    let there = project(EAST, Body::SIDE);
    assert!(close(there.dx, 0.0));
    assert!(close(there.depth, -1.0), "its left is away from the camera");
}

#[test]
fn the_figures_left_is_east_when_it_faces_south() {
    let there = project(SOUTH, Body::SIDE);
    assert!(close(there.dx, 1.0));
    assert!(close(there.depth, 0.0));
}

#[test]
fn height_is_not_foreshortened() {
    for facing in [EAST, SOUTH, WEST, NORTH] {
        let there = project(facing, Body::UP);
        assert!(close(there.dx, 0.0));
        assert!(
            close(there.dy, -1.0),
            "up moves a whole pixel up the screen"
        );
        assert!(close(there.depth, 0.0));
    }
}

#[test]
fn depth_is_not_the_screen_row() {
    // A raised hand draws higher without becoming further away, which is why
    // the sort key cannot be the row: it would put the hand behind the body.
    let flat = project(SOUTH, Body::new(4.0, 0.0, 0.0));
    let raised = project(SOUTH, Body::new(4.0, 0.0, 30.0));
    assert!(close(flat.depth, raised.depth));
    assert!(raised.dy < flat.dy, "the raised part draws higher");
}

#[test]
fn a_whole_turn_of_headings_needs_no_new_geometry() {
    // The saving the body frame exists for: every heading is the same
    // arrangement seen differently, so none of them is degenerate.
    let offset = Body::new(6.0, 2.0, 40.0);
    for step in 0..16u32 {
        let facing = Facing(u16::try_from(step * 4096).expect("inside a turn"));
        let there = project(facing, offset);
        assert!(there.dx.is_finite() && there.dy.is_finite() && there.depth.is_finite());
    }
}

#[test]
fn a_pitch_turns_the_outline_fully_in_profile() {
    // Seen from the side, a fore-and-aft swing is a screen rotation of
    // exactly the same angle. Positive pitch leans the top forward, which
    // facing east is toward screen-right, which a placed outline calls
    // positive.
    let swing = 0.4;
    let basis = Basis::of(Rotation::new(swing, 0.0, 0.0));
    assert!(close(screen_turn(EAST, basis), swing));
    // Seen from the other side the same swing leans the other way.
    assert!(close(screen_turn(WEST, basis), -swing));
}

#[test]
fn a_pitch_stops_turning_the_outline_in_depth() {
    // Facing the camera the swing happens in depth, so the billboard
    // correctly stops rotating rather than needing a branch on the heading.
    let basis = Basis::of(Rotation::new(0.4, 0.0, 0.0));
    assert!(close(screen_turn(SOUTH, basis), 0.0));
    assert!(close(screen_turn(NORTH, basis), 0.0));
}

#[test]
fn a_pitch_is_exact_at_a_large_angle() {
    // Not a small-angle estimate: a raised arm turns as far as it went, so
    // the rescale off the rotation's own angle is doing its job.
    let swing = FRAC_PI_2 * 0.9;
    let basis = Basis::of(Rotation::new(swing, 0.0, 0.0));
    assert!(close(screen_turn(EAST, basis), swing));
}

#[test]
fn yaw_never_turns_an_outline() {
    // A ground-plane rotation projects to a shear, not a rotation, and every
    // outline is symmetric about its vertical axis so the shear leaves it be.
    for facing in [EAST, SOUTH, WEST, NORTH] {
        for yaw in [-0.6, -0.2, 0.2, 0.6] {
            let basis = Basis::of(Rotation::new(0.0, yaw, 0.0));
            assert!(
                close(screen_turn(facing, basis), 0.0),
                "yaw {yaw} turned an outline"
            );
        }
    }
}

#[test]
fn a_roll_turns_the_outline_in_depth_and_not_in_profile() {
    // Facing the camera, the figure's right is screen-left, so a positive
    // roll — top toward its right — leans the outline anticlockwise.
    let tilt = 0.3;
    let basis = Basis::of(Rotation::new(0.0, 0.0, tilt));
    assert!(close(screen_turn(SOUTH, basis), -tilt));
    assert!(close(screen_turn(NORTH, basis), tilt));
    assert!(close(screen_turn(EAST, basis), 0.0));
}

#[test]
fn rest_turns_nothing() {
    for facing in [EAST, SOUTH, WEST, NORTH] {
        assert!(close(screen_turn(facing, Basis::IDENTITY), 0.0));
    }
}

#[test]
fn a_rest_rotation_is_the_identity_basis() {
    assert_eq!(Basis::of(Rotation::REST), Basis::IDENTITY);
}

#[test]
fn a_composed_rotation_stays_orthonormal() {
    // The invariant every joint's frame rests on: were it to drift, a deep
    // chain would shear its parts rather than turn them.
    let basis = Basis::of(Rotation::new(0.7, -0.4, 0.9));
    for axis in [basis.forward, basis.side, basis.up] {
        assert!(close(axis.length(), 1.0), "axis is not a unit direction");
    }
    assert!(close(basis.forward.dot(basis.side), 0.0));
    assert!(close(basis.side.dot(basis.up), 0.0));
    assert!(close(basis.up.dot(basis.forward), 0.0));
}

#[test]
fn composing_two_frames_is_applying_them_in_turn() {
    let outer = Basis::of(Rotation::new(0.3, 0.5, -0.2));
    let inner = Basis::of(Rotation::new(-0.4, 0.1, 0.6));
    let point = Body::new(3.0, -2.0, 7.0);
    let composed = outer.compose(inner).apply(point);
    let in_turn = outer.apply(inner.apply(point));
    assert!(close(composed.forward, in_turn.forward));
    assert!(close(composed.side, in_turn.side));
    assert!(close(composed.up, in_turn.up));
}

#[test]
fn rotating_about_an_axis_leaves_that_axis_alone() {
    let point = Body::new(0.0, 0.0, 5.0);
    let turned = point.turned_about(Body::UP, 1.1);
    assert!(close(turned.up, 5.0));
    assert!(close(turned.forward, 0.0));
    assert!(close(turned.side, 0.0));
}

#[test]
fn rotation_is_right_handed_about_each_axis() {
    // Stated once, here, because a sign flipped in the transform would put
    // every limb on the wrong side of its joint and still look plausible.
    let forward = Body::FORWARD.turned_about(Body::UP, FRAC_PI_2);
    assert!(
        close(forward.side, 1.0),
        "yaw turns forward toward the left"
    );
    let up = Body::UP.turned_about(Body::SIDE, FRAC_PI_2);
    assert!(close(up.forward, 1.0), "pitch leans the top forward");
    let tipped = Body::UP.turned_about(Body::FORWARD, FRAC_PI_2);
    assert!(close(tipped.side, -1.0), "roll leans the top to the right");
}

#[test]
fn a_hanging_part_swings_opposite_to_the_joints_own_top() {
    // The rule every joint limit's sign is read off: a limb hangs below its
    // joint, so a pitch that leans the top forward swings the limb back.
    let hanging = Body::new(0.0, 0.0, -1.0);
    let swung = Basis::of(Rotation::new(0.5, 0.0, 0.0)).apply(hanging);
    assert!(swung.forward < 0.0, "positive pitch must swing a limb back");
    let splayed = Basis::of(Rotation::new(0.0, 0.0, 0.5)).apply(hanging);
    assert!(splayed.side > 0.0, "positive roll must swing a limb left");
}

#[test]
fn an_offset_reports_an_unreal_component() {
    assert!(Body::new(1.0, 2.0, 3.0).is_real());
    assert!(!Body::new(f64::NAN, 0.0, 0.0).is_real());
    assert!(!Body::new(0.0, f64::INFINITY, 0.0).is_real());
    assert!(!Body::new(0.0, 0.0, f64::NEG_INFINITY).is_real());
}

#[test]
fn a_rotation_reports_an_unreal_angle() {
    assert!(Rotation::REST.is_real());
    assert!(!Rotation::new(f64::NAN, 0.0, 0.0).is_real());
    assert!(!Rotation::new(0.0, f64::INFINITY, 0.0).is_real());
}
