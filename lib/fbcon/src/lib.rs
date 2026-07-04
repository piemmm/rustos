//! Shared framebuffer text-console engine (`lib/fbcon`).
//!
//! An architecture-neutral ANSI/VT/xterm-256color terminal that renders the
//! shared `rustos_vt::Op` stream straight onto a borrowed 32-bit scan-out
//! surface with the shared `rustos_font` glyph atlas. Every arch port drives
//! its display console through this one definition; a port supplies only the
//! board-specific surface (discovered at runtime) and calls
//! [`TextConsole::write_bytes`].
//!
//! The engine holds no cell grid — the pixels are the state — so reaching the
//! bottom scrolls the pixels up (a real terminal scroll), not a ring wrap. It
//! never allocates, so a freestanding boot console with no global allocator
//! links it directly.

#![no_std]
#![forbid(unsafe_code)]
#![deny(missing_docs)]

use rustos_font::glyphs;
use rustos_vt::{Attributes, Color, EraseMode, Op, Parser};

/// Glyph cell width in pixels at scale 1: the atlas glyph plus one column of
/// inter-character spacing.
const CELL_WIDTH: u32 = glyphs::GLYPH_WIDTH + 1;

/// Glyph cell height in pixels at scale 1: the atlas glyph plus one row of
/// inter-line spacing.
const CELL_HEIGHT: u32 = glyphs::GLYPH_HEIGHT + 1;

/// What [`Color::Default`] resolves to for text: light grey, opaque.
///
/// The surface stores each pixel as a little-endian `u32` whose bytes are
/// `B, G, R, X/A`, so a colour is packed `0xFF00_0000 | (r << 16) | (g << 8)
/// | b` ([`pack_rgb`]). Grey is symmetric across the channels regardless.
const DEFAULT_FOREGROUND: u32 = 0xFFD8_D8D8;

/// What [`Color::Default`] resolves to for the background: opaque black.
const DEFAULT_BACKGROUND: u32 = 0xFF00_0000;

/// Largest glyph scale the policy selects.
///
/// Beyond 4× the 5×7 atlas looks blocky without gaining legibility, so the
/// policy caps there even on very tall displays.
const MAX_SCALE: u32 = 4;

/// Pixel rows of display height per unit of glyph scale.
///
/// `height / 360` keeps roughly 45 text rows on screen at every common mode
/// (480p → 1×, 720p → 2×, 1080p → 3×, 2160p → 4×): enough log to read, large
/// enough to read it on a TV across a room.
const ROWS_PER_SCALE: u32 = 360;

/// Validated framebuffer text geometry: the scan-out extents plus the glyph
/// scale the policy chose for them.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub struct Geometry {
    /// Visible width in pixels.
    pub width_px: u32,
    /// Visible height in pixels.
    pub height_px: u32,
    /// Pixels (not bytes) between consecutive scanlines.
    pub stride_px: u32,
    /// Integer glyph scale (`1..=MAX_SCALE`).
    pub scale: u32,
}

impl Geometry {
    /// Derive the text geometry for a firmware-confirmed surface.
    ///
    /// Returns `None` when the surface cannot host even one glyph cell, the
    /// pitch is not whole pixels, or the pitch is narrower than a scanline —
    /// the caller leaves the console unconfigured rather than rendering out of
    /// bounds (fail closed: the geometry is firmware input).
    #[must_use]
    pub fn for_display(width_px: u32, height_px: u32, pitch_bytes: u32) -> Option<Self> {
        if pitch_bytes % 4 != 0 {
            return None;
        }
        let stride_px = pitch_bytes / 4;
        if width_px == 0 || height_px == 0 || stride_px < width_px {
            return None;
        }
        let scale = (height_px / ROWS_PER_SCALE).clamp(1, MAX_SCALE);
        let geometry = Self {
            width_px,
            height_px,
            stride_px,
            scale,
        };
        (geometry.columns() != 0 && geometry.rows() != 0).then_some(geometry)
    }

