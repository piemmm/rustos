//! Unit tests for the terminal's screen grid and window sizing.

use tairix_font::BitmapFont;
use tairix_geometry::Scale;
use tairix_theme::Theme;

use crate::grid::MAX_DIMENSION;
use crate::profile::{DEFAULT_FONT_SIZE_PX, MIN_FONT_SIZE_PX};

use super::{chrome_extent, fit_font_size, grid_dims, grid_size, window_size, COLS, ROWS};

/// The font a terminal opens with at the profile's default text size, on an
/// unscaled (100%) display.
fn default_font() -> BitmapFont {
    BitmapFont::monospace(Scale::ONE.scale_length(u32::from(DEFAULT_FONT_SIZE_PX)))
}

// --- The headline requirement: the conventional screen fits a small display -

#[test]
fn the_default_80x25_screen_fits_inside_a_640x480_display() {
    let theme = Theme::dark();
    let scale = Scale::ONE;
    let screen = (640, 480);
    let font = default_font();

    let (client_w, client_h) = window_size(font, screen, &theme, scale);
    let (chrome_w, chrome_h) = chrome_extent(&theme, scale, true);
    let outer_w = client_w + chrome_w;
    let outer_h = client_h + chrome_h;

    assert!(
        outer_w <= screen.0,
        "outer window width {outer_w} exceeds the {}-pixel-wide screen",
        screen.0
    );
    assert!(
        outer_h <= screen.1,
        "outer window height {outer_h} exceeds the {}-pixel-tall screen",
        screen.1
    );

    // The client viewport really measures out to the conventional grid.
    assert_eq!(grid_dims(client_w, client_h, font), (COLS, ROWS));

    // The window must not take the screen over: it leaves comfortable room,
    // in particular enough for a taskbar along one edge.
    assert!(
        screen.1 - outer_h >= 40 || screen.0 - outer_w >= 40,
        "the window leaves no comfortable margin on a 640x480 screen"
    );
}

// --- fit_font_size -----------------------------------------------------------

#[test]
fn fit_font_size_keeps_the_preferred_size_on_a_large_screen() {
    let theme = Theme::dark();
    let scale = Scale::ONE;
    let preferred = DEFAULT_FONT_SIZE_PX;
    let fitted = fit_font_size(preferred, (1920, 1080), &theme, scale);
    assert_eq!(fitted, preferred);
}

#[test]
fn fit_font_size_steps_down_on_a_small_screen() {
    let theme = Theme::dark();
    let scale = Scale::ONE;
    let preferred = 40;
    let fitted = fit_font_size(preferred, (320, 240), &theme, scale);
    assert!(fitted < preferred);
    assert!(fitted >= MIN_FONT_SIZE_PX);
}

#[test]
fn fit_font_size_never_returns_below_the_minimum() {
    let theme = Theme::dark();
    let scale = Scale::ONE;
    // A screen far too small for even the smallest legible face still
    // returns a usable size rather than zero or an unbounded search.
    let fitted = fit_font_size(40, (1, 1), &theme, scale);
    assert_eq!(fitted, MIN_FONT_SIZE_PX);
}

// --- grid_size / grid_dims ----------------------------------------------------

#[test]
fn grid_size_and_grid_dims_are_inverse_consistent() {
    let font = default_font();
    let (width_px, height_px) = grid_size(COLS, ROWS, font);
    assert_eq!(grid_dims(width_px, height_px, font), (COLS, ROWS));
}

#[test]
fn grid_dims_clamps_to_at_least_one_by_one() {
    let font = default_font();
    assert_eq!(grid_dims(0, 0, font), (1, 1));
}

#[test]
fn grid_dims_clamps_to_at_most_the_maximum_dimension() {
    let font = default_font();
    let huge = u32::from(MAX_DIMENSION) * font.cell_width().max(1) * 4;
    let (cols, rows) = grid_dims(huge, huge, font);
    assert_eq!(cols, MAX_DIMENSION);
    assert_eq!(rows, MAX_DIMENSION);
}

// --- chrome_extent -------------------------------------------------------------

#[test]
fn chrome_extent_is_larger_for_a_resizable_window_than_a_fixed_one() {
    let theme = Theme::dark();
    let scale = Scale::ONE;
    let (resizable_w, resizable_h) = chrome_extent(&theme, scale, true);
    let (fixed_w, fixed_h) = chrome_extent(&theme, scale, false);
    assert!(resizable_w >= fixed_w);
    assert!(resizable_h >= fixed_h);
    assert!(resizable_w > fixed_w || resizable_h > fixed_h);
}

#[test]
fn chrome_extent_scales_with_the_desktop_scale() {
    let theme = Theme::dark();
    let (base_w, base_h) = chrome_extent(&theme, Scale::ONE, true);
    let doubled = Scale::from_percent(200).expect("200% is a valid scale");
    let (scaled_w, scaled_h) = chrome_extent(&theme, doubled, true);
    assert!(scaled_w > base_w);
    assert!(scaled_h > base_h);
}
