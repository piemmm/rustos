//! Painting the browser's current directory into a pixel [`Surface`].
//!
//! [`render`] turns a [`Browser`]'s path and entries into a premultiplied-alpha
//! [`Surface`] sized to the app's content viewport, using the active theme's
//! [`Palette`] for the path bar and the shared `lib/controls` collection
//! controls for the entry list. The surface is the window manager's to place
//! and round: the browser paints a *rectangular* buffer and the compositor
//! applies any corner radius through its single anti-aliased rounded-corner
//! path. There is no rounding here.
//!
//! The top row is a path bar showing the current directory; the rows below it
//! are the entries, each drawn as a shared [`TableRow`] with an aligned
//! name/size/modified column layout, the selected entry carrying the row
//! chrome's selection state. Painting the list through the same collection
//! controls the trusted picker uses keeps the two views one coherent themed
//! surface (§2.2). The visible window, each row's rectangle, and the scroll
//! anchor come from the one shared [`ListView`] geometry, so the pointer
//! hit-test ([`entry_index_at`]) and the paint can never disagree.
//!
//! When there are more entries than fit, the list scrolls so the selected
//! entry stays visible. Every length saturates and every blit clips, so a
//! degenerate viewport paints nothing rather than panicking.

use alloc::string::String;
use alloc::vec;

use tairix_controls::{TableCell, TableRow};
use tairix_font::BitmapFont;
use tairix_geometry::{Rect, Scale};
use tairix_raster::{Color, Surface};
use tairix_theme::{Palette, Theme};

use crate::browser::Browser;
use crate::entry::{Entry, EntryKind};
use crate::format::{format_date, format_size};
use crate::layout::ListView;
use crate::source::DirectorySource;

/// Padding in pixels between the path bar's edge and its label text.
const LABEL_PADDING: u32 = 4;

/// Vertical padding above and below a row's glyphs.
const ROW_PADDING: u32 = 2;

/// Relative widths of the entry list's name, size, and modified columns.
///
/// [`TableRow::render`] scales these proportionally into the actual content
/// width, so they act as weights independent of the window size: the name
/// column dominates, with narrower size and date columns beside it. Defining
/// them once here keeps the column layout a single definition (§2.2).
const COLUMNS: [u32; 3] = [240, 96, 128];

/// Paint `browser`'s current directory into a [`Surface`] the size of
/// `viewport`, using `theme`'s palette and the shared collection controls.
///
/// Only `viewport`'s dimensions are used; the window manager places the
/// returned surface at `viewport`'s origin. Returns `None` only when those
/// dimensions cannot be allocated (a surface that could never exist), so the
/// caller fails closed rather than panicking.
#[must_use]
pub fn render<S: DirectorySource>(
    browser: &Browser<S>,
    theme: &Theme,
    font: BitmapFont,
    viewport: Rect,
) -> Option<Surface> {
    let row_height = row_height(font);
    let mut surface = Surface::new(viewport.width, viewport.height)?;
    let palette = theme.palette();

    surface.fill(palette.surface.into());
    draw_path_bar(&mut surface, font, palette, &browser.path(), row_height);
    draw_entries(&mut surface, font, theme, browser, row_height);
    Some(surface)
}

/// Fill the top path bar and draw the current directory's path into it.
fn draw_path_bar(
    surface: &mut Surface,
    font: BitmapFont,
    palette: &Palette,
    path: &str,
    row_height: u32,
) {
    surface.fill_rect(
        0,
        0,
        surface.width(),
        row_height,
        palette.surface_raised.into(),
    );
    draw_label(
        surface,
        font,
        0,
        row_height,
        path,
        palette.on_surface.into(),
    );
}

/// Draw the visible entry rows below the path bar as shared [`TableRow`]s,
/// giving the selected entry the row chrome's selection state and scrolling so
/// it stays on screen.
fn draw_entries<S: DirectorySource>(
    surface: &mut Surface,
    font: BitmapFont,
    theme: &Theme,
    browser: &Browser<S>,
    row_height: u32,
) {
    let viewport = Rect::new(0, 0, surface.width(), surface.height());
    let entries = browser.entries();
    let view = ListView::new(viewport, row_height, row_height, entries.len());
    let visible = view.visible_rows();
    if visible == 0 {
        return;
    }
    let selected = browser.selected_index();
    let first = view.first_visible(selected);

    for (index, entry) in entries.iter().enumerate().skip(first).take(visible) {
        let Some(bounds) = view.row_rect(selected, index) else {
            continue;
        };
        let row = entry_row(entry, selected == Some(index));
        row.render(surface, bounds, Scale::ONE, theme, font, &COLUMNS);
    }
}

