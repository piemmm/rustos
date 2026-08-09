//! Host unit tests for the shared framebuffer console engine: pure CPU pixel
//! arithmetic over a borrowed surface, so the whole terminal is exercised
//! without any hardware.

extern crate alloc;
use alloc::vec;
use alloc::vec::Vec;

use super::*;

// --- Geometry policy -------------------------------------------------------

#[test]
fn geometry_scale_policy_tracks_display_height() {
    for (height, scale) in [
        (480, 1),
        (720, 1),
        (1080, 1),
        (2160, 2),
        (4320, 4),
        (8640, 4),
    ] {
        let geometry = Geometry::for_display(1920, height, 1920 * 4).expect("geometry");
        assert_eq!(geometry.scale, scale, "height {height}");
    }
}

#[test]
fn geometry_rejects_unusable_surfaces() {
    // Pitch not whole pixels.
    assert!(Geometry::for_display(640, 480, 640 * 4 + 2).is_none());
    // Pitch narrower than a scanline.
    assert!(Geometry::for_display(640, 480, 639 * 4).is_none());
    // Degenerate extents.
    assert!(Geometry::for_display(0, 480, 640 * 4).is_none());
    assert!(Geometry::for_display(640, 0, 640 * 4).is_none());
    // Too small for one glyph cell.
    assert!(Geometry::for_display(4, 4, 4 * 4).is_none());
}

// --- Renderer / terminal ---------------------------------------------------

/// A scale-1 test surface `cols`×`rows` cells with the cursor left visible
/// (the console's default), stride two pixels wider than the visible width so
/// tests exercise `stride != width`.
///
/// The two cell grids are leaked to `&'static mut [Cell]` (a host test runs
/// once and exits, so the leak is harmless) so the returned console borrows
/// them for `'static`, mirroring how a kernel caller leaks heap grid storage.
fn cursor_console_of(cols: u32, rows: u32) -> (TextConsole<'static>, Vec<u32>) {
    let width_px = cols * CELL_WIDTH;
    let height_px = rows * CELL_HEIGHT;
    let geometry = Geometry {
        width_px,
        height_px,
        stride_px: width_px + 2,
        scale: 1,
    };
    assert_eq!((geometry.columns(), geometry.rows()), (cols, rows));
    let mut pixels = vec![0u32; geometry.pixel_count()];
    let main: &'static mut [Cell] = vec![Cell::BLANK; geometry.cell_count()].leak();
    let alt: &'static mut [Cell] = vec![Cell::BLANK; geometry.cell_count()].leak();
    let mut console = TextConsole::new(geometry, main, alt);
    console.clear(&mut pixels);
    (console, pixels)
}

/// [`cursor_console_of`] with the cursor hidden (`CSI ? 25 l`), so the tests
/// of glyphs, scrolling, and screens assert their own subject without the
/// cursor overlay standing on a cell; the overlay has its own test section.
fn console_of(cols: u32, rows: u32) -> (TextConsole<'static>, Vec<u32>) {
    let (mut console, mut pixels) = cursor_console_of(cols, rows);
    console.write_bytes(&mut pixels, b"\x1b[?25l");
    (console, pixels)
}

/// A 2-column × 2-row scale-1 test surface.
fn small_console() -> (TextConsole<'static>, Vec<u32>) {
    console_of(2, 2)
}

/// A `cols`-column surface one pixel taller than a whole number of cell rows,
/// so it has a bottom margin as well as stride slack: the pixels no cell flush
/// can reach.
fn console_with_margin(cols: u32, rows: u32) -> (TextConsole<'static>, Vec<u32>, Geometry) {
    let geometry = Geometry {
        width_px: cols * CELL_WIDTH,
        height_px: rows * CELL_HEIGHT + 1,
        stride_px: cols * CELL_WIDTH + 3,
        scale: 1,
    };
    let mut pixels = vec![0u32; geometry.pixel_count()];
    let main: &'static mut [Cell] = vec![Cell::BLANK; geometry.cell_count()].leak();
    let alt: &'static mut [Cell] = vec![Cell::BLANK; geometry.cell_count()].leak();
    let console = TextConsole::new(geometry, main, alt);
    pixels.fill(0);
    (console, pixels, geometry)
}

/// The pixels of one glyph cell, row-major.
fn cell(pixels: &[u32], geometry: &Geometry, column: u32, row: u32) -> Vec<u32> {
    let mut out = Vec::new();
    for y in 0..CELL_HEIGHT * geometry.scale {
        for x in 0..CELL_WIDTH * geometry.scale {
            let index = (row * CELL_HEIGHT * geometry.scale + y) as usize
                * geometry.stride_px as usize
                + (column * CELL_WIDTH * geometry.scale + x) as usize;
            out.push(pixels[index]);
        }
    }
    out
}

/// Whether cell `(column, row)` holds at least one pixel of `color`.
fn cell_has(pixels: &[u32], geometry: &Geometry, column: u32, row: u32, color: u32) -> bool {
    cell(pixels, geometry, column, row).contains(&color)
}

/// Whether cell `(column, row)` is entirely the default background.
fn cell_blank(pixels: &[u32], geometry: &Geometry, column: u32, row: u32) -> bool {
    cell(pixels, geometry, column, row)
        .iter()
        .all(|&p| p == DEFAULT_BACKGROUND)
}

#[test]
fn clear_paints_the_background_and_homes_the_cursor() {
    let (_, pixels) = small_console();
    assert!(pixels.iter().all(|&p| p == DEFAULT_BACKGROUND));
}

