//! Integer cell geometry: a [`Size`] in rows and columns and a [`Pos`] of a
//! single cell.
//!
//! Curses addresses the screen in character cells, not pixels, so this is a
//! small, self-contained integer geometry — distinct from `lib/geometry`,
//! which models *pixel* geometry and the desktop DPI scale for the GUI
//! (`AGENTS.md` §10). The two never mix: a TUI is cells, a compositor is
//! pixels.

/// A rectangle size in character cells.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct Size {
    /// Height in rows.
    pub rows: u16,
    /// Width in columns.
    pub cols: u16,
}

impl Size {
    /// A size of `rows` × `cols` cells.
    #[must_use]
    pub const fn new(rows: u16, cols: u16) -> Size {
        Size { rows, cols }
    }

    /// The number of cells this size contains.
    #[must_use]
    pub const fn area(self) -> usize {
        self.rows as usize * self.cols as usize
    }

    /// Whether either dimension is zero (the size encloses no cell).
    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.rows == 0 || self.cols == 0
    }

    /// Whether `pos` lies within a rectangle of this size anchored at the
    /// origin.
    #[must_use]
    pub const fn contains(self, pos: Pos) -> bool {
        pos.row < self.rows && pos.col < self.cols
    }
}

/// A zero-based cell position: `row` from the top, `col` from the left.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct Pos {
    /// Row index from the top, zero-based.
    pub row: u16,
    /// Column index from the left, zero-based.
    pub col: u16,
}

impl Pos {
    /// The top-left cell.
    pub const ORIGIN: Pos = Pos { row: 0, col: 0 };

    /// The cell at `(row, col)`.
    #[must_use]
    pub const fn new(row: u16, col: u16) -> Pos {
        Pos { row, col }
    }

    /// This position shifted by `origin`, saturating at [`u16::MAX`] so an
    /// offset can never wrap (`AGENTS.md` §2.9).
    #[must_use]
    pub const fn offset_by(self, origin: Pos) -> Pos {
        Pos {
            row: self.row.saturating_add(origin.row),
            col: self.col.saturating_add(origin.col),
        }
    }
}
