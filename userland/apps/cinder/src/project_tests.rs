//! Camera tests: the foreshortening, the turnaround, and the contact shadow.

use super::{
    contact_shadow, ground_distance, heading_towards, project, step, turn_towards, Body, Ground,
    ELEVATION_DEGREES, GROUND_DEPTH,
};
use tairix_util::mathf;

/// How close two floats must be to count as equal here.
const EPSILON: f64 = 1e-9;

fn close(a: f64, b: f64) -> bool {
    mathf::fabs(a - b) < 1e-6
}

#[test]
fn the_depth_constant_is_the_tangent_of_the_stated_elevation() {
    let radians = ELEVATION_DEGREES * core::f64::consts::PI / 180.0;
    assert!(
        mathf::fabs(GROUND_DEPTH - mathf::tan(radians)) < EPSILON,
        "the written constant and the stated elevation must not drift apart"
    );
}

#[test]
fn moving_into_the_screen_covers_less_ground_than_moving_across_it() {
    let speed = 100.0;
    let (across, _) = step(0.0, speed);
    let (_, into) = step(core::f64::consts::FRAC_PI_2, speed);
    assert!(close(across, speed));
    assert!(close(mathf::fabs(into), speed * GROUND_DEPTH));
    assert!(
        mathf::fabs(into) < across,
        "a foreshortened floor is the whole point of the camera"
    );
}

#[test]
fn a_blob_in_front_of_the_body_sorts_nearer_facing_the_camera() {
    let at = Ground::new(100.0, 100.0);
    let muzzle = Body::new(20.0, 0.0, 40.0);
    // Facing the camera (down the screen) the muzzle is nearer than the body.
    let facing = project(at, -core::f64::consts::FRAC_PI_2, muzzle);
    let body = project(at, -core::f64::consts::FRAC_PI_2, Body::new(0.0, 0.0, 40.0));
    assert!(
        facing.depth > body.depth,
        "facing the camera, the face must sort in front of the head"
    );
}

#[test]
fn the_same_blob_sorts_behind_the_body_facing_away() {
    let at = Ground::new(100.0, 100.0);
    let muzzle = Body::new(20.0, 0.0, 40.0);
    let away = project(at, core::f64::consts::FRAC_PI_2, muzzle);
    let body = project(at, core::f64::consts::FRAC_PI_2, Body::new(0.0, 0.0, 40.0));
    assert!(
        away.depth < body.depth,
        "walking away, the face must fall behind the head and simply vanish"
    );
}

#[test]
fn height_moves_a_blob_up_the_screen_without_moving_it_nearer() {
    let at = Ground::new(50.0, 50.0);
    let low = project(at, 0.0, Body::new(0.0, 0.0, 0.0));
    let high = project(at, 0.0, Body::new(0.0, 0.0, 30.0));
    assert!(high.y < low.y, "height draws higher on screen");
    assert!(
        close(high.depth, low.depth),
        "but a lifted paw must not sort in front of the body it belongs to"
    );
}

#[test]
fn a_heading_aims_at_the_floor_position_rather_than_the_screen_position() {
    let from = Ground::new(0.0, 0.0);
    // Straight up the screen is straight away from the camera.
    let away = heading_towards(from, Ground::new(0.0, -100.0));
    assert!(close(away, core::f64::consts::FRAC_PI_2));
    let right = heading_towards(from, Ground::new(100.0, 0.0));
    assert!(close(right, 0.0));
    // A point that is equally far in screen pixels diagonally is *further*
    // away on the floor, so the heading tips past forty-five degrees.
    let diagonal = heading_towards(from, Ground::new(100.0, -100.0));
    assert!(
        diagonal > core::f64::consts::FRAC_PI_4,
        "the floor is foreshortened, so an equal screen offset is a longer walk"
    );
}

#[test]
fn a_heading_to_where_one_already_stands_is_not_a_spin() {
    assert!(close(
        heading_towards(Ground::new(4.0, 4.0), Ground::new(4.0, 4.0)),
        0.0
    ));
}

#[test]
fn ground_distance_measures_the_walk_rather_than_the_look() {
    let from = Ground::new(0.0, 0.0);
    let across = ground_distance(from, Ground::new(100.0, 0.0));
    let into = ground_distance(from, Ground::new(0.0, -100.0));
    assert!(close(across, 100.0));
    assert!(
        into > across,
        "a hundred pixels up the screen is a longer walk than a hundred across"
    );
}

#[test]
fn a_turn_always_takes_the_short_way_round() {
    let nearly_full = core::f64::consts::TAU - 0.1;
    assert!(
        close(turn_towards(0.0, nearly_full), -0.1),
        "turning to almost-a-full-circle is a small turn the other way"
    );
    assert!(close(turn_towards(nearly_full, 0.0), 0.1));
    assert!(close(
        turn_towards(0.0, core::f64::consts::FRAC_PI_2),
        core::f64::consts::FRAC_PI_2
    ));
}

#[test]
fn the_contact_shadow_is_squashed_by_the_camera_and_fades_with_height() {
    let (rx, ry, on_floor) = contact_shadow(20.0, 0.0);
    assert!(close(ry, rx * GROUND_DEPTH), "a floor shadow is an ellipse");
    assert!(on_floor > 0);

    let (high_rx, _, aloft) = contact_shadow(20.0, 40.0);
    assert!(aloft < on_floor, "a jump lightens the shadow");
    assert!(high_rx > rx, "and spreads it");
    assert!(
        aloft > 0,
        "but never erases it: the mark is what reads as a jump"
    );
}
