//! Unit tests for the integer geometry primitives.

use super::{Point, Rect};

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