#[test]
fn a_glyph_renders_its_atlas_coverage_in_the_default_colours() {
    let (mut console, mut pixels) = small_console();
    let dirty = console.write_bytes(&mut pixels, b"!");
    assert_eq!(dirty, Some((0, CELL_HEIGHT)), "dirty band covers the cell");
    let rendered = cell(&pixels, console.geometry(), 0, 0);
    let glyph = lookup_or_fallback('!');
    let ramp = coverage_ramp(DEFAULT_FOREGROUND, DEFAULT_BACKGROUND);
    for y in 0..CELL_HEIGHT {
        for x in 0..CELL_WIDTH {
            let expected = ramp[usize::from(glyph.coverage(x, y))];
            assert_eq!(
                rendered[(y * CELL_WIDTH + x) as usize],
                expected,
                "({x},{y})"
            );
        }
    }
    assert!(
        rendered.contains(&DEFAULT_FOREGROUND),
        "the glyph has at least one fully covered pixel"
    );
}

#[test]
fn the_coverage_ramp_is_exact_at_its_endpoints_and_monotone() {
    let ramp = coverage_ramp(DEFAULT_FOREGROUND, DEFAULT_BACKGROUND);
    assert_eq!(ramp[0], DEFAULT_BACKGROUND);
    assert_eq!(ramp[15], DEFAULT_FOREGROUND);
    // Dark-on-light must blend just as correctly as light-on-dark.
    let inverted = coverage_ramp(DEFAULT_BACKGROUND, DEFAULT_FOREGROUND);
    assert_eq!(inverted[0], DEFAULT_FOREGROUND);
    assert_eq!(inverted[15], DEFAULT_BACKGROUND);
    for level in 1..16 {
        let channel = |p: u32| (p >> 8) & 0xFF;
        assert!(channel(ramp[level]) >= channel(ramp[level - 1]));
        assert!(channel(inverted[level]) <= channel(inverted[level - 1]));
    }
}

#[test]
fn control_bytes_are_consumed_not_printed() {
    // A bare C0 control (here SOH) is not a glyph: the parser drops it, so
    // nothing is drawn (unlike a byte-renderer's `?`).
    let (mut console, mut pixels) = small_console();
    assert_eq!(console.write_bytes(&mut pixels, &[0x01]), None);
    let geometry = *console.geometry();
    assert!(cell_blank(&pixels, &geometry, 0, 0));
}

#[test]
fn a_covered_unicode_scalar_renders_its_own_glyph() {
    // `é` is in the face's repertoire: it draws its own glyph, distinct from
    // both a bare `e` and the replacement character.
    let (mut console, mut pixels) = small_console();
    console.write_bytes(&mut pixels, "é".as_bytes());
    let geometry = *console.geometry();
    let accented = cell(&pixels, &geometry, 0, 0);
    assert!(accented.contains(&DEFAULT_FOREGROUND), "é has ink");
    let (mut reference, mut ref_pixels) = small_console();
    reference.write_bytes(&mut ref_pixels, b"e");
    assert_ne!(accented, cell(&ref_pixels, reference.geometry(), 0, 0));
}

#[test]
fn hebrew_scalars_render_the_fallback_in_single_cells() {
    // Hebrew is not in the console atlas, so each Hebrew scalar renders the
    // U+FFFD fallback in its own single cell (`fontd` serves real Hebrew); a
    // following `a` renders its own distinct glyph.
    let (mut console, mut pixels) = console_of(6, 1);
    console.write_bytes(&mut pixels, "שלוםa".as_bytes());
    let geometry = *console.geometry();
    for column in 0..5 {
        assert!(
            cell_has(&pixels, &geometry, column, 0, DEFAULT_FOREGROUND),
            "column {column} has no glyph ink"
        );
    }
    let fallback = cell(&pixels, &geometry, 0, 0);
    for column in 1..4 {
        assert_eq!(
            cell(&pixels, &geometry, column, 0),
            fallback,
            "Hebrew cell {column} is not the same fallback glyph"
        );
    }
    assert_ne!(
        cell(&pixels, &geometry, 4, 0),
        fallback,
        "`a` should render its own glyph, not the fallback"
    );
}

#[test]
fn hebrew_ink_survives_a_coloured_background_and_local_overwrite() {
    let (mut console, mut pixels) = console_of(3, 1);
    console.write_bytes(&mut pixels, "\u{1B}[48;2;10;20;30mאב".as_bytes());
    let geometry = *console.geometry();
    let background = pack_rgb(10, 20, 30);
    for column in 0..2 {
        assert!(
            cell(&pixels, &geometry, column, 0)
                .iter()
                .any(|&pixel| pixel != background),
            "Hebrew cell {column} contains only background"
        );
    }

    console.write_bytes(&mut pixels, "\u{1B}[1;2H ".as_bytes());
    assert!(
        cell(&pixels, &geometry, 0, 0)
            .iter()
            .any(|&pixel| pixel != background),
        "overwriting the second cell erased the first glyph"
    );
    assert!(
        cell(&pixels, &geometry, 1, 0)
            .iter()
            .all(|&pixel| pixel == background),
        "space did not erase exactly the selected Hebrew cell"
    );
}

#[test]
fn wide_east_asian_scalars_reserve_two_cells_and_advance_by_two() {
    // A wide (CJK) scalar is not in the console atlas, so it renders the
    // narrow U+FFFD fallback in its lead cell; it still reserves its
    // continuation cell and advances the cursor by two columns (`fontd`
    // serves the real two-cell CJK glyph).
    for (text, language) in [("世a", "Japanese"), ("한a", "Korean")] {
        let (mut console, mut pixels) = console_of(4, 1);
        console.write_bytes(&mut pixels, text.as_bytes());
        let geometry = *console.geometry();
        assert!(
            !cell_blank(&pixels, &geometry, 0, 0),
            "{language} lead glyph (fallback) drawn"
        );
        assert!(
            cell_blank(&pixels, &geometry, 1, 0),
            "{language} continuation cell reserved but blank"
        );
        assert!(
            cell_has(&pixels, &geometry, 2, 0, DEFAULT_FOREGROUND),
            "a follows {language} glyph in column 2"
        );
    }
}

