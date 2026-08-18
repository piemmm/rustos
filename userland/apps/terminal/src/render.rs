//! Painting the terminal's screen into a pixel [`Surface`].
//!
//! [`Screen`] owns the window-sized premultiplied-alpha surface the grid is
//! drawn into and **keeps it between frames**. Each
//! [`paint`](Screen::paint) compares the [`Grid`] against the cells the
//! surface was last painted from, redraws only the block that differs, and
//! returns that block as a surface rectangle — the damage the app presents.
//! A keystroke therefore costs the cell it wrote and the two the cursor
//! moved between, not a whole-window render, copy, and recomposite.
//!
//! Each [`Cell`] is drawn with its own rendition: the
//! [`Attributes`](tairix_vt::Attributes) the shared `lib/vt` parser folded
//! onto it choose the foreground and background, which are resolved through
//! the profile's [`Painted`] colours — the one place a terminal colour comes
//! from. The surface is the window manager's to place and round: the terminal
//! paints a *rectangular* buffer and the compositor applies any corner radius
//! through its single anti-aliased rounded-corner path.
//!
//! # What the diff is allowed to assume
//!
//! Two equal cells paint identically **only** under the same colours and the
//! same face, so a caller that changes either must
//! [`invalidate`](Screen::invalidate) first; a resize does it implicitly.
//! Nothing else can make a retained pixel stale: the cursor block is tracked
//! alongside the cells, and a cell is compared whole (glyph *and* rendition).
//!
//! The monospace face carries no separate bold/italic/underline glyphs, so
//! those parsed attributes do not change the rendered shape — a renderer
//! limitation, not a parsing gap. A wide glyph's continuation cell paints
//! background only; the lead glyph covers it, so a damaged block is widened
//! to whole glyphs before it is drawn.
//!
//! Every length saturates and every blit clips, so a viewport smaller than
//! the grid paints what fits rather than panicking.
//!
//! Translucency is not a separate step: the default background is filled at
//! the alpha the profile asks for, so the compositor's own premultiplied
//! blend shows what is behind the window while a glyph drawn over it stays
//! opaque. The screen effects ([`crate::effects`]) are a post-process over a
//! copy of the finished picture, so they never accumulate into the retained
//! surface.

use alloc::vec::Vec;

use tairix_font::BitmapFont;
use tairix_geometry::Rect;
use tairix_raster::{Color, Surface};
use tairix_vt::{char_width, Cell, CONTINUATION};

use crate::grid::Grid;
use crate::scheme::Painted;

/// The terminal's window picture, retained between frames, and the cells it
/// was last painted from.
#[derive(Debug)]
pub struct Screen {
    /// The painted picture, without any screen effect applied.
    surface: Surface,
    /// The cells as painted, row-major and `cols * rows` long.
    painted: Vec<Cell>,
    /// The grid width `painted` describes.
    cols: u16,
    /// The grid height `painted` describes.
    rows: u16,
    /// The cell the cursor block was painted in, or `None` if it was hidden.
    cursor: Option<(u16, u16)>,
    /// Whether the surface holds pixels no snapshot describes, which is what
    /// makes the next paint cover the window. True before the first paint and
    /// after anything the diff cannot see: new colours, a new face, a
    /// reshape, or a session redraw request.
    stale: bool,
}

impl Screen {
    /// A `width_px` × `height_px` screen with nothing painted yet, so the
    /// first [`paint`](Self::paint) draws the whole window.
    ///
    /// Returns `None` only when those dimensions cannot be allocated (a
    /// surface that could never exist), so the caller fails closed rather
    /// than panicking.
    #[must_use]
    pub fn new(width_px: u32, height_px: u32) -> Option<Self> {
        Some(Self {
            surface: Surface::new(width_px, height_px)?,
            painted: Vec::new(),
            cols: 0,
            rows: 0,
            cursor: None,
            stale: true,
        })
    }

    /// The painted picture.
    #[must_use]
    pub const fn surface(&self) -> &Surface {
        &self.surface
    }

    /// Reshape to `width_px` × `height_px`, discarding what was painted.
    ///
    /// Returns `false`, leaving the current surface intact, when the new one
    /// cannot be allocated: the caller keeps the window it has rather than
    /// losing it.
    #[must_use]
    pub fn resize(&mut self, width_px: u32, height_px: u32) -> bool {
        let Some(surface) = Surface::new(width_px, height_px) else {
            return false;
        };
        self.surface = surface;
        self.invalidate();
        true
    }

    /// Forget what is painted, so the next [`paint`](Self::paint) draws the
    /// whole window. Required whenever the colours or the face change, which
    /// the retained pixels cannot detect for themselves.
    pub fn invalidate(&mut self) {
        self.stale = true;
    }

