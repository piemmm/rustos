//! Painting the browser's current directory into a pixel [`Surface`].
//!
//! [`render_into`] turns a [`Browser`]'s path and entries into a premultiplied-alpha
//! [`Surface`] sized to the app's content viewport, using the active theme's
//! [`Palette`](tairix_theme::Palette) for the chrome and the shared
//! `lib/controls` collection controls for the items, every length converted
//! from logical pixels through the desktop's one [`Scale`]. The theme picks
//! the text face, never the caller. The surface is the window manager's to
//! place and round: the browser paints a *rectangular* buffer and the
//! compositor applies any corner radius through its single anti-aliased
//! rounded-corner path. There is no rounding here.
//!
//! The top row is the command toolbar; below it the current directory is drawn
//! in whichever [`ViewMode`] the browser holds — a column of full-width
//! [`TableRow`]s (list) or a wrapped grid of [`IconTile`]s (grid) — over the
//! one shared selection state, with a drawn [`ScrollBar`] in a reserved
//! right-edge gutter. Painting through the same collection controls the trusted
//! picker uses keeps the two views one coherent themed surface. The visible
//! window, each item's rectangle, the scroll offset, and the scrollbar geometry
//! all come from the one shared [`ViewLayout`], so the pointer hit-test
//! ([`entry_index_at`]) and the paint can never disagree.
//!
//! Every length saturates and every blit clips, so a degenerate viewport paints
//! nothing rather than panicking. The grid additionally confines its paint to
//! the item area ([`GridView::tile_area`]), so a tile can never mark the chrome
//! above it or the scrollbar gutter beside it whatever it draws inside its own
//! rectangle.

use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;

use tairix_controls::button::{Button, ButtonContent};
use tairix_controls::decision::Dialog;
use tairix_controls::scroll::{ScrollModel, ScrollRange};
use tairix_controls::state::{
    ActivityState, AuthorityState, ControlRole, ControlState, PointerState, SelectionState,
};
use tairix_controls::text::TextField;
use tairix_controls::value::Progress;
use tairix_controls::{
    Checkbox, IconButton, IconTile, ListRow, Panel, ScrollAction, ScrollBar, ScrollPart, TableCell,
    TableRow, Toolbar,
};
use tairix_font::BitmapFont;
use tairix_geometry::{Point, Rect, Region, Scale};
use tairix_icon::{IconArtwork, IconKind, IconRequest};
use tairix_input::{InputEvent, PointerButton};
use tairix_raster::Surface;
use tairix_theme::{TextRole, Theme};

use crate::browser::Browser;
use crate::chrome::{
    self, ManagerTool, ManagerToolModel, ToolbarBand, ToolbarCommand, ToolbarModel,
};
use crate::delete::DeletePlan;
use crate::entry::{Entry, EntryKind};
use crate::format::{format_date, format_size};
use crate::layout::{
    GridFill, GridFlow, GridMetrics, GridView, ListView, SidebarView, ViewLayout, ViewMode,
};
use crate::media::{entry_icon_request, icon_for_entry};
use crate::open_with::OpenWithChooser;
use crate::places::{self, Place, Places};
use crate::progress::ProgressModel;
use crate::properties::{Attributes, Properties};
use crate::rowlist::RowList;
use crate::source::DirectorySource;
use crate::trash::DeleteDisposition;

/// Padding between a panel's edge and its label text, in logical pixels.
const LABEL_PADDING: u32 = 4;

/// Vertical padding above and below a row's glyphs, in logical pixels.
const ROW_PADDING: u32 = 2;

/// Relative widths of the list view's name, size, and modified columns.
///
/// [`TableRow::render`] scales these proportionally into the actual content
/// width, so they act as weights independent of the window size: the name
/// column dominates, with narrower size and date columns beside it. Defining
/// them once here keeps the column layout a single definition.
const COLUMNS: [u32; 3] = [240, 96, 128];

/// Paint `browser`'s current directory into `surface` at `viewport`'s size,
/// using `theme`'s palette and the shared collection controls.
///
/// The caller owns the surface, and holds it for the life of its window: a
/// repaint clipped to what a round changed
/// ([`Surface::with_clip`](tairix_raster::Surface::with_clip)) is sound only
/// because every pixel outside the clip is the one already on screen.
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
/// `artwork` is the draw-site icon lookup the grid view resolves each tile's
/// real icon through: for every grid tile the renderer classifies the entry to
/// an [`IconKind`] — naming the bundle itself as well when the entry is one —
/// and asks `artwork` for a pre-rasterised surface at the tile's icon slot,
/// falling back to the built-in glyph when it returns `None`. The list view is
/// text-only and never consults it. A caller with no artwork cache passes
/// [`NoArtwork`](tairix_icon::NoArtwork), which always returns `None` (every
/// tile then draws its built-in glyph).
///
/// `scale` is the desktop's density factor: every chrome length here is
/// authored logically and converted through it, and the text face is the one
/// `theme`'s ladder names at that scale — the caller never chooses a typeface.
///
/// Only `viewport`'s dimensions are used; the window manager places the
/// surface at `viewport`'s origin.
pub fn render_into<S: DirectorySource>(
    surface: &mut Surface,
    browser: &Browser<S>,
    scale: Scale,
    theme: &Theme,
    viewport: Rect,
    chrome: &ManagerChrome<'_>,
    artwork: &mut dyn IconArtwork,
) {
    let palette = theme.palette();

    surface.fill(palette.surface.into());
    let area = content_area(viewport, scale, theme, chrome.sidebar, chrome.toolbar);
    if let (Some(places), Some(view)) = (
        chrome.sidebar,
        sidebar_view(viewport, scale, theme, chrome.sidebar, chrome.toolbar),
    ) {
        let selected = places.index_of(browser.components());
        draw_sidebar(surface, scale, theme, places, &view, selected, artwork);
    }
    // The toolbar is window chrome: its band spans the full window, above the
    // rail, so it aligns with the rest of the desktop's chrome. Everything
    // below it is laid out in `area`, the window less the rail.
    if chrome.toolbar.is_shown() {
        draw_toolbar(
            surface,
            scale,
            theme,
            browser,
            viewport,
            chrome.tools,
            chrome.tool_model,
            artwork,
        );
    }

    let content = content_viewport(area, scale, theme);
    if awaiting_listing(browser) {
        draw_listing_cue(surface, scale, theme, content);
        return;
    }
    match browser.view_mode() {
        ViewMode::List => draw_list(
            surface,
            scale,
            theme,
            browser,
            content,
            chrome.toolbar,
            artwork,
        ),
        ViewMode::Grid => draw_grid(
            surface,
            scale,
            theme,
            browser,
            content,
            chrome.toolbar,
            artwork,
        ),
    }
    draw_scrollbar(surface, scale, theme, browser, area, chrome.toolbar);
}

/// What the listing area says while a directory read is still in flight.
///
/// One definition, so the drawn cue and any observer of it (a test, a QEMU
/// vertical reading the scan-out) agree on the exact text.
pub const LISTING_MESSAGE: &str = "Listing…";

/// Whether the listing area should show [`LISTING_MESSAGE`] instead of items.
///
/// Two cases, one rule. A read of *somewhere else* is in flight, so the items on
/// screen belong to a directory the user has already asked to leave and showing
/// them as if current would be a lie. Or there are no items to show at all — a
/// window that has just opened — where a blank area says nothing. A re-read of
/// the directory already shown is neither: it keeps its items, so a periodic
/// re-list cannot make the view flicker.
fn awaiting_listing<S: DirectorySource>(browser: &Browser<S>) -> bool {
    match browser.listing_target() {
        None => false,
        Some(target) => target != browser.components() || browser.entries().is_empty(),
    }
}

/// Draw [`LISTING_MESSAGE`] centred in `content`, in the muted ink an inactive
/// label uses: it is a state, not an error.
fn draw_listing_cue(surface: &mut Surface, scale: Scale, theme: &Theme, content: Rect) {
    let font = BitmapFont::for_role(theme.fonts(), TextRole::Body, scale);
    let width = font.text_width(LISTING_MESSAGE);
    let x = content
        .left()
        .saturating_add_unsigned(content.width.saturating_sub(width) / 2);
    let y = content
        .top()
        .saturating_add_unsigned(content.height.saturating_sub(font.glyph_height()) / 2);
    font.draw_text(
        surface,
        x,
        y,
        LISTING_MESSAGE,
        theme.palette().on_surface_muted.into(),
    );
}

/// The manager-only chrome drawn around the shared browser view.
///
/// The file manager owns write tools and a places rail; the trusted file
/// picker owns neither, and passes [`ManagerChrome::none`]. Grouping them
/// keeps the pieces that appear and disappear together in one value, so a
/// caller cannot draw a rail's rows while hit-testing a window that has none.
///
/// The picker's emptiness is deliberate, not an omission to fill in later. It
/// is a read-only chooser: it has no write authority, so a write tool would be
/// a control it could never honour; and its whole purpose is bounded to the
/// directory tree the requesting application was authorised to be shown, so a
/// rail offering one-click jumps to arbitrary mounted volumes would widen the
/// pick beyond what was asked for. It draws the listing and nothing else.
pub struct ManagerChrome<'a> {
    /// The manager write tools drawn after the shared read-only commands.
    pub tools: &'a [ManagerTool],
    /// Each write tool's enable state; a disabled tool renders muted.
    pub tool_model: ManagerToolModel,
    /// The places rail drawn down the window's leading edge, or `None` for a
    /// view with no rail (the window is then laid out exactly as it is with no
    /// sidebar at all).
    pub sidebar: Option<&'a Places>,
    /// Whether the command toolbar strip is drawn across the top.
    pub toolbar: ToolbarBand,
}

impl ManagerChrome<'_> {
    /// The chrome of a view with no manager surface at all: no write tools, no
    /// enable model, and no places rail — but the shared read-only command
    /// toolbar, which is not a manager surface and which every consumer of the
    /// browser draws by default.
    #[must_use]
    pub const fn none() -> Self {
        Self {
            tools: &[],
            tool_model: ManagerToolModel::none(),
            sidebar: None,
            toolbar: ToolbarBand::Shown,
        }
    }
}

/// The width of the places rail: a row's icon column, the widest fixed place
/// label, and the row padding around them, all measured from the drawn face
/// and the theme's metrics rather than a fixed pixel count, so the rail tracks
/// the interface's density. Clamped to a third of the window so a narrow
/// window keeps most of its width for the listing.
fn sidebar_width(scale: Scale, theme: &Theme, font: BitmapFont, viewport_width: u32) -> u32 {
    let pad = scale.scale_length(theme.metrics().control_inset).max(1);
    font.glyph_height()
        .saturating_add(font.text_width(places::WIDEST_FIXED_LABEL))
        .saturating_add(pad.saturating_mul(3))
        .min(viewport_width / 3)
}

/// The height of the band separating the user's own places from the mounted
/// volumes: the theme's control gap, so the separation reads at the same
/// rhythm as every other gap in the interface.
fn separator_height(scale: Scale, theme: &Theme) -> u32 {
    scale.scale_length(theme.metrics().control_gap).max(1)
}

/// The places rail's geometry for `sidebar` within `window` (the **whole**
/// window), or `None` when there is no rail to draw (no model, or a model with
/// no rows).
///
/// The rail is laid out *below* the command toolbar band — inset at the top by
/// [`chrome_height`], with the rest of the window's height — because the
/// toolbar is window chrome that spans the full width. A view with no toolbar
/// reserves no band, so the rail starts at the top of the window. Its row
/// pitch is [`row_height`], the pitch the list rows use, so the rail's
/// rows land on exactly the row grid of the listing beside them.
///
/// The one definition the painter, the pointer hit-test
/// ([`sidebar_index_at`]), and the content inset ([`content_area`]) all read,
/// so the drawn rail and every measurement of it agree by construction.
#[must_use]
pub fn sidebar_view(
    window: Rect,
    scale: Scale,
    theme: &Theme,
    sidebar: Option<&Places>,
    toolbar: ToolbarBand,
) -> Option<SidebarView> {
    let places = sidebar?;
    if places.is_empty() {
        return None;
    }
    let font = BitmapFont::for_role(theme.fonts(), TextRole::Body, scale);
    let band = chrome_height(scale, theme, toolbar);
    Some(SidebarView::new(
        Rect::new(
            window.origin.x,
            window.origin.y.saturating_add_unsigned(band),
            window.width,
            window.height.saturating_sub(band),
        ),
        sidebar_width(scale, theme, font, window.width),
        row_height(scale, theme),
        separator_height(scale, theme),
        places.len(),
        places.volume_start(),
    ))
}

/// The window area the item view and the scrollbar occupy: the whole
/// `window`, less the places rail on the leading edge when one is drawn.
///
/// The command toolbar is **not** measured here. It is window chrome: its band
/// spans the full window width across the top, above the rail and over this
/// area's own top strip, so [`toolbar_command_at`], [`manager_tool_at`], and
/// [`manager_tool_rect`] take the *window* while every entry point below the
/// band — [`entry_index_at`], [`entry_rect`], [`scrollbar_bounds`] and the
/// overlays — takes this area. A caller drawing a rail resolves this once and
/// passes it wherever it would otherwise pass the window; the rows and the
/// scrollbar then sit exactly where a click looks for them.
///
/// A caller with no rail gets the window back unchanged, so a view without a
/// sidebar (the trusted file picker) is laid out precisely as it was before
/// there was one.
#[must_use]
pub fn content_area(
    window: Rect,
    scale: Scale,
    theme: &Theme,
    sidebar: Option<&Places>,
    toolbar: ToolbarBand,
) -> Rect {
    let Some(view) = sidebar_view(window, scale, theme, sidebar, toolbar) else {
        return window;
    };
    let rail = view.width();
    Rect::new(
        window.origin.x.saturating_add_unsigned(rail),
        window.origin.y,
        window.width.saturating_sub(rail),
        window.height,
    )
}

