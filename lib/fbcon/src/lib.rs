//! Shared framebuffer text-console engine (`lib/fbcon`).
//!
//! An architecture-neutral ANSI/VT/xterm-256color terminal that renders the
//! shared `rustos_vt::Op` stream straight onto a borrowed 32-bit scan-out
//! surface with the shared `rustos_font` glyph atlas. Every arch port drives
//! its display console through this one definition; a port supplies only the
//! board-specific surface (discovered at runtime) and calls
//! [`TextConsole::write_bytes`].
//!
//! The engine keeps a character-cell grid (the visible screen, one
//! [`Cell`] per position) so it can honour the alternate-screen buffer a
//! full-screen program uses: entering the alternate screen (`CSI ? 1049 h`)
//! preserves the main screen in its own grid and shows a cleared one;
//! leaving it (`CSI ? 1049 l`) restores the saved main screen exactly, the
//! way every xterm-family terminal does. Reaching the bottom scrolls both the
//! grid and the pixels up (a real terminal scroll), not a ring wrap.
//!
//! The grid storage is **borrowed**, not owned: the caller passes two
//! `&mut [Cell]` buffers (main and alternate), so the crate itself never
//! allocates and a freestanding boot console with no global allocator links
//! it directly. An allocator-having caller leaks a heap buffer sized to the
//! discovered geometry; an allocator-free caller supplies a `static`.

#![no_std]
#![forbid(unsafe_code)]
#![deny(missing_docs)]

use rustos_font::atlas;
use rustos_font::glyph::lookup_or_fallback;
use rustos_vt::{char_width, Attributes, Color, EraseMode, Op, Parser, CONTINUATION};

pub use rustos_vt::Cell;

/// Glyph cell width in pixels at scale 1. The face's uniform advance already
/// carries the inter-character spacing, so the cell is the atlas cell.
const CELL_WIDTH: u32 = atlas::CELL_WIDTH;

/// Glyph cell height in pixels at scale 1. The face's ascent + descent
/// already carry the line box, so the cell is the atlas cell.
const CELL_HEIGHT: u32 = atlas::CELL_HEIGHT;

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
/// Beyond 4× the atlas looks blocky without gaining legibility, so the
/// policy caps there even on very tall displays.
const MAX_SCALE: u32 = 4;

