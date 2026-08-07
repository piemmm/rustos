//! The character-cell screen the terminal draws to.
//!
//! A [`Grid`] is a fixed rectangle of [`Cell`]s plus a cursor and the current
//! rendition pen. It exposes the cursor-relative operations a terminal needs —
//! writing a glyph, the C0 control moves (backspace, tab, line feed, carriage
//! return), absolute and relative cursor positioning, the erase operations, the
//! scroll region and explicit scrolling, the alternate screen, cursor
//! visibility, and the saved-cursor state — and nothing about byte parsing:
//! turning a byte stream into these calls is the
//! [`Parser`](crate::parser::Parser)'s job, so the two concerns stay separate.
//!
//! The [`Cell`] and its [`Attributes`] are [`lib/vt`](tairix_vt)'s shared
//! representation, not a second copy: the emulator is a *consumer* of the one
//! ANSI/VT/xterm vocabulary, so a cell here is exactly the
//! cell a curses renderer emits.
//!
//! Every operation is total and saturating: an out-of-range coordinate clamps
//! into the grid and a full region scrolls rather than growing, so a hostile
//! or buggy byte stream can never index out of bounds or panic.

use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

use tairix_vt::{char_width, Attributes, Cell, EraseMode, CONTINUATION};

/// The largest grid dimension, in cells, the terminal will allocate.
///
/// A fixed ceiling keeps `cols * rows` bounded so a caller cannot ask the
/// terminal to allocate an unreasonable buffer; a larger request fails closed
/// in [`Grid::new`].
pub const MAX_DIMENSION: u16 = 1024;

/// The tab stop interval, in cells.
const TAB_WIDTH: u16 = 8;

/// A snapshot of the screen state saved when entering the alternate screen and
/// restored when leaving it.
#[derive(Clone, Debug, Eq, PartialEq)]
struct Screen {
    cursor_col: u16,
    cursor_row: u16,
    pen: Attributes,
    scroll_top: u16,
    scroll_bottom: u16,
    cells: Vec<Cell>,
}

/// A saved cursor position and pen (`ESC 7` / `ESC 8`).
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
struct SavedCursor {
    col: u16,
    row: u16,
    pen: Attributes,
}

/// A fixed-size character-cell screen with a cursor and rendition pen.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Grid {
    cols: u16,
    rows: u16,
    cursor_col: u16,
    cursor_row: u16,
    /// Whether the last glyph filled the row and the wrap is owed.
    ///
    /// The cursor still rests on the last column; the wrap is paid by the
    /// next glyph and cancelled by anything that moves or erases first. The
    /// column itself therefore stays inside the grid, so every reader — the
    /// erase operations, the renderer's cursor block — addresses a real cell.
    pending_wrap: bool,
    cells: Vec<Cell>,
    /// The rendition new glyphs are written with (the folded SGR state).
    pen: Attributes,
    /// Whether the cursor is shown (`CSI ? 25 h` / `l`).
    cursor_visible: bool,
    /// The 0-based top row of the scroll region (inclusive).
    scroll_top: u16,
    /// The 0-based bottom row of the scroll region (inclusive).
    scroll_bottom: u16,
    /// The saved cursor for `ESC 7` / `ESC 8`, if any.
    saved_cursor: Option<SavedCursor>,
    /// The main screen, saved while the alternate screen is active.
    alternate: Option<Screen>,
    /// The window title most recently set by an OSC title sequence.
    title: String,
}

