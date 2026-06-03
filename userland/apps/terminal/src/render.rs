//! Painting the terminal's screen into a pixel [`Surface`].
//!
//! [`render`] turns a [`Terminal`]'s [`Grid`] into a premultiplied-alpha
//! [`Surface`] sized to the app's content viewport. Each
//! [`Cell`](rustos_vt::Cell) is drawn with its own rendition: the
//! [`Attributes`] the shared `lib/vt` parser folded onto
//! it select the foreground and background, which are resolved against the
//! active theme's [`Palette`](rustos_theme::Palette) and the standard ANSI
//! colour tables. The surface is the window manager's to place and round: the
//! terminal paints a *rectangular* buffer and the compositor applies any corner
//! radius through its single anti-aliased rounded-corner path (`AGENTS.md`
//! §2.2). There is no rounding here.
//!
//! Colour is resolved one way (`AGENTS.md` §2.2): a cell's
//! [`Color::Default`](rustos_vt::Color) foreground and background take the
//! theme's `on_surface` / `surface` roles, the 16 [`BasicColor`]s and the
//! 256-colour palette map through the standard ANSI tables, and truecolour is
//! used directly; [`Attributes::reverse`] swaps the pair and
//! [`Attributes::bold`] brightens a basic colour. The 5×7 monospace face
//! carries no separate bold/italic/underline glyphs, so those parsed
//! attributes do not change the rendered shape — a renderer limitation, not a
//! parsing gap.
//!
//! Rows are drawn top to bottom and the cursor cell, when visible, is
//! highlighted with the accent role. Every length saturates and every blit
//! clips, so a viewport smaller than the grid paints what fits rather than
//! panicking (`AGENTS.md` §2.9).

use alloc::string::String;

use rustos_font::BitmapFont;
use rustos_geometry::Rect;
use rustos_raster::{Color, Surface};
use rustos_theme::{Palette, Theme};
use rustos_vt::{Attributes, BasicColor, Color as VtColor};

use crate::grid::Grid;
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
    surface.fill(Color::from(palette.surface));

    let grid = terminal.grid();
    let cell_width = font.advance();
    let line_height = font.line_height();

    draw_cells(&mut surface, &font, grid, palette, cell_width, line_height);
    if grid.cursor_visible() {
        draw_cursor(
            &mut surface,
            &font,
            grid,
            cell_width,
            line_height,
            Color::from(palette.accent),
            Color::from(palette.on_accent),
        );
    }
    Some(surface)
}

/// Draw every grid cell with its own rendition, top to bottom, left to right.
fn draw_cells(
    surface: &mut Surface,
    font: &BitmapFont,
    grid: &Grid,
    palette: &Palette,
    cell_width: u32,
    line_height: u32,
) {
    let base = Color::from(palette.surface);
    for row in 0..grid.rows() {
        let y = line_height.saturating_mul(u32::from(row));
        for col in 0..grid.cols() {
            let Some(cell) = grid.cell(col, row) else {
                continue;
            };
            let x = cell_width.saturating_mul(u32::from(col));
            let (fg, bg) = resolve_colors(cell.attrs, palette);
            if bg != base {
                surface.fill_rect(x, y, cell_width, line_height, bg);
            }
            draw_glyph(surface, font, to_i32(x), to_i32(y), cell.ch, fg);
        }
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
    let x = cell_width.saturating_mul(u32::from(grid.cursor_col()));
    let y = line_height.saturating_mul(u32::from(grid.cursor_row()));
    surface.fill_rect(x, y, cell_width, line_height, fill);
    let ch = grid
        .cell(grid.cursor_col(), grid.cursor_row())
        .map_or(' ', |cell| cell.ch);
    draw_glyph(surface, font, to_i32(x), to_i32(y), ch, text);
}

/// Draw a single glyph `ch` at `(x, y)` in `color`.
fn draw_glyph(surface: &mut Surface, font: &BitmapFont, x: i32, y: i32, ch: char, color: Color) {
    let mut glyph = String::with_capacity(ch.len_utf8());
    glyph.push(ch);
    font.draw_text(surface, x, y, &glyph, color);
}

/// Resolve a cell's [`Attributes`] into concrete foreground and background
/// [`Color`]s, applying reverse video last so it swaps the resolved pair.
fn resolve_colors(attrs: Attributes, palette: &Palette) -> (Color, Color) {
    let foreground = palette.on_surface;
    let background = palette.surface;
    let fg = resolve(attrs.foreground, attrs.bold, Color::from(foreground));
    let bg = resolve(attrs.background, false, Color::from(background));
    if attrs.reverse {
        (bg, fg)
    } else {
        (fg, bg)
    }
}

/// Resolve one [`VtColor`] to a concrete [`Color`], falling back to `default`
/// for [`VtColor::Default`]. A `bold` basic colour is brightened, the common
/// terminal convention.
fn resolve(color: VtColor, bold: bool, default: Color) -> Color {
    match color {
        VtColor::Default => default,
        VtColor::Basic(basic) => basic_color(if bold { brighten(basic) } else { basic }),
        VtColor::Indexed(index) => indexed_color(index),
        VtColor::Rgb(r, g, b) => Color::rgb(r, g, b),
    }
}

/// The bright counterpart of a basic colour; an already-bright colour is left
/// unchanged.
fn brighten(basic: BasicColor) -> BasicColor {
    if basic.is_bright() {
        basic
    } else {
        BasicColor::from_index(basic.index() + 8).unwrap_or(basic)
    }
}

/// The RGB of one of the sixteen ANSI [`BasicColor`]s (the standard xterm
/// palette).
fn basic_color(basic: BasicColor) -> Color {
    const PALETTE: [(u8, u8, u8); 16] = [
        (0, 0, 0),
        (205, 0, 0),
        (0, 205, 0),
        (205, 205, 0),
        (0, 0, 238),
        (205, 0, 205),
        (0, 205, 205),
        (229, 229, 229),
        (127, 127, 127),
        (255, 0, 0),
        (0, 255, 0),
        (255, 255, 0),
        (92, 92, 255),
        (255, 0, 255),
        (0, 255, 255),
        (255, 255, 255),
    ];
    let (r, g, b) = PALETTE[usize::from(basic.index())];
    Color::rgb(r, g, b)
}

/// The RGB of a 256-colour palette `index`: `0..=15` are the basic colours,
/// `16..=231` the 6×6×6 colour cube, and `232..=255` the 24-step greyscale ramp.
fn indexed_color(index: u8) -> Color {
    if index < 16 {
        return BasicColor::from_index(index).map_or(Color::rgb(0, 0, 0), basic_color);
    }
    if index < 232 {
        let offset = u32::from(index - 16);
        let r = cube_level(offset / 36);
        let g = cube_level((offset / 6) % 6);
        let b = cube_level(offset % 6);
        return Color::rgb(r, g, b);
    }
    let level = u8::try_from(u32::from(index - 232) * 10 + 8).unwrap_or(u8::MAX);
    Color::rgb(level, level, level)
}

/// One channel of the 6×6×6 colour cube: level `0` is black, the rest are
/// `level * 40 + 55`.
fn cube_level(level: u32) -> u8 {
    if level == 0 {
        0
    } else {
        u8::try_from(level * 40 + 55).unwrap_or(u8::MAX)
    }
}

/// Saturating `u32` → `i32`.
fn to_i32(value: u32) -> i32 {
    i32::try_from(value).unwrap_or(i32::MAX)
}
