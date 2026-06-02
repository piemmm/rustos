//! The character-cell screen the terminal draws to.
//!
//! A [`Grid`] is a fixed rectangle of [`Cell`]s plus a cursor. It exposes the
//! cursor-relative operations a terminal needs — writing a glyph, the C0
//! control moves (backspace, tab, line feed, carriage return), absolute and
//! relative cursor positioning, the erase operations, and scrolling — and
//! nothing about byte parsing: turning a byte stream into these calls is the
//! [`Parser`](crate::parser::Parser)'s job, so the two concerns stay
//! separate.
//!
//! Every operation is total and saturating: an out-of-range coordinate clamps
//! into the grid and a full screen scrolls rather than growing, so a hostile
//! or buggy byte stream can never index out of bounds or panic (`AGENTS.md`
//! §2.9).

use alloc::vec;
use alloc::vec::Vec;

/// The largest grid dimension, in cells, the terminal will allocate.
///
/// A fixed ceiling keeps `cols * rows` bounded so a caller cannot ask the
/// terminal to allocate an unreasonable buffer; a larger request fails closed
/// in [`Grid::new`] (`AGENTS.md` §2.9).
pub const MAX_DIMENSION: u16 = 1024;

/// The tab stop interval, in cells.
const TAB_WIDTH: u16 = 8;

/// One character cell: the glyph shown at a screen position.
///
/// A cell holds exactly one `char`; an unwritten cell is a space, so the grid
/// is always renderable without a separate "empty" sentinel (`AGENTS.md`
/// §2.11).
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct Cell {
    ch: char,
}

impl Cell {
    /// An unwritten cell: a single space.
    pub const BLANK: Self = Self { ch: ' ' };

    /// The glyph this cell shows.
    #[must_use]
    pub const fn ch(self) -> char {
        self.ch
    }
}

impl Default for Cell {
    fn default() -> Self {
        Self::BLANK
    }
}

/// A fixed-size character-cell screen with a cursor.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Grid {
    cols: u16,
    rows: u16,
    cursor_col: u16,
    cursor_row: u16,
    cells: Vec<Cell>,
}

impl Grid {
    /// Create a `cols`×`rows` grid of blank cells with the cursor at the home
    /// position `(0, 0)`.
    ///
    /// Returns `None` for a zero dimension or a dimension above
    /// [`MAX_DIMENSION`], so an unusable screen size fails closed rather than
    /// allocating something degenerate (`AGENTS.md` §2.9).
    #[must_use]
    pub fn new(cols: u16, rows: u16) -> Option<Self> {
        if cols == 0 || rows == 0 || cols > MAX_DIMENSION || rows > MAX_DIMENSION {
            return None;
        }
        let count = usize::from(cols) * usize::from(rows);
        Some(Self {
            cols,
            rows,
            cursor_col: 0,
            cursor_row: 0,
            cells: vec![Cell::BLANK; count],
        })
    }

    /// The number of columns.
    #[must_use]
    pub const fn cols(&self) -> u16 {
        self.cols
    }

    /// The number of rows.
    #[must_use]
    pub const fn rows(&self) -> u16 {
        self.rows
    }

    /// The cursor column, `0..cols`.
    #[must_use]
    pub const fn cursor_col(&self) -> u16 {
        self.cursor_col
    }

    /// The cursor row, `0..rows`.
    #[must_use]
    pub const fn cursor_row(&self) -> u16 {
        self.cursor_row
    }

    /// The cell at `(col, row)`, or `None` if the coordinate is off the grid.
    #[must_use]
    pub fn cell(&self, col: u16, row: u16) -> Option<Cell> {
        if col >= self.cols || row >= self.rows {
            return None;
        }
        self.cells.get(self.index(col, row)).copied()
    }

    /// Write `ch` at the cursor and advance one column, wrapping to the next
    /// line (scrolling if already on the last row) at the right edge.
    pub fn write_char(&mut self, ch: char) {
        if self.cursor_col >= self.cols {
            self.carriage_return();
            self.line_feed();
        }
        let index = self.index(self.cursor_col, self.cursor_row);
        if let Some(cell) = self.cells.get_mut(index) {
            cell.ch = ch;
        }
        self.cursor_col = self.cursor_col.saturating_add(1);
        if self.cursor_col >= self.cols {
            self.carriage_return();
            self.line_feed();
        }
    }

    /// Move the cursor down one row, scrolling the screen up when it is
    /// already on the last row.
    pub fn line_feed(&mut self) {
        if self.cursor_row.saturating_add(1) >= self.rows {
            self.scroll_up();
        } else {
            self.cursor_row += 1;
        }
    }

