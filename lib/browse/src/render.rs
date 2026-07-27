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

use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;

use tairix_controls::button::{Button, ButtonContent};
use tairix_controls::decision::Dialog;
use tairix_controls::scroll::{ScrollModel, ScrollRange};
use tairix_controls::state::{
    ActivityState, AuthorityState, ControlRole, ControlState, SelectionState,
};
use tairix_controls::text::TextField;
use tairix_controls::value::Progress;
use tairix_controls::{
    Card, Checkbox, IconButton, Menu, MenuItem, Panel, ScrollAction, ScrollBar, ScrollPart,
    TableCell, TableRow, Toolbar,
};
use tairix_font::BitmapFont;
use tairix_geometry::{Point, Rect, Scale};
use tairix_input::{InputEvent, PointerButton};
use tairix_raster::Surface;
use tairix_theme::{Palette, Theme};

use crate::breadcrumb::{self, SEPARATOR};
use crate::browser::Browser;
use crate::chrome::{
    self, ContextCommand, ContextMenuModel, ManagerTool, ManagerToolModel, ToolbarCommand,
    ToolbarModel,
};
use crate::delete::DeletePlan;
use crate::entry::{Entry, EntryKind};
use crate::format::{format_date, format_size};
use crate::layout::{GridView, ListView, ViewLayout, ViewMode};
use crate::open_with::AppAssociation;
use crate::progress::ProgressModel;
use crate::properties::Properties;
use crate::source::DirectorySource;
use crate::trash::DeleteDisposition;

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
/// `tools` are the manager-only write tools ([`chrome::MANAGER_TOOLS`]) to draw
/// on the toolbar after the shared read-only commands: the file manager passes
/// them, the trusted read-only picker passes an empty slice so it never draws a
/// write tool. The read-only commands keep their positions regardless (the
/// toolbar left-packs fixed-width buttons), so a click on a read-only command
/// resolves identically for both consumers. `tool_model` supplies each write
/// tool's enable state (the file manager's [`ManagerToolModel`]; the picker's
/// [`ManagerToolModel::none`], since it draws none): a disabled tool renders
/// muted, never hidden.
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
    tools: &[ManagerTool],
    tool_model: ManagerToolModel,
) -> Option<Surface> {
    let row_height = row_height(font);
    let mut surface = Surface::new(viewport.width, viewport.height)?;
    let palette = theme.palette();

    surface.fill(palette.surface.into());
    draw_toolbar(
        &mut surface,
        theme,
        font,
        browser,
        viewport,
        tools,
        tool_model,
    );
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
    let Some(bounds) = scrollbar_bounds(theme, font, viewport) else {
        return;
    };
    // Draw the browser's own interactive bar (its live hover/drag/held state),
    // with its model re-synced from the current geometry so the thumb size and
    // position match the listing exactly. The bar is `Copy`, so this reflects
    // the live interaction state without disturbing the stored offset owner.
    let mut bar: ScrollBar = *browser.scrollbar();
    bar.set_model(scroll_model(browser, font, theme, viewport));
    bar.render(surface, bounds, Scale::ONE, theme);
}

/// The screen rectangle (window-local) the vertical [`ScrollBar`] occupies: the
/// reserved right-edge gutter spanning the item area below the path bar, or
/// `None` when the window is too narrow for a gutter or too short for any item
/// area. This is the exact geometry the drawn scrollbar paints into (and that
/// [`scroll_pointer`] hit-tests against), so a pointer hit-test and the drawn
/// bar can never disagree (§2.2).
#[must_use]
pub fn scrollbar_bounds(theme: &Theme, font: BitmapFont, viewport: Rect) -> Option<Rect> {
    let gutter = gutter_width(theme, viewport.width);
    let header = chrome_height(font, theme);
    if gutter == 0 || viewport.height <= header {
        return None;
    }
    let content = content_viewport(viewport, theme);
    Some(Rect::new(
        i32::try_from(content.width).unwrap_or(i32::MAX),
        i32::try_from(header).unwrap_or(i32::MAX),
        gutter,
        viewport.height.saturating_sub(header),
    ))
}

