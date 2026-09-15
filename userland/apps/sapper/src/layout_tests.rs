//! Layout arithmetic tests, at more than one scale.

use super::*;

use crate::board::{Difficulty, MAX_SIDE, MIN_SIDE};

fn scale(percent: u32) -> Scale {
    Scale::from_percent(percent).expect("a scale the desktop allows")
}

fn beginner() -> Dimensions {
    Difficulty::Beginner.dimensions()
}

fn laid_out(width: u32, height: u32, dims: Dimensions, percent: u32) -> Layout {
    Layout::resolve(Rect::new(0, 0, width, height), dims, scale(percent))
}

#[test]
fn a_window_opens_large_enough_for_its_board() {
    for preset in Difficulty::PRESETS {
        let dims = preset.dimensions();
        let (width, height) = Layout::preferred(dims, scale(100));
        let layout = laid_out(width, height, dims, 100);
        assert!(
            layout.grid.width <= width && layout.grid.height <= height,
            "{} does not fit its own preferred window",
            preset.title()
        );
        assert!(layout.grid.top() >= layout.header.bottom());
    }
}

#[test]
fn the_resize_floor_still_fits_the_board() {
    for preset in Difficulty::PRESETS {
        let dims = preset.dimensions();
        let (width, height) = Layout::minimum(dims, scale(100));
        let layout = laid_out(width, height, dims, 100);
        assert!(
            layout.grid.width <= width && layout.grid.height <= height,
            "{} does not fit its own floor",
            preset.title()
        );
        assert!(layout.cell >= 1);
    }
}

#[test]
fn the_floor_is_never_larger_than_the_opening_size() {
    for preset in Difficulty::PRESETS {
        let dims = preset.dimensions();
        let (min_w, min_h) = Layout::minimum(dims, scale(100));
        let (pref_w, pref_h) = Layout::preferred(dims, scale(100));
        assert!(min_w <= pref_w && min_h <= pref_h, "{}", preset.title());
    }
}

#[test]
fn a_denser_display_gets_a_bigger_board_not_a_smaller_one() {
    let dims = beginner();
    let (single, _) = Layout::preferred(dims, scale(100));
    let (double, _) = Layout::preferred(dims, scale(200));
    assert!(double > single, "{double} should exceed {single}");
    let dense = laid_out(double, double, dims, 200);
    let plain = laid_out(single, single, dims, 100);
    assert!(dense.cell > plain.cell);
}

#[test]
fn a_bigger_window_grows_the_cells_then_centres_the_board() {
    let dims = beginner();
    let (width, height) = Layout::preferred(dims, scale(100));
    let small = laid_out(width, height, dims, 100);
    let large = laid_out(width * 2, height * 2, dims, 100);
    assert!(large.cell > small.cell, "the board grows with the window");

    // Past the ceiling the cells stop growing and the slack becomes margin.
    let huge = laid_out(width * 20, height * 20, dims, 100);
    assert_eq!(huge.cell, scale(100).scale_length(CELL_MAX));
    let left_slack = huge.grid.left();
    let right_slack = i32::try_from(width * 20).expect("fits") - huge.grid.right();
    assert!(
        (left_slack - right_slack).abs() <= 1,
        "left {left_slack} right {right_slack}"
    );
}

#[test]
fn a_cell_never_shrinks_below_the_legible_floor() {
    // A window far smaller than the board's floor: the cell clamps rather than
    // collapsing to nothing, so the grid is still drawn.
    let layout = laid_out(40, 40, Difficulty::Expert.dimensions(), 100);
    assert_eq!(layout.cell, scale(100).scale_length(CELL_MIN));
    assert!(layout.cell >= 1);
}

#[test]
fn a_degenerate_window_still_answers() {
    for dims in [
        beginner(),
        Dimensions::new(MIN_SIDE, MIN_SIDE, 1).expect("legal"),
        Dimensions::new(MAX_SIDE, MAX_SIDE, 1).expect("legal"),
    ] {
        for (width, height) in [(0, 0), (1, 1), (0, 500), (500, 0)] {
            let layout = laid_out(width, height, dims, 100);
            assert!(layout.cell >= 1);
            assert!(layout.header.height <= height);
        }
    }
}

// --- Cells --------------------------------------------------------------

