//! Painting the taskbar's regions into a pixel [`Surface`].
//!
//! [`TaskbarRenderer::render`] turns the taskbar's computed [`BarLayout`] and
//! live state into a premultiplied-alpha [`Surface`] sized to the bar. The
//! two permanent leading launchers are the shared `lib/controls`
//! [`IconButton`](tairix_controls::IconButton)s the model owns, painted with
//! their live hover/pressed state; every running application is one shared
//! icon-only [`TaskbarItem`](tairix_controls::TaskbarItem) slot, so the bar's
//! application buttons have exactly one visual recipe and read as one strip
//! of equal icons. Per-application artwork (rasterised by the session) is
//! blitted through the control, drawn from the bundle the kernel attested
//! opened the window, so a running application is recognised by what it
//! actually is. The one mark the bar draws for itself is the [`BarLayout::separator`] rule dividing the Library launcher
//! from everything after it, filled in [`Palette::border`].
//! The bar's own background is the shared floating-surface plate
//! ([`paint_surface_plate`]) the popups it opens already wear: a rim one
//! [`plate_border`] thick in [`Palette::rim`] at the theme's chrome weight,
//! then the raised ground inside it, both rounded by
//! [`BarLayout::corner_radius`]. That is the same radius the window manager
//! cuts the bar window to through its single anti-aliased rounded-corner
//! path, exactly as it rounds windows, so the rim follows the bar's real
//! silhouette instead of squaring off across the cut. Placing the surface
//! stays the window manager's job.
//!
//! [`TaskbarRenderer::render_library`] paints the open program-library popup:
//! the shared [`Panel`](tairix_controls::Panel) chrome anchored back at the
//! Library button, the search field, one shared list row per folder or
//! entry, the scrollbar when the rows overflow, and the calm placeholder when
//! nothing is listed. The theme is the taskbar's own
//! ([`Taskbar::theme`]), so the geometry the input router hit-tests and the
//! pixels painted here can never come from two different themes.
//!
//! [`TaskbarRenderer::render_notifications`] paints the notification popover:
//! the shared `lib/controls` [`Panel`](tairix_controls::Panel) chrome anchored
//! back at the notification region, and one shared [`Notification`] card per
//! raised notification (severity mapped to the card's composed state),
//! presented by the window manager as its own small window above the bar.
//!
//! [`TaskbarRenderer::render_tray_readout`] paints the Switchboard capsule's
//! expanded readout: the shared [`TraySignal`](tairix_controls::TraySignal)
//! draws its own elevated plate at the geometry
//! [`Taskbar::tray_readout_layout`] computed, presented by the window
//! manager as its own small window beside the capsule (the capsule itself is
//! painted on the bar by [`render`](TaskbarRenderer::render)).
//!
//! Region rectangles are in screen space; each is translated into the
//! surface's local space by subtracting the surface's origin. The translation
//! saturates and [`Surface::fill_rect`] clips, so a degenerate layout paints
//! nothing rather than panicking. A label is truncated to
//! the characters that fit its region so text never spills into a neighbour.
//! Each status-signal slot draws a scalable, themeable [`tairix_icon`] vector
//! glyph resolved from the signal's kind, rasterised to the slot size and
//! composited through `lib/raster`'s single blit path.

use tairix_controls::shell::Notification;
use tairix_controls::state::{ActivityState, ValidationState};
use tairix_controls::{paint_surface_plate, plate_border, ChromeLayer, ControlRole, ControlState};
use tairix_font::BitmapFont;
use tairix_geometry::{Point, Rect, Scale};
use tairix_icon::{IconArtwork, IconKind, IconPicture, IconRequest, IconSet};
use tairix_log::Sink;
use tairix_raster::{Color, Surface};
use tairix_reclaim::{disposable_ui_cache, CacheAccounting, PressureGauge, ReclaimCache};
use tairix_theme::{Palette, TextRole};

use crate::layout::BarLayout;
use crate::library::{chrome_panel, list_row, popup_font, LibraryFocus};
use crate::notifications::{NotificationArea, NotifySeverity, TransientNotification};
use crate::taskbar::Taskbar;

/// Padding in pixels between a notification slot's edge and its icon glyph.
const ICON_PADDING: u32 = 4;

/// Worst-case per-entry bookkeeping the cache charges on top of a
/// notification glyph's own pixel bytes: the LRU/index tick and
/// charged-size fields (`u64` + `usize`) plus this cache's small share of
/// its two `BTreeMap`s' node overhead. `IconKind` itself is a bare enum
/// discriminant, so the key contributes negligible bytes beyond this.
const ENTRY_METADATA_BYTES: usize = 64;

