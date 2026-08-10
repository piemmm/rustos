//! Unit tests for [`Region`], including a differential sweep against a naive
//! pixel-grid model: the canonical band form is worth nothing unless it covers
//! exactly the pixels a set-of-rectangles model says it should.

use alloc::vec;
use alloc::vec::Vec;

use super::{Point, Rect, Region};

/// A region as a plain grid of covered pixels over `[0, SIDE) x [0, SIDE)`,
/// which is what the banded form is checked against.
const SIDE: i32 = 24;

#[derive(Clone, Eq, PartialEq)]
struct Grid {
    covered: Vec<bool>,
}

impl core::fmt::Debug for Grid {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        for row in self.covered.chunks(usize_of(SIDE)) {
            writeln!(f)?;
            for cell in row {
                f.write_str(if *cell { "#" } else { "." })?;
            }
        }
        Ok(())
    }
}

impl Grid {
    fn new() -> Self {
        Self {
            covered: vec![false; usize_of(SIDE * SIDE)],
        }
    }

    fn from_region(region: &Region) -> Self {
        let mut grid = Self::new();
        for rect in region.rects() {
            grid.paint(*rect, true);
        }
        grid
    }

    fn paint(&mut self, rect: Rect, value: bool) {
        for y in rect.top().max(0)..rect.bottom().min(SIDE) {
            for x in rect.left().max(0)..rect.right().min(SIDE) {
                if let Some(cell) = self.covered.get_mut(usize_of(y * SIDE + x)) {
                    *cell = value;
                }
            }
        }
    }

    fn keep_only(&mut self, rect: Rect) {
        let kept = {
            let mut only = Self::new();
            only.paint(rect, true);
            only
        };
        for (cell, keep) in self.covered.iter_mut().zip(kept.covered.iter()) {
            *cell &= *keep;
        }
    }

    fn count(&self) -> usize {
        self.covered.iter().filter(|c| **c).count()
    }
}

fn usize_of(v: i32) -> usize {
    usize::try_from(v).unwrap_or(0)
}

/// Assert the region's rectangles are canonical: non-empty, band-ordered,
/// pairwise disjoint, and never adjoining within a band.
fn assert_canonical(region: &Region) {
    let rects = region.rects();
    for rect in rects {
        assert!(
            !rect.is_empty(),
            "canonical regions hold no empty rectangle"
        );
    }
    for pair in rects.windows(2) {
        let [a, b] = pair else { continue };
        if a.top() == b.top() {
            assert_eq!(a.bottom(), b.bottom(), "a band's rectangles share rows");
            assert!(
                a.right() < b.left(),
                "spans in a band never adjoin: {a:?} {b:?}"
            );
        } else {
            assert!(a.top() < b.top(), "bands run top to bottom: {a:?} {b:?}");
            assert!(a.bottom() <= b.top(), "bands never overlap: {a:?} {b:?}");
        }
    }
    for (i, a) in rects.iter().enumerate() {
        for b in rects.iter().skip(i + 1) {
            assert!(
                a.intersection(b).is_empty(),
                "rectangles are pairwise disjoint: {a:?} {b:?}"
            );
        }
    }
    let bounds = rects.iter().fold(Rect::EMPTY, |acc, r| acc.union(r));
    assert_eq!(region.bounds(), bounds, "bounds track the rectangles");
}

/// Assert the region covers exactly the pixels `grid` marks.
fn assert_matches(region: &Region, grid: &Grid) {
    assert_canonical(region);
    assert_eq!(&Grid::from_region(region), grid, "region {region:?}");
}

#[test]
fn a_new_region_is_empty() {
    let region = Region::new();
    assert!(region.is_empty());
    assert!(region.rects().is_empty());
    assert_eq!(region.bounds(), Rect::EMPTY);
    assert_eq!(region.budget(), None);
}

#[test]
fn adding_an_empty_rect_changes_nothing() {
    let mut region = Region::new();
    region.add(Rect::new(4, 4, 0, 9));
    region.add(Rect::new(4, 4, 9, 0));
    assert!(region.is_empty());
    region.add(Rect::new(0, 0, 4, 4));
    region.add(Rect::EMPTY);
    assert_eq!(region.rects(), &[Rect::new(0, 0, 4, 4)]);
}

