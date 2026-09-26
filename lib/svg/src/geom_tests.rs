//! Unit tests for carrying geometry between coordinate spaces.
//!
//! A non-scaling stroke is outlined in the host space rather than the
//! element's, so both the contour and the marker vertex have to be readable
//! from there — and a direction is not carried the way a point is.

use alloc::vec;

use tairix_raster::Affine;

use super::{place, Point, SubPath, Vertex};

/// Whether two points agree to the last few bits, which is all an exact
/// rotation of an exact unit vector can lose.
fn close(left: Point, right: Point) -> bool {
    (left.0 - right.0).abs() < 1e-12 && (left.1 - right.1).abs() < 1e-12
}

#[test]
fn mapping_a_contour_moves_its_points_and_keeps_its_closure() {
    let open = SubPath::open(vec![(1.0, 2.0), (3.0, 4.0)]);
    let moved = open.mapped(Affine::scale(2.0, 3.0).then(Affine::translate(1.0, 1.0)));
    assert_eq!(moved.points, vec![(3.0, 7.0), (7.0, 13.0)]);
    assert!(!moved.closed);

    let closed = SubPath::closed(vec![(0.0, 0.0)]);
    assert!(closed.mapped(Affine::IDENTITY).closed);
}

#[test]
fn a_mapped_vertex_moves_its_point_but_only_rotates_its_directions() {
    let vertex = Vertex {
        at: (2.0, 0.0),
        incoming: Some((1.0, 0.0)),
        outgoing: Some((0.0, 1.0)),
    };
    // A translation moves where the marker sits and not the way the path
    // runs through it.
    let shifted = vertex.mapped(Affine::translate(5.0, 7.0));
    assert_eq!(shifted.at, (7.0, 7.0));
    assert_eq!(shifted.incoming, vertex.incoming);
    assert_eq!(shifted.outgoing, vertex.outgoing);

    let turned = vertex.mapped(Affine::rotate_degrees(90.0));
    assert!(close(turned.at, (0.0, 2.0)), "{:?}", turned.at);
    let arriving = turned.incoming.expect("a direction survives a rotation");
    assert!(close(arriving, (0.0, 1.0)), "{arriving:?}");
}

#[test]
fn a_scaled_direction_is_renormalised_rather_than_stretched() {
    let vertex = Vertex {
        at: (0.0, 0.0),
        incoming: Some((1.0, 0.0)),
        outgoing: Some((0.6, 0.8)),
    };
    let scaled = vertex.mapped(Affine::scale(4.0, 1.0));
    assert_eq!(scaled.incoming, Some((1.0, 0.0)));
    let (x, y) = scaled.outgoing.expect("a direction survives a scale");
    assert!(
        (x * x + y * y - 1.0).abs() < 1e-12,
        "a direction stayed {x},{y}"
    );
    // An anisotropic scale turns the direction as well as rescaling it.
    assert!(x > 0.9, "the wider axis should dominate, got {x}");
}

#[test]
fn a_direction_a_map_collapses_is_dropped() {
    let vertex = Vertex {
        at: (1.0, 1.0),
        incoming: Some((0.0, 1.0)),
        outgoing: Some((1.0, 0.0)),
    };
    // A map that flattens onto the x axis leaves the vertical direction with
    // no length, which is the same "no direction here" a zero-length segment
    // already states.
    let flattened = vertex.mapped(Affine::scale(1.0, 0.0));
    assert_eq!(flattened.incoming, None);
    assert_eq!(flattened.outgoing, Some((1.0, 0.0)));
}

#[test]
fn a_vertex_with_no_directions_maps_to_one_with_none() {
    let bare = Vertex {
        at: (3.0, 4.0),
        incoming: None,
        outgoing: None,
    };
    let moved = bare.mapped(Affine::scale(2.0, 2.0));
    assert_eq!(moved.at, (6.0, 8.0));
    assert_eq!(moved.incoming, None);
    assert_eq!(moved.outgoing, None);
}

#[test]
fn placing_rounds_onto_the_grid_and_drops_contours_with_no_area() {
    let triangle = SubPath::closed(vec![(0.2, 0.2), (1.26, 0.2), (0.2, 1.74)]);
    let sliver = SubPath::open(vec![(0.0, 0.0), (1.0, 1.0)]);
    let placed = place(&[triangle, sliver], Affine::scale(10.0, 10.0));
    assert_eq!(placed, vec![vec![(2, 2), (13, 2), (2, 17)]]);
}
