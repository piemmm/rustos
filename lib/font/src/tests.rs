//! Unit tests: atlas integrity, Unicode lookup, and the glyph blitter.

use crate::atlas;
use crate::glyph::{lookup, lookup_or_fallback, Glyph};

#[test]
fn atlas_payload_matches_its_declared_shape() {
    let table_entries = atlas::CELL_COUNT as usize + 1;
    let table_len = table_entries * size_of::<u32>();
    assert_eq!(
        read_payload_offset(0),
        0,
        "the first compressed glyph must start at payload offset zero"
    );
    let compressed_len = atlas::COVERAGE
        .len()
        .checked_sub(table_len)
        .expect("payload contains the complete offset table");
    let mut previous = 0usize;
    for index in 1..table_entries {
        let offset = read_payload_offset(index);
        assert!(offset >= previous, "glyph offsets are not monotonic");
        assert!(offset <= compressed_len, "glyph offset exceeds the payload");
        previous = offset;
    }
    assert_eq!(previous, compressed_len, "the final offset is not the end");
    assert!(
        lookup('\u{FFFD}').is_some(),
        "the declared fallback cell must be a real mapped glyph"
    );
}

fn read_payload_offset(index: usize) -> usize {
    let start = index * size_of::<u32>();
    let bytes: [u8; 4] = atlas::COVERAGE[start..start + 4]
        .try_into()
        .expect("complete offset entry");
    u32::from_le_bytes(bytes) as usize
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
fn ukrainian_cyrillic_has_glyphs_with_ink() {
    // The console-font regression behind LANG=uk-UA: every Ukrainian letter
    // must resolve to its own glyph with visible ink, never the U+FFFD
    // fallback.
    for ch in [
        'і', 'ї', 'є', 'ґ', 'І', 'Ї', 'Є', 'Ґ', 'а', 'я', 'А', 'Я', 'Щ', 'ь',
    ] {
        assert!(lookup(ch).is_some(), "{ch:?} has no glyph");
        assert!(!lookup_or_fallback(ch).is_blank(), "{ch:?} has no ink");
    }
}

#[test]
fn japanese_text_has_distinct_glyphs_with_ink() {
    for ch in ['あ', 'ア', '漢', '字', '日', '本', '語', '。', '「', '」'] {
        let glyph = lookup(ch);
        assert!(glyph.is_some(), "{ch:?} has no glyph");
        assert_ne!(glyph, lookup('\u{FFFD}'), "{ch:?} uses the fallback glyph");
        assert!(!lookup_or_fallback(ch).is_blank(), "{ch:?} has no ink");
    }
}

#[test]
fn korean_text_has_distinct_glyphs_with_ink() {
    for ch in ['가', '각', '한', '글', '안', '녕', '하', '세', '요', '힣'] {
        let glyph = lookup(ch);
        assert!(glyph.is_some(), "{ch:?} has no glyph");
        assert_ne!(glyph, lookup('\u{FFFD}'), "{ch:?} uses the fallback glyph");
        assert!(!lookup_or_fallback(ch).is_blank(), "{ch:?} has no ink");
    }
}

#[test]
fn hebrew_text_has_distinct_glyphs_with_ink() {
    for ch in [
        'א', 'ב', 'ה', 'ו', 'ל', 'ם', 'ש', 'ך', 'ן', 'ף', 'ץ', '־', '׳', '״', '\u{05B0}',
        '\u{05B8}', '\u{05BC}',
    ] {
        let glyph = lookup(ch);
        assert!(glyph.is_some(), "{ch:?} has no glyph");
        assert_ne!(glyph, lookup('\u{FFFD}'), "{ch:?} uses the fallback glyph");
        assert!(!lookup_or_fallback(ch).is_blank(), "{ch:?} has no ink");
    }
}

#[test]
fn unmapped_scalars_fall_back_to_the_replacement_glyph() {
    assert_eq!(lookup('🦀'), None);
    assert_eq!(lookup_or_fallback('🦀'), lookup_or_fallback('\u{FFFD}'));
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
fn full_block_is_solid_with_no_holes() {
    // U+2588 FULL BLOCK fills the face's exact advance and its exact
    // ascent-to-descent extent. The cell rounds the advance to whole pixels
    // and rounds the vertical metrics up to whole rows, so the interior is
    // fully covered while the outermost column and row carry only their
    // fractional share of the ink — most of a pixel, never a hole.
    let block = lookup_or_fallback('█');
    for y in 0..atlas::CELL_HEIGHT {
        for x in 0..atlas::CELL_WIDTH {
            let coverage = block.coverage(x, y);
            let interior = x < atlas::CELL_WIDTH - 1 && y < atlas::CELL_HEIGHT - 1;
            if interior {
                assert_eq!(coverage, 15, "hole at ({x}, {y})");
            } else {
                assert!(coverage > 7, "edge too thin at ({x}, {y}): {coverage}");
            }
        }
    }
}

#[test]
fn coverage_is_transparent_outside_the_glyph() {
    let glyph = lookup_or_fallback('A');
    for x in atlas::CELL_WIDTH..atlas::GLYPH_WIDTH {
        assert_eq!(glyph.coverage(x, 0), 0, "narrow glyph spills at x={x}");
    }
    assert_eq!(glyph.coverage(atlas::GLYPH_WIDTH, 0), 0);
    assert_eq!(glyph.coverage(0, atlas::CELL_HEIGHT), 0);
    assert_eq!(glyph.coverage(u32::MAX, u32::MAX), 0);
}

#[test]
fn fallback_never_panics_and_is_the_replacement_character() {
    assert_eq!(Glyph::fallback(), lookup_or_fallback('\u{FFFD}'));
}

#[cfg(feature = "render")]
mod render {
    use tairix_raster::{Color, Surface};

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
        assert_eq!(font.text_width("日本"), 4 * font.advance());
        assert_eq!(font.text_width("한글"), 4 * font.advance());
    }

    #[test]
    fn truncate_to_width_cuts_on_char_boundaries() {
        let font = BitmapFont::inconsolata();
        let advance = font.advance();
        assert_eq!(font.truncate_to_width("hello", 5 * advance), "hello");
        assert_eq!(font.truncate_to_width("hello", 3 * advance), "hel");
        assert_eq!(font.truncate_to_width("hello", advance - 1), "");
        assert_eq!(font.truncate_to_width("ééé", 2 * advance), "éé");
        assert_eq!(font.truncate_to_width("a日本", 3 * advance), "a日");
        assert_eq!(font.truncate_to_width("日本", advance), "");
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
    fn draw_text_paints_wide_glyphs_across_two_cells() {
        let font = BitmapFont::inconsolata();
        for (text, language) in [("日", "Japanese"), ("한", "Korean")] {
            let mut surface = surface();
            let pen = font.draw_text(&mut surface, 0, 0, text, WHITE);
            assert_eq!(pen, i32::try_from(2 * font.advance()).expect("fits"));
            assert!(
                (font.advance()..2 * font.advance()).any(|x| {
                    (0..font.glyph_height())
                        .any(|y| surface.get(x, y).is_some_and(|pixel| pixel.a > 0))
                }),
                "{language} glyph has no ink in its continuation cell"
            );
        }
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
