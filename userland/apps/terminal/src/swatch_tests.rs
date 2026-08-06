//! Unit tests for the colour-well grid.

use tairix_font::BitmapFont;
use tairix_geometry::{Point, Rect, Scale};
use tairix_input::{InputEvent, Key, NamedKey, PointerButton};
use tairix_theme::Theme;

use crate::scheme::{ColorScheme, Scheme};
use crate::swatch::{SwatchAction, SwatchGrid, WELL_COUNT};

const W: u32 = 400;
const H: u32 = 200;

fn font() -> BitmapFont {
    BitmapFont::monospace(13)
}

fn bounds() -> Rect {
    Rect::new(0, 0, W, H)
}

fn moved(x: i32, y: i32) -> InputEvent {
    InputEvent::PointerMoved {
        to: Point::new(x, y),
    }
}

const PRESS: InputEvent = InputEvent::PointerPressed {
    button: PointerButton::Primary,
};
const RELEASE: InputEvent = InputEvent::PointerReleased {
    button: PointerButton::Primary,
};

fn scheme() -> ColorScheme {
    Scheme::Contrast.palette().expect("contrast has a palette")
}

// --- Round trip ----------------------------------------------------------

#[test]
fn from_scheme_then_apply_to_is_identity() {
    let original = scheme();
    let grid = SwatchGrid::from_scheme(&original);
    let mut round_tripped = ColorScheme::from_theme(&Theme::dark());
    grid.apply_to(&mut round_tripped);
    assert_eq!(round_tripped, original);
}

#[test]
fn from_scheme_orders_the_twenty_wells_as_documented() {
    let grid = SwatchGrid::from_scheme(&scheme());
    assert_eq!(grid.label(0), Some("Background"));
    assert_eq!(grid.label(1), Some("Foreground"));
    assert_eq!(grid.label(2), Some("Cursor"));
    assert_eq!(grid.label(3), Some("Cursor text"));
    assert_eq!(grid.label(4), Some("Black"));
    assert_eq!(grid.label(19), Some("Bright white"));
    assert_eq!(grid.label(WELL_COUNT), None);
}

#[test]
fn set_color_is_reflected_by_color_and_apply_to() {
    let mut grid = SwatchGrid::from_scheme(&scheme());
    let orange = crate::scheme::Rgb::new(0xff, 0x80, 0x00);
    grid.set_color(0, orange);
    assert_eq!(grid.color(0), Some(orange));
    let mut applied = scheme();
    grid.apply_to(&mut applied);
    assert_eq!(applied.background, orange);
}

// --- Selection ------------------------------------------------------------

#[test]
fn new_grid_selects_the_first_well() {
    let grid = SwatchGrid::from_scheme(&scheme());
    assert_eq!(grid.selected(), 0);
}

#[test]
fn set_selected_ignores_an_out_of_range_index() {
    let mut grid = SwatchGrid::from_scheme(&scheme());
    grid.set_selected(WELL_COUNT);
    assert_eq!(grid.selected(), 0);
    grid.set_selected(5);
    assert_eq!(grid.selected(), 5);
}

// --- Pointer selection -----------------------------------------------------

#[test]
fn pointer_press_and_release_over_the_same_well_selects_it() {
    let mut grid = SwatchGrid::from_scheme(&scheme());
    // Well 4 ("Black") is the first cell of the second row, in the grid's
    // second column band from the left within the test bounds.
    let point = well_centre(4);
    grid.on_pointer(&moved(point.x, point.y), bounds());
    assert_eq!(grid.on_pointer(&PRESS, bounds()), None);
    assert_eq!(
        grid.on_pointer(&RELEASE, bounds()),
        Some(SwatchAction::Selected { index: 4 })
    );
    assert_eq!(grid.selected(), 4);
}

#[test]
fn releasing_over_a_different_well_does_not_select() {
    let mut grid = SwatchGrid::from_scheme(&scheme());
    let press_point = well_centre(0);
    let release_point = well_centre(10);
    grid.on_pointer(&moved(press_point.x, press_point.y), bounds());
    grid.on_pointer(&PRESS, bounds());
    grid.on_pointer(&moved(release_point.x, release_point.y), bounds());
    assert_eq!(grid.on_pointer(&RELEASE, bounds()), None);
    assert_eq!(grid.selected(), 0);
}

#[test]
fn pointer_outside_every_well_selects_nothing() {
    let mut grid = SwatchGrid::from_scheme(&scheme());
    grid.on_pointer(&moved(-100, -100), bounds());
    grid.on_pointer(&PRESS, bounds());
    assert_eq!(grid.on_pointer(&RELEASE, bounds()), None);
}