#[test]
fn a_wide_scalar_reserved_continuation_is_background_only() {
    // The wide scalar renders the narrow fallback in its lead cell and
    // reserves its continuation cell, which is painted with the background
    // and carries no glyph ink.
    let (mut console, mut pixels) = console_of(2, 1);
    console.write_bytes(&mut pixels, "\u{1B}[48;2;10;20;30m日".as_bytes());
    let geometry = *console.geometry();
    let background = pack_rgb(10, 20, 30);
    assert!(
        cell(&pixels, &geometry, 1, 0)
            .iter()
            .all(|&pixel| pixel == background),
        "the reserved continuation cell has glyph ink"
    );
}

#[test]
fn positioned_overwrite_clears_the_other_half_of_a_japanese_glyph() {
    for column in [1, 2] {
        let (mut console, mut pixels) = console_of(4, 1);
        let sequence = alloc::format!("日\u{1B}[1;{column}Hx");
        console.write_bytes(&mut pixels, sequence.as_bytes());
        let geometry = *console.geometry();
        let overwritten = column - 1;
        let cleared = 1 - overwritten;
        assert!(
            cell_has(&pixels, &geometry, overwritten, 0, DEFAULT_FOREGROUND),
            "replacement glyph is visible"
        );
        assert!(
            cell_blank(&pixels, &geometry, cleared, 0),
            "the other half is cleared"
        );
    }
}

#[test]
fn partial_erase_clears_a_complete_japanese_glyph() {
    for sequence in ["日\u{1B}[1;1H\u{1B}[1K", "日\u{1B}[1;2H\u{1B}[0K"] {
        let (mut console, mut pixels) = console_of(3, 1);
        console.write_bytes(&mut pixels, sequence.as_bytes());
        let geometry = *console.geometry();
        assert!(cell_blank(&pixels, &geometry, 0, 0));
        assert!(cell_blank(&pixels, &geometry, 1, 0));
    }
}

#[test]
fn a_wide_glyph_wraps_whole_when_one_column_remains() {
    let (mut console, mut pixels) = console_of(3, 2);
    console.write_bytes(&mut pixels, "ab世".as_bytes());
    let geometry = *console.geometry();
    // Two narrow glyphs leave one column on row 0: the wide glyph wraps whole
    // to row 1, and the leftover column stays blank.
    assert!(
        cell_blank(&pixels, &geometry, 2, 0),
        "leftover column blank"
    );
    assert!(
        !cell_blank(&pixels, &geometry, 0, 1),
        "lead wrapped to row 1"
    );
    assert!(
        cell_blank(&pixels, &geometry, 1, 1),
        "reserved continuation cell on row 1 is blank (narrow fallback)"
    );
}

#[test]
fn an_sgr_escape_is_interpreted_not_printed() {
    // `CSI 31 m` selects red; the escape draws no glyphs of its own and the
    // following `A` is drawn in red — proving escape codes are parsed, not
    // echoed to the screen.
    let (mut console, mut pixels) = small_console();
    console.write_bytes(&mut pixels, b"\x1b[31mA");
    let geometry = *console.geometry();
    assert!(
        cell_has(&pixels, &geometry, 0, 0, BASIC_PALETTE[1]),
        "the glyph is drawn in red"
    );
    assert!(
        !cell_has(&pixels, &geometry, 0, 0, DEFAULT_FOREGROUND),
        "no default-colour pixels: the SGR took effect",
    );
    // Only one cell advanced: the escape bytes produced no glyph.
    assert!(cell_blank(&pixels, &geometry, 1, 0));
}

#[test]
fn sgr_256_colour_and_truecolour_resolve_to_the_palette() {
    let (mut console, mut pixels) = small_console();
    console.write_bytes(&mut pixels, b"\x1b[38;5;196mA");
    let geometry = *console.geometry();
    assert!(cell_has(&pixels, &geometry, 0, 0, indexed_pixel(196)));

    let (mut console, mut pixels) = small_console();
    console.write_bytes(&mut pixels, b"\x1b[38;2;10;20;30mA");
    assert!(cell_has(&pixels, &geometry, 0, 0, pack_rgb(10, 20, 30)));
}

#[test]
fn reverse_video_swaps_foreground_and_background() {
    // `CSI 7 m`: the glyph's lit pixels take the background colour and its cell
    // fills with the foreground colour.
    let (mut console, mut pixels) = small_console();
    console.write_bytes(&mut pixels, b"\x1b[7m!");
    let geometry = *console.geometry();
    assert!(cell_has(&pixels, &geometry, 0, 0, DEFAULT_BACKGROUND));
    assert!(cell_has(&pixels, &geometry, 0, 0, DEFAULT_FOREGROUND));
}

#[test]
fn backspace_steps_the_cursor_back_without_painting() {
    let (mut console, mut pixels) = small_console();
    // The `BS SP BS` rub-out overwrites the glyph and leaves the cursor back on
    // the erased cell.
    console.write_bytes(&mut pixels, b"A\x08 \x08");
    let geometry = *console.geometry();
    assert!(
        cell_blank(&pixels, &geometry, 0, 0),
        "the rub-out blanked it"
    );
    // The next glyph lands on the erased column, not the one after it.
    console.write_bytes(&mut pixels, b"!");
    assert!(cell_has(&pixels, &geometry, 0, 0, DEFAULT_FOREGROUND));
}

#[test]
fn carriage_return_returns_to_column_zero() {
    let (mut console, mut pixels) = small_console();
    console.write_bytes(&mut pixels, b"A\r ");
    let geometry = *console.geometry();
    // `\r` then a space overwrote the `A` in column 0.
    assert!(cell_blank(&pixels, &geometry, 0, 0));
}

#[test]
fn cursor_position_places_the_glyph_absolutely() {
    let (mut console, mut pixels) = small_console();
    // `CSI 2 ; 2 H` homes to row 2, column 2 (1-based) — the bottom-right cell.
    console.write_bytes(&mut pixels, b"\x1b[2;2HX");
    let geometry = *console.geometry();
    assert!(cell_has(&pixels, &geometry, 1, 1, DEFAULT_FOREGROUND));
    assert!(cell_blank(&pixels, &geometry, 0, 0));
}

