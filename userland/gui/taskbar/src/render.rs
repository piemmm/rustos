//! Painting the taskbar's regions into a pixel [`Surface`].
//!
//! [`TaskbarRenderer::render`] turns the taskbar's computed [`BarLayout`] and
//! live state into a premultiplied-alpha [`Surface`] sized to the bar. The
//! two permanent leading launchers are the shared `lib/controls`
//! [`IconButton`](tairix_controls::IconButton)s the model owns, painted with
//! their live hover/pressed state; every pinned shortcut and running task is
//! one shared [`TaskbarItem`] — an icon-only slot for a pin, an icon+title
//! plate for a task — so the bar's application buttons have exactly one
//! visual recipe. A pin's per-application artwork (rasterised by the
//! session) is blitted through the control; a running task whose window
//! matches a pin borrows that same artwork, so one application shows one
//! icon everywhere. The surface is the window manager's to place and round:
//! the taskbar paints a *rectangular* buffer and the compositor applies
//! [`BarLayout::corner_radius`] through its single anti-aliased
//! rounded-corner path, exactly as it rounds windows. There is no rounding —
//! and no colour algebra — here.
//!
//! [`TaskbarRenderer::render_menu`] paints the open context menu: the shared
//! `lib/controls` [`Menu`](tairix_controls::Menu) plate at the geometry
//! [`Taskbar::menu_layout`] computed, presented by the window manager as its
//! own small window above the bar.
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
use tairix_controls::{ControlRole, ControlState, PointerState, TaskVisibility, TaskbarItem};
use tairix_font::BitmapFont;
use tairix_geometry::{Point, Rect, Scale};
use tairix_icon::{IconKind, IconSet};
use tairix_raster::{Color, RasterCache, Surface};
use tairix_theme::{Palette, TextRole, Theme};

use crate::layout::BarLayout;
use crate::library::{chrome_panel, list_row, popup_font, LibraryFocus};
use crate::notifications::{NotificationArea, NotifySeverity, TransientNotification};
use crate::pins::PinView;
use crate::taskbar::Taskbar;

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

        let strip = taskbar.pins();
        for (index, slot) in layout.pins.iter().enumerate() {
            if slot.is_empty() {
                continue;
            }
            let Some(item) = strip.item(index, taskbar.tasks()) else {
                continue;
            };
            let artwork = strip.get(index).and_then(PinView::artwork);
            item.render(
                &mut surface,
                local_rect(*slot, origin),
                scale,
                theme,
                fonts.text,
                artwork,
            );
        }

        for (index, (slot, entry)) in layout
            .tasks
            .iter()
            .zip(taskbar.tasks().entries())
            .enumerate()
        {
            if slot.is_empty() {
                continue;
            }
            let visibility = if taskbar.tasks().focused() == Some(entry.id) {
                TaskVisibility::Active
            } else if entry.minimised {
                TaskVisibility::Minimized
            } else {
                TaskVisibility::Running
            };
            let pointer = if taskbar.task_hover() == Some(index) {
                PointerState::Hover
            } else {
                PointerState::None
            };
            // A running window that matches a pin borrows the pin's identity
            // — its class glyph and per-application artwork — so one
            // application shows one icon on the bar.
            let pin = strip.view_for_window(entry.id);
            let icon = pin.map_or(IconKind::AppBundle, PinView::icon);
            let artwork = pin.and_then(PinView::artwork);
            TaskbarItem::new(entry.title.clone(), icon)
                .with_visibility(visibility)
                .with_state(ControlState::idle().with_pointer(pointer))
                .render(
                    &mut surface,
                    local_rect(*slot, origin),
                    scale,
                    theme,
                    fonts.text,
                    artwork,
                );
        }

        paint_trailing(
            &mut surface,
            &layout,
            taskbar.notifications(),
            taskbar.clock().label(),
            theme.palette(),
            fonts,
            &mut icons,
        );
        Some(surface)
    }

    /// Paint the open context menu into a [`Surface`] using the taskbar's
    /// own theme.
    ///
    /// Returns `None` when the menu is closed (there is nothing to draw) or
    /// when its pixel dimensions cannot be allocated, so the caller fails
    /// closed rather than panicking. The window manager places the returned
    /// surface above the bar and rounds it with
    /// [`MenuLayout::corner_radius`](crate::menu::MenuLayout::corner_radius).
    #[must_use]
    pub fn render_menu(&self, taskbar: &Taskbar, scale: Scale) -> Option<Surface> {
        let layout = taskbar.menu_layout(scale)?;
        let theme = taskbar.theme();
        let fonts = PanelFonts::resolve(theme, scale);
        let mut surface = Surface::new(layout.panel.width, layout.panel.height)?;
        let local = Rect::new(0, 0, layout.panel.width, layout.panel.height);
        taskbar
            .menu()
            .control()
            .render(&mut surface, local, scale, theme, fonts.text);
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
        let fonts = PanelFonts::resolve(theme, scale);
        let mut surface = Surface::new(layout.panel.width, layout.panel.height)?;
        let origin = layout.panel.origin;
        let local_bounds = Rect::new(0, 0, layout.panel.width, layout.panel.height);

        crate::layout::notif_panel(local_point(layout.anchor, origin)).render(
            &mut surface,
            local_bounds,
            scale,
            theme,
            fonts.text,
        );

        let notifications = taskbar.notifications();
        for placed in &layout.cards {
            let Some(note) = notifications.notification(placed.index) else {
                continue;
            };
            card_control(note).render(
                &mut surface,
                local_rect(placed.card, origin),
                scale,
                theme,
                fonts.text,
            );
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

/// The shared notification card for `note`, its role and composed state
/// mapping the notification's severity to the Reactive Alloy semantics: an
/// informational notice stays calm, a success shows the completion accent, a
/// warning shows the caution rail, and a critical notification reads as a
/// destructive, invalid state. Built once here so the popover render (and any
/// later hit-test) compose the identical control (§2.2).
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
    fonts: PanelFonts,
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
        fonts.clock,
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