    /// Text columns the surface holds.
    #[must_use]
    pub const fn columns(&self) -> u32 {
        self.width_px / (CELL_WIDTH * self.scale)
    }

    /// Text rows the surface holds.
    #[must_use]
    pub const fn rows(&self) -> u32 {
        self.height_px / (CELL_HEIGHT * self.scale)
    }

    /// Pixel rows one text row occupies.
    const fn cell_height_px(&self) -> u32 {
        CELL_HEIGHT * self.scale
    }

    /// Pixel columns one text column occupies.
    const fn cell_width_px(&self) -> u32 {
        CELL_WIDTH * self.scale
    }

    /// Pixel count of the rendered band (`stride × height`), the slice length
    /// the renderer draws into.
    #[must_use]
    pub const fn pixel_count(&self) -> usize {
        self.stride_px as usize * self.height_px as usize
    }
}

/// The pixel-row band `[start, end)` a rendering call touched, so a
/// freestanding writer can clean exactly those scanlines to coherency.
pub type DirtyBand = (u32, u32);

/// Merge two optional dirty bands into their union.
#[must_use]
pub fn merge_bands(a: Option<DirtyBand>, b: Option<DirtyBand>) -> Option<DirtyBand> {
    match (a, b) {
        (Some((a0, a1)), Some((b0, b1))) => Some((a0.min(b0), a1.max(b1))),
        (band, None) | (None, band) => band,
    }
}

/// Pack an opaque 8-bit-per-channel colour into a scan-out `u32`.
///
/// Red occupies bits 16..=23, green 8..=15, blue 0..=7, with the top byte
/// opaque.
const fn pack_rgb(r: u8, g: u8, b: u8) -> u32 {
    0xFF00_0000 | ((r as u32) << 16) | ((g as u32) << 8) | (b as u32)
}

/// The sixteen ANSI base colours as scan-out pixels, indexed by palette index
/// `0..=15` (the standard xterm values).
const BASIC_PALETTE: [u32; 16] = [
    pack_rgb(0, 0, 0),
    pack_rgb(205, 0, 0),
    pack_rgb(0, 205, 0),
    pack_rgb(205, 205, 0),
    pack_rgb(0, 0, 238),
    pack_rgb(205, 0, 205),
    pack_rgb(0, 205, 205),
    pack_rgb(229, 229, 229),
    pack_rgb(127, 127, 127),
    pack_rgb(255, 0, 0),
    pack_rgb(0, 255, 0),
    pack_rgb(255, 255, 0),
    pack_rgb(92, 92, 255),
    pack_rgb(255, 0, 255),
    pack_rgb(0, 255, 255),
    pack_rgb(255, 255, 255),
];

/// The pixel for a 256-colour palette index: the sixteen base colours, the
/// 6×6×6 colour cube (`16..=231`), then the 24-step grey ramp (`232..=255`).
fn indexed_pixel(index: u8) -> u32 {
    match index {
        0..=15 => BASIC_PALETTE[index as usize],
        16..=231 => {
            let i = index - 16;
            let level = |v: u8| -> u8 {
                if v == 0 {
                    0
                } else {
                    55 + v * 40
                }
            };
            pack_rgb(level(i / 36), level((i / 6) % 6), level(i % 6))
        }
        _ => {
            let grey = 8 + (index - 232) * 10;
            pack_rgb(grey, grey, grey)
        }
    }
}

/// Resolve a foreground colour to a scan-out pixel. `bold` brightens one of the
/// eight dim base colours to its bright twin, as terminals do.
fn foreground_pixel(color: Color, bold: bool) -> u32 {
    match color {
        Color::Default => DEFAULT_FOREGROUND,
        Color::Basic(basic) => {
            let index = if bold && !basic.is_bright() {
                basic.index() + 8
            } else {
                basic.index()
            };
            BASIC_PALETTE[index as usize]
        }
        Color::Indexed(index) => indexed_pixel(index),
        Color::Rgb(r, g, b) => pack_rgb(r, g, b),
    }
}