/// The epoch a cached notification glyph is valid for: the tint it is drawn
/// in, the pixel side it is rasterised to, and the generation of the active
/// [`IconSet`]. A theme change moves the tint on, a scale change moves the
/// side on, and installing a different icon set moves the generation on — any
/// of the three invalidates every cached glyph.
pub type IconEpoch = (Color, u32, u64);

/// Build the one [`ReclaimCache`] a [`TaskbarRenderer`] retains rasterised
/// notification glyphs in, classified through the shared desktop cache
/// policy (`tairix_reclaim::disposable_ui_cache`).
///
/// `seat` is the seat the renderer belongs to and `fb_bytes` is the real
/// output's backing byte size, so the cache's budget scales with the actual
/// display rather than a guessed constant; `pressure` and `sink` are the
/// process's live pressure gauge and audit sink. The embedder — the only
/// party that knows all four — calls this once and hands the result to
/// [`TaskbarRenderer::new`].
#[must_use]
pub fn icon_cache(
    seat: u64,
    fb_bytes: usize,
    pressure: &'static (dyn PressureGauge + 'static),
    sink: &'static (dyn Sink + Sync),
) -> ReclaimCache<IconKind, Surface, IconEpoch> {
    disposable_ui_cache(
        "taskbar.icon",
        seat,
        fb_bytes,
        ENTRY_METADATA_BYTES,
        pressure,
        sink,
    )
}

/// Everything [`draw_icon`] needs to resolve and cache a notification glyph:
/// the across-frame glyph cache, the active icon set, and the set's
/// generation (part of the cache epoch, so installing a new set invalidates
/// the cached glyphs). Bundled so the painters take one parameter rather than
/// three.
struct IconContext<'a> {
    cache: &'a mut ReclaimCache<IconKind, Surface, IconEpoch>,
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
/// re-render only on a scale or theme change" rule, through the same bounded,
/// pressure-governed [`ReclaimCache`] the window manager uses for cursors.
/// The cache is a required constructor argument, not something this crate
/// builds itself: a cache built without a live pressure gauge would classify
/// and serve every lookup correctly while retaining nothing, which is a
/// defect that looks exactly like working software.
///
/// Neither `Clone` nor `PartialEq`/`Eq`/`Default` are derived: the cache
/// holds a pressure gauge and a diagnostics sink behind trait objects, which
/// are neither cloneable nor comparable, and cloning a live cache's charged
/// ledger would double-count its bytes.
#[derive(Debug)]
pub struct TaskbarRenderer {
    icons: ReclaimCache<IconKind, Surface, IconEpoch>,
    icon_set: IconSet,
    icon_generation: u64,
}

