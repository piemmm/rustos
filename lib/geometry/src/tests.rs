//! Unit tests for the integer geometry primitives.

use super::{to_i32, Point, Rect, Scale, REFERENCE_DPI};

#[test]
fn rect_intersection_overlap() {
    let a = Rect::new(0, 0, 4, 4);
    let b = Rect::new(2, 2, 4, 4);
    assert_eq!(a.intersection(&b), Rect::new(2, 2, 2, 2));
}

#[test]
fn rect_intersection_disjoint_is_empty() {
    let a = Rect::new(0, 0, 2, 2);
    let b = Rect::new(5, 5, 2, 2);
    assert!(a.intersection(&b).is_empty());
}

#[test]
fn rect_union_with_empty_is_other() {
    let r = Rect::new(1, 2, 3, 4);
    assert_eq!(Rect::EMPTY.union(&r), r);
    assert_eq!(r.union(&Rect::EMPTY), r);
}

#[test]
fn rect_union_covers_both() {
    let a = Rect::new(0, 0, 2, 2);
    let b = Rect::new(4, 4, 2, 2);
    assert_eq!(a.union(&b), Rect::new(0, 0, 6, 6));
}

#[test]
fn rect_contains_is_half_open() {
    let r = Rect::new(0, 0, 2, 2);
    assert!(r.contains(Point::new(0, 0)));
    assert!(r.contains(Point::new(1, 1)));
    assert!(!r.contains(Point::new(2, 0)));
    assert!(!r.contains(Point::new(0, 2)));
}

#[test]
fn empty_rect_is_empty() {
    assert!(Rect::EMPTY.is_empty());
    assert!(Rect::new(0, 0, 0, 5).is_empty());
    assert!(Rect::new(0, 0, 5, 0).is_empty());
    assert!(!Rect::new(0, 0, 1, 1).is_empty());
}

#[test]
fn edges_saturate_rather_than_wrap() {
    let r = Rect::new(i32::MAX - 1, i32::MAX - 1, 100, 100);
    assert_eq!(r.right(), i32::MAX);
    assert_eq!(r.bottom(), i32::MAX);
}

#[test]
fn scale_one_is_identity() {
    let one = Scale::ONE;
    assert_eq!(one.percent(), 100);
    assert_eq!(one.dpi(), REFERENCE_DPI);
    assert_eq!(one.scale_length(40), 40);
    assert_eq!(one.scale_length(0), 0);
    assert_eq!(Scale::default(), Scale::ONE);
}

#[test]
fn scale_length_scales_logical_to_physical() {
    let double = Scale::from_percent(200).expect("200% is in range");
    assert_eq!(double.scale_length(40), 80);
    let half = Scale::from_percent(50).expect("50% is in range");
    assert_eq!(half.scale_length(40), 20);
    let one_and_half = Scale::from_percent(150).expect("150% is in range");
    // Truncating division: 12 logical px at 150% is 18 physical px.
    assert_eq!(one_and_half.scale_length(12), 18);
}

#[test]
fn scale_length_saturates_rather_than_wrapping() {
    let big = Scale::from_percent(800).expect("800% is in range");
    assert_eq!(big.scale_length(u32::MAX), u32::MAX);
}

#[test]
fn out_of_range_percentages_are_rejected() {
    assert_eq!(Scale::from_percent(0), None);
    assert_eq!(Scale::from_percent(Scale::MIN_PERCENT - 1), None);
    assert_eq!(Scale::from_percent(Scale::MAX_PERCENT + 1), None);
    assert!(Scale::from_percent(Scale::MIN_PERCENT).is_some());
    assert!(Scale::from_percent(Scale::MAX_PERCENT).is_some());
}

#[test]
fn dpi_round_trips_through_the_reference_density() {
    let from_dpi = Scale::from_dpi(REFERENCE_DPI * 2).expect("192 DPI is in range");
    assert_eq!(from_dpi.percent(), 200);
    assert_eq!(from_dpi.dpi(), REFERENCE_DPI * 2);
    assert_eq!(Scale::from_dpi(REFERENCE_DPI), Some(Scale::ONE));
    // A density below the floor maps to a rejected percentage.
    assert_eq!(Scale::from_dpi(1), None);
}

#[test]
fn to_i32_carries_ordinary_extents_through() {
    assert_eq!(to_i32(0), 0);
    assert_eq!(to_i32(40), 40);
    assert_eq!(to_i32(u32::try_from(i32::MAX).expect("fits")), i32::MAX);
}

#[test]
fn to_i32_saturates_rather_than_wrapping() {
    assert_eq!(to_i32(u32::MAX), i32::MAX);
}