#[test]
fn erase_in_line_clears_the_row() {
    let (mut console, mut pixels) = console_of(3, 1);
    console.write_bytes(&mut pixels, b"AB");
    // Return to column 0 and erase the whole line (`CSI 2 K`).
    console.write_bytes(&mut pixels, b"\r\x1b[2K");
    let geometry = *console.geometry();
    assert!(cell_blank(&pixels, &geometry, 0, 0));
    assert!(cell_blank(&pixels, &geometry, 1, 0));
}

#[test]
fn reaching_the_bottom_scrolls_the_text_up() {
    // A 1×3 surface: three lines fill it, and the fourth scrolls the first line
    // off the top rather than wrapping ring-style.
    let (mut console, mut pixels) = console_of(1, 3);
    console.write_bytes(&mut pixels, b"A\r\nB\r\nC");
    let geometry = *console.geometry();
    assert!(
        cell_has(&pixels, &geometry, 0, 0, DEFAULT_FOREGROUND),
        "A on row 0"
    );
    assert!(
        cell_has(&pixels, &geometry, 0, 2, DEFAULT_FOREGROUND),
        "C on row 2"
    );
    // The fourth line scrolls everything up one row: `A` is gone, `C` has moved
    // to row 1, and a fresh `D` lands on the new bottom row.
    console.write_bytes(&mut pixels, b"\r\nD");
    assert!(
        cell_has(&pixels, &geometry, 0, 2, DEFAULT_FOREGROUND),
        "D on the bottom row"
    );
    // Row 0 now holds what was row 1 (`B`), and the top `A` is gone: the surface
    // scrolled, it did not clear-and-wrap.
    assert!(
        !cell_blank(&pixels, &geometry, 0, 0),
        "content scrolled up, not cleared"
    );
}

#[test]
fn explicit_scroll_up_within_a_region() {
    let (mut console, mut pixels) = console_of(1, 3);
    console.write_bytes(&mut pixels, b"A\r\nB\r\nC");
    // `CSI 2 S`: scroll the whole display up two lines.
    console.write_bytes(&mut pixels, b"\x1b[2S");
    let geometry = *console.geometry();
    // `C` (row 2) moved to row 0; the two vacated rows are blank.
    assert!(cell_has(&pixels, &geometry, 0, 0, DEFAULT_FOREGROUND));
    assert!(cell_blank(&pixels, &geometry, 0, 1));
    assert!(cell_blank(&pixels, &geometry, 0, 2));
}

#[test]
fn entering_the_alternate_screen_clears_the_surface() {
    // A full-screen program (`top`) enters the alternate screen with
    // `CSI ? 1049 h`: the primary screen's content is hidden and a cleared
    // alternate screen is shown.
    let (mut console, mut pixels) = console_of(3, 1);
    console.write_bytes(&mut pixels, b"abc");
    let geometry = *console.geometry();
    assert!(
        cell_has(&pixels, &geometry, 0, 0, DEFAULT_FOREGROUND),
        "abc drawn"
    );
    let dirty = console.write_bytes(&mut pixels, b"\x1b[?1049h");
    assert_eq!(
        dirty,
        Some((0, geometry.height_px)),
        "the whole surface cleared"
    );
    assert!(
        cell_blank(&pixels, &geometry, 0, 0),
        "alternate screen is blank"
    );
    assert!(cell_blank(&pixels, &geometry, 1, 0));
    assert!(cell_blank(&pixels, &geometry, 2, 0));
}

#[test]
fn leaving_the_alternate_screen_restores_the_primary_screen() {
    // The heart of the alternate-screen contract: whatever the program drew
    // on the alternate screen is discarded on `CSI ? 1049 l`, and the primary
    // screen is repainted exactly as it was before the program started.
    let (mut console, mut pixels) = console_of(3, 1);
    console.write_bytes(&mut pixels, b"AB");
    let geometry = *console.geometry();
    let before: Vec<Vec<u32>> = (0..3).map(|c| cell(&pixels, &geometry, c, 0)).collect();

    // Enter the alternate screen and scribble different content on it.
    console.write_bytes(&mut pixels, b"\x1b[?1049hXYZ");
    assert!(
        cell_has(&pixels, &geometry, 0, 0, DEFAULT_FOREGROUND),
        "X drawn on alt"
    );

    // Leaving restores the primary screen pixel-for-pixel.
    let dirty = console.write_bytes(&mut pixels, b"\x1b[?1049l");
    assert_eq!(
        dirty,
        Some((0, geometry.height_px)),
        "the primary screen is repainted"
    );
    let after: Vec<Vec<u32>> = (0..3).map(|c| cell(&pixels, &geometry, c, 0)).collect();
    assert_eq!(
        after, before,
        "the primary screen returned exactly as it was"
    );
}

#[test]
fn the_alternate_screen_does_not_disturb_the_primary_grid() {
    // Content written while on the alternate screen must not leak into the
    // primary screen when it is restored — the two grids are independent.
    let (mut console, mut pixels) = console_of(2, 2);
    console.write_bytes(&mut pixels, b"P");
    let geometry = *console.geometry();
    let primary_p = cell(&pixels, &geometry, 0, 0);

    // On the alternate screen, fill both rows with different glyphs.
    console.write_bytes(&mut pixels, b"\x1b[?1049hQ\r\nR");
    // Restore: only the single `P` from the primary screen comes back.
    console.write_bytes(&mut pixels, b"\x1b[?1049l");
    assert_eq!(cell(&pixels, &geometry, 0, 0), primary_p, "P restored");
    assert!(
        cell_blank(&pixels, &geometry, 1, 0),
        "no alt content leaked"
    );
    assert!(
        cell_blank(&pixels, &geometry, 0, 1),
        "no alt content leaked"
    );
}

