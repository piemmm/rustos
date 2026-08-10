//! Unit tests for the path grammar and curve flattening.

use alloc::vec::Vec;

use core::f64::consts::PI;

use tairix_util::mathf::{round, sqrt};

use crate::error::SvgError;
use crate::geom::Point;

use super::{
    cubic_at, flatten_cubic, flatten_ellipse_arc, flatten_quadratic, parse_path_data, MAX_SEGMENTS,
};

/// A generous point budget: every test here draws a handful of segments, so
/// only the budget test itself should ever reach it.
const BUDGET: usize = 10_000;

/// The flattening accuracy the tests ask for, in the same user units as their
/// coordinates.
const TOL: f64 = 0.01;

#[track_caller]
fn close(actual: f64, expected: f64) {
    assert!(
        (actual - expected).abs() <= 1e-9,
        "expected {expected}, got {actual}"
    );
}

#[track_caller]
fn close_point(actual: Point, expected: Point) {
    close(actual.0, expected.0);
    close(actual.1, expected.1);
}

/// The points of a path that parses to exactly one sub-path.
#[track_caller]
fn one(d: &str) -> Vec<Point> {
    let subpaths = parse_path_data(d, TOL, BUDGET).expect("a valid path");
    assert_eq!(subpaths.len(), 1, "expected one sub-path from {d:?}");
    subpaths[0].points.clone()
}

// --- the straight-line commands -----------------------------------------

#[test]
fn absolute_and_relative_linetos_reach_the_same_place() {
    close_point(one("M10 10 L30 40")[1], (30.0, 40.0));
    close_point(one("M10 10 l20 30")[1], (30.0, 40.0));
}

#[test]
fn horizontal_and_vertical_commands_hold_the_other_axis() {
    let points = one("M10 10 H30 V40 h-5 v-5");
    close_point(points[1], (30.0, 10.0));
    close_point(points[2], (30.0, 40.0));
    close_point(points[3], (25.0, 40.0));
    close_point(points[4], (25.0, 35.0));
}

/// A bare parameter set repeats the previous command — except after a
/// moveto, where SVG defines the repeat as a lineto.
#[test]
fn a_repeated_parameter_set_repeats_the_command() {
    let points = one("M0 0 L10 0 20 0 30 0");
    assert_eq!(points.len(), 4);
    close_point(points[3], (30.0, 0.0));
}

#[test]
fn a_repeated_moveto_parameter_set_is_a_lineto() {
    let points = one("M0 0 10 0 20 0");
    assert_eq!(points.len(), 3);
    close_point(points[2], (20.0, 0.0));
}

#[test]
fn a_second_moveto_starts_a_second_subpath() {
    let subpaths = parse_path_data("M0 0 L5 0 M10 10 L15 10", TOL, BUDGET).expect("two sub-paths");
    assert_eq!(subpaths.len(), 2);
    close_point(subpaths[1].points[0], (10.0, 10.0));
}

#[test]
fn closepath_marks_the_subpath_and_returns_the_pen_to_its_start() {
    let subpaths = parse_path_data("M10 10 L20 10 L20 20 Z M30 30 L40 30", TOL, BUDGET)
        .expect("a closed then an open sub-path");
    assert_eq!(subpaths.len(), 2);
    assert!(subpaths[0].closed);
    assert!(!subpaths[1].closed);
}

/// After a closepath the pen sits on the sub-path's start, and a segment
/// that follows without a moveto begins a fresh contour there.
#[test]
fn a_segment_after_a_closepath_begins_at_the_start_point() {
    let subpaths = parse_path_data("M10 10 L20 10 Z L30 30", TOL, BUDGET).expect("two sub-paths");
    assert_eq!(subpaths.len(), 2);
    close_point(subpaths[1].points[0], (10.0, 10.0));
    close_point(subpaths[1].points[1], (30.0, 30.0));
}

/// A relative command after a closepath measures from the start point, not
/// from where the pen was when the sub-path closed.
#[test]
fn relative_movement_after_a_closepath_measures_from_the_start_point() {
    let subpaths =
        parse_path_data("M10 10 L90 90 Z m5 5 l1 0", TOL, BUDGET).expect("two sub-paths");
    close_point(subpaths[1].points[0], (15.0, 15.0));
}

