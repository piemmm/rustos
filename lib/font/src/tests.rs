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
fn cjk_and_hebrew_are_not_in_the_console_atlas() {
    // The compiled-in atlas is the primary Latin face's repertoire only; CJK
    // and Hebrew scalars are not mapped, so the console shows the U+FFFD
    // fallback and `fontd` serves rich text at runtime instead.
    for ch in ['あ', 'ア', '漢', '日', '가', '각', '한', 'א', 'ב', 'ש'] {
        assert_eq!(lookup(ch), None, "{ch:?} must not be in the console atlas");
        assert_eq!(
            lookup_or_fallback(ch),
            lookup_or_fallback('\u{FFFD}'),
            "{ch:?} must fall back to the replacement glyph"
        );
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
    use crate::client::install_test_transport;
    use crate::font::BitmapFont;

    const WHITE: Color = Color::rgb(255, 255, 255);

    /// Install the shared solid test transport (`client::SolidTestTransport`).
    /// Every draw test installs the same transport, so the process-global
    /// client is deterministic even with the harness running in parallel.
    fn install() {
        install_test_transport();
    }

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
        install();
        let font = BitmapFont::inconsolata();
        let mut surface = surface();
        let pen = font.draw_text(&mut surface, 0, 0, "Hi", WHITE);
        assert_eq!(pen, i32::try_from(2 * font.advance()).expect("fits"));
        assert!(surface.pixels().iter().any(|p| p.a > 0), "no ink was drawn");
    }

    #[test]
    fn draw_text_paints_wide_glyphs_across_two_cells() {
        install();
        let font = BitmapFont::inconsolata();
        // Wide (CJK) scalars advance two cells; the service returns a two-cell
        // bitmap, so ink reaches the continuation cell.
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
        install();
        let font = BitmapFont::inconsolata();
        let mut surface = surface();
        font.draw_text(&mut surface, 0, 0, "█", WHITE);
        // Full 8-bit coverage (255) must map to the caller's exact colour, not
        // one rounded down — the top entry of the 256-entry blend table.
        let px = surface.get(1, 1).expect("in bounds");
        assert_eq!((px.r, px.g, px.b, px.a), (255, 255, 255, 255));
    }

    #[test]
    fn offscreen_text_clips_without_panicking() {
        install();
        let font = BitmapFont::inconsolata();
        let mut surface = surface();
        font.draw_text(&mut surface, -1000, -1000, "clip", WHITE);
        font.draw_text(&mut surface, i32::MAX - 3, i32::MAX - 3, "clip", WHITE);
        assert!(surface.pixels().iter().all(|p| p.a == 0));
    }

    #[test]
    fn a_scalar_the_faces_do_not_cover_still_draws() {
        install();
        // The client blits whatever coverage the service returns; resolving an
        // unmapped scalar to the U+FFFD fallback is the service's job (tested
        // in `fontd`). Here the scalar still produces a drawn glyph rather than
        // being silently dropped.
        let font = BitmapFont::inconsolata();
        let mut surface = surface();
        font.draw_text(&mut surface, 0, 0, "🦀", WHITE);
        assert!(surface.pixels().iter().any(|p| p.a > 0));
    }

    #[test]
    fn native_height_is_the_default_font() {
        // Exactly the native cell height is the console font, so nothing about
        // console-size rendering changes.
        assert_eq!(
            BitmapFont::with_pixel_height(atlas::CELL_HEIGHT),
            BitmapFont::inconsolata()
        );
    }

    #[test]
    fn oversized_height_is_clamped_to_the_maximum() {
        // A larger-than-native size is honoured (no longer clamped to native),
        // but a pathologically huge request clamps to the bound.
        assert_eq!(
            BitmapFont::with_pixel_height(atlas::CELL_HEIGHT + 100).glyph_height(),
            atlas::CELL_HEIGHT + 100
        );
        assert_eq!(
            BitmapFont::with_pixel_height(10_000).glyph_height(),
            BitmapFont::MAX_PIXEL_HEIGHT
        );
    }

    #[test]
    fn a_large_font_rasterises_bigger_crisp_glyphs_from_the_outline() {
        install();
        // A size well above native asks the service to rasterise a large
        // glyph from the outline (never an upscaled bitmap): metrics and ink
        // both scale up with the cell height.
        let big = BitmapFont::with_pixel_height(200);
        assert_eq!(big.glyph_height(), 200);
        assert!(big.advance() > BitmapFont::inconsolata().advance());

        let ink = |font: BitmapFont| {
            let mut surface =
                Surface::new(font.advance() * 2, font.glyph_height()).expect("surface");
            font.draw_text(&mut surface, 0, 0, "R", WHITE);
            surface.pixels().iter().filter(|p| p.a > 0).count()
        };
        let large_ink = ink(big);
        let small_ink = ink(BitmapFont::with_pixel_height(14));
        assert!(
            large_ink > 1000,
            "200px glyph has too little ink: {large_ink}"
        );
        assert!(
            large_ink > small_ink,
            "large glyph did not scale up ({large_ink} vs {small_ink})"
        );
    }

    #[test]
    fn pixel_height_clamps_to_the_legible_range() {
        let tiny = BitmapFont::with_pixel_height(1);
        assert_eq!(tiny.glyph_height(), BitmapFont::MIN_PIXEL_HEIGHT);
    }

    #[test]
    fn scaled_metrics_track_the_cell_height() {
        // Half the native height renders roughly half-size text while keeping
        // the width-to-height ratio: advance = round(15 * 14 / 28) = 8.
        let font = BitmapFont::with_pixel_height(14);
        assert_eq!(font.glyph_height(), 14);
        assert_eq!(font.line_height(), 14);
        assert_eq!(font.advance(), 8);
        assert_eq!(font.glyph_width(), 8);
        assert_eq!(font.text_width("abc"), 3 * font.advance());
        assert_eq!(font.text_width("日"), 2 * font.advance());
        assert_eq!(font.advance() * 2, font.text_width("ab"));
        // Every non-native cell height stays strictly smaller than native.
        assert!(font.advance() < BitmapFont::inconsolata().advance());
    }

    #[test]
    fn scaled_text_advances_by_the_scaled_metric_and_leaves_ink() {
        install();
        let font = BitmapFont::with_pixel_height(14);
        let mut surface = surface();
        let pen = font.draw_text(&mut surface, 0, 0, "Hi", WHITE);
        assert_eq!(pen, i32::try_from(2 * font.advance()).expect("fits"));
        assert!(surface.pixels().iter().any(|p| p.a > 0), "no ink was drawn");
        // Ink stays within the scaled cell box: nothing is drawn at or below
        // the scaled cell height, so a smaller font really is smaller.
        assert!(
            (font.glyph_height()..64)
                .all(|y| (0..64).all(|x| surface.get(x, y).is_none_or(|p| p.a == 0))),
            "ink spilled past the scaled cell height"
        );
    }

    #[test]
    fn scaled_full_block_is_opaque() {
        install();
        // Full coverage stays the caller's exact colour at a non-native size
        // too.
        let font = BitmapFont::with_pixel_height(14);
        let mut surface = surface();
        font.draw_text(&mut surface, 0, 0, "█", WHITE);
        let px = surface.get(1, 1).expect("in bounds");
        assert_eq!((px.r, px.g, px.b, px.a), (255, 255, 255, 255));
    }

    #[test]
    fn scaled_rendering_is_deterministic_across_the_cache() {
        install();
        // Drawing the same text at the same size twice must be identical:
        // a cache miss then a cache hit resolve to the same bytes.
        let font = BitmapFont::with_pixel_height(13);
        let mut first = surface();
        let mut second = surface();
        font.draw_text(&mut first, 0, 0, "cache me", WHITE);
        font.draw_text(&mut second, 0, 0, "cache me", WHITE);
        assert_eq!(first.pixels(), second.pixels());
    }

    #[test]
    fn scaled_wide_glyph_paints_its_continuation_cell() {
        install();
        let font = BitmapFont::with_pixel_height(16);
        let mut surface = surface();
        font.draw_text(&mut surface, 0, 0, "日", WHITE);
        assert!(
            (font.advance()..2 * font.advance()).any(|x| {
                (0..font.glyph_height()).any(|y| surface.get(x, y).is_some_and(|p| p.a > 0))
            }),
            "wide glyph has no ink in its continuation cell when scaled"
        );
    }

    #[test]
    fn scaled_offscreen_text_clips_without_panicking() {
        install();
        let font = BitmapFont::with_pixel_height(12);
        let mut surface = surface();
        font.draw_text(&mut surface, -1000, -1000, "clip", WHITE);
        font.draw_text(&mut surface, i32::MAX - 3, i32::MAX - 3, "clip", WHITE);
        assert!(surface.pixels().iter().all(|p| p.a == 0));
    }
}
