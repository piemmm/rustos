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
//! The window is pointer-first: an "Open…" [`Button`] requests the pick,
//! and a vertical [`ScrollBar`] down the text area's trailing edge is
//! pressed, dragged, and clicked exactly as the design language
//! prescribes. The keyboard remains a fully working secondary path (Enter
//! retries the pick, the arrow/page/home/end keys still step the view).
//!
//! # What this crate is
//!
//! The host-testable view engine the `Run` binary composes:
//!
//! * [`Viewer`] — the whole window's composed, pointer- and
//!   keyboard-driven state: the current file view or status message, the
//!   "Open…" button, and the scrollbar, kept in sync through one shared
//!   [`tairix_controls::ScrollModel`]. [`Viewer::on_pointer`] is the single
//!   pure entry point a host feeds a translated [`tairix_input::InputEvent`]
//!   into; [`Viewer::render`] draws the whole window.
//! * [`ViewerLayout`] — the one definition of where the header, the
//!   "Open…" button, the text area, and the scrollbar sit within a
//!   `width_px` × `height_px` window, shared by rendering, hit-testing,
//!   and the tests so the three can never disagree about where a control
//!   actually is.
//! * [`content_lines`] — the pure, bounded byte→line model: the picked
//!   file's bytes split into at most `max_rows` lines of at most
//!   `max_cols` characters, every non-printable byte sanitised to a
//!   placeholder so untrusted file content can never smuggle control
//!   sequences into the renderer (fail closed, never raw).
//! * [`render_status`] / [`render_lines`] — the themed painters: a
//!   one-line status ("no file chosen", "pick refused") or the content
//!   lines, drawn with the shared `lib/font` face onto a `lib/raster`
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
//! `unwrap`/`expect`/`panic!` in production paths. Every drawn control is
//! the shared [`tairix_controls`] implementation — this crate paints no
//! control of its own.

#![no_std]
#![forbid(unsafe_op_in_unsafe_fn)]
#![deny(missing_docs)]

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;

use tairix_controls::{
    Button, ButtonAction, ButtonContent, ControlRole, ScrollAction, ScrollBar, ScrollModel,
    ScrollOrientation, ScrollRange,
};
use tairix_font::BitmapFont;
use tairix_geometry::{Rect, Scale};
use tairix_input::InputEvent;
use tairix_raster::Surface;
use tairix_theme::Theme;

/// Initial window content width of the viewer window, in pixels — the
/// size the `Run` binary opens its window with and a host-side observer
/// measures against. The viewer is resizable: after opening it re-lays
/// its content out to whatever client size the window manager reports
/// ([`visible_cols_for`] / [`visible_rows_for`]), so this is a starting
/// size, not a fixed one.
pub const WIN_WIDTH: u32 = 480;

/// Initial window content height of the viewer window, in pixels (see
/// [`WIN_WIDTH`]).
pub const WIN_HEIGHT: u32 = 320;

/// The smallest client width the viewer draws into, in pixels: a floor so
/// a resize-to-nothing still shows at least a sliver of a line rather than
/// a zero-sized surface. The window manager clamps its own resize to a
/// minimum client too; this is the app's independent guard (fail closed,
/// never a zero-extent surface).
pub const MIN_WIN_WIDTH: u32 = 2 * TEXT_PADDING + 1;

/// The smallest client height the viewer draws into, in pixels (see
/// [`MIN_WIN_WIDTH`]).
pub const MIN_WIN_HEIGHT: u32 = 1;

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

/// The header bar's height: the standard interactive [`control_height`]
/// metric, scaled. It is also the row the "Open…" button sits in.
///
/// [`control_height`]: tairix_theme::Metrics::control_height
fn header_height(theme: &Theme, scale: Scale) -> u32 {
    scale.scale_length(theme.metrics().control_height)
}

/// The trailing-edge gutter the vertical scrollbar occupies, scaled from
/// the theme's own [`scrollbar_breadth`] metric.
///
/// [`scrollbar_breadth`]: tairix_theme::Metrics::scrollbar_breadth
fn scrollbar_gutter(theme: &Theme, scale: Scale) -> u32 {
    scale.scale_length(theme.metrics().scrollbar_breadth)
}

