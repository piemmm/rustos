//! Unit tests for the affine transform.

use tairix_util::mathf;

use super::Affine;

/// How close two mapped coordinates must be to count as equal.
///
/// The transcendental kernels behind a rotation are accurate to about 1e-9
/// relative, which is far finer than the sub-pixel grid anything rasterises
/// onto, so a fixed tolerance is enough for every case here.
const TOLERANCE: f64 = 1e-9;

#[track_caller]
fn assert_close(got: f64, want: f64) {
    let error = if got > want { got - want } else { want - got };
    assert!(error <= TOLERANCE, "got {got}, want {want}");
}

#[track_caller]
fn assert_point_close(got: (f64, f64), want: (f64, f64)) {
    assert_close(got.0, want.0);
    assert_close(got.1, want.1);
}

#[test]
fn identity_leaves_a_point_alone() {
    assert_point_close(Affine::IDENTITY.apply((3.0, -7.5)), (3.0, -7.5));
}

#[test]
fn translate_and_scale_move_and_stretch() {
    assert_point_close(Affine::translate(2.0, -3.0).apply((1.0, 1.0)), (3.0, -2.0));
    assert_point_close(Affine::scale(2.0, 0.5).apply((3.0, 8.0)), (6.0, 4.0));
}

#[test]
fn then_applies_the_receiver_first() {
    // Scaling a translated point is not translating a scaled one: the order
    // the composition applies is what the reading order says.
    let scale_after = Affine::translate(1.0, 0.0).then(Affine::scale(10.0, 10.0));
    let translate_after = Affine::scale(10.0, 10.0).then(Affine::translate(1.0, 0.0));
    assert_point_close(scale_after.apply((0.0, 0.0)), (10.0, 0.0));
    assert_point_close(translate_after.apply((0.0, 0.0)), (1.0, 0.0));
}

#[test]
fn then_matches_applying_each_transform_in_turn() {
    let inner = Affine::rotate_degrees(30.0);
    let outer = Affine::translate(4.0, -2.0).then(Affine::scale(3.0, 5.0));
    let point = (1.5, -0.25);
    assert_point_close(
        inner.then(outer).apply(point),
        outer.apply(inner.apply(point)),
    );
}

#[test]
fn rotation_turns_a_known_point_a_quarter_turn() {
    // Positive angles turn clockwise on a y-down canvas, so the x axis lands
    // on the y axis.
    assert_point_close(Affine::rotate_degrees(90.0).apply((1.0, 0.0)), (0.0, 1.0));
    assert_point_close(Affine::rotate_degrees(180.0).apply((1.0, 0.0)), (-1.0, 0.0));
}

#[test]
fn rotation_about_a_centre_keeps_that_centre_fixed() {
    let rotate = Affine::rotate_degrees_about(37.0, 12.0, -5.0);
    assert_point_close(rotate.apply((12.0, -5.0)), (12.0, -5.0));
    // The rotated point keeps its distance from the centre.
    let (x, y) = rotate.apply((12.0, 0.0));
    assert_close((x - 12.0) * (x - 12.0) + (y + 5.0) * (y + 5.0), 25.0);
}

#[test]
fn skew_shears_along_one_axis_only() {
    assert_point_close(Affine::skew_x_degrees(45.0).apply((0.0, 1.0)), (1.0, 1.0));
    assert_point_close(Affine::skew_x_degrees(45.0).apply((1.0, 0.0)), (1.0, 0.0));
    assert_point_close(Affine::skew_y_degrees(45.0).apply((1.0, 0.0)), (1.0, 1.0));
    assert_point_close(Affine::skew_y_degrees(45.0).apply((0.0, 1.0)), (0.0, 1.0));
}

#[test]
fn invert_round_trips_a_point() {
    let transform = Affine::translate(3.0, -4.0)
        .then(Affine::rotate_degrees(25.0))
        .then(Affine::scale(2.5, -0.75))
        .then(Affine::skew_x_degrees(10.0));
    let inverse = transform.invert().expect("the transform is invertible");
    let point = (7.25, -3.5);
    assert_point_close(inverse.apply(transform.apply(point)), point);
    assert_point_close(transform.apply(inverse.apply(point)), point);
}

#[test]
fn a_degenerate_transform_has_no_inverse() {
    // A zero scale, a single collapsed axis, and two linearly dependent rows
    // all flatten the plane, so none of them can be undone.
    assert_eq!(Affine::scale(0.0, 0.0).invert(), None);
    assert_eq!(Affine::scale(4.0, 0.0).invert(), None);
    let dependent = Affine {
        a: 2.0,
        b: 4.0,
        c: 1.0,
        d: 2.0,
        e: 9.0,
        f: -1.0,
    };
    assert_eq!(dependent.invert(), None);
}

#[test]
fn a_non_finite_transform_has_no_inverse() {
    let broken = Affine {
        a: f64::NAN,
        ..Affine::IDENTITY
    };
    assert_eq!(broken.invert(), None);
    let huge = Affine {
        a: f64::MAX,
        d: f64::MAX,
        ..Affine::IDENTITY
    };
    assert_eq!(huge.invert(), None);
}

#[test]
fn max_scale_reports_the_larger_stretch() {
    assert_close(Affine::IDENTITY.max_scale(), 1.0);
    assert_close(Affine::scale(3.0, 3.0).max_scale(), 3.0);
    assert_close(Affine::scale(3.0, 7.0).max_scale(), 7.0);
    assert_close(Affine::scale(-6.0, 2.0).max_scale(), 6.0);
    // A translation stretches nothing.
    assert_close(Affine::translate(100.0, -100.0).max_scale(), 1.0);
}

#[test]
fn max_scale_ignores_rotation_and_translation() {
    let placed = Affine::scale(4.0, 4.0)
        .then(Affine::rotate_degrees(33.0))
        .then(Affine::translate(-17.0, 6.0));
    assert_close(placed.max_scale(), 4.0);
}

#[test]
fn max_scale_of_a_shear_exceeds_its_axis_scales() {
    // A 45° shear stretches the diagonal it leans along, which is why a
    // flattening tolerance cannot be taken from the diagonal terms alone.
    let scale = Affine::skew_x_degrees(45.0).max_scale();
    assert!(scale > 1.0, "a shear must stretch something: {scale}");
    assert_close(scale, f64::midpoint(mathf::sqrt(5.0), 1.0));
}

#[test]
fn max_scale_of_a_collapsed_or_broken_transform_is_zero() {
    assert_close(Affine::scale(0.0, 0.0).max_scale(), 0.0);
    let broken = Affine {
        a: f64::INFINITY,
        ..Affine::IDENTITY
    };
    assert_close(broken.max_scale(), 0.0);
    let unmeasurable = Affine {
        b: f64::NAN,
        ..Affine::IDENTITY
    };
    assert_close(unmeasurable.max_scale(), 0.0);
}