/// The centre point of well `index`'s cell within [`bounds`], for driving
/// pointer tests without hard-coding the grid's own geometry twice.
fn well_centre(index: usize) -> Point {
    let columns = 5usize;
    let rows = 4u32;
    let col = u32::try_from(index % columns).expect("small index");
    let row = u32::try_from(index / columns).expect("small index");
    let cw = W / u32::try_from(columns).expect("small columns");
    let ch = H / rows;
    Point::new(
        i32::try_from(col * cw + cw / 2).expect("fits"),
        i32::try_from(row * ch + ch / 2).expect("fits"),
    )
}

// --- Keyboard navigation ---------------------------------------------------

#[test]
fn right_and_left_move_the_selection_by_one_well_and_wrap() {
    let mut grid = SwatchGrid::from_scheme(&scheme());
    assert_eq!(
        grid.on_key(Key::Named(NamedKey::Right)),
        Some(SwatchAction::Selected { index: 1 })
    );
    assert_eq!(
        grid.on_key(Key::Named(NamedKey::Left)),
        Some(SwatchAction::Selected { index: 0 })
    );
    // Left from the first well wraps to the last.
    assert_eq!(
        grid.on_key(Key::Named(NamedKey::Left)),
        Some(SwatchAction::Selected {
            index: WELL_COUNT - 1
        })
    );
    // Right from the last well wraps back to the first.
    assert_eq!(
        grid.on_key(Key::Named(NamedKey::Right)),
        Some(SwatchAction::Selected { index: 0 })
    );
}

#[test]
fn down_and_up_move_by_one_row_and_wrap_within_the_column() {
    let mut grid = SwatchGrid::from_scheme(&scheme());
    assert_eq!(
        grid.on_key(Key::Named(NamedKey::Down)),
        Some(SwatchAction::Selected { index: 5 })
    );
    assert_eq!(
        grid.on_key(Key::Named(NamedKey::Up)),
        Some(SwatchAction::Selected { index: 0 })
    );
    // Up from row 0 wraps to the last row of the same column.
    assert_eq!(
        grid.on_key(Key::Named(NamedKey::Up)),
        Some(SwatchAction::Selected { index: 15 })
    );
}

#[test]
fn an_unhandled_key_is_ignored() {
    let mut grid = SwatchGrid::from_scheme(&scheme());
    assert_eq!(grid.on_key(Key::Char('x')), None);
    assert_eq!(grid.selected(), 0);
}

// --- Rendering --------------------------------------------------------------

#[test]
fn renders_without_panicking_at_a_generous_and_a_tiny_size() {
    let theme = Theme::dark();
    let grid = SwatchGrid::from_scheme(&scheme());

    let mut big = tairix_raster::Surface::new(W, H).expect("surface");
    grid.render(&mut big, bounds(), Scale::ONE, &theme, font());

    let mut tiny = tairix_raster::Surface::new(24, 10).expect("surface");
    grid.render(
        &mut tiny,
        Rect::new(0, 0, 24, 10),
        Scale::ONE,
        &theme,
        font(),
    );
}

#[test]
fn the_selected_well_draws_a_shape_mark_distinct_from_an_unselected_well() {
    let theme = Theme::dark();
    let mut grid = SwatchGrid::from_scheme(&scheme());
    // Every well in the contrast scheme's ANSI slot 7 ("White") differs from
    // the accent ring/mark colours, so any extra pixel drawn over an
    // unselected well is exactly the selection mark.
    grid.set_selected(0);
    let mut unselected = tairix_raster::Surface::new(W, H).expect("surface");
    grid.render(&mut unselected, bounds(), Scale::ONE, &theme, font());

    grid.set_selected(3);
    let mut selected = tairix_raster::Surface::new(W, H).expect("surface");
    grid.render(&mut selected, bounds(), Scale::ONE, &theme, font());

    assert_ne!(
        unselected.pixels(),
        selected.pixels(),
        "moving the selection must repaint a visible mark"
    );
}

#[test]
fn preferred_height_is_stable_across_scale() {
    let theme = Theme::dark();
    let grid = SwatchGrid::from_scheme(&scheme());
    let one = grid.preferred_height(Scale::ONE, &theme);
    let two = grid.preferred_height(Scale::from_percent(200).expect("valid scale"), &theme);
    assert!(one > 0);
    assert!(two >= one);
}
