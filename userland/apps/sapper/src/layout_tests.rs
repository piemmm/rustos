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

/// A display large enough that it never caps what a board asks for, so a test
/// about the board's own arithmetic is not measuring the screen.
fn roomy() -> Rect {
    Rect::new(0, 0, u32::MAX, u32::MAX)
}

/// The window `dims` asks for at `percent`, on a display that caps nothing.
fn asked_for(dims: Dimensions, percent: u32) -> WindowGeometry {
    WindowGeometry::resolve(dims, scale(percent), roomy())
}

/// The opening client size `dims` asks for at `percent`.
fn opens_at(dims: Dimensions, percent: u32) -> (u32, u32) {
    let asked = asked_for(dims, percent);
    (asked.width, asked.height)
}

#[test]
fn a_window_opens_large_enough_for_its_board() {
    for preset in Difficulty::PRESETS {
        let dims = preset.dimensions();
        let (width, height) = opens_at(dims, 100);
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
        let asked = asked_for(dims, 100);
        let (width, height) = (asked.sizing.min_width_px(), asked.sizing.min_height_px());
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
fn a_window_opens_inside_the_range_it_declares_on_every_display() {
    // The three sizes are one decision, so a board can never ask to open at
    // a size the window manager would refuse to let it be dragged to — and
    // that has to hold on a display too small for the board as well, where
    // the opening size is capped and the floor must follow it down.
    for preset in Difficulty::PRESETS {
        for percent in [75, 100, 150, 200] {
            for screen in [
                roomy(),
                Rect::new(0, 0, 640, 480),
                Rect::new(0, 0, 200, 120),
            ] {
                let asked = WindowGeometry::resolve(preset.dimensions(), scale(percent), screen);
                let floor = (asked.sizing.min_width_px(), asked.sizing.min_height_px());
                let ceiling = (asked.sizing.max_width_px(), asked.sizing.max_height_px());
                let what = preset.title();
                assert!(
                    floor.0 <= asked.width && floor.1 <= asked.height,
                    "{what} opens below its own floor at {percent}% on {screen:?}"
                );
                assert!(
                    asked.width <= ceiling.0 && asked.height <= ceiling.1,
                    "{what} opens above its own ceiling at {percent}% on {screen:?}"
                );
                assert!(
                    floor.0 <= ceiling.0 && floor.1 <= ceiling.1,
                    "{what} declares a ceiling under its floor at {percent}% on {screen:?}"
                );
            }
        }
    }
}

#[test]
fn the_ceiling_is_the_size_the_board_stops_growing_at() {
    // What the ceiling is *for*: at it the cell has reached its largest, so
    // every further pixel of window would be margin and nothing else.
    for preset in Difficulty::PRESETS {
        let dims = preset.dimensions();
        let asked = asked_for(dims, 100);
        let (width, height) = (asked.sizing.max_width_px(), asked.sizing.max_height_px());
        let ceiling = laid_out(width, height, dims, 100);
        assert_eq!(
            ceiling.cell,
            scale(100).scale_length(CELL_MAX),
            "{} does not reach its largest cell at its ceiling",
            preset.title()
        );
        assert!(
            ceiling.grid.width <= width && ceiling.grid.height <= height,
            "{} does not fit its own ceiling",
            preset.title()
        );
        // And a window past it gains the board nothing at all.
        let larger = laid_out(width * 2, height * 2, dims, 100);
        assert_eq!(larger.cell, ceiling.cell);
    }
}

#[test]
fn a_window_never_opens_larger_than_the_display() {
    // A board whose opening size exceeds the screen would put its own last
    // rows out of reach, so the opening size is capped. The *ceiling* is
    // not: where a user drags a window is theirs to decide.
    let dims = Difficulty::Expert.dimensions();
    let roomy = asked_for(dims, 100);
    let screen = Rect::new(0, 0, roomy.width / 2, roomy.height / 2);
    let cramped = WindowGeometry::resolve(dims, scale(100), screen);
    assert_eq!(
        (cramped.width, cramped.height),
        (screen.width, screen.height)
    );
    assert_eq!(
        (
            cramped.sizing.max_width_px(),
            cramped.sizing.max_height_px()
        ),
        (roomy.sizing.max_width_px(), roomy.sizing.max_height_px())
    );

    // A display that reports no extent at all is not a reason to ask for a
    // window with no pixels in it.
    let blind = WindowGeometry::resolve(dims, scale(100), Rect::new(0, 0, 0, 0));
    assert_eq!((blind.width, blind.height), (roomy.width, roomy.height));
}

#[test]
fn a_denser_display_gets_a_bigger_board_not_a_smaller_one() {
    let dims = beginner();
    let (single, _) = opens_at(dims, 100);
    let (double, _) = opens_at(dims, 200);
    assert!(double > single, "{double} should exceed {single}");
    let dense = laid_out(double, double, dims, 200);
    let plain = laid_out(single, single, dims, 100);
    assert!(dense.cell > plain.cell);
}

#[test]
fn a_bigger_window_grows_the_cells_then_centres_the_board() {
    let dims = beginner();
    let (width, height) = opens_at(dims, 100);
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
    let (width, height) = opens_at(dims, 100);
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
        let (width, height) = opens_at(dims, percent);
        let layout = laid_out(width, height, dims, percent);
        assert!(
            layout.grid.top() >= layout.header.bottom(),
            "scale {percent}%"
        );
    }
}