impl TaskbarRenderer {
    /// A renderer drawing the built-in icon set, caching rasterised glyphs
    /// in `cache`.
    ///
    /// The desktop has a complete icon set before any on-disk SVG asset
    /// loads; [`set_icons`](Self::set_icons) swaps a loaded set in at
    /// runtime. `cache` is built by the embedder from the shared desktop
    /// cache policy ([`icon_cache`]), wired to the real display backing
    /// size, the owning seat, and the process's live pressure gauge — this
    /// renderer never invents that policy itself.
    #[must_use]
    pub const fn new(cache: ReclaimCache<IconKind, Surface, IconEpoch>) -> Self {
        Self {
            icons: cache,
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

    /// Wipe every cached notification glyph, so no rasterised pixel data
    /// from this seat's session survives it.
    ///
    /// Called when the seat this renderer belongs to is lost or its session
    /// ends. The cache stays usable afterwards — a later render simply
    /// rebuilds what it needs — this only discards what was already
    /// rendered.
    pub fn teardown(&mut self) {
        self.icons.teardown();
    }

    /// Apply the current memory-pressure band's forced shrink to the glyph
    /// cache, returning the bytes released.
    ///
    /// The session calls this when the kernel wakes it with a deepened
    /// band, so the bar gives its rasterised pixels back at the moment
    /// pressure rises rather than at whatever later frame happens to
    /// repaint a notification. A band that demands nothing releases
    /// nothing.
    pub fn trim(&mut self) -> usize {
        self.icons.enforce_pressure()
    }

    /// Rasterised notification glyphs currently retained.
    #[must_use]
    pub fn cache_len(&self) -> usize {
        self.icons.len()
    }

    /// Bytes the glyph cache currently has charged: retained pixel data
    /// plus its own per-entry bookkeeping.
    #[must_use]
    pub fn cache_bytes(&self) -> usize {
        self.icons.charged_bytes()
    }

    /// The glyph cache's byte ledger and event counters, for diagnostics.
    #[must_use]
    pub fn cache_stats(&self) -> &CacheAccounting {
        self.icons.accounting()
    }

    /// Paint `taskbar` into a [`Surface`] using its own theme's palette.
    ///
    /// `artwork` is the desktop's shipped icon lookup: the two leading
    /// launchers draw the [`Library`](IconKind::Library) and
    /// [`Folder`](IconKind::Folder) artwork, the trailing Switchboard capsule
    /// draws its own kind's, and a running application's slot draws its own
    /// artwork when it has one, its kind's shipped artwork otherwise. Every
    /// one of those falls back to its
    /// built-in glyph when the lookup returns `None` — the shared icon slot
    /// does that itself — so a system with no `/System/Graphics` is fully
    /// usable ([`NoArtwork`](tairix_icon::NoArtwork) resolves entirely to
    /// glyphs).
    ///
    /// The bar's background is the shared floating-surface plate
    /// ([`paint_surface_plate`]): a rim one [`plate_border`] thick in the
    /// palette's `rim`, then the raised ground inside it, both at the
    /// palette's `chrome_alpha` and both rounded by
    /// [`BarLayout::corner_radius`]. The rim is the bar's own edge rather than
    /// a mark on it, so it takes the same weight as the ground and reads a
    /// step lighter than it on a dark theme, a step darker on a light one.
    /// Those two are the bar's translucent layers — the wallpaper and the
    /// windows behind read through them over the backdrop the compositor
    /// blurs. A slot's hover or press wash is a plate laid on them, a step
    /// more solid, and the ink on top (the glyphs, the clock, the separator
    /// rule, every seam and bead) is drawn solid so it reads against whatever
    /// wallpaper is behind.
    ///
    /// Returns `None` only if the bar's pixel dimensions cannot be allocated
    /// (a surface that could never exist), so the caller fails closed rather
    /// than panicking. The window manager presents the
    /// returned surface and rounds it with [`BarLayout::corner_radius`].
    #[must_use]
    pub fn render(
        &mut self,
        taskbar: &Taskbar,
        scale: Scale,
        artwork: &mut dyn IconArtwork,
    ) -> Option<Surface> {
        let theme = taskbar.theme();
        let layout = taskbar.layout(scale);
        let mut icons = IconContext {
            cache: &mut self.icons,
            set: &self.icon_set,
            generation: self.icon_generation,
        };
        let mut surface = Surface::new(layout.bar.width, layout.bar.height)?;
        let origin = layout.bar.origin;
        // The clock is a de-emphasised annotation, set a step smaller like
        // every other caption on the desktop.
        let clock_font = BitmapFont::for_role(theme.fonts(), TextRole::Caption, scale);

        // The bar wears the same plate as the popups it opens; its regions are
        // laid out in screen space, so the interior the plate reports is moot.
        let _ = paint_surface_plate(
            &mut surface,
            (0, 0, layout.bar.width, layout.bar.height),
            (layout.corner_radius, plate_border(theme, scale)),
            theme,
            (theme.palette().surface_raised, ChromeLayer::Ground),
        );
        if !layout.library.is_empty() {
            let button = taskbar.library_button();
            let bounds = local_rect(layout.library, origin);
            let side = button.icon_side(bounds, scale, theme);
            let art = artwork.artwork(IconRequest::kind(IconKind::Library), side);
            button.render(&mut surface, bounds, scale, theme, art);
        }

        surface.fill_rect(
            local(layout.separator.left(), origin.x),
            local(layout.separator.top(), origin.y),
            layout.separator.width,
            layout.separator.height,
            theme.palette().border.into(),
        );

        let strip = taskbar.apps();
        for (index, slot) in layout.apps.iter().enumerate() {
            if slot.is_empty() {
                continue;
            }
            let (Some(item), Some(app)) = (strip.item(index), strip.get(index)) else {
                continue;
            };
            // The slot wears its own application's icon, resolved by the
            // session from the bundle the kernel attested owns the process,
            // so an application is recognised by its own picture.
            let bounds = local_rect(*slot, origin);
            let side = item.icon_side(bounds, scale, theme);
            let art = slot_artwork(app.artwork(), app.icon(), side, artwork);
            item.render(&mut surface, bounds, scale, theme, art);
        }

        paint_trailing(
            &mut surface,
            &layout,
            taskbar.notifications(),
            taskbar.clock().label(),
            theme.palette(),
            clock_font,
            &mut icons,
        );

        if !layout.switchboard.is_empty() {
            let signal = taskbar.tray().signal();
            let bounds = local_rect(layout.switchboard, origin);
            let side = signal.icon_side(bounds, scale, theme);
            let art = artwork.artwork(IconRequest::kind(signal.icon()), side);
            signal.render(&mut surface, bounds, scale, theme, art);
        }
        Some(surface)
    }

    /// Paint the open window picker into a [`Surface`] using the taskbar's
    /// own theme.
    ///
    /// Each cell draws the thumbnail the embedder supplied for that window,
    /// falling back to the application's own glyph when it has none, so a
    /// cell always states something; a grid with more rows than the panel
    /// shows draws the scrollbar that reaches the rest. Returns `None` when
    /// the picker is closed or its dimensions cannot be allocated (fail
    /// closed).
    #[must_use]
    pub fn render_picker(&self, taskbar: &Taskbar, scale: Scale) -> Option<Surface> {
        let layout = taskbar.picker_layout(scale)?;
        let theme = taskbar.theme();
        let mut surface = Surface::new(layout.panel.width, layout.panel.height)?;
        let origin = layout.panel.origin;
        let _ = paint_surface_plate(
            &mut surface,
            (0, 0, layout.panel.width, layout.panel.height),
            (layout.corner_radius, plate_border(theme, scale)),
            theme,
            (theme.palette().surface_raised, ChromeLayer::Ground),
        );
        let picker = taskbar.picker();
        for (index, cell) in layout.cells.iter().enumerate() {
            if cell.is_empty() {
                continue;
            }
            let (Some(preview), Some(entry)) = (picker.preview(index), picker.entries().get(index))
            else {
                continue;
            };
            // The picker holds no icon cache of its own, and this fallback is
            // the transient case of a window whose first frame has not
            // arrived: a popup that opens on a gesture, not a surface being
            // scrolled, so the glyph is drawn inline here.
            preview.render(
                &mut surface,
                local_rect(*cell, origin),
                scale,
                theme,
                entry.thumbnail(),
                None,
            );
        }
        if let Some(scrollbar) = layout.scrollbar {
            picker
                .scrollbar()
                .render(&mut surface, local_rect(scrollbar, origin), scale, theme);
        }
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
        );
        popup.search_field().render(
            &mut surface,
            local_rect(layout.search, origin),
            scale,
            theme,
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
                popup.row_artwork(index).map(IconPicture::Artwork),
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
            );
        }

        if let Some(scrollbar) = layout.scrollbar {
            popup
                .scrollbar()
                .render(&mut surface, local_rect(scrollbar, origin), scale, theme);
        }
        Some(surface)
    }