#[test]
fn every_cell_is_inside_the_grid_and_none_overlap() {
    let dims = beginner();
    let (width, height) = Layout::preferred(dims, scale(100));
    let layout = laid_out(width, height, dims, 100);
    let mut previous_right = layout.grid.left();
    for col in 0..dims.cols() {
        let rect = layout.cell_rect(Coord::new(col, 0));
        assert!(rect.left() >= previous_right, "column {col} overlaps");
        assert!(
            rect.right() <= layout.grid.right(),
            "column {col} overflows"
        );
        previous_right = rect.right();
    }
    let last = layout.cell_rect(Coord::new(dims.cols() - 1, dims.rows() - 1));
    assert_eq!(last.right(), layout.grid.right());
    assert_eq!(last.bottom(), layout.grid.bottom());
}

#[test]
fn an_off_board_cell_draws_nothing() {
    let dims = beginner();
    let layout = laid_out(400, 400, dims, 100);
    assert!(layout.cell_rect(Coord::new(dims.cols(), 0)).is_empty());
    assert!(layout.cell_rect(Coord::new(0, dims.rows())).is_empty());
    assert!(layout.cell_damage(Coord::new(99, 99)).is_empty());
}

#[test]
fn a_cells_damage_covers_it_and_the_gutter_around_it() {
    let layout = laid_out(400, 400, beginner(), 100);
    let at = Coord::new(3, 3);
    let rect = layout.cell_rect(at);
    let damage = layout.cell_damage(at);
    assert!(damage.left() < rect.left() && damage.top() < rect.top());
    assert!(damage.right() > rect.right() && damage.bottom() > rect.bottom());
}

// --- Hit testing --------------------------------------------------------

#[test]
fn a_click_finds_the_cell_it_landed_on() {
    let dims = beginner();
    let layout = laid_out(400, 460, dims, 100);
    for row in 0..dims.rows() {
        for col in 0..dims.cols() {
            let at = Coord::new(col, row);
            let rect = layout.cell_rect(at);
            for probe in [
                rect.center(),
                Point::new(rect.left(), rect.top()),
                Point::new(rect.right() - 1, rect.bottom() - 1),
            ] {
                assert_eq!(layout.cell_at(probe), Some(at), "{at:?} at {probe:?}");
            }
        }
    }
}

#[test]
fn a_click_in_the_gutter_belongs_to_the_cell_before_it() {
    let layout = laid_out(400, 460, beginner(), 100);
    assert!(layout.gap >= 1, "this test needs a visible gutter");
    let first = layout.cell_rect(Coord::new(0, 0));
    let gutter = Point::new(first.right(), first.center().y);
    assert_eq!(layout.cell_at(gutter), Some(Coord::new(0, 0)));
}

#[test]
fn a_click_outside_the_grid_finds_nothing() {
    let dims = beginner();
    let layout = laid_out(400, 460, dims, 100);
    for probe in [
        Point::new(-1, -1),
        Point::new(layout.grid.left() - 1, layout.grid.center().y),
        Point::new(layout.grid.right(), layout.grid.center().y),
        Point::new(layout.grid.center().x, layout.grid.top() - 1),
        Point::new(layout.grid.center().x, layout.grid.bottom()),
        layout.face.center(),
    ] {
        assert_eq!(layout.cell_at(probe), None, "{probe:?}");
    }
}

#[test]
fn every_point_in_the_grid_resolves_to_a_cell() {
    let dims = Dimensions::new(MIN_SIDE, MIN_SIDE, 1).expect("legal");
    let layout = laid_out(300, 340, dims, 100);
    for y in layout.grid.top()..layout.grid.bottom() {
        for x in layout.grid.left()..layout.grid.right() {
            let found = layout.cell_at(Point::new(x, y));
            assert!(found.is_some(), "({x},{y}) resolved to nothing");
            let at = found.expect("checked just above");
            assert!(at.col < dims.cols() && at.row < dims.rows());
        }
    }
}

// --- The header ---------------------------------------------------------

#[test]
fn the_header_reads_counter_then_button_then_clock() {
    let layout = laid_out(500, 520, beginner(), 100);
    assert!(layout.counter.right() <= layout.face.left());
    assert!(layout.face.right() <= layout.clock.left());
    assert!(layout.clock.right() <= layout.header.right());
    assert!(layout.counter.left() >= layout.header.left());
    for part in [layout.counter, layout.face, layout.clock] {
        assert!(part.top() >= layout.header.top());
        assert!(part.bottom() <= layout.header.bottom());
    }
}

#[test]
fn the_header_never_overlaps_the_grid() {
    for percent in [75, 100, 150, 200] {
        let dims = beginner();
        let (width, height) = Layout::preferred(dims, scale(percent));
        let layout = laid_out(width, height, dims, percent);
        assert!(
            layout.grid.top() >= layout.header.bottom(),
            "scale {percent}%"
        );
    }
}