/// Resolve a background colour to a scan-out pixel.
fn background_pixel(color: Color) -> u32 {
    match color {
        Color::Default => DEFAULT_BACKGROUND,
        Color::Basic(basic) => BASIC_PALETTE[basic.index() as usize],
        Color::Indexed(index) => indexed_pixel(index),
        Color::Rgb(r, g, b) => pack_rgb(r, g, b),
    }
}

/// Map a printed character to its atlas byte: printable ASCII renders directly,
/// and everything else (a control byte that reached here, or a non-Latin scalar
/// the 5×7 atlas has no glyph for) renders `?` rather than being dropped.
fn atlas_byte(ch: char) -> u8 {
    u8::try_from(ch as u32)
        .ok()
        .filter(|b| (0x20..=0x7E).contains(b))
        .unwrap_or(b'?')
}

/// The terminal screen state rendered directly onto the scan-out surface: the
/// cursor, the current rendition pen, a DEC scroll region, and the saved
/// cursor. It holds no cell grid — the pixels are the state — so every write
/// paints (or scrolls) the borrowed surface immediately. Pure CPU pixel
/// arithmetic over a borrowed slice, so it is host-testable.
///
/// The rendered attributes are colour (16/256/truecolour), bold (brightens the
/// base colours) and reverse-video (swaps foreground and background);
/// underline/italic/blink/dim/strike are parsed and folded into the pen but the
/// 5×7 bitmap atlas does not draw them. The alternate screen has no separate
/// buffer, so entering or leaving it clears the surface (a documented degrade —
/// a boot console keeps no scrollback). No hardware cursor is drawn.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub struct Screen {
    geometry: Geometry,
    column: u32,
    row: u32,
    pen: Attributes,
    region_top: u32,
    region_bottom: u32,
    saved: Option<(u32, u32, Attributes)>,
}

impl Screen {
    /// A screen homed at the top-left of a `geometry`-sized surface with the
    /// plain pen and a full-height scroll region.
    #[must_use]
    pub const fn new(geometry: Geometry) -> Self {
        let bottom = geometry.rows() - 1;
        Self {
            geometry,
            column: 0,
            row: 0,
            pen: Attributes::PLAIN,
            region_top: 0,
            region_bottom: bottom,
            saved: None,
        }
    }

    /// The validated geometry this screen renders into.
    #[must_use]
    pub const fn geometry(&self) -> &Geometry {
        &self.geometry
    }

    /// Text columns the surface holds.
    fn cols(&self) -> u32 {
        self.geometry.columns()
    }

    /// Text rows the surface holds.
    fn rows(&self) -> u32 {
        self.geometry.rows()
    }

