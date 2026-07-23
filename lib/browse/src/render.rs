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
use alloc::vec::Vec;

use tairix_controls::scroll::{ScrollModel, ScrollOrientation, ScrollRange};
use tairix_controls::state::{ControlRole, ControlState, SelectionState};
use tairix_controls::{Card, IconButton, Panel, ScrollBar, TableCell, TableRow, Toolbar};
use tairix_font::BitmapFont;
use tairix_geometry::{Point, Rect, Scale};
use tairix_raster::Surface;
use tairix_theme::{Palette, Theme};

use crate::breadcrumb::{self, SEPARATOR};
use crate::browser::Browser;
use crate::chrome::{self, ToolbarCommand, ToolbarModel};
use crate::entry::{Entry, EntryKind};
use crate::format::{format_date, format_size};
use crate::layout::{GridView, ListView, ViewLayout, ViewMode};
use crate::properties::Properties;
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
    draw_toolbar(&mut surface, theme, font, browser, viewport);
    draw_path_bar(
        &mut surface,
        font,
        palette,
        browser,
        toolbar_height(theme),
        row_height,
    );

    let content = content_viewport(viewport, theme);
    match browser.view_mode() {
        ViewMode::List => draw_list(&mut surface, font, theme, browser, content),
        ViewMode::Grid => draw_grid(&mut surface, font, theme, browser, content),
    }
    draw_scrollbar(&mut surface, theme, browser, font, viewport);
    Some(surface)
}

/// Fill the top path bar and draw the current directory as a clickable
/// breadcrumb trail: the root crumb followed by one crumb per path component,
/// right-anchored so the current directory stays visible when the trail
/// overflows (`plans/NEW-FILEMANAGER.md` `FM4b`).
///
/// Ancestor crumbs are drawn in the accent colour to read as navigable; the
/// terminal crumb (the current directory) is drawn solid to read as "you are
/// here" and is inert; the separators between them are muted. The placement is
/// the shared [`breadcrumb::layout`], so the hit-test ([`crumb_at`]) resolves a
/// click to exactly the crumb painted here (§2.2).
fn draw_path_bar<S: DirectorySource>(
    surface: &mut Surface,
    font: BitmapFont,
    palette: &Palette,
    browser: &Browser<S>,
    top: u32,
    row_height: u32,
) {
    surface.fill_rect(
        0,
        top,
        surface.width(),
        row_height,
        palette.surface_raised.into(),
    );
    let crumbs = chrome::breadcrumbs(browser);
    let widths = crumb_widths(&crumbs, font);
    let sep_width = font.text_width(SEPARATOR);
    let placed = breadcrumb::layout(&widths, surface.width(), LABEL_PADDING, sep_width);
    let y = top.saturating_add(row_height.saturating_sub(font.glyph_height()) / 2);
    let y = to_i32(y);
    for (position, crumb) in placed.iter().zip(crumbs.iter()) {
        // The separator sits in the gap before every crumb but the first,
        // drawn from the previous crumb's right edge so it lands exactly in
        // the space the layout reserved for it.
        if position.index > 0 {
            let sep_x = position.x.saturating_sub(to_i32(sep_width));
            font.draw_text(
                surface,
                sep_x,
                y,
                SEPARATOR,
                palette.on_surface_muted.into(),
            );
        }
        let color = if crumb.is_current() {
            palette.on_surface
        } else {
            palette.accent
        };
        font.draw_text(surface, position.x, y, crumb.label(), color.into());
    }
}

/// The rendered pixel width of each crumb's label, in crumb order — the
/// per-crumb measurement the shared [`breadcrumb::layout`] places from.
fn crumb_widths(crumbs: &[chrome::Crumb], font: BitmapFont) -> Vec<u32> {
    crumbs
        .iter()
        .map(|crumb| font.text_width(crumb.label()))
        .collect()
}