/// Pixel rows of display height per unit of glyph scale.
///
/// `height / 1080` keeps roughly 41 text rows on screen from 1080p up
/// (1080p → 1×, 2160p → 2×) with the 26-pixel cell; smaller modes (480p → 18
/// rows, 720p → 27) simply hold fewer rows at 1× rather than shrinking the
/// glyphs below legibility.
const ROWS_PER_SCALE: u32 = 1080;

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

    /// Character-cell count (`columns × rows`), the length each grid buffer
    /// ([`TextConsole::new`]'s `main`/`alt`) must have.
    #[must_use]
    pub const fn cell_count(&self) -> usize {
        self.columns() as usize * self.rows() as usize
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

/// The (foreground, background) scan-out pixels a cell's `attrs` render with:
/// bold brightens a dim base foreground, and reverse-video swaps the two.
fn colors_of(attrs: &Attributes) -> (u32, u32) {
    let fg = foreground_pixel(attrs.foreground, attrs.bold);
    let bg = background_pixel(attrs.background);
    if attrs.reverse {
        (bg, fg)
    } else {
        (fg, bg)
    }
}

/// The scan-out pixel for each of the 16 glyph coverage levels: the linear
/// blend of `fg` over `bg` at `level / 15`, computed once per cell blit so
/// the per-pixel work is a table load.
fn coverage_ramp(fg: u32, bg: u32) -> [u32; 16] {
    let channel = |shift: u32, level: u32| -> u32 {
        let f = (fg >> shift) & 0xFF;
        let b = (bg >> shift) & 0xFF;
        // Rounded weighted average; exact at the endpoints (level 0 is `bg`,
        // level 15 is `fg`) and never overflows (≤ 255 × 15 + 7).
        ((b * (15 - level) + f * level + 7) / 15) << shift
    };
    let mut ramp = [0u32; 16];
    for (level, slot) in (0u32..).zip(ramp.iter_mut()) {
        *slot = 0xFF00_0000 | channel(16, level) | channel(8, level) | channel(0, level);
    }
    ramp
}

/// A saved cursor: position plus the rendition pen, captured by `ESC 7`
/// (DECSC) or on entering the alternate screen and restored by `ESC 8`
/// (DECRC) / on leaving it.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
struct SavedCursor {
    column: u32,
    row: u32,
    pen: Attributes,
}

/// The terminal screen: the cursor, the rendition pen, a DEC scroll region,
/// the saved cursor, and the **character-cell grid** of the visible screen.
///
/// The grid records what each screen position shows, so the engine can honour
/// the alternate-screen buffer: the `main` grid holds the primary screen and
/// the `alt` grid the alternate one. Every write updates the active grid *and*
/// paints (or scrolls) the borrowed pixel surface immediately, so the display
/// stays live without a separate flush; the grid exists so a screen can be
/// **repainted from its cells** — which is exactly how leaving the alternate
/// screen restores the primary one. The grid buffers are borrowed (`&mut
/// [Cell]`), so the engine never allocates.
///
/// The rendered attributes are colour (16/256/truecolour), bold (brightens the
/// base colours) and reverse-video (swaps foreground and background);
/// underline/italic/blink/dim/strike are parsed and folded into the pen but
/// the bitmap atlas carries no variant glyphs for them.
///
/// The cursor is drawn in software as a reverse-video block over the cell it
/// rests on — there is no hardware cursor on a dumb scan-out surface — and it
/// honours the DECTCEM show/hide operations (`CSI ? 25 h` / `l`) a full-screen
/// program uses. The overlay is painted after a batch of operations and
/// removed before the next batch, so it never contaminates the grid: the cell
/// under it repaints exactly from its recorded state.
#[derive(Debug)]
pub struct Screen<'a> {
    geometry: Geometry,
    column: u32,
    row: u32,
    pen: Attributes,
    region_top: u32,
    region_bottom: u32,
    saved: Option<SavedCursor>,
    /// Whether the terminal cursor is shown (DECTCEM, default on).
    cursor_visible: bool,
    /// The cell the cursor overlay is currently painted over, if any.
    overlay: Option<(u32, u32)>,
    /// The primary-screen cell grid (`columns × rows`, row-major).
    main: &'a mut [Cell],
    /// The alternate-screen cell grid (`columns × rows`, row-major).
    alt: &'a mut [Cell],
    /// Whether the alternate screen is currently shown.
    on_alt: bool,
    /// The primary-screen cursor saved on entering the alternate screen,
    /// restored on leaving it (the `CSI ? 1049` cursor save/restore).
    alt_saved: Option<SavedCursor>,
}

