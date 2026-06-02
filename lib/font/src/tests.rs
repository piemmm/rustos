//! Unit tests for the bitmap font and glyph blitter.

use rustos_raster::{Color, Pixel, Surface};

use crate::font::BitmapFont;
use crate::glyphs::{FIRST_CHAR, GLYPHS, LAST_CHAR};

fn surface(w: u32, h: u32) -> Surface {
    Surface::new(w, h).expect("test surface allocates")
}

#[test]
fn atlas_covers_every_printable_ascii_code_point() {
    let expected = (LAST_CHAR as usize) - (FIRST_CHAR as usize) + 1;
    assert_eq!(GLYPHS.len(), expected);
    assert_eq!(FIRST_CHAR, ' ');
    assert_eq!(LAST_CHAR, '~');
}

#[test]
fn mono_face_reports_its_metrics() {
    let font = BitmapFont::mono5x7();
    assert_eq!(font.glyph_width(), 5);
    assert_eq!(font.glyph_height(), 7);
    assert_eq!(font.advance(), 6);
    assert_eq!(font.line_height(), 8);
}

#[test]
fn text_width_is_the_tight_bounding_width() {
    let font = BitmapFont::mono5x7();
    assert_eq!(font.text_width(""), 0);
    assert_eq!(font.text_width("A"), 5);
    assert_eq!(font.text_width("AB"), 11);
    assert_eq!(font.text_width("12:00"), 4 * 6 + 5);
}

#[test]
fn truncate_to_width_keeps_what_fits() {
    let font = BitmapFont::mono5x7();
    // One glyph needs `glyph_width` (5) pixels; each further glyph adds
    // `advance` (6). Less than one glyph fits nothing.
    assert_eq!(font.truncate_to_width("System", 4), "");
    assert_eq!(font.truncate_to_width("System", 5), "S");
    // 5 + 6 = 11 fits two glyphs; a third needs 5 + 2*6 = 17.
    assert_eq!(font.truncate_to_width("System", 11), "Sy");
    assert_eq!(font.truncate_to_width("System", 16), "Sy");
    assert_eq!(font.truncate_to_width("System", 17), "Sys");
    // A string that already fits is returned whole, untouched.
    assert_eq!(font.truncate_to_width("Sy", 1000), "Sy");
    assert_eq!(font.truncate_to_width("", 1000), "");
}

#[test]
fn truncate_to_width_cuts_on_a_char_boundary() {
    let font = BitmapFont::mono5x7();
    // Multi-byte chars must not be split mid-byte.
    let truncated = font.truncate_to_width("é€ß", 5);
    assert_eq!(truncated, "é");
}

#[test]
fn space_glyph_paints_nothing() {
    let font = BitmapFont::mono5x7();
    let mut surface = surface(8, 8);
    font.draw_text(&mut surface, 0, 0, " ", Color::rgb(255, 255, 255));
    assert!(surface.pixels().iter().all(|p| *p == Pixel::TRANSPARENT));
}

#[test]
fn draws_a_known_glyph_at_the_expected_pixels() {
    let font = BitmapFont::mono5x7();
    let mut surface = surface(8, 8);
    let red = Color::rgb(255, 0, 0);
    // '-' lights only row 3, all five columns.
    font.draw_text(&mut surface, 0, 0, "-", red);
    let lit = red.premultiply();
    for col in 0..5 {
        assert_eq!(surface.get(col, 3), Some(lit), "column {col} of the dash");
    }
    assert_eq!(surface.get(0, 0), Some(Pixel::TRANSPARENT));
    assert_eq!(surface.get(0, 6), Some(Pixel::TRANSPARENT));
}

#[test]
fn draw_text_returns_the_pen_after_the_last_glyph() {
    let font = BitmapFont::mono5x7();
    let mut surface = surface(64, 8);
    let pen = font.draw_text(&mut surface, 3, 0, "AB", Color::rgb(255, 255, 255));
    assert_eq!(pen, 3 + 2 * 6);
}

#[test]
fn unsupported_character_renders_the_fallback_box() {
    let font = BitmapFont::mono5x7();
    let mut surface = surface(8, 8);
    let white = Color::rgb(255, 255, 255);
    font.draw_text(&mut surface, 0, 0, "£", white);
    let lit = white.premultiply();
    // The fallback box has a fully lit top row.
    for col in 0..5 {
        assert_eq!(surface.get(col, 0), Some(lit), "fallback top row col {col}");
    }
    // ...and a hollow interior.
    assert_eq!(surface.get(2, 3), Some(Pixel::TRANSPARENT));
}

#[test]
fn off_screen_text_clips_without_panicking() {
    let font = BitmapFont::mono5x7();
    let mut surface = surface(8, 8);
    let white = Color::rgb(255, 255, 255);
    font.draw_text(&mut surface, -100, -100, "clipped", white);
    font.draw_text(&mut surface, 1000, 1000, "clipped", white);
    assert!(surface.pixels().iter().all(|p| *p == Pixel::TRANSPARENT));
}

#[test]
fn glyph_partially_off_the_left_edge_keeps_its_visible_columns() {
    let font = BitmapFont::mono5x7();
    let mut surface = surface(8, 8);
    let white = Color::rgb(255, 255, 255);
    // '-' starting at x = -2 keeps columns 2..5 (mapped to x 0..3).
    font.draw_text(&mut surface, -2, 0, "-", white);
    let lit = white.premultiply();
    assert_eq!(surface.get(0, 3), Some(lit));
    assert_eq!(surface.get(2, 3), Some(lit));
    assert_eq!(surface.get(3, 3), Some(Pixel::TRANSPARENT));
}

#[test]
fn translucent_text_composites_over_the_background() {
    let font = BitmapFont::mono5x7();
    let mut surface = surface(8, 8);
    surface.fill(Color::rgb(0, 0, 255));
    // Semi-transparent red dash over opaque blue.
    font.draw_text(&mut surface, 0, 0, "-", Color::rgba(255, 0, 0, 128));
    // Porter–Duff over with premultiplied operands (see lib/raster::Pixel).
    assert_eq!(
        surface.get(0, 3),
        Some(Pixel {
            r: 128,
            g: 0,
            b: 127,
            a: 255,
        })
    );
    // A row the dash does not touch keeps the opaque-blue background.
    assert_eq!(surface.get(0, 0), Some(Color::rgb(0, 0, 255).premultiply()));
}