#[test]
fn numbers_may_run_together_without_separators() {
    let points = one("M0-4L2-6");
    close_point(points[0], (0.0, -4.0));
    close_point(points[1], (2.0, -6.0));
}

// --- curves ---------------------------------------------------------------

/// The flattened polyline must stay within the tolerance of the true curve,
/// which is the whole promise of an error-bounded subdivision.
#[test]
fn a_flattened_cubic_stays_within_tolerance_of_the_true_curve() {
    let (from, c1, c2, to) = ((0.0, 0.0), (0.0, 100.0), (100.0, 100.0), (100.0, 0.0));
    let mut flat = alloc::vec![from];
    flatten_cubic(from, c1, c2, to, TOL, &mut flat);
    close_point(flat[flat.len() - 1], to);

    // Every point of the true curve is near some segment of the polyline.
    for step in 0..=200 {
        let t = f64::from(step) / 200.0;
        let exact = cubic_at(from, c1, c2, to, t);
        let nearest = flat
            .windows(2)
            .map(|pair| distance_to_segment(exact, pair[0], pair[1]))
            .fold(f64::MAX, f64::min);
        assert!(nearest <= TOL * 1.5, "curve departs by {nearest}");
    }
}

#[test]
fn a_tighter_tolerance_never_gives_fewer_points() {
    let (from, c1, c2, to) = ((0.0, 0.0), (0.0, 50.0), (50.0, 50.0), (50.0, 0.0));
    let mut coarse = Vec::new();
    let mut fine = Vec::new();
    flatten_cubic(from, c1, c2, to, 1.0, &mut coarse);
    flatten_cubic(from, c1, c2, to, 0.01, &mut fine);
    assert!(fine.len() >= coarse.len());
}

#[test]
fn a_straight_cubic_needs_only_one_segment() {
    let mut flat = Vec::new();
    flatten_cubic(
        (0.0, 0.0),
        (10.0, 0.0),
        (20.0, 0.0),
        (30.0, 0.0),
        TOL,
        &mut flat,
    );
    assert_eq!(flat.len(), 1);
}

#[test]
fn a_quadratic_passes_through_its_endpoints() {
    let mut flat = Vec::new();
    flatten_quadratic((0.0, 0.0), (50.0, 100.0), (100.0, 0.0), TOL, &mut flat);
    close_point(flat[flat.len() - 1], (100.0, 0.0));
    // The curve's midpoint is the average of the endpoints and the control,
    // weighted a quarter, a half, a quarter.
    let mid = flat[flat.len() / 2 - 1];
    assert!(mid.1 > 40.0 && mid.1 < 50.0, "midpoint height {}", mid.1);
}

/// `S` mirrors the previous cubic's control point, but only when the
/// previous command really was a cubic.
#[test]
fn a_smooth_cubic_reflects_only_after_another_cubic() {
    let reflected = one("M0 0 C0 10 10 10 10 0 S20 -10 20 0");
    let unreflected = one("M0 0 L10 0 S20 -10 20 0");
    close_point(reflected[reflected.len() - 1], (20.0, 0.0));
    close_point(unreflected[unreflected.len() - 1], (20.0, 0.0));
    // The reflected curve leaves the seam upward, the unreflected one does
    // not, so their shapes differ even though their endpoints match.
    assert_ne!(reflected.len(), 0);
    let seam = reflected
        .iter()
        .position(|point| (point.0 - 10.0).abs() < 1e-9 && point.1.abs() < 1e-9)
        .expect("the seam point");
    assert!(reflected[seam + 1].1 < 0.0, "the reflection should rise");
}

#[test]
fn a_smooth_quadratic_reflects_its_control_point() {
    let points = one("M0 0 Q5 10 10 0 T20 0");
    close_point(points[points.len() - 1], (20.0, 0.0));
    // The second curve mirrors the first, so it dips where the first rose.
    let lowest = points.iter().fold(f64::MAX, |acc, point| acc.min(point.1));
    assert!(lowest < 0.0, "the reflected quadratic should dip");
}

// --- elliptical arcs ------------------------------------------------------

#[test]
fn an_arc_lands_exactly_on_its_endpoint() {
    let points = one("M0 0 A50 50 0 0 1 100 0");
    close_point(points[points.len() - 1], (100.0, 0.0));
}

