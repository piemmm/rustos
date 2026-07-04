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
    for (height, scale) in [(480, 1), (720, 2), (1080, 3), (2160, 4), (4320, 4)] {
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

/// A scale-1 test surface `cols`×`rows` cells, stride two pixels wider than the
/// visible width so tests exercise `stride != width`.
///
/// The two cell grids are leaked to `&'static mut [Cell]` (a host test runs
/// once and exits, so the leak is harmless) so the returned console borrows
/// them for `'static`, mirroring how a kernel caller leaks heap grid storage.
fn console_of(cols: u32, rows: u32) -> (TextConsole<'static>, Vec<u32>) {
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

/// A 2-column × 2-row scale-1 test surface.
fn small_console() -> (TextConsole<'static>, Vec<u32>) {
    console_of(2, 2)
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
fn a_glyph_renders_its_atlas_rows_in_the_default_colours() {
    let (mut console, mut pixels) = small_console();
    let dirty = console.write_bytes(&mut pixels, b"!");
    assert_eq!(dirty, Some((0, 8)), "dirty band covers the cell");
    let rendered = cell(&pixels, console.geometry(), 0, 0);
    let glyph = &glyphs::GLYPHS[(b'!' - b' ') as usize];
    for (y, &bits) in glyph.iter().enumerate() {
        for x in 0..CELL_WIDTH as usize {
            let lit = x < glyphs::GLYPH_WIDTH as usize
                && bits & (1 << (glyphs::GLYPH_WIDTH as usize - 1 - x)) != 0;
            let expected = if lit {
                DEFAULT_FOREGROUND
            } else {
                DEFAULT_BACKGROUND
            };
            assert_eq!(rendered[y * CELL_WIDTH as usize + x], expected, "({x},{y})");
        }
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
fn a_non_atlas_scalar_renders_the_question_mark_fallback() {
    // A Unicode scalar the 5×7 atlas has no glyph for prints `?`.
    let (mut console, mut pixels) = small_console();
    console.write_bytes(&mut pixels, "é".as_bytes());
    let geometry = *console.geometry();
    let fallback = cell(&pixels, &geometry, 0, 0);
    let (mut reference, mut ref_pixels) = small_console();
    reference.write_bytes(&mut ref_pixels, b"?");
    assert_eq!(fallback, cell(&ref_pixels, reference.geometry(), 0, 0));
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

#[test]
fn dirty_bands_merge_to_their_union() {
    assert_eq!(merge_bands(None, None), None);
    assert_eq!(merge_bands(Some((8, 16)), None), Some((8, 16)));
    assert_eq!(merge_bands(Some((8, 16)), Some((0, 8))), Some((0, 16)));
}
