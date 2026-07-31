//! Painting the taskbar's regions into a pixel [`Surface`].
//!
//! [`TaskbarRenderer::render`] turns the taskbar's computed [`BarLayout`] and
//! live state into a premultiplied-alpha [`Surface`] sized to the bar, filling
//! each region with a colour from the active theme's [`Palette`] and drawing
//! the clock label and task titles on top with the shared [`BitmapFont`]. The
//! two permanent leading launchers are the shared `lib/controls`
//! [`IconButton`](tairix_controls::IconButton)s the model owns, painted with
//! their live hover/pressed
//! state. The surface is the window manager's to place and round: the taskbar
//! paints a *rectangular* buffer and the compositor applies
//! [`BarLayout::corner_radius`] through its single anti-aliased
//! rounded-corner path, exactly as it rounds windows. There is no rounding —
//! and no colour algebra — here.
//!
//! [`TaskbarRenderer::render_library`] paints the open program-library popup:
//! the shared [`Panel`](tairix_controls::Panel) chrome anchored back at the
//! Library button, the search field, one shared list row per folder or
//! entry, the scrollbar when the rows overflow, and the calm placeholder when
//! nothing is listed. The theme is the taskbar's own
//! ([`Taskbar::theme`]), so the geometry the input router hit-tests and the
//! pixels painted here can never come from two different themes.
//!
//! Region rectangles are in screen space; each is translated into the
//! surface's local space by subtracting the surface's origin. The translation
//! saturates and [`Surface::fill_rect`] clips, so a degenerate layout paints
//! nothing rather than panicking. A label is truncated to
//! the characters that fit its region so text never spills into a neighbour.
//! Each notification slot draws a scalable, themeable [`tairix_icon`] vector
//! glyph resolved from the icon's asset id, rasterised to the slot size and
//! composited through `lib/raster`'s single blit path.

use tairix_font::BitmapFont;
use tairix_geometry::{Point, Rect, Scale};
use tairix_icon::{IconKind, IconSet};
use tairix_raster::{Color, RasterCache, Surface};
use tairix_theme::{Palette, TextRole, Theme};

use crate::layout::BarLayout;
use crate::library::{chrome_panel, list_row, popup_font, LibraryFocus};
use crate::notifications::NotificationArea;
use crate::taskbar::Taskbar;
use crate::tasks::TaskList;

/// Padding in pixels between a task slot's edge and its title text.
const LABEL_PADDING: u32 = 4;

/// Padding in pixels between a notification slot's edge and its icon glyph.
const ICON_PADDING: u32 = 4;

/// The epoch a cached notification glyph is valid for: the tint it is drawn
/// in, the pixel side it is rasterised to, and the generation of the active
/// [`IconSet`]. A theme change moves the tint on, a scale change moves the
/// side on, and installing a different icon set moves the generation on — any
/// of the three invalidates every cached glyph.
type IconEpoch = (Color, u32, u64);

/// Everything [`draw_icon`] needs to resolve and cache a notification glyph:
/// the across-frame glyph cache, the active icon set, and the set's
/// generation (part of the cache epoch, so installing a new set invalidates
/// the cached glyphs). Bundled so the painters take one parameter rather than
/// three.
struct IconContext<'a> {
    cache: &'a mut RasterCache<IconKind, Surface, IconEpoch>,
    set: &'a IconSet,
    generation: u64,
}

/// Paints a taskbar into a [`Surface`], caching the rasterised notification
/// glyphs so each is converted only once per tint and size.
///
/// The renderer holds the icon cache across frames: the bar's regions, clock,
/// and task titles are cheap to repaint every frame, but the vector
/// notification glyphs are rasterised through `lib/raster` once per
/// theme/scale and reused until one changes — the SVG-first "convert once,
/// re-render only on a scale or theme change" rule, sharing the one
/// [`RasterCache`] the window manager uses for cursors.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct TaskbarRenderer {
    icons: RasterCache<IconKind, Surface, IconEpoch>,
    icon_set: IconSet,
    icon_generation: u64,
}