/// Route a pointer `event` (a primary press, release, or a motion) to the
/// browser's interactive scrollbar, returning `Some(repaint)` when the bar
/// consumed it (so the caller does not also treat the press as a click in the
/// content) and `None` when the pointer had nothing to do with the bar (the
/// caller handles it as content input).
///
/// The bar owns the interaction the press started: a press on an end button or
/// track region steps the offset once, a press on the thumb captures a drag,
/// and the subsequent motions and the release are routed here (the window
/// manager's client pointer grab delivers them) until the release ends it. A
/// hover over the bar is consumed so the bar can brighten. The requested
/// offset is applied through [`Browser::set_scroll_offset`], keeping the
/// browser the one owner of the authoritative offset. `event` must carry the
/// window-local pointer position (a press/release is preceded here by a
/// synthetic move to that position, exactly as the window controls are fed).
pub fn scroll_pointer<S: DirectorySource>(
    browser: &mut Browser<S>,
    font: BitmapFont,
    theme: &Theme,
    viewport: Rect,
    point: Point,
    event: &InputEvent,
) -> Option<bool> {
    let bounds = scrollbar_bounds(theme, font, viewport)?;
    let model = scroll_model(browser, font, theme, viewport);
    let bar = browser.scrollbar_mut();
    bar.set_model(model);
    // Position the bar at this event before applying the action, so a press
    // knows which part it landed on and a drag reads the current point (the
    // press/release events carry no position of their own).
    let synth = bar.on_pointer(
        &InputEvent::PointerMoved { to: point },
        bounds,
        Scale::ONE,
        theme,
    );
    let pressing_before = bar.is_pressing();
    let on_bar = bar.part_at(bounds, point, Scale::ONE, theme) != ScrollPart::Outside;
    let (consumed, action) = match event {
        InputEvent::PointerPressed {
            button: PointerButton::Primary,
        } => (on_bar, bar.on_pointer(event, bounds, Scale::ONE, theme)),
        InputEvent::PointerReleased {
            button: PointerButton::Primary,
        } => (
            pressing_before,
            bar.on_pointer(event, bounds, Scale::ONE, theme),
        ),
        InputEvent::PointerMoved { .. } => (pressing_before || on_bar, synth),
        _ => (false, None),
    };
    if !consumed {
        return None;
    }
    match action {
        Some(ScrollAction::ScrollTo { offset }) => {
            browser.set_scroll_offset(offset);
            Some(true)
        }
        // A consumed press on the thumb (drag start) or a hover moves nothing
        // yet but changes the bar's drawn state, so the caller repaints.
        None => Some(true),
    }
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

/// The toolbar group the manager-only write tools sit in — after the read-only
/// navigation/refresh/view groups (0..=2), so a quiet divider sets them apart.
const MANAGER_TOOL_GROUP: u16 = 3;

/// Build the drawn command toolbar for `model`: one [`IconButton`] per
/// [`chrome::TOOLBAR_COMMANDS`] entry, in order, each carrying the command's
/// glyph and rendered disabled (not hidden) when the model reports the command
/// is not currently actionable, so the toolbar's shape is stable. The
/// manager-only write `tools` follow the read-only commands (a picker passes an
/// empty slice), so their [`Toolbar`] indices are
/// `chrome::TOOLBAR_COMMANDS.len() + i`.
fn build_toolbar(
    model: ToolbarModel,
    tools: &[ManagerTool],
    tool_model: ManagerToolModel,
) -> Toolbar {
    let mut toolbar = Toolbar::new();
    for &command in chrome::TOOLBAR_COMMANDS {
        let mut button = IconButton::new(command.icon(), ControlRole::Navigation);
        if !model.is_enabled(command) {
            button.set_state(ControlState::disabled());
        }
        toolbar = toolbar.with_icon(button, toolbar_group(command));
    }
    for &tool in tools {
        let mut button = IconButton::new(tool.icon(), ControlRole::Neutral);
        if !tool_model.is_enabled(tool) {
            button.set_state(ControlState::disabled());
        }
        toolbar = toolbar.with_icon(button, MANAGER_TOOL_GROUP);
    }
    toolbar
}

/// Draw the command toolbar in the top strip: [`chrome::TOOLBAR_COMMANDS`] then
/// the manager-only write `tools`, as themed [`IconButton`]s over the
/// [`ToolbarModel`], spanning the full window width above the path bar. A
/// disabled command reads muted rather than vanishing (the model decides
/// which).
fn draw_toolbar<S: DirectorySource>(
    surface: &mut Surface,
    theme: &Theme,
    font: BitmapFont,
    browser: &Browser<S>,
    viewport: Rect,
    tools: &[ManagerTool],
    tool_model: ManagerToolModel,
) {
    let toolbar = build_toolbar(ToolbarModel::for_browser(browser), tools, tool_model);
    let bounds = Rect::new(0, 0, viewport.width, toolbar_height(theme));
    toolbar.render(surface, bounds, Scale::ONE, theme, font);
}

/// The actionable toolbar command at window-local pixel `point`, or `None`
/// when the click is not on one — outside the toolbar strip, on a group
/// gutter, on a manager write tool, or on a command the [`ToolbarModel`] has
/// disabled (fail closed: a disabled tool does not act, §5.4). It mirrors the
/// drawn toolbar's own layout so a click resolves to exactly the tool
/// [`render`] painted (§2.2). The read-only commands keep the same positions
/// whether or not write tools follow them, so this needs no `tools` argument.
#[must_use]
pub fn toolbar_command_at<S: DirectorySource>(
    browser: &Browser<S>,
    theme: &Theme,
    viewport: Rect,
    point: Point,
) -> Option<ToolbarCommand> {
    let model = ToolbarModel::for_browser(browser);
    let toolbar = build_toolbar(model, &[], ManagerToolModel::none());
    let bounds = Rect::new(0, 0, viewport.width, toolbar_height(theme));
    let index = toolbar.tool_at(bounds, Scale::ONE, theme, point)?;
    let command = *chrome::TOOLBAR_COMMANDS.get(index)?;
    model.is_enabled(command).then_some(command)
}

/// The manager-only write [`ManagerTool`] at window-local pixel `point`, or
/// `None` when the click is not on one. `tools` is the same set handed to
/// [`render`] (a read-only picker passes an empty slice and so never resolves a
/// write tool). The full toolbar — read-only commands then the write tools — is
/// rebuilt so the write tools sit at exactly the positions [`render`] painted
/// them (§2.2); a hit resolves only in the write-tool index range, so a click
/// on a read-only command returns `None` here (it is handled by
/// [`toolbar_command_at`]). `tool_model` is the same enable state handed to
/// [`render`]: a click on a tool the model has disabled resolves to `None`
/// (fail closed — a disabled tool does not act, §5.4).
#[must_use]
pub fn manager_tool_at<S: DirectorySource>(
    browser: &Browser<S>,
    theme: &Theme,
    viewport: Rect,
    point: Point,
    tools: &[ManagerTool],
    tool_model: ManagerToolModel,
) -> Option<ManagerTool> {
    let toolbar = build_toolbar(ToolbarModel::for_browser(browser), tools, tool_model);
    let bounds = Rect::new(0, 0, viewport.width, toolbar_height(theme));
    let index = toolbar.tool_at(bounds, Scale::ONE, theme, point)?;
    let tool_index = index.checked_sub(chrome::TOOLBAR_COMMANDS.len())?;
    let tool = tools.get(tool_index).copied()?;
    tool_model.is_enabled(tool).then_some(tool)
}

/// The window-local [`Rect`] the manager write `tool` occupies, or `None`
/// when `tool` is not among `tools`. The forward mirror of
/// [`manager_tool_at`] over the same rebuilt toolbar (read-only commands then
/// the write tools), so a caller that must aim *at* a write tool — the desktop
/// integration harness that clicks New Folder — reads the exact geometry
/// [`render`] paints and [`manager_tool_at`] hit-tests, never a hand-copied
/// position (§2.2). Fails closed: an out-of-range or unlisted tool is `None`.
///
/// The toolbar left-packs fixed-width buttons and a disabled tool renders in
/// place (muted, never hidden), so a tool's rectangle is independent of its
/// enable state; the geometry is built with every tool enabled
/// ([`ManagerToolModel::new(true)`](ManagerToolModel::new)) and a caller that
/// must only *act* on an enabled tool gates that through [`manager_tool_at`].
#[must_use]
pub fn manager_tool_rect<S: DirectorySource>(
    browser: &Browser<S>,
    theme: &Theme,
    viewport: Rect,
    tools: &[ManagerTool],
    tool: ManagerTool,
) -> Option<Rect> {
    let position = tools.iter().position(|&t| t == tool)?;
    let toolbar = build_toolbar(
        ToolbarModel::for_browser(browser),
        tools,
        ManagerToolModel::new(true),
    );
    let bounds = Rect::new(0, 0, viewport.width, toolbar_height(theme));
    let index = chrome::TOOLBAR_COMMANDS.len().checked_add(position)?;
    toolbar.tool_rect(index, bounds, Scale::ONE, theme)
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

/// The Properties overlay field labels, in display order. One definition so
/// [`properties_rows`] and the inline permission-toggle placement agree on the
/// label column width and which row is the permissions row (§2.2).
const PROPERTY_LABELS: [&str; PROPERTY_ROW_COUNT] = [
    "Kind",
    "Size",
    "Permissions",
    "Owner",
    "Created",
    "Modified",
    "Accessed",
    "Changed",
];

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

    let values = [
        String::from(props.kind_label()),
        size,
        permissions,
        owner,
        props.created_display(),
        props.modified_display(),
        props.accessed_display(),
        props.changed_display(),
    ];
    PROPERTY_LABELS.into_iter().zip(values).collect()
}

/// The number of extra content rows the *editable* Properties popup reserves
/// below the metadata fields for the labelled permissions grid: one blank
/// separator row, one column-header row (Read / Write / Execute), and the
/// three owner/group/other triad rows.
const PERMISSION_GRID_ROWS: usize = 5;

/// The centered bounds of a Properties popup [`Panel`] within `viewport`
/// holding `content_rows` text rows (a title bar plus one line per row with a
/// top and bottom margin), clamped to the window so a small window still
/// yields a drawable — if clipped — panel rather than a panic.
///
/// One definition so the read-only and editable popups differ only in how many
/// rows they reserve, never in how the panel is placed or clamped (§2.2).
fn properties_panel_rect_for(
    viewport: Rect,
    font: BitmapFont,
    theme: &Theme,
    content_rows: usize,
) -> Rect {
    let line = row_height(font);
    let title = Scale::ONE
        .scale_length(theme.metrics().title_bar_height)
        .max(1);
    let rows = u32::try_from(content_rows).unwrap_or(u32::MAX);
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

/// The centered bounds of the read-only Properties popup [`Panel`] within
/// `viewport`, sized to comfortably show the [`properties_rows`] fields (a
/// title bar plus one line per field with a top and bottom margin) and clamped
/// to the window.
///
/// The trusted read-only picker draws this. The file manager's *editable*
/// popup uses the taller [`properties_editable_panel_rect`], which reserves
/// extra rows below the fields for the labelled permissions grid.
#[must_use]
pub fn properties_panel_rect(viewport: Rect, font: BitmapFont, theme: &Theme) -> Rect {
    properties_panel_rect_for(viewport, font, theme, PROPERTY_ROW_COUNT)
}

/// The centered bounds of the *editable* Properties popup [`Panel`] within
/// `viewport`: the read-only panel grown by a few extra rows so the labelled
/// owner/group/other × read/write/execute permissions grid has its own room
/// below the metadata fields rather than being crammed onto — and overlapping
/// — a single text row.
///
/// Only the write-capable file manager draws this; the read-only picker uses
/// [`properties_panel_rect`] and never draws a permission toggle (the editable
/// surface is separated by call site, the manager-only write-tool precedent).
#[must_use]
pub fn properties_editable_panel_rect(viewport: Rect, font: BitmapFont, theme: &Theme) -> Rect {
    properties_panel_rect_for(
        viewport,
        font,
        theme,
        PROPERTY_ROW_COUNT + PERMISSION_GRID_ROWS,
    )
}

/// The shared column geometry of the Properties overlay content area: the
/// label column x, the value column x, and the per-row pitch. One definition
/// so the drawn fields and the inline permission toggles line up exactly
/// (§2.2).
struct FieldLayout {
    /// Left x of the label column.
    left: i32,
    /// Left x of the value column (past the widest label plus a gap).
    value_x: i32,
    /// Vertical pitch between successive rows.
    line: u32,
}

impl FieldLayout {
    fn resolve(content: Rect, font: BitmapFont) -> Self {
        let label_col = PROPERTY_LABELS
            .iter()
            .map(|label| font.text_width(label))
            .max()
            .unwrap_or(0);
        let gap = font.text_width("  ").max(LABEL_PADDING);
        let left = content.left().saturating_add(to_i32(LABEL_PADDING));
        let value_x = left.saturating_add(to_i32(label_col.saturating_add(gap)));
        Self {
            left,
            value_x,
            line: row_height(font),
        }
    }
}

/// Draw the [`properties_rows`] metadata fields as muted-label / solid-value
/// rows within `content`, clipping at the content's bottom edge. Shared by the
/// read-only and editable overlays so the fields read identically (§2.2).
fn draw_property_fields(
    surface: &mut Surface,
    props: &Properties,
    content: Rect,
    palette: &Palette,
    font: BitmapFont,
) {
    let layout = FieldLayout::resolve(content, font);
    let bottom = content.top().saturating_add(to_i32(content.height));
    let mut y = content.top().saturating_add(to_i32(ROW_PADDING));
    for (label, value) in &properties_rows(props) {
        if y >= bottom {
            break;
        }
        font.draw_text(
            surface,
            layout.left,
            y,
            label,
            palette.on_surface_muted.into(),
        );
        font.draw_text(surface, layout.value_x, y, value, palette.on_surface.into());
        y = y.saturating_add(to_i32(layout.line));
    }
}

/// Draw the Properties [`Panel`] for `props` at `bounds`: a panel titled with
/// the node's name, its labelled metadata fields drawn as muted-label /
/// solid-value rows in the panel's content area.
///
/// The read-only and editable popups share this so the metadata reads
/// identically and the panel is placed identically (§2.2); the editable popup
/// then draws the permissions grid over the room its taller `bounds` reserve.
/// Every blit clips, so a window too small for the whole panel simply shows
/// what fits rather than panicking. It reads only the already-authorised
/// [`Properties`] and draws — it performs no I/O and holds no authority
/// (§4, §5.4).
fn draw_properties_at(
    surface: &mut Surface,
    props: &Properties,
    theme: &Theme,
    font: BitmapFont,
    bounds: Rect,
) {
    let panel = Panel::new(props.name());
    panel.render(surface, bounds, Scale::ONE, theme, font);
    let Some(content) = panel.content_rect(bounds, Scale::ONE, theme) else {
        return;
    };
    draw_property_fields(surface, props, content, theme.palette(), font);
}

/// Draw the read-only Properties overlay for `props` centered in `viewport`.
///
/// The trusted read-only picker draws this. It reads only the already-
/// authorised [`Properties`] and draws — it performs no I/O and holds no
/// authority (§4, §5.4).
pub fn draw_properties(
    surface: &mut Surface,
    props: &Properties,
    theme: &Theme,
    font: BitmapFont,
    viewport: Rect,
) {
    draw_properties_at(
        surface,
        props,
        theme,
        font,
        properties_panel_rect(viewport, font, theme),
    );
}

/// The nine settable owner/group/other × read/write/execute permission bits,
/// in the left-to-right order the inline permission control lays them out (the
/// owner triad, then group, then other) — the same order as the symbolic
/// `rwxrwxrwx` spelling they sit over, so the drawn toggles and their hit-test
/// share one definition of which cell carries which bit (§2.2).
///
/// Only these nine `rwx` bits are offered as toggles — the familiar, legible
/// permission set. The setuid/setgid/sticky bits stay visible in the
/// Properties panel's octal and symbolic spelling and are edited through the
/// `chmod` command: a deliberate scope boundary for a best-in-class,
/// bloat-free panel, not an omission. Toggling a cell flips only its own `rwx`
/// bit and preserves whatever the higher bits currently are.
pub const PERMISSION_BITS: [u32; 9] = [
    0o400, 0o200, 0o100, // owner: read, write, execute
    0o040, 0o020, 0o010, // group: read, write, execute
    0o004, 0o002, 0o001, // other: read, write, execute
];

/// Which of the nine [`PERMISSION_BITS`] `mode` currently sets, in the same
/// left-to-right order — the one definition the drawn toggles' states and
/// their tests read, so a toggle can never disagree with the mode it depicts
/// (§2.2).
#[must_use]
pub const fn permission_cells(mode: u32) -> [bool; 9] {
    let mut cells = [false; 9];
    let mut i = 0;
    while i < PERMISSION_BITS.len() {
        cells[i] = mode & PERMISSION_BITS[i] != 0;
        i += 1;
    }
    cells
}

/// The column headers of the permissions grid, left-to-right, matching the
/// read/write/execute order of each triad in [`PERMISSION_BITS`].
const PERMISSION_COLUMN_LABELS: [&str; 3] = ["Read", "Write", "Exec"];

/// The row labels of the permissions grid, top-to-bottom, matching the
/// owner/group/other triad order of [`PERMISSION_BITS`].
const PERMISSION_ROW_LABELS: [&str; 3] = ["Owner", "Group", "Other"];

/// The shared geometry of the editable Properties popup's labelled permissions
/// grid: a column-header row (Read / Write / Execute) above three
/// owner/group/other triad rows, each triad a leading row label followed by
/// its three `rwx` checkboxes.
///
/// One definition so the painted grid, its headers and row labels, and the
/// click hit-test all agree on where every cell sits — the checkboxes are laid
/// out on a real grid pitch (never crammed one glyph apart), so they no longer
/// overlap and each column and row reads under its own label (§2.2).
struct PermGrid {
    /// Left x of the row-label column (Owner / Group / Other).
    label_x: i32,
    /// Left x of the first (Read) checkbox column.
    cols_x: i32,
    /// Horizontal pitch between successive `rwx` columns.
    col_pitch: u32,
    /// The square side of each checkbox box.
    box_side: u32,
    /// Top y of the column-header row.
    header_y: i32,
    /// Top y of the first (Owner) triad row.
    first_row_y: i32,
    /// Vertical pitch between successive triad rows.
    row_line: u32,
}

impl PermGrid {
    /// The checkbox cell for grid index `i` (`i = triad * 3 + bit`, matching
    /// [`PERMISSION_BITS`]): triad selects the owner/group/other row, bit the
    /// read/write/execute column.
    fn cell(&self, index: usize) -> Rect {
        let triad = u32::try_from(index / 3).unwrap_or(0);
        let bit = u32::try_from(index % 3).unwrap_or(0);
        let x = self
            .cols_x
            .saturating_add(to_i32(self.col_pitch.saturating_mul(bit)));
        let y = self
            .first_row_y
            .saturating_add(to_i32(self.row_line.saturating_mul(triad)));
        Rect::new(x, y, self.box_side, self.box_side)
    }

    /// All nine checkbox cells, in [`PERMISSION_BITS`] order.
    fn cells(&self) -> [Rect; 9] {
        core::array::from_fn(|i| self.cell(i))
    }
}

/// The editable Properties popup's permissions-grid geometry, or `None` when
/// the grid does not fit the panel's content (a window too small) — so the
/// painter and the hit-test both fail closed there rather than placing cells
/// off the panel (§2.2, §5.4).
fn perm_grid(viewport: Rect, font: BitmapFont, theme: &Theme) -> Option<PermGrid> {
    let bounds = properties_editable_panel_rect(viewport, font, theme);
    let content = Panel::new(String::new()).content_rect(bounds, Scale::ONE, theme)?;
    let line = row_height(font);
    let box_side = font.glyph_height().max(1);
    // The metadata fields occupy the first `PROPERTY_ROW_COUNT` rows from the
    // top; the grid sits below a one-row blank separator, its column headers
    // one row above the three triad rows.
    let meta_rows = u32::try_from(PROPERTY_ROW_COUNT).unwrap_or(u32::MAX);
    let top = content.top().saturating_add(to_i32(ROW_PADDING));
    let header_y = top.saturating_add(to_i32(line.saturating_mul(meta_rows.saturating_add(1))));
    let first_row_y = header_y.saturating_add(to_i32(line));
    let last_row_bottom = first_row_y
        .saturating_add(to_i32(line.saturating_mul(2)))
        .saturating_add(to_i32(box_side));
    let content_bottom = content.top().saturating_add(to_i32(content.height));
    if last_row_bottom > content_bottom {
        return None;
    }
    let label_x = content.left().saturating_add(to_i32(LABEL_PADDING));
    let row_label_w = PERMISSION_ROW_LABELS
        .iter()
        .map(|label| font.text_width(label))
        .max()
        .unwrap_or(0);
    let col_label_w = PERMISSION_COLUMN_LABELS
        .iter()
        .map(|label| font.text_width(label))
        .max()
        .unwrap_or(0);
    let gap = font.text_width("  ").max(LABEL_PADDING);
    let cols_x = label_x.saturating_add(to_i32(row_label_w.saturating_add(gap)));
    let col_pitch = col_label_w.max(box_side).saturating_add(gap);
    Some(PermGrid {
        label_x,
        cols_x,
        col_pitch,
        box_side,
        header_y,
        first_row_y,
        row_line: line,
    })
}

/// The nine clickable permission-toggle rects, in [`PERMISSION_BITS`] order,
/// laid out on the labelled permissions grid. `None` when the grid does not
/// fit the panel's content (a window too small), so the painter and the
/// hit-test both fail closed there (§2.2, §5.4).
pub(crate) fn permission_toggle_cells(
    viewport: Rect,
    font: BitmapFont,
    theme: &Theme,
) -> Option<[Rect; 9]> {
    Some(perm_grid(viewport, font, theme)?.cells())
}

/// Draw the editable Properties overlay for `props`: the metadata fields as in
/// [`draw_properties`], drawn in the taller editable popup, plus the labelled
/// permissions grid below them — read/write/execute column headers over three
/// owner/group/other triad rows of clickable [`Checkbox`] toggles reflecting
/// the current mode. The grid replaces the old cramped single-row layout, so
/// the toggles never overlap and each reads under its own label.
///
/// Only the write-capable file manager calls this; the trusted read-only
/// picker calls [`draw_properties`] and never draws or resolves a permission
/// toggle (the editable surface is separated by call site, not a runtime flag
/// — the manager-only write-tool precedent). Every blit clips, so a window too
/// small simply shows what fits rather than panicking. It reads only the
/// already-authorised [`Properties`] and draws — the commit happens in the
/// caller's own capability-checked
/// [`Browser::set_mode_selected`](crate::Browser::set_mode_selected) tail, so
/// this holds no authority (§4, §5.4).
pub fn draw_properties_editable(
    surface: &mut Surface,
    props: &Properties,
    theme: &Theme,
    font: BitmapFont,
    viewport: Rect,
) {
    draw_properties_at(
        surface,
        props,
        theme,
        font,
        properties_editable_panel_rect(viewport, font, theme),
    );
    let Some(grid) = perm_grid(viewport, font, theme) else {
        return;
    };
    let palette = theme.palette();
    // Column headers (Read / Write / Exec) above their checkbox columns, so
    // each toggle reads under the access it grants rather than as an unlabelled
    // box.
    for (bit, label) in PERMISSION_COLUMN_LABELS.iter().enumerate() {
        let x = grid.cols_x.saturating_add(to_i32(
            grid.col_pitch
                .saturating_mul(u32::try_from(bit).unwrap_or(0)),
        ));
        font.draw_text(
            surface,
            x,
            grid.header_y,
            label,
            palette.on_surface_muted.into(),
        );
    }
    // Each triad row: its Owner / Group / Other label, then the three `rwx`
    // checkboxes reflecting the current mode.
    let states = permission_cells(props.mode());
    let label_dy = (to_i32(grid.box_side) - to_i32(font.glyph_height())).max(0) / 2;
    for (triad, row_label) in PERMISSION_ROW_LABELS.iter().enumerate() {
        let row_y = grid.first_row_y.saturating_add(to_i32(
            grid.row_line
                .saturating_mul(u32::try_from(triad).unwrap_or(0)),
        ));
        font.draw_text(
            surface,
            grid.label_x,
            row_y.saturating_add(label_dy),
            row_label,
            palette.on_surface.into(),
        );
        for bit in 0..3 {
            let index = triad * 3 + bit;
            let selection = if states[index] {
                SelectionState::Selected
            } else {
                SelectionState::Unselected
            };
            Checkbox::new(String::new(), selection).render(
                surface,
                grid.cell(index),
                Scale::ONE,
                theme,
                font,
            );
        }
    }
}

/// The permission bit whose toggle the editable Properties overlay draws at
/// window-local pixel `point`, or `None` when the click is not on a toggle.
///
/// This mirrors [`draw_properties_editable`]'s placement through the shared
/// `permission_toggle_cells` geometry, so a click toggles exactly the bit
/// the user pressed (§2.2). Only the file manager calls it — the caller flips
/// the returned bit in the current mode and commits through its own
/// capability-checked
/// [`Browser::set_mode_selected`](crate::Browser::set_mode_selected). A click
/// anywhere but a toggle returns `None`, changing nothing (fail closed, §5.4).
#[must_use]
pub fn permission_cell_at(
    viewport: Rect,
    font: BitmapFont,
    theme: &Theme,
    point: Point,
) -> Option<u32> {
    let cells = permission_toggle_cells(viewport, font, theme)?;
    for (i, rect) in cells.iter().enumerate() {
        let right = rect.left().saturating_add(to_i32(rect.width));
        let bottom = rect.top().saturating_add(to_i32(rect.height));
        if point.x >= rect.left() && point.x < right && point.y >= rect.top() && point.y < bottom {
            return PERMISSION_BITS.get(i).copied();
        }
    }
    None
}

/// The index of the "Owner" row within [`PROPERTY_LABELS`] — the row the file
/// manager overlays with the inline uid/gid ownership control.
const OWNER_ROW_INDEX: usize = 3;

/// Which of the two owning ids the inline ownership control edits.
///
/// The owning user (`uid`) and group (`gid`) are the two independently
/// editable values on the Properties overlay's owner row; a click resolves to
/// exactly one of them ([`owner_field_at`]) and the caller commits that one
/// field through
/// [`Browser::set_owner_selected`](crate::Browser::set_owner_selected).
#[derive(Copy, Clone, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub enum OwnerField {
    /// The owning user id (`chown`).
    Uid,
    /// The owning group id (`chgrp`).
    Gid,
}

/// The geometry of the Properties overlay's owner row: the clickable bounds of
/// the uid and gid values (`[uid, gid]`), each sized to the digits it shows,
/// and the row pitch the active editor is sized from.
struct OwnerRowGeom {
    /// The uid value's cell and the gid value's cell, left-to-right.
    cells: [Rect; 2],
    /// The vertical pitch between rows — the height the inline editor uses so
    /// it is tall enough to read while it overlays the value.
    line: u32,
}

/// The owner row's geometry, or `None` when the owner row does not fit the
/// panel's content (a window too small) — so the painter and the hit-test both
/// fail closed there rather than placing a control off the row (§2.2, §5.4).
///
/// The cells are measured from the same `uid N / gid N` spelling
/// [`properties_rows`] draws, so a click lands exactly on the number it edits.
fn owner_row_geom(
    props: &Properties,
    viewport: Rect,
    font: BitmapFont,
    theme: &Theme,
) -> Option<OwnerRowGeom> {
    let bounds = properties_editable_panel_rect(viewport, font, theme);
    let content = Panel::new(String::new()).content_rect(bounds, Scale::ONE, theme)?;
    let layout = FieldLayout::resolve(content, font);
    let glyph = font.glyph_height().max(1);
    let row_index = u32::try_from(OWNER_ROW_INDEX).unwrap_or(u32::MAX);
    let row_top = content
        .top()
        .saturating_add(to_i32(ROW_PADDING))
        .saturating_add(to_i32(layout.line.saturating_mul(row_index)));
    let content_bottom = content.top().saturating_add(to_i32(content.height));
    if row_top.saturating_add(to_i32(glyph)) > content_bottom {
        return None;
    }
    let uid_str = props.uid().to_string();
    let gid_str = props.gid().to_string();
    let uid_x = layout
        .value_x
        .saturating_add(to_i32(font.text_width("uid ")));
    let uid_w = font.text_width(&uid_str).max(1);
    let gid_x = uid_x
        .saturating_add(to_i32(uid_w))
        .saturating_add(to_i32(font.text_width(" / gid ")));
    let gid_w = font.text_width(&gid_str).max(1);
    Some(OwnerRowGeom {
        cells: [
            Rect::new(uid_x, row_top, uid_w, glyph),
            Rect::new(gid_x, row_top, gid_w, glyph),
        ],
        line: layout.line,
    })
}

/// The owning-id field whose value the editable Properties overlay draws at
/// window-local pixel `point`, or `None` when the click is not on a value.
///
/// This mirrors [`draw_owner_control`]'s placement through the shared
/// `owner_row_geom`, so a click begins editing exactly the id the user pressed
/// (§2.2). Only the file manager — and only where the user holds
/// `CAP_FS_CHOWN` — calls it; a click anywhere but a value returns `None`,
/// changing nothing (fail closed, §5.4).
#[must_use]
pub fn owner_field_at(
    props: &Properties,
    viewport: Rect,
    font: BitmapFont,
    theme: &Theme,
    point: Point,
) -> Option<OwnerField> {
    let geom = owner_row_geom(props, viewport, font, theme)?;
    let fields = [OwnerField::Uid, OwnerField::Gid];
    for (rect, field) in geom.cells.iter().zip(fields) {
        let right = rect.left().saturating_add(to_i32(rect.width));
        let bottom = rect.top().saturating_add(to_i32(rect.height));
        if point.x >= rect.left() && point.x < right && point.y >= rect.top() && point.y < bottom {
            return Some(field);
        }
    }
    None
}

/// The width, in pixels, the active owner editor is drawn at — comfortably
/// wider than a single number so a `u32` id (up to ten digits) is readable
/// while typed.
fn owner_editor_width(font: BitmapFont) -> u32 {
    font.text_width("0000000000").max(1)
}

/// Draw the inline ownership control over the Properties overlay's owner row:
/// an accent underline beneath the uid and gid values marking each as
/// clickable to edit, and — when `editor` names a field being edited — the
/// active [`TextField`] over that value.
///
/// Only the file manager, and only where the launching user holds
/// `CAP_FS_CHOWN`, calls this: reassigning an owner is a privileged operation
/// (unlike renaming or a mode change), so the control is offered only where it
/// can be used, and a session without the capability is never shown a control
/// it cannot use (§2.24). The trusted read-only picker never calls it (the
/// write surface is separated by call site, the manager-only write-tool
/// precedent). Every blit clips, so a window too small simply shows what fits
/// rather than panicking. It reads only the already-authorised [`Properties`]
/// and draws — the commit happens in the caller's own capability-checked
/// [`Browser::set_owner_selected`](crate::Browser::set_owner_selected) tail
/// over `fs_set_owner`, so this holds no authority (§4, §5.4).
pub fn draw_owner_control(
    surface: &mut Surface,
    props: &Properties,
    theme: &Theme,
    font: BitmapFont,
    viewport: Rect,
    editor: Option<(OwnerField, &TextField)>,
) {
    let Some(geom) = owner_row_geom(props, viewport, font, theme) else {
        return;
    };
    let palette = theme.palette();
    let thickness = Scale::ONE.scale_length(1).max(1);
    let fields = [OwnerField::Uid, OwnerField::Gid];
    for (rect, field) in geom.cells.iter().zip(fields) {
        if let Some((editing, text_field)) = editor {
            if editing == field {
                let left = u32::try_from(rect.left()).unwrap_or(0);
                let avail = viewport.width.saturating_sub(left).max(1);
                let width = owner_editor_width(font).min(avail);
                let bounds = Rect::new(rect.left(), rect.top(), width, geom.line);
                text_field.render(surface, bounds, Scale::ONE, theme, font);
                continue;
            }
        }
        let underline_y = rect.top().saturating_add(to_i32(rect.height));
        surface.fill_rect(
            u32::try_from(rect.left()).unwrap_or(0),
            u32::try_from(underline_y).unwrap_or(0),
            rect.width,
            thickness,
            palette.accent.into(),
        );
    }
}

/// Saturating `u32` → `i32`.
fn to_i32(value: u32) -> i32 {
    i32::try_from(value).unwrap_or(i32::MAX)
}

/// The action-button index of the destructive **Delete** action in the
/// delete-confirmation [`Dialog`] [`build_delete_dialog`] produces.
pub const DELETE_CONFIRM_INDEX: usize = 0;

/// The action-button index of the safe **Cancel** action in the
/// delete-confirmation [`Dialog`] [`build_delete_dialog`] produces.
pub const DELETE_CANCEL_INDEX: usize = 1;

/// Build the modal delete-confirmation [`Dialog`] for `plan`, worded honestly
/// for the `disposition` the caller will actually carry out (§2.24): a
/// recoverable **Move to Trash** or an irreversible **Delete Permanently**.
///
/// The [`DeleteTarget`](crate::DeleteTarget) count and
/// [`has_directories`](DeletePlan::has_directories) come straight from the
/// already-captured [`DeletePlan`], so the confirmation reports the true scope
/// of the removal rather than a fabricated figure. `disposition`
/// ([`DeleteDisposition`]) is the caller's own decision — computed from the
/// targets' and the user's Trash directory's volume ids — so the dialog never
/// promises a wording its execution will not honour: a
/// [`Trash`](DeleteDisposition::Trash) confirmation offers a safe, recoverable
/// **Move to Trash**, a [`Permanent`](DeleteDisposition::Permanent) one the
/// destructive **Delete Permanently** with the honest warmth on the safe
/// Cancel. The dialog performs nothing itself — the caller drives the removal
/// in its own capability-checked tail once the user confirms — so composing it
/// grants no authority. Both the file manager (which builds one) and, in
/// principle, any other write-capable consumer share this one definition; the
/// read-only picker never deletes, so it never builds one.
#[must_use]
pub fn build_delete_dialog(plan: &DeletePlan, disposition: DeleteDisposition) -> Dialog {
    match disposition {
        DeleteDisposition::Trash => build_trash_dialog(plan),
        DeleteDisposition::Permanent => build_permanent_delete_dialog(plan),
    }
}

/// The recoverable **Move to Trash** confirmation: nothing is destroyed, so the
/// confirm action is the recommended (safe) primary rather than a destructive
/// one, and the message states that trashed items can be restored.
fn build_trash_dialog(plan: &DeletePlan) -> Dialog {
    let title = if plan.len() == 1 {
        alloc::format!(
            "Move \u{201c}{}\u{201d} to Trash?",
            plan.targets()[0].name()
        )
    } else {
        alloc::format!("Move {} items to Trash?", plan.len())
    };
    // Recoverable: the honest warmth sits on the confirm action because the
    // move can be undone by restoring from Trash — it is not destructive.
    let confirm = Button::new(
        ButtonContent::Label(String::from("Move to Trash")),
        ControlRole::Recommended,
    );
    let cancel = Button::new(
        ButtonContent::Label(String::from("Cancel")),
        ControlRole::Neutral,
    );
    Dialog::new(title)
        .with_message("Items stay in the Trash until you empty it, so you can restore them.")
        .with_actions(vec![confirm, cancel])
}

/// The irreversible **Delete Permanently** confirmation: the destructive action
/// carries the Destructive role and the confirmation posture, and Cancel is the
/// recommended (safe, trailing) action so the honest warmth sits on the safe
/// choice, never on the delete.
fn build_permanent_delete_dialog(plan: &DeletePlan) -> Dialog {
    let title = if plan.len() == 1 {
        alloc::format!(
            "Delete \u{201c}{}\u{201d} permanently?",
            plan.targets()[0].name()
        )
    } else {
        alloc::format!("Delete {} items permanently?", plan.len())
    };
    let message = if plan.has_directories() {
        "Folders and everything inside them will be removed. This cannot be undone."
    } else {
        "This cannot be undone."
    };
    let mut delete = Button::new(
        ButtonContent::Label(String::from("Delete Permanently")),
        ControlRole::Destructive,
    );
    delete.set_state(ControlState::idle().with_authority(AuthorityState::NeedsConfirmation));
    let cancel = Button::new(
        ButtonContent::Label(String::from("Cancel")),
        ControlRole::Recommended,
    );
    Dialog::new(title)
        .with_message(message)
        .with_actions(vec![delete, cancel])
}

/// The centered, clamped bounds of the delete-confirmation dialog within
/// `viewport`.
///
/// Sized to comfortably show the title, the warning message, and the action
/// button band, and clamped to the window so a small window still yields a
/// drawable — if clipped — dialog rather than a panic (§2.9). One definition so
/// [`draw_delete_dialog`] and [`delete_dialog_action_at`] place and hit-test
/// the same rectangle (§2.2).
#[must_use]
pub fn delete_dialog_rect(viewport: Rect, font: BitmapFont, theme: &Theme) -> Rect {
    // Title bar, up to two message lines, and the action-button band, with
    // margins — generous so the buttons are not clipped at a normal size.
    centered_overlay_rect(viewport, font, theme, 6)
}

/// A centered, clamped modal-overlay rectangle within `viewport`, sized to a
/// title bar plus `content_lines` text rows and four-fifths of the window
/// width, clamped so a small window still yields a drawable — if clipped —
/// rectangle rather than a panic (§2.9).
///
/// The one sizing definition the delete-confirmation dialog and the progress
/// panel share, so their placement stays consistent and cannot drift (§2.2).
fn centered_overlay_rect(
    viewport: Rect,
    font: BitmapFont,
    theme: &Theme,
    content_lines: u32,
) -> Rect {
    let line = row_height(font);
    let title = Scale::ONE
        .scale_length(theme.metrics().title_bar_height)
        .max(1);
    let content = line.saturating_mul(content_lines);
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

/// Draw the delete-confirmation `dialog` centered in `viewport`, on top of the
/// current view.
///
/// Every blit clips, so a window too small for the whole dialog simply shows
/// what fits rather than panicking. It reads only the passed-in dialog and
/// draws — it performs no I/O and holds no authority (§4, §5.4).
pub fn draw_delete_dialog(
    surface: &mut Surface,
    dialog: &Dialog,
    theme: &Theme,
    font: BitmapFont,
    viewport: Rect,
) {
    let bounds = delete_dialog_rect(viewport, font, theme);
    dialog.render(surface, bounds, Scale::ONE, theme, font);
}

/// The action-button index the delete-confirmation `dialog` draws at
/// window-local pixel `point`, or `None` when the click is not on a button.
///
/// This mirrors [`draw_delete_dialog`]'s placement through the shared
/// [`delete_dialog_rect`] and the dialog's own
/// [`action_rects`](Dialog::action_rects) geometry, so a click resolves to
/// exactly the button the user pressed (§2.2) — [`DELETE_CONFIRM_INDEX`] for
/// Delete, [`DELETE_CANCEL_INDEX`] for Cancel. Only the file manager calls it;
/// a click anywhere but a button returns `None`, changing nothing (fail
/// closed, §5.4).
#[must_use]
pub fn delete_dialog_action_at(
    dialog: &Dialog,
    viewport: Rect,
    font: BitmapFont,
    theme: &Theme,
    point: Point,
) -> Option<usize> {
    let bounds = delete_dialog_rect(viewport, font, theme);
    let rects = dialog.action_rects(bounds, Scale::ONE, theme, font);
    for (i, rect) in rects.iter().enumerate() {
        if rect.width == 0 {
            continue;
        }
        let right = rect.left().saturating_add(to_i32(rect.width));
        let bottom = rect.top().saturating_add(to_i32(rect.height));
        if point.x >= rect.left() && point.x < right && point.y >= rect.top() && point.y < bottom {
            return Some(i);
        }
    }
    None
}

/// The centered, clamped bounds of the long-operation progress panel within
/// `viewport`.
///
/// One definition so [`draw_progress_dialog`] and [`progress_cancel_at`] place
/// and hit-test the same rectangle (§2.2), sized like the delete-confirmation
/// dialog so the two modal surfaces sit consistently.
#[must_use]
pub fn progress_dialog_rect(viewport: Rect, font: BitmapFont, theme: &Theme) -> Rect {
    centered_overlay_rect(viewport, font, theme, 6)
}

/// The Cancel-button rectangle within the progress panel's `content` area —
/// bottom-right, sized to the "Cancel" label plus padding, clamped to the
/// content so a small window never places it off the panel. The one definition
/// [`draw_progress_dialog`] paints and [`progress_cancel_at`] hit-tests, so a
/// click resolves to exactly the drawn button (§2.2).
fn progress_cancel_rect(content: Rect, font: BitmapFont) -> Rect {
    let pad = font.text_width("  ").max(LABEL_PADDING);
    let width = font
        .text_width("Cancel")
        .saturating_add(pad.saturating_mul(2))
        .min(content.width);
    let height = row_height(font).min(content.height);
    let x = content
        .left()
        .saturating_add(to_i32(content.width.saturating_sub(width)));
    let y = content
        .top()
        .saturating_add(to_i32(content.height.saturating_sub(height)));
    Rect::new(x, y, width, height)
}

/// Build the progress panel's [`Progress`] trace for `model`: an indeterminate
/// "working" bar captioned with the model's honest running count.
///
/// The total is unknown until the driving walk's reads reveal it, so the trace
/// is [`ActivityState::Working`] (a bounded moving segment) rather than a
/// fabricated percentage (§2.24). Its moving-segment phase is derived from the
/// count, so the bar advances on real job-progress events, never an idle
/// animation loop (§2.23).
#[must_use]
pub fn build_progress(model: &ProgressModel) -> Progress {
    let mut progress = Progress::new().with_label(model.status_line());
    progress.set_state(ControlState::idle().with_activity(ActivityState::Working));
    // A permille phase that turns over as items are processed — motion is
    // driven by real progress, not a timer.
    let phase = u16::try_from(model.done() % 1000).unwrap_or(0);
    progress.set_phase(phase);
    progress
}

/// Build the progress panel's Cancel [`Button`] for `model`: enabled while the
/// run is in progress, disabled once a cancel has already been latched (so a
/// second press cannot re-request what is already stopping).
#[must_use]
pub fn build_progress_cancel(model: &ProgressModel) -> Button {
    let mut button = Button::new(
        ButtonContent::Label(String::from("Cancel")),
        ControlRole::Neutral,
    );
    if model.is_cancel_requested() {
        button.set_state(ControlState::disabled());
    }
    button
}

/// Draw the long-operation progress panel for `model` centered in `viewport`,
/// on top of the current view: a titled [`Panel`], an indeterminate progress
/// trace captioned with the honest running count, and a Cancel button.
///
/// Every blit clips, so a window too small for the whole panel simply shows
/// what fits rather than panicking (§2.9). It reads only the passed-in model
/// and draws — it performs no I/O and holds no authority (§4, §5.4). Only the
/// write-capable file manager drives a long operation, so only it draws this;
/// the read-only picker never does.
pub fn draw_progress_dialog(
    surface: &mut Surface,
    model: &ProgressModel,
    theme: &Theme,
    font: BitmapFont,
    viewport: Rect,
) {
    let bounds = progress_dialog_rect(viewport, font, theme);
    let panel = Panel::new(model.title());
    panel.render(surface, bounds, Scale::ONE, theme, font);
    let Some(content) = panel.content_rect(bounds, Scale::ONE, theme) else {
        return;
    };
    let bar = Rect::new(
        content.left(),
        content.top(),
        content.width,
        row_height(font),
    );
    build_progress(model).render(surface, bar, Scale::ONE, theme, font);
    let cancel_rect = progress_cancel_rect(content, font);
    build_progress_cancel(model).render(surface, cancel_rect, Scale::ONE, theme, font);
}

/// Whether the progress panel's Cancel button is drawn at window-local pixel
/// `point`.
///
/// Mirrors [`draw_progress_dialog`]'s placement through the shared
/// [`progress_dialog_rect`] and the same private cancel-button rectangle it
/// paints, so a click resolves to exactly the drawn button (§2.2). A click
/// anywhere but the button — or on
/// a panel too small to place it — returns `false`, changing nothing (fail
/// closed, §5.4).
#[must_use]
pub fn progress_cancel_at(viewport: Rect, font: BitmapFont, theme: &Theme, point: Point) -> bool {
    let bounds = progress_dialog_rect(viewport, font, theme);
    // The content area is title-text-independent, so an empty-title panel
    // mirrors the titled panel [`draw_progress_dialog`] draws.
    let Some(content) = Panel::new(String::new()).content_rect(bounds, Scale::ONE, theme) else {
        return false;
    };
    let rect = progress_cancel_rect(content, font);
    if rect.width == 0 || rect.height == 0 {
        return false;
    }
    let right = rect.left().saturating_add(to_i32(rect.width));
    let bottom = rect.top().saturating_add(to_i32(rect.height));
    point.x >= rect.left() && point.x < right && point.y >= rect.top() && point.y < bottom
}

/// Build the drawn right-click context [`Menu`] for `model`: one [`MenuItem`]
/// per [`chrome::CONTEXT_COMMANDS`] entry, in order, each carrying the
/// command's label and keyboard-shortcut caption and rendered disabled (not
/// hidden) when the model reports the command is not currently actionable — so
/// the menu's shape is stable and an inapplicable command reads muted rather
/// than vanishing. The menu performs nothing itself — the caller dispatches the
/// chosen command in its own capability-checked tail — so composing it grants
/// no authority; the read-only picker never opens a write context menu, so it
/// never builds one.
#[must_use]
pub fn build_context_menu(model: ContextMenuModel) -> Menu {
    let items: Vec<MenuItem> = chrome::CONTEXT_COMMANDS
        .iter()
        .map(|&command| {
            let mut item = MenuItem::new(command.label()).with_shortcut(command.shortcut());
            if !model.is_enabled(command) {
                item = item.with_state(ControlState::disabled());
            }
            item
        })
        .collect();
    Menu::new(items)
}

/// The bounds of the context `menu` anchored at window-local `anchor` (the
/// right-click point), clamped so the whole menu stays inside `viewport`.
///
/// The menu's top-left is placed at `anchor`; if it would overflow the right or
/// bottom edge it is shifted left/up so it fits, and it never leaves the
/// viewport origin. One definition so [`draw_context_menu`] and
/// [`context_menu_command_at`] place and hit-test the same rectangle (§2.2). A
/// degenerate viewport still yields a drawable — if clipped — rectangle rather
/// than a panic (§2.9).
#[must_use]
pub fn context_menu_rect(
    menu: &Menu,
    anchor: Point,
    viewport: Rect,
    font: BitmapFont,
    theme: &Theme,
) -> Rect {
    let width = menu
        .preferred_width(Scale::ONE, theme, font)
        .clamp(1, viewport.width.max(1));
    let height = menu
        .preferred_height(Scale::ONE, theme)
        .clamp(1, viewport.height.max(1));
    let origin_x = viewport.origin.x;
    let origin_y = viewport.origin.y;
    let max_x = origin_x.saturating_add(to_i32(viewport.width.saturating_sub(width)));
    let max_y = origin_y.saturating_add(to_i32(viewport.height.saturating_sub(height)));
    let x = anchor.x.clamp(origin_x, max_x.max(origin_x));
    let y = anchor.y.clamp(origin_y, max_y.max(origin_y));
    Rect::new(x, y, width, height)
}

/// Draw the context `menu` anchored at `anchor`, on top of the current view.
///
/// Every blit clips, so an anchor near an edge simply shows the shifted,
/// possibly-clipped menu rather than panicking. It reads only the passed-in
/// menu and draws — no I/O, no authority (§4, §5.4).
pub fn draw_context_menu(
    surface: &mut Surface,
    menu: &Menu,
    anchor: Point,
    theme: &Theme,
    font: BitmapFont,
    viewport: Rect,
) {
    let bounds = context_menu_rect(menu, anchor, viewport, font, theme);
    menu.render(surface, bounds, Scale::ONE, theme, font);
}

/// The enabled [`ContextCommand`] the context `menu` (opened at `anchor`) draws
/// at window-local pixel `point`, or `None` when the click is not on an
/// actionable row — off the menu, or on a command rendered disabled (fail
/// closed: a disabled row never acts, §5.4).
///
/// This mirrors [`draw_context_menu`]'s placement through the shared
/// [`context_menu_rect`] and the menu's own [`Menu::row_at`] geometry, so a
/// click resolves to exactly the row the user pressed (§2.2). The menu is built
/// from [`chrome::CONTEXT_COMMANDS`] in order, so the row index maps straight
/// back to its command.
#[must_use]
pub fn context_menu_command_at(
    menu: &Menu,
    anchor: Point,
    viewport: Rect,
    font: BitmapFont,
    theme: &Theme,
    point: Point,
) -> Option<ContextCommand> {
    let index = menu_enabled_row_at(menu, anchor, viewport, font, theme, point)?;
    chrome::CONTEXT_COMMANDS.get(index).copied()
}

/// The window-local [`Rect`] the context `menu` (opened at `anchor`) draws the
/// row for `command` at, or `None` when `command` is not among
/// [`chrome::CONTEXT_COMMANDS`]. The forward mirror of
/// [`context_menu_command_at`] over the shared [`context_menu_rect`] placement
/// and the menu's own [`Menu::row_rect`] geometry, so a caller that must aim
/// *at* a command — the desktop integration harness that clicks Delete — reads
/// the exact rectangle [`draw_context_menu`] paints and
/// [`context_menu_command_at`] hit-tests, never a hand-copied position (§2.2).
#[must_use]
pub fn context_menu_command_rect(
    menu: &Menu,
    anchor: Point,
    viewport: Rect,
    font: BitmapFont,
    theme: &Theme,
    command: ContextCommand,
) -> Option<Rect> {
    let index = chrome::CONTEXT_COMMANDS
        .iter()
        .position(|&c| c == command)?;
    let bounds = context_menu_rect(menu, anchor, viewport, font, theme);
    menu.row_rect(index, bounds, Scale::ONE, theme)
}

/// The index of the enabled row of `menu` (anchored at `anchor`) that
/// window-local pixel `point` lands on, or `None` when the click is off the
/// menu or on a disabled row (fail closed, §5.4).
///
/// The one placement + row geometry the drawn menus resolve a click through:
/// both the right-click [`context_menu_command_at`] and the "Open With…"
/// chooser [`open_with_index_at`] map a press to a row this way, so a menu's
/// paint and its hit-test can never disagree (§2.2).
fn menu_enabled_row_at(
    menu: &Menu,
    anchor: Point,
    viewport: Rect,
    font: BitmapFont,
    theme: &Theme,
    point: Point,
) -> Option<usize> {
    let bounds = context_menu_rect(menu, anchor, viewport, font, theme);
    let index = menu.row_at(bounds, Scale::ONE, theme, point)?;
    if !menu.items().get(index)?.state().is_actionable() {
        return None;
    }
    Some(index)
}

/// Build the drawn "Open With…" chooser [`Menu`] from `apps` — the installed
/// applications [`applications_for`](crate::open_with::applications_for)
/// returned for the file, in that source order: one enabled [`MenuItem`] per
/// candidate, captioned with the bundle's name.
///
/// The rows carry no keyboard shortcut (a chosen application is picked by
/// pointer) and are all actionable, since each is a genuine candidate. The
/// caller only opens this chooser when `apps` is non-empty — no application is
/// an honest "no application" answer stated elsewhere, never an empty menu
/// (§2.24). The menu performs nothing itself: launching the chosen bundle is
/// the file manager's own capability-checked hand-off, so composing it grants
/// no authority (the read-only picker never opens it).
#[must_use]
pub fn build_open_with_menu(apps: &[&AppAssociation]) -> Menu {
    let items: Vec<MenuItem> = apps.iter().map(|app| MenuItem::new(app.name())).collect();
    Menu::new(items)
}

/// The index into the "Open With…" chooser's application list that the drawn
/// `menu` (opened at `anchor`) resolves window-local pixel `point` to, or
/// `None` when the click is off the menu (fail closed, §5.4).
///
/// [`build_open_with_menu`] builds the menu from the candidate list in order,
/// so the returned index maps straight back to that application. It shares the
/// placement and row geometry the right-click menu uses
/// ([`context_menu_command_at`]), so paint and click agree (§2.2).
#[must_use]
pub fn open_with_index_at(
    menu: &Menu,
    anchor: Point,
    viewport: Rect,
    font: BitmapFont,
    theme: &Theme,
    point: Point,
) -> Option<usize> {
    menu_enabled_row_at(menu, anchor, viewport, font, theme, point)
}