/// Rows of text a `height_px`-tall viewer window's text area shows, below
/// the header bar.
#[must_use]
pub fn visible_rows_for(height_px: u32, theme: &Theme, scale: Scale) -> usize {
    let text_h = height_px.saturating_sub(header_height(theme, scale));
    let line = line_height();
    if line == 0 {
        return 0;
    }
    usize::try_from(text_h / line).unwrap_or(0)
}

/// Rows of text the initial [`WIN_HEIGHT`]-tall viewer window shows.
#[must_use]
pub fn visible_rows(theme: &Theme, scale: Scale) -> usize {
    visible_rows_for(WIN_HEIGHT, theme, scale)
}

/// Columns of text a `width_px`-wide viewer window's text area shows,
/// derived from the shared monospace face and shrunk by the scrollbar
/// gutter so text never runs under the bar.
#[must_use]
pub fn visible_cols_for(width_px: u32, theme: &Theme, scale: Scale) -> usize {
    let font = BitmapFont::console();
    let advance = font.cell_width();
    if advance == 0 {
        return 0;
    }
    let text_w = width_px.saturating_sub(scrollbar_gutter(theme, scale));
    usize::try_from(text_w.saturating_sub(TEXT_PADDING * 2) / advance).unwrap_or(0)
}

/// Columns of text the initial [`WIN_WIDTH`]-wide viewer window shows.
#[must_use]
pub fn visible_cols(theme: &Theme, scale: Scale) -> usize {
    visible_cols_for(WIN_WIDTH, theme, scale)
}

/// Height in pixels of one drawn text line.
fn line_height() -> u32 {
    BitmapFont::console()
        .glyph_height()
        .saturating_add(LINE_PADDING * 2)
}

/// Paint a one-line status message (the waiting and cancelled states)
/// centred on the first text row of a `width_px` × `height_px` window.
/// Returns `None` only when the window surface cannot be allocated (the
/// caller fails closed).
#[must_use]
pub fn render_status(text: &str, theme: &Theme, width_px: u32, height_px: u32) -> Option<Surface> {
    let lines = [String::from(text)];
    render_slice(&lines, theme, width_px, height_px)
}

/// Paint the picked file's display `lines` from the top of a `width_px`
/// × `height_px` window. Returns `None` only when the window surface
/// cannot be allocated.
#[must_use]
pub fn render_lines(
    lines: &[String],
    theme: &Theme,
    width_px: u32,
    height_px: u32,
) -> Option<Surface> {
    render_slice(lines, theme, width_px, height_px)
}