/// The places-rail row at window-local pixel `point`, or `None` when the point
/// is not on one — above the rail in the toolbar band, outside the rail, in
/// the separation between the user's places and the volumes, or below the last
/// drawn row.
///
/// Takes the **whole** window, the rectangle [`sidebar_view`] lays the rail out
/// in; it is the exact inverse of what [`render_into`] painted, through that one
/// shared geometry.
#[must_use]
pub fn sidebar_index_at(
    window: Rect,
    scale: Scale,
    theme: &Theme,
    sidebar: Option<&Places>,
    toolbar: ToolbarBand,
    point: Point,
) -> Option<usize> {
    let view = sidebar_view(window, scale, theme, sidebar, toolbar)?;
    let x = u32::try_from(point.x).ok()?;
    let y = u32::try_from(point.y).ok()?;
    view.index_at(x, y)
}

/// Paint the places rail: its raised band, one shared [`ListRow`] per place,
/// and the hairline separating the user's own places from the mounted
/// volumes.
///
/// Each row asks `artwork` for its icon at exactly the slot the row will draw
/// it in, so a volume shows the artwork for the medium it really sits on and
/// falls back to the built-in glyph when the system has no asset for it. Rows
/// the window is too short to draw in full are simply not drawn, which is the
/// same set [`SidebarView::index_at`] will resolve a click to.
fn draw_sidebar(
    surface: &mut Surface,
    scale: Scale,
    theme: &Theme,
    places: &Places,
    view: &SidebarView,
    selected: Option<usize>,
    artwork: &mut dyn IconArtwork,
) {
    let palette = theme.palette();
    let rail = view.rail_rect();
    let rail_x = u32::try_from(rail.origin.x).unwrap_or(0);
    let rail_y = u32::try_from(rail.origin.y).unwrap_or(0);
    surface.fill_rect(
        rail_x,
        rail_y,
        rail.width,
        rail.height,
        palette.surface_raised.into(),
    );
    if let Some(band) = view.separator_rect() {
        let pad = scale.scale_length(theme.metrics().control_inset).max(1);
        let x = u32::try_from(band.origin.x)
            .unwrap_or(0)
            .saturating_add(pad);
        let y = u32::try_from(band.origin.y)
            .unwrap_or(0)
            .saturating_add(band.height / 2);
        surface.fill_rect(
            x,
            y,
            band.width.saturating_sub(pad.saturating_mul(2)),
            1,
            palette.on_surface_muted.into(),
        );
    }
    for (index, place) in places.rows().iter().enumerate() {
        let Some(bounds) = view.row_rect(index) else {
            break;
        };
        let row = place_row(place, places, index, selected);
        let side = row.icon_side(bounds, scale, theme);
        let art = artwork.artwork(IconRequest::kind(place.icon()), side);
        row.render(surface, bounds, scale, theme, art);
    }
}

/// Build the shared [`ListRow`] for one rail row, carrying every state the
/// rail can put it in: a place whose target was refused reads disabled (and
/// never also hovered — a control the user cannot use does not light up under
/// the pointer), the row under the pointer hovers, the keyboard cursor's row
/// is focused while the rail owns focus, and the row matching the browser's
/// current location reads selected through the control's own selection state
/// rather than a highlight painted here.
fn place_row(place: &Place, places: &Places, index: usize, selected: Option<usize>) -> ListRow {
    let mut state = if place.is_available() {
        let idle = ControlState::idle();
        if places.hovered() == Some(index) {
            idle.with_pointer(PointerState::Hover)
        } else {
            idle
        }
    } else {
        ControlState::disabled()
    };
    if selected == Some(index) {
        state = state.with_selection(SelectionState::Selected);
    }
    let mut row = ListRow::new(place.label())
        .with_icon(place.icon())
        .with_state(state);
    row.set_in_focus_field(places.is_focused());
    row.set_focused(places.is_focused() && places.cursor() == index);
    row
}

/// Draw the visible list rows below the toolbar as shared [`TableRow`]s,
/// giving the selected entry the row chrome's selection state.
///
/// Each row's identity icon resolves through `artwork` at the row's own icon
/// side, exactly as a grid tile's does — so a row draws the shipped class
/// artwork where there is any and the cached built-in glyph otherwise, and
/// **no** row rasterises vector coverage a previous frame already resolved.
/// Only the rows on screen are asked for.
///
/// A row asks by *class* rather than naming a bundle: a list is a dense text
/// view of a directory that may hold hundreds of bundles, and reading each
/// one's manifest to find its own icon is work a row-height picture cannot
/// show. The grid, whose tiles are large enough to tell two applications
/// apart, names the bundle.
fn draw_list<S: DirectorySource>(
    surface: &mut Surface,
    scale: Scale,
    theme: &Theme,
    browser: &Browser<S>,
    content: Rect,
    toolbar: ToolbarBand,
    artwork: &mut dyn IconArtwork,
) {
    let view = list_view(browser, scale, theme, content, toolbar);
    let offset = browser.scroll_offset();
    let selected = browser.selected_index();
    let parent = browser.components();
    let entries = browser.entries();
    for index in view.visible_range(offset) {
        let Some(entry) = entries.get(index) else {
            break;
        };
        let Some(bounds) = view.row_rect(offset, index) else {
            continue;
        };
        let kind = icon_for_entry(entry, parent);
        let row = entry_row(entry, selected == Some(index), kind);
        let side = TableRow::icon_side(bounds, scale, theme);
        let art = artwork.artwork(IconRequest::kind(kind), side);
        row.render(surface, bounds, scale, theme, &COLUMNS, art);
    }
}

/// Draw the visible icon-grid tiles below the toolbar as shared [`IconTile`]s,
/// giving the selected entry the tile's selection state.
///
/// Each tile's icon is the shared classification ([`icon_for_entry`]): the
/// entry's content type and folder occupancy
/// decide an [`IconKind`], and `artwork` is asked for a
/// pre-rasterised surface at the tile's [`IconTile::icon_side`] slot. When it
/// supplies one the tile draws that artwork; otherwise the tile falls back to
/// the built-in glyph for the kind. The classification is resolved once here so
/// the manager and picker draw the same icon for the same entry.
///
/// An application bundle additionally names *itself* in the request, so the
/// artwork layer can prefer the icon the bundle carries in its own
/// `Resources/` over the generic bundle artwork. Only the tiles actually on
/// screen are asked for, so browsing a store of a thousand applications reads
/// and decodes only the ones in view.
///
/// The grid lays out only whole tiles and spreads each row's leftover width
/// between them ([`GridFill::Spread`]), so a widened window shares the extra
/// space out evenly until one more tile fits. Painting is confined to the item
/// area, so no tile can encroach on the scrollbar gutter beside it or the
/// chrome above it whatever it draws inside its own rectangle, and no tile has
/// to know it sits at an edge.
fn draw_grid<S: DirectorySource>(
    surface: &mut Surface,
    scale: Scale,
    theme: &Theme,
    browser: &Browser<S>,
    content: Rect,
    toolbar: ToolbarBand,
    artwork: &mut dyn IconArtwork,
) {
    let view = grid_view(browser, scale, theme, content, toolbar);
    let Some((area_x, area_y, area_w, area_h)) = area_pixels(view.tile_area()) else {
        return;
    };
    let offset = browser.scroll_offset();
    let selected = browser.selected_index();
    let parent = browser.components();
    let entries = browser.entries();
    // Spelled once for the whole frame; each bundle tile appends its own leaf
    // into one reused buffer rather than allocating a path per tile.
    let dir = crate::vfs::spell_absolute_path(parent);
    let mut bundle = String::new();
    surface.with_clip(area_x, area_y, area_w, area_h, |surface| {
        for index in view.visible_range(offset) {
            let Some(entry) = entries.get(index) else {
                break;
            };
            let Some(bounds) = view.cell_rect(offset, index) else {
                continue;
            };
            let kind = icon_for_entry(entry, parent);
            let request = entry_icon_request(&dir, entry, kind, &mut bundle);
            let mut state = ControlState::idle();
            if selected == Some(index) {
                state.selection = SelectionState::Selected;
            }
            let tile = grid_tile(entry, state, kind);
            let side = IconTile::icon_side(bounds, scale, theme);
            let art = artwork.artwork(request, side);
            tile.render(surface, bounds, scale, theme, art);
        }
    });
}

/// A screen rectangle as surface pixels `(x, y, w, h)`, or `None` when it is
/// off-surface or empty — the shape a clip window is asked for.
fn area_pixels(area: Rect) -> Option<(u32, u32, u32, u32)> {
    if area.width == 0 || area.height == 0 {
        return None;
    }
    Some((
        u32::try_from(area.left()).ok()?,
        u32::try_from(area.top()).ok()?,
        area.width,
        area.height,
    ))
}

/// Draw the vertical [`ScrollBar`] in the reserved right-edge gutter, spanning
/// the item area below the toolbar. A viewport with no room for the gutter
/// (or with no scrollable content) simply draws nothing there.
fn draw_scrollbar<S: DirectorySource>(
    surface: &mut Surface,
    scale: Scale,
    theme: &Theme,
    browser: &Browser<S>,
    viewport: Rect,
    toolbar: ToolbarBand,
) {
    let Some(bounds) = scrollbar_bounds(scale, theme, viewport, toolbar) else {
        return;
    };
    // Draw the browser's own interactive bar (its live hover/drag/held state),
    // with its model re-synced from the current geometry so the thumb size and
    // position match the listing exactly. The bar is `Copy`, so this reflects
    // the live interaction state without disturbing the stored offset owner.
    let mut bar: ScrollBar = *browser.scrollbar();
    bar.set_model(scroll_model(browser, scale, theme, viewport, toolbar));
    bar.render(surface, bounds, scale, theme);
}