#[test]
fn adding_the_same_rect_twice_keeps_one() {
    let mut region = Region::new();
    region.add(Rect::new(2, 3, 5, 6));
    region.add(Rect::new(2, 3, 5, 6));
    assert_eq!(region.rects(), &[Rect::new(2, 3, 5, 6)]);
}

#[test]
fn disjoint_rects_stay_separate() {
    let mut region = Region::new();
    region.add(Rect::new(0, 0, 2, 2));
    region.add(Rect::new(10, 10, 2, 2));
    assert_eq!(region.rects().len(), 2);
    assert_eq!(region.bounds(), Rect::new(0, 0, 12, 12));
    assert_canonical(&region);
}

#[test]
fn stacked_rects_of_equal_span_become_one_band() {
    let mut region = Region::new();
    region.add(Rect::new(0, 0, 10, 10));
    region.add(Rect::new(0, 10, 10, 10));
    assert_eq!(region.rects(), &[Rect::new(0, 0, 10, 20)]);
}

#[test]
fn touching_spans_in_a_band_coalesce() {
    let mut region = Region::new();
    region.add(Rect::new(0, 0, 5, 4));
    region.add(Rect::new(5, 0, 5, 4));
    assert_eq!(region.rects(), &[Rect::new(0, 0, 10, 4)]);
}

#[test]
fn a_gap_of_one_pixel_keeps_two_spans() {
    let mut region = Region::new();
    region.add(Rect::new(0, 0, 5, 4));
    region.add(Rect::new(6, 0, 5, 4));
    assert_eq!(
        region.rects(),
        &[Rect::new(0, 0, 5, 4), Rect::new(6, 0, 5, 4)]
    );
}

#[test]
fn overlapping_rects_are_split_not_unioned() {
    let mut region = Region::new();
    region.add(Rect::new(0, 0, 10, 10));
    region.add(Rect::new(5, 5, 10, 10));
    let mut grid = Grid::new();
    grid.paint(Rect::new(0, 0, 10, 10), true);
    grid.paint(Rect::new(5, 5, 10, 10), true);
    assert_matches(&region, &grid);
    // The bounding box would have been 15x15 = 225 pixels; the region covers
    // only what was actually added.
    assert_eq!(grid.count(), 175);
}

#[test]
fn add_is_order_independent() {
    let rects = [
        Rect::new(1, 1, 6, 6),
        Rect::new(4, 0, 3, 12),
        Rect::new(0, 5, 12, 2),
        Rect::new(9, 9, 4, 4),
    ];
    let mut forward = Region::new();
    for rect in rects {
        forward.add(rect);
    }
    let mut backward = Region::new();
    for rect in rects.iter().rev() {
        backward.add(*rect);
    }
    assert_eq!(forward, backward);
    assert_eq!(forward.rects(), backward.rects());
}

#[test]
fn subtract_removes_exactly_the_rect() {
    let mut region = Region::from(Rect::new(0, 0, 10, 10));
    region.subtract(Rect::new(4, 4, 2, 2));
    let mut grid = Grid::new();
    grid.paint(Rect::new(0, 0, 10, 10), true);
    grid.paint(Rect::new(4, 4, 2, 2), false);
    assert_matches(&region, &grid);
    assert_eq!(grid.count(), 96);
}

#[test]
fn subtracting_the_whole_region_empties_it() {
    let mut region = Region::new();
    region.add(Rect::new(0, 0, 4, 4));
    region.add(Rect::new(8, 8, 4, 4));
    region.subtract(Rect::new(0, 0, 20, 20));
    assert!(region.is_empty());
    assert_eq!(region.bounds(), Rect::EMPTY);
}

#[test]
fn subtracting_a_disjoint_rect_changes_nothing() {
    let mut region = Region::from(Rect::new(0, 0, 4, 4));
    let before = region.clone();
    region.subtract(Rect::new(20, 20, 4, 4));
    region.subtract(Rect::EMPTY);
    assert_eq!(region, before);
}

