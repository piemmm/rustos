//! [`Buffer`] — a dense rectangular grid of character [`Cell`]s.
//!
//! It is the storage every drawing surface is built on: a [`crate::Window`]
//! owns one, and the [screen driver] keeps two (the assembled *virtual* screen
//! and the last-flushed *physical* screen) so the [renderer] can diff them. All
//! access is bounds-checked and total — an out-of-range coordinate is a
//! [`None`]/`false`, never a panic.
//!
//! [screen driver]: crate::Screen
//! [renderer]: mod@crate::render

use alloc::vec;
use alloc::vec::Vec;

use tairix_vt::Cell;

use crate::geom::{Pos, Size};

/// A dense `rows × cols` grid of [`Cell`]s in row-major order.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Buffer {
    size: Size,
    cells: Vec<Cell>,
}

impl Buffer {
    /// A buffer of `size`, every cell [`Cell::BLANK`].
    #[must_use]
    pub fn new(size: Size) -> Buffer {
        Buffer {
            size,
            cells: vec![Cell::BLANK; size.area()],
        }
    }

    /// The buffer's dimensions.
    #[must_use]
    pub const fn size(&self) -> Size {
        self.size
    }

    /// The flat index of `pos`, or `None` if it is out of bounds.
    fn index(&self, pos: Pos) -> Option<usize> {
        if !self.size.contains(pos) {
            return None;
        }
        Some(usize::from(pos.row) * usize::from(self.size.cols) + usize::from(pos.col))
    }

    /// The cell at `pos`, or `None` if `pos` is out of bounds.
    #[must_use]
    pub fn get(&self, pos: Pos) -> Option<Cell> {
        self.index(pos).map(|i| self.cells[i])
    }

    /// Write `cell` at `pos`, returning `false` (and writing nothing) if `pos`
    /// is out of bounds.
    pub fn set(&mut self, pos: Pos, cell: Cell) -> bool {
        match self.index(pos) {
            Some(i) => {
                self.cells[i] = cell;
                true
            }
            None => false,
        }
    }

    /// Reset every cell to `fill`.
    pub fn fill(&mut self, fill: Cell) {
        self.cells.fill(fill);
    }

    /// The cells of row `row` as a slice, or `None` if `row` is out of bounds.
    #[must_use]
    pub fn row(&self, row: u16) -> Option<&[Cell]> {
        if row >= self.size.rows {
            return None;
        }
        let start = usize::from(row) * usize::from(self.size.cols);
        let end = start + usize::from(self.size.cols);
        self.cells.get(start..end)
    }

    /// Resize to `size`, preserving the cells that remain in range and filling
    /// any newly exposed cells with [`Cell::BLANK`].
    pub fn resize(&mut self, size: Size) {
        let mut next = vec![Cell::BLANK; size.area()];
        let copy_rows = self.size.rows.min(size.rows);
        let copy_cols = self.size.cols.min(size.cols);
        for row in 0..copy_rows {
            for col in 0..copy_cols {
                let src = usize::from(row) * usize::from(self.size.cols) + usize::from(col);
                let dst = usize::from(row) * usize::from(size.cols) + usize::from(col);
                next[dst] = self.cells[src];
            }
        }
        self.size = size;
        self.cells = next;
    }

    /// Copy every cell of `src` into this buffer with its top-left corner at
    /// `origin`, clipping anything that falls outside this buffer.
    ///
    /// This is how a window is composited onto the virtual screen: the source
    /// is drawn at its screen origin and silently clipped at the edges.
    pub fn blit(&mut self, src: &Buffer, origin: Pos) {
        for row in 0..src.size.rows {
            for col in 0..src.size.cols {
                let src_pos = Pos::new(row, col);
                if let Some(cell) = src.get(src_pos) {
                    self.set(src_pos.offset_by(origin), cell);
                }
            }
        }
    }

    /// Copy the `region`-sized sub-rectangle of `src` whose top-left is
    /// `src_origin` into this buffer at `dest_origin`, clipping at both edges.
    ///
    /// This backs a pad refresh, where only a window onto the (larger) pad is
    /// shown on screen.
    pub fn blit_region(&mut self, src: &Buffer, src_origin: Pos, dest_origin: Pos, region: Size) {
        for row in 0..region.rows {
            for col in 0..region.cols {
                let offset = Pos::new(row, col);
                let from = offset.offset_by(src_origin);
                if let Some(cell) = src.get(from) {
                    self.set(offset.offset_by(dest_origin), cell);
                }
            }
        }
    }
}