/// The screen rectangle (window-local) the vertical [`ScrollBar`] occupies: the
/// reserved right-edge gutter spanning the item area below the toolbar, or
/// `None` when the window is too narrow for a gutter or too short for any item
/// area. This is the exact geometry the drawn scrollbar paints into (and that
/// [`scroll_pointer`] hit-tests against), so a pointer hit-test and the drawn
/// bar can never disagree.
#[must_use]
pub fn scrollbar_bounds(
    scale: Scale,
    theme: &Theme,
    viewport: Rect,
    toolbar: ToolbarBand,
) -> Option<Rect> {
    let gutter = gutter_width(scale, theme, viewport.width);
    let header = chrome_height(scale, theme, toolbar);
    if gutter == 0 || viewport.height <= header {
        return None;
    }
    let content = content_viewport(viewport, scale, theme);
    Some(Rect::new(
        content.origin.x.saturating_add_unsigned(content.width),
        content
            .origin
            .y
            .saturating_add_unsigned(header.min(i32::MAX.unsigned_abs())),
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
#[allow(clippy::too_many_arguments)] // The bar's geometry, the sample, and the round's report.
pub fn scroll_pointer<S: DirectorySource>(
    browser: &mut Browser<S>,
    scale: Scale,
    theme: &Theme,
    viewport: Rect,
    toolbar: ToolbarBand,
    point: Point,
    event: &InputEvent,
    damage: &mut Region,
) -> Option<bool> {
    let bounds = scrollbar_bounds(scale, theme, viewport, toolbar)?;
    let model = scroll_model(browser, scale, theme, viewport, toolbar);
    let bar = browser.scrollbar_mut();
    bar.set_model(model);
    if let ScrollRouted::ScrollTo { offset } =
        route_scroll_bar(bar, bounds, scale, theme, point, event, damage)?
    {
        browser.set_scroll_offset(offset);
    }
    Some(true)
}

/// What routing a pointer event to a drawn scrollbar did.
enum ScrollRouted {
    /// The bar consumed it and asks its owner to scroll to this offset.
    ScrollTo {
        /// The offset the bar resolved, already clamped by its own model.
        offset: u64,
    },
    /// The bar consumed it and only its own drawn state moved — a drag begun,
    /// or a hover.
    Redrawn,
}

/// Route a pointer `event` at window-local `point` to `bar` over `bounds`,
/// answering what the bar made of it or `None` when the pointer had nothing to
/// do with it.
///
/// The one routing every drawn scrollbar in this engine resolves a press
/// through — the listing's ([`scroll_pointer`]) and the "Open With…" chooser's
/// ([`open_with_scroll_pointer`]) — so a bar cannot come to behave differently
/// in one surface from another. The caller has already synced the bar's model
/// to its own geometry; this positions the bar at the event before applying
/// the action, because a press knows which part it landed on only from the
/// pointer's current place (a press and a release carry no position of their
/// own).
fn route_scroll_bar(
    bar: &mut ScrollBar,
    bounds: Rect,
    scale: Scale,
    theme: &Theme,
    point: Point,
    event: &InputEvent,
    damage: &mut Region,
) -> Option<ScrollRouted> {
    let synth = bar.on_pointer(
        &InputEvent::PointerMoved { to: point },
        bounds,
        scale,
        theme,
        damage,
    );
    let pressing_before = bar.is_pressing();
    let on_bar = bar.part_at(bounds, point, scale, theme) != ScrollPart::Outside;
    let (consumed, action) = match event {
        InputEvent::PointerPressed {
            button: PointerButton::Primary,
        } => (on_bar, bar.on_pointer(event, bounds, scale, theme, damage)),
        InputEvent::PointerReleased {
            button: PointerButton::Primary,
        } => (
            pressing_before,
            bar.on_pointer(event, bounds, scale, theme, damage),
        ),
        InputEvent::PointerMoved { .. } => (pressing_before || on_bar, synth),
        _ => (false, None),
    };
    if !consumed {
        return None;
    }
    Some(match action {
        Some(ScrollAction::ScrollTo { offset }) => ScrollRouted::ScrollTo { offset },
        None => ScrollRouted::Redrawn,
    })
}

/// Build the [`TableRow`] for one list entry: a leading name cell carrying the
/// entry's `icon`, a trailing numeric size cell (blank for a directory, a
/// bundle, or a link, none of which carries a meaningful byte size — a link's
/// own size is the length of the path it stores), and a modified-date cell.
///
/// `icon` is the shared classification ([`icon_for_entry`]) the grid tile
/// draws too, so a row and a tile can never picture the same entry
/// differently. The name cell paints the built-in glyph for that kind — the
/// row control takes an [`IconKind`], not cached artwork — which is what makes
/// the kind readable in a view whose rows are otherwise text.
fn entry_row(entry: &Entry, selected: bool, icon: IconKind) -> TableRow {
    let size = if matches!(entry.kind(), EntryKind::File) {
        format_size(entry.size())
    } else {
        String::new()
    };
    let cells = vec![
        TableCell::new(entry_label(entry)).with_icon(icon),
        TableCell::numeric(size),
        TableCell::new(format_date(entry.modified())),
    ];
    let mut row = TableRow::new(cells);
    row.set_selected(selected);
    row
}

/// Build the [`IconTile`] for one grid entry: the entry's file-type `icon`
/// above its label, carrying the shared selection state when selected. The
/// `icon` is the shared classification ([`icon_for_entry`]) resolved by the
/// caller — a display hint only, decided once so a tile and a list row draw
/// the same icon for the same entry.
#[must_use]
pub fn grid_tile(entry: &Entry, state: ControlState, icon: IconKind) -> IconTile {
    IconTile::new(entry_label(entry), icon).with_state(state)
}

/// The name shown for an entry: exactly the name the volume holds.
///
/// No kind suffix is appended — both views carry the entry's icon
/// ([`icon_for_entry`]), so the label is the name and nothing else, and what
/// the user reads on screen is what they type, copy, and rename.
#[must_use]
pub fn entry_label(entry: &Entry) -> String {
    String::from(entry.name())
}

/// Height in pixels of one rendered list row, measured on the theme's own body
/// face at `scale` exactly as [`render_into`] draws them, so hit-testing and
/// painting can never disagree.
#[must_use]
pub fn row_height(scale: Scale, theme: &Theme) -> u32 {
    BitmapFont::for_role(theme.fonts(), TextRole::Body, scale)
        .glyph_height()
        .saturating_add(scale.scale_length(ROW_PADDING).saturating_mul(2))
}

/// The width of the reserved scrollbar gutter for a `viewport_width`-pixel
/// window: the theme's scrollbar breadth, clamped so it never exceeds the
/// window (a window too narrow for the gutter simply has none).
fn gutter_width(scale: Scale, theme: &Theme, viewport_width: u32) -> u32 {
    scale
        .scale_length(theme.metrics().scrollbar_breadth)
        .max(1)
        .min(viewport_width)
}

/// The command toolbar's band: the full width of `window`, at its top.
///
/// The toolbar is window chrome, so it spans the whole window rather than the
/// rail-inset [`content_area`] — it reaches the window's leading edge and the
/// places rail begins below it. One definition, so the drawn toolbar and each
/// of the three hit-tests that invert it cannot place it differently.
/// A hidden band has no rectangle at all rather than a flat one: the shared
/// [`Toolbar`] lays its buttons out from the origin it is given and would
/// resolve a press on the window's top row against a strip nothing painted.
fn toolbar_bounds(scale: Scale, theme: &Theme, window: Rect, toolbar: ToolbarBand) -> Option<Rect> {
    toolbar.is_shown().then(|| {
        Rect::new(
            window.origin.x,
            window.origin.y,
            window.width,
            chrome_height(scale, theme, toolbar),
        )
    })
}

/// The content viewport (the window minus the reserved scrollbar gutter). The
/// item views lay out within this, so no item ever underlaps the scrollbar.
fn content_viewport(viewport: Rect, scale: Scale, theme: &Theme) -> Rect {
    let gutter = gutter_width(scale, theme, viewport.width);
    Rect::new(
        viewport.origin.x,
        viewport.origin.y,
        viewport.width.saturating_sub(gutter),
        viewport.height,
    )
}

/// The dimensions of one grid tile and the gap between tiles, measured on the
/// theme's own body face at `scale`, so a tile grows with the desktop's
/// density instead of staying a fixed pixel count.
///
/// Shared with the desktop's icon column, which lays the same tiles out under
/// a different [`GridFlow`], so the two views can never disagree about how big
/// an icon tile is.
#[must_use]
pub fn grid_metrics(scale: Scale, theme: &Theme) -> GridMetrics {
    let glyph = BitmapFont::for_role(theme.fonts(), TextRole::Body, scale)
        .glyph_height()
        .max(1);
    GridMetrics {
        cell_width: glyph.saturating_mul(6).max(scale.scale_length(48)),
        cell_height: glyph.saturating_mul(5).max(scale.scale_length(48)),
        gap: (glyph / 2).max(scale.scale_length(2)),
    }
}

/// The height in pixels of the command toolbar strip at the top of the window:
/// the theme's control height plus a gap above and below, scaled to physical
/// pixels. One definition so the drawn toolbar, the item area, and the
/// hit-tests all agree on where the chrome band sits.
#[must_use]
pub fn toolbar_height(scale: Scale, theme: &Theme) -> u32 {
    let metrics = theme.metrics();
    let gap = scale.scale_length(metrics.control_gap);
    scale
        .scale_length(metrics.control_height)
        .saturating_add(gap.saturating_mul(2))
        .max(1)
}

/// The total height reserved for the window chrome above the item area: the
/// command toolbar strip when `toolbar` is drawn, and nothing else. This is
/// the header the item views lay out below and the top of the scrollbar
/// gutter, so paint and hit-test share one offset — and it is zero for a view
/// with no strip, whose listing therefore starts at the top of the window
/// rather than below an empty band.
#[must_use]
pub fn chrome_height(scale: Scale, theme: &Theme, toolbar: ToolbarBand) -> u32 {
    if toolbar.is_shown() {
        toolbar_height(scale, theme)
    } else {
        0
    }
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
/// [`ToolbarModel`], spanning the full window width above the item view. A
/// disabled command reads muted rather than vanishing (the model decides
/// which).
#[allow(clippy::too_many_arguments)] // The band, its model, its tools, and the icon lookup.
fn draw_toolbar<S: DirectorySource>(
    surface: &mut Surface,
    scale: Scale,
    theme: &Theme,
    browser: &Browser<S>,
    window: Rect,
    tools: &[ManagerTool],
    tool_model: ManagerToolModel,
    artwork: &mut dyn IconArtwork,
) {
    let toolbar = build_toolbar(ToolbarModel::for_browser(browser), tools, tool_model);
    let Some(bounds) = toolbar_bounds(scale, theme, window, ToolbarBand::Shown) else {
        return;
    };
    toolbar.render(surface, bounds, scale, theme, artwork);
}

/// The actionable toolbar command at window-local pixel `point`, or `None`
/// when the click is not on one — outside the toolbar band, on a group
/// gutter, on a manager write tool, or on a command the [`ToolbarModel`] has
/// disabled (fail closed: a disabled tool does not act). It mirrors the
/// drawn toolbar's own layout so a click resolves to exactly the tool
/// [`render_into`] painted. The read-only commands keep the same positions
/// whether or not write tools follow them, so this needs no `tools` argument.
///
/// `window` is the **whole** window, the rectangle the toolbar band spans —
/// not the rail-inset [`content_area`] the listing takes.
#[must_use]
pub fn toolbar_command_at<S: DirectorySource>(
    browser: &Browser<S>,
    scale: Scale,
    theme: &Theme,
    window: Rect,
    band: ToolbarBand,
    point: Point,
) -> Option<ToolbarCommand> {
    let model = ToolbarModel::for_browser(browser);
    let toolbar = build_toolbar(model, &[], ManagerToolModel::none());
    let bounds = toolbar_bounds(scale, theme, window, band)?;
    let index = toolbar.tool_at(bounds, scale, theme, point)?;
    let command = *chrome::TOOLBAR_COMMANDS.get(index)?;
    model.is_enabled(command).then_some(command)
}

/// The manager-only write [`ManagerTool`] at window-local pixel `point`, or
/// `None` when the click is not on one. `tools` is the same set handed to
/// [`render_into`] (a read-only picker passes an empty slice and so never resolves a
/// write tool). The full toolbar — read-only commands then the write tools — is
/// rebuilt so the write tools sit at exactly the positions [`render_into`] painted
/// them; a hit resolves only in the write-tool index range, so a click
/// on a read-only command returns `None` here (it is handled by
/// [`toolbar_command_at`]). `tool_model` is the same enable state handed to
/// [`render_into`]: a click on a tool the model has disabled resolves to `None`
/// (fail closed — a disabled tool does not act).
///
/// `window` is the **whole** window, the rectangle the toolbar band spans —
/// not the rail-inset [`content_area`] the listing takes.
#[must_use]
#[allow(clippy::too_many_arguments)] // The band, the sample, and the tools with their enable state.
pub fn manager_tool_at<S: DirectorySource>(
    browser: &Browser<S>,
    scale: Scale,
    theme: &Theme,
    window: Rect,
    band: ToolbarBand,
    point: Point,
    tools: &[ManagerTool],
    tool_model: ManagerToolModel,
) -> Option<ManagerTool> {
    let toolbar = build_toolbar(ToolbarModel::for_browser(browser), tools, tool_model);
    let bounds = toolbar_bounds(scale, theme, window, band)?;
    let index = toolbar.tool_at(bounds, scale, theme, point)?;
    let tool_index = index.checked_sub(chrome::TOOLBAR_COMMANDS.len())?;
    let tool = tools.get(tool_index).copied()?;
    tool_model.is_enabled(tool).then_some(tool)
}

/// The window-local [`Rect`] the manager write `tool` occupies, or `None`
/// when `tool` is not among `tools`. The forward mirror of
/// [`manager_tool_at`] over the same rebuilt toolbar (read-only commands then
/// the write tools), so a caller that must aim *at* a write tool — the desktop
/// integration harness that clicks New Folder — reads the exact geometry
/// [`render_into`] paints and [`manager_tool_at`] hit-tests, never a hand-copied
/// position. Fails closed: an out-of-range or unlisted tool is `None`.
///
/// The toolbar left-packs fixed-width buttons and a disabled tool renders in
/// place (muted, never hidden), so a tool's rectangle is independent of its
/// enable state; the geometry is built with every tool enabled
/// ([`ManagerToolModel::new(true)`](ManagerToolModel::new)) and a caller that
/// must only *act* on an enabled tool gates that through [`manager_tool_at`].
///
/// `window` is the **whole** window, the rectangle the toolbar band spans —
/// not the rail-inset [`content_area`] the listing takes.
#[must_use]
pub fn manager_tool_rect<S: DirectorySource>(
    browser: &Browser<S>,
    scale: Scale,
    theme: &Theme,
    window: Rect,
    band: ToolbarBand,
    tools: &[ManagerTool],
    tool: ManagerTool,
) -> Option<Rect> {
    let position = tools.iter().position(|&t| t == tool)?;
    let toolbar = build_toolbar(
        ToolbarModel::for_browser(browser),
        tools,
        ManagerToolModel::new(true),
    );
    let bounds = toolbar_bounds(scale, theme, window, band)?;
    let index = chrome::TOOLBAR_COMMANDS.len().checked_add(position)?;
    toolbar.tool_rect(index, bounds, scale, theme)
}

/// The [`ListView`] for `browser` at the given content viewport.
fn list_view<S: DirectorySource>(
    browser: &Browser<S>,
    scale: Scale,
    theme: &Theme,
    content: Rect,
    toolbar: ToolbarBand,
) -> ListView {
    ListView::new(
        content,
        row_height(scale, theme),
        chrome_height(scale, theme, toolbar),
        browser.entries().len(),
    )
}

/// The [`GridView`] for `browser` at the given content viewport.
///
/// The window is resizable, so the grid spreads ([`GridFill::Spread`]): a row's
/// leftover width is shared out evenly between its tiles rather than parked as
/// a blank margin at the trailing edge, and widening the window past one more
/// tile re-flows the listing into the extra column.
fn grid_view<S: DirectorySource>(
    browser: &Browser<S>,
    scale: Scale,
    theme: &Theme,
    content: Rect,
    toolbar: ToolbarBand,
) -> GridView {
    GridView::new(
        content,
        grid_metrics(scale, theme),
        chrome_height(scale, theme, toolbar),
        browser.entries().len(),
        GridFlow::RowsFromLeading,
        GridFill::Spread,
    )
}

/// The scroll model the drawn [`ScrollBar`] and the wheel share: the active
/// view's clamped [`ScrollRange`], stepping one line at a time and one visible
/// page per page gesture. `theme` supplies the scrollbar gutter width so the
/// model measures the same content viewport the renderer draws.
#[must_use]
pub fn scroll_model<S: DirectorySource>(
    browser: &Browser<S>,
    scale: Scale,
    theme: &Theme,
    viewport: Rect,
    toolbar: ToolbarBand,
) -> ScrollModel {
    let view = view_layout_for(browser, scale, theme, viewport, toolbar);
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
    scale: Scale,
    theme: &Theme,
    viewport: Rect,
    toolbar: ToolbarBand,
    delta: i64,
) -> bool {
    let view = view_layout_for(browser, scale, theme, viewport, toolbar);
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
    scale: Scale,
    theme: &Theme,
    viewport: Rect,
    toolbar: ToolbarBand,
) {
    let view = view_layout_for(browser, scale, theme, viewport, toolbar);
    let revealed = view.reveal(browser.scroll_offset(), browser.selected_index());
    browser.set_scroll_offset(revealed);
}

/// The index of the entry at window-local pixel `point` for the browser's
/// current view and scroll offset, or `None` for the toolbar band, an empty
/// gap, the scrollbar gutter, and any coordinate outside the item area.
///
/// This mirrors [`render_into`]'s own layout through the shared [`ViewLayout`], so a
/// pointer-driven view resolves a click to exactly the item the user saw —
/// never a re-derived guess. `theme` supplies the same scrollbar gutter width
/// the renderer reserved.
#[must_use]
pub fn entry_index_at<S: DirectorySource>(
    browser: &Browser<S>,
    scale: Scale,
    theme: &Theme,
    viewport: Rect,
    toolbar: ToolbarBand,
    point: Point,
) -> Option<usize> {
    let x = u32::try_from(point.x).ok()?;
    let y = u32::try_from(point.y).ok()?;
    let view = view_layout_for(browser, scale, theme, viewport, toolbar);
    view.index_at(browser.scroll_offset(), x, y)
}

/// The window-local pixel rectangle entry `index` is drawn in, or `None` when
/// it is scrolled out of view (or the view seats nothing there).
///
/// This is [`render_into`]'s own layout for that entry, through the shared
/// [`ViewLayout`], so a caller reporting damage for a mark that moved between
/// two entries names exactly the rectangles the renderer painted. A caller
/// reveals the selection first (via [`reveal_selection`]) if it needs an
/// entry's rect to be on screen.
#[must_use]
pub fn entry_rect<S: DirectorySource>(
    browser: &Browser<S>,
    scale: Scale,
    theme: &Theme,
    viewport: Rect,
    toolbar: ToolbarBand,
    index: usize,
) -> Option<Rect> {
    let view = view_layout_for(browser, scale, theme, viewport, toolbar);
    view.item_rect(browser.scroll_offset(), index)
}

/// The window-local pixel rectangle the browser's currently selected item
/// draws its **name** in, or `None` when nothing is selected or the selection
/// is scrolled out of view.
///
/// This is where the in-place rename editor goes: the whole item rectangle is
/// the row (icon, name, size and date columns) or the whole tile (picture
/// above the label), and a field laid over either covers what the user is not
/// editing. [`entry_rect`] stays the *item's* rectangle, which is what a
/// damage report needs.
#[must_use]
pub fn selection_name_rect<S: DirectorySource>(
    browser: &Browser<S>,
    scale: Scale,
    theme: &Theme,
    viewport: Rect,
    toolbar: ToolbarBand,
) -> Option<Rect> {
    entry_name_rect(
        browser,
        scale,
        theme,
        viewport,
        toolbar,
        browser.selected_index()?,
    )
}

/// The window-local pixel rectangle entry `index` draws its **name** in, or
/// `None` when it is scrolled out of view (or the view seats no name there).
///
/// Read from the drawn controls themselves — the list row's own name-cell text
/// span, the grid tile's own label band — so an overlay cannot land where the
/// name is not. The band is grown to a field's own height where it is shorter
/// (a tile's label band is one line of glyphs, and a field wants its plate)
/// and clamped back into the item's rectangle, so the editor never spills onto
/// a neighbour.
#[must_use]
pub fn entry_name_rect<S: DirectorySource>(
    browser: &Browser<S>,
    scale: Scale,
    theme: &Theme,
    viewport: Rect,
    toolbar: ToolbarBand,
    index: usize,
) -> Option<Rect> {
    let view = view_layout_for(browser, scale, theme, viewport, toolbar);
    let item = view.item_rect(browser.scroll_offset(), index)?;
    let name = match view {
        // The name is the first cell, and the row control reports the span
        // its glyphs occupy inside that column.
        ViewLayout::List(_) => {
            let entry = browser.entries().get(index)?;
            let kind = icon_for_entry(entry, browser.components());
            entry_row(entry, false, kind).cell_text_rect(item, scale, theme, &COLUMNS, 0)?
        }
        ViewLayout::Grid(_) => IconTile::label_rect(item, scale, theme)?,
    };
    let height = name.height.max(TextField::height(scale, theme));
    Some(Rect::new(name.left(), name.top(), name.width, height).intersection(&item))
        .filter(|rect| !rect.is_empty())
}

/// The window-local pixel rectangle the item area occupies — every entry the
/// view draws, and nothing else.
///
/// A scroll moves every row at once, and a listing change replaces them all,
/// so this is what such a round repaints. It is the renderer's own content
/// viewport, so the reported rectangle and the painted one are the same fact.
#[must_use]
pub fn item_area(scale: Scale, theme: &Theme, viewport: Rect) -> Rect {
    content_viewport(viewport, scale, theme)
}

/// The half-open range of entry indices `browser` currently draws at
/// `viewport` — the one definition of "what is on screen", whichever view is
/// active.
///
/// [`render_into`] iterates exactly this range, so a caller that resolves per-entry
/// state through it (the file manager's folder-occupancy probe,
/// [`Browser::resolve_occupancy`]) pays for precisely the entries the next
/// frame paints, and the two can never disagree about which those are.
///
/// [`Browser::resolve_occupancy`]: crate::Browser::resolve_occupancy
#[must_use]
pub fn visible_range<S: DirectorySource>(
    browser: &Browser<S>,
    scale: Scale,
    theme: &Theme,
    viewport: Rect,
    toolbar: ToolbarBand,
) -> core::ops::Range<usize> {
    view_layout_for(browser, scale, theme, viewport, toolbar).visible_range(browser.scroll_offset())
}

/// The resolved view layout for `browser` at `viewport` — the one dispatch the
/// scroll helpers and the pointer hit-test share, laid out within the same
/// content viewport (window minus the scrollbar gutter) the renderer uses.
fn view_layout_for<S: DirectorySource>(
    browser: &Browser<S>,
    scale: Scale,
    theme: &Theme,
    viewport: Rect,
    toolbar: ToolbarBand,
) -> ViewLayout {
    let content = content_viewport(viewport, scale, theme);
    match browser.view_mode() {
        ViewMode::List => ViewLayout::List(list_view(browser, scale, theme, content, toolbar)),
        ViewMode::Grid => ViewLayout::Grid(grid_view(browser, scale, theme, content, toolbar)),
    }
}

/// The most label/value rows [`properties_rows`] can produce — the field count
/// a surface sized for the fields must reserve room for.
pub const PROPERTY_ROW_COUNT: usize = Field::ALL.len();

/// One metadata field a Properties surface shows.
///
/// A closed vocabulary rather than a label/value list, so the display order,
/// each field's label, each field's value, and which fields a given node shows
/// at all are one definition — and so a surface can place a control on a
/// field's row without paying to format every value to find out where it is.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
enum Field {
    /// The human kind label.
    Kind,
    /// The spelling a symbolic link stores.
    Alias,
    /// Apparent size, with the on-disk allocation beside it.
    Size,
    /// The symbolic mode, with its octal spelling beside it.
    Permissions,
    /// The owning user and group ids.
    Owner,
    /// The four timestamps.
    Created,
    /// See [`Field::Created`].
    Modified,
    /// See [`Field::Created`].
    Accessed,
    /// See [`Field::Created`].
    Changed,
}

impl Field {
    /// Every field, in display order.
    const ALL: [Self; 9] = [
        Self::Kind,
        Self::Alias,
        Self::Size,
        Self::Permissions,
        Self::Owner,
        Self::Created,
        Self::Modified,
        Self::Accessed,
        Self::Changed,
    ];

    /// The field's label.
    const fn label(self) -> &'static str {
        match self {
            Self::Kind => "Kind",
            Self::Alias => "Alias to",
            Self::Size => "Size",
            Self::Permissions => "Permissions",
            Self::Owner => "Owner",
            Self::Created => "Created",
            Self::Modified => "Modified",
            Self::Accessed => "Accessed",
            Self::Changed => "Changed",
        }
    }

    /// Whether this field has anything to say about `props`.
    ///
    /// Only the alias row is conditional: a node that stores no target has
    /// nothing to put there, and an empty field would read as a broken link.
    fn shown(self, props: &Properties) -> bool {
        !matches!(self, Self::Alias) || props.target().is_some()
    }

    /// The field's value, straight from the model.
    fn value(self, props: &Properties) -> String {
        match self {
            Self::Kind => String::from(props.kind_label()),
            Self::Alias => String::from(props.target().unwrap_or_default()),
            Self::Size => alloc::format!(
                "{} ({} on disk)",
                props.size_display(),
                props.allocated_display()
            ),
            Self::Permissions => {
                alloc::format!("{} ({})", props.permissions(), props.mode_octal())
            }
            Self::Owner => alloc::format!("uid {} / gid {}", props.uid(), props.gid()),
            Self::Created => props.created_display(),
            Self::Modified => props.modified_display(),
            Self::Accessed => props.accessed_display(),
            Self::Changed => props.changed_display(),
        }
    }

    /// The fields `props` shows, in display order.
    fn shown_in(props: &Properties) -> impl Iterator<Item = Self> + '_ {
        Self::ALL.into_iter().filter(|field| field.shown(props))
    }

    /// Which drawn row `self` lands on for `props`, or `None` when the node
    /// does not show it.
    fn row(self, props: &Properties) -> Option<usize> {
        Self::shown_in(props).position(|field| field == self)
    }
}

/// The labelled metadata fields a Properties surface shows for `props`, in
/// display order: kind, a link's stored target, size (apparent + on-disk),
/// permissions (symbolic + octal), owner, and the four timestamps.
///
/// Every value comes straight from the [`Properties`] model — itself taken
/// straight from `fs_stat` — so a timestamp the backing does not keep renders
/// blank rather than a fabricated wall time, and no field is invented. The
/// alias row appears only for a node that stores a target, and carries the
/// spelling the link holds verbatim: that is what explains a broken one.
#[must_use]
pub fn properties_rows(props: &Properties) -> Vec<(&'static str, String)> {
    Field::shown_in(props)
        .map(|field| (field.label(), field.value(props)))
        .collect()
}

/// The centered bounds of the read-only Properties popup [`Panel`] within
/// `viewport`, sized to comfortably show the [`properties_rows`] fields (a
/// title bar plus one line per field with a top and bottom margin) and clamped
/// to the window so a small window still yields a drawable — if clipped —
/// panel rather than a panic.
///
/// The trusted read-only picker draws this over the listing it is showing, so
/// it takes the same four-fifths proportion as every other surface
/// centred over a window. The file manager's *editable* Properties surface is
/// a window of its own and is laid out from its own client area instead
/// ([`draw_properties_window`]).
#[must_use]
pub fn properties_panel_rect(viewport: Rect, scale: Scale, theme: &Theme) -> Rect {
    let line = row_height(scale, theme);
    let title = scale.scale_length(theme.metrics().title_bar_height).max(1);
    let rows = u32::try_from(PROPERTY_ROW_COUNT).unwrap_or(u32::MAX);
    let content = line.saturating_mul(rows.saturating_add(2));
    let height = title.saturating_add(content).min(viewport.height.max(1));
    let width = overlay_width(viewport);
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

/// The shared column geometry of a Properties surface's content area: the label
/// column x, the value column x, and the per-row pitch. One definition so the
/// drawn fields and the controls laid over them line up exactly.
struct FieldLayout {
    /// Left x of the label column.
    left: i32,
    /// Left x of the value column (past the widest label plus a gap).
    value_x: i32,
    /// Top y of the first field row — the origin every band below is measured
    /// from, so nothing re-derives the content's own margin.
    top: i32,
    /// Vertical pitch between successive rows.
    line: u32,
}

impl FieldLayout {
    fn resolve(content: Rect, scale: Scale, theme: &Theme, font: BitmapFont) -> Self {
        let label_col = Field::ALL
            .into_iter()
            .map(|field| font.text_width(field.label()))
            .max()
            .unwrap_or(0);
        let pad = scale.scale_length(LABEL_PADDING);
        let gap = font.text_width("  ").max(pad);
        let left = content.left().saturating_add(to_i32(pad));
        let value_x = left.saturating_add(to_i32(label_col.saturating_add(gap)));
        Self {
            left,
            value_x,
            top: content
                .top()
                .saturating_add(to_i32(scale.scale_length(ROW_PADDING))),
            line: row_height(scale, theme),
        }
    }

    /// Top y of the row at `index`.
    fn row_y(&self, index: u32) -> i32 {
        self.top
            .saturating_add(to_i32(self.line.saturating_mul(index)))
    }
}

/// Draw the [`properties_rows`] metadata fields as muted-label / solid-value
/// rows within `content`, clipping at the content's bottom edge. Shared by the
/// read-only panel and the manager's window so the fields read identically.
fn draw_property_fields(
    surface: &mut Surface,
    props: &Properties,
    content: Rect,
    scale: Scale,
    theme: &Theme,
    font: BitmapFont,
) {
    let palette = theme.palette();
    let layout = FieldLayout::resolve(content, scale, theme, font);
    let bottom = content.top().saturating_add(to_i32(content.height));
    let mut y = layout.top;
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

/// Draw the read-only Properties overlay for `props` centered in `viewport`: a
/// panel titled with the node's name and its labelled metadata fields drawn as
/// muted-label / solid-value rows in the panel's content area.
///
/// The trusted read-only picker draws this. It reads only the already-
/// authorised [`Properties`] and draws — it performs no I/O and holds no
/// authority. Every blit clips, so a window too small for the whole panel
/// simply shows what fits rather than panicking.
pub fn draw_properties(
    surface: &mut Surface,
    props: &Properties,
    scale: Scale,
    theme: &Theme,
    viewport: Rect,
) {
    let font = BitmapFont::for_role(theme.fonts(), TextRole::Body, scale);
    let bounds = properties_panel_rect(viewport, scale, theme);
    let panel = Panel::new(props.name());
    panel.render(surface, bounds, scale, theme);
    let Some(content) = panel.content_rect(bounds, scale, theme) else {
        return;
    };
    draw_property_fields(surface, props, content, scale, theme, font);
}

/// The nine settable owner/group/other × read/write/execute permission bits, in
/// the left-to-right order the permission control lays them out (the owner
/// triad, then group, then other) — the same order as the symbolic `rwxrwxrwx`
/// spelling they sit over, so the drawn toggles and their hit-test share one
/// definition of which cell carries which bit.
///
/// Only these nine `rwx` bits are offered as toggles — the familiar, legible
/// permission set. The setuid/setgid/sticky bits stay visible in the
/// Properties fields' octal and symbolic spelling and are edited through the
/// `chmod` command: a deliberate scope boundary for a best-in-class,
/// bloat-free surface, not an omission. Toggling a cell flips only its own
/// `rwx` bit and preserves whatever the higher bits currently are.
pub const PERMISSION_BITS: [u32; 9] = [
    0o400, 0o200, 0o100, // owner: read, write, execute
    0o040, 0o020, 0o010, // group: read, write, execute
    0o004, 0o002, 0o001, // other: read, write, execute
];

/// Which of the nine [`PERMISSION_BITS`] `mode` currently sets, in the same
/// left-to-right order — the one definition the drawn toggles' states and their
/// tests read, so a toggle can never disagree with the mode it depicts.
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

/// The permissions grid's column headers, over the read/write/execute columns.
const PERMISSION_COLUMN_LABELS: [&str; 3] = ["Read", "Write", "Exec"];

/// The permissions grid's row labels, naming each `rwx` triad.
const PERMISSION_ROW_LABELS: [&str; 3] = ["Owner", "Group", "Other"];

/// The section label the extended attributes are listed under.
const ATTR_SECTION_LABEL: &str = "Extended attributes";

/// What the attributes section says in place of a list it has no rows for.
const ATTR_UNSUPPORTED: &str = "not stored by this volume";

/// What the attributes section says for a node that carries none.
const ATTR_NONE: &str = "none";

/// What the attributes section says when the listing itself was refused.
const ATTR_REFUSED: &str = "could not be read";

/// The shared geometry of a Properties surface's labelled permissions grid: a
/// column-header row (Read / Write / Execute) above three owner/group/other
/// triad rows, each triad a leading row label followed by its three `rwx`
/// checkboxes.
///
/// One definition so the painted grid, its headers and row labels, and the
/// click hit-test all agree on where every cell sits — the checkboxes are laid
/// out on a real grid pitch (never crammed one glyph apart), so they no longer
/// overlap and each column and row reads under its own label.
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

/// Which of the two owning ids the inline ownership control edits.
///
/// The owning user (`uid`) and group (`gid`) are the two independently
/// editable values on a Properties surface's owner row; a click resolves to
/// exactly one of them and the caller commits that one field.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub enum OwnerField {
    /// The owning user id (`chown`).
    Uid,
    /// The owning group id (`chgrp`).
    Gid,
}

/// The geometry of a Properties surface's owner row: the clickable bounds of
/// the uid and gid values (`[uid, gid]`), each sized to the digits it shows,
/// and the row pitch the active editor is sized from.
struct OwnerRowGeom {
    /// The uid value's cell and the gid value's cell, left-to-right.
    cells: [Rect; 2],
    /// The vertical pitch between rows — the height the inline editor uses so
    /// it is tall enough to read while it overlays the value.
    line: u32,
}

/// What a press on a Properties window resolves to.
///
/// One hit-test rather than one per control, so the precedence between them is
/// stated once: the capability-free permission toggles are resolved before the
/// privileged ownership control, and a press on nothing resolves to nothing —
/// never to whichever control happens to be nearest (fail closed).
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum PropertiesTarget {
    /// One of the nine permission toggles, by the `rwx` bit it flips.
    Permission(u32),
    /// An owning id's value, which only a holder of `CAP_FS_CHOWN` may edit.
    Owner(OwnerField),
    /// An attribute row, by its index in the visible set.
    Attribute(usize),
    /// The `key = value` editor's field.
    Editor,
    /// One of the attribute actions beneath the list.
    Action(AttrAction),
}

/// What a press on the attributes action band asks for.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub enum AttrAction {
    /// Apply the editor's `key = value` line to the node.
    Set,
    /// Remove the attribute the cursor row names.
    Remove,
}

/// The two attribute actions, in the order they are drawn.
const ATTR_ACTIONS: [(AttrAction, &str); 2] =
    [(AttrAction::Remove, "Remove"), (AttrAction::Set, "Set")];

/// Where a Properties window's parts sit within its client area.
///
/// Resolved once from the node's own fields, so the painter and every
/// hit-test read one answer: the metadata fields, the permissions grid and
/// ownership control beneath them, and the extended-attribute list with its
/// editor at the foot. Each band is [`None`] when the client leaves it no
/// room, so a window dragged too small draws and resolves nothing there
/// rather than placing a control off its own surface.
struct PropertiesLayout {
    /// The band the metadata fields are drawn in.
    fields: Rect,
    /// The permissions grid.
    grid: Option<PermGrid>,
    /// The owner row's two clickable id cells.
    owner: Option<OwnerRowGeom>,
    /// Top y of the attributes section label.
    label_y: i32,
    /// The band the attribute rows occupy, gutter included.
    rows: Option<Rect>,
    /// The editor's row, the actions band included.
    editor: Option<Rect>,
}

/// The rows a Properties window reserves between the metadata fields and the
/// permissions grid, and between the grid and the attributes section: one blank
/// separator each, plus the grid's own column-header row.
const PROPERTIES_GRID_ROWS: u32 = 5;

impl PropertiesLayout {
    /// Resolve every band of the window from its `content` (its whole client
    /// area) and the node it is showing.
    fn resolve(
        props: &Properties,
        content: Rect,
        scale: Scale,
        theme: &Theme,
        font: BitmapFont,
    ) -> Self {
        let layout = FieldLayout::resolve(content, scale, theme, font);
        let line = layout.line.max(1);
        let bottom = content.top().saturating_add(to_i32(content.height));
        let field_rows = u32::try_from(Field::shown_in(props).count()).unwrap_or(u32::MAX);

        // The editor claims the last full row of the client; everything above
        // it is laid out from the top, so a window too short simply loses its
        // lower bands rather than overlapping them.
        let editor_top = bottom.saturating_sub(to_i32(line));
        let grid = perm_grid(&layout, content, scale, font, field_rows);
        let owner = owner_row_geom(props, &layout, content, font);
        let label_y = layout.row_y(
            field_rows
                .saturating_add(PROPERTIES_GRID_ROWS)
                .saturating_add(1),
        );
        let rows_top = label_y.saturating_add(to_i32(line));
        let list = (rows_top.saturating_add(to_i32(line)) <= editor_top).then(|| {
            let height = u32::try_from(editor_top.saturating_sub(rows_top)).unwrap_or(0);
            Rect::new(content.left(), rows_top, content.width, height)
        });
        let editor = (editor_top >= rows_top).then(|| {
            Rect::new(
                content
                    .left()
                    .saturating_add(to_i32(scale.scale_length(LABEL_PADDING))),
                editor_top,
                content
                    .width
                    .saturating_sub(scale.scale_length(LABEL_PADDING).saturating_mul(2)),
                line,
            )
        });
        Self {
            fields: content,
            grid,
            owner,
            label_y,
            rows: list,
            editor,
        }
    }

    /// How many attribute rows the list band shows.
    fn visible_rows(&self, line: u32) -> usize {
        match (self.rows, line) {
            (Some(band), pitch) if pitch > 0 => (band.height / pitch) as usize,
            _ => 0,
        }
    }

    /// The rectangle attribute row `slot` is drawn in, the scroll gutter
    /// excluded.
    fn row_rect(&self, slot: usize, line: u32, gutter: u32) -> Option<Rect> {
        let band = self.rows?;
        let top = band.top().saturating_add(to_i32(
            line.saturating_mul(u32::try_from(slot).unwrap_or(u32::MAX)),
        ));
        Some(Rect::new(
            band.left(),
            top,
            band.width.saturating_sub(gutter),
            line,
        ))
    }

    /// The scroll gutter beside the attribute rows, or [`None`] when the band
    /// is too narrow for one.
    fn gutter_rect(&self, gutter: u32) -> Option<Rect> {
        let band = self.rows?;
        (gutter > 0).then(|| {
            Rect::new(
                band.left()
                    .saturating_add(to_i32(band.width.saturating_sub(gutter))),
                band.top(),
                gutter,
                band.height,
            )
        })
    }

    /// The editor's text field and the action buttons beside it, or [`None`]
    /// when the row does not fit.
    fn editor_parts(
        &self,
        scale: Scale,
        font: BitmapFont,
    ) -> Option<(Rect, [Rect; ATTR_ACTIONS.len()])> {
        let row = self.editor?;
        let pad = open_with_action_pad(scale, font);
        let widths = ATTR_ACTIONS.map(|(_, label)| {
            font.text_width(label)
                .saturating_add(pad.saturating_mul(2))
                .min(row.width)
        });
        let mut right = row.left().saturating_add(to_i32(row.width));
        let mut rects = [Rect::EMPTY; ATTR_ACTIONS.len()];
        for slot in (0..ATTR_ACTIONS.len()).rev() {
            let left = right.saturating_sub(to_i32(widths[slot]));
            rects[slot] = Rect::new(left, row.top(), widths[slot], row.height);
            right = left.saturating_sub(to_i32(pad));
        }
        let field_width = u32::try_from(right.saturating_sub(row.left())).unwrap_or(0);
        let field = Rect::new(row.left(), row.top(), field_width, row.height);
        (field.width > 0).then_some((field, rects))
    }
}

/// The permissions-grid geometry within `content`, or `None` when the grid
/// does not fit — so the painter and the hit-test both fail closed there
/// rather than placing cells off the surface.
fn perm_grid(
    layout: &FieldLayout,
    content: Rect,
    scale: Scale,
    font: BitmapFont,
    field_rows: u32,
) -> Option<PermGrid> {
    let line = layout.line.max(1);
    let box_side = font.glyph_height().max(1);
    // The metadata fields occupy the rows above; the grid sits below a
    // one-row blank separator, its column headers one row above the three
    // triad rows.
    let header_y = layout.row_y(field_rows.saturating_add(1));
    let first_row_y = header_y.saturating_add(to_i32(line));
    let last_row_bottom = first_row_y
        .saturating_add(to_i32(line.saturating_mul(2)))
        .saturating_add(to_i32(box_side));
    let content_bottom = content.top().saturating_add(to_i32(content.height));
    if last_row_bottom > content_bottom {
        return None;
    }
    let pad = scale.scale_length(LABEL_PADDING);
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
    let gap = font.text_width("  ").max(pad);
    let label_x = layout.left;
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

/// The owner row's geometry, or `None` when the row does not fit the content.
///
/// The cells are measured from the same `uid N / gid N` spelling
/// [`properties_rows`] draws, on whichever row that spelling landed on, so a
/// click lands exactly on the number it edits however many fields precede it.
fn owner_row_geom(
    props: &Properties,
    layout: &FieldLayout,
    content: Rect,
    font: BitmapFont,
) -> Option<OwnerRowGeom> {
    let index = Field::Owner.row(props)?;
    let glyph = font.glyph_height().max(1);
    let row_index = u32::try_from(index).unwrap_or(u32::MAX);
    let row_top = layout.row_y(row_index);
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

/// The width, in pixels, the active owner editor is drawn at — comfortably
/// wider than a single number so a `u32` id (up to ten digits) is readable
/// while typed.
fn owner_editor_width(font: BitmapFont) -> u32 {
    font.text_width("0000000000").max(1)
}

/// The rectangle the inline editor occupies over `field`'s value: the value's
/// own origin, widened to the readable editor width but never past the
/// content, and as tall as the row pitch.
fn owner_editor_geom(
    geom: &OwnerRowGeom,
    field: OwnerField,
    content: Rect,
    font: BitmapFont,
) -> Rect {
    let cell = match field {
        OwnerField::Uid => geom.cells[0],
        OwnerField::Gid => geom.cells[1],
    };
    let right = content.left().saturating_add(to_i32(content.width));
    let avail = u32::try_from(right.saturating_sub(cell.left()))
        .unwrap_or(0)
        .max(1);
    let width = owner_editor_width(font).min(avail);
    Rect::new(cell.left(), cell.top(), width, geom.line)
}

/// What a Properties window is showing right now.
///
/// The read a window is opened by leaves the loop (a node's metadata is one
/// `fs_stat` and its attributes one call per key), so a window states that it
/// is reading, or why it could not, rather than showing an empty or invented
/// summary until the answer lands.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum PropertiesFrame<'a> {
    /// The read is in flight.
    Reading,
    /// The read was refused; the reason is stated on the surface.
    Refused(&'a str),
    /// The node's metadata and attributes, as the read found them.
    Ready(&'a Properties),
}

/// How far the attribute list is scrolled and which of its rows the keyboard
/// acts on — the drawn state the window's own [`RowList`] holds.
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
pub struct AttrView {
    /// The first visible attribute row.
    pub offset: u64,
    /// The row the keyboard and the Remove action act on.
    pub cursor: usize,
}

/// The Properties window's default extent in physical pixels at `scale`: wide
/// enough for the label and value columns and tall enough to show the metadata
/// fields, the permissions grid, and the head of the attributes list.
///
/// A window, so the user may resize it; this is only what it opens at. The
/// attribute list scrolls inside whatever it is given, so a shorter window
/// loses rows from the list and never a field.
#[must_use]
pub fn properties_window_extent(scale: Scale, theme: &Theme) -> (u32, u32) {
    let line = row_height(scale, theme).max(1);
    // Every field, the grid and its separators, the attributes label, a few
    // rows of list, and the editor.
    let rows = u32::try_from(PROPERTY_ROW_COUNT)
        .unwrap_or(u32::MAX)
        .saturating_add(PROPERTIES_GRID_ROWS)
        .saturating_add(1)
        .saturating_add(PROPERTIES_OPEN_ATTR_ROWS)
        .saturating_add(1);
    (
        scale.scale_length(PROPERTIES_WINDOW_WIDTH).max(1),
        line.saturating_mul(rows)
            .saturating_add(scale.scale_length(ROW_PADDING).saturating_mul(2))
            .max(1),
    )
}

/// The Properties window's width when it opens, in logical pixels at the
/// reference density: room for the label column, a value as long as a
/// timestamp or a path, and the attribute rows beside their values.
const PROPERTIES_WINDOW_WIDTH: u32 = 420;

/// How many attribute rows the window opens tall enough to show. The list
/// scrolls, so this is a starting size and not a bound on what a node may
/// carry.
const PROPERTIES_OPEN_ATTR_ROWS: u32 = 5;

/// Draw a Properties window's whole client area for `frame`.
///
/// The window's own title bar names the node, so the client is the fields, the
/// permissions grid, and the extended-attribute list — no second panel header
/// inside a window that already has one. `view` places the attribute list;
/// `editor` and `owner`, when present, are drawn over the rows they belong to.
///
/// It reads only the already-authorised [`Properties`] and draws: no I/O, no
/// authority, and every blit clips, so a window dragged small shows what fits
/// rather than panicking. The ownership control is drawn only when `owner` is
/// supplied, which the caller does only where the launching user holds
/// `CAP_FS_CHOWN` — a session that cannot reassign an owner is never shown the
/// control.
pub fn draw_properties_window(
    surface: &mut Surface,
    frame: PropertiesFrame<'_>,
    view: AttrView,
    controls: PropertiesControls<'_>,
    scale: Scale,
    theme: &Theme,
    window: Rect,
) {
    let font = BitmapFont::for_role(theme.fonts(), TextRole::Body, scale);
    let palette = theme.palette();
    surface.fill_rect(
        u32::try_from(window.left()).unwrap_or(0),
        u32::try_from(window.top()).unwrap_or(0),
        window.width,
        window.height,
        palette.surface.into(),
    );
    let left = window
        .left()
        .saturating_add(to_i32(scale.scale_length(LABEL_PADDING)));
    let first_row = window
        .top()
        .saturating_add(to_i32(scale.scale_length(ROW_PADDING)));
    let props = match frame {
        PropertiesFrame::Reading => {
            font.draw_text(
                surface,
                left,
                first_row,
                PROPERTIES_READING,
                palette.on_surface_muted.into(),
            );
            return;
        }
        PropertiesFrame::Refused(reason) => {
            font.draw_text(surface, left, first_row, reason, palette.on_surface.into());
            return;
        }
        PropertiesFrame::Ready(props) => props,
    };
    let layout = PropertiesLayout::resolve(props, window, scale, theme, font);
    let line = row_height(scale, theme).max(1);
    draw_property_fields(surface, props, layout.fields, scale, theme, font);
    draw_permission_grid(surface, props, &layout, scale, theme, font);
    if let Some(geom) = layout.owner.as_ref() {
        if controls.can_chown {
            draw_owner_control(surface, geom, window, scale, theme, font, controls.owner);
        }
    }
    draw_attribute_section(
        surface, props, &layout, view, controls, scale, theme, font, line,
    );
}

/// What the window says while its read is in flight.
const PROPERTIES_READING: &str = "Reading…";

/// The live editors a Properties window draws over its rows.
#[derive(Copy, Clone)]
pub struct PropertiesControls<'a> {
    /// Whether the launching user holds `CAP_FS_CHOWN`, the one gate on
    /// offering the ownership control at all.
    pub can_chown: bool,
    /// The open owning-id editor, when one is being typed into.
    pub owner: Option<(OwnerField, &'a TextField)>,
    /// The `key = value` attribute editor, which the window always has: it is
    /// the one text surface the section is edited through.
    pub attribute: &'a TextField,
    /// The attribute list's own scrollbar, carrying its live hover/drag state.
    pub scrollbar: &'a ScrollBar,
}

/// Draw the labelled permissions grid: read/write/execute column headers over
/// three owner/group/other triad rows of [`Checkbox`] toggles reflecting the
/// current mode.
fn draw_permission_grid(
    surface: &mut Surface,
    props: &Properties,
    layout: &PropertiesLayout,
    scale: Scale,
    theme: &Theme,
    font: BitmapFont,
) {
    let Some(grid) = layout.grid.as_ref() else {
        return;
    };
    let palette = theme.palette();
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
            Checkbox::new(String::new(), selection).render(surface, grid.cell(index), scale, theme);
        }
    }
}

/// Draw the extended-attribute section: its label, then the node's attributes
/// as selectable rows with the cursor row marked, the scroll bar beside them
/// when the list is longer than the band shows, and the `key = value` editor
/// with its Set and Remove actions at the foot.
///
/// A volume that stores no attributes says so, and a node that carries none
/// says that instead — an empty list would be a claim the reader cannot tell
/// apart from either.
#[allow(clippy::too_many_arguments)] // The node, its layout, its live state, and the frame.
fn draw_attribute_section(
    surface: &mut Surface,
    props: &Properties,
    layout: &PropertiesLayout,
    view: AttrView,
    controls: PropertiesControls<'_>,
    scale: Scale,
    theme: &Theme,
    font: BitmapFont,
    line: u32,
) {
    let palette = theme.palette();
    let left = layout
        .fields
        .left()
        .saturating_add(to_i32(scale.scale_length(LABEL_PADDING)));
    font.draw_text(
        surface,
        left,
        layout.label_y,
        ATTR_SECTION_LABEL,
        palette.on_surface_muted.into(),
    );
    let attrs = props.attributes();
    let note = match attrs {
        Attributes::Unread | Attributes::Unsupported => Some(String::from(ATTR_UNSUPPORTED)),
        Attributes::Refused(errno) => Some(alloc::format!("{ATTR_REFUSED} ({errno})")),
        Attributes::Visible(list) if list.is_empty() => Some(String::from(ATTR_NONE)),
        Attributes::Visible(_) => None,
    };
    if let Some(note) = note {
        if let Some(band) = layout.rows {
            font.draw_text(
                surface,
                left,
                band.top(),
                &note,
                palette.on_surface_muted.into(),
            );
        }
        draw_attribute_editor(surface, layout, controls, scale, theme, font);
        return;
    }
    let list = attrs.visible();
    let visible = layout.visible_rows(line);
    let gutter = layout
        .rows
        .map_or(0, |band| gutter_width(scale, theme, band.width));
    let first = usize::try_from(view.offset).unwrap_or(usize::MAX);
    for slot in 0..visible {
        let Some(index) = first.checked_add(slot) else {
            break;
        };
        let Some(attr) = list.get(index) else {
            break;
        };
        let Some(bounds) = layout.row_rect(slot, line, gutter) else {
            break;
        };
        let mut row = ListRow::new(attr.key_display()).with_trailing(attr.display());
        row.set_selected(index == view.cursor);
        row.render(surface, bounds, scale, theme, None);
    }
    if visible < list.len() {
        if let Some(gutter) = layout.gutter_rect(gutter) {
            let mut bar: ScrollBar = *controls.scrollbar;
            bar.set_model(ScrollModel::new(
                ScrollRange::new(
                    u64::try_from(list.len()).unwrap_or(u64::MAX),
                    u64::try_from(visible).unwrap_or(u64::MAX),
                    view.offset,
                ),
                1,
                u64::try_from(visible.max(1)).unwrap_or(u64::MAX),
            ));
            bar.render(surface, gutter, scale, theme);
        }
    }
    draw_attribute_editor(surface, layout, controls, scale, theme, font);
}

/// Draw the `key = value` editor and its Set / Remove actions.
fn draw_attribute_editor(
    surface: &mut Surface,
    layout: &PropertiesLayout,
    controls: PropertiesControls<'_>,
    scale: Scale,
    theme: &Theme,
    font: BitmapFont,
) {
    let Some((field, actions)) = layout.editor_parts(scale, font) else {
        return;
    };
    controls.attribute.render(surface, field, scale, theme);
    for ((action, label), rect) in ATTR_ACTIONS.iter().zip(actions.iter()) {
        let role = match action {
            AttrAction::Set => ControlRole::Primary,
            AttrAction::Remove => ControlRole::Destructive,
        };
        Button::new(ButtonContent::Label(String::from(*label)), role)
            .render(surface, *rect, scale, theme);
    }
}

/// Draw the inline ownership control over the owner row: an accent underline
/// beneath the uid and gid values marking each as clickable to edit, and —
/// when `editor` names a field being edited — the active [`TextField`] over
/// that value.
///
/// Reassigning an owner is a privileged operation (unlike renaming or a mode
/// change), so the caller draws this only where the launching user holds
/// `CAP_FS_CHOWN`. It holds no authority itself: the commit is the caller's
/// own capability-checked `fs_set_owner`.
#[allow(clippy::too_many_arguments)] // The row, its content, and the frame.
fn draw_owner_control(
    surface: &mut Surface,
    geom: &OwnerRowGeom,
    content: Rect,
    scale: Scale,
    theme: &Theme,
    font: BitmapFont,
    editor: Option<(OwnerField, &TextField)>,
) {
    let palette = theme.palette();
    let thickness = scale.scale_length(1).max(1);
    let fields = [OwnerField::Uid, OwnerField::Gid];
    for (rect, field) in geom.cells.iter().zip(fields) {
        if let Some((editing, text_field)) = editor {
            if editing == field {
                let bounds = owner_editor_geom(geom, field, content, font);
                text_field.render(surface, bounds, scale, theme);
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

/// What a press at window-local `point` on a Properties window showing `props`
/// resolves to, or `None` when it is on nothing.
///
/// Mirrors [`draw_properties_window`]'s placement through the one shared
/// layout, so a press acts on exactly the control the user saw. The
/// capability-free permission toggles resolve before the privileged ownership
/// control, so a session that may not reassign an owner can still toggle a
/// mode bit on the same surface; a press on nothing changes nothing.
#[must_use]
pub fn properties_hit(
    props: &Properties,
    view: AttrView,
    window: Rect,
    scale: Scale,
    theme: &Theme,
    point: Point,
) -> Option<PropertiesTarget> {
    let font = BitmapFont::for_role(theme.fonts(), TextRole::Body, scale);
    let layout = PropertiesLayout::resolve(props, window, scale, theme, font);
    let line = row_height(scale, theme).max(1);
    if let Some(grid) = layout.grid.as_ref() {
        for (index, rect) in grid.cells().iter().enumerate() {
            if contains(*rect, point) {
                return PERMISSION_BITS
                    .get(index)
                    .copied()
                    .map(PropertiesTarget::Permission);
            }
        }
    }
    if let Some(geom) = layout.owner.as_ref() {
        for (rect, field) in geom.cells.iter().zip([OwnerField::Uid, OwnerField::Gid]) {
            if contains(*rect, point) {
                return Some(PropertiesTarget::Owner(field));
            }
        }
    }
    if let Some((field, actions)) = layout.editor_parts(scale, font) {
        for ((action, _), rect) in ATTR_ACTIONS.iter().zip(actions.iter()) {
            if contains(*rect, point) {
                return Some(PropertiesTarget::Action(*action));
            }
        }
        if contains(field, point) {
            return Some(PropertiesTarget::Editor);
        }
    }
    let gutter = layout
        .rows
        .map_or(0, |band| gutter_width(scale, theme, band.width));
    let visible = layout.visible_rows(line);
    let first = usize::try_from(view.offset).unwrap_or(usize::MAX);
    for slot in 0..visible {
        let Some(bounds) = layout.row_rect(slot, line, gutter) else {
            break;
        };
        if contains(bounds, point) {
            let index = first.checked_add(slot)?;
            return (index < props.attributes().visible().len())
                .then_some(PropertiesTarget::Attribute(index));
        }
    }
    None
}

/// Whether `point` lies inside `rect`, on the same half-open convention every
/// other hit-test in this module uses.
fn contains(rect: Rect, point: Point) -> bool {
    let right = rect.left().saturating_add(to_i32(rect.width));
    let bottom = rect.top().saturating_add(to_i32(rect.height));
    point.x >= rect.left() && point.x < right && point.y >= rect.top() && point.y < bottom
}

/// How many attribute rows a Properties window of this size shows at once —
/// the count its scroll offset is clamped against, so the drawn list and the
/// list the wheel moves are one fact.
#[must_use]
pub fn properties_attr_visible_rows(
    props: &Properties,
    window: Rect,
    scale: Scale,
    theme: &Theme,
) -> usize {
    let font = BitmapFont::for_role(theme.fonts(), TextRole::Body, scale);
    PropertiesLayout::resolve(props, window, scale, theme, font)
        .visible_rows(row_height(scale, theme).max(1))
}

/// Where the active owner editor for `field` is drawn, or `None` when the
/// owner row does not fit.
///
/// The one placement [`draw_properties_window`] draws it at, published so the
/// host feeding that editor keys reports the rectangle it repaints instead of
/// re-deriving this layout.
#[must_use]
pub fn properties_owner_editor_rect(
    props: &Properties,
    window: Rect,
    scale: Scale,
    theme: &Theme,
    field: OwnerField,
) -> Option<Rect> {
    let font = BitmapFont::for_role(theme.fonts(), TextRole::Body, scale);
    let layout = PropertiesLayout::resolve(props, window, scale, theme, font);
    let geom = layout.owner.as_ref()?;
    Some(owner_editor_geom(geom, field, window, font))
}

/// Where the `key = value` attribute editor's field is drawn, or `None` when
/// the row does not fit.
#[must_use]
pub fn properties_attr_editor_rect(
    props: &Properties,
    window: Rect,
    scale: Scale,
    theme: &Theme,
) -> Option<Rect> {
    let font = BitmapFont::for_role(theme.fonts(), TextRole::Body, scale);
    let layout = PropertiesLayout::resolve(props, window, scale, theme, font);
    layout.editor_parts(scale, font).map(|(field, _)| field)
}

/// Route a pointer event over the attribute list's scroll gutter, moving
/// `rows`.
///
/// `None` when the press was not the gutter's; otherwise whether the *offset*
/// moved, so a caller repaints the rows it scrolled and leaves a bar that only
/// changed its own look to the damage the bar itself reported.
///
/// The same routing the listing and the *Open With…* chooser use, so a drag on
/// this bar behaves exactly as a drag on either of those.
#[allow(clippy::too_many_arguments)] // The node, its list, the geometry, and the event.
pub fn properties_scroll_pointer(
    props: &Properties,
    rows: &mut RowList,
    window: Rect,
    scale: Scale,
    theme: &Theme,
    point: Point,
    event: &InputEvent,
    damage: &mut Region,
) -> Option<bool> {
    let font = BitmapFont::for_role(theme.fonts(), TextRole::Body, scale);
    let layout = PropertiesLayout::resolve(props, window, scale, theme, font);
    let line = row_height(scale, theme).max(1);
    let band = layout.rows?;
    let gutter_w = gutter_width(scale, theme, band.width);
    let gutter = layout.gutter_rect(gutter_w)?;
    let visible = layout.visible_rows(line);
    let model = rows.scroll_model(visible);
    let routed = {
        let bar = rows.scrollbar_mut();
        bar.set_model(model);
        route_scroll_bar(bar, gutter, scale, theme, point, event, damage)?
    };
    Some(match routed {
        ScrollRouted::ScrollTo { offset } => rows.set_offset(offset, visible),
        ScrollRouted::Redrawn => false,
    })
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
/// for the `disposition` the caller will actually carry out: a recoverable
/// **Move to Trash** or an irreversible **Delete Permanently**.
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
/// drawable — if clipped — dialog rather than a panic. One definition so
/// [`draw_delete_dialog`] and [`delete_dialog_action_at`] place and hit-test
/// the same rectangle.
#[must_use]
pub fn delete_dialog_rect(viewport: Rect, scale: Scale, theme: &Theme) -> Rect {
    // Title bar, up to two message lines, and the action-button band, with
    // margins — generous so the buttons are not clipped at a normal size.
    centered_overlay_rect(viewport, scale, theme, 6)
}

/// A centered, clamped modal-overlay rectangle within `viewport`, sized to a
/// title bar plus `content_lines` text rows and four-fifths of the window
/// width, clamped so a small window still yields a drawable — if clipped —
/// rectangle rather than a panic.
///
/// The one sizing definition the delete-confirmation dialog and the progress
/// panel share, so their placement stays consistent and cannot drift.
fn centered_overlay_rect(viewport: Rect, scale: Scale, theme: &Theme, content_lines: u32) -> Rect {
    let line = row_height(scale, theme);
    let title = scale.scale_length(theme.metrics().title_bar_height).max(1);
    let content = line.saturating_mul(content_lines);
    let height = title.saturating_add(content).min(viewport.height.max(1));
    let width = overlay_width(viewport);
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

/// The width every one of the manager's modal surfaces takes within
/// `viewport`: four fifths of it, clamped to it, so they read as a family.
///
/// Its own definition because a surface in its own popup window takes the
/// width without taking the centring — the session places the popup — and two
/// spellings of "four fifths" would be one too many.
fn overlay_width(viewport: Rect) -> u32 {
    viewport
        .width
        .saturating_mul(4)
        .checked_div(5)
        .unwrap_or(viewport.width)
        .clamp(1, viewport.width.max(1))
}

/// Draw the delete-confirmation `dialog` centered in `viewport`, on top of the
/// current view.
///
/// Every blit clips, so a window too small for the whole dialog simply shows
/// what fits rather than panicking. It reads only the passed-in dialog and
/// draws — it performs no I/O and holds no authority.
pub fn draw_delete_dialog(
    surface: &mut Surface,
    dialog: &Dialog,
    scale: Scale,
    theme: &Theme,
    viewport: Rect,
) {
    let bounds = delete_dialog_rect(viewport, scale, theme);
    dialog.render(surface, bounds, scale, theme);
}

/// The action-button index the delete-confirmation `dialog` draws at
/// window-local pixel `point`, or `None` when the click is not on a button.
///
/// This mirrors [`draw_delete_dialog`]'s placement through the shared
/// [`delete_dialog_rect`] and the dialog's own
/// [`action_rects`](Dialog::action_rects) geometry, so a click resolves to
/// exactly the button the user pressed — [`DELETE_CONFIRM_INDEX`] for Delete,
/// [`DELETE_CANCEL_INDEX`] for Cancel. Only the file manager calls it; a click
/// anywhere but a button returns `None`, changing nothing (fail closed).
#[must_use]
pub fn delete_dialog_action_at(
    dialog: &Dialog,
    viewport: Rect,
    scale: Scale,
    theme: &Theme,
    point: Point,
) -> Option<usize> {
    let bounds = delete_dialog_rect(viewport, scale, theme);
    let rects = dialog.action_rects(bounds, scale, theme);
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
/// and hit-test the same rectangle, sized like the delete-confirmation dialog
/// so the two modal surfaces sit consistently.
#[must_use]
pub fn progress_dialog_rect(viewport: Rect, scale: Scale, theme: &Theme) -> Rect {
    centered_overlay_rect(viewport, scale, theme, 6)
}

/// The Cancel-button rectangle within the progress panel's `content` area —
/// bottom-right, sized to the "Cancel" label plus padding, clamped to the
/// content so a small window never places it off the panel. The one definition
/// [`draw_progress_dialog`] paints and [`progress_cancel_at`] hit-tests, so a
/// click resolves to exactly the drawn button.
fn progress_cancel_rect(content: Rect, scale: Scale, theme: &Theme, font: BitmapFont) -> Rect {
    let pad = font.text_width("  ").max(scale.scale_length(LABEL_PADDING));
    let width = font
        .text_width("Cancel")
        .saturating_add(pad.saturating_mul(2))
        .min(content.width);
    let height = row_height(scale, theme).min(content.height);
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
/// fabricated percentage. Its moving-segment phase is derived from the count,
/// so the bar advances on real job-progress events, never an idle animation
/// loop.
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
/// what fits rather than panicking. It reads only the passed-in model and draws
/// — it performs no I/O and holds no authority. Only the write-capable file
/// manager drives a long operation, so only it draws this; the read-only picker
/// never does.
pub fn draw_progress_dialog(
    surface: &mut Surface,
    model: &ProgressModel,
    scale: Scale,
    theme: &Theme,
    viewport: Rect,
) {
    let font = BitmapFont::for_role(theme.fonts(), TextRole::Body, scale);
    let bounds = progress_dialog_rect(viewport, scale, theme);
    let panel = Panel::new(model.title());
    panel.render(surface, bounds, scale, theme);
    let Some(content) = panel.content_rect(bounds, scale, theme) else {
        return;
    };
    let bar = Rect::new(
        content.left(),
        content.top(),
        content.width,
        row_height(scale, theme),
    );
    build_progress(model).render(surface, bar, scale, theme);
    let cancel_rect = progress_cancel_rect(content, scale, theme, font);
    build_progress_cancel(model).render(surface, cancel_rect, scale, theme);
}

/// Whether the progress panel's Cancel button is drawn at window-local pixel
/// `point`.
///
/// Mirrors [`draw_progress_dialog`]'s placement through the shared
/// [`progress_dialog_rect`] and the same private cancel-button rectangle it
/// paints, so a click resolves to exactly the drawn button. A click anywhere
/// but the button — or on a panel too small to place it — returns `false`,
/// changing nothing (fail closed).
#[must_use]
pub fn progress_cancel_at(viewport: Rect, scale: Scale, theme: &Theme, point: Point) -> bool {
    let bounds = progress_dialog_rect(viewport, scale, theme);
    // The content area is title-text-independent, so an empty-title panel
    // mirrors the titled panel [`draw_progress_dialog`] draws.
    let Some(content) = Panel::new(String::new()).content_rect(bounds, scale, theme) else {
        return false;
    };
    let font = BitmapFont::for_role(theme.fonts(), TextRole::Body, scale);
    let rect = progress_cancel_rect(content, scale, theme, font);
    if rect.width == 0 || rect.height == 0 {
        return false;
    }
    let right = rect.left().saturating_add(to_i32(rect.width));
    let bottom = rect.top().saturating_add(to_i32(rect.height));
    point.x >= rect.left() && point.x < right && point.y >= rect.top() && point.y < bottom
}

/// Most candidate rows the "Open With…" chooser's list shows at once.
///
/// A bound on the *popup*, not on the candidate set: the set grows with the
/// applications a user installs, and a longer list scrolls inside the panel.
/// Fewer candidates make a **shorter** popup — the surface is sized to its
/// content, so one candidate is one row of plate and not eight.
pub const OPEN_WITH_MAX_ROWS: usize = 8;

/// The rows the chooser's list wants for `candidates`: all of them, up to the
/// bound, and never none — a chooser is never built over an empty list.
fn open_with_wanted_rows(candidates: usize) -> u32 {
    u32::try_from(candidates.clamp(1, OPEN_WITH_MAX_ROWS)).unwrap_or(1)
}

/// The narrowest the chooser is ever drawn, in logical pixels at the reference
/// density — a panel narrower than this reads as a sliver whatever it holds.
///
/// A floor, not the width: the rule below widens it to whatever the longest
/// candidate, the title, and the action buttons actually measure.
const OPEN_WITH_MIN_WIDTH: u32 = 200;

/// The extent of the chooser's own popup window for `chooser`, capped to the
/// `screen` it must fit on.
///
/// The popup **is** the chooser: the panel fills it, so the surface's own size
/// is what decides how many rows are shown and how much of each name reads. A
/// one-candidate chooser is a one-row popup rather than a plate with seven rows
/// of nothing, and a chooser of short names is a compact panel rather than a
/// letterbox four fifths of the display wide — the manager's *centred* modal
/// surfaces take that proportion because they are drawn over the listing; a
/// surface in its own window is sized to what it draws.
///
/// Both extents are content-derived through the same inverses the panel lays
/// its content out with, so nothing the chooser was widened or heightened for
/// is elided or clipped: the widest candidate row, the title, and the whole
/// action band each fit, floored at a stated logical width and clamped to the
/// screen.
#[must_use]
pub fn open_with_chooser_extent(
    chooser: &OpenWithChooser,
    scale: Scale,
    theme: &Theme,
    screen: Rect,
) -> (u32, u32) {
    // One line for the actions beneath the list, which the panel's content
    // holds along with the rows. The height comes from the panel's own
    // inverse rather than a second reckoning of its header and rim: a
    // difference of one border there costs the list a whole row once it is
    // divided by a row height.
    let lines = open_with_wanted_rows(chooser.candidates().len()).saturating_add(1);
    let content = row_height(scale, theme).saturating_mul(lines);
    let height = Panel::height_for_content(content, scale, theme);
    (
        open_with_chooser_width(chooser, scale, theme, screen),
        height.min(screen.height.max(1)).max(1),
    )
}

/// The chooser popup's width: the widest of what it must hold, floored at
/// [`OPEN_WITH_MIN_WIDTH`] and clamped to `screen`.
///
/// Each candidate is measured as the [`ListRow`] it is drawn as — icon column,
/// paddings and all — with the scroll gutter beside it, so a name the panel was
/// widened for is not then elided by the row's own reservations.
fn open_with_chooser_width(
    chooser: &OpenWithChooser,
    scale: Scale,
    theme: &Theme,
    screen: Rect,
) -> u32 {
    let font = BitmapFont::for_role(theme.fonts(), TextRole::Body, scale);
    let row = row_height(scale, theme);
    let rows = chooser
        .candidates()
        .iter()
        .map(|candidate| {
            ListRow::new(candidate.name())
                .with_icon(IconKind::AppBundle)
                .width_for_content(row, scale, theme)
        })
        .max()
        .unwrap_or(0);
    // The gutter is sized off the panel's own width, which is what this is
    // computing; asking for it at the floor keeps the reservation stable
    // instead of chasing its own answer.
    let gutter = gutter_width(scale, theme, scale.scale_length(OPEN_WITH_MIN_WIDTH));
    let title = font.text_width(&alloc::format!("Open {} with", chooser.display_name()));
    let content = rows
        .saturating_add(gutter)
        .max(title)
        .max(open_with_actions_width(scale, font));
    Panel::width_for_content(content, scale, theme)
        .max(scale.scale_length(OPEN_WITH_MIN_WIDTH))
        .clamp(1, screen.width.max(1))
}

/// The chooser panel's bounds within its own popup `viewport`: the whole of
/// it.
///
/// One placement definition, shared by [`draw_open_with_chooser`],
/// [`open_with_row_at`], [`open_with_action_at`] and
/// [`open_with_visible_rows`], so what is drawn and what a press resolves to
/// can never disagree.
#[must_use]
pub const fn open_with_chooser_rect(viewport: Rect) -> Rect {
    viewport
}

/// How many candidate rows the chooser's list shows at the current geometry.
///
/// Derived from the content the popup actually has, so a popup the screen
/// clamped shows what it can rather than what it asked for — and the count
/// the extent was computed for and the count the list draws are one fact.
/// Zero when the popup leaves the list no room at all, which draws and
/// hit-tests nothing rather than dividing by an empty row.
#[must_use]
pub fn open_with_visible_rows(viewport: Rect, scale: Scale, theme: &Theme) -> usize {
    let Some(content) = open_with_list_rect(viewport, scale, theme) else {
        return 0;
    };
    let row = row_height(scale, theme);
    if row == 0 {
        return 0;
    }
    (content.height / row) as usize
}

/// The chooser panel's whole content area — the rows, the scroll gutter, and
/// the action band beneath them.
fn open_with_content_rect(viewport: Rect, scale: Scale, theme: &Theme) -> Option<Rect> {
    let bounds = open_with_chooser_rect(viewport);
    Panel::new(String::new()).content_rect(bounds, scale, theme)
}

/// The part of the content the candidate list occupies: everything above the
/// action band.
fn open_with_list_rect(viewport: Rect, scale: Scale, theme: &Theme) -> Option<Rect> {
    let content = open_with_content_rect(viewport, scale, theme)?;
    let actions = row_height(scale, theme).min(content.height);
    let height = content.height.saturating_sub(actions);
    (height > 0).then(|| Rect::new(content.left(), content.top(), content.width, height))
}

/// The window-local rectangle the chooser's list draws its row at `slot` (a
/// position on screen, not a candidate index) into, and the gutter the
/// scrollbar occupies beside them.
fn open_with_row_rect(content: Rect, scale: Scale, theme: &Theme, slot: usize) -> Rect {
    let row = row_height(scale, theme);
    let gutter = gutter_width(scale, theme, content.width);
    let top = content.origin.y.saturating_add(to_i32(
        row.saturating_mul(u32::try_from(slot).unwrap_or(u32::MAX)),
    ));
    Rect::new(
        content.origin.x,
        top,
        content.width.saturating_sub(gutter),
        row,
    )
}

/// The scroll gutter beside the chooser's rows, or `None` when the panel is too
/// narrow for one.
fn open_with_gutter_rect(content: Rect, scale: Scale, theme: &Theme) -> Option<Rect> {
    let gutter = gutter_width(scale, theme, content.width);
    if gutter == 0 {
        return None;
    }
    Some(Rect::new(
        content
            .origin
            .x
            .saturating_add(to_i32(content.width.saturating_sub(gutter))),
        content.origin.y,
        gutter,
        content.height,
    ))
}

/// What a press on the chooser's action band asks for.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum OpenWithAction {
    /// Open the file with the current candidate.
    Open,
    /// Close the chooser without opening anything.
    Cancel,
}

/// The two action buttons the chooser offers, in the order they are drawn.
const OPEN_WITH_ACTIONS: [(OpenWithAction, &str); 2] = [
    (OpenWithAction::Cancel, "Cancel"),
    (OpenWithAction::Open, "Open"),
];

/// Where each action button is drawn in the band beneath the list, trailing
/// edge last — the one definition [`draw_open_with_chooser`] paints and
/// [`open_with_action_at`] hit-tests.
///
/// `None` when the popup leaves no band, which draws and resolves nothing
/// (fail closed): a chooser with no visible Open button is closed with Escape
/// or by choosing a row, never left with a hidden action.
fn open_with_action_rects(
    viewport: Rect,
    scale: Scale,
    theme: &Theme,
    font: BitmapFont,
) -> Option<[Rect; OPEN_WITH_ACTIONS.len()]> {
    let content = open_with_content_rect(viewport, scale, theme)?;
    let height = row_height(scale, theme);
    if height == 0 || content.height < height {
        return None;
    }
    let pad = open_with_action_pad(scale, font);
    let widths = open_with_action_widths(scale, font);
    let top = content
        .top()
        .saturating_add(to_i32(content.height.saturating_sub(height)));
    let mut right = content.left().saturating_add(to_i32(content.width));
    let mut rects = [Rect::EMPTY; OPEN_WITH_ACTIONS.len()];
    // Laid out from the trailing edge back, so the primary action sits
    // furthest right whatever the labels measure.
    for slot in (0..OPEN_WITH_ACTIONS.len()).rev() {
        let width = widths[slot].min(content.width);
        let left = right.saturating_sub(to_i32(width));
        rects[slot] = Rect::new(left, top, width, height);
        right = left.saturating_sub(to_i32(pad));
    }
    Some(rects)
}

/// The gap the action band leaves around and between its buttons.
fn open_with_action_pad(scale: Scale, font: BitmapFont) -> u32 {
    font.text_width("  ").max(scale.scale_length(LABEL_PADDING))
}

/// Each action button's intrinsic width, in the order they are drawn — the one
/// definition [`open_with_action_rects`] places and the chooser's own extent
/// reserves room for, so the band can never be sized narrower than the buttons
/// it must hold.
fn open_with_action_widths(scale: Scale, font: BitmapFont) -> [u32; OPEN_WITH_ACTIONS.len()] {
    let pad = open_with_action_pad(scale, font);
    OPEN_WITH_ACTIONS.map(|(_, label)| font.text_width(label).saturating_add(pad.saturating_mul(2)))
}

/// The whole action band's intrinsic width: every button plus the gap between
/// each pair and the one the trailing edge keeps.
fn open_with_actions_width(scale: Scale, font: BitmapFont) -> u32 {
    let pad = open_with_action_pad(scale, font);
    open_with_action_widths(scale, font)
        .into_iter()
        .fold(pad, |total, width| {
            total.saturating_add(width).saturating_add(pad)
        })
}

/// Draw the "Open With…" `chooser` into its own popup `viewport`: a titled
/// panel naming the file, one [`ListRow`] per visible candidate with the
/// current one selected, the scrollbar beside them when the list is longer
/// than the panel shows, and the Open/Cancel actions beneath.
///
/// Each candidate's row draws its application's own icon where `artwork`
/// resolves one and the built-in bundle glyph otherwise, exactly as a grid
/// tile does. It reads only the passed-in chooser and draws — no I/O, no
/// authority — and every blit clips, so a popup too small for the panel simply
/// shows what fits.
pub fn draw_open_with_chooser(
    surface: &mut Surface,
    chooser: &OpenWithChooser,
    scale: Scale,
    theme: &Theme,
    viewport: Rect,
    artwork: &mut dyn IconArtwork,
) {
    let bounds = open_with_chooser_rect(viewport);
    let panel = Panel::new(alloc::format!("Open {} with", chooser.display_name()));
    panel.render(surface, bounds, scale, theme);
    let Some(content) = open_with_list_rect(viewport, scale, theme) else {
        return;
    };
    let visible = open_with_visible_rows(viewport, scale, theme);
    let first = usize::try_from(chooser.offset()).unwrap_or(usize::MAX);
    for slot in 0..visible {
        let Some(candidate) = chooser.candidates().get(first.saturating_add(slot)) else {
            break;
        };
        let mut row = ListRow::new(candidate.name()).with_icon(IconKind::AppBundle);
        row.set_selected(first.saturating_add(slot) == chooser.selected());
        let bounds = open_with_row_rect(content, scale, theme, slot);
        let side = row.icon_side(bounds, scale, theme);
        let art = artwork.artwork(
            IconRequest::bundle(IconKind::AppBundle, candidate.bundle_path()),
            side,
        );
        row.render(surface, bounds, scale, theme, art);
    }
    if let Some(gutter) = open_with_gutter_rect(content, scale, theme) {
        let mut bar: ScrollBar = *chooser.scrollbar();
        bar.set_model(chooser.scroll_model(visible));
        bar.render(surface, gutter, scale, theme);
    }
    let font = BitmapFont::for_role(theme.fonts(), TextRole::Body, scale);
    if let Some(rects) = open_with_action_rects(viewport, scale, theme, font) {
        for ((action, label), rect) in OPEN_WITH_ACTIONS.iter().zip(rects.iter()) {
            build_open_with_action(*action, label, chooser).render(surface, *rect, scale, theme);
        }
    }
}

/// Build one of the chooser's action buttons: Open is the primary action and
/// is offered only while a candidate is current, Cancel is always available.
fn build_open_with_action(
    action: OpenWithAction,
    label: &str,
    chooser: &OpenWithChooser,
) -> Button {
    let role = match action {
        OpenWithAction::Open => ControlRole::Primary,
        OpenWithAction::Cancel => ControlRole::Neutral,
    };
    let mut button = Button::new(ButtonContent::Label(String::from(label)), role);
    if action == OpenWithAction::Open && chooser.chosen().is_none() {
        button.set_state(ControlState::disabled());
    }
    button
}

/// The action the drawn `chooser` resolves popup-local pixel `point` to, or
/// `None` when the press is not on one.
///
/// Mirrors [`draw_open_with_chooser`]'s own band through the one private
/// action-rectangle rule they share, so a click resolves to exactly the button
/// the user pressed. An Open with no current candidate resolves to nothing
/// rather than opening whatever happens to be first (fail closed).
#[must_use]
pub fn open_with_action_at(
    chooser: &OpenWithChooser,
    viewport: Rect,
    scale: Scale,
    theme: &Theme,
    point: Point,
) -> Option<OpenWithAction> {
    let font = BitmapFont::for_role(theme.fonts(), TextRole::Body, scale);
    let rects = open_with_action_rects(viewport, scale, theme, font)?;
    OPEN_WITH_ACTIONS
        .iter()
        .zip(rects.iter())
        .find(|(_, rect)| !rect.is_empty() && rect.contains(point))
        .map(|((action, _), _)| *action)
        .filter(|action| *action != OpenWithAction::Open || chooser.chosen().is_some())
}

/// The candidate index the drawn `chooser` resolves popup-local pixel `point`
/// to, or `None` when the press is not on a candidate row — off the panel, on
/// its title band, in the scroll gutter, on the action band, or past the last
/// row (fail closed).
///
/// It mirrors [`draw_open_with_chooser`]'s geometry through the one private
/// list rectangle they share, so a press resolves to exactly the row the user
/// saw. The index is absolute (the chooser's scroll offset is applied), so it
/// names a candidate rather than a position on screen.
#[must_use]
pub fn open_with_row_at(
    chooser: &OpenWithChooser,
    viewport: Rect,
    scale: Scale,
    theme: &Theme,
    point: Point,
) -> Option<usize> {
    let content = open_with_list_rect(viewport, scale, theme)?;
    let visible = open_with_visible_rows(viewport, scale, theme);
    let first = usize::try_from(chooser.offset()).unwrap_or(usize::MAX);
    (0..visible).find_map(|slot| {
        let rect = open_with_row_rect(content, scale, theme, slot);
        if rect.width == 0 || !rect.contains(point) {
            return None;
        }
        let index = first.saturating_add(slot);
        (index < chooser.candidates().len()).then_some(index)
    })
}

/// Route a pointer `event` at window-local `point` to the chooser's own
/// scrollbar, reporting `Some(repaint)` when the bar consumed it (so the caller
/// does not also treat the press as a click on a row) and `None` when the
/// pointer had nothing to do with the bar.
///
/// The bar owns the interaction its press started, exactly as the listing's
/// does ([`scroll_pointer`]) and through the same shared routing, so the two
/// cannot come to behave differently.
pub fn open_with_scroll_pointer(
    chooser: &mut OpenWithChooser,
    scale: Scale,
    theme: &Theme,
    viewport: Rect,
    point: Point,
    event: &InputEvent,
    damage: &mut Region,
) -> Option<bool> {
    let content = open_with_list_rect(viewport, scale, theme)?;
    let gutter = open_with_gutter_rect(content, scale, theme)?;
    let visible = open_with_visible_rows(viewport, scale, theme);
    let model = chooser.scroll_model(visible);
    let routed = {
        let bar = chooser.scrollbar_mut();
        bar.set_model(model);
        route_scroll_bar(bar, gutter, scale, theme, point, event, damage)?
    };
    if let ScrollRouted::ScrollTo { offset } = routed {
        chooser.set_offset(offset, visible);
    }
    Some(true)
}
