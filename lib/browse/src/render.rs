//! Painting the browser's current directory into a pixel [`Surface`].
//!
//! [`render`] turns a [`Browser`]'s path and entries into a premultiplied-alpha
//! [`Surface`] sized to the app's content viewport, using the active theme's
//! [`Palette`] for the path bar and the shared `lib/controls` collection
//! controls for the items. The surface is the window manager's to place and
//! round: the browser paints a *rectangular* buffer and the compositor applies
//! any corner radius through its single anti-aliased rounded-corner path. There
//! is no rounding here.
//!
//! The top row is a path bar showing the current directory; below it the
//! current directory is drawn in whichever [`ViewMode`] the browser holds — a
//! column of full-width [`TableRow`]s (list) or a wrapped grid of [`Card`]
//! tiles (grid) — over the one shared selection state, with a drawn
//! [`ScrollBar`] in a reserved right-edge gutter. Painting through the same
//! collection controls the trusted picker uses keeps the two views one coherent
//! themed surface (§2.2). The visible window, each item's rectangle, the scroll
//! offset, and the scrollbar geometry all come from the one shared
//! [`ViewLayout`], so the pointer hit-test ([`entry_index_at`]) and the paint
//! can never disagree.
//!
//! Every length saturates and every blit clips, so a degenerate viewport paints
//! nothing rather than panicking.

use alloc::string::String;
use alloc::vec;

use tairix_controls::scroll::{ScrollModel, ScrollOrientation, ScrollRange};
use tairix_controls::state::{ControlState, SelectionState};
use tairix_controls::{Card, ScrollBar, TableCell, TableRow};
use tairix_font::BitmapFont;
use tairix_geometry::{Point, Rect, Scale};
use tairix_raster::{Color, Surface};
use tairix_theme::{Palette, Theme};

use crate::browser::Browser;
use crate::entry::{Entry, EntryKind};
use crate::format::{format_date, format_size};
use crate::layout::{GridView, ListView, ViewLayout, ViewMode};
use crate::source::DirectorySource;

/// Padding in pixels between the path bar's edge and its label text.
const LABEL_PADDING: u32 = 4;

/// Vertical padding above and below a row's glyphs.
const ROW_PADDING: u32 = 2;

/// Relative widths of the list view's name, size, and modified columns.
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

    let content = content_viewport(viewport, theme);
    match browser.view_mode() {
        ViewMode::List => draw_list(&mut surface, font, theme, browser, content),
        ViewMode::Grid => draw_grid(&mut surface, font, theme, browser, content),
    }
    draw_scrollbar(&mut surface, theme, browser, font, viewport);
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

/// Draw the visible list rows below the path bar as shared [`TableRow`]s,
/// giving the selected entry the row chrome's selection state.
fn draw_list<S: DirectorySource>(
    surface: &mut Surface,
    font: BitmapFont,
    theme: &Theme,
    browser: &Browser<S>,
    content: Rect,
) {
    let view = list_view(browser, font, content);
    let visible = view.visible_rows();
    if visible == 0 {
        return;
    }
    let offset = browser.scroll_offset();
    let first = view.first_visible(offset);
    let selected = browser.selected_index();
    for (index, entry) in browser
        .entries()
        .iter()
        .enumerate()
        .skip(first)
        .take(visible)
    {
        let Some(bounds) = view.row_rect(offset, index) else {
            continue;
        };
        let row = entry_row(entry, selected == Some(index));
        row.render(surface, bounds, Scale::ONE, theme, font, &COLUMNS);
    }
}

/// Draw the visible icon-grid tiles below the path bar as shared [`Card`]s,
/// giving the selected entry the card's selection state.
fn draw_grid<S: DirectorySource>(
    surface: &mut Surface,
    font: BitmapFont,
    theme: &Theme,
    browser: &Browser<S>,
    content: Rect,
) {
    let view = grid_view(browser, font, content);
    let columns = view.columns();
    let visible_rows = view.visible_rows();
    if columns == 0 || visible_rows == 0 {
        return;
    }
    let offset = browser.scroll_offset();
    let first_row = view.first_visible(offset);
    let selected = browser.selected_index();
    let entries = browser.entries();
    let start = first_row.saturating_mul(columns);
    let end = first_row
        .saturating_add(visible_rows)
        .saturating_mul(columns)
        .min(entries.len());
    for index in start..end {
        let Some(entry) = entries.get(index) else {
            break;
        };
        let Some(bounds) = view.cell_rect(offset, index) else {
            continue;
        };
        grid_tile(entry, selected == Some(index)).render(surface, bounds, Scale::ONE, theme, font);
    }
}