#[test]
fn leaving_the_alternate_screen_restores_the_saved_cursor() {
    // Entering saves the primary cursor; leaving restores it, so text written
    // after the program exits continues where it left off.
    let (mut console, mut pixels) = console_of(4, 1);
    console.write_bytes(&mut pixels, b"ab");
    let geometry = *console.geometry();
    // Enter, move the alt cursor elsewhere, then leave.
    console.write_bytes(&mut pixels, b"\x1b[?1049h\x1b[1;4HZ\x1b[?1049l");
    // The next glyph lands in column 2 (where the primary cursor was), not
    // column 3 where the alternate cursor ended up.
    console.write_bytes(&mut pixels, b"c");
    assert!(
        cell_has(&pixels, &geometry, 2, 0, DEFAULT_FOREGROUND),
        "c in column 2"
    );
    assert!(cell_blank(&pixels, &geometry, 3, 0), "column 3 untouched");
}

#[test]
fn leaving_the_alternate_screen_when_not_on_it_is_a_no_op() {
    // A stray `CSI ? 1049 l` with no matching enter must not clear or repaint.
    let (mut console, mut pixels) = console_of(2, 1);
    console.write_bytes(&mut pixels, b"A");
    let geometry = *console.geometry();
    assert_eq!(console.write_bytes(&mut pixels, b"\x1b[?1049l"), None);
    assert!(
        cell_has(&pixels, &geometry, 0, 0, DEFAULT_FOREGROUND),
        "A untouched"
    );
}

// --- Cursor overlay ----------------------------------------------------------

#[test]
fn the_cursor_is_drawn_as_a_reverse_video_block() {
    // A cleared console rests its cursor on the blank home cell: reverse
    // video fills the whole cell with the default foreground.
    let (console, pixels) = cursor_console_of(2, 2);
    let geometry = *console.geometry();
    assert!(cell(&pixels, &geometry, 0, 0)
        .iter()
        .all(|&p| p == DEFAULT_FOREGROUND));
    assert!(cell_blank(&pixels, &geometry, 1, 0));
}

#[test]
fn the_cursor_follows_text_and_restores_the_cell_it_leaves() {
    let (mut console, mut pixels) = cursor_console_of(3, 1);
    let geometry = *console.geometry();
    // After printing `A` the cursor block stands on column 1…
    console.write_bytes(&mut pixels, b"A");
    assert!(cell(&pixels, &geometry, 1, 0)
        .iter()
        .all(|&p| p == DEFAULT_FOREGROUND));
    // …and the glyph cell shows the normal (non-reversed) `A`.
    assert!(cell_has(&pixels, &geometry, 0, 0, DEFAULT_FOREGROUND));
    assert!(cell_has(&pixels, &geometry, 0, 0, DEFAULT_BACKGROUND));
    // A carriage return moves the block back over the `A` (reversed), and
    // the vacated cell repaints to its recorded blank — the overlay never
    // leaks into the grid.
    console.write_bytes(&mut pixels, b"\r");
    assert!(cell_blank(&pixels, &geometry, 1, 0));
    assert!(cell_has(&pixels, &geometry, 0, 0, DEFAULT_FOREGROUND));
}

#[test]
fn a_cursor_leaving_a_wide_continuation_restores_its_ink() {
    let (mut console, mut pixels) = cursor_console_of(3, 1);
    console.write_bytes(&mut pixels, "日".as_bytes());
    let geometry = *console.geometry();
    let expected = cell(&pixels, &geometry, 1, 0);

    console.write_bytes(&mut pixels, b"\x1b[1;2H");
    assert_ne!(
        cell(&pixels, &geometry, 1, 0),
        expected,
        "cursor overlays tail"
    );
    console.write_bytes(&mut pixels, b"\x1b[1;3H");

    assert_eq!(cell(&pixels, &geometry, 1, 0), expected);
}

#[test]
fn hide_and_show_cursor_toggle_the_overlay() {
    // DECTCEM: a full-screen program hides the cursor while it owns the
    // screen and shows it again on leave — the block must obey both.
    let (mut console, mut pixels) = cursor_console_of(2, 1);
    let geometry = *console.geometry();
    console.write_bytes(&mut pixels, b"\x1b[?25l");
    assert!(cell_blank(&pixels, &geometry, 0, 0));
    console.write_bytes(&mut pixels, b"\x1b[?25h");
    assert!(cell(&pixels, &geometry, 0, 0)
        .iter()
        .all(|&p| p == DEFAULT_FOREGROUND));
}

#[test]
fn dirty_bands_merge_to_their_union() {
    assert_eq!(merge_bands(None, None), None);
    assert_eq!(merge_bands(Some((8, 16)), None), Some((8, 16)));
    assert_eq!(merge_bands(Some((8, 16)), Some((0, 8))), Some((0, 16)));
}

// --- Batched rendering -------------------------------------------------

/// One batched write and the same stream fed a byte at a time must render
/// identical pixels: the per-batch flush is a scheduling change, never a
/// visible one. The stream wraps, scrolls several times, and changes
/// colour, so the grid-then-flush path is exercised across the operations
/// a listing burst produces.
#[test]
fn a_batched_burst_renders_identically_to_byte_at_a_time_writes() {
    let stream: &[u8] = b"one\r\ntwo\r\nthree3\r\n\x1b[31;44mred\x1b[0m\r\nlast";
    let (mut batched, mut batched_px) = console_of(4, 3);
    batched.write_bytes(&mut batched_px, stream);
    let (mut stepped, mut stepped_px) = console_of(4, 3);
    for byte in stream {
        stepped.write_bytes(&mut stepped_px, core::slice::from_ref(byte));
    }
    assert_eq!(batched_px, stepped_px);
}

/// A burst that scrolls reports the whole scrolled region as its dirty
/// band, so the freestanding writer cleans every scanline the flush
/// repainted.
#[test]
fn a_scrolling_burst_reports_the_full_region_dirty() {
    let (mut console, mut pixels) = console_of(1, 3);
    let dirty = console.write_bytes(&mut pixels, b"a\r\nb\r\nc\r\nd\r\ne");
    assert_eq!(dirty, Some((0, 3 * CELL_HEIGHT)));
}