/// A quarter circle must bulge the right way and stay on its radius.
#[test]
fn a_quarter_circle_arc_keeps_its_radius() {
    let points = one("M0 50 A50 50 0 0 1 50 0");
    for point in &points {
        let (dx, dy) = (point.0 - 50.0, point.1 - 50.0);
        let radius = sqrt(dx * dx + dy * dy);
        assert!((radius - 50.0).abs() <= 0.1, "radius drifted to {radius}");
    }
}

/// The four flag combinations pick the four different arcs between the same
/// two points.
#[test]
fn the_arc_flags_choose_four_distinct_arcs() {
    // The two small arcs stay inside the endpoints' box, so only the point
    // halfway along each arc tells all four apart.
    let mut midpoints = Vec::new();
    for (large, sweep) in [(0, 0), (0, 1), (1, 0), (1, 1)] {
        let d = alloc::format!("M0 0 A50 50 0 {large} {sweep} 50 50");
        let points = one(&d);
        let middle = points[points.len() / 2];
        midpoints.push((round(middle.0), round(middle.1)));
    }
    let mut unique = midpoints.clone();
    unique.sort_by(|a, b| a.partial_cmp(b).unwrap_or(core::cmp::Ordering::Equal));
    unique.dedup();
    assert_eq!(
        unique.len(),
        4,
        "the four arcs should differ: {midpoints:?}"
    );
}

/// Radii too small to join the endpoints are grown until they just reach,
/// rather than the arc being refused.
#[test]
fn undersized_arc_radii_are_scaled_up_to_reach() {
    let points = one("M0 0 A1 1 0 0 1 100 0");
    close_point(points[points.len() - 1], (100.0, 0.0));
    // Scaled to a radius of 50, the half-circle reaches that far off the
    // line — above it, since a positive sweep turns that way with the y axis
    // pointing down.
    let reach = points.iter().fold(0.0_f64, |acc, point| acc.min(point.1));
    assert!((reach + 50.0).abs() <= 0.5, "reach {reach}");
}

#[test]
fn a_zero_radius_arc_is_a_straight_line() {
    let points = one("M0 0 A0 0 0 0 1 10 10");
    assert_eq!(points.len(), 2);
    close_point(points[1], (10.0, 10.0));
}

#[test]
fn an_arc_that_ends_where_it_began_draws_nothing_new() {
    let points = one("M10 10 A5 5 0 1 1 10 10");
    assert_eq!(points.len(), 2);
    close_point(points[1], (10.0, 10.0));
}

#[test]
fn a_rotated_arc_tilts_its_ellipse() {
    let upright = one("M0 0 A40 20 0 0 1 60 0");
    let tilted = one("M0 0 A40 20 90 0 1 60 0");
    let upright_depth = upright.iter().fold(0.0_f64, |acc, p| acc.max(p.1.abs()));
    let tilted_depth = tilted.iter().fold(0.0_f64, |acc, p| acc.max(p.1.abs()));
    assert!(
        tilted_depth > upright_depth,
        "rotating the ellipse should change how far the arc bulges"
    );
}

/// A whole turn is a legitimate sweep, which is what draws a circle.
#[test]
fn a_full_sweep_closes_on_its_own_start() {
    let mut points = Vec::new();
    flatten_ellipse_arc(
        (0.0, 0.0),
        (10.0, 10.0),
        0.0,
        0.0,
        2.0 * PI,
        TOL,
        &mut points,
    );
    close_point(points[points.len() - 1], (10.0, 0.0));
    assert!(points.len() > 8);
}

// --- refusals and bounds --------------------------------------------------

#[test]
fn a_path_must_begin_with_a_moveto() {
    assert_eq!(
        parse_path_data("L10 10", TOL, BUDGET),
        Err(SvgError::UnsupportedPath)
    );
    assert_eq!(
        parse_path_data("Z", TOL, BUDGET),
        Err(SvgError::UnsupportedPath)
    );
}

#[test]
fn an_unknown_command_is_refused() {
    assert_eq!(
        parse_path_data("M0 0 X5 5", TOL, BUDGET),
        Err(SvgError::UnsupportedPath)
    );
}

#[test]
fn a_missing_parameter_is_refused() {
    assert_eq!(
        parse_path_data("M0 0 L", TOL, BUDGET),
        Err(SvgError::InvalidNumber)
    );
    assert_eq!(
        parse_path_data("M0", TOL, BUDGET),
        Err(SvgError::InvalidNumber)
    );
}

