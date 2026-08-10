//! Unit tests for the basic shapes.

use alloc::format;
use alloc::vec::Vec;

use tairix_util::mathf::sqrt;

use crate::error::SvgError;
use crate::geom::{Point, SubPath};
use crate::xml;

use super::{is_shape, shape_subpaths};

/// The viewport percentages resolve against in these tests.
const VIEWPORT: (f64, f64) = (100.0, 100.0);

/// The flattening accuracy the tests ask for.
const TOL: f64 = 0.01;

/// A budget larger than any shape here needs.
const BUDGET: usize = 10_000;

/// The sub-paths one shape element flattens to.
#[track_caller]
fn shapes(tag: &str) -> Vec<SubPath> {
    flatten(tag).expect("a shape")
}

#[track_caller]
fn flatten(tag: &str) -> Result<Vec<SubPath>, SvgError> {
    let document = format!("<svg>{tag}</svg>");
    let root = xml::parse(&document).expect("a document");
    let child = root.children.first().expect("a child element");
    shape_subpaths(child, VIEWPORT, TOL, BUDGET)
}

/// The bounds of every point in `subpaths`.
fn bounds(subpaths: &[SubPath]) -> (Point, Point) {
    let mut min = (f64::MAX, f64::MAX);
    let mut max = (f64::MIN, f64::MIN);
    for point in subpaths.iter().flat_map(|sub| sub.points.iter()) {
        min = (min.0.min(point.0), min.1.min(point.1));
        max = (max.0.max(point.0), max.1.max(point.1));
    }
    (min, max)
}

#[track_caller]
fn close(actual: f64, expected: f64) {
    assert!(
        (actual - expected).abs() <= 1e-6,
        "expected {expected}, got {actual}"
    );
}

#[test]
fn the_drawable_element_names_are_recognised() {
    for name in [
        "path", "rect", "circle", "ellipse", "line", "polyline", "polygon",
    ] {
        assert!(is_shape(name), "{name} should be a shape");
    }
    for name in ["g", "defs", "text", "svg", "linearGradient"] {
        assert!(!is_shape(name), "{name} should not be a shape");
    }
}

// --- rect -----------------------------------------------------------------