#[test]
fn program_output_cooks_a_scrolling_burst_in_one_batch() {
    let (mut cooked, mut cooked_pixels) = console_of(1, 3);
    let dirty = cooked.write_output_bytes(&mut cooked_pixels, b"a\nb\nc\nd\ne");

    let (mut explicit, mut explicit_pixels) = console_of(1, 3);
    explicit.write_bytes(&mut explicit_pixels, b"a\r\nb\r\nc\r\nd\r\ne");

    assert_eq!(dirty, Some((0, 3 * CELL_HEIGHT)));
    assert_eq!(cooked_pixels, explicit_pixels);
}

#[test]
fn program_output_keeps_parser_state_across_writes() {
    let (mut cooked, mut cooked_pixels) = console_of(2, 2);
    cooked.write_output_bytes(&mut cooked_pixels, b"\x1b[3");
    cooked.write_output_bytes(&mut cooked_pixels, b"1mA\nB");

    let (mut explicit, mut explicit_pixels) = console_of(2, 2);
    explicit.write_bytes(&mut explicit_pixels, b"\x1b[31mA\r\nB");

    assert_eq!(cooked_pixels, explicit_pixels);
}

// --- Sharing the surface with another presenter ------------------------

#[test]
fn a_console_starts_owning_its_surface() {
    let (console, _) = small_console();
    assert_eq!(console.surface(), Surface::Shown);
}

/// The whole point of hiding: a graphical session's pixels must survive
/// every kind of console output written while it holds the surface.
#[test]
fn a_hidden_console_touches_no_pixel() {
    // Stands in for a composited frame: a colour no console write produces —
    // neither the default background nor any glyph colour.
    const FRAME: u32 = 0xFF12_3456;
    let (mut console, mut pixels) = cursor_console_of(4, 2);
    pixels.fill(FRAME);

    console.hide();
    assert_eq!(console.surface(), Surface::Hidden);
    // Printing, cooked output, a scroll, an erase, a cursor move, the
    // alternate screen, and an explicit clear — every path that paints.
    assert_eq!(console.write_bytes(&mut pixels, b"\x1b[?25h"), None);
    assert_eq!(
        console.write_output_bytes(&mut pixels, b"a\nb\nc\nd\n"),
        None
    );
    assert_eq!(console.write_bytes(&mut pixels, b"\x1b[2J\x1b[1;1Hx"), None);
    assert_eq!(console.write_bytes(&mut pixels, b"\x1b[?1049h"), None);
    assert_eq!(console.write_bytes(&mut pixels, b"\x1b[?1049l"), None);
    assert_eq!(console.clear(&mut pixels), None);

    assert!(
        pixels.iter().all(|&p| p == FRAME),
        "the other presenter's pixels are untouched"
    );
}

/// Retained, not discarded: what arrives while hidden is on screen the
/// moment the surface comes back, pixel-identical to a console that was
/// never hidden. This is the "return to the terminal you started from"
/// guarantee.
#[test]
fn showing_replays_everything_written_while_hidden() {
    let stream: &[u8] = b"\x1b[31mone\r\ntwo\r\nthree\r\n\x1b[0mlast";
    let (mut hidden, mut hidden_px) = cursor_console_of(5, 3);
    hidden.hide();
    hidden.write_bytes(&mut hidden_px, stream);
    hidden_px.fill(0xFFAB_CDEF);
    let band = hidden.show(&mut hidden_px);

    let (mut plain, mut plain_px) = cursor_console_of(5, 3);
    plain.write_bytes(&mut plain_px, stream);

    assert_eq!(hidden.surface(), Surface::Shown);
    assert_eq!(
        band,
        Some((0, 3 * CELL_HEIGHT)),
        "the whole surface repaints"
    );
    assert_eq!(hidden_px, plain_px);
}

/// The repaint covers the pixel margins outside the cell grid and the
/// stride slack too, so no sliver of the previous presenter's frame is
/// left in the gaps a cell flush never reaches.
#[test]
fn showing_blanks_the_margins_and_the_stride_slack() {
    let (mut console, mut pixels, _) = console_with_margin(2, 1);
    pixels.fill(0xFFAB_CDEF);

    console.show(&mut pixels);

    assert!(pixels.iter().all(|&p| p != 0xFFAB_CDEF));
}

/// A full-surface repaint dirties those margins, so the band it reports must
/// cover them: a presenter that flushed only the cell rows would leave a
/// stale sliver of the previous content on screen.
#[test]
fn a_full_repaint_reports_the_margins_in_its_dirty_band() {
    let (mut console, mut pixels, geometry) = console_with_margin(2, 1);
    let whole_surface = Some((0, geometry.height_px));

    assert_eq!(console.clear(&mut pixels), whole_surface);
    console.hide();
    assert_eq!(console.show(&mut pixels), whole_surface);
    assert_eq!(console.purge(&mut pixels), whole_surface);
}

/// Idempotent in both directions, which is what lets the panic path
/// reclaim the surface without knowing who held it.
#[test]
fn hide_and_show_are_idempotent() {
    let (mut console, mut pixels) = cursor_console_of(2, 1);
    console.write_bytes(&mut pixels, b"a");
    let shown = pixels.clone();

    console.hide();
    console.hide();
    assert_eq!(console.surface(), Surface::Hidden);

    console.show(&mut pixels);
    let once = pixels.clone();
    console.show(&mut pixels);

    assert_eq!(console.surface(), Surface::Shown);
    assert_eq!(pixels, once);
    assert_eq!(pixels, shown);
}

// --- Session purge ---------------------------------------------------------