impl<'a> Screen<'a> {
    /// A screen homed at the top-left of a `geometry`-sized surface with the
    /// plain pen, a full-height scroll region, and the two borrowed cell grids
    /// (`main` and `alt`, each at least [`Geometry::cell_count`] long — a
    /// shorter buffer simply bounds what the grid can hold; indexing fails
    /// closed rather than panicking).
    #[must_use]
    pub fn new(geometry: Geometry, main: &'a mut [Cell], alt: &'a mut [Cell]) -> Self {
        let bottom = geometry.rows() - 1;
        Self {
            geometry,
            column: 0,
            row: 0,
            pen: Attributes::PLAIN,
            region_top: 0,
            region_bottom: bottom,
            saved: None,
            cursor_visible: true,
            overlay: None,
            main,
            alt,
            on_alt: false,
            alt_saved: None,
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

    /// The cell grid currently shown (the alternate grid while on the
    /// alternate screen, else the primary grid).
    fn active_cells(&mut self) -> &mut [Cell] {
        if self.on_alt {
            &mut *self.alt
        } else {
            &mut *self.main
        }
    }

    /// Write `cell` into the active grid at `(col, row)`, ignoring an
    /// out-of-range coordinate (fail closed, never a panic).
    fn grid_set(&mut self, col: u32, row: u32, cell: Cell) {
        let cols = self.cols();
        let index = row as usize * cols as usize + col as usize;
        if let Some(slot) = self.active_cells().get_mut(index) {
            *slot = cell;
        }
    }

    /// Fill columns `[from_col, to_col)` of active-grid `row` with `cell`.
    fn grid_fill_cells(&mut self, from_col: u32, to_col: u32, row: u32, cell: Cell) {
        let cols = self.cols();
        let to_col = to_col.min(cols);
        let base = row as usize * cols as usize;
        let cells = self.active_cells();
        for col in from_col..to_col {
            if let Some(slot) = cells.get_mut(base + col as usize) {
                *slot = cell;
            }
        }
    }

    /// Fill whole active-grid rows `[from_row, to_row)` with `cell`.
    fn grid_fill_rows(&mut self, from_row: u32, to_row: u32, cell: Cell) {
        let cols = self.cols();
        let start = from_row as usize * cols as usize;
        let end = to_row as usize * cols as usize;
        let cells = self.active_cells();
        if let Some(span) = cells.get_mut(start..end.min(cells.len())) {
            span.fill(cell);
        }
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
                self.saved = Some(SavedCursor {
                    column: self.column,
                    row: self.row,
                    pen: self.pen,
                });
                None
            }
            Op::RestoreCursor => {
                if let Some(saved) = self.saved {
                    self.column = saved.column.min(self.cols().saturating_sub(1));
                    self.row = saved.row.min(self.rows().saturating_sub(1));
                    self.pen = saved.pen;
                }
                None
            }
            Op::EnterAltScreen => self.enter_alt_screen(pixels),
            Op::LeaveAltScreen => self.leave_alt_screen(pixels),
            Op::ShowCursor => {
                self.cursor_visible = true;
                None
            }
            Op::HideCursor => {
                self.cursor_visible = false;
                None
            }
            // Parsed but with no rendered effect on this bitmap console: the
            // bell and the input-reporting operations that flow program-ward
            // (keys, meta chords, mouse, paste markers, mode toggles).
            _ => None,
        }
    }

    /// The recorded cell at `(col, row)` of the active grid, or a blank for
    /// an out-of-range coordinate (fail closed, never a panic).
    fn cell_at(&self, col: u32, row: u32) -> Cell {
        let cells = if self.on_alt { &*self.alt } else { &*self.main };
        let index = row as usize * self.cols() as usize + col as usize;
        cells.get(index).copied().unwrap_or(Cell::BLANK)
    }

    /// Remove the drawn cursor overlay, if any, by repainting the cell under
    /// it from the grid. Called before a batch of operations so the overlay
    /// never mixes into what the batch paints.
    fn undraw_cursor(&mut self, pixels: &mut [u32]) -> Option<DirtyBand> {
        let (col, row) = self.overlay.take()?;
        let cell = self.cell_at(col, row);
        Some(self.blit_cell(pixels, col, row, cell.ch, cell.attrs))
    }

    /// Paint the cursor overlay — the cell at the cursor in reverse video —
    /// when the cursor is visible. A cursor resting past the last column (the
    /// pending-wrap position after printing there) is shown clamped onto it,
    /// as hardware text cursors are.
    fn draw_cursor(&mut self, pixels: &mut [u32]) -> Option<DirtyBand> {
        if !self.cursor_visible {
            return None;
        }
        let col = self.column.min(self.cols().saturating_sub(1));
        let row = self.row.min(self.rows().saturating_sub(1));
        let cell = self.cell_at(col, row);
        let mut attrs = cell.attrs;
        attrs.reverse = !attrs.reverse;
        self.overlay = Some((col, row));
        Some(self.blit_cell(pixels, col, row, cell.ch, attrs))
    }

    /// The (foreground, background) scan-out pixels for the current pen, with
    /// bold brightening applied and reverse-video swapping the two.
    fn colors(&self) -> (u32, u32) {
        colors_of(&self.pen)
    }

    /// The cell an erase/scroll writes into vacated positions: a space in the
    /// current pen, so the erased region keeps the pen's background colour and
    /// a later repaint reproduces exactly what the immediate pixel fill drew.
    fn blank_cell(&self) -> Cell {
        Cell::styled(' ', self.pen)
    }

