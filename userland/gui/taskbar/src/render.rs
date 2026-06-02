//! Painting the taskbar's regions into a pixel [`Surface`].
//!
//! [`render`] turns the taskbar's computed [`BarLayout`] and live state into
//! a premultiplied-alpha [`Surface`] sized to the bar, filling each region
//! with a colour from the active theme's [`Palette`] and drawing the clock
//! label and task titles on top with the shared [`BitmapFont`]. The surface is
//! the window manager's to place and round: the taskbar paints a *rectangular*
//! buffer and the compositor applies [`BarLayout::corner_radius`] through its
//! single anti-aliased rounded-corner path, exactly as it rounds windows
//! (`AGENTS.md` §2.2). There is no rounding — and no colour algebra — here.
//!
//! Region rectangles are in screen space; each is translated into the
//! surface's local space by subtracting the bar's origin. The translation
//! saturates and [`Surface::fill_rect`] clips, so a degenerate layout paints
//! nothing rather than panicking (`AGENTS.md` §2.9). A label is truncated to
//! the characters that fit its region so text never spills into a neighbour.
//! Notification-icon artwork remains a later increment.

use rustos_font::BitmapFont;
use rustos_geometry::{Point, Rect};
use rustos_raster::{Color, Surface};
use rustos_theme::{Palette, Theme};

use crate::layout::{BarLayout, MenuLayout};
use crate::menu::StartMenu;
use crate::taskbar::Taskbar;
use crate::tasks::TaskList;

/// Padding in pixels between a task slot's edge and its title text.
const LABEL_PADDING: u32 = 4;

/// Paint `taskbar` into a [`Surface`] using `theme`'s palette.
///
/// Returns `None` only if the bar's pixel dimensions cannot be allocated (a
/// surface that could never exist), so the caller fails closed rather than
/// panicking (`AGENTS.md` §2.9). The window manager presents the returned
/// surface and rounds it with [`BarLayout::corner_radius`].
#[must_use]
pub fn render(taskbar: &Taskbar, theme: &Theme) -> Option<Surface> {
    let layout = taskbar.layout();
    paint(
        &layout,
        taskbar.tasks(),
        taskbar.clock().label(),
        theme.palette(),
    )
}

/// Fill every region, then draw the clock and task titles into a fresh
/// surface.
fn paint(
    layout: &BarLayout,
    tasks: &TaskList,
    clock_label: &str,
    palette: &Palette,
) -> Option<Surface> {
    let mut surface = Surface::new(layout.bar.width, layout.bar.height)?;
    let origin = layout.bar.origin;
    let font = BitmapFont::mono5x7();

    surface.fill(palette.surface_raised.into());
    fill_region(
        &mut surface,
        origin,
        layout.start_button,
        palette.accent.into(),
    );

    for (slot, entry) in layout.tasks.iter().zip(tasks.entries()) {
        let focused = tasks.focused() == Some(entry.id);
        fill_region(
            &mut surface,
            origin,
            *slot,
            task_fill(palette, focused, entry.minimised),
        );
        draw_label(
            &mut surface,
            origin,
            *slot,
            &entry.title,
            task_text(palette, focused, entry.minimised),
            &font,
            Align::Leading,
        );
    }

    for slot in &layout.notifications {
        fill_region(&mut surface, origin, *slot, palette.on_surface_muted.into());
    }

    draw_label(
        &mut surface,
        origin,
        layout.clock,
        clock_label,
        palette.on_surface.into(),
        &font,
        Align::Centre,
    );

    Some(surface)
}

/// Paint the open start-menu popup into a [`Surface`] using `theme`'s palette.
///
/// Returns `None` when the menu is closed (there is nothing to draw) or when
/// the popup's pixel dimensions cannot be allocated, so the caller fails
/// closed rather than panicking (`AGENTS.md` §2.9). The window manager places
/// the returned surface above the bar and rounds it with
/// [`MenuLayout::corner_radius`], exactly as it rounds the bar (§2.2).
#[must_use]
pub fn render_menu(taskbar: &Taskbar, theme: &Theme) -> Option<Surface> {
    if !taskbar.start_menu().is_open() {
        return None;
    }
    let layout = taskbar.menu_layout();
    paint_menu(&layout, taskbar.start_menu(), theme.palette())
}

/// Fill the popup panel, then draw each entry's label into a fresh surface.
fn paint_menu(layout: &MenuLayout, menu: &StartMenu, palette: &Palette) -> Option<Surface> {
    if layout.panel.is_empty() {
        return None;
    }
    let mut surface = Surface::new(layout.panel.width, layout.panel.height)?;
    let origin = layout.panel.origin;
    let font = BitmapFont::mono5x7();

    surface.fill(palette.surface_raised.into());
    for (row, entry) in layout.entries.iter().zip(menu.entries()) {
        draw_label(
            &mut surface,
            origin,
            *row,
            entry.label(),
            palette.on_surface.into(),
            &font,
            Align::Leading,
        );
    }
    Some(surface)
}

/// Where a label sits along the main axis of its region.
#[derive(Copy, Clone, Eq, PartialEq)]
enum Align {
    /// Padded in from the leading (left/top) edge — used for task titles.
    Leading,
    /// Centred within the region — used for the clock.
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
/// region is truncated to the characters that fit (`AGENTS.md` §2.9).
fn draw_label(
    surface: &mut Surface,
    origin: Point,
    rect: Rect,
    text: &str,
    color: Color,
    font: &BitmapFont,
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
    let fitted = fit(text, max_chars(font, usable));
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

/// How many glyphs of [`font`](BitmapFont) fit in `width` pixels, accounting
/// for the tight bounding width (no trailing inter-glyph gap).
fn max_chars(font: &BitmapFont, width: u32) -> usize {
    if width < font.glyph_width() {
        return 0;
    }
    let extra = width - font.glyph_width();
    let advance = font.advance().max(1);
    (1 + extra / advance) as usize
}

/// Truncate `text` to at most `max` characters on a `char` boundary.
fn fit(text: &str, max: usize) -> &str {
    match text.char_indices().nth(max) {
        Some((byte, _)) => &text[..byte],
        None => text,
    }
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

/// Translate a screen-space coordinate into bar-local space, clamping a
/// coordinate that would fall before the bar's origin to zero.
fn local(coord: i32, origin: i32) -> u32 {
    u32::try_from(i64::from(coord) - i64::from(origin)).unwrap_or(0)
}

/// Saturating `u32` → `i32`.
fn to_i32(value: u32) -> i32 {
    i32::try_from(value).unwrap_or(i32::MAX)
}
