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
    use alloc::boxed::Box;

    use tairix_abi::font_ipc::FamilyKey;
    use tairix_log::DiscardSink;
    use tairix_raster::{Color, Surface};
    use tairix_reclaim::{PressureBand, ReclaimCache, ReclaimOwner, ReportedPressure};

    use crate::atlas;
    use crate::client::{install_test_transport, set_glyph_cache};
    use crate::font::BitmapFont;
    use crate::glyph_cache::{glyph_cache_budget, glyph_cache_candidate};

    const WHITE: Color = Color::rgb(255, 255, 255);

    /// A family the shared test transport serves as proportional (see
    /// `client::SolidTestTransport`).
    fn proportional_family() -> FamilyKey {
        FamilyKey::new("inter").expect("a well-formed family key")
    }

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
    fn console_metrics_are_the_atlas_cell() {
        install();
        let font = BitmapFont::console();
        assert_eq!(font.cell_width(), atlas::CELL_WIDTH);
        assert_eq!(font.glyph_height(), atlas::CELL_HEIGHT);
        assert_eq!(font.monospace_advance(), Some(atlas::CELL_WIDTH));
        assert_eq!(font.line_height(), atlas::CELL_HEIGHT);
    }

    #[test]
    fn text_width_is_cells_times_advance() {
        install();
        let font = BitmapFont::console();
        assert_eq!(font.text_width(""), 0);
        assert_eq!(font.text_width("abc"), 3 * font.cell_width());
        // Chars, not bytes: a two-byte UTF-8 scalar is still one cell.
        assert_eq!(font.text_width("é"), font.cell_width());
        assert_eq!(font.text_width("日本"), 4 * font.cell_width());
        assert_eq!(font.text_width("한글"), 4 * font.cell_width());
    }

    #[test]
    fn truncate_to_width_cuts_on_char_boundaries() {
        install();
        let font = BitmapFont::console();
        let cell = font.cell_width();
        assert_eq!(font.truncate_to_width("hello", 5 * cell), "hello");
        assert_eq!(font.truncate_to_width("hello", 3 * cell), "hel");
        assert_eq!(font.truncate_to_width("hello", cell - 1), "");
        assert_eq!(font.truncate_to_width("ééé", 2 * cell), "éé");
        assert_eq!(font.truncate_to_width("a日本", 3 * cell), "a日");
        assert_eq!(font.truncate_to_width("日本", cell), "");
    }

    #[test]
    fn draw_text_advances_the_pen_and_leaves_ink() {
        install();
        let font = BitmapFont::console();
        let mut surface = surface();
        let pen = font.draw_text(&mut surface, 0, 0, "Hi", WHITE);
        assert_eq!(pen, i32::try_from(font.text_width("Hi")).expect("fits"));
        assert!(surface.pixels().iter().any(|p| p.a > 0), "no ink was drawn");
    }

    #[test]
    fn draw_text_paints_wide_glyphs_across_two_cells() {
        install();
        let font = BitmapFont::console();
        // Wide (CJK) scalars advance two cells; the service returns a two-cell
        // bitmap, so ink reaches the continuation cell.
        for (text, language) in [("日", "Japanese"), ("한", "Korean")] {
            let mut surface = surface();
            let pen = font.draw_text(&mut surface, 0, 0, text, WHITE);
            assert_eq!(pen, i32::try_from(2 * font.cell_width()).expect("fits"));
            assert!(
                (font.cell_width()..2 * font.cell_width()).any(|x| {
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
        let font = BitmapFont::console();
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
        let font = BitmapFont::console();
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
        let font = BitmapFont::console();
        let mut surface = surface();
        font.draw_text(&mut surface, 0, 0, "🦀", WHITE);
        assert!(surface.pixels().iter().any(|p| p.a > 0));
    }

    #[test]
    fn native_height_is_the_default_font() {
        install();
        // Exactly the native cell height is the console font, so nothing about
        // console-size rendering changes.
        assert_eq!(
            BitmapFont::monospace(atlas::CELL_HEIGHT),
            BitmapFont::console()
        );
    }

    #[test]
    fn oversized_height_is_clamped_to_the_maximum() {
        // A larger-than-native size is honoured (no longer clamped to native),
        // but a pathologically huge request clamps to the bound.
        assert_eq!(
            BitmapFont::monospace(atlas::CELL_HEIGHT + 100).glyph_height(),
            atlas::CELL_HEIGHT + 100
        );
        assert_eq!(
            BitmapFont::monospace(10_000).glyph_height(),
            BitmapFont::MAX_PIXEL_HEIGHT
        );
    }

    #[test]
    fn a_large_font_rasterises_bigger_crisp_glyphs_from_the_outline() {
        install();
        // A size well above native asks the service to rasterise a large
        // glyph from the outline (never an upscaled bitmap): metrics and ink
        // both scale up with the cell height.
        let big = BitmapFont::monospace(200);
        assert_eq!(big.glyph_height(), 200);
        assert!(big.cell_width() > BitmapFont::console().cell_width());

        let ink = |font: BitmapFont| {
            let mut surface =
                Surface::new(font.cell_width() * 2, font.glyph_height()).expect("surface");
            font.draw_text(&mut surface, 0, 0, "R", WHITE);
            surface.pixels().iter().filter(|p| p.a > 0).count()
        };
        let large_ink = ink(big);
        let small_ink = ink(BitmapFont::monospace(14));
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
        let tiny = BitmapFont::monospace(1);
        assert_eq!(tiny.glyph_height(), BitmapFont::MIN_PIXEL_HEIGHT);
    }

    #[test]
    fn scaled_metrics_track_the_cell_height() {
        install();
        // Half the native height renders roughly half-size text while keeping
        // the width-to-height ratio: advance = round(15 * 14 / 28) = 8.
        let font = BitmapFont::monospace(14);
        assert_eq!(font.glyph_height(), 14);
        assert_eq!(font.line_height(), 14);
        assert_eq!(font.cell_width(), 8);
        assert_eq!(font.text_width("abc"), 3 * font.cell_width());
        assert_eq!(font.text_width("日"), 2 * font.cell_width());
        assert_eq!(font.cell_width() * 2, font.text_width("ab"));
        // Every non-native cell height stays strictly smaller than native.
        assert!(font.cell_width() < BitmapFont::console().cell_width());
    }

    #[test]
    fn scaled_text_advances_by_the_scaled_metric_and_leaves_ink() {
        install();
        let font = BitmapFont::monospace(14);
        let mut surface = surface();
        let pen = font.draw_text(&mut surface, 0, 0, "Hi", WHITE);
        assert_eq!(pen, i32::try_from(2 * font.cell_width()).expect("fits"));
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
        let font = BitmapFont::monospace(14);
        let mut surface = surface();
        font.draw_text(&mut surface, 0, 0, "█", WHITE);
        let px = surface.get(1, 1).expect("in bounds");
        assert_eq!((px.r, px.g, px.b, px.a), (255, 255, 255, 255));
    }

    #[test]
    fn text_renders_identically_uncached_cached_and_after_a_forced_shrink() {
        static SINK: DiscardSink = DiscardSink;

        install();
        // The cache is an accelerator, never a correctness dependency, so the
        // same text must paint the same pixels in all three states: with no
        // cache installed at all, served from a cache, and after memory
        // pressure has emptied one.
        let font = BitmapFont::monospace(13);
        let mut uncached = surface();
        font.draw_text(&mut uncached, 0, 0, "cache me", WHITE);

        let gauge: &'static ReportedPressure = Box::leak(Box::new(ReportedPressure::unknown()));
        gauge.report(PressureBand::Normal);
        set_glyph_cache(ReclaimCache::new(
            "test.font.render",
            glyph_cache_candidate(ReclaimOwner::UserlandProcess("test.font")),
            glyph_cache_budget(1 << 30),
            gauge,
            &SINK,
        ));

        let mut miss = surface();
        let mut hit = surface();
        font.draw_text(&mut miss, 0, 0, "cache me", WHITE);
        font.draw_text(&mut hit, 0, 0, "cache me", WHITE);
        assert_eq!(uncached.pixels(), miss.pixels());
        assert_eq!(uncached.pixels(), hit.pixels());

        gauge.report(PressureBand::Mild);
        let mut shrunk = surface();
        font.draw_text(&mut shrunk, 0, 0, "cache me", WHITE);
        assert_eq!(uncached.pixels(), shrunk.pixels());
    }

    #[test]
    fn scaled_wide_glyph_paints_its_continuation_cell() {
        install();
        let font = BitmapFont::monospace(16);
        let mut surface = surface();
        font.draw_text(&mut surface, 0, 0, "日", WHITE);
        assert!(
            (font.cell_width()..2 * font.cell_width()).any(|x| {
                (0..font.glyph_height()).any(|y| surface.get(x, y).is_some_and(|p| p.a > 0))
            }),
            "wide glyph has no ink in its continuation cell when scaled"
        );
    }

    #[test]
    fn scaled_offscreen_text_clips_without_panicking() {
        install();
        let font = BitmapFont::monospace(12);
        let mut surface = surface();
        font.draw_text(&mut surface, -1000, -1000, "clip", WHITE);
        font.draw_text(&mut surface, i32::MAX - 3, i32::MAX - 3, "clip", WHITE);
        assert!(surface.pixels().iter().all(|p| p.a == 0));
    }

    // -- Proportional-family coverage -----------------------------------

    #[test]
    fn a_proportional_family_reports_no_monospace_advance() {
        install();
        let font = BitmapFont::new(proportional_family(), 20);
        assert_eq!(font.monospace_advance(), None);
    }

    #[test]
    fn a_proportional_familys_glyphs_have_varying_advances() {
        install();
        let font = BitmapFont::new(proportional_family(), 24);
        let widths = ['i', 'M', 'x', 'W'].map(|ch| font.advance(ch));
        assert!(
            widths.iter().any(|&w| w != widths[0]),
            "advances must genuinely differ across scalars: {widths:?}"
        );
        // `text_width` sums the real per-glyph advances rather than
        // multiplying a character count by one cell width.
        let text = "iMxW";
        let expected: u32 = text.chars().map(|ch| font.advance(ch)).sum();
        assert_eq!(font.text_width(text), expected);
    }

    #[test]
    fn a_proportional_labels_measured_width_centres_it_in_its_box() {
        install();
        let font = BitmapFont::new(proportional_family(), 20);
        let label = "Settings";
        let box_width = 200u32;
        let measured = font.text_width(label);
        assert!(
            measured > 0 && measured < box_width,
            "label must fit: {measured}"
        );
        let left_margin = (box_width - measured) / 2;
        // A caller centring by measurement (rather than a guessed column
        // count) leaves equal, non-degenerate margins on both sides.
        let right_margin = box_width - measured - left_margin;
        assert!(left_margin.abs_diff(right_margin) <= 1);
    }

    #[test]
    fn proportional_truncation_respects_each_glyphs_own_advance() {
        install();
        let font = BitmapFont::new(proportional_family(), 20);
        let text = "iMxWiMxW";
        let full_width = font.text_width(text);
        // Truncating to the full width returns the whole string.
        assert_eq!(font.truncate_to_width(text, full_width), text);
        // Truncating to less than the first character's own advance yields
        // the empty string, not a guessed one-column prefix.
        let first_advance = font.advance(text.chars().next().expect("non-empty"));
        assert_eq!(font.truncate_to_width(text, first_advance - 1), "");
        // A width that lands exactly on a prefix boundary keeps exactly that
        // many real characters, verified by re-measuring the prefix.
        let mut boundary = 0u32;
        let mut prefix_len = 0usize;
        for ch in text.chars().take(text.chars().count() - 1) {
            boundary += font.advance(ch);
            prefix_len += ch.len_utf8();
        }
        assert_eq!(font.truncate_to_width(text, boundary), &text[..prefix_len]);
    }

    /// The hit-test every proportional-aware caller must use: walk
    /// characters accumulating real advances until the click x falls within
    /// the current glyph's box.
    fn hit_test(font: BitmapFont, text: &str, x: u32) -> usize {
        let mut pen = 0u32;
        for (index, ch) in text.char_indices() {
            let advance = font.advance(ch);
            if x < pen + advance {
                return index;
            }
            pen += advance;
        }
        text.len()
    }

    #[test]
    fn a_click_hit_test_maps_x_to_a_character_index_by_accumulating_advances() {
        install();
        let font = BitmapFont::new(proportional_family(), 20);
        let text = "iMxW";
        let first_advance = font.advance('i');
        assert_eq!(hit_test(font, text, 0), 0);
        assert_eq!(hit_test(font, text, first_advance), 'i'.len_utf8());
        assert_eq!(hit_test(font, text, font.text_width(text) + 1), text.len());
    }

    #[test]
    fn draw_text_offsets_a_glyph_by_its_own_left_bearing() {
        install();
        // The test transport reports a zero left bearing, so this pins the
        // pen-plus-bearing contract against a regression that ignores
        // `left` entirely: moving the font's own advance forward must still
        // land ink starting no earlier than the pen.
        let font = BitmapFont::new(proportional_family(), 20);
        let mut surface = surface();
        font.draw_text(&mut surface, 5, 0, "M", WHITE);
        assert!(
            (0..5)
                .all(|x| (0..font.glyph_height())
                    .all(|y| surface.get(x, y).is_none_or(|p| p.a == 0))),
            "ink must not appear left of the pen when the bearing is zero"
        );
        assert!(surface.pixels().iter().any(|p| p.a > 0), "no ink was drawn");
    }

    #[test]
    fn families_and_metrics_reach_the_bitmap_font() {
        install();
        let entries = crate::client::families();
        assert!(entries.iter().any(|entry| entry.key == FamilyKey::MONO));
        let metrics = BitmapFont::console().metrics();
        assert_eq!(metrics.pixel_height, atlas::CELL_HEIGHT);
    }
}