    /// Paint the notification popover into a [`Surface`] using the taskbar's
    /// own theme.
    ///
    /// Returns `None` when no notification is raised (there is nothing to
    /// draw) or when the surface cannot be allocated, so the caller fails
    /// closed rather than panicking. The window manager places the returned
    /// surface above the bar and rounds it with
    /// [`NotificationsLayout::corner_radius`](crate::NotificationsLayout::corner_radius),
    /// exactly as it rounds the bar and the library popup. Each card is the
    /// shared [`Notification`] control, so the popover restates no chrome and
    /// no colour algebra of its own.
    #[must_use]
    pub fn render_notifications(&self, taskbar: &Taskbar, scale: Scale) -> Option<Surface> {
        let layout = taskbar.notifications_layout(scale)?;
        let theme = taskbar.theme();
        let mut surface = Surface::new(layout.panel.width, layout.panel.height)?;
        let origin = layout.panel.origin;
        let local_bounds = Rect::new(0, 0, layout.panel.width, layout.panel.height);

        crate::layout::notif_panel(local_point(layout.anchor, origin)).render(
            &mut surface,
            local_bounds,
            scale,
            theme,
        );

        let notifications = taskbar.notifications();
        for placed in &layout.cards {
            let Some(note) = notifications.notification(placed.index) else {
                continue;
            };
            card_control(note).render(&mut surface, local_rect(placed.card, origin), scale, theme);
        }
        Some(surface)
    }

    /// Paint the Switchboard capsule's expanded readout into a [`Surface`]
    /// using the taskbar's own theme.
    ///
    /// Returns `None` while the readout is collapsed (there is nothing to
    /// draw) or when the surface cannot be allocated, so the caller fails
    /// closed rather than panicking. The window manager places the returned
    /// surface beside the capsule and rounds it with
    /// [`TrayReadoutLayout::corner_radius`](crate::TrayReadoutLayout::corner_radius)
    /// — the same popup radius the readout plate draws for itself, kept in
    /// step by construction.
    #[must_use]
    pub fn render_tray_readout(&self, taskbar: &Taskbar, scale: Scale) -> Option<Surface> {
        let layout = taskbar.tray_readout_layout(scale)?;
        let theme = taskbar.theme();
        let mut surface = Surface::new(layout.panel.width, layout.panel.height)?;
        let local = Rect::new(0, 0, layout.panel.width, layout.panel.height);
        taskbar
            .tray()
            .signal()
            .render_readout(&mut surface, local, scale, theme);
        Some(surface)
    }
}