    /// Bring the surface up to date with `grid` in `painted`'s colours, and
    /// return the surface rectangle that changed ([`Rect::EMPTY`] when
    /// nothing did).
    #[must_use]
    pub fn paint(&mut self, grid: &Grid, painted: &Painted, font: BitmapFont) -> Rect {
        let metrics = CellMetrics::of(font);
        let cursor = grid
            .cursor_visible()
            .then(|| (grid.cursor_col(), grid.cursor_row()));
        let whole = self.stale || self.cols != grid.cols() || self.rows != grid.rows();
        let block = if whole {
            CellBlock::covering(grid.cols(), grid.rows())
        } else {
            self.changed(grid, cursor)
        };
        if !whole && block.is_none() {
            return Rect::EMPTY;
        }

        if whole {
            self.surface.fill(painted.background());
            self.reshape(grid);
        }
        let mut damage = Rect::EMPTY;
        if let Some(block) = block.map(|block| block.widened_to_glyphs(grid)) {
            damage = metrics.pixels(block).intersection(&self.bounds());
            if !whole {
                if let Some((x, y, w, h)) = extent(damage) {
                    self.surface.fill_rect(x, y, w, h, painted.background());
                }
            }
            draw_cells(&mut self.surface, grid, painted, metrics, block);
            if let Some((col, row)) = cursor {
                if block.holds(col, row) {
                    draw_cursor(&mut self.surface, grid, painted, metrics, col, row);
                }
            }
            self.record(grid, block);
        }
        self.cursor = cursor;
        self.stale = false;
        if whole {
            self.bounds()
        } else {
            damage
        }
    }

    /// The whole surface as a rectangle.
    fn bounds(&self) -> Rect {
        Rect::new(0, 0, self.surface.width(), self.surface.height())
    }

    /// The block of cells whose painted pixels `grid` no longer agrees with:
    /// every cell whose glyph or rendition differs, plus the cell the cursor
    /// block left and the one it moved to.
    fn changed(&self, grid: &Grid, cursor: Option<(u16, u16)>) -> Option<CellBlock> {
        let mut block: Option<CellBlock> = None;
        for row in 0..self.rows {
            let base = usize::from(row) * usize::from(self.cols);
            for col in 0..self.cols {
                if self.painted.get(base + usize::from(col)).copied() != grid.cell(col, row) {
                    CellBlock::grow(&mut block, col, row);
                }
            }
        }
        if cursor != self.cursor {
            for (col, row) in self.cursor.into_iter().chain(cursor) {
                CellBlock::grow(&mut block, col, row);
            }
        }
        block
    }

    /// Resize the snapshot to `grid`'s shape, discarding what it held: the
    /// caller has just repainted the whole surface.
    fn reshape(&mut self, grid: &Grid) {
        self.painted.clear();
        self.painted
            .resize(cell_count(grid.cols(), grid.rows()), Cell::BLANK);
        self.cols = grid.cols();
        self.rows = grid.rows();
    }

    /// Record `block`'s cells as what the surface now shows.
    ///
    /// Only the block is written, which is sound because a cell outside it
    /// did not change: the block is the bounding box of every difference the
    /// diff found, widened only outwards.
    fn record(&mut self, grid: &Grid, block: CellBlock) {
        for row in block.top..=block.bottom {
            for col in block.left..=block.right {
                let index = usize::from(row) * usize::from(self.cols) + usize::from(col);
                if let Some(slot) = self.painted.get_mut(index) {
                    *slot = grid.cell(col, row).unwrap_or(Cell::BLANK);
                }
            }
        }
    }
}

/// The cell count a `cols` × `rows` grid holds.
fn cell_count(cols: u16, rows: u16) -> usize {
    usize::from(cols) * usize::from(rows)
}

/// The pixel extent of `rect` as the unsigned quadruple the surface fills
/// take, or `None` when the rectangle is empty or lies off the top-left.
fn extent(rect: Rect) -> Option<(u32, u32, u32, u32)> {
    if rect.is_empty() {
        return None;
    }
    Some((
        u32::try_from(rect.left()).ok()?,
        u32::try_from(rect.top()).ok()?,
        rect.width,
        rect.height,
    ))
}

/// The face's cell extent, read once per paint rather than once per cell:
/// both are service-backed queries.
#[derive(Copy, Clone, Debug)]
struct CellMetrics {
    /// The face this describes, carried so the drawing calls need only this.
    font: BitmapFont,
    /// Cell advance in pixels, never zero.
    width: u32,
    /// Row pitch in pixels, never zero.
    height: u32,
}

impl CellMetrics {
    /// The extent `font` lays cells out on.
    fn of(font: BitmapFont) -> Self {
        Self {
            font,
            width: font.cell_width().max(1),
            height: font.line_height().max(1),
        }
    }

    /// The surface rectangle `block` occupies.
    fn pixels(self, block: CellBlock) -> Rect {
        let x = self.width.saturating_mul(u32::from(block.left));
        let y = self.height.saturating_mul(u32::from(block.top));
        let w = self
            .width
            .saturating_mul(u32::from(block.right - block.left) + 1);
        let h = self
            .height
            .saturating_mul(u32::from(block.bottom - block.top) + 1);
        Rect::new(to_i32(x), to_i32(y), w, h)
    }

