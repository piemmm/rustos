//! Unit tests for marker placement: the one matrix per instance.

use alloc::format;

use tairix_raster::Affine;

use crate::error::SvgError;
use crate::geom::{Point, Vertex};
use crate::xml;

use super::{Marker, Placement, Position};

/// The referencing element's viewport, which a percentage extent resolves
/// against.
const VIEWPORT: (f64, f64) = (100.0, 100.0);

/// How close two placed coordinates must be to count as equal: the matrices
/// are built from exact unit vectors, so only floating-point rounding stands
/// between them and the expected value.
const EPSILON: f64 = 1e-9;

/// Read the one `<marker>` in a fragment.
#[track_caller]
fn marker(tag: &str) -> Marker {
    read(tag)
        .expect("a readable marker")
        .expect("a drawn marker")
}

#[track_caller]
fn read(tag: &str) -> Result<Option<Marker>, SvgError> {
    let document = format!("<svg>{tag}</svg>");
    let root = xml::parse(&document).expect("a document");
    let node = root.children.first().expect("a child element");
    // Read into an owned value so the borrowed document may be dropped.
    Marker::read(node, VIEWPORT)
}

/// A vertex at the origin running along the positive x axis.
fn along_x() -> Vertex {
    Vertex {
        at: (0.0, 0.0),
        incoming: Some((1.0, 0.0)),
        outgoing: Some((1.0, 0.0)),
    }
}

#[track_caller]
fn place(marker: &Marker, vertex: &Vertex, position: Position, stroke: f64) -> Placement {
    marker
        .place(vertex, position, stroke)
        .expect("a drawn placement")
}

#[track_caller]
fn assert_near(got: Point, want: Point) {
    assert!(
        (got.0 - want.0).abs() < EPSILON && (got.1 - want.1).abs() < EPSILON,
        "placed {got:?}, expected {want:?}"
    );
}

// --- the viewport and its units -------------------------------------------

