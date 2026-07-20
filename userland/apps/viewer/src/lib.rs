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
//! * [`ScrollView`] — the vertical scroll offset through a long file, held
//!   in the shared `lib/controls` scroll model (the same behaviour the
//!   window manager's root-viewport bars use). Arrow, page, and home/end
//!   keys step it; [`visible`](ScrollView::visible) is the line window the
//!   renderer draws.
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

/// Most display lines the viewer keeps for a picked file. A validation
/// bound like [`CONTENT_MAX`]: every retained line costs at least one input
/// byte (a character or a line feed), so this caps the line vector for a
/// hostile all-newline file without ever panicking.
pub const MAX_LINES: usize = CONTENT_MAX;

/// A scrollable view over a picked file's display lines.
///
/// The viewer is the *second, independent* consumer of the shared scroll
/// geometry engine ([`tairix_controls`]) — the window manager's
/// root-viewport bars are the first — so a nested application scroll uses the
/// same range validation and offset behaviour as a window-level bar rather
/// than a private recipe. The scroll unit here is a **display row**: the
/// content extent is the number of lines, the viewport extent is the rows the
/// window shows, and the [`ScrollModel`](tairix_controls::ScrollModel) owns
/// the first-visible-row offset. Arrow keys step one line, Page Up/Down step a
/// page, and Home/End jump to the bounds, exactly as the design language's
/// scrollbar keyboard model prescribes.
pub struct ScrollView {
    lines: Vec<String>,
    model: tairix_controls::ScrollModel,
}

impl ScrollView {
    /// Build a view over `lines` showing `visible_rows` rows at once.
    ///
    /// A page step is one row shy of a full page so a line stays on screen
    /// across a page turn; a degenerate zero-row window fails closed to a
    /// non-scrollable view (the shared range pins the offset to zero).
    #[must_use]
    pub fn new(lines: Vec<String>, visible_rows: usize) -> Self {
        let content = u64::try_from(lines.len()).unwrap_or(u64::MAX);
        let viewport = u64::try_from(visible_rows).unwrap_or(u64::MAX);
        let page = u64::try_from(visible_rows.saturating_sub(1).max(1)).unwrap_or(u64::MAX);
        let model = tairix_controls::ScrollModel::new(
            tairix_controls::ScrollRange::new(content, viewport, 0),
            1,
            page,
        );
        Self { lines, model }
    }

    /// The first visible row (the scroll offset), clamped into the lines.
    #[must_use]
    pub fn offset(&self) -> usize {
        usize::try_from(self.model.offset())
            .unwrap_or(usize::MAX)
            .min(self.lines.len())
    }

    /// The number of display lines the file produced.
    #[must_use]
    pub fn total_lines(&self) -> usize {
        self.lines.len()
    }

    /// The rows shown at once (the viewport extent).
    #[must_use]
    pub fn window_rows(&self) -> usize {
        usize::try_from(self.model.range().viewport_extent()).unwrap_or(usize::MAX)
    }

    /// The lines currently in view: at most [`window_rows`](Self::window_rows)
    /// lines starting at the offset.
    #[must_use]
    pub fn visible(&self) -> &[String] {
        let start = self.offset();
        let end = start
            .saturating_add(self.window_rows())
            .min(self.lines.len());
        self.lines.get(start..end).unwrap_or(&[])
    }

    /// The underlying scroll model, for a future scrollbar renderer to size a
    /// thumb from the same offset the view shows.
    #[must_use]
    pub fn model(&self) -> tairix_controls::ScrollModel {
        self.model
    }

    /// Scroll one line toward the start, returning whether the view moved.
    pub fn line_up(&mut self) -> bool {
        self.apply(tairix_controls::ScrollModel::line_backward)
    }

    /// Scroll one line toward the end, returning whether the view moved.
    pub fn line_down(&mut self) -> bool {
        self.apply(tairix_controls::ScrollModel::line_forward)
    }

    /// Scroll one page toward the start, returning whether the view moved.
    pub fn page_up(&mut self) -> bool {
        self.apply(tairix_controls::ScrollModel::page_backward)
    }

    /// Scroll one page toward the end, returning whether the view moved.
    pub fn page_down(&mut self) -> bool {
        self.apply(tairix_controls::ScrollModel::page_forward)
    }