    /// Move the cursor to the start of the current row.
    pub fn carriage_return(&mut self) {
        self.cursor_col = 0;
    }

    /// Move the cursor one column left, stopping at the left edge.
    pub fn backspace(&mut self) {
        self.cursor_col = self.cursor_col.saturating_sub(1);
    }

    /// Advance the cursor to the next tab stop, clamped to the last column.
    pub fn tab(&mut self) {
        let next = (self.cursor_col / TAB_WIDTH)
            .saturating_add(1)
            .saturating_mul(TAB_WIDTH);
        self.cursor_col = next.min(self.cols.saturating_sub(1));
    }

    /// Move the cursor to `(col, row)`, clamping each coordinate into the
    /// grid.
    pub fn move_to(&mut self, col: u16, row: u16) {
        self.cursor_col = col.min(self.cols.saturating_sub(1));
        self.cursor_row = row.min(self.rows.saturating_sub(1));
    }

    /// Move the cursor up `n` rows, stopping at the top.
    pub fn move_up(&mut self, n: u16) {
        self.cursor_row = self.cursor_row.saturating_sub(n);
    }

    /// Move the cursor down `n` rows, stopping at the bottom.
    pub fn move_down(&mut self, n: u16) {
        let target = self.cursor_row.saturating_add(n);
        self.cursor_row = target.min(self.rows.saturating_sub(1));
    }

    /// Move the cursor left `n` columns, stopping at the left edge.
    pub fn move_left(&mut self, n: u16) {
        self.cursor_col = self.cursor_col.saturating_sub(n);
    }

    /// Move the cursor right `n` columns, stopping at the right edge.
    pub fn move_right(&mut self, n: u16) {
        let target = self.cursor_col.saturating_add(n);
        self.cursor_col = target.min(self.cols.saturating_sub(1));
    }

    /// Erase part of the current row relative to the cursor.
    ///
    /// `mode` follows ANSI `EL`: `0` clears from the cursor to the end of the
    /// row, `1` from the start of the row to the cursor (inclusive), and `2`
    /// the whole row. Any other value is a no-op. The cursor does not move.
    pub fn erase_in_line(&mut self, mode: u16) {
        let row_start = self.index(0, self.cursor_row);
        let row_end = row_start + usize::from(self.cols);
        let cursor = self.index(self.cursor_col, self.cursor_row);
        match mode {
            0 => self.blank(cursor, row_end),
            1 => self.blank(row_start, cursor + 1),
            2 => self.blank(row_start, row_end),
            _ => {}
        }
    }

    /// Erase part of the whole screen relative to the cursor.
    ///
    /// `mode` follows ANSI `ED`: `0` clears from the cursor to the end of the
    /// screen, `1` from the top of the screen to the cursor (inclusive), and
    /// `2` the whole screen. Any other value is a no-op. The cursor does not
    /// move.
    pub fn erase_in_display(&mut self, mode: u16) {
        let cursor = self.index(self.cursor_col, self.cursor_row);
        let len = self.cells.len();
        match mode {
            0 => self.blank(cursor, len),
            1 => self.blank(0, cursor + 1),
            2 => self.blank(0, len),
            _ => {}
        }
    }

    /// Blank every cell and move the cursor home.
    pub fn clear(&mut self) {
        for cell in &mut self.cells {
            *cell = Cell::BLANK;
        }
        self.cursor_col = 0;
        self.cursor_row = 0;
    }

    /// The flat-buffer index of `(col, row)`. Callers guarantee the
    /// coordinate is in range; an out-of-range index simply addresses a cell
    /// that `cells.get` will reject.
    fn index(&self, col: u16, row: u16) -> usize {
        usize::from(row) * usize::from(self.cols) + usize::from(col)
    }

    /// Blank the half-open cell range `start..end`, clamped to the buffer.
    fn blank(&mut self, start: usize, end: usize) {
        let len = self.cells.len();
        let start = start.min(len);
        let end = end.min(len);
        if let Some(slice) = self.cells.get_mut(start..end) {
            for cell in slice {
                *cell = Cell::BLANK;
            }
        }
    }

    /// Shift every row up by one and blank the freed last row, leaving the
    /// cursor on the (now blank) last row.
    fn scroll_up(&mut self) {
        let stride = usize::from(self.cols);
        let len = self.cells.len();
        if stride == 0 || len < stride {
            return;
        }
        self.cells.copy_within(stride.., 0);
        self.blank(len - stride, len);
        self.cursor_row = self.rows.saturating_sub(1);
    }
}