/// The leak an erase cannot close: text a program left on the screen it was
/// not using is still in the grid that is not shown, so only a purge reaches
/// it.
#[test]
fn purge_blanks_the_screen_that_is_not_shown() {
    let (mut console, mut pixels) = console_of(6, 1);
    // A full-screen program's output, left behind on the alternate screen.
    console.write_bytes(&mut pixels, b"\x1b[?1049hsecret\x1b[?1049l");
    assert!(
        console.screen.alt.iter().any(|c| c.ch != ' '),
        "the alternate grid holds the program's output"
    );
    // An erase of the shown screen cannot reach it.
    console.write_bytes(&mut pixels, b"\x1b[2J");
    assert!(console.screen.alt.iter().any(|c| c.ch != ' '));

    console.purge(&mut pixels);

    assert!(
        console.screen.alt.iter().all(|&c| c == Cell::BLANK),
        "the purge blanks the grid that is not shown"
    );
    assert!(console.screen.main.iter().all(|&c| c == Cell::BLANK));
}

/// Every pixel is rewritten, the margins and the stride slack included, so no
/// sliver of the ended session's text survives on the surface.
#[test]
fn purge_repaints_the_whole_surface() {
    let (mut console, mut pixels, geometry) = console_with_margin(3, 1);
    console.write_bytes(&mut pixels, b"abc");
    pixels.fill(0xFFAB_CDEF);

    let dirty = console.purge(&mut pixels);

    assert_eq!(dirty, Some((0, geometry.height_px)));
    assert!(
        pixels.iter().all(|&p| p != 0xFFAB_CDEF),
        "not one pixel of the ended session's surface survives"
    );
}

/// The whole point of the purge as a session boundary: what the next session
/// sees and writes is what it would see on a console nobody had used.
#[test]
fn a_purged_console_is_indistinguishable_from_a_fresh_one() {
    // A pen colour, a reverse attribute, the alternate screen, a saved
    // cursor, a restricted scroll region, and a hidden cursor: every piece
    // of screen state a session can leave set.
    let used_by: &[u8] = b"\x1b[31;7mred\x1b7\x1b[?1049halt\x1b[?1049l\x1b[2;3r\x1b[?25l";
    // Enough output to scroll, so a scroll region the purge failed to
    // release would land the text elsewhere.
    let next_session: &[u8] = b"one\r\ntwo\r\nthree\r\nfour";

    let (mut used, mut used_px) = cursor_console_of(5, 3);
    used.write_bytes(&mut used_px, used_by);
    used.purge(&mut used_px);
    used.write_bytes(&mut used_px, next_session);

    let (mut fresh, mut fresh_px) = cursor_console_of(5, 3);
    fresh.write_bytes(&mut fresh_px, next_session);

    assert_eq!(used_px, fresh_px);
}

/// A session that ended mid-sequence must not have its held prefix completed
/// by the next session's first bytes.
#[test]
fn purge_drops_a_partly_received_escape_sequence() {
    let (mut console, mut pixels) = console_of(6, 1);
    console.write_bytes(&mut pixels, b"\x1b[");

    console.purge(&mut pixels);
    console.write_bytes(&mut pixels, b"31mX");

    assert_eq!(
        console.screen.cell_at(0, 0).ch,
        '3',
        "the bytes print literally instead of finishing the held sequence"
    );
}

/// A hidden console purges its retained state and paints nothing, so the
/// purge is what the next `show` reveals — the seat lease cannot resurrect
/// the ended session's text.
#[test]
fn purging_while_hidden_wipes_what_show_would_reveal() {
    const FRAME: u32 = 0xFF12_3456;
    let (mut console, mut pixels) = cursor_console_of(4, 2);
    console.write_bytes(&mut pixels, b"left\r\nover");
    console.hide();
    pixels.fill(FRAME);

    assert_eq!(console.purge(&mut pixels), None);
    assert!(
        pixels.iter().all(|&p| p == FRAME),
        "the other presenter's pixels are untouched"
    );

    console.show(&mut pixels);
    let (_, blank_px) = cursor_console_of(4, 2);
    assert_eq!(pixels, blank_px, "nothing of the session is revealed");
}

/// A hand-over between two graphical presenters must show neither the
/// outgoing session's pixels nor a replay of the text screen: blanking
/// clears every pixel and leaves the retained grid alone.
#[test]
fn blanking_clears_the_surface_without_replaying_the_text_screen() {
    let (mut console, mut pixels) = cursor_console_of(4, 2);
    console.write_bytes(&mut pixels, b"text");
    // Whatever the outgoing presenter left on the surface.
    pixels.fill(0xFFAB_CDEF);

    let band = console.blank(&mut pixels);

    assert_eq!(console.surface(), Surface::Blank);
    assert_eq!(
        band,
        Some((0, 2 * CELL_HEIGHT)),
        "the whole surface is cleared"
    );
    assert!(
        pixels.iter().all(|&p| p == DEFAULT_BACKGROUND),
        "no pixel of the outgoing frame survives"
    );
}

/// The blank is not a lockout: a program writing to this console takes the
/// surface back, and brings the whole retained screen with it — so a
/// hand-over that never completes still shows the reason it failed.
#[test]
fn a_programs_output_takes_a_blanked_surface_back_whole() {
    let (mut console, mut pixels) = cursor_console_of(5, 2);
    console.write_output_bytes(&mut pixels, b"one\n");
    console.blank(&mut pixels);
    assert!(pixels.iter().all(|&p| p == DEFAULT_BACKGROUND));

    let band = console.write_output_bytes(&mut pixels, b"two");

    // Including the line written before the blank: what comes back is the
    // whole retained screen, not just what this write dirtied.
    let (mut plain, mut plain_px) = cursor_console_of(5, 2);
    plain.write_output_bytes(&mut plain_px, b"one\ntwo");
    assert_eq!(console.surface(), Surface::Shown);
    assert_eq!(
        band,
        Some((0, 2 * CELL_HEIGHT)),
        "the whole screen comes back, not just the cells this write dirtied"
    );
    assert_eq!(pixels, plain_px);
}