    /// The surface origin of cell `(col, row)`.
    fn origin(self, col: u16, row: u16) -> (u32, u32) {
        (
            self.width.saturating_mul(u32::from(col)),
            self.height.saturating_mul(u32::from(row)),
        )
    }
}

/// An inclusive rectangle of cells, in grid coordinates.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
struct CellBlock {
    /// Leftmost column, inclusive.
    left: u16,
    /// Topmost row, inclusive.
    top: u16,
    /// Rightmost column, inclusive.
    right: u16,
    /// Bottommost row, inclusive.
    bottom: u16,
}

impl CellBlock {
    /// The whole of a `cols` × `rows` grid, or `None` when it has no cells.
    fn covering(cols: u16, rows: u16) -> Option<Self> {
        if cols == 0 || rows == 0 {
            return None;
        }
        Some(Self {
            left: 0,
            top: 0,
            right: cols - 1,
            bottom: rows - 1,
        })
    }

    /// Grow `block` to hold cell `(col, row)`, starting it if it is empty.
    fn grow(block: &mut Option<Self>, col: u16, row: u16) {
        *block = Some(match *block {
            None => Self {
                left: col,
                top: row,
                right: col,
                bottom: row,
            },
            Some(held) => Self {
                left: held.left.min(col),
                top: held.top.min(row),
                right: held.right.max(col),
                bottom: held.bottom.max(row),
            },
        });
    }

    /// Whether `(col, row)` lies in the block.
    fn holds(self, col: u16, row: u16) -> bool {
        (self.left..=self.right).contains(&col) && (self.top..=self.bottom).contains(&row)
    }

    /// The block grown so no wide glyph is drawn from the middle: a left edge
    /// on a continuation cell takes in the lead cell beside it, and a right
    /// edge on a lead cell takes in its continuation.
    ///
    /// One column each side is always enough — a continuation sits directly
    /// right of its lead — but both edges are tested on every row the block
    /// covers, because a column that continues a wide glyph on one row may
    /// start a narrow one on the next.
    fn widened_to_glyphs(self, grid: &Grid) -> Self {
        let last = grid.cols().saturating_sub(1);
        let mut widened = self;
        for row in self.top..=self.bottom {
            if grid
                .cell(self.left, row)
                .is_some_and(|cell| cell.ch == CONTINUATION)
            {
                widened.left = widened.left.min(self.left.saturating_sub(1));
            }
            if grid
                .cell(self.right, row)
                .is_some_and(|cell| char_width(cell.ch) == 2)
            {
                widened.right = widened.right.max(self.right.saturating_add(1).min(last));
            }
        }
        widened
    }
}

/// Draw every cell of `block` with its own rendition, left to right, over a
/// background the caller has already laid.
fn draw_cells(
    surface: &mut Surface,
    grid: &Grid,
    painted: &Painted,
    metrics: CellMetrics,
    block: CellBlock,
) {
    let base = painted.background();
    for row in block.top..=block.bottom {
        let mut col = block.left;
        while col <= block.right {
            let Some(cell) = grid.cell(col, row) else {
                col += 1;
                continue;
            };
            if cell.ch == CONTINUATION {
                col += 1;
                continue;
            }
            let (x, y) = metrics.origin(col, row);
            let (fg, bg) = painted.cell_colors(cell.attrs);
            let cells = char_width(cell.ch);
            if bg != base {
                surface.fill_rect(
                    x,
                    y,
                    metrics.width.saturating_mul(u32::from(cells)),
                    metrics.height,
                    bg,
                );
            }
            draw_glyph(surface, metrics.font, x, y, cell.ch, fg);
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
    grid: &Grid,
    painted: &Painted,
    metrics: CellMetrics,
    col: u16,
    row: u16,
) {
    let (x, y) = metrics.origin(col, row);
    surface.fill_rect(
        x,
        y,
        metrics.width,
        metrics.height,
        painted.scheme.cursor.opaque(),
    );
    let ch = grid.cell(col, row).map_or(' ', |cell| cell.ch);
    // The cursor over a wide glyph's continuation cell shows covered space.
    let ch = if ch == CONTINUATION { ' ' } else { ch };
    draw_glyph(
        surface,
        metrics.font,
        x,
        y,
        ch,
        painted.scheme.cursor_text.opaque(),
    );
}

/// Draw a single glyph `ch` at `(x, y)` in `color`.
fn draw_glyph(surface: &mut Surface, font: BitmapFont, x: u32, y: u32, ch: char, color: Color) {
    let mut utf8 = [0u8; 4];
    font.draw_text(
        surface,
        to_i32(x),
        to_i32(y),
        ch.encode_utf8(&mut utf8),
        color,
    );
}

/// Saturating `u32` → `i32`.
fn to_i32(value: u32) -> i32 {
    i32::try_from(value).unwrap_or(i32::MAX)
}