/// The one painter behind both renderers, sized to the current window.
fn render_slice(lines: &[String], theme: &Theme, width_px: u32, height_px: u32) -> Option<Surface> {
    let font = BitmapFont::console();
    let line = line_height();
    let mut surface = Surface::new(width_px, height_px)?;
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
        if top >= height_px {
            break;
        }
        let usable = width_px.saturating_sub(TEXT_PADDING * 2);
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
/// window shows, and the [`ScrollModel`] owns
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

    /// Re-lay the view out for a resized window: replace the display
    /// `lines` (re-wrapped to the new column count by the caller) and show
    /// `visible_rows` rows at once, preserving the current first-visible
    /// row as closely as the new bounds allow (the shared range clamps it
    /// to the new maximum offset). This is the resize path — the viewer
    /// re-wraps its stored file to the new width and calls this so a
    /// resize keeps the reader's place instead of jumping to the top.
    pub fn relayout(&mut self, lines: Vec<String>, visible_rows: usize) {
        let offset = u64::try_from(self.offset()).unwrap_or(u64::MAX);
        let content = u64::try_from(lines.len()).unwrap_or(u64::MAX);
        let viewport = u64::try_from(visible_rows).unwrap_or(u64::MAX);
        let page = u64::try_from(visible_rows.saturating_sub(1).max(1)).unwrap_or(u64::MAX);
        self.lines = lines;
        // `ScrollRange::new` clamps the carried offset to the new maximum,
        // so a window that grew past the content pins the offset back into
        // range rather than leaving it dangling.
        self.model = tairix_controls::ScrollModel::new(
            tairix_controls::ScrollRange::new(content, viewport, offset),
            1,
            page,
        );
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

    /// Jump directly to `offset` — the scrollbar's own drag/track/end-button
    /// requests, which already carry a clamped absolute offset rather than a
    /// relative step. Returns whether the view moved.
    pub fn scroll_to(&mut self, offset: u64) -> bool {
        self.apply(|model| model.scroll_to(offset))
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

/// Logical width of the "Open…" button in the header bar.
const OPEN_BUTTON_WIDTH: u32 = 96;

/// The on-screen regions of a `width_px` × `height_px` viewer window: the
/// header bar holding the "Open…" button, the scrollable text area (already
/// shrunk by the trailing-edge scrollbar gutter), and the scrollbar's own
/// track.
///
/// This is the one definition of where each region sits, shared by
/// rendering, pointer routing, and the tests, so the three can never
/// disagree about where a control actually is.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct ViewerLayout {
    /// The header bar across the top of the window.
    pub header: Rect,
    /// The "Open…" button's own rectangle within the header.
    pub button: Rect,
    /// The scrollable text area, already shrunk by the scrollbar gutter.
    pub text: Rect,
    /// The vertical scrollbar's track, down the text area's trailing edge.
    pub scrollbar: Rect,
}

impl ViewerLayout {
    /// Resolve the layout for a `width_px` × `height_px` window under the
    /// active theme and scale. Every dimension is derived through saturating
    /// arithmetic, so an extreme or degenerate window size never panics —
    /// it simply yields a region too small to see or hit.
    #[must_use]
    pub fn for_window(width_px: u32, height_px: u32, theme: &Theme, scale: Scale) -> Self {
        let header_h = header_height(theme, scale).min(height_px);
        let header = Rect::new(0, 0, width_px, header_h);

        let inset = scale.scale_length(theme.metrics().control_inset);
        let button_h = header_h.saturating_sub(inset.saturating_mul(2)).max(1);
        let button_w = scale
            .scale_length(OPEN_BUTTON_WIDTH)
            .min(width_px.saturating_sub(inset.saturating_mul(2)));
        let button_y = header_h.saturating_sub(button_h) / 2;
        let button = Rect::new(to_i32(inset), to_i32(button_y), button_w, button_h);

        let below = height_px.saturating_sub(header_h);
        let breadth = scrollbar_gutter(theme, scale).min(width_px);
        let text_w = width_px.saturating_sub(breadth);
        let text = Rect::new(0, to_i32(header_h), text_w, below);
        let scrollbar = Rect::new(to_i32(text_w), to_i32(header_h), breadth, below);

        Self {
            header,
            button,
            text,
            scrollbar,
        }
    }
}

/// The outcome of routing one pointer event into a [`Viewer`].
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct ViewerPointerOutcome {
    /// Whether anything the window draws actually changed — a hover, a
    /// press, a drag, or a scroll — so the host knows whether to repaint.
    pub changed: bool,
    /// Whether the "Open…" button was activated: the host should issue the
    /// same pick request the Enter key sends.
    pub open_requested: bool,
}

/// The whole viewer window's pointer- and keyboard-driven state.
///
/// A `Viewer` owns the current file view (or a status message shown in its
/// place), the shared "Open…" [`Button`], and the shared vertical
/// [`ScrollBar`], and keeps the bar's held [`ScrollModel`] in lock-step with
/// the authoritative one inside its [`ScrollView`] on every navigation,
/// resize, or drag. [`Viewer::on_pointer`] is the single pure entry point a
/// host feeds a translated pointer event into, so the routing here is
/// host-testable without a window; [`Viewer::render`] draws the whole
/// window from the shared controls, adding no painting of its own.
pub struct Viewer {
    /// The raw picked-file bytes currently shown, kept so a resize can
    /// re-wrap them to the new column count.
    content: Option<Vec<u8>>,
    /// The scrolled view over the current file's display lines, or `None`
    /// while a status message is shown in its place.
    scroll: Option<ScrollView>,
    /// The status message shown when no file is open.
    status: String,
    /// The shared "Open…" button that requests a new file pick.
    open_button: Button,
    /// The shared vertical scrollbar down the text area's trailing edge.
    scrollbar: ScrollBar,
}

impl Default for Viewer {
    fn default() -> Self {
        Self::new()
    }
}

impl Viewer {
    /// A freshly opened window: no file chosen yet, the scrollbar showing an
    /// empty, non-scrollable range.
    #[must_use]
    pub fn new() -> Self {
        Self {
            content: None,
            scroll: None,
            status: String::from("No file chosen."),
            open_button: Button::new(
                ButtonContent::Label(String::from("Open…")),
                ControlRole::Primary,
            ),
            scrollbar: ScrollBar::new(
                ScrollOrientation::Vertical,
                ScrollModel::new(ScrollRange::EMPTY, 1, 1),
            ),
        }
    }

    /// Show `text` in place of any file view (a cancelled pick, a refusal, or
    /// the initial prompt), clearing any open content and pinning the
    /// scrollbar to an empty, non-scrollable range.
    pub fn show_status(&mut self, text: impl Into<String>) {
        self.content = None;
        self.scroll = None;
        self.status = text.into();
        self.sync_scrollbar();
    }

    /// The status message currently shown, or `None` while a file is open.
    #[must_use]
    pub fn status(&self) -> Option<&str> {
        self.scroll.is_none().then_some(self.status.as_str())
    }

    /// Whether a file is currently open (a status message is showing otherwise).
    #[must_use]
    pub fn has_content(&self) -> bool {
        self.scroll.is_some()
    }

    /// The picked-file lines currently in view, or `None` while a status
    /// message is shown instead.
    #[must_use]
    pub fn visible_lines(&self) -> Option<&[String]> {
        self.scroll.as_ref().map(ScrollView::visible)
    }

    /// The scrolled view over the open file, or `None` while a status
    /// message is shown instead.
    #[must_use]
    pub fn scroll_view(&self) -> Option<&ScrollView> {
        self.scroll.as_ref()
    }

    /// Open `bytes` as the file view, wrapped to fit a `width_px` ×
    /// `height_px` window under the active theme and scale.
    pub fn open(
        &mut self,
        bytes: Vec<u8>,
        width_px: u32,
        height_px: u32,
        theme: &Theme,
        scale: Scale,
    ) {
        let rows = visible_rows_for(height_px, theme, scale);
        let cols = visible_cols_for(width_px, theme, scale);
        let lines = content_lines(&bytes, MAX_LINES, cols);
        self.scroll = Some(ScrollView::new(lines, rows));
        self.content = Some(bytes);
        self.sync_scrollbar();
    }

    /// Re-lay the current file out (if any) for a resized `width_px` ×
    /// `height_px` window, preserving the reader's place. A no-op while a
    /// status message is shown — its text does not depend on the window size.
    pub fn relayout(&mut self, width_px: u32, height_px: u32, theme: &Theme, scale: Scale) {
        let Some(bytes) = self.content.as_ref() else {
            return;
        };
        let rows = visible_rows_for(height_px, theme, scale);
        let cols = visible_cols_for(width_px, theme, scale);
        let lines = content_lines(bytes, MAX_LINES, cols);
        if let Some(scroll) = self.scroll.as_mut() {
            scroll.relayout(lines, rows);
        }
        self.sync_scrollbar();
    }

    /// Scroll one line toward the start, returning whether the view moved.
    pub fn line_up(&mut self) -> bool {
        self.navigate(ScrollView::line_up)
    }

    /// Scroll one line toward the end, returning whether the view moved.
    pub fn line_down(&mut self) -> bool {
        self.navigate(ScrollView::line_down)
    }

    /// Scroll one page toward the start, returning whether the view moved.
    pub fn page_up(&mut self) -> bool {
        self.navigate(ScrollView::page_up)
    }

    /// Scroll one page toward the end, returning whether the view moved.
    pub fn page_down(&mut self) -> bool {
        self.navigate(ScrollView::page_down)
    }

    /// Jump to the first line, returning whether the view moved.
    pub fn to_top(&mut self) -> bool {
        self.navigate(ScrollView::to_top)
    }

    /// Jump so the last lines are in view, returning whether the view moved.
    pub fn to_bottom(&mut self) -> bool {
        self.navigate(ScrollView::to_bottom)
    }

    /// Scroll by `ticks` wheel detents (see [`ScrollView::scroll_ticks`]),
    /// returning whether the view moved.
    pub fn scroll_ticks(&mut self, ticks: i32) -> bool {
        self.navigate(|scroll| scroll.scroll_ticks(ticks))
    }

    /// Apply a navigation step to the open file view (a no-op returning
    /// `false` while a status message is shown), resyncing the scrollbar
    /// when it moves.
    fn navigate(&mut self, step: impl FnOnce(&mut ScrollView) -> bool) -> bool {
        let Some(scroll) = self.scroll.as_mut() else {
            return false;
        };
        let moved = step(scroll);
        if moved {
            self.sync_scrollbar();
        }
        moved
    }

    /// Push the authoritative scroll model (or the empty range while no file
    /// is open) into the held scrollbar, so the bar never keeps an offset of
    /// its own.
    fn sync_scrollbar(&mut self) {
        let model = self.scroll.as_ref().map_or_else(
            || ScrollModel::new(ScrollRange::EMPTY, 1, 1),
            ScrollView::model,
        );
        self.scrollbar.set_model(model);
    }

    /// Draw the whole window — the header bar and its "Open…" button, the
    /// text area (the status message or the file's visible lines), and the
    /// scrollbar — into a fresh `width_px` × `height_px` surface. Returns
    /// `None` only when a surface cannot be allocated (the caller fails
    /// closed).
    #[must_use]
    pub fn render(
        &self,
        theme: &Theme,
        scale: Scale,
        width_px: u32,
        height_px: u32,
    ) -> Option<Surface> {
        let layout = ViewerLayout::for_window(width_px, height_px, theme, scale);
        let mut surface = Surface::new(width_px, height_px)?;
        let palette = theme.palette();
        surface.fill(palette.surface.into());
        surface.fill_rect(
            0,
            0,
            layout.header.width,
            layout.header.height,
            palette.surface_raised.into(),
        );

        let font = BitmapFont::console();
        self.open_button
            .render(&mut surface, layout.button, scale, theme, font);

        let text_surface = match self.scroll.as_ref() {
            Some(scroll) => render_lines(
                scroll.visible(),
                theme,
                layout.text.width,
                layout.text.height,
            ),
            None => render_status(&self.status, theme, layout.text.width, layout.text.height),
        };
        if let Some(text_surface) = text_surface {
            surface.blit(layout.text.left(), layout.text.top(), &text_surface);
        }

        self.scrollbar
            .render(&mut surface, layout.scrollbar, scale, theme);
        Some(surface)
    }

    /// Route one pointer event into the header button and the scrollbar, the
    /// single pure entry point a host feeds a translated
    /// [`tairix_input::InputEvent`] into.
    ///
    /// A control's visible change decides whether it actually needs a
    /// repaint — a mere hover move reports no action yet still changes the
    /// picture, so a naive "did an action fire" check would miss it. The
    /// button's [`ControlState`](tairix_controls::ControlState) is its whole
    /// visible interaction surface (its content and role never move under a
    /// pointer event), so comparing `state()` before and after states the
    /// repaint intent directly. The scrollbar's own render-equivalence
    /// [`PartialEq`] is compared instead, because the hovered end button,
    /// the drag in progress, and which end/track region is held are all
    /// visible on it yet, unlike the button, live outside its
    /// `ControlState` (documented on [`ScrollBar`]); it is `Copy`, so
    /// capturing it before the event costs nothing.
    #[must_use]
    pub fn on_pointer(
        &mut self,
        event: &InputEvent,
        width_px: u32,
        height_px: u32,
        theme: &Theme,
        scale: Scale,
    ) -> ViewerPointerOutcome {
        let layout = ViewerLayout::for_window(width_px, height_px, theme, scale);

        let button_state_before = self.open_button.state();
        let action = self.open_button.on_pointer(event, layout.button);
        let button_changed = self.open_button.state() != button_state_before;

        let bar_before = self.scrollbar;
        let scroll_action = self
            .scrollbar
            .on_pointer(event, layout.scrollbar, scale, theme);
        let bar_changed = self.scrollbar != bar_before;

        let mut changed = button_changed || bar_changed;
        if let Some(ScrollAction::ScrollTo { offset }) = scroll_action {
            if let Some(scroll) = self.scroll.as_mut() {
                if scroll.scroll_to(offset) {
                    changed = true;
                }
                self.scrollbar.set_model(scroll.model());
            }
        }

        ViewerPointerOutcome {
            changed,
            open_requested: action == Some(ButtonAction::Activated),
        }
    }
}

#[cfg(test)]
mod tests;