    /// Jump to the first line, returning whether the view moved.
    pub fn to_top(&mut self) -> bool {
        self.apply(tairix_controls::ScrollModel::to_start)
    }

    /// Jump so the last lines are in view, returning whether the view moved.
    pub fn to_bottom(&mut self) -> bool {
        self.apply(tairix_controls::ScrollModel::to_end)
    }

    /// Scroll by `ticks` wheel detents, one line step per tick — the same
    /// convention the window manager's root viewport uses. A positive tick
    /// scrolls toward the end (downward), a negative one toward the start.
    /// Returns whether the view moved.
    pub fn scroll_ticks(&mut self, ticks: i32) -> bool {
        self.apply(|model| {
            let step = i64::try_from(model.line_step()).unwrap_or(i64::MAX);
            model.scroll_by(i64::from(ticks).saturating_mul(step))
        })
    }

    /// Apply `change` to the model, returning whether the offset changed.
    fn apply(
        &mut self,
        change: impl FnOnce(tairix_controls::ScrollModel) -> tairix_controls::ScrollModel,
    ) -> bool {
        let before = self.model.offset();
        self.model = change(self.model);
        self.model.offset() != before
    }
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

    /// Build a view over `total` numbered lines showing `rows` at once.
    fn view(total: usize, rows: usize) -> ScrollView {
        let lines: Vec<String> = (0..total).map(|n| alloc::format!("line {n}")).collect();
        ScrollView::new(lines, rows)
    }

    #[test]
    fn scroll_view_shows_a_window_of_lines_from_the_offset() {
        let mut v = view(100, 10);
        assert_eq!(v.offset(), 0);
        assert_eq!(v.visible().len(), 10);
        assert_eq!(v.visible()[0], "line 0");

        assert!(v.line_down());
        assert_eq!(v.offset(), 1);
        assert_eq!(v.visible()[0], "line 1");

        // A page steps one row shy of a full window so a line stays visible.
        assert!(v.page_down());
        assert_eq!(v.offset(), 1 + 9);
    }

    #[test]
    fn scroll_view_clamps_at_both_ends() {
        let mut v = view(100, 10);
        assert!(!v.line_up(), "already at the top");
        assert!(v.to_bottom());
        // The last row of content is the last row on screen: offset = 100 - 10.
        assert_eq!(v.offset(), 90);
        assert_eq!(v.visible().last().map(String::as_str), Some("line 99"));
        assert!(!v.line_down(), "already at the bottom");
        assert!(v.to_top());
        assert_eq!(v.offset(), 0);
    }

    #[test]
    fn scroll_view_scrolls_by_wheel_ticks_one_line_per_tick_and_clamps() {
        let mut v = view(100, 10);
        // Positive ticks scroll toward the end, one line per tick.
        assert!(v.scroll_ticks(3));
        assert_eq!(v.offset(), 3);
        // Negative ticks scroll back toward the start.
        assert!(v.scroll_ticks(-1));
        assert_eq!(v.offset(), 2);
        // A zero tick moves nothing (fail closed, no guessed distance).
        assert!(!v.scroll_ticks(0));
        assert_eq!(v.offset(), 2);
        // A large or hostile tick count saturates at the last row rather
        // than overshooting, and reports no further movement once pinned.
        assert!(v.scroll_ticks(i32::MAX));
        assert_eq!(v.offset(), 90);
        assert!(!v.scroll_ticks(i32::MAX));
        assert_eq!(v.offset(), 90);
    }

    #[test]
    fn scroll_view_with_fewer_lines_than_rows_is_not_scrollable() {
        let mut v = view(3, 10);
        assert_eq!(v.total_lines(), 3);
        assert!(!v.line_down(), "content fits, so nothing scrolls");
        assert!(!v.page_down());
        assert!(!v.to_bottom());
        assert_eq!(v.offset(), 0);
        assert_eq!(v.visible().len(), 3);
    }

    #[test]
    fn scroll_view_and_window_bars_share_the_same_offset_math() {
        // The viewer's model and a window-manager-style geometry over the same
        // range agree on the offset a thumb position implies — the point of one
        // shared engine.
        let v = view(1000, 20);
        let range = v.model().range();
        assert_eq!(range.content_extent(), 1000);
        assert_eq!(range.viewport_extent(), 20);
        assert_eq!(range.max_offset(), 980);
    }
}