/// An unstated extent is SVG's three, and the content maps straight through.
#[test]
fn an_unstated_marker_states_a_three_unit_viewport() {
    let read = marker(r#"<marker id="m"/>"#);
    assert_eq!(read.viewport, (3.0, 3.0));
    assert_eq!(read.content_viewport, (3.0, 3.0));
    let placed = place(&read, &along_x(), Position::Mid, 1.0);
    assert_eq!(placed.content, placed.viewport);
}

/// A zero extent disables the marker, exactly as a zero tile disables a
/// pattern; a negative one is not geometry an author can have meant.
#[test]
fn a_marker_with_no_extent_draws_nothing_and_a_negative_one_is_refused() {
    for tag in [
        r#"<marker id="m" markerWidth="0"/>"#,
        r#"<marker id="m" markerHeight="0"/>"#,
    ] {
        assert_eq!(read(tag), Ok(None), "{tag}");
    }
    assert_eq!(
        read(r#"<marker id="m" markerWidth="-1"/>"#),
        Err(SvgError::InvalidNumber)
    );
}

/// The default units measure the marker in the referencing element's stroke
/// widths, so a marker grows with the line it decorates.
#[test]
fn stroke_width_units_scale_the_instance_and_user_space_units_do_not() {
    let scaled = marker(r#"<marker id="m" markerWidth="2" markerHeight="2"/>"#);
    let fixed =
        marker(r#"<marker id="m" markerWidth="2" markerHeight="2" markerUnits="userSpaceOnUse"/>"#);
    let vertex = along_x();
    for width in [1.0, 4.0] {
        assert_near(
            place(&scaled, &vertex, Position::Mid, width)
                .content
                .apply((1.0, 0.0)),
            (width, 0.0),
        );
        assert_near(
            place(&fixed, &vertex, Position::Mid, width)
                .content
                .apply((1.0, 0.0)),
            (1.0, 0.0),
        );
    }
}

/// A stroke of no width scales the marker to nothing under the default units,
/// so there is no instance to draw rather than a collapsed one.
#[test]
fn a_zero_stroke_width_places_no_instance_under_stroke_width_units() {
    let scaled = marker(r#"<marker id="m"/>"#);
    assert_eq!(scaled.place(&along_x(), Position::Mid, 0.0), None);
    let fixed = marker(r#"<marker id="m" markerUnits="userSpaceOnUse"/>"#);
    assert!(fixed.place(&along_x(), Position::Mid, 0.0).is_some());
}

// --- the reference point ---------------------------------------------------

/// `refX`/`refY` name the content point that lands on the vertex.
#[test]
fn the_reference_point_is_what_sits_on_the_vertex() {
    let read = marker(
        r#"<marker id="m" markerWidth="4" markerHeight="4" refX="2" refY="3"
        markerUnits="userSpaceOnUse"/>"#,
    );
    let vertex = Vertex {
        at: (10.0, 20.0),
        ..along_x()
    };
    assert_near(
        place(&read, &vertex, Position::Mid, 1.0)
            .content
            .apply((2.0, 3.0)),
        (10.0, 20.0),
    );
}

/// The reference point is stated in the content's coordinates, so a `viewBox`
/// moves where it lands in the viewport — which is what SVG means by taking it
/// after the fit.
#[test]
fn the_reference_point_is_read_through_the_view_box_fit() {
    let read = marker(
        r#"<marker id="m" markerWidth="4" markerHeight="4" refX="5" refY="5"
        viewBox="0 0 10 10" markerUnits="userSpaceOnUse"/>"#,
    );
    let vertex = Vertex {
        at: (7.0, 7.0),
        ..along_x()
    };
    let placed = place(&read, &vertex, Position::Mid, 1.0);
    // The fit halves the content, so the content's (5,5) is the viewport's
    // (2,2) — the middle of a four-unit viewport, landing on the vertex.
    assert_near(placed.content.apply((5.0, 5.0)), (7.0, 7.0));
    assert_near(placed.viewport.apply((2.0, 2.0)), (7.0, 7.0));
    assert_eq!(read.content_viewport, (10.0, 10.0));
}

/// A `viewBox` fits the content to the marker viewport under
/// `preserveAspectRatio`, so non-square content is letter-boxed rather than
/// stretched.
#[test]
fn a_marker_view_box_is_fitted_under_its_aspect_ratio() {
    let read = marker(
        r#"<marker id="m" markerWidth="4" markerHeight="4" viewBox="0 0 10 5"
        markerUnits="userSpaceOnUse"/>"#,
    );
    let placed = place(&read, &along_x(), Position::Mid, 1.0);
    // Meet fits the wider axis: ten content units span all four of the
    // viewport's, so the five vertical ones span two and keep their shape.
    assert_near(placed.content.apply((0.0, 0.0)), (0.0, 0.0));
    assert_near(placed.content.apply((10.0, 5.0)), (4.0, 2.0));
    // And those two units sit centred in the four the viewport covers, which
    // is the letter-boxing rather than a stretch.
    assert_near(placed.viewport.apply((0.0, 0.0)), (0.0, -1.0));
    assert_near(placed.viewport.apply((4.0, 4.0)), (4.0, 3.0));

    let stretched = marker(
        r#"<marker id="m" markerWidth="4" markerHeight="4" viewBox="0 0 10 5"
        preserveAspectRatio="none" markerUnits="userSpaceOnUse"/>"#,
    );
    let placed = place(&stretched, &along_x(), Position::Mid, 1.0);
    assert_near(placed.content.apply((10.0, 5.0)), (4.0, 4.0));
}

// --- orientation -----------------------------------------------------------

/// `orient="auto"` turns the marker's positive x axis along the path.
#[test]
fn auto_orientation_follows_the_path() {
    let read = marker(r#"<marker id="m" orient="auto" markerUnits="userSpaceOnUse"/>"#);
    let down = Vertex {
        at: (0.0, 0.0),
        incoming: Some((0.0, 1.0)),
        outgoing: Some((0.0, 1.0)),
    };
    assert_near(
        place(&read, &down, Position::Mid, 1.0)
            .content
            .apply((1.0, 0.0)),
        (0.0, 1.0),
    );
}

/// A corner takes the bisector of what arrives and what leaves.
#[test]
fn a_corner_faces_the_bisector_of_its_two_segments() {
    let read = marker(r#"<marker id="m" orient="auto" markerUnits="userSpaceOnUse"/>"#);
    let corner = Vertex {
        at: (0.0, 0.0),
        incoming: Some((1.0, 0.0)),
        outgoing: Some((0.0, 1.0)),
    };
    let half = core::f64::consts::FRAC_1_SQRT_2;
    assert_near(
        place(&read, &corner, Position::Mid, 1.0)
            .content
            .apply((1.0, 0.0)),
        (half, half),
    );
}

/// A path that doubles back exactly has no bisector; approached from either
/// side the answer tends to a perpendicular, and a perpendicular is what it
/// gets rather than whatever the arithmetic would otherwise produce.
#[test]
fn an_exact_reversal_faces_the_perpendicular() {
    let read = marker(r#"<marker id="m" orient="auto" markerUnits="userSpaceOnUse"/>"#);
    let doubled_back = Vertex {
        at: (0.0, 0.0),
        incoming: Some((1.0, 0.0)),
        outgoing: Some((-1.0, 0.0)),
    };
    assert_near(
        place(&read, &doubled_back, Position::Mid, 1.0)
            .content
            .apply((1.0, 0.0)),
        (0.0, 1.0),
    );
}

/// An end of an open sub-path has one segment, and takes its direction; a
/// vertex with neither faces the positive x axis, where an unoriented marker
/// already points.
#[test]
fn one_sided_and_directionless_vertices_still_place() {
    let read = marker(r#"<marker id="m" orient="auto" markerUnits="userSpaceOnUse"/>"#);
    let leaving_only = Vertex {
        at: (0.0, 0.0),
        incoming: None,
        outgoing: Some((0.0, -1.0)),
    };
    assert_near(
        place(&read, &leaving_only, Position::Start, 1.0)
            .content
            .apply((1.0, 0.0)),
        (0.0, -1.0),
    );
    let alone = Vertex::default();
    assert_near(
        place(&read, &alone, Position::Start, 1.0)
            .content
            .apply((1.0, 0.0)),
        (1.0, 0.0),
    );
}

/// `auto-start-reverse` is `auto` turned about at the start and nowhere else,
/// which is what lets one arrowhead point out of both ends of a path.
#[test]
fn auto_start_reverse_turns_only_the_start_about() {
    let read =
        marker(r#"<marker id="m" orient="auto-start-reverse" markerUnits="userSpaceOnUse"/>"#);
    let vertex = along_x();
    assert_near(
        place(&read, &vertex, Position::Start, 1.0)
            .content
            .apply((1.0, 0.0)),
        (-1.0, 0.0),
    );
    for position in [Position::Mid, Position::End] {
        assert_near(
            place(&read, &vertex, position, 1.0)
                .content
                .apply((1.0, 0.0)),
            (1.0, 0.0),
        );
    }
}

/// A stated angle ignores the path, and reads in degrees unless it names a
/// unit.
#[test]
fn a_stated_angle_is_degrees_unless_it_names_a_unit() {
    let vertex = Vertex {
        at: (0.0, 0.0),
        incoming: Some((0.0, 1.0)),
        outgoing: Some((0.0, 1.0)),
    };
    for spelling in ["90", "90deg", "100grad", "0.25turn"] {
        let read = marker(&format!(
            r#"<marker id="m" orient="{spelling}" markerUnits="userSpaceOnUse"/>"#
        ));
        assert_near(
            place(&read, &vertex, Position::Mid, 1.0)
                .content
                .apply((1.0, 0.0)),
            (0.0, 1.0),
        );
    }
    let radians =
        marker(r#"<marker id="m" orient="3.14159265358979rad" markerUnits="userSpaceOnUse"/>"#);
    assert_near(
        place(&radians, &vertex, Position::Mid, 1.0)
            .content
            .apply((1.0, 0.0)),
        (-1.0, 0.0),
    );
}

/// An `orient` the grammar does not admit refuses the document, exactly as a
/// malformed length does: drawing the marker at some other angle would be a
/// picture the author did not write.
#[test]
fn a_malformed_orient_is_refused() {
    for spelling in ["sideways", "", "90degrees", "auto-reverse"] {
        assert_eq!(
            read(&format!(r#"<marker id="m" orient="{spelling}"/>"#)),
            Err(SvgError::InvalidNumber),
            "{spelling}"
        );
    }
}

/// An angle far outside one turn still turns the marker rather than resizing
/// it: the matrix is normalised, so it stays a rotation.
#[test]
fn an_enormous_angle_still_turns_rather_than_scales() {
    let read = marker(r#"<marker id="m" orient="1e17" markerUnits="userSpaceOnUse"/>"#);
    let placed = place(&read, &along_x(), Position::Mid, 1.0);
    let unit = placed.content.apply((1.0, 0.0));
    let length = tairix_util::mathf::hypot(unit.0, unit.1);
    assert!((length - 1.0).abs() < EPSILON, "scaled by {length}");
}

// --- the whole chain -------------------------------------------------------

/// Every part of the placement composes into one matrix, in SVG's order:
/// reference point off, marker units on, turned to face the path, moved to
/// the vertex.
#[test]
fn the_placement_is_one_matrix_in_the_specified_order() {
    let read = marker(
        r#"<marker id="m" markerWidth="2" markerHeight="2" refX="1" refY="1"
        orient="auto"/>"#,
    );
    let vertex = Vertex {
        at: (5.0, 6.0),
        incoming: Some((0.0, 1.0)),
        outgoing: Some((0.0, 1.0)),
    };
    let placed = place(&read, &vertex, Position::End, 3.0);
    let expected = Affine::translate(-1.0, -1.0)
        .then(Affine::scale(3.0, 3.0))
        .then(Affine::rotate_degrees(90.0))
        .then(Affine::translate(5.0, 6.0));
    for probe in [(0.0, 0.0), (1.0, 1.0), (2.0, 0.5)] {
        assert_near(placed.content.apply(probe), expected.apply(probe));
    }
}