impl TaskbarRenderer {
    /// A renderer with an empty glyph cache drawing the built-in icon set.
    ///
    /// The desktop has a complete icon set before any on-disk SVG asset loads;
    /// [`set_icons`](Self::set_icons) swaps a loaded set in at runtime.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            icons: RasterCache::new(),
            icon_set: IconSet::builtin(),
            icon_generation: 0,
        }
    }

    /// Install a loaded notification-icon set, replacing the one in use.
    ///
    /// The set is the decoded on-disk SVG assets (`IconSet::from_assets`),
    /// each kind keeping its authored colours, with a built-in fallback for
    /// any kind the assets omit. Installing a set bumps
    /// an internal generation that is part of the glyph-cache epoch, so the
    /// next render discards the previously rasterised glyphs and re-rasterises
    /// from the new set. The generation saturates rather
    /// than wrapping.
    pub fn set_icons(&mut self, set: IconSet) {
        self.icon_set = set;
        self.icon_generation = self.icon_generation.saturating_add(1);
    }

    /// The notification-icon set currently in use (built-in until
    /// [`set_icons`](Self::set_icons) installs a loaded one).
    #[must_use]
    pub const fn icons(&self) -> &IconSet {
        &self.icon_set
    }

    /// Paint `taskbar` into a [`Surface`] using its own theme's palette.
    ///
    /// Returns `None` only if the bar's pixel dimensions cannot be allocated
    /// (a surface that could never exist), so the caller fails closed rather
    /// than panicking. The window manager presents the
    /// returned surface and rounds it with [`BarLayout::corner_radius`].
    #[must_use]
    pub fn render(&mut self, taskbar: &Taskbar, scale: Scale) -> Option<Surface> {
        let theme = taskbar.theme();
        let layout = taskbar.layout(scale);
        let mut icons = IconContext {
            cache: &mut self.icons,
            set: &self.icon_set,
            generation: self.icon_generation,
        };
        let fonts = PanelFonts::resolve(theme, scale);
        let mut surface = Surface::new(layout.bar.width, layout.bar.height)?;
        let origin = layout.bar.origin;

        surface.fill(theme.palette().surface_raised.into());
        for (button, rect) in [
            (taskbar.library_button(), layout.library),
            (taskbar.files_button(), layout.files),
        ] {
            if rect.is_empty() {
                continue;
            }
            button.render(
                &mut surface,
                local_rect(rect, origin),
                scale,
                theme,
                fonts.text,
            );
        }

        paint_body(
            &mut surface,
            &layout,
            taskbar.tasks(),
            taskbar.notifications(),
            taskbar.clock().label(),
            theme.palette(),
            fonts,
            &mut icons,
        );
        Some(surface)
    }

    /// Paint the open program-library popup into a [`Surface`] using the
    /// taskbar's own theme.
    ///
    /// Returns `None` when the popup is closed (there is nothing to draw) or
    /// when its pixel dimensions cannot be allocated, so the caller fails
    /// closed rather than panicking. The window manager places the returned
    /// surface above the bar and rounds it with
    /// [`LibraryLayout::corner_radius`](crate::library::LibraryLayout::corner_radius),
    /// exactly as it rounds the bar.
    #[must_use]
    pub fn render_library(&self, taskbar: &Taskbar, scale: Scale) -> Option<Surface> {
        let popup = taskbar.library();
        if !popup.is_open() {
            return None;
        }
        let theme = taskbar.theme();
        let layout = taskbar.library_layout(scale);
        let mut surface = Surface::new(layout.panel.width, layout.panel.height)?;
        let origin = layout.panel.origin;
        let font = popup_font(theme, scale);
        let local_bounds = Rect::new(0, 0, layout.panel.width, layout.panel.height);

        chrome_panel(local_point(layout.anchor, origin)).render(
            &mut surface,
            local_bounds,
            scale,
            theme,
            font,
        );
        popup.search_field().render(
            &mut surface,
            local_rect(layout.search, origin),
            scale,
            theme,
            font,
        );

        let row_focus = popup.focus() == LibraryFocus::Rows;
        for &(index, rect) in &layout.rows {
            let Some(row) = popup.rows().get(index) else {
                continue;
            };
            let current = popup.current() == Some(index);
            let hovered = popup.hover() == Some(index);
            list_row(row, current, hovered, row_focus).render(
                &mut surface,
                local_rect(rect, origin),
                scale,
                theme,
                font,
            );
        }

        if let Some(placeholder) = popup.placeholder() {
            draw_label(
                &mut surface,
                origin,
                layout.viewport,
                placeholder,
                theme.palette().on_surface_muted.into(),
                font,
                Align::Centre,
            );
        }

        if let Some(scrollbar) = layout.scrollbar {
            popup
                .scrollbar()
                .render(&mut surface, local_rect(scrollbar, origin), scale, theme);
        }
        Some(surface)
    }
}

