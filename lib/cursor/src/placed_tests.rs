//! Unit tests for placing a rasterised cursor: hotspot-relative origin,
//! bounds that follow the pointer, and row/pixel sampling.

use alloc::vec;

use tairix_geometry::Point;
use tairix_raster::Color;

use super::PlacedCursor;
use crate::raster::CursorImage;
use crate::vector::{Shape, VectorCursor, Vertex};

/// An opaque `size`×`size` cursor whose hotspot is at `(hx, hy)`.
fn image(size: u32, hx: i32, hy: i32) -> CursorImage {
    let s = i32::try_from(size).unwrap_or(i32::MAX);
    let shape = Shape::new(
        Color::rgb(255, 255, 255),
        vec![
            Vertex::new(0, 0),
            Vertex::new(s, 0),
            Vertex::new(s, s),
            Vertex::new(0, s),
        ],
    );
    VectorCursor::new(size, hx, hy, vec![shape])
        .rasterise(100)
        .expect("renderable")
}

/// A cursor that fills only the left half of its grid, so the right half is
/// transparent.
fn half_image(size: u32) -> CursorImage {
    let s = i32::try_from(size).unwrap_or(i32::MAX);
    let shape = Shape::new(
        Color::rgb(255, 0, 0),
        vec![
            Vertex::new(0, 0),
            Vertex::new(s / 2, 0),
            Vertex::new(s / 2, s),
            Vertex::new(0, s),
        ],
    );
    VectorCursor::new(size, 0, 0, vec![shape])
        .rasterise(100)
        .expect("renderable")
}

#[test]
fn the_origin_is_the_pointer_minus_the_hotspot() {
    let placed = PlacedCursor::new(image(8, 2, 3), Point::new(40, 50));
    let bounds = placed.bounds();
    assert_eq!(bounds.left(), 38);
    assert_eq!(bounds.top(), 47);
    assert_eq!(bounds.width, 8);
    assert_eq!(bounds.height, 8);
}

#[test]
fn the_bounds_follow_the_pointer() {
    let mut placed = PlacedCursor::new(image(8, 2, 3), Point::new(40, 50));
    placed.set_pointer(Point::new(10, 11));
    let bounds = placed.bounds();
    assert_eq!(bounds.left(), 8);
    assert_eq!(bounds.top(), 8);
    assert_eq!(bounds.width, 8);
    assert_eq!(bounds.height, 8);
}

#[test]
fn a_local_row_resolves_only_inside_the_image() {
    let placed = PlacedCursor::new(image(8, 0, 0), Point::new(20, 30));
    assert_eq!(placed.local_row(29), None);
    assert_eq!(placed.local_row(30), Some(0));
    assert_eq!(placed.local_row(37), Some(7));
    assert_eq!(placed.local_row(38), None);
}

#[test]
fn a_row_sample_reads_the_pixel_under_that_screen_column() {
    let placed = PlacedCursor::new(image(8, 0, 0), Point::new(20, 30));
    let ly = placed.local_row(30).expect("inside");
    assert!(placed.sample_row(20, ly).is_some());
    assert!(placed.sample_row(27, ly).is_some());
    assert_eq!(placed.sample_row(19, ly), None);
    assert_eq!(placed.sample_row(28, ly), None);
}

#[test]
fn a_transparent_pixel_draws_nothing() {
    let placed = PlacedCursor::new(half_image(8), Point::new(0, 0));
    assert!(placed.sample_local(0, 0).is_some());
    assert_eq!(placed.sample_local(7, 0), None);
    assert_eq!(placed.sample_local(8, 0), None);
    assert_eq!(placed.sample_local(0, 8), None);
}

#[test]
fn a_pointer_at_the_coordinate_floor_saturates_rather_than_wrapping() {
    let placed = PlacedCursor::new(image(8, 4, 4), Point::new(i32::MIN, i32::MIN));
    assert_eq!(placed.bounds().left(), i32::MIN);
    assert_eq!(placed.bounds().top(), i32::MIN);
    assert_eq!(placed.local_row(i32::MIN), Some(0));
}