    /// Apply one parsed [`Op`] to the surface, returning the pixel-row band it
    /// touched (or `None` for a pure cursor/state change).
    pub fn apply(&mut self, pixels: &mut [u32], op: &Op) -> Option<DirtyBand> {
        match *op {
            Op::Print(ch) => Some(self.print(pixels, ch)),
            Op::Backspace => {
                self.column = self.column.saturating_sub(1);
                None
            }
            Op::Tab => {
                let next = (self.column / 8 + 1) * 8;
                self.column = next.min(self.cols().saturating_sub(1));
                None
            }
            Op::LineFeed => self.line_feed(pixels),
            Op::CarriageReturn => {
                self.column = 0;
                None
            }
            Op::CursorUp(n) => {
                self.row = self.row.saturating_sub(u32::from(n));
                None
            }
            Op::CursorDown(n) => {
                self.row = (self.row + u32::from(n)).min(self.rows().saturating_sub(1));
                None
            }
            Op::CursorForward(n) => {
                self.column = (self.column + u32::from(n)).min(self.cols().saturating_sub(1));
                None
            }
            Op::CursorBack(n) => {
                self.column = self.column.saturating_sub(u32::from(n));
                None
            }
            Op::CursorNextLine(n) => {
                self.row = (self.row + u32::from(n)).min(self.rows().saturating_sub(1));
                self.column = 0;
                None
            }
            Op::CursorPrevLine(n) => {
                self.row = self.row.saturating_sub(u32::from(n));
                self.column = 0;
                None
            }
            Op::CursorColumn(col) => {
                self.column = u32::from(col - 1).min(self.cols().saturating_sub(1));
                None
            }
            Op::CursorPosition { row, col } => {
                self.row = u32::from(row - 1).min(self.rows().saturating_sub(1));
                self.column = u32::from(col - 1).min(self.cols().saturating_sub(1));
                None
            }
            Op::EraseInDisplay(mode) => Some(self.erase_display(pixels, mode)),
            Op::EraseInLine(mode) => Some(self.erase_line(pixels, mode)),
            Op::ScrollUp(n) => Some(self.scroll_region_up(pixels, u32::from(n))),
            Op::ScrollDown(n) => Some(self.scroll_region_down(pixels, u32::from(n))),
            Op::SetScrollRegion { top, bottom } => {
                self.set_scroll_region(u32::from(top - 1), u32::from(bottom - 1));
                None
            }
            Op::ResetScrollRegion => {
                self.region_top = 0;
                self.region_bottom = self.rows().saturating_sub(1);
                None
            }
            Op::Sgr(sgr) => {
                self.pen.apply(sgr);
                None
            }
            Op::SaveCursor => {
                self.saved = Some((self.column, self.row, self.pen));
                None
            }
            Op::RestoreCursor => {
                if let Some((col, row, pen)) = self.saved {
                    self.column = col.min(self.cols().saturating_sub(1));
                    self.row = row.min(self.rows().saturating_sub(1));
                    self.pen = pen;
                }
                None
            }
            // No separate alternate-screen buffer: switching either way clears
            // the surface rather than restoring hidden content.
            Op::EnterAltScreen | Op::LeaveAltScreen => self.clear(pixels),
            // Parsed but with no rendered effect on this bitmap console: cursor
            // visibility (no drawn cursor), the bell, and the input-reporting
            // operations that flow program-ward (keys, mouse, paste markers,
            // mode toggles).
            _ => None,
        }
    }

    /// The (foreground, background) scan-out pixels for the current pen, with
    /// bold brightening applied and reverse-video swapping the two.
    fn colors(&self) -> (u32, u32) {
        let fg = foreground_pixel(self.pen.foreground, self.pen.bold);
        let bg = background_pixel(self.pen.background);
        if self.pen.reverse {
            (bg, fg)
        } else {
            (fg, bg)
        }
    }

    /// Print one character at the cursor with the current pen, advancing the
    /// cursor and wrapping (scrolling at the bottom margin) at the right edge.
    fn print(&mut self, pixels: &mut [u32], ch: char) -> DirtyBand {
        let mut dirty = None;
        if self.column >= self.cols() {
            self.column = 0;
            dirty = self.line_feed(pixels);
        }
        let (fg, bg) = self.colors();
        let glyph_band = self.blit_glyph(pixels, atlas_byte(ch), fg, bg);
        self.column += 1;
        merge_bands(dirty, Some(glyph_band)).unwrap_or(glyph_band)
    }

    /// Advance one line, scrolling the region up when at its bottom margin.
    fn line_feed(&mut self, pixels: &mut [u32]) -> Option<DirtyBand> {
        if self.row == self.region_bottom {
            Some(self.scroll_region_up(pixels, 1))
        } else if self.row + 1 < self.rows() {
            self.row += 1;
            None
        } else {
            None
        }
    }

    /// Set the 1-based-decoded scroll region to `top..=bottom` (0-based),
    /// ignoring a degenerate or out-of-range request, and home the cursor.
    fn set_scroll_region(&mut self, top: u32, bottom: u32) {
        let last = self.rows().saturating_sub(1);
        let bottom = bottom.min(last);
        if top < bottom {
            self.region_top = top;
            self.region_bottom = bottom;
            self.column = 0;
            self.row = top;
        }
    }

    /// Clear the whole surface to the default background and home the cursor.
    pub fn clear(&mut self, pixels: &mut [u32]) -> Option<DirtyBand> {
        for pixel in pixels.iter_mut() {
            *pixel = DEFAULT_BACKGROUND;
        }
        self.column = 0;
        self.row = 0;
        Some((0, self.geometry.height_px))
    }