/// The two fonts the panel draws with, each resolved from the text role whose
/// job it does.
///
/// A task title and a popup row are ordinary interface text; the clock is a
/// de-emphasised annotation, set a step smaller like every other caption on
/// the desktop. Resolving both from the theme's roles keeps their relative
/// size and weight the theme's decision rather than this screen's.
#[derive(Copy, Clone)]
struct PanelFonts {
    /// Task titles and the leading buttons' (unused) label font.
    text: BitmapFont,
    /// The clock readout.
    clock: BitmapFont,
}

impl PanelFonts {
    /// The panel's fonts under `theme` at `scale`.
    fn resolve(theme: &Theme, scale: Scale) -> Self {
        let fonts = theme.fonts();
        Self {
            text: BitmapFont::for_role(fonts, TextRole::Body, scale),
            clock: BitmapFont::for_role(fonts, TextRole::Caption, scale),
        }
    }
}

/// Fill the task, notification, and clock regions of an already-created bar
/// surface.
#[allow(clippy::too_many_arguments)]
fn paint_body(
    surface: &mut Surface,
    layout: &BarLayout,
    tasks: &TaskList,
    notifications: &NotificationArea,
    clock_label: &str,
    palette: &Palette,
    fonts: PanelFonts,
    icons: &mut IconContext<'_>,
) {
    let origin = layout.bar.origin;
    for (slot, entry) in layout.tasks.iter().zip(tasks.entries()) {
        let focused = tasks.focused() == Some(entry.id);
        fill_region(
            surface,
            origin,
            *slot,
            task_fill(palette, focused, entry.minimised),
        );
        draw_label(
            surface,
            origin,
            *slot,
            &entry.title,
            task_text(palette, focused, entry.minimised),
            fonts.text,
            Align::Leading,
        );
    }

    for (slot, icon) in layout.notifications.iter().zip(notifications.icons()) {
        draw_icon(
            surface,
            origin,
            *slot,
            &icon.asset,
            palette.on_surface_muted.into(),
            icons,
        );
    }

    draw_label(
        surface,
        origin,
        layout.clock,
        clock_label,
        palette.on_surface.into(),
        fonts.clock,
        Align::Centre,
    );
}

/// Where a label sits along the main axis of its region.
#[derive(Copy, Clone, Eq, PartialEq)]
enum Align {
    /// Padded in from the leading (left/top) edge — used for task titles.
    Leading,
    /// Centred within the region — used for the clock and the popup's calm
    /// placeholder.
    Centre,
}

/// The fill colour for a task slot.
///
/// The focused, non-minimised task takes the accent so the active window
/// stands out; a minimised task recedes into the bar background; every other
/// running task gets the plain surface colour, which the palette guarantees
/// reads as distinct from the raised bar background.
fn task_fill(palette: &Palette, focused: bool, minimised: bool) -> Color {
    if minimised {
        palette.surface_raised.into()
    } else if focused {
        palette.accent.into()
    } else {
        palette.surface.into()
    }
}

/// The title colour for a task slot, matching the foreground role of its
/// [`task_fill`] background so the text stays legible.
fn task_text(palette: &Palette, focused: bool, minimised: bool) -> Color {
    if minimised {
        palette.on_surface_muted.into()
    } else if focused {
        palette.on_accent.into()
    } else {
        palette.on_surface.into()
    }
}

