//! The terminal's geometry: the screen grid, the text size that fits it on a
//! given display, and the window that holds it.
//!
//! Every size here is *derived* — from the shared monospace face's own
//! metrics at the size in force, from the desktop's density, and from the
//! window furniture the compositor will wrap the client in. Nothing is a
//! hand-picked pixel count, so the same rules give a workable window on a
//! 640×480 display and on a dense 4K one.
//!
//! # Why the terminal opens at the size it does
//!
//! A terminal's natural size is a *character count*, not a pixel count:
//! [`COLS`]×[`ROWS`] is the conventional 80×25 screen. The window is
//! therefore whatever that grid measures in the face actually being drawn
//! with, and the profile's text size is reduced — never the grid — when the
//! display is too small to hold it ([`fit_font_size`]). A terminal that
//! silently dropped to 60 columns would break every program that lays itself
//! out for 80.

use tairix_controls::{FrameInsets, WindowFrame, WindowFurnitureState};
use tairix_font::BitmapFont;
use tairix_geometry::Scale;
use tairix_theme::Theme;

use crate::grid::MAX_DIMENSION;
use crate::profile::MIN_FONT_SIZE_PX;

/// Columns of the terminal's screen grid when it opens — the conventional
/// 80-column text screen every command-line program assumes.
pub const COLS: u16 = 80;

/// Rows of the terminal's screen grid when it opens — the conventional
/// 25-row text screen.
pub const ROWS: u16 = 25;

/// The physical pixel size of a `cols`×`rows` grid drawn in `font`.
///
/// The advance and line height come from the face itself, so the window and
/// the renderer can never disagree about how much room a grid needs.
#[must_use]
pub fn grid_size(cols: u16, rows: u16, font: BitmapFont) -> (u32, u32) {
    let advance = font.cell_width().max(1);
    let line_height = font.line_height().max(1);
    (
        advance.saturating_mul(u32::from(cols)),
        line_height.saturating_mul(u32::from(rows)),
    )
}

/// The character grid `(cols, rows)` that fits a `width_px` × `height_px`
/// client drawn in `font`.
///
/// Floored so the grid never exceeds the surface (no clipped cell), at least
/// `1`×`1`, and capped at [`MAX_DIMENSION`] so a huge window never asks for
/// an unbounded grid.
#[must_use]
pub fn grid_dims(width_px: u32, height_px: u32, font: BitmapFont) -> (u16, u16) {
    let advance = font.cell_width().max(1);
    let line_height = font.line_height().max(1);
    let fit = |extent: u32, cell: u32| -> u16 {
        u16::try_from((extent / cell).clamp(1, u32::from(MAX_DIMENSION))).unwrap_or(MAX_DIMENSION)
    };
    (fit(width_px, advance), fit(height_px, line_height))
}

/// The per-edge physical pixels the window furniture adds around a client
/// viewport.
///
/// Read from the one shared frame definition the compositor decorates with,
/// so an app sizing itself to the screen and the window manager framing it
/// agree by construction rather than by two copies of the same arithmetic.
fn chrome_insets(theme: &Theme, scale: Scale, resizable: bool) -> FrameInsets {
    WindowFrame::new(WindowFurnitureState {
        resizable,
        ..WindowFurnitureState::default()
    })
    .insets(scale, theme)
}

/// The physical pixels the window furniture adds around a client viewport, as
/// `(horizontal, vertical)` totals.
#[must_use]
pub fn chrome_extent(theme: &Theme, scale: Scale, resizable: bool) -> (u32, u32) {
    let insets = chrome_insets(theme, scale, resizable);
    (
        insets.left.saturating_add(insets.right),
        insets.top.saturating_add(insets.bottom),
    )
}

/// The largest text size, in logical pixels, no greater than `preferred`
/// whose [`COLS`]×[`ROWS`] grid still fits `screen` once the window furniture
/// is allowed for.
///
/// The search walks down one logical pixel at a time and stops at
/// [`MIN_FONT_SIZE_PX`], so a display too small even for the smallest legible
/// face gets that face and a window clamped to the screen rather than a
/// terminal that refuses to open.
#[must_use]
pub fn fit_font_size(preferred: u16, screen: (u32, u32), theme: &Theme, scale: Scale) -> u16 {
    let (chrome_w, chrome_h) = chrome_extent(theme, scale, true);
    let budget_w = screen.0.saturating_sub(chrome_w);
    let budget_h = screen.1.saturating_sub(chrome_h);
    let mut size = preferred.max(MIN_FONT_SIZE_PX);
    while size > MIN_FONT_SIZE_PX {
        let font = BitmapFont::monospace(scale.scale_length(u32::from(size)));
        let (need_w, need_h) = grid_size(COLS, ROWS, font);
        if need_w <= budget_w && need_h <= budget_h {
            break;
        }
        size -= 1;
    }
    size
}

/// The client size, in physical pixels, a terminal drawn in `font` opens at
/// on `screen`.
///
/// The [`COLS`]×[`ROWS`] grid, clamped to what the screen can actually show
/// once the furniture is allowed for — so the window never opens larger than
/// the display it is shown on.
#[must_use]
pub fn window_size(
    font: BitmapFont,
    screen: (u32, u32),
    theme: &Theme,
    scale: Scale,
) -> (u32, u32) {
    let (chrome_w, chrome_h) = chrome_extent(theme, scale, true);
    let (want_w, want_h) = grid_size(COLS, ROWS, font);
    (
        want_w.clamp(1, screen.0.saturating_sub(chrome_w).max(1)),
        want_h.clamp(1, screen.1.saturating_sub(chrome_h).max(1)),
    )
}

#[cfg(test)]
#[path = "layout_tests.rs"]
mod tests;