/// Build the [`TableRow`] for one entry: a leading name cell (a directory
/// suffixed with `/`), a trailing numeric size cell (blank for a directory or
/// bundle, which carry no meaningful byte size), and a modified-date cell.
///
/// A `selected` row is given the shared selection state so the row chrome
/// draws it with the accent selection rail — the one selection look every
/// collection view shares, not a browser-private highlight.
fn entry_row(entry: &Entry, selected: bool) -> TableRow {
    let size = if matches!(entry.kind(), EntryKind::File) {
        format_size(entry.size())
    } else {
        String::new()
    };
    let cells = vec![
        TableCell::new(entry_label(entry)),
        TableCell::numeric(size),
        TableCell::new(format_date(entry.modified())),
    ];
    let mut row = TableRow::new(cells);
    row.set_selected(selected);
    row
}

/// The name shown for an entry: a directory is suffixed with `/` so its kind
/// reads at a glance even before file-type icons (a later stage) are drawn.
fn entry_label(entry: &Entry) -> String {
    let mut label = String::from(entry.name());
    if entry.is_directory() {
        label.push('/');
    }
    label
}

/// Height in pixels of one rendered row — the path bar and every entry
/// row alike, derived from `font` exactly as [`render`] draws them, so
/// hit-testing and painting can never disagree.
///
/// The caller passes the same `font` it renders with (the desktop resolves it
/// from the theme's font size); the browser never assumes a size of its own.
#[must_use]
pub fn row_height(font: BitmapFont) -> u32 {
    font.glyph_height()
        .saturating_add(ROW_PADDING.saturating_mul(2))
}

/// The index of the entry drawn at the view-local pixel row `y` in a
/// viewport `viewport_height` pixels tall, or `None` for the path bar,
/// the empty space below the last entry, and any coordinate outside the
/// viewport.
///
/// This mirrors [`render`]'s own layout through the shared [`ListView`]
/// geometry (the path-bar offset, the row height, and the scroll anchor), so
/// a pointer-driven view resolves a click to exactly the entry the user
/// saw — never a re-derived guess. Only the vertical coordinate matters for
/// the list view, so no horizontal position is taken.
#[must_use]
pub fn entry_index_at<S: DirectorySource>(
    browser: &Browser<S>,
    font: BitmapFont,
    viewport_height: u32,
    y: u32,
) -> Option<usize> {
    let row = row_height(font);
    // Only the viewport height feeds the vertical hit-test; the width is
    // irrelevant to which row a `y` falls in, so a zero-width viewport is fine.
    let view = ListView::new(
        Rect::new(0, 0, 0, viewport_height),
        row,
        row,
        browser.entries().len(),
    );
    view.row_index_at(browser.selected_index(), y)
}

/// Draw `text` leading-aligned and vertically centred within the row spanning
/// the full surface width at top `y` with height `row_height`. Text wider than
/// the row is truncated to what fits.
fn draw_label(
    surface: &mut Surface,
    font: BitmapFont,
    y: u32,
    row_height: u32,
    text: &str,
    color: Color,
) {
    if text.is_empty() {
        return;
    }
    let usable = surface
        .width()
        .saturating_sub(LABEL_PADDING.saturating_mul(2));
    let fitted = font.truncate_to_width(text, usable);
    if fitted.is_empty() {
        return;
    }
    let y_offset = row_height.saturating_sub(font.glyph_height()) / 2;
    font.draw_text(
        surface,
        to_i32(LABEL_PADDING),
        to_i32(y.saturating_add(y_offset)),
        fitted,
        color,
    );
}

/// Saturating `u32` → `i32`.
fn to_i32(value: u32) -> i32 {
    i32::try_from(value).unwrap_or(i32::MAX)
}