#[test]
fn a_rect_is_four_corners() {
    let subpaths = shapes(r#"<rect x="3" y="4" width="10" height="6"/>"#);
    assert_eq!(subpaths.len(), 1);
    assert!(subpaths[0].closed);
    assert_eq!(
        subpaths[0].points,
        alloc::vec![(3.0, 4.0), (13.0, 4.0), (13.0, 10.0), (3.0, 10.0)]
    );
}

#[test]
fn a_rects_position_defaults_to_the_origin() {
    let subpaths = shapes(r#"<rect width="10" height="10"/>"#);
    let (min, max) = bounds(&subpaths);
    close(min.0, 0.0);
    close(min.1, 0.0);
    close(max.0, 10.0);
    close(max.1, 10.0);
}

#[test]
fn a_rounded_rect_keeps_its_extent_but_rounds_its_corners() {
    let subpaths = shapes(r#"<rect width="20" height="20" rx="5"/>"#);
    let (min, max) = bounds(&subpaths);
    close(min.0, 0.0);
    close(max.0, 20.0);
    assert!(subpaths[0].points.len() > 8, "the corners should be curved");
    // No point sits in the square cut away by a corner radius.
    for point in &subpaths[0].points {
        let corner = point.0 < 5.0 && point.1 < 5.0;
        if corner {
            let (dx, dy) = (5.0 - point.0, 5.0 - point.1);
            assert!(
                sqrt(dx * dx + dy * dy) <= 5.0 + TOL,
                "a corner point escaped its radius"
            );
        }
    }
}

/// One radius given means both, and neither may exceed half its side — SVG
/// clamps rather than letting the corners meet and invert.
#[test]
fn a_single_corner_radius_applies_to_both_axes_and_is_clamped() {
    let one = shapes(r#"<rect width="20" height="20" rx="5"/>"#);
    let both = shapes(r#"<rect width="20" height="20" rx="5" ry="5"/>"#);
    assert_eq!(one[0].points.len(), both[0].points.len());

    let clamped = shapes(r#"<rect width="20" height="10" rx="500" ry="500"/>"#);
    let (min, max) = bounds(&clamped);
    close(min.0, 0.0);
    close(max.0, 20.0);
    close(max.1, 10.0);
}

/// A zero extent simply does not render; a negative one is not geometry the
/// author can have meant.
#[test]
fn a_rect_without_area_draws_nothing_and_a_negative_one_is_refused() {
    assert!(shapes(r#"<rect width="0" height="10"/>"#).is_empty());
    assert!(shapes(r#"<rect width="10" height="0"/>"#).is_empty());
    assert_eq!(
        flatten(r#"<rect width="-10" height="10"/>"#),
        Err(SvgError::InvalidNumber)
    );
    assert_eq!(
        flatten(r#"<rect width="10" height="10" rx="-1"/>"#),
        Err(SvgError::InvalidNumber)
    );
}

// --- circle and ellipse ---------------------------------------------------

#[test]
fn a_circle_keeps_every_point_on_its_radius() {
    let subpaths = shapes(r#"<circle cx="10" cy="10" r="5"/>"#);
    assert_eq!(subpaths.len(), 1);
    assert!(subpaths[0].closed);
    for point in &subpaths[0].points {
        let (dx, dy) = (point.0 - 10.0, point.1 - 10.0);
        assert!((sqrt(dx * dx + dy * dy) - 5.0).abs() <= TOL);
    }
}

/// The ring must not repeat its start point, or the contour would carry a
/// duplicate vertex at the seam.
#[test]
fn a_circles_ring_does_not_repeat_its_first_point() {
    let subpaths = shapes(r#"<circle cx="0" cy="0" r="10"/>"#);
    let points = &subpaths[0].points;
    let first = points[0];
    let last = points[points.len() - 1];
    assert!((first.0 - last.0).abs() > 1e-6 || (first.1 - last.1).abs() > 1e-6);
}

#[test]
fn an_ellipse_takes_a_radius_per_axis() {
    let subpaths = shapes(r#"<ellipse cx="0" cy="0" rx="20" ry="10"/>"#);
    let (min, max) = bounds(&subpaths);
    assert!((min.0 + 20.0).abs() <= TOL && (max.0 - 20.0).abs() <= TOL);
    assert!((min.1 + 10.0).abs() <= TOL && (max.1 - 10.0).abs() <= TOL);
}

/// An `auto` radius takes the other axis's value.
#[test]
fn an_auto_ellipse_radius_follows_the_other_axis() {
    let subpaths = shapes(r#"<ellipse cx="0" cy="0" rx="10" ry="auto"/>"#);
    let (min, max) = bounds(&subpaths);
    assert!((max.0 - 10.0).abs() <= TOL && (max.1 - 10.0).abs() <= TOL);
    assert!((min.0 + 10.0).abs() <= TOL && (min.1 + 10.0).abs() <= TOL);
}

#[test]
fn a_radius_of_nothing_draws_nothing_and_a_negative_one_is_refused() {
    assert!(shapes(r#"<circle cx="0" cy="0" r="0"/>"#).is_empty());
    assert!(shapes(r#"<ellipse rx="0" ry="5"/>"#).is_empty());
    assert_eq!(
        flatten(r#"<circle cx="0" cy="0" r="-5"/>"#),
        Err(SvgError::InvalidNumber)
    );
}

// --- line, polyline, polygon ----------------------------------------------

#[test]
fn a_line_is_two_points_and_is_never_closed() {
    let subpaths = shapes(r#"<line x1="1" y1="2" x2="3" y2="4"/>"#);
    assert_eq!(subpaths.len(), 1);
    assert!(!subpaths[0].closed);
    assert_eq!(subpaths[0].points, alloc::vec![(1.0, 2.0), (3.0, 4.0)]);
}

#[test]
fn a_polyline_stays_open_and_a_polygon_closes() {
    let open = shapes(r#"<polyline points="0,0 10,0 10,10"/>"#);
    assert!(!open[0].closed);
    assert_eq!(open[0].points.len(), 3);

    let closed = shapes(r#"<polygon points="0,0 10,0 10,10"/>"#);
    assert!(closed[0].closed);
    assert_eq!(closed[0].points.len(), 3);
}

/// A trailing coordinate with no partner ends the list rather than
/// invalidating everything before it.
#[test]
fn an_odd_trailing_coordinate_ends_the_point_list() {
    let subpaths = shapes(r#"<polygon points="0,0 10,0 10"/>"#);
    assert_eq!(subpaths[0].points.len(), 2);
}

#[test]
fn a_malformed_point_list_is_refused() {
    assert_eq!(
        flatten(r#"<polygon points="0,0 wat,3"/>"#),
        Err(SvgError::InvalidNumber)
    );
}

#[test]
fn an_absent_point_list_or_path_draws_nothing() {
    assert!(shapes("<polygon/>").is_empty());
    assert!(shapes("<path/>").is_empty());
}

// --- percentages and paths ------------------------------------------------

#[test]
fn a_percentage_extent_resolves_against_the_viewport() {
    let subpaths = shapes(r#"<rect width="50%" height="25%"/>"#);
    let (_, max) = bounds(&subpaths);
    close(max.0, 50.0);
    close(max.1, 25.0);
}

#[test]
fn a_path_element_flattens_its_data() {
    let subpaths = shapes(r#"<path d="M0 0 L10 0 L10 10 Z"/>"#);
    assert_eq!(subpaths.len(), 1);
    assert!(subpaths[0].closed);
    assert_eq!(subpaths[0].points.len(), 3);
}

#[test]
fn a_shape_that_exceeds_the_budget_is_refused() {
    let document = format!("<svg>{}</svg>", r#"<polygon points="0,0 1,1 2,2 3,3"/>"#);
    let root = xml::parse(&document).expect("a document");
    let child = root.children.first().expect("a child element");
    assert_eq!(
        shape_subpaths(child, VIEWPORT, TOL, 2),
        Err(SvgError::TooComplex)
    );
}