/// Draw the vertical [`ScrollBar`] in the reserved right-edge gutter, spanning
/// the item area below the path bar. A viewport with no room for the gutter
/// (or with no scrollable content) simply draws nothing there.
fn draw_scrollbar<S: DirectorySource>(
    surface: &mut Surface,
    theme: &Theme,
    browser: &Browser<S>,
    font: BitmapFont,
    viewport: Rect,
) {
    let gutter = gutter_width(theme, viewport.width);
    let header = row_height(font);
    if gutter == 0 || viewport.height <= header {
        return;
    }
    let content = content_viewport(viewport, theme);
    let bounds = Rect::new(
        i32::try_from(content.width).unwrap_or(i32::MAX),
        i32::try_from(header).unwrap_or(i32::MAX),
        gutter,
        viewport.height.saturating_sub(header),
    );
    let bar = ScrollBar::new(
        ScrollOrientation::Vertical,
        scroll_model(browser, font, theme, viewport),
    );
    bar.render(surface, bounds, Scale::ONE, theme);
}

/// Build the [`TableRow`] for one list entry: a leading name cell (a directory
/// suffixed with `/`), a trailing numeric size cell (blank for a directory or
/// bundle, which carry no meaningful byte size), and a modified-date cell.
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

/// Build the [`Card`] tile for one grid entry: the entry's file-type icon
/// above its label, carrying the shared selection state when selected. The
/// icon is the shared [`icon_for`](crate::icon::icon_for) classification — a
/// display hint only, decided once here so the manager and picker draw the
/// same glyph for the same entry (§2.2).
fn grid_tile(entry: &Entry, selected: bool) -> Card {
    let mut state = ControlState::idle();
    if selected {
        state.selection = SelectionState::Selected;
    }
    Card::new(entry_label(entry))
        .with_icon(crate::icon::icon_for(entry))
        .with_state(state)
}

/// The name shown for an entry: a directory is suffixed with `/` so its kind
/// reads at a glance in the list view (whose rows carry no icon), and stays a
/// familiar cue beneath the grid tile's folder glyph.
fn entry_label(entry: &Entry) -> String {
    let mut label = String::from(entry.name());
    if entry.is_directory() {
        label.push('/');
    }
    label
}

/// Height in pixels of one rendered list row — the path bar and every entry
/// row alike, derived from `font` exactly as [`render`] draws them, so
/// hit-testing and painting can never disagree.
#[must_use]
pub fn row_height(font: BitmapFont) -> u32 {
    font.glyph_height()
        .saturating_add(ROW_PADDING.saturating_mul(2))
}

/// The width of the reserved scrollbar gutter for a `viewport_width`-pixel
/// window: the theme's scrollbar breadth, clamped so it never exceeds the
/// window (a window too narrow for the gutter simply has none).
fn gutter_width(theme: &Theme, viewport_width: u32) -> u32 {
    Scale::ONE
        .scale_length(theme.metrics().scrollbar_breadth)
        .max(1)
        .min(viewport_width)
}

/// The content viewport (the window minus the reserved scrollbar gutter). The
/// item views lay out within this, so no item ever underlaps the scrollbar.
fn content_viewport(viewport: Rect, theme: &Theme) -> Rect {
    let gutter = gutter_width(theme, viewport.width);
    Rect::new(
        viewport.origin.x,
        viewport.origin.y,
        viewport.width.saturating_sub(gutter),
        viewport.height,
    )
}

/// The dimensions of one grid tile and the gap between tiles, derived from the
/// render font so they scale with the theme's UI size (the window is not
/// DPI-scaled today, so the font is the density proxy).
fn grid_metrics(font: BitmapFont) -> (u32, u32, u32) {
    let glyph = font.glyph_height().max(1);
    let cell_width = glyph.saturating_mul(6).max(48);
    let cell_height = glyph.saturating_mul(5).max(48);
    let gap = (glyph / 2).max(2);
    (cell_width, cell_height, gap)
}

/// The [`ListView`] for `browser` at the given content viewport.
fn list_view<S: DirectorySource>(
    browser: &Browser<S>,
    font: BitmapFont,
    content: Rect,
) -> ListView {
    let row_height = row_height(font);
    ListView::new(content, row_height, row_height, browser.entries().len())
}

/// The [`GridView`] for `browser` at the given content viewport.
fn grid_view<S: DirectorySource>(
    browser: &Browser<S>,
    font: BitmapFont,
    content: Rect,
) -> GridView {
    let (cell_width, cell_height, gap) = grid_metrics(font);
    GridView::new(
        content,
        cell_width,
        cell_height,
        gap,
        row_height(font),
        browser.entries().len(),
    )
}