/// Draw `text` inside the screen-space `rect`, clipped to it, vertically
/// centred and aligned along the main axis per `align`. Text wider than the
/// region is truncated to the characters that fit.
fn draw_label(
    surface: &mut Surface,
    origin: Point,
    rect: Rect,
    text: &str,
    color: Color,
    font: BitmapFont,
    align: Align,
) {
    if rect.is_empty() || text.is_empty() {
        return;
    }
    let inset = match align {
        Align::Leading => LABEL_PADDING,
        Align::Centre => 0,
    };
    let usable = rect.width.saturating_sub(inset.saturating_mul(2));
    let fitted = font.truncate_to_width(text, usable);
    if fitted.is_empty() {
        return;
    }

    let text_width = font.text_width(fitted);
    let x_offset = match align {
        Align::Leading => inset,
        Align::Centre => rect.width.saturating_sub(text_width) / 2,
    };
    let y_offset = rect.height.saturating_sub(font.glyph_height()) / 2;
    let x = rect
        .left()
        .saturating_sub(origin.x)
        .saturating_add(to_i32(x_offset));
    let y = rect
        .top()
        .saturating_sub(origin.y)
        .saturating_add(to_i32(y_offset));
    font.draw_text(surface, x, y, fitted, color);
}

/// Draw a notification icon's glyph centred in its screen-space `rect`,
/// tinted with `color`. The glyph is a scalable [`tairix_icon`] vector icon
/// resolved from the asset id and rasterised to the slot size at this scale,
/// then composited onto the bar-local surface through the shared blit path. The rasterised glyph is taken from `icons`, which keeps
/// it across frames so it is converted only once per tint and size and
/// re-rendered only on a theme or scale change. An empty
/// slot, a slot too small to hold a glyph, or an unrenderable size paints
/// nothing rather than panicking.
fn draw_icon(
    surface: &mut Surface,
    origin: Point,
    rect: Rect,
    asset: &str,
    color: Color,
    icons: &mut IconContext<'_>,
) {
    if rect.is_empty() {
        return;
    }
    let side = rect
        .width
        .min(rect.height)
        .saturating_sub(ICON_PADDING.saturating_mul(2));
    let kind = IconKind::for_asset(asset_key(asset));
    let set = icons.set;
    let Some(image) = icons
        .cache
        .get_or_render(&(color, side, icons.generation), kind, || {
            set.icon(kind, color).rasterise(side)
        })
    else {
        return;
    };
    let x_offset = rect.width.saturating_sub(side) / 2;
    let y_offset = rect.height.saturating_sub(side) / 2;
    let x = to_i32(local(rect.left(), origin.x).saturating_add(x_offset));
    let y = to_i32(local(rect.top(), origin.y).saturating_add(y_offset));
    surface.blit(x, y, image);
}

/// The glyph key for an asset id: the segment after the last `.`, so the
/// taskbar's namespaced ids (`icon.network`) resolve to the bare glyph name
/// (`network`) the icon library knows. A dotless id maps to itself.
fn asset_key(asset: &str) -> &str {
    asset.rsplit('.').next().unwrap_or(asset)
}

/// Fill a screen-space `rect` into the bar-local surface, offsetting by the
/// bar's `origin`. Empty rectangles paint nothing.
fn fill_region(surface: &mut Surface, origin: Point, rect: Rect, color: Color) {
    if rect.is_empty() {
        return;
    }
    let x = local(rect.left(), origin.x);
    let y = local(rect.top(), origin.y);
    surface.fill_rect(x, y, rect.width, rect.height, color);
}

/// Translate a screen-space rectangle into surface-local space.
fn local_rect(rect: Rect, origin: Point) -> Rect {
    Rect::new(
        rect.left().saturating_sub(origin.x),
        rect.top().saturating_sub(origin.y),
        rect.width,
        rect.height,
    )
}

/// Translate a screen-space point into surface-local space.
fn local_point(point: Point, origin: Point) -> Point {
    Point::new(
        point.x.saturating_sub(origin.x),
        point.y.saturating_sub(origin.y),
    )
}

/// Translate a screen-space coordinate into bar-local space, clamping a
/// coordinate that would fall before the bar's origin to zero.
fn local(coord: i32, origin: i32) -> u32 {
    u32::try_from(i64::from(coord) - i64::from(origin)).unwrap_or(0)
}

/// Saturating `u32` → `i32`.
fn to_i32(value: u32) -> i32 {
    i32::try_from(value).unwrap_or(i32::MAX)
}