    /// Print one character at the cursor with the current pen, recording it in
    /// the active grid and painting it, advancing the cursor and wrapping
    /// (scrolling at the bottom margin) at the right edge.
    ///
    /// A double-width glyph (see [`char_width`]) occupies two cells: the lead
    /// cell and a [`CONTINUATION`] cell to its right, exactly the layout the
    /// `lib/curses` window writer produces, so a TUI's column arithmetic and
    /// this console agree. When only one column remains the wide glyph wraps
    /// whole, blanking the leftover column rather than splitting.
    fn print(&mut self, pixels: &mut [u32], ch: char) -> DirtyBand {
        let width = u32::from(char_width(ch));
        let mut dirty = None;
        if self.column + width > self.cols() {
            let (col, row) = (self.column, self.row);
            if col < self.cols() {
                // A wide glyph with one column left: blank the leftover cell.
                let blank = self.blank_cell();
                self.grid_set(col, row, blank);
                dirty = Some(self.blit_cell(pixels, col, row, ' ', blank.attrs));
            }
            self.column = 0;
            dirty = merge_bands(dirty, self.line_feed(pixels));
        }
        let (col, row, pen) = (self.column, self.row, self.pen);
        self.grid_set(col, row, Cell::styled(ch, pen));
        let mut band = self.blit_cell(pixels, col, row, ch, pen);
        // On a degenerate one-column surface there is no second cell; writing
        // it anyway would alias the next row (the grid is row-major).
        if width == 2 && col + 1 < self.cols() {
            self.grid_set(col + 1, row, Cell::styled(CONTINUATION, pen));
            band = merge_bands(
                Some(band),
                Some(self.blit_cell(pixels, col + 1, row, CONTINUATION, pen)),
            )
            .unwrap_or(band);
        }
        self.column += width;
        merge_bands(dirty, Some(band)).unwrap_or(band)
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

    /// Clear the whole surface to the default background, blank the active
    /// grid, and home the cursor. The wipe removes any drawn cursor overlay
    /// with the rest of the pixels.
    pub fn clear(&mut self, pixels: &mut [u32]) -> Option<DirtyBand> {
        for pixel in pixels.iter_mut() {
            *pixel = DEFAULT_BACKGROUND;
        }
        self.active_cells().fill(Cell::BLANK);
        self.column = 0;
        self.row = 0;
        self.overlay = None;
        Some((0, self.geometry.height_px))
    }

    /// Enter the alternate screen (`CSI ? 1049 h`): save the primary-screen
    /// cursor, switch to the alternate grid, and show it cleared. A second
    /// request while already on the alternate screen is a no-op.
    fn enter_alt_screen(&mut self, pixels: &mut [u32]) -> Option<DirtyBand> {
        if self.on_alt {
            return None;
        }
        self.alt_saved = Some(SavedCursor {
            column: self.column,
            row: self.row,
            pen: self.pen,
        });
        self.on_alt = true;
        self.clear(pixels)
    }

    /// Leave the alternate screen (`CSI ? 1049 l`): switch back to the primary
    /// grid, restore its saved cursor, and repaint it from its cells so the
    /// screen the program covered returns exactly. A request while not on the
    /// alternate screen is a no-op.
    fn leave_alt_screen(&mut self, pixels: &mut [u32]) -> Option<DirtyBand> {
        if !self.on_alt {
            return None;
        }
        self.on_alt = false;
        if let Some(saved) = self.alt_saved.take() {
            self.column = saved.column.min(self.cols().saturating_sub(1));
            self.row = saved.row.min(self.rows().saturating_sub(1));
            self.pen = saved.pen;
        }
        Some(self.repaint(pixels))
    }

    /// Repaint the whole visible surface from the active grid's cells (used to
    /// restore the primary screen on leaving the alternate one).
    fn repaint(&self, pixels: &mut [u32]) -> DirtyBand {
        for pixel in pixels.iter_mut() {
            *pixel = DEFAULT_BACKGROUND;
        }
        let cols = self.cols();
        let cells = if self.on_alt { &*self.alt } else { &*self.main };
        for row in 0..self.rows() {
            for col in 0..cols {
                if let Some(cell) = cells.get((row * cols + col) as usize) {
                    self.blit_cell(pixels, col, row, cell.ch, cell.attrs);
                }
            }
        }
        (0, self.geometry.height_px)
    }

    /// Erase part of the display relative to the cursor, filling both the grid
    /// and the pixels with the current background, and return the affected
    /// pixel-row band.
    fn erase_display(&mut self, pixels: &mut [u32], mode: EraseMode) -> DirtyBand {
        let (_, bg) = self.colors();
        let blank = self.blank_cell();
        let cell_h = self.geometry.cell_height_px();
        let (col, row, cols, rows) = (self.column, self.row, self.cols(), self.rows());
        match mode {
            EraseMode::ToEnd => {
                self.grid_fill_cells(col, cols, row, blank);
                self.grid_fill_rows(row + 1, rows, blank);
                self.fill_cells(pixels, col, cols, row, bg);
                let below_y = (row + 1) * cell_h;
                self.fill_rows(pixels, below_y, self.geometry.height_px, bg);
                (row * cell_h, self.geometry.height_px)
            }
            EraseMode::ToStart => {
                self.grid_fill_rows(0, row, blank);
                self.grid_fill_cells(0, col + 1, row, blank);
                self.fill_rows(pixels, 0, row * cell_h, bg);
                self.fill_cells(pixels, 0, col + 1, row, bg);
                (0, (row + 1) * cell_h)
            }
            EraseMode::All => {
                self.grid_fill_rows(0, rows, blank);
                self.fill_rows(pixels, 0, self.geometry.height_px, bg);
                (0, self.geometry.height_px)
            }
        }
    }

    /// Erase part of the current line relative to the cursor, filling both the
    /// grid and the pixels with the current background, and return the affected
    /// pixel-row band.
    fn erase_line(&mut self, pixels: &mut [u32], mode: EraseMode) -> DirtyBand {
        let (_, bg) = self.colors();
        let blank = self.blank_cell();
        let (col, row) = (self.column, self.row);
        let (start, end) = match mode {
            EraseMode::ToEnd => (col, self.cols()),
            EraseMode::ToStart => (0, col + 1),
            EraseMode::All => (0, self.cols()),
        };
        self.grid_fill_cells(start, end, row, blank);
        self.fill_cells(pixels, start, end, row, bg);
        let y0 = row * self.geometry.cell_height_px();
        (y0, y0 + self.geometry.cell_height_px())
    }

    /// Scroll the scroll region up by `n` text rows in both the grid and the
    /// pixels, clearing the vacated bottom rows to the current background.
    fn scroll_region_up(&mut self, pixels: &mut [u32], n: u32) -> DirtyBand {
        let region_rows = self.region_bottom - self.region_top + 1;
        let n = n.min(region_rows);
        let blank = self.blank_cell();
        {
            let cols = self.cols() as usize;
            let top = self.region_top as usize * cols;
            let bottom = (self.region_bottom as usize + 1) * cols;
            let shift = n as usize * cols;
            let cells = self.active_cells();
            let bottom = bottom.min(cells.len());
            if let Some(region) = cells.get_mut(top..bottom) {
                let len = region.len();
                if shift < len {
                    region.copy_within(shift.., 0);
                    region[len - shift..].fill(blank);
                } else {
                    region.fill(blank);
                }
            }
        }
        let (_, bg) = self.colors();
        let cell_h = self.geometry.cell_height_px();
        let stride = self.geometry.stride_px as usize;
        let top_y = self.region_top * cell_h;
        let end_y = (self.region_bottom + 1) * cell_h;
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

    /// Scroll the scroll region down by `n` text rows in both the grid and the
    /// pixels, clearing the vacated top rows to the current background.
    fn scroll_region_down(&mut self, pixels: &mut [u32], n: u32) -> DirtyBand {
        let region_rows = self.region_bottom - self.region_top + 1;
        let n = n.min(region_rows);
        let blank = self.blank_cell();
        {
            let cols = self.cols() as usize;
            let top = self.region_top as usize * cols;
            let bottom = (self.region_bottom as usize + 1) * cols;
            let shift = n as usize * cols;
            let cells = self.active_cells();
            let bottom = bottom.min(cells.len());
            if let Some(region) = cells.get_mut(top..bottom) {
                let len = region.len();
                if shift < len {
                    region.copy_within(..len - shift, shift);
                    region[..shift].fill(blank);
                } else {
                    region.fill(blank);
                }
            }
        }
        let (_, bg) = self.colors();
        let cell_h = self.geometry.cell_height_px();
        let stride = self.geometry.stride_px as usize;
        let top_y = self.region_top * cell_h;
        let end_y = (self.region_bottom + 1) * cell_h;
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

    /// Blit the glyph for `ch` at cell `(col, row)` in the rendition `attrs`:
    /// each pixel is the [`coverage_ramp`] blend of the foreground over the
    /// background at the glyph's anti-aliased coverage, with bold/reverse
    /// resolved by [`colors_of`]. A scalar the face does not cover blits the
    /// U+FFFD replacement glyph; a wide glyph's [`CONTINUATION`] cell blits as
    /// background (the lead glyph already covers it visually).
    fn blit_cell(
        &self,
        pixels: &mut [u32],
        col: u32,
        row: u32,
        ch: char,
        attrs: Attributes,
    ) -> DirtyBand {
        let (fg, bg) = colors_of(&attrs);
        let ramp = coverage_ramp(fg, bg);
        let shown = if ch == CONTINUATION { ' ' } else { ch };
        let glyph = lookup_or_fallback(shown);
        let geometry = &self.geometry;
        let x0 = col * geometry.cell_width_px();
        let y0 = row * geometry.cell_height_px();
        for cell_y in 0..CELL_HEIGHT {
            for cell_x in 0..CELL_WIDTH {
                let colour = ramp[usize::from(glyph.coverage(cell_x, cell_y))];
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
///
/// The two cell grids the screen needs are borrowed (`main`/`alt`), so the
/// console carries the borrow lifetime and never allocates.
#[derive(Debug)]
pub struct TextConsole<'a> {
    parser: Parser,
    screen: Screen<'a>,
}

impl<'a> TextConsole<'a> {
    /// A console at the top-left of a `geometry`-sized surface, backed by the
    /// borrowed `main` and `alt` cell grids (each at least
    /// [`Geometry::cell_count`] long).
    #[must_use]
    pub fn new(geometry: Geometry, main: &'a mut [Cell], alt: &'a mut [Cell]) -> Self {
        Self {
            parser: Parser::new(),
            screen: Screen::new(geometry, main, alt),
        }
    }

    /// The validated geometry this console renders into.
    #[must_use]
    pub fn geometry(&self) -> &Geometry {
        self.screen.geometry()
    }

    /// Clear the whole surface to the background and home the cursor, returning
    /// the dirty band (the full surface height). The cursor overlay is redrawn
    /// at the home position.
    pub fn clear(&mut self, pixels: &mut [u32]) -> Option<DirtyBand> {
        let dirty = self.screen.clear(pixels);
        merge_bands(dirty, self.screen.draw_cursor(pixels))
    }

    /// Interpret `bytes` as an ANSI/VT/xterm stream, rendering the result onto
    /// `pixels`, and return the union of the pixel-row bands it touched.
    ///
    /// The cursor overlay is lifted before the stream is applied and repainted
    /// at the (possibly new) cursor position afterwards, so the console shows
    /// a live cursor without the overlay ever mixing into the cell grid.
    pub fn write_bytes(&mut self, pixels: &mut [u32], bytes: &[u8]) -> Option<DirtyBand> {
        let Self { parser, screen } = self;
        let mut dirty = screen.undraw_cursor(pixels);
        parser.feed(bytes, |op| {
            dirty = merge_bands(dirty, screen.apply(pixels, &op));
        });
        merge_bands(dirty, screen.draw_cursor(pixels))
    }
}

#[cfg(test)]
mod tests;
