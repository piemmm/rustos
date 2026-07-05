//! Unit tests: atlas integrity, Unicode lookup, and the glyph blitter.

use crate::atlas;
use crate::glyph::{lookup, lookup_or_fallback, Glyph};

#[test]
fn atlas_payload_matches_its_declared_shape() {
    assert_eq!(
        atlas::COVERAGE.len(),
        atlas::CELL_COUNT as usize * atlas::BYTES_PER_CELL,
        "payload length disagrees with the declared cell count"
    );
    assert_eq!(
        atlas::COVERAGE.len()
            % ((atlas::CELL_WIDTH as usize).div_ceil(2) * atlas::CELL_HEIGHT as usize),
        0,
        "payload is not whole packed cells"
    );
    assert!(
        lookup('\u{FFFD}').is_some(),
        "the declared fallback cell must be a real mapped glyph"
    );
}

#[test]
fn ranges_are_sorted_dense_and_in_cell_order() {
    let mut previous_end = 0u32;
    let mut expected_base = 0u32;
    for &(first, len, base) in atlas::RANGES {
        assert!(len > 0);
        assert!(first >= previous_end, "ranges overlap or are unsorted");
        assert_eq!(base, expected_base, "cells are not in range order");
        previous_end = first + len;
        expected_base += len;
    }
    assert_eq!(expected_base, atlas::CELL_COUNT);
}

#[test]
fn printable_ascii_is_covered() {
    for code in 0x20..=0x7Eu32 {
        let ch = char::from_u32(code).expect("printable ASCII");
        assert!(lookup(ch).is_some(), "U+{code:04X} has no glyph");
    }
}

#[test]
fn coverage_reaches_beyond_ascii() {
    for ch in ['é', 'ß', 'ő', '─', '┌', '█', '▒', '→', '…', '€', '\u{0301}'] {
        assert!(lookup(ch).is_some(), "{ch:?} has no glyph");
    }
}

#[test]
fn unmapped_scalars_fall_back_to_the_replacement_glyph() {
    // CJK, Hangul, and emoji are outside Inconsolata's repertoire.
    for ch in ['一', '한', '🦀'] {
        assert_eq!(lookup(ch), None);
        assert_eq!(lookup_or_fallback(ch), lookup_or_fallback('\u{FFFD}'));
    }
    assert!(
        !lookup_or_fallback('🦀').is_blank(),
        "the fallback glyph must be visible"
    );
}

#[test]
fn space_is_blank_and_letters_have_ink() {
    assert!(lookup_or_fallback(' ').is_blank());
    for ch in ['A', 'g', '0', '#', 'é'] {
        assert!(!lookup_or_fallback(ch).is_blank(), "{ch:?} has no ink");
    }
}

#[test]
fn full_block_is_solid_edge_to_edge() {
    let block = lookup_or_fallback('█');
    for y in 0..atlas::CELL_HEIGHT {
        for x in 0..atlas::CELL_WIDTH {
            assert_eq!(block.coverage(x, y), 15, "hole at ({x}, {y})");
        }
    }
}

#[test]
fn coverage_is_transparent_outside_the_cell() {
    let glyph = lookup_or_fallback('A');
    assert_eq!(glyph.coverage(atlas::CELL_WIDTH, 0), 0);
    assert_eq!(glyph.coverage(0, atlas::CELL_HEIGHT), 0);
    assert_eq!(glyph.coverage(u32::MAX, u32::MAX), 0);
}

#[test]
fn fallback_never_panics_and_is_the_replacement_character() {
    assert_eq!(Glyph::fallback(), lookup_or_fallback('\u{FFFD}'));
}

#[cfg(feature = "render")]
mod render {
    use rustos_raster::{Color, Surface};

    use crate::atlas;
    use crate::font::BitmapFont;

    const WHITE: Color = Color::rgb(255, 255, 255);

    fn surface() -> Surface {
        Surface::new(64, 32).expect("surface")
    }

    #[test]
    fn metrics_are_the_atlas_cell() {
        let font = BitmapFont::inconsolata();
        assert_eq!(font.glyph_width(), atlas::CELL_WIDTH);
        assert_eq!(font.glyph_height(), atlas::CELL_HEIGHT);
        assert_eq!(font.advance(), atlas::CELL_WIDTH);
        assert_eq!(font.line_height(), atlas::CELL_HEIGHT);
    }

    #[test]
    fn text_width_is_cells_times_advance() {
        let font = BitmapFont::inconsolata();
        assert_eq!(font.text_width(""), 0);
        assert_eq!(font.text_width("abc"), 3 * font.advance());
        // Chars, not bytes: a two-byte UTF-8 scalar is still one cell.
        assert_eq!(font.text_width("é"), font.advance());
    }

    #[test]
    fn truncate_to_width_cuts_on_char_boundaries() {
        let font = BitmapFont::inconsolata();
        let advance = font.advance();
        assert_eq!(font.truncate_to_width("hello", 5 * advance), "hello");
        assert_eq!(font.truncate_to_width("hello", 3 * advance), "hel");
        assert_eq!(font.truncate_to_width("hello", advance - 1), "");
        assert_eq!(font.truncate_to_width("ééé", 2 * advance), "éé");
    }

    #[test]
    fn draw_text_advances_the_pen_and_leaves_ink() {
        let font = BitmapFont::inconsolata();
        let mut surface = surface();
        let pen = font.draw_text(&mut surface, 0, 0, "Hi", WHITE);
        assert_eq!(pen, i32::try_from(2 * font.advance()).expect("fits"));
        assert!(surface.pixels().iter().any(|p| p.a > 0), "no ink was drawn");
    }

    #[test]
    fn full_coverage_keeps_the_callers_colour() {
        let font = BitmapFont::inconsolata();
        let mut surface = surface();
        font.draw_text(&mut surface, 0, 0, "█", WHITE);
        // The full block covers every cell pixel at 15/15, which must map to
        // the caller's exact colour, not one rounded down.
        let px = surface.get(1, 1).expect("in bounds");
        assert_eq!((px.r, px.g, px.b, px.a), (255, 255, 255, 255));
    }

    #[test]
    fn offscreen_text_clips_without_panicking() {
        let font = BitmapFont::inconsolata();
        let mut surface = surface();
        font.draw_text(&mut surface, -1000, -1000, "clip", WHITE);
        font.draw_text(&mut surface, i32::MAX - 3, i32::MAX - 3, "clip", WHITE);
        assert!(surface.pixels().iter().all(|p| p.a == 0));
    }

    #[test]
    fn unmapped_text_draws_the_replacement_glyph() {
        let font = BitmapFont::inconsolata();
        let mut crab = surface();
        font.draw_text(&mut crab, 0, 0, "🦀", WHITE);
        let mut replacement = surface();
        font.draw_text(&mut replacement, 0, 0, "\u{FFFD}", WHITE);
        assert_eq!(crab.pixels(), replacement.pixels());
        assert!(crab.pixels().iter().any(|p| p.a > 0));
    }
}
