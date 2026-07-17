//! TAIRiX **file viewer** — the windowed read-only text viewer and the
//! first consumer of the desktop's trusted file picker
//! (`plans/APPWIN.md` AW5, `plans/CAPABILITY_USE.md` CU6).
//!
//! The viewer holds **no filesystem capability**: it cannot open, list,
//! or stat anything by itself. Its only reach into the filesystem is the
//! one file the user hands it — the app asks the desktop session to run
//! its trusted picker (`WindowRequest::PickFile`), and the session
//! delegates the chosen file one-shot (`fd_grant`), which the viewer
//! redeems into a read-only descriptor operated under the *session's*
//! authority. That is the whole CU6 model, exercised end to end by a
//! shipping app.
//!
//! # What this crate is
//!
//! The host-testable view engine the `Run` binary composes:
//!
//! * [`content_lines`] — the pure, bounded byte→line model: the picked
//!   file's bytes split into at most `max_rows` lines of at most
//!   `max_cols` characters, every non-printable byte sanitised to a
//!   placeholder so untrusted file content can never smuggle control
//!   sequences into the renderer (fail closed, never raw).
//! * [`render_status`] / [`render_lines`] — the themed painters: a
//!   one-line status ("waiting", "cancelled") or the content lines,
//!   drawn with the shared `lib/font` face onto a `lib/raster`
//!   [`Surface`] through the active `lib/theme` palette.
//!
//! # Layering & safety
//!
//! `no_std` (with `alloc`); depends only on the audited `lib/abi` crate
//! and the shared `lib/*` desktop libraries — never a kernel, driver, or
//! window-manager crate. No `unsafe` in this engine, and no
//! `unwrap`/`expect`/`panic!` in production paths.

#![no_std]
#![forbid(unsafe_op_in_unsafe_fn)]
#![deny(missing_docs)]

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;

use tairix_font::BitmapFont;
use tairix_raster::Surface;
use tairix_theme::Theme;

/// Window content width of the viewer window, in pixels — the one
/// definition the `Run` binary sizes its window with and a host-side
/// observer measures against.
pub const WIN_WIDTH: u32 = 480;

/// Window content height of the viewer window, in pixels (see
/// [`WIN_WIDTH`]).
pub const WIN_HEIGHT: u32 = 320;

/// Most picked-file bytes the viewer reads and shows. A validation
/// bound, not a capacity: the window shows a few dozen short lines, and
/// bounding the read keeps a hostile or enormous picked file from
/// pinning unbounded memory in the viewer.
pub const CONTENT_MAX: usize = 16 * 1024;

/// Padding in pixels between the window edge and the text.
const TEXT_PADDING: u32 = 4;

/// Vertical padding above and below a line's glyphs.
const LINE_PADDING: u32 = 2;

/// The placeholder shown for a byte that is not printable ASCII. One
/// visible character, so binary content reads as obviously sanitised
/// rather than corrupting the drawn line.
const PLACEHOLDER: char = '.';

/// Split `bytes` into at most `max_rows` display lines of at most
/// `max_cols` characters each.
///
/// The model is deliberately strict: printable ASCII (space through
/// tilde) passes through, a line feed ends a line, and **every** other
/// byte — control bytes, carriage returns, tabs, and non-ASCII — is
/// sanitised to a single visible placeholder dot. The picked file is
/// untrusted input; the
/// viewer shows an honest, bounded rendition and never feeds raw bytes
/// to anything that could interpret them.
#[must_use]
pub fn content_lines(bytes: &[u8], max_rows: usize, max_cols: usize) -> Vec<String> {
    let mut lines = Vec::new();
    let mut current = String::new();
    for &byte in bytes {
        if lines.len() >= max_rows {
            break;
        }
        if byte == b'\n' {
            lines.push(core::mem::take(&mut current));
            continue;
        }
        if current.len() >= max_cols {
            // The overflow is dropped, not wrapped: the viewer shows the
            // head of each line and the bound keeps the render cheap.
            continue;
        }
        let shown = if (b' '..=b'~').contains(&byte) {
            byte as char
        } else {
            PLACEHOLDER
        };
        current.push(shown);
    }
    if lines.len() < max_rows && !current.is_empty() {
        lines.push(current);
    }
    lines
}

/// Rows of text a [`WIN_HEIGHT`]-tall viewer window shows.
#[must_use]
pub fn visible_rows() -> usize {
    let line = line_height();
    if line == 0 {
        return 0;
    }
    usize::try_from(WIN_HEIGHT / line).unwrap_or(0)
}