#[test]
fn a_malformed_arc_flag_is_refused() {
    assert_eq!(
        parse_path_data("M0 0 A1 1 0 5 1 2 2", TOL, BUDGET),
        Err(SvgError::InvalidNumber)
    );
}

#[test]
fn an_empty_path_draws_nothing() {
    assert_eq!(parse_path_data("", TOL, BUDGET), Ok(Vec::new()));
    assert_eq!(parse_path_data("   ", TOL, BUDGET), Ok(Vec::new()));
}

/// A single-point sub-path is legal: it fills as nothing but strokes as a
/// round-capped dot.
#[test]
fn a_lone_moveto_yields_a_single_point_subpath() {
    let subpaths = parse_path_data("M10 10 Z", TOL, BUDGET).expect("one point");
    assert_eq!(subpaths.len(), 1);
    assert_eq!(subpaths[0].points.len(), 1);
    assert!(subpaths[0].closed);
}

#[test]
fn exceeding_the_point_budget_is_refused() {
    assert_eq!(
        parse_path_data("M0 0 L1 1 L2 2 L3 3", TOL, 3),
        Err(SvgError::TooComplex)
    );
}

/// However hostile the input, one curve can never ask for an unbounded
/// subdivision.
#[test]
fn no_single_curve_exceeds_the_segment_bound() {
    let mut flat = Vec::new();
    flatten_cubic(
        (0.0, 0.0),
        (1e12, 1e12),
        (-1e12, 1e12),
        (1.0, 0.0),
        1e-9,
        &mut flat,
    );
    assert!(flat.len() <= MAX_SEGMENTS as usize);

    let mut arc = Vec::new();
    flatten_ellipse_arc((0.0, 0.0), (1e12, 1e12), 0.0, 0.0, 1e6, 1e-12, &mut arc);
    assert!(arc.len() <= MAX_SEGMENTS as usize);
}

/// A degenerate tolerance must not ask for an endless subdivision.
#[test]
fn a_degenerate_tolerance_is_floored() {
    for tolerance in [0.0, -1.0, f64::NAN, f64::INFINITY] {
        let mut flat = Vec::new();
        flatten_quadratic((0.0, 0.0), (10.0, 10.0), (20.0, 0.0), tolerance, &mut flat);
        assert!(!flat.is_empty() && flat.len() <= MAX_SEGMENTS as usize);
    }
}

/// The parser is run on hostile strings for one property only: it must
/// always answer, never panic, and never emit a value that is not a number.
#[test]
fn assorted_hostile_paths_never_panic() {
    let cases = [
        "M",
        "m",
        "z",
        "Zz",
        "M0 0Z Z Z",
        "A",
        "M0 0 A",
        "M0 0 a1",
        "M0,0,,,L1,1",
        "M0 0 C1",
        "M0 0 c",
        "M1e400 0 L1 1",
        "M0 0 L0x10 1",
        "M0 0 t",
        "M0 0 s1 1",
        "M0 0 A1 1 0 1 1 0 0",
        "M0 0 A-1 -1 0 1 1 5 5",
        "M.5.5.5.5",
        "M0 0 H",
        "M0 0 V",
        "M0 0 l1 1 ",
        "MMM",
        "M0 0 Q1 1 2 2 T",
        "M0 0 A1 1 0 11 5 5",
    ];
    for case in cases {
        if let Ok(subpaths) = parse_path_data(case, TOL, BUDGET) {
            for point in subpaths.iter().flat_map(|sub| sub.points.iter()) {
                assert!(point.0.is_finite() && point.1.is_finite(), "{case:?}");
            }
        }
    }
}

/// The distance from `point` to the segment `a`..`b`, used to measure how
/// far a flattened polyline departs from the curve it approximates.
fn distance_to_segment(point: Point, a: Point, b: Point) -> f64 {
    let (dx, dy) = (b.0 - a.0, b.1 - a.1);
    let length2 = dx * dx + dy * dy;
    if length2 == 0.0 {
        return distance(point, a);
    }
    let t = (((point.0 - a.0) * dx + (point.1 - a.1) * dy) / length2).clamp(0.0, 1.0);
    distance(point, (a.0 + t * dx, a.1 + t * dy))
}

/// The distance between two points.
fn distance(a: Point, b: Point) -> f64 {
    let (dx, dy) = (a.0 - b.0, a.1 - b.1);
    sqrt(dx * dx + dy * dy)
}
