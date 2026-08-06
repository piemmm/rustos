//! Painting the terminal's screen into a pixel [`Surface`].
//!
//! [`render`] turns a [`Terminal`]'s [`Grid`] into a premultiplied-alpha
//! [`Surface`] sized to the app's content viewport. Each
//! [`Cell`](tairix_vt::Cell) is drawn with its own rendition: the
//! [`Attributes`](tairix_vt::Attributes) the shared `lib/vt` parser folded onto it choose the
//! foreground and background, which are resolved through the profile's
//! [`Painted`] colours — the one place a terminal colour comes from. The
//! surface is the window manager's to place and round: the terminal paints a
//! *rectangular* buffer and the compositor applies any corner radius through
//! its single anti-aliased rounded-corner path.
//!
//! The monospace face carries no separate bold/italic/underline glyphs, so
//! those parsed attributes do not change the rendered shape — a renderer
//! limitation, not a parsing gap. A wide glyph's continuation cell paints
//! background only; the lead glyph covers it.
//!
//! Rows are drawn top to bottom and the cursor cell, when visible, is drawn
//! as the scheme's cursor block. Every length saturates and every blit clips,
//! so a viewport smaller than the grid paints what fits rather than
//! panicking.
//!
//! Translucency is not a separate step: the default background is filled at
//! the alpha the profile asks for, so the compositor's own premultiplied
//! blend shows what is behind the window while a glyph drawn over it stays
//! opaque. The screen effects ([`crate::effects`]) run afterwards, over the
//! finished frame.

use alloc::string::String;

use tairix_font::BitmapFont;
use tairix_geometry::Rect;
use tairix_raster::{Color, Surface};
use tairix_vt::{char_width, CONTINUATION};

use crate::grid::Grid;
use crate::scheme::Painted;
use crate::shell::ShellSource;
use crate::terminal::Terminal;

/// Paint `terminal`'s screen into a [`Surface`] the size of `viewport`, in
/// `painted`'s colours.
///
/// Only `viewport`'s dimensions are used; the window manager places the
/// returned surface at `viewport`'s origin. Returns `None` only when those
/// dimensions cannot be allocated (a surface that could never exist), so the
/// caller fails closed rather than panicking.
#[must_use]
pub fn render<S: ShellSource>(
    terminal: &Terminal<S>,
    painted: &Painted,
    viewport: Rect,
    font: BitmapFont,
) -> Option<Surface> {
    let mut surface = Surface::new(viewport.width, viewport.height)?;
    surface.fill(painted.background());

    let grid = terminal.grid();
    let cell_width = font.cell_width();
    let line_height = font.line_height();

    draw_cells(&mut surface, font, grid, painted, cell_width, line_height);
    if grid.cursor_visible() {
        draw_cursor(&mut surface, font, grid, cell_width, line_height, painted);
    }
    Some(surface)
}

/// Draw every grid cell with its own rendition, top to bottom, left to right.
fn draw_cells(
    surface: &mut Surface,
    font: BitmapFont,
    grid: &Grid,
    painted: &Painted,
    cell_width: u32,
    line_height: u32,
) {
    let base = painted.background();
    for row in 0..grid.rows() {
        let y = line_height.saturating_mul(u32::from(row));
        let mut col = 0;
        while col < grid.cols() {
            let Some(cell) = grid.cell(col, row) else {
                col += 1;
                continue;
            };
            if cell.ch == CONTINUATION {
                col += 1;
                continue;
            }
            let x = cell_width.saturating_mul(u32::from(col));
            let (fg, bg) = painted.cell_colors(cell.attrs);
            let cells = char_width(cell.ch);
            if bg != base {
                surface.fill_rect(
                    x,
                    y,
                    cell_width.saturating_mul(u32::from(cells)),
                    line_height,
                    bg,
                );
            }
            draw_glyph(surface, font, to_i32(x), to_i32(y), cell.ch, fg);
            col = col.saturating_add(cells);
        }
    }
}

/// Draw the cursor cell as the scheme's cursor block with its glyph in the
/// scheme's cursor-text colour.
///
/// Both are opaque whatever the window's translucency: a cursor that faded
/// with the background would be the hardest thing on screen to find.
fn draw_cursor(
    surface: &mut Surface,
    font: BitmapFont,
    grid: &Grid,
    cell_width: u32,
    line_height: u32,
    painted: &Painted,
) {
    let x = cell_width.saturating_mul(u32::from(grid.cursor_col()));
    let y = line_height.saturating_mul(u32::from(grid.cursor_row()));
    surface.fill_rect(
        x,
        y,
        cell_width,
        line_height,
        painted.scheme.cursor.opaque(),
    );
    let ch = grid
        .cell(grid.cursor_col(), grid.cursor_row())
        .map_or(' ', |cell| cell.ch);
    // The cursor over a wide glyph's continuation cell shows covered space.
    let ch = if ch == CONTINUATION { ' ' } else { ch };
    draw_glyph(
        surface,
        font,
        to_i32(x),
        to_i32(y),
        ch,
        painted.scheme.cursor_text.opaque(),
    );
}

/// Draw a single glyph `ch` at `(x, y)` in `color`.
fn draw_glyph(surface: &mut Surface, font: BitmapFont, x: i32, y: i32, ch: char, color: Color) {
    let mut glyph = String::with_capacity(ch.len_utf8());
    glyph.push(ch);
    font.draw_text(surface, x, y, &glyph, color);
}

/// Saturating `u32` → `i32`.
fn to_i32(value: u32) -> i32 {
    i32::try_from(value).unwrap_or(i32::MAX)
}