impl Grid {
    /// Create a `cols`×`rows` grid of blank cells with the cursor at the home
    /// position `(0, 0)`, the plain rendition pen, the cursor shown, and the
    /// scroll region covering the whole screen.
    ///
    /// Returns `None` for a zero dimension or a dimension above
    /// [`MAX_DIMENSION`], so an unusable screen size fails closed rather than
    /// allocating something degenerate.
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
            pending_wrap: false,
            cells: vec![Cell::BLANK; count],
            pen: Attributes::PLAIN,
            cursor_visible: true,
            scroll_top: 0,
            scroll_bottom: rows - 1,
            saved_cursor: None,
            alternate: None,
            title: String::new(),
        })
    }

    /// Reshape the screen to `cols`×`rows`, preserving the top-left overlap of
    /// the current contents (a window resize).
    ///
    /// Cells inside both the old and new rectangle keep their glyph and pen;
    /// cells the resize newly exposes are blank; cells it drops are discarded.
    /// The cursor, the saved cursor, and the scroll region are clamped into the
    /// new bounds (the scroll region reset to the full screen, as a real tty
    /// does on `TIOCSWINSZ`), and any active alternate screen is reshaped the
    /// same way so its state stays consistent. Returns `false`, changing
    /// nothing, for a zero/oversized dimension or a no-op resize (fail closed).
    #[must_use]
    pub fn resize(&mut self, cols: u16, rows: u16) -> bool {
        if cols == 0 || rows == 0 || cols > MAX_DIMENSION || rows > MAX_DIMENSION {
            return false;
        }
        if cols == self.cols && rows == self.rows {
            return false;
        }
        self.cells = Self::reshape_cells(&self.cells, self.cols, self.rows, cols, rows);
        if let Some(alt) = self.alternate.as_mut() {
            alt.cells = Self::reshape_cells(&alt.cells, self.cols, self.rows, cols, rows);
            alt.cursor_col = alt.cursor_col.min(cols - 1);
            alt.cursor_row = alt.cursor_row.min(rows - 1);
            alt.scroll_top = 0;
            alt.scroll_bottom = rows - 1;
        }
        self.cols = cols;
        self.rows = rows;
        self.cursor_col = self.cursor_col.min(cols - 1);
        self.cursor_row = self.cursor_row.min(rows - 1);
        self.pending_wrap = false;
        self.scroll_top = 0;
        self.scroll_bottom = rows - 1;
        if let Some(saved) = self.saved_cursor.as_mut() {
            saved.col = saved.col.min(cols - 1);
            saved.row = saved.row.min(rows - 1);
        }
        true
    }

    /// A fresh `cols`×`rows` cell buffer holding the top-left overlap of
    /// `old` (an `old_cols`×`old_rows` buffer); newly exposed cells are blank.
    fn reshape_cells(
        old: &[Cell],
        old_cols: u16,
        old_rows: u16,
        cols: u16,
        rows: u16,
    ) -> Vec<Cell> {
        let mut out = vec![Cell::BLANK; usize::from(cols) * usize::from(rows)];
        let copy_rows = old_rows.min(rows);
        let copy_cols = old_cols.min(cols);
        for r in 0..copy_rows {
            for c in 0..copy_cols {
                let src = usize::from(r) * usize::from(old_cols) + usize::from(c);
                let dst = usize::from(r) * usize::from(cols) + usize::from(c);
                if let (Some(&cell), Some(slot)) = (old.get(src), out.get_mut(dst)) {
                    *slot = cell;
                }
            }
        }
        out
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

    /// Whether the cursor is currently shown.
    #[must_use]
    pub const fn cursor_visible(&self) -> bool {
        self.cursor_visible
    }

    /// The rendition new glyphs are currently written with.
    #[must_use]
    pub const fn pen(&self) -> Attributes {
        self.pen
    }

    /// Whether the alternate screen buffer is currently active.
    #[must_use]
    pub const fn on_alternate_screen(&self) -> bool {
        self.alternate.is_some()
    }

    /// The window title most recently set by an OSC title sequence (empty if
    /// none has been set).
    #[must_use]
    pub fn title(&self) -> &str {
        &self.title
    }

    /// The cell at `(col, row)`, or `None` if the coordinate is off the grid.
    #[must_use]
    pub fn cell(&self, col: u16, row: u16) -> Option<Cell> {
        if col >= self.cols || row >= self.rows {
            return None;
        }
        self.cells.get(self.index(col, row)).copied()
    }

    /// Replace the rendition pen, so subsequently written glyphs carry `attrs`.
    pub fn set_attributes(&mut self, attrs: Attributes) {
        self.pen = attrs;
    }

    /// Set whether the cursor is shown.
    pub fn set_cursor_visible(&mut self, visible: bool) {
        self.cursor_visible = visible;
    }

    /// Set the window title from a parsed OSC title.
    pub fn set_title(&mut self, title: &str) {
        self.title.clear();
        self.title.push_str(title);
    }

    /// Write `ch` at the cursor with the current pen and advance one column
    /// (two for a double-width glyph).
    ///
    /// Filling the last column does not wrap: the cursor rests on that column
    /// and the wrap is *owed*, paid by the next glyph and cancelled by
    /// anything that moves or erases first — the rule every terminal follows
    /// and the shared [`conformance`](tairix_vt::conformance) script pins.
    /// Wrapping eagerly would line-feed, and at the bottom margin scroll the
    /// whole screen, the moment a program painted a full-width row.
    ///
    /// A double-width glyph (see [`char_width`]) occupies two cells: the lead
    /// cell and a [`CONTINUATION`] cell to its right — the same layout the
    /// `lib/curses` window writer and the framebuffer console produce, so a
    /// TUI's column arithmetic and this grid agree. When only one column
    /// remains the wide glyph wraps whole, blanking the leftover column.
    pub fn write_char(&mut self, ch: char) {
        let width = char_width(ch);
        if self.pending_wrap {
            self.carriage_return();
            self.line_feed();
        }
        let pen = self.pen;
        if width == 2 && self.cursor_col.saturating_add(1) >= self.cols {
            self.clear_wide_at(self.cursor_col, self.cursor_row);
            let index = self.index(self.cursor_col, self.cursor_row);
            if let Some(cell) = self.cells.get_mut(index) {
                *cell = Cell::styled(' ', pen);
            }
            self.carriage_return();
            self.line_feed();
        }
        self.clear_wide_at(self.cursor_col, self.cursor_row);
        if width == 2 {
            self.clear_wide_at(self.cursor_col.saturating_add(1), self.cursor_row);
        }
        let index = self.index(self.cursor_col, self.cursor_row);
        if let Some(cell) = self.cells.get_mut(index) {
            *cell = Cell::styled(ch, pen);
        }
        // On a degenerate one-column grid there is no second cell; writing it
        // anyway would alias the next row (`index` is row-major).
        if width == 2 && self.cursor_col.saturating_add(1) < self.cols {
            let index = self.index(self.cursor_col.saturating_add(1), self.cursor_row);
            if let Some(cell) = self.cells.get_mut(index) {
                *cell = Cell::styled(CONTINUATION, pen);
            }
        }
        let next = self.cursor_col.saturating_add(width);
        if next < self.cols {
            self.cursor_col = next;
        } else {
            self.cursor_col = self.cols.saturating_sub(1);
            self.pending_wrap = true;
        }
    }

    /// Place the cursor at `(col, row)`, clamped into the grid.
    ///
    /// Every explicit cursor movement goes through here, so none of them can
    /// forget that moving cancels an owed wrap or leave the column outside
    /// the grid.
    fn place(&mut self, col: u16, row: u16) {
        self.cursor_col = col.min(self.cols.saturating_sub(1));
        self.cursor_row = row.min(self.rows.saturating_sub(1));
        self.pending_wrap = false;
    }

    /// Move the cursor down one row, scrolling the region up when it is already
    /// on the bottom margin.
    pub fn line_feed(&mut self) {
        self.pending_wrap = false;
        if self.cursor_row == self.scroll_bottom {
            self.scroll_region_up(1);
        } else if self.cursor_row.saturating_add(1) < self.rows {
            self.cursor_row += 1;
        }
    }

    /// Move the cursor to the start of the current row.
    pub fn carriage_return(&mut self) {
        self.place(0, self.cursor_row);
    }

    /// Move the cursor one column left, stopping at the left edge.
    ///
    /// With a wrap owed the cursor already rests on the last column, so the
    /// backspace only cancels the wrap: a rubout (backspace, space,
    /// backspace) then erases the glyph that filled the row rather than the
    /// one before it.
    pub fn backspace(&mut self) {
        if self.pending_wrap {
            self.pending_wrap = false;
            return;
        }
        self.place(self.cursor_col.saturating_sub(1), self.cursor_row);
    }

    /// Advance the cursor to the next tab stop, clamped to the last column.
    pub fn tab(&mut self) {
        let next = (self.cursor_col / TAB_WIDTH)
            .saturating_add(1)
            .saturating_mul(TAB_WIDTH);
        self.place(next, self.cursor_row);
    }

    /// Move the cursor to `(col, row)`, clamping each coordinate into the
    /// grid.
    pub fn move_to(&mut self, col: u16, row: u16) {
        self.place(col, row);
    }

    /// Move the cursor to 0-based `col` on the current row, clamped.
    pub fn move_to_column(&mut self, col: u16) {
        self.place(col, self.cursor_row);
    }

    /// Move the cursor up `n` rows, stopping at the top.
    pub fn move_up(&mut self, n: u16) {
        self.place(self.cursor_col, self.cursor_row.saturating_sub(n));
    }

    /// Move the cursor down `n` rows, stopping at the bottom.
    pub fn move_down(&mut self, n: u16) {
        self.place(self.cursor_col, self.cursor_row.saturating_add(n));
    }

    /// Move the cursor left `n` columns, stopping at the left edge.
    pub fn move_left(&mut self, n: u16) {
        self.place(self.cursor_col.saturating_sub(n), self.cursor_row);
    }

    /// Move the cursor right `n` columns, stopping at the right edge.
    pub fn move_right(&mut self, n: u16) {
        self.place(self.cursor_col.saturating_add(n), self.cursor_row);
    }

    /// Move the cursor `n` rows down to the start of that row (`CNL`).
    pub fn next_line(&mut self, n: u16) {
        self.place(0, self.cursor_row.saturating_add(n));
    }

    /// Move the cursor `n` rows up to the start of that row (`CPL`).
    pub fn prev_line(&mut self, n: u16) {
        self.place(0, self.cursor_row.saturating_sub(n));
    }

    /// Erase part of the current row relative to the cursor.
    ///
    /// Follows ANSI `EL`: [`EraseMode::ToEnd`] clears from the cursor to the
    /// end of the row, [`EraseMode::ToStart`] from the start of the row to the
    /// cursor (inclusive), and [`EraseMode::All`] the whole row. The cursor
    /// does not move.
    pub fn erase_in_line(&mut self, mode: EraseMode) {
        self.pending_wrap = false;
        let row_start = self.index(0, self.cursor_row);
        let row_end = row_start + usize::from(self.cols);
        let cursor = self.index(self.cursor_col, self.cursor_row);
        match mode {
            EraseMode::ToEnd => self.blank(cursor, row_end),
            EraseMode::ToStart => self.blank(row_start, cursor + 1),
            EraseMode::All => self.blank(row_start, row_end),
        }
    }

    /// Erase part of the whole screen relative to the cursor.
    ///
    /// Follows ANSI `ED`: [`EraseMode::ToEnd`] clears from the cursor to the
    /// end of the screen, [`EraseMode::ToStart`] from the top of the screen to
    /// the cursor (inclusive), and [`EraseMode::All`] the whole screen. The
    /// cursor does not move.
    pub fn erase_in_display(&mut self, mode: EraseMode) {
        self.pending_wrap = false;
        let cursor = self.index(self.cursor_col, self.cursor_row);
        let len = self.cells.len();
        match mode {
            EraseMode::ToEnd => self.blank(cursor, len),
            EraseMode::ToStart => self.blank(0, cursor + 1),
            EraseMode::All => self.blank(0, len),
        }
    }

    /// Set the scroll region to the 1-based rows `top..=bottom`, clamped into
    /// the grid; a degenerate or inverted request falls back to the whole
    /// screen (fail closed).
    ///
    /// `DECSTBM` homes the cursor to the top-left *of the region*, not of the
    /// screen, so a program that reserves a header above its scrolling body
    /// starts drawing inside the body.
    pub fn set_scroll_region(&mut self, top: u16, bottom: u16) {
        let last = self.rows.saturating_sub(1);
        let top = top.saturating_sub(1).min(last);
        let bottom = bottom.saturating_sub(1).min(last);
        if top < bottom {
            self.scroll_top = top;
            self.scroll_bottom = bottom;
        } else {
            self.reset_scroll_region();
        }
        self.place(0, self.scroll_top);
    }

    /// Reset the scroll region to the whole screen.
    pub fn reset_scroll_region(&mut self) {
        self.scroll_top = 0;
        self.scroll_bottom = self.rows.saturating_sub(1);
    }

    /// Scroll the scroll region up `n` lines (`SU`): content moves toward the
    /// top and the freed bottom lines are blanked. The cursor does not move.
    pub fn scroll_up(&mut self, n: u16) {
        self.scroll_region_up(n);
    }

    /// Scroll the scroll region down `n` lines (`SD`): content moves toward the
    /// bottom and the freed top lines are blanked. The cursor does not move.
    pub fn scroll_down(&mut self, n: u16) {
        let stride = usize::from(self.cols);
        if stride == 0 {
            return;
        }
        let region_rows = usize::from(self.scroll_bottom - self.scroll_top) + 1;
        let lines = usize::from(n).min(region_rows);
        let top = self.index(0, self.scroll_top);
        let bottom = self.index(0, self.scroll_bottom) + stride;
        let shift = lines * stride;
        if let Some(region) = self.cells.get_mut(top..bottom) {
            region.copy_within(..region.len() - shift, shift);
        }
        self.blank(top, top + shift);
    }

    /// Switch to the alternate screen buffer, saving the main screen. A second
    /// request while already on the alternate screen is a no-op.
    pub fn enter_alt_screen(&mut self) {
        if self.alternate.is_some() {
            return;
        }
        self.alternate = Some(self.snapshot());
        self.clear();
    }

    /// Switch back to the main screen buffer, restoring its saved contents. A
    /// request while not on the alternate screen is a no-op.
    pub fn leave_alt_screen(&mut self) {
        if let Some(main) = self.alternate.take() {
            self.restore(main);
        }
    }

    /// Save the cursor position and pen (`ESC 7`).
    pub fn save_cursor(&mut self) {
        self.saved_cursor = Some(SavedCursor {
            col: self.cursor_col,
            row: self.cursor_row,
            pen: self.pen,
        });
    }

    /// Restore the saved cursor position and pen (`ESC 8`). With nothing saved,
    /// the cursor homes and the pen resets — the conventional fallback.
    pub fn restore_cursor(&mut self) {
        if let Some(saved) = self.saved_cursor {
            self.place(saved.col, saved.row);
            self.pen = saved.pen;
        } else {
            self.place(0, 0);
            self.pen = Attributes::PLAIN;
        }
    }

    /// Blank every cell and move the cursor home. The pen is left unchanged.
    pub fn clear(&mut self) {
        self.cells.fill(Cell::BLANK);
        self.place(0, 0);
    }

    /// The flat-buffer index of `(col, row)`. Callers guarantee the
    /// coordinate is in range; an out-of-range index simply addresses a cell
    /// that `cells.get` will reject.
    fn index(&self, col: u16, row: u16) -> usize {
        usize::from(row) * usize::from(self.cols) + usize::from(col)
    }

    /// Clear the complete wide glyph, if any, intersecting `(col, row)`.
    fn clear_wide_at(&mut self, col: u16, row: u16) {
        if col >= self.cols || row >= self.rows {
            return;
        }
        let index = self.index(col, row);
        let Some(cell) = self.cells.get(index).copied() else {
            return;
        };
        let blank = Cell::styled(' ', self.pen);
        if cell.ch == CONTINUATION && col > 0 {
            if let Some(span) = self.cells.get_mut(index - 1..=index) {
                span.fill(blank);
            }
        } else if char_width(cell.ch) == 2 {
            let end = (index + 2).min(self.cells.len());
            if let Some(span) = self.cells.get_mut(index..end) {
                span.fill(blank);
            }
        }
    }

    /// Blank the half-open cell range `start..end`, expanding its boundaries
    /// to clear any wide glyph it intersects.
    fn blank(&mut self, start: usize, end: usize) {
        let len = self.cells.len();
        let mut start = start.min(len);
        let mut end = end.min(len);
        if start < end
            && self
                .cells
                .get(start)
                .is_some_and(|cell| cell.ch == CONTINUATION)
        {
            start = start.saturating_sub(1);
        }
        if start < end
            && self
                .cells
                .get(end - 1)
                .is_some_and(|cell| char_width(cell.ch) == 2)
        {
            end = end.saturating_add(1).min(len);
        }
        let blank = Cell::styled(' ', self.pen);
        if let Some(slice) = self.cells.get_mut(start..end) {
            slice.fill(blank);
        }
    }

    /// Scroll the scroll region up `n` lines, blanking the freed bottom lines.
    fn scroll_region_up(&mut self, n: u16) {
        let stride = usize::from(self.cols);
        if stride == 0 {
            return;
        }
        let region_rows = usize::from(self.scroll_bottom - self.scroll_top) + 1;
        let lines = usize::from(n).min(region_rows);
        let top = self.index(0, self.scroll_top);
        let bottom = self.index(0, self.scroll_bottom) + stride;
        let shift = lines * stride;
        if let Some(region) = self.cells.get_mut(top..bottom) {
            region.copy_within(shift.., 0);
        }
        self.blank(bottom - shift, bottom);
    }

    /// Capture the current screen state for the alternate-screen swap.
    fn snapshot(&self) -> Screen {
        Screen {
            cursor_col: self.cursor_col,
            cursor_row: self.cursor_row,
            pen: self.pen,
            scroll_top: self.scroll_top,
            scroll_bottom: self.scroll_bottom,
            cells: self.cells.clone(),
        }
    }

    /// Restore a previously captured screen state.
    fn restore(&mut self, screen: Screen) {
        self.pen = screen.pen;
        self.scroll_top = screen.scroll_top;
        self.scroll_bottom = screen.scroll_bottom;
        self.cells = screen.cells;
        self.place(screen.cursor_col, screen.cursor_row);
    }
}