/// The shared notification card for `note`, its role and composed state mapping
/// the notification's severity to the Reactive Alloy semantics: an
/// informational notice stays calm, a success shows the completion accent, a
/// warning shows the caution rail, and a critical notification reads as a
/// destructive, invalid state. Built once here so the popover render (and any
/// later hit-test) compose the identical control.
fn card_control(note: &TransientNotification) -> Notification {
    let (role, state) = match note.severity {
        NotifySeverity::Info => (ControlRole::Neutral, ControlState::idle()),
        NotifySeverity::Success => (
            ControlRole::Recommended,
            ControlState::idle().with_activity(ActivityState::Complete),
        ),
        NotifySeverity::Warning => (
            ControlRole::Neutral,
            ControlState::idle().with_validation(ValidationState::Warning),
        ),
        NotifySeverity::Critical => (
            ControlRole::Destructive,
            ControlState::idle().with_validation(ValidationState::Invalid),
        ),
    };
    let card = Notification::new(note.title.clone())
        .with_role(role)
        .with_state(state);
    if note.body.is_empty() {
        card
    } else {
        card.with_message(note.body.clone())
    }
}

/// Fill the notification and clock regions of an already-created bar
/// surface.
fn paint_trailing(
    surface: &mut Surface,
    layout: &BarLayout,
    notifications: &NotificationArea,
    clock_label: &str,
    palette: &Palette,
    clock_font: BitmapFont,
    icons: &mut IconContext<'_>,
) {
    let origin = layout.bar.origin;
    for (slot, signal) in layout.notifications.iter().zip(notifications.signals()) {
        draw_icon(
            surface,
            origin,
            *slot,
            signal.kind.icon(),
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
        clock_font,
    );
}

/// Draw `text` centred inside the screen-space `rect`, clipped to it. Text
/// wider than the region is truncated to the characters that fit.
fn draw_label(
    surface: &mut Surface,
    origin: Point,
    rect: Rect,
    text: &str,
    color: Color,
    font: BitmapFont,
) {
    if rect.is_empty() || text.is_empty() {
        return;
    }
    let fitted = font.truncate_to_width(text, rect.width);
    if fitted.is_empty() {
        return;
    }

    let text_width = font.text_width(fitted);
    let x_offset = rect.width.saturating_sub(text_width) / 2;
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

/// Draw a status signal's glyph centred in its screen-space `rect`, tinted
/// with `color`. The glyph is the scalable [`tairix_icon`] vector icon for
/// `kind`, rasterised to the slot size at this scale from the loaded
/// `/System/Graphics` icon set (with the built-in fallback), then composited
/// onto the bar-local surface through the shared blit path. The rasterised
/// glyph is taken from `icons`, which keeps it across frames so it is
/// converted only once per tint and size and re-rendered only on a theme or
/// scale change. An empty slot, a slot too small to hold a glyph, or an
/// unrenderable size paints nothing rather than panicking.
fn draw_icon(
    surface: &mut Surface,
    origin: Point,
    rect: Rect,
    kind: IconKind,
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
    let set = icons.set;
    let epoch = (color, side, icons.generation);
    let Some(image) = icons
        .cache
        .get_or_build(&epoch, kind, || set.icon(kind, color).rasterise(side))
    else {
        return;
    };
    let x_offset = rect.width.saturating_sub(side) / 2;
    let y_offset = rect.height.saturating_sub(side) / 2;
    let x = to_i32(local(rect.left(), origin.x).saturating_add(x_offset));
    let y = to_i32(local(rect.top(), origin.y).saturating_add(y_offset));
    surface.blit(x, y, &image);
}

/// Resolve the artwork an application slot draws: its own application artwork
/// when it has one, else its kind's shipped artwork at `side`, else `None` for
/// the control's built-in glyph.
///
/// The final glyph fallback is the shared icon slot's own, so `None` here is
/// not a blank slot.
fn slot_artwork<'a>(
    app: Option<&'a Surface>,
    kind: IconKind,
    side: u32,
    artwork: &'a mut dyn IconArtwork,
) -> Option<IconPicture<'a>> {
    match app {
        Some(surface) => Some(IconPicture::Artwork(surface)),
        None => artwork.artwork(IconRequest::kind(kind), side),
    }
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