    /// Erase part of the display relative to the cursor, filling with the
    /// current background, and return the affected pixel-row band.
    fn erase_display(&self, pixels: &mut [u32], mode: EraseMode) -> DirtyBand {
        let (_, bg) = self.colors();
        let cell_h = self.geometry.cell_height_px();
        match mode {
            EraseMode::ToEnd => {
                self.fill_cells(pixels, self.column, self.cols(), self.row, bg);
                let below_y = (self.row + 1) * cell_h;
                self.fill_rows(pixels, below_y, self.geometry.height_px, bg);
                (self.row * cell_h, self.geometry.height_px)
            }
            EraseMode::ToStart => {
                self.fill_rows(pixels, 0, self.row * cell_h, bg);
                self.fill_cells(pixels, 0, self.column + 1, self.row, bg);
                (0, (self.row + 1) * cell_h)
            }
            EraseMode::All => {
                self.fill_rows(pixels, 0, self.geometry.height_px, bg);
                (0, self.geometry.height_px)
            }
        }
    }

    /// Erase part of the current line relative to the cursor, filling with the
    /// current background, and return the affected pixel-row band.
    fn erase_line(&self, pixels: &mut [u32], mode: EraseMode) -> DirtyBand {
        let (_, bg) = self.colors();
        let (start, end) = match mode {
            EraseMode::ToEnd => (self.column, self.cols()),
            EraseMode::ToStart => (0, self.column + 1),
            EraseMode::All => (0, self.cols()),
        };
        self.fill_cells(pixels, start, end, self.row, bg);
        let y0 = self.row * self.geometry.cell_height_px();
        (y0, y0 + self.geometry.cell_height_px())
    }

    /// Scroll the scroll region up by `n` text rows, clearing the vacated
    /// bottom rows to the current background.
    fn scroll_region_up(&self, pixels: &mut [u32], n: u32) -> DirtyBand {
        let (_, bg) = self.colors();
        let cell_h = self.geometry.cell_height_px();
        let stride = self.geometry.stride_px as usize;
        let region_rows = self.region_bottom - self.region_top + 1;
        let top_y = self.region_top * cell_h;
        let end_y = (self.region_bottom + 1) * cell_h;
        let n = n.min(region_rows);
        let shift = n * cell_h;
        if n < region_rows {
            let src = (top_y + shift) as usize * stride;
            let dst = top_y as usize * stride;
            let src_end = end_y as usize * stride;
            if src <= src_end && src_end <= pixels.len() {
                pixels.copy_within(src..src_end, dst);
            }
        }
        self.fill_rows(pixels, end_y - shift, end_y, bg);
        (top_y, end_y)
    }

    /// Scroll the scroll region down by `n` text rows, clearing the vacated top
    /// rows to the current background.
    fn scroll_region_down(&self, pixels: &mut [u32], n: u32) -> DirtyBand {
        let (_, bg) = self.colors();
        let cell_h = self.geometry.cell_height_px();
        let stride = self.geometry.stride_px as usize;
        let region_rows = self.region_bottom - self.region_top + 1;
        let top_y = self.region_top * cell_h;
        let end_y = (self.region_bottom + 1) * cell_h;
        let n = n.min(region_rows);
        let shift = n * cell_h;
        if n < region_rows {
            let src = top_y as usize * stride;
            let src_end = (end_y - shift) as usize * stride;
            let dst = (top_y + shift) as usize * stride;
            if src_end <= pixels.len() && dst + (src_end - src) <= pixels.len() {
                pixels.copy_within(src..src_end, dst);
            }
        }
        self.fill_rows(pixels, top_y, top_y + shift, bg);
        (top_y, end_y)
    }

    /// Fill whole scanlines `[y0, y1)` (visible width) with `color`.
    fn fill_rows(&self, pixels: &mut [u32], y0: u32, y1: u32, color: u32) {
        let stride = self.geometry.stride_px as usize;
        let width = self.geometry.width_px as usize;
        for y in y0..y1 {
            let start = y as usize * stride;
            if let Some(span) = pixels.get_mut(start..start + width) {
                span.fill(color);
            }
        }
    }

