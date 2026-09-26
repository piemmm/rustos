//! The projection's stated properties, each as a number rather than a
//! screenshot.

use core::f64::consts::{FRAC_PI_2, PI};

use tairix_util::mathf;
use tairix_wintersun_net::value::Facing;

use super::{project, toward_camera, Basis, Body, Heading, Rotation, FORESHORTEN};

/// Facings in the sense `Facing` itself documents: zero east, advancing south.
const EAST: Facing = Facing(0);
const SOUTH: Facing = Facing(0x4000);
const WEST: Facing = Facing(0x8000);
const NORTH: Facing = Facing(0xC000);

/// Slack for a value that has been through the shared transcendentals: far
/// coarser than their error, far finer than a pixel.
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
    let there = project(Heading::of(EAST), Body::FORWARD);
    assert!(close(there.dx, 1.0));
    assert!(close(there.dy, 0.0));
    assert!(close(there.depth, 0.0));
}

#[test]
fn facing_south_lays_forward_into_the_scene_foreshortened() {
    let there = project(Heading::of(SOUTH), Body::FORWARD);
    assert!(close(there.dx, 0.0));
    // South is toward the camera, so a step forward draws lower and nearer.
    assert!(close(there.dy, FORESHORTEN));
    assert!(close(there.depth, 1.0));
}

#[test]
fn facing_north_walks_away_and_sorts_further() {
    let there = project(Heading::of(NORTH), Body::FORWARD);
    assert!(close(there.dy, -FORESHORTEN));
    assert!(
        there.depth < 0.0,
        "walking away must sort further, not nearer"
    );
}

#[test]
fn the_figures_left_is_north_when_it_faces_east() {
    let there = project(Heading::of(EAST), Body::SIDE);
    assert!(close(there.dx, 0.0));
    assert!(close(there.depth, -1.0), "its left is away from the camera");
}

#[test]
fn the_figures_left_is_east_when_it_faces_south() {
    let there = project(Heading::of(SOUTH), Body::SIDE);
    assert!(close(there.dx, 1.0));
    assert!(close(there.depth, 0.0));
}

#[test]
fn height_is_not_foreshortened() {
    for facing in [EAST, SOUTH, WEST, NORTH] {
        let there = project(Heading::of(facing), Body::UP);
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
    let flat = project(Heading::of(SOUTH), Body::new(4.0, 0.0, 0.0));
    let raised = project(Heading::of(SOUTH), Body::new(4.0, 0.0, 30.0));
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
        let there = project(Heading::of(facing), offset);
        assert!(there.dx.is_finite() && there.dy.is_finite() && there.depth.is_finite());
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

/// A basis is orthonormal, so stating a direction in the frame it is held in
/// and reading it back in the frame itself needs no solve — which is what
/// lets a layer name a target in the body frame and hand a joint the
/// direction in its parent's.
#[test]
fn a_basis_reads_a_held_direction_back_exactly() {
    let basis = Basis::of(Rotation::new(0.31, -0.47, 0.19));
    for local in [
        Body::FORWARD,
        Body::SIDE,
        Body::UP,
        Body::new(3.0, -7.5, 2.25),
        Body::ORIGIN,
    ] {
        let held = basis.apply(local);
        let back = basis.unapply(held);
        for (a, b) in [
            (back.forward, local.forward),
            (back.side, local.side),
            (back.up, local.up),
        ] {
            // A composed rotation is orthonormal to within its own
            // transcendentals, so the round trip is exact to well under a
            // pixel rather than to the last bit.
            assert!(mathf::fabs(a - b) < 1e-9, "{a} against {b}");
        }
    }
}

/// The one direction a point may move along without moving on screen: what
/// a surface's near side is judged against, so a silhouette is exact rather
/// than an approximation of a rotation.
#[test]
fn the_camera_direction_moves_nothing_on_screen() {
    for facing in [EAST, SOUTH, WEST, NORTH, Facing(0x1234)] {
        let toward = toward_camera(Heading::of(facing));
        assert!(close(toward.length(), 1.0), "it must be a direction");
        let here = project(Heading::of(facing), Body::new(3.0, -2.0, 7.0));
        let moved = project(
            Heading::of(facing),
            Body::new(3.0, -2.0, 7.0).plus(toward.scaled(5.0)),
        );
        assert!(
            close(here.dx, moved.dx) && close(here.dy, moved.dy),
            "{facing:?} moved the point on screen"
        );
        assert!(moved.depth > here.depth, "{facing:?} must come nearer");
    }
}
