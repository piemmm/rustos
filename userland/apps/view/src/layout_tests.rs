//! Host tests for the viewer's window geometry.
//!
//! No test hard-codes a coordinate: each asks the layout where a band is and
//! checks a property of it, so a change to the geometry moves the tests with
//! it rather than quietly making them assert about empty space.

use tairix_font::BitmapFont;
use tairix_geometry::{Rect, Scale};
use tairix_theme::{TextRole, Theme, ThemeRegistry};

use super::Layout;

/// The face every test resolves its geometry in.
fn font(theme: &Theme, scale: Scale) -> BitmapFont {
    BitmapFont::for_role(theme.fonts(), TextRole::Body, scale)
}

/// The layout of a `width`x`height` window at `scale`, with the information
/// panel in the state named.
fn layout(width: u32, height: u32, scale: Scale, info: bool) -> Layout {
    let registry = ThemeRegistry::with_builtins();
    let theme = registry.active();
    Layout::for_window(width, height, theme, scale, font(theme, scale), info)
}

/// An ordinary window with the panel closed.
fn plain() -> Layout {
    layout(900, 640, Scale::ONE, false)
}

/// Whether two rectangles share any pixel.
fn overlaps(a: Rect, b: Rect) -> bool {
    !a.intersection(&b).is_empty()
}

/// Every band the layout resolves, so a property can be asserted over all of
/// them without listing them at each call site.
fn bands(layout: &Layout) -> [(&'static str, Rect); 7] {
    [
        ("toolbar", layout.toolbar()),
        ("zoom slider", layout.zoom_slider()),
        ("canvas", layout.canvas()),
        ("vertical bar", layout.vertical_bar()),
        ("horizontal bar", layout.horizontal_bar()),
        ("info", layout.info()),
        ("status", layout.status()),
    ]
}

#[test]
fn every_band_stays_inside_the_window() {
    for scale in [Scale::ONE, Scale::from_percent(150).expect("a legal scale")] {
        for (width, height) in [(900, 640), (420, 320), (1920, 1080), (3840, 2160)] {
            let layout = layout(width, height, scale, true);
            let window = layout.window();
            assert_eq!(window, Rect::new(0, 0, width, height));
            for (name, band) in bands(&layout) {
                if band.is_empty() {
                    continue;
                }
                assert_eq!(
                    band.intersection(&window),
                    band,
                    "{name} escapes a {width}x{height} window at {}%",
                    scale.percent()
                );
            }
        }
    }
}

#[test]
fn the_canvas_and_the_bands_around_it_do_not_overlap() {
    let layout = layout(1_280, 800, Scale::ONE, true);
    let canvas = layout.canvas();
    assert!(!canvas.is_empty(), "an ordinary window has a canvas");
    for (name, band) in bands(&layout) {
        if name == "canvas" || band.is_empty() {
            continue;
        }
        assert!(
            !overlaps(canvas, band),
            "{name} {band:?} overlaps the canvas {canvas:?}"
        );
    }
}

#[test]
fn the_zoom_slider_sits_inside_the_toolbar_and_beyond_the_tools() {
    let layout = plain();
    let slider = layout.zoom_slider();
    assert!(!slider.is_empty());
    assert_eq!(slider.intersection(&layout.toolbar()), slider);
    assert!(
        !overlaps(slider, layout.tools()),
        "a click meant for a tool cannot land on the slider"
    );
    assert!(
        layout.tools().right() <= slider.left(),
        "the tools grow from the leading edge"
    );
}

#[test]
fn the_toolbar_and_status_line_survive_a_window_too_small_for_a_canvas() {
    // The property the claim order exists for: however small the window
    // becomes, the tools stay reachable and only the canvas gives up room.
    for height in [0, 1, 8, 24, 60] {
        let layout = layout(420, height, Scale::ONE, false);
        let toolbar = layout.toolbar();
        let status = layout.status();
        assert!(
            toolbar.height + status.height <= height.max(toolbar.height),
            "the two bands do not exceed a {height}-pixel window"
        );
        assert!(!overlaps(toolbar, status), "at {height} pixels");
    }
}

#[test]
fn a_closed_panel_takes_no_room_and_the_canvas_takes_it_instead() {
    let with = layout(1_280, 800, Scale::ONE, true);
    let without = layout(1_280, 800, Scale::ONE, false);
    assert!(!with.info().is_empty(), "an open info panel has room here");
    assert!(without.info().is_empty());
    assert!(
        without.canvas().width > with.canvas().width,
        "closing the panel widens the canvas"
    );
}

#[test]
fn a_panel_too_narrow_to_say_anything_is_not_drawn_at_all() {
    // A sliver of a panel would show a fact nobody could read, so the layout
    // declines it rather than drawing a useless strip.
    let narrow = layout(200, 640, Scale::ONE, true);
    assert!(narrow.info().is_empty(), "no room for a fact");
    assert!(
        !narrow.canvas().is_empty(),
        "the picture still has the window"
    );
}

#[test]
fn the_scrollbars_sit_on_the_canvas_trailing_edges() {
    let layout = plain();
    let canvas = layout.canvas();
    let vertical = layout.vertical_bar();
    let horizontal = layout.horizontal_bar();
    assert_eq!(vertical.left(), canvas.right(), "down the trailing edge");
    assert_eq!(horizontal.top(), canvas.bottom(), "along the bottom edge");
    assert_eq!(vertical.height, canvas.height);
    assert_eq!(horizontal.width, canvas.width);
}

#[test]
fn a_denser_scale_gives_the_chrome_more_pixels_and_the_canvas_fewer() {
    let one = layout(1_280, 800, Scale::ONE, false);
    let dense = layout(
        1_280,
        800,
        Scale::from_percent(200).expect("a legal scale"),
        false,
    );
    assert!(
        dense.toolbar().height > one.toolbar().height,
        "the toolbar is authored in logical pixels"
    );
    assert!(
        dense.canvas().height < one.canvas().height,
        "the canvas gives up what the chrome takes"
    );
}

#[test]
fn a_zero_sized_window_yields_empty_bands_rather_than_an_error() {
    let layout = layout(0, 0, Scale::ONE, true);
    for (name, band) in bands(&layout) {
        assert!(
            band.is_empty(),
            "{name} is empty in a window with no pixels"
        );
    }
    assert!(layout.window().is_empty());
}