    /// Fill the cell columns `[from_col, to_col)` of text `row` with `color`.
    fn fill_cells(&self, pixels: &mut [u32], from_col: u32, to_col: u32, row: u32, color: u32) {
        let cell_w = self.geometry.cell_width_px();
        let cell_h = self.geometry.cell_height_px();
        let stride = self.geometry.stride_px as usize;
        let to_col = to_col.min(self.cols());
        if from_col >= to_col {
            return;
        }
        let x0 = (from_col * cell_w) as usize;
        let w = ((to_col - from_col) * cell_w) as usize;
        let y0 = row * cell_h;
        for y in y0..y0 + cell_h {
            let start = y as usize * stride + x0;
            if let Some(span) = pixels.get_mut(start..start + w) {
                span.fill(color);
            }
        }
    }

    /// Blit one atlas glyph at the cursor, lit pixels in `fg` and the rest
    /// (including inter-cell padding) in `bg`.
    fn blit_glyph(&self, pixels: &mut [u32], byte: u8, fg: u32, bg: u32) -> DirtyBand {
        let geometry = &self.geometry;
        let glyph = &glyphs::GLYPHS[(byte - glyphs::FIRST_CHAR as u8) as usize];
        let x0 = self.column * geometry.cell_width_px();
        let y0 = self.row * geometry.cell_height_px();
        for cell_y in 0..CELL_HEIGHT {
            let bits = if cell_y < glyphs::GLYPH_HEIGHT {
                glyph[cell_y as usize]
            } else {
                0
            };
            for cell_x in 0..CELL_WIDTH {
                let lit = cell_x < glyphs::GLYPH_WIDTH
                    && bits & (1 << (glyphs::GLYPH_WIDTH - 1 - cell_x)) != 0;
                let colour = if lit { fg } else { bg };
                for sub_y in 0..geometry.scale {
                    let y = (y0 + cell_y * geometry.scale + sub_y) as usize;
                    let x = (x0 + cell_x * geometry.scale) as usize;
                    let start = y * geometry.stride_px as usize + x;
                    if let Some(span) = pixels.get_mut(start..start + geometry.scale as usize) {
                        span.fill(colour);
                    }
                }
            }
        }
        (y0, y0 + geometry.cell_height_px())
    }
}

/// The framebuffer text console: the shared `rustos_vt::Parser` feeding a
/// [`Screen`] that renders each parsed operation straight onto the scan-out
/// surface. The parser and the screen are separate fields so a write can borrow
/// the parser (to feed it) and the screen (to apply the ops it yields) at once.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct TextConsole {
    parser: Parser,
    screen: Screen,
}

impl TextConsole {
    /// A console at the top-left of a `geometry`-sized surface.
    #[must_use]
    pub const fn new(geometry: Geometry) -> Self {
        Self {
            parser: Parser::new(),
            screen: Screen::new(geometry),
        }
    }

    /// The validated geometry this console renders into.
    #[must_use]
    pub const fn geometry(&self) -> &Geometry {
        self.screen.geometry()
    }

    /// Clear the whole surface to the background and home the cursor, returning
    /// the dirty band (the full surface height).
    pub fn clear(&mut self, pixels: &mut [u32]) -> Option<DirtyBand> {
        self.screen.clear(pixels)
    }

    /// Interpret `bytes` as an ANSI/VT/xterm stream, rendering the result onto
    /// `pixels`, and return the union of the pixel-row bands it touched.
    pub fn write_bytes(&mut self, pixels: &mut [u32], bytes: &[u8]) -> Option<DirtyBand> {
        let Self { parser, screen } = self;
        let mut dirty = None;
        parser.feed(bytes, |op| {
            dirty = merge_bands(dirty, screen.apply(pixels, &op));
        });
        dirty
    }
}

#[cfg(test)]
mod tests;