#[test]
fn subtract_can_split_a_rect_into_two_spans() {
    let mut region = Region::from(Rect::new(0, 0, 10, 4));
    region.subtract(Rect::new(4, 0, 2, 4));
    assert_eq!(
        region.rects(),
        &[Rect::new(0, 0, 4, 4), Rect::new(6, 0, 4, 4)]
    );
}

#[test]
fn clip_keeps_only_the_overlap() {
    let mut region = Region::new();
    region.add(Rect::new(0, 0, 10, 10));
    region.add(Rect::new(20, 20, 10, 10));
    region.clip(Rect::new(5, 5, 10, 10));
    assert_eq!(region.rects(), &[Rect::new(5, 5, 5, 5)]);
}

#[test]
fn clipping_to_an_empty_rect_empties_the_region() {
    let mut region = Region::from(Rect::new(0, 0, 10, 10));
    region.clip(Rect::EMPTY);
    assert!(region.is_empty());
}

#[test]
fn translate_shifts_every_rect() {
    let mut region = Region::new();
    region.add(Rect::new(0, 0, 4, 4));
    region.add(Rect::new(10, 10, 4, 4));
    region.translate(3, -2);
    assert_eq!(
        region.rects(),
        &[Rect::new(3, -2, 4, 4), Rect::new(13, 8, 4, 4)]
    );
    assert_eq!(region.bounds(), Rect::new(3, -2, 14, 14));
}

#[test]
fn translate_by_zero_is_a_no_op() {
    let mut region = Region::from(Rect::new(1, 2, 3, 4));
    let before = region.clone();
    region.translate(0, 0);
    assert_eq!(region, before);
    assert_eq!(region.bounds(), before.bounds());
}

#[test]
fn translate_past_the_coordinate_range_collapses_to_the_clamped_bounds() {
    let mut region = Region::new();
    region.add(Rect::new(0, 0, 4, 4));
    region.add(Rect::new(10, 10, 4, 4));
    region.translate(i32::MAX, 0);
    assert_eq!(region.rects().len(), 1);
    assert_eq!(region.bounds(), Rect::new(i32::MAX, 0, 14, 14));
    assert_canonical(&region);
}

#[test]
fn contains_answers_the_point_query() {
    let mut region = Region::new();
    region.add(Rect::new(0, 0, 4, 4));
    region.add(Rect::new(10, 10, 4, 4));
    assert!(region.contains(Point::new(0, 0)));
    assert!(region.contains(Point::new(3, 3)));
    assert!(!region.contains(Point::new(4, 4)));
    assert!(region.contains(Point::new(13, 13)));
    assert!(!region.contains(Point::new(5, 5)));
    assert!(!Region::new().contains(Point::ORIGIN));
}

#[test]
fn intersects_answers_the_rect_query() {
    let mut region = Region::new();
    region.add(Rect::new(0, 0, 4, 4));
    region.add(Rect::new(10, 10, 4, 4));
    assert!(region.intersects(Rect::new(3, 3, 4, 4)));
    // Inside the bounding box, but in neither rectangle.
    assert!(!region.intersects(Rect::new(5, 5, 4, 4)));
    assert!(!region.intersects(Rect::new(100, 100, 4, 4)));
    assert!(!region.intersects(Rect::EMPTY));
}

#[test]
fn clear_forgets_everything() {
    let mut region = Region::from(Rect::new(0, 0, 4, 4));
    region.clear();
    assert!(region.is_empty());
    assert_eq!(region.bounds(), Rect::EMPTY);
    region.add(Rect::new(1, 1, 2, 2));
    assert_eq!(region.rects(), &[Rect::new(1, 1, 2, 2)]);
}

