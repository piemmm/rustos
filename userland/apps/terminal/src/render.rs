//! Painting the terminal's screen into a pixel [`Surface`].
//!
//! [`render`] turns a [`Terminal`]'s [`Grid`] into a
//! premultiplied-alpha [`Surface`] sized to the app's content viewport, using
//! the active theme's [`Palette`](rustos_theme::Palette) for every colour and
//! the shared monospace
//! [`BitmapFont`] for every glyph. The surface is the window manager's to place
//! and round: the terminal paints a *rectangular* buffer and the compositor
//! applies any corner radius through its single anti-aliased rounded-corner
//! path (`AGENTS.md` §2.2). There is no rounding — and no colour algebra —
//! here.
//!
//! Each grid cell maps to one monospace glyph cell; rows are drawn top to
//! bottom and the cursor cell is highlighted with the accent role. Every
//! length saturates and every blit clips, so a viewport smaller than the grid
//! paints what fits rather than panicking (`AGENTS.md` §2.9).

use alloc::string::String;

use rustos_font::BitmapFont;
use rustos_geometry::Rect;
use rustos_raster::{Color, Surface};
use rustos_theme::Theme;

use crate::grid::{Cell, Grid};
use crate::shell::ShellSource;
use crate::terminal::Terminal;

/// Paint `terminal`'s screen into a [`Surface`] the size of `viewport`, using
/// `theme`'s palette.
///
/// Only `viewport`'s dimensions are used; the window manager places the
/// returned surface at `viewport`'s origin. Returns `None` only when those
/// dimensions cannot be allocated (a surface that could never exist), so the
/// caller fails closed rather than panicking (`AGENTS.md` §2.9).
#[must_use]
pub fn render<S: ShellSource>(
    terminal: &Terminal<S>,
    theme: &Theme,
    viewport: Rect,
) -> Option<Surface> {
    let font = BitmapFont::mono5x7();
    let mut surface = Surface::new(viewport.width, viewport.height)?;
    let palette = theme.palette();
    surface.fill(palette.surface.into());

    let grid = terminal.grid();
    let cell_width = font.advance();
    let line_height = font.line_height();

    draw_rows(
        &mut surface,
        &font,
        grid,
        line_height,
        palette.on_surface.into(),
    );
    draw_cursor(
        &mut surface,
        &font,
        grid,
        cell_width,
        line_height,
        palette.accent.into(),
        palette.on_accent.into(),
    );
    Some(surface)
}

/// Draw every grid row as one monospace string, top to bottom.
fn draw_rows(
    surface: &mut Surface,
    font: &BitmapFont,
    grid: &Grid,
    line_height: u32,
    color: Color,
) {
    for row in 0..grid.rows() {
        let y = line_height.saturating_mul(u32::from(row));
        let mut line = String::with_capacity(usize::from(grid.cols()));
        for col in 0..grid.cols() {
            let ch = grid.cell(col, row).map_or(' ', Cell::ch);
            line.push(ch);
        }
        font.draw_text(surface, 0, to_i32(y), &line, color);
    }
}

/// Highlight the cursor cell with `fill` and redraw its glyph in `text`.
fn draw_cursor(
    surface: &mut Surface,
    font: &BitmapFont,
    grid: &Grid,
    cell_width: u32,
    line_height: u32,
    fill: Color,
    text: Color,
) {
    let col = u32::from(grid.cursor_col());
    let row = u32::from(grid.cursor_row());
    let x = cell_width.saturating_mul(col);
    let y = line_height.saturating_mul(row);
    surface.fill_rect(x, y, cell_width, line_height, fill);
    let ch = grid
        .cell(grid.cursor_col(), grid.cursor_row())
        .map_or(' ', Cell::ch);
    let mut glyph = String::with_capacity(1);
    glyph.push(ch);
    font.draw_text(surface, to_i32(x), to_i32(y), &glyph, text);
}

/// Saturating `u32` → `i32`.
fn to_i32(value: u32) -> i32 {
    i32::try_from(value).unwrap_or(i32::MAX)
}