/// Columns of text a [`WIN_WIDTH`]-wide viewer window shows, derived
/// from the shared monospace face.
#[must_use]
pub fn visible_cols() -> usize {
    let font = BitmapFont::inconsolata();
    let advance = font.advance();
    if advance == 0 {
        return 0;
    }
    usize::try_from(WIN_WIDTH.saturating_sub(TEXT_PADDING * 2) / advance).unwrap_or(0)
}

/// Height in pixels of one drawn text line.
fn line_height() -> u32 {
    BitmapFont::inconsolata()
        .glyph_height()
        .saturating_add(LINE_PADDING * 2)
}

/// Paint a one-line status message (the waiting and cancelled states)
/// centred on the first text row. Returns `None` only when the window
/// surface cannot be allocated (the caller fails closed).
#[must_use]
pub fn render_status(text: &str, theme: &Theme) -> Option<Surface> {
    let lines = [String::from(text)];
    render_slice(&lines, theme)
}

/// Paint the picked file's display `lines` from the top of the window.
/// Returns `None` only when the window surface cannot be allocated.
#[must_use]
pub fn render_lines(lines: &[String], theme: &Theme) -> Option<Surface> {
    render_slice(lines, theme)
}

/// The one painter behind both renderers.
fn render_slice(lines: &[String], theme: &Theme) -> Option<Surface> {
    let font = BitmapFont::inconsolata();
    let line = line_height();
    let mut surface = Surface::new(WIN_WIDTH, WIN_HEIGHT)?;
    let palette = theme.palette();
    surface.fill(palette.surface.into());
    let y_offset = line.saturating_sub(font.glyph_height()) / 2;
    for (row, text) in lines.iter().enumerate() {
        if text.is_empty() {
            continue;
        }
        let top = u32::try_from(row)
            .ok()
            .and_then(|row| row.checked_mul(line));
        let Some(top) = top else {
            break;
        };
        if top >= WIN_HEIGHT {
            break;
        }
        let usable = WIN_WIDTH.saturating_sub(TEXT_PADDING * 2);
        let fitted = font.truncate_to_width(text, usable);
        if fitted.is_empty() {
            continue;
        }
        font.draw_text(
            &mut surface,
            to_i32(TEXT_PADDING),
            to_i32(top.saturating_add(y_offset)),
            fitted,
            palette.on_surface.into(),
        );
    }
    Some(surface)
}

/// Saturating `u32` → `i32`.
fn to_i32(value: u32) -> i32 {
    i32::try_from(value).unwrap_or(i32::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;
    use tairix_theme::ThemeRegistry;

    #[test]
    fn content_lines_split_on_line_feeds_and_bound_rows_and_cols() {
        let lines = content_lines(b"one\ntwo\nthree", 8, 80);
        assert_eq!(lines, vec!["one", "two", "three"]);
        // The row bound truncates the tail, never panicking.
        assert_eq!(content_lines(b"a\nb\nc", 2, 80), vec!["a", "b"]);
        // The column bound drops each line's overflow.
        assert_eq!(content_lines(b"abcdef", 8, 3), vec!["abc"]);
        // Empty input shows nothing (not one empty line).
        assert!(content_lines(b"", 8, 80).is_empty());
    }

    #[test]
    fn content_lines_sanitise_every_non_printable_byte() {
        // Control bytes, CR, tab, DEL, and non-ASCII all become the
        // placeholder: untrusted content never reaches the renderer raw.
        let lines = content_lines(b"a\x1b[31mb\r\tc\x7f\xffd", 8, 80);
        assert_eq!(lines, vec!["a.[31mb..c..d"]);
    }

    #[test]
    fn renderers_produce_window_sized_surfaces() {
        let themes = ThemeRegistry::with_builtins();
        let theme = themes.active();
        let status = render_status("Choose a file", theme).expect("status renders");
        assert_eq!((status.width(), status.height()), (WIN_WIDTH, WIN_HEIGHT));
        let lines = content_lines(b"hello\nworld", visible_rows(), visible_cols());
        let content = render_lines(&lines, theme).expect("content renders");
        assert_eq!((content.width(), content.height()), (WIN_WIDTH, WIN_HEIGHT));
        // The two states draw observably different pixels somewhere.
        assert_ne!(status.pixels(), content.pixels());
    }

    #[test]
    fn view_geometry_is_non_degenerate() {
        assert!(visible_rows() > 4, "the window shows several lines");
        assert!(visible_cols() > 16, "the window shows several columns");
    }
}