#[test]
fn a_budget_degrades_to_the_bounding_box() {
    let mut region = Region::with_budget(2);
    assert_eq!(region.budget(), Some(2));
    region.add(Rect::new(0, 0, 2, 2));
    region.add(Rect::new(10, 0, 2, 2));
    assert_eq!(region.rects().len(), 2);
    region.add(Rect::new(20, 20, 2, 2));
    assert_eq!(region.rects(), &[Rect::new(0, 0, 22, 22)]);
    assert_canonical(&region);
}

#[test]
fn a_zero_budget_still_keeps_the_bounding_box() {
    let mut region = Region::with_budget(0);
    assert_eq!(region.budget(), Some(1));
    region.add(Rect::new(0, 0, 2, 2));
    region.add(Rect::new(10, 10, 2, 2));
    assert_eq!(region.rects(), &[Rect::new(0, 0, 12, 12)]);
}

#[test]
fn a_budgeted_region_never_loses_a_covered_pixel() {
    let mut exact = Region::new();
    let mut bounded = Region::with_budget(3);
    for rect in [
        Rect::new(0, 0, 2, 2),
        Rect::new(6, 1, 3, 9),
        Rect::new(2, 8, 5, 2),
        Rect::new(11, 11, 4, 4),
    ] {
        exact.add(rect);
        bounded.add(rect);
    }
    let (fine, coarse) = (Grid::from_region(&exact), Grid::from_region(&bounded));
    for (index, covered) in fine.covered.iter().enumerate() {
        assert!(
            !*covered || coarse.covered.get(index).copied().unwrap_or(false),
            "degrading may over-cover, never under-cover"
        );
    }
}

#[test]
fn clone_carries_the_budget_and_the_pixels() {
    let mut region = Region::with_budget(4);
    region.add(Rect::new(1, 1, 3, 3));
    let copy = region.clone();
    assert_eq!(copy.budget(), Some(4));
    assert_eq!(copy.rects(), region.rects());
    assert_eq!(copy.bounds(), region.bounds());
}

#[test]
fn equality_is_the_covered_set_not_the_budget() {
    let mut exact = Region::new();
    let mut bounded = Region::with_budget(8);
    exact.add(Rect::new(0, 0, 4, 4));
    bounded.add(Rect::new(0, 0, 4, 4));
    assert_eq!(exact, bounded);
}

#[test]
fn a_rect_converts_into_a_one_rect_region() {
    let region = Region::from(Rect::new(2, 3, 4, 5));
    assert_eq!(region.rects(), &[Rect::new(2, 3, 4, 5)]);
    assert!(Region::from(Rect::EMPTY).is_empty());
}

#[test]
fn saturating_edges_do_not_break_the_canonical_form() {
    let mut region = Region::new();
    region.add(Rect::new(i32::MAX - 4, i32::MAX - 4, 100, 100));
    region.add(Rect::new(i32::MAX - 2, i32::MAX - 2, 100, 100));
    assert_canonical(&region);
    region.subtract(Rect::new(i32::MAX - 3, i32::MAX - 3, 1, 1));
    assert_canonical(&region);
}

/// Random rectangles, each operation applied to both the region and a plain
/// pixel grid, comparing after every step. This is the real proof the banded
/// form is a faithful set.
#[test]
fn differential_sweep_against_a_pixel_grid() {
    let mut state = 0x1234_5678_9abc_def0u64;
    let mut next = move || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };
    for _ in 0..400 {
        let mut region = Region::new();
        let mut grid = Grid::new();
        for _ in 0..12 {
            let draw = next();
            let x = i32::try_from(draw % 20).unwrap_or(0);
            let y = i32::try_from((draw >> 8) % 20).unwrap_or(0);
            let w = u32::try_from((draw >> 16) % 8).unwrap_or(0);
            let h = u32::try_from((draw >> 24) % 8).unwrap_or(0);
            let rect = Rect::new(x, y, w, h);
            match (draw >> 32) % 3 {
                0 => {
                    region.add(rect);
                    grid.paint(rect, true);
                }
                1 => {
                    region.subtract(rect);
                    grid.paint(rect, false);
                }
                _ => {
                    region.clip(rect);
                    grid.keep_only(rect);
                }
            }
            assert_matches(&region, &grid);
        }
    }
}