/// The ancestor depth to [`navigate_to_depth`](Browser::navigate_to_depth) for
/// a click at window-local pixel `point`, or `None` when the click is not on a
/// navigable crumb — outside the path bar row, on a separator gap, on the
/// inert current crumb, or past a crumb clipped off the trail's left edge.
///
/// This mirrors the drawn path bar's own placement through the shared
/// [`breadcrumb::layout`], so a pointer-driven jump lands on exactly the crumb
/// the user clicked (§2.2). `theme` gives the path bar's vertical band (it sits
/// below the toolbar strip); the crumbs span the whole window width, not the
/// scrollbar-inset content area.
#[must_use]
pub fn crumb_at<S: DirectorySource>(
    browser: &Browser<S>,
    font: BitmapFont,
    theme: &Theme,
    viewport: Rect,
    point: Point,
) -> Option<usize> {
    let y = u32::try_from(point.y).ok()?;
    let top = toolbar_height(theme);
    if y < top || y >= top.saturating_add(row_height(font)) {
        return None;
    }
    let crumbs = chrome::breadcrumbs(browser);
    let widths = crumb_widths(&crumbs, font);
    let placed = breadcrumb::layout(
        &widths,
        viewport.width,
        LABEL_PADDING,
        font.text_width(SEPARATOR),
    );
    let index = breadcrumb::crumb_at(&placed, point.x, viewport.width)?;
    let crumb = crumbs.get(index)?;
    if crumb.is_current() {
        None
    } else {
        Some(crumb.depth())
    }
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
    let view = list_view(browser, font, theme, content);
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
    let view = grid_view(browser, font, theme, content);
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
    let header = chrome_height(font, theme);
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

/// The height in pixels of the command toolbar strip at the top of the window:
/// the theme's control height plus a gap above and below, scaled to physical
/// pixels. One definition so the drawn toolbar, the path bar's vertical offset,
/// the item area, and the hit-tests all agree on where each chrome band sits.
#[must_use]
pub fn toolbar_height(theme: &Theme) -> u32 {
    let metrics = theme.metrics();
    let gap = Scale::ONE.scale_length(metrics.control_gap);
    Scale::ONE
        .scale_length(metrics.control_height)
        .saturating_add(gap.saturating_mul(2))
        .max(1)
}

/// The total height reserved for the window chrome above the item area: the
/// command toolbar strip plus the breadcrumb path bar. This is the header the
/// item views lay out below and the top of the scrollbar gutter, so paint and
/// hit-test share one offset (§2.2).
#[must_use]
pub fn chrome_height(font: BitmapFont, theme: &Theme) -> u32 {
    toolbar_height(theme).saturating_add(row_height(font))
}

/// The group each toolbar command belongs to, so related commands read as a
/// unit with a quiet divider between groups: navigation, refresh, and the
/// view/sort presentation controls.
const fn toolbar_group(command: ToolbarCommand) -> u16 {
    match command {
        ToolbarCommand::Back | ToolbarCommand::Forward | ToolbarCommand::Up => 0,
        ToolbarCommand::Refresh => 1,
        ToolbarCommand::ToggleView | ToolbarCommand::Sort => 2,
    }
}

/// Build the drawn command toolbar for `model`: one [`IconButton`] per
/// [`chrome::TOOLBAR_COMMANDS`] entry, in order, each carrying the command's
/// glyph and rendered disabled (not hidden) when the model reports the command
/// is not currently actionable, so the toolbar's shape is stable.
fn build_toolbar(model: ToolbarModel) -> Toolbar {
    let mut toolbar = Toolbar::new();
    for &command in chrome::TOOLBAR_COMMANDS {
        let mut button = IconButton::new(command.icon(), ControlRole::Navigation);
        if !model.is_enabled(command) {
            button.set_state(ControlState::disabled());
        }
        toolbar = toolbar.with_icon(button, toolbar_group(command));
    }
    toolbar
}

/// Draw the command toolbar in the top strip: [`chrome::TOOLBAR_COMMANDS`] as
/// themed [`IconButton`]s over the [`ToolbarModel`], spanning the full window
/// width above the path bar. A disabled command reads muted rather than
/// vanishing (the model decides which).
fn draw_toolbar<S: DirectorySource>(
    surface: &mut Surface,
    theme: &Theme,
    font: BitmapFont,
    browser: &Browser<S>,
    viewport: Rect,
) {
    let toolbar = build_toolbar(ToolbarModel::for_browser(browser));
    let bounds = Rect::new(0, 0, viewport.width, toolbar_height(theme));
    toolbar.render(surface, bounds, Scale::ONE, theme, font);
}

/// The actionable toolbar command at window-local pixel `point`, or `None`
/// when the click is not on one — outside the toolbar strip, on a group
/// gutter, or on a command the [`ToolbarModel`] has disabled (fail closed: a
/// disabled tool does not act, §5.4). It mirrors the drawn toolbar's own
/// layout so a click resolves to exactly the tool [`render`] painted (§2.2).
#[must_use]
pub fn toolbar_command_at<S: DirectorySource>(
    browser: &Browser<S>,
    theme: &Theme,
    viewport: Rect,
    point: Point,
) -> Option<ToolbarCommand> {
    let model = ToolbarModel::for_browser(browser);
    let toolbar = build_toolbar(model);
    let bounds = Rect::new(0, 0, viewport.width, toolbar_height(theme));
    let index = toolbar.tool_at(bounds, Scale::ONE, theme, point)?;
    let command = *chrome::TOOLBAR_COMMANDS.get(index)?;
    model.is_enabled(command).then_some(command)
}

/// The [`ListView`] for `browser` at the given content viewport.
fn list_view<S: DirectorySource>(
    browser: &Browser<S>,
    font: BitmapFont,
    theme: &Theme,
    content: Rect,
) -> ListView {
    let row_height = row_height(font);
    ListView::new(
        content,
        row_height,
        chrome_height(font, theme),
        browser.entries().len(),
    )
}

/// The [`GridView`] for `browser` at the given content viewport.
fn grid_view<S: DirectorySource>(
    browser: &Browser<S>,
    font: BitmapFont,
    theme: &Theme,
    content: Rect,
) -> GridView {
    let (cell_width, cell_height, gap) = grid_metrics(font);
    GridView::new(
        content,
        cell_width,
        cell_height,
        gap,
        chrome_height(font, theme),
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
        ViewMode::List => ViewLayout::List(list_view(browser, font, theme, content)),
        ViewMode::Grid => ViewLayout::Grid(grid_view(browser, font, theme, content)),
    }
}

/// The number of label/value rows [`properties_rows`] produces — the field
/// count the Properties overlay is sized to show.
pub const PROPERTY_ROW_COUNT: usize = 8;

/// The labelled metadata fields the Properties overlay shows for `props`, in
/// display order: kind, size (apparent + on-disk), permissions (symbolic +
/// octal), owner, and the four timestamps.
///
/// One definition so the drawn panel and its tests agree on exactly which
/// fields appear and how each reads (§2.2). Every value comes straight from
/// the [`Properties`] model — itself taken straight from `fs_stat` — so a
/// timestamp the backing does not keep renders blank rather than a fabricated
/// wall time, and no field is invented.
#[must_use]
pub fn properties_rows(props: &Properties) -> Vec<(&'static str, String)> {
    let mut size = props.size_display();
    size.push_str(" (");
    size.push_str(&props.allocated_display());
    size.push_str(" on disk)");

    let mut permissions = props.permissions();
    permissions.push_str(" (");
    permissions.push_str(&props.mode_octal());
    permissions.push(')');

    let owner = alloc::format!("uid {} / gid {}", props.uid(), props.gid());

    vec![
        ("Kind", String::from(props.kind_label())),
        ("Size", size),
        ("Permissions", permissions),
        ("Owner", owner),
        ("Created", props.created_display()),
        ("Modified", props.modified_display()),
        ("Accessed", props.accessed_display()),
        ("Changed", props.changed_display()),
    ]
}

/// The centered bounds of the Properties overlay [`Panel`] within `viewport`,
/// sized to comfortably show the [`properties_rows`] fields (a header plus one
/// line per field with a top and bottom margin) and clamped to the window so a
/// small window still yields a drawable — if clipped — panel rather than a
/// panic.
#[must_use]
pub fn properties_panel_rect(viewport: Rect, font: BitmapFont, theme: &Theme) -> Rect {
    let line = row_height(font);
    let title = Scale::ONE
        .scale_length(theme.metrics().title_bar_height)
        .max(1);
    let rows = u32::try_from(PROPERTY_ROW_COUNT).unwrap_or(u32::MAX);
    let content = line.saturating_mul(rows.saturating_add(2));
    let height = title.saturating_add(content).min(viewport.height.max(1));
    let width = viewport
        .width
        .saturating_mul(4)
        .checked_div(5)
        .unwrap_or(viewport.width)
        .clamp(1, viewport.width.max(1));
    let x = viewport
        .origin
        .x
        .saturating_add(to_i32(viewport.width.saturating_sub(width) / 2));
    let y = viewport
        .origin
        .y
        .saturating_add(to_i32(viewport.height.saturating_sub(height) / 2));
    Rect::new(x, y, width, height)
}

/// Draw the Properties overlay for `props` centered in `viewport`: a [`Panel`]
/// titled with the node's name, its labelled metadata fields drawn as
/// muted-label / solid-value rows in the panel's content area.
///
/// The overlay is drawn on top of the current view. Every blit clips, so a
/// window too small for the whole panel simply shows what fits rather than
/// panicking. It reads only the already-authorised [`Properties`] and draws —
/// it performs no I/O and holds no authority (§4, §5.4).
pub fn draw_properties(
    surface: &mut Surface,
    props: &Properties,
    theme: &Theme,
    font: BitmapFont,
    viewport: Rect,
) {
    let bounds = properties_panel_rect(viewport, font, theme);
    let panel = Panel::new(props.name());
    panel.render(surface, bounds, Scale::ONE, theme, font);
    let Some(content) = panel.content_rect(bounds, Scale::ONE, theme) else {
        return;
    };
    let palette = theme.palette();
    let rows = properties_rows(props);
    // Start the value column past the widest label plus a gap, so the values
    // line up in one column whatever the labels' widths.
    let label_col = rows
        .iter()
        .map(|(label, _)| font.text_width(label))
        .max()
        .unwrap_or(0);
    let gap = font.text_width("  ").max(LABEL_PADDING);
    let line = row_height(font);
    let left = content.left().saturating_add(to_i32(LABEL_PADDING));
    let value_x = left.saturating_add(to_i32(label_col.saturating_add(gap)));
    let bottom = content.top().saturating_add(to_i32(content.height));
    let mut y = content.top().saturating_add(to_i32(ROW_PADDING));
    for (label, value) in &rows {
        if y >= bottom {
            break;
        }
        font.draw_text(surface, left, y, label, palette.on_surface_muted.into());
        font.draw_text(surface, value_x, y, value, palette.on_surface.into());
        y = y.saturating_add(to_i32(line));
    }
}

/// Saturating `u32` → `i32`.
fn to_i32(value: u32) -> i32 {
    i32::try_from(value).unwrap_or(i32::MAX)
}
