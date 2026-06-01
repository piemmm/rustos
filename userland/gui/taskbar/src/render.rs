//! Painting the taskbar's regions into a pixel [`Surface`].
//!
//! [`render`] turns the taskbar's computed [`BarLayout`] and live state into
//! a premultiplied-alpha [`Surface`] sized to the bar, filling each region
//! with a colour from the active theme's [`Palette`]. The surface is the
//! window manager's to place and round: the taskbar paints a *rectangular*
//! buffer and the compositor applies [`BarLayout::corner_radius`] through its
//! single anti-aliased rounded-corner path, exactly as it rounds windows
//! (`AGENTS.md` §2.2). There is no rounding — and no colour algebra — here.
//!
//! Region rectangles are in screen space; each is translated into the
//! surface's local space by subtracting the bar's origin. The translation
//! saturates and [`Surface::fill_rect`] clips, so a degenerate layout paints
//! nothing rather than panicking (`AGENTS.md` §2.9). Glyphs for the clock and
//! the task titles, and notification-icon artwork, are a later increment;
//! this increment lays down the themed region fills they will draw onto.

use rustos_geometry::{Point, Rect};
use rustos_raster::{Color, Surface};
use rustos_theme::{Palette, Theme};

use crate::layout::BarLayout;
use crate::taskbar::Taskbar;
use crate::tasks::TaskList;

/// Paint `taskbar` into a [`Surface`] using `theme`'s palette.
///
/// Returns `None` only if the bar's pixel dimensions cannot be allocated (a
/// surface that could never exist), so the caller fails closed rather than
/// panicking (`AGENTS.md` §2.9). The window manager presents the returned
/// surface and rounds it with [`BarLayout::corner_radius`].
#[must_use]
pub fn render(taskbar: &Taskbar, theme: &Theme) -> Option<Surface> {
    let layout = taskbar.layout();
    paint(&layout, taskbar.tasks(), theme.palette())
}

/// Fill the bar background, the start button, every task slot, and every
/// notification icon into a fresh surface.
fn paint(layout: &BarLayout, tasks: &TaskList, palette: &Palette) -> Option<Surface> {
    let mut surface = Surface::new(layout.bar.width, layout.bar.height)?;
    let origin = layout.bar.origin;

    surface.fill(palette.surface_raised.into());
    fill_region(
        &mut surface,
        origin,
        layout.start_button,
        palette.accent.into(),
    );

    for (slot, entry) in layout.tasks.iter().zip(tasks.entries()) {
        let fill = task_fill(palette, tasks.focused() == Some(entry.id), entry.minimised);
        fill_region(&mut surface, origin, *slot, fill);
    }

    for slot in &layout.notifications {
        fill_region(&mut surface, origin, *slot, palette.on_surface_muted.into());
    }

    Some(surface)
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