/// A kernel diagnostic must not take a blanked surface back. On a shippable
/// image the diagnostic sink renders onto this very framebuffer, so one
/// routine record logged between two graphical sessions — "desktop session
/// ended" — would otherwise replay the whole boot log into the hand-over.
/// The record still reaches the retained screen, and its log.
#[test]
fn a_kernel_diagnostic_leaves_a_blanked_surface_blank() {
    let (mut console, mut pixels) = cursor_console_of(5, 2);
    console.write_output_bytes(&mut pixels, b"one\n");
    console.blank(&mut pixels);

    assert_eq!(console.write_bytes(&mut pixels, b"logged"), None);

    assert_eq!(console.surface(), Surface::Blank);
    assert!(
        pixels.iter().all(|&p| p == DEFAULT_BACKGROUND),
        "the hand-over's black survives a diagnostic"
    );

    // Not lost, though: taking the surface back shows it, exactly as a
    // diagnostic written while a desktop held the screen comes back.
    console.show(&mut pixels);
    let (mut plain, mut plain_px) = cursor_console_of(5, 2);
    plain.write_output_bytes(&mut plain_px, b"one\n");
    plain.write_bytes(&mut plain_px, b"logged");
    assert_eq!(pixels, plain_px);
}

/// Only real output reclaims a blanked surface. A write of no bytes says
/// nothing, so treating it as something to show would repaint the stale
/// text screen into the very gap the blank exists to keep black.
#[test]
fn an_empty_write_leaves_a_blanked_surface_blank() {
    let (mut console, mut pixels) = cursor_console_of(3, 1);
    console.write_output_bytes(&mut pixels, b"abc");
    console.blank(&mut pixels);

    assert_eq!(console.write_bytes(&mut pixels, b""), None);
    assert_eq!(console.write_output_bytes(&mut pixels, b""), None);

    assert_eq!(console.surface(), Surface::Blank);
    assert!(pixels.iter().all(|&p| p == DEFAULT_BACKGROUND));
}

/// Clearing a blanked surface leaves it blanked: the grid is cleared, and
/// taking the surface back to show nothing would defeat the hand-over.
#[test]
fn clearing_leaves_a_blanked_surface_blank() {
    let (mut console, mut pixels) = cursor_console_of(3, 1);
    console.write_output_bytes(&mut pixels, b"abc");
    console.blank(&mut pixels);

    assert_eq!(console.clear(&mut pixels), None);

    assert_eq!(console.surface(), Surface::Blank);
    assert!(pixels.iter().all(|&p| p == DEFAULT_BACKGROUND));
}

/// Ending one session's terminal must not end the hand-over the next one is
/// arriving through: the purge discards the retained screen, and the surface
/// stays black rather than being taken back to show the nothing it left.
#[test]
fn purging_leaves_a_blanked_surface_blank() {
    let (mut console, mut pixels) = cursor_console_of(4, 2);
    console.write_output_bytes(&mut pixels, b"\x1b[?1049hsecret\x1b[?1049l");
    console.blank(&mut pixels);

    assert_eq!(console.purge(&mut pixels), None);

    assert_eq!(console.surface(), Surface::Blank);
    assert!(pixels.iter().all(|&p| p == DEFAULT_BACKGROUND));
    assert!(
        console.screen.alt.iter().all(|&c| c == Cell::BLANK),
        "the discard still reaches the grid that is not shown"
    );
}

/// Adjacent blank runs with different backgrounds each keep their own
/// colour: the flush's span-fill fast path must break a run at a
/// background change, never bleed one background across it.
#[test]
fn adjacent_blank_runs_keep_their_own_backgrounds() {
    let (mut console, mut pixels) = console_of(4, 1);
    // Two red-background spaces then two green-background spaces, in one
    // burst, on one row.
    console.write_bytes(&mut pixels, b"\x1b[41m  \x1b[42m  ");
    let geometry = *console.geometry();
    let red = BASIC_PALETTE[1];
    let green = BASIC_PALETTE[2];
    for (column, expected) in [(0, red), (1, red), (2, green), (3, green)] {
        assert!(
            cell(&pixels, &geometry, column, 0)
                .iter()
                .all(|&p| p == expected),
            "column {column} keeps its own background"
        );
    }
}

// --- Shared conformance script -----------------------------------------

/// The shared conformance script's view of this screen model: a console
/// built at the script's `COLS`×`ROWS`, driving the grid through the same
/// per-operation entry point [`TextConsole::write_bytes`] batches use.
///
/// No surface is needed: the screen is a pure model, so the script checks
/// exactly what a hidden console would record.
struct ConformanceConsole {
    console: TextConsole<'static>,
}

impl ConformanceConsole {
    fn new() -> Self {
        let (console, _pixels) = console_of(
            u32::from(tairix_vt::conformance::COLS),
            u32::from(tairix_vt::conformance::ROWS),
        );
        Self { console }
    }
}

impl tairix_vt::conformance::ScreenModel for ConformanceConsole {
    fn cols(&self) -> u16 {
        u16::try_from(self.console.screen.cols()).unwrap_or(u16::MAX)
    }

    fn rows(&self) -> u16 {
        u16::try_from(self.console.screen.rows()).unwrap_or(u16::MAX)
    }

    fn apply(&mut self, op: &Op) {
        self.console.screen.apply(op);
    }

    fn glyph(&self, col: u16, row: u16) -> char {
        self.console
            .screen
            .cell_at(u32::from(col), u32::from(row))
            .ch
    }

    fn cursor(&self) -> (u16, u16) {
        let col = u16::try_from(self.console.screen.column).unwrap_or(u16::MAX);
        let row = u16::try_from(self.console.screen.row).unwrap_or(u16::MAX);
        (col, row)
    }
}

#[test]
fn the_screen_model_passes_the_shared_conformance_script() {
    // The desktop terminal emulator runs this same script over its own
    // `Grid`, so the two screens a program can be drawn on cannot disagree
    // about where its output lands.
    let mut screen = ConformanceConsole::new();
    if let Err(divergence) = tairix_vt::conformance::check(&mut screen) {
        panic!("screen conformance: {divergence:?}");
    }
}
