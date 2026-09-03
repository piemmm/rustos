//! Unit tests for an account's identity picture.

use tairix_font::BitmapFont;
use tairix_raster::{Color, Pixel};

use super::{monogram_disc, monogram_of, FALLBACK_MONOGRAM};

const FILL: Color = Color::rgb(60, 120, 220);
const INK: Color = Color::rgb(255, 255, 255);

/// The side every disc here is produced at, and the face marking it.
const SIDE: u32 = 48;
const MARK_HEIGHT: u32 = 20;

fn font() -> BitmapFont {
    BitmapFont::monospace(MARK_HEIGHT)
}

/// The opaque premultiplied form of a straight-alpha colour.
fn opaque(color: Color) -> Pixel {
    Pixel {
        r: color.r,
        g: color.g,
        b: color.b,
        a: u8::MAX,
    }
}

#[test]
fn a_monogram_is_the_first_character_uppercased() {
    assert_eq!(monogram_of("ann"), 'A');
    assert_eq!(monogram_of("Ann Smith"), 'A');
    assert_eq!(monogram_of("édith"), 'É');
}

#[test]
fn a_nameless_account_falls_back_rather_than_drawing_nothing() {
    assert_eq!(monogram_of(""), FALLBACK_MONOGRAM);
}

#[test]
fn a_multi_character_uppercase_form_contributes_its_first() {
    // 'ß' uppercases to "SS", and only one character is drawn.
    assert_eq!(monogram_of("ßeta"), 'S');
}

#[test]
fn a_disc_is_produced_at_exactly_the_side_asked_for() {
    let disc = monogram_disc('A', SIDE, font(), (FILL, INK)).expect("renderable");
    assert_eq!((disc.width(), disc.height()), (SIDE, SIDE));
}

#[test]
fn a_zero_side_disc_is_refused_rather_than_allocated() {
    assert!(monogram_disc('A', 0, font(), (FILL, INK)).is_none());
}

#[test]
fn a_disc_is_circular_so_its_corners_are_clear_and_its_edge_midpoints_are_not() {
    let disc = monogram_disc('A', SIDE, font(), (FILL, INK)).expect("renderable");
    for (x, y) in [(0, 0), (SIDE - 1, 0), (0, SIDE - 1), (SIDE - 1, SIDE - 1)] {
        assert_eq!(
            disc.get(x, y).expect("in bounds").a,
            0,
            "the corner at ({x}, {y}) lies outside the circle"
        );
    }
    for (x, y) in [
        (SIDE / 2, 0),
        (0, SIDE / 2),
        (SIDE / 2, SIDE - 1),
        (SIDE - 1, SIDE / 2),
    ] {
        assert_eq!(
            disc.get(x, y).expect("in bounds").a,
            u8::MAX,
            "the edge midpoint at ({x}, {y}) lies on the circle"
        );
    }
}

#[test]
fn the_mark_is_drawn_in_the_ink_over_the_fill() {
    let disc = monogram_disc('A', SIDE, font(), (FILL, INK)).expect("renderable");
    assert_eq!(
        disc.get(SIDE / 2, SIDE / 2).expect("in bounds"),
        opaque(INK),
        "the mark covers the middle of the disc"
    );
    assert_eq!(
        disc.get(1, SIDE / 2).expect("in bounds"),
        opaque(FILL),
        "the disc's leading edge is the fill the mark sits on"
    );
}

#[test]
fn a_blank_mark_still_produces_the_disc() {
    let disc = monogram_disc(' ', SIDE, font(), (FILL, INK)).expect("renderable");
    assert_eq!(
        disc.get(SIDE / 2, SIDE / 2).expect("in bounds"),
        opaque(FILL),
        "a mark with no ink leaves the disc itself"
    );
}