/// The scroll model the drawn [`ScrollBar`] and the wheel share: the active
/// view's clamped [`ScrollRange`], stepping one line at a time and one visible
/// page per page gesture. `theme` supplies the scrollbar gutter width so the
/// model measures the same content viewport the renderer draws (§2.2).
#[must_use]
pub fn scroll_model<S: DirectorySource>(
    browser: &Browser<S>,
    font: BitmapFont,
    theme: &Theme,
    viewport: Rect,
) -> ScrollModel {
    let view = view_layout_for(browser, font, theme, viewport);
    scroll_model_for(&view, browser.scroll_offset())
}

/// The scroll model from a resolved view layout and desired offset.
fn scroll_model_for(view: &ViewLayout, offset: u64) -> ScrollModel {
    let range: ScrollRange = view.scroll_range(offset);
    let page = u64::try_from(view.visible_rows().max(1)).unwrap_or(1);
    ScrollModel::new(range, 1, page)
}

/// Move the scroll offset by `delta` lines (positive scrolls toward the end),
/// routed through the shared [`ScrollModel`] so it clamps exactly like the
/// drawn scrollbar. Returns `true` when the offset actually moved.
pub fn scroll_lines<S: DirectorySource>(
    browser: &mut Browser<S>,
    font: BitmapFont,
    theme: &Theme,
    viewport: Rect,
    delta: i64,
) -> bool {
    let view = view_layout_for(browser, font, theme, viewport);
    let model = scroll_model_for(&view, browser.scroll_offset());
    let moved = model.scroll_by(delta);
    let changed = moved.offset() != model.offset();
    browser.set_scroll_offset(moved.offset());
    changed
}

/// Adjust the scroll offset so the current selection is visible, moving the
/// least (a no-op when it already is). A caller runs this after a
/// selection-changing key or a directory change, before it repaints.
pub fn reveal_selection<S: DirectorySource>(
    browser: &mut Browser<S>,
    font: BitmapFont,
    theme: &Theme,
    viewport: Rect,
) {
    let view = view_layout_for(browser, font, theme, viewport);
    let revealed = view.reveal(browser.scroll_offset(), browser.selected_index());
    browser.set_scroll_offset(revealed);
}

/// The index of the entry at window-local pixel `point` for the browser's
/// current view and scroll offset, or `None` for the path bar, an empty gap,
/// the scrollbar gutter, and any coordinate outside the item area.
///
/// This mirrors [`render`]'s own layout through the shared [`ViewLayout`], so a
/// pointer-driven view resolves a click to exactly the item the user saw —
/// never a re-derived guess. `theme` supplies the same scrollbar gutter width
/// the renderer reserved.
#[must_use]
pub fn entry_index_at<S: DirectorySource>(
    browser: &Browser<S>,
    font: BitmapFont,
    theme: &Theme,
    viewport: Rect,
    point: Point,
) -> Option<usize> {
    let x = u32::try_from(point.x).ok()?;
    let y = u32::try_from(point.y).ok()?;
    let view = view_layout_for(browser, font, theme, viewport);
    view.index_at(browser.scroll_offset(), x, y)
}

/// The window-local pixel rectangle the browser's currently selected item is
/// drawn in, or `None` when nothing is selected or the selection is scrolled
/// out of view.
///
/// This is [`render`]'s own layout for the selected entry, through the shared
/// [`ViewLayout`], so an overlay drawn there — the in-place rename editor —
/// sits exactly over the item the renderer painted (§2.2). A caller reveals
/// the selection first (via [`reveal_selection`]) if it needs the rect to be
/// on screen.
#[must_use]
pub fn selection_rect<S: DirectorySource>(
    browser: &Browser<S>,
    font: BitmapFont,
    theme: &Theme,
    viewport: Rect,
) -> Option<Rect> {
    let selected = browser.selected_index()?;
    let view = view_layout_for(browser, font, theme, viewport);
    view.item_rect(browser.scroll_offset(), selected)
}

/// The resolved view layout for `browser` at `viewport` — the one dispatch the
/// scroll helpers and the pointer hit-test share, laid out within the same
/// content viewport (window minus the scrollbar gutter) the renderer uses.
fn view_layout_for<S: DirectorySource>(
    browser: &Browser<S>,
    font: BitmapFont,
    theme: &Theme,
    viewport: Rect,
) -> ViewLayout {
    let content = content_viewport(viewport, theme);
    match browser.view_mode() {
        ViewMode::List => ViewLayout::List(list_view(browser, font, content)),
        ViewMode::Grid => ViewLayout::Grid(grid_view(browser, font, content)),
    }
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
