//! [`Window`] — an application drawing surface.
//!
//! A window is the curses unit an application draws into: a [`Buffer`] of
//! cells, a cursor, the current drawing [`Attributes`], an optional scrolling
//! region, and the screen `origin` at which the [screen driver] composites it.
//! It is the *client* draw model — deliberately distinct from the terminal
//! emulator's *server* `Grid`, the legitimate §2.2 carve-out (`plans/CURSES.md`
//! §C4): the two play different roles, so they are not duplication.
//!
//! Every coordinate is window-relative and bounds-checked; an out-of-range
//! request returns [`CursesError::OutOfBounds`] rather than panicking
//! (`AGENTS.md` §2.9). Writing past the right edge wraps to the next line, and
//! writing past the last line of the scrolling region scrolls when scrolling is
//! enabled (the default-off curses `scrollok` behaviour).
//!
//! [screen driver]: crate::Screen

use rustos_vt::{Attributes, Cell, Color};

use crate::buffer::Buffer;
use crate::error::{CursesError, Result};
use crate::geom::{Pos, Size};
use crate::width::{char_width, CONTINUATION};

/// The Unicode box-drawing glyphs a default [`Window::draw_box`] uses.
///
/// These are the curses ACS line-drawing characters' Unicode equivalents, so a
/// box renders as connected lines on any UTF-8 terminal.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct BorderChars {
    /// Left and right edge glyph.
    pub vertical: char,
    /// Top and bottom edge glyph.
    pub horizontal: char,
    /// Top-left corner.
    pub top_left: char,
    /// Top-right corner.
    pub top_right: char,
    /// Bottom-left corner.
    pub bottom_left: char,
    /// Bottom-right corner.
    pub bottom_right: char,
}

impl BorderChars {
    /// The default light box-drawing set (`┌─┐│└┘`).
    pub const LIGHT: BorderChars = BorderChars {
        vertical: '│',
        horizontal: '─',
        top_left: '┌',
        top_right: '┐',
        bottom_left: '└',
        bottom_right: '┘',
    };
}

/// An application drawing surface: a cell buffer plus cursor and pen state.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Window {
    origin: Pos,
    buf: Buffer,
    cursor: Pos,
    attrs: Attributes,
    scroll_top: u16,
    scroll_bottom: u16,
    scrolling: bool,
}

impl Window {
    /// A blank window of `size` whose top-left sits at the screen `origin`.
    #[must_use]
    pub fn new(origin: Pos, size: Size) -> Window {
        let rows = size.rows;
        Window {
            origin,
            buf: Buffer::new(size),
            cursor: Pos::ORIGIN,
            attrs: Attributes::PLAIN,
            scroll_top: 0,
            scroll_bottom: rows.saturating_sub(1),
            scrolling: false,
        }
    }

    /// The window's screen origin (its top-left corner on the composited
    /// screen).
    #[must_use]
    pub const fn origin(&self) -> Pos {
        self.origin
    }

    /// Move the window's screen origin (curses `mvwin`).
    pub fn set_origin(&mut self, origin: Pos) {
        self.origin = origin;
    }

    /// The window's dimensions.
    #[must_use]
    pub const fn size(&self) -> Size {
        self.buf.size()
    }

    /// The window's underlying cell buffer (for compositing).
    #[must_use]
    pub const fn buffer(&self) -> &Buffer {
        &self.buf
    }

    /// The current cursor position, window-relative.
    #[must_use]
    pub const fn cursor(&self) -> Pos {
        self.cursor
    }

    /// Move the cursor to `pos`.
    ///
    /// # Errors
    ///
    /// [`CursesError::OutOfBounds`] if `pos` lies outside the window.
    pub fn move_to(&mut self, pos: Pos) -> Result<()> {
        if !self.buf.size().contains(pos) {
            return Err(CursesError::OutOfBounds);
        }
        self.cursor = pos;
        Ok(())
    }

    /// The current drawing attributes.
    #[must_use]
    pub const fn attributes(&self) -> Attributes {
        self.attrs
    }

    /// Replace the drawing attributes (curses `attrset`).
    pub fn set_attributes(&mut self, attrs: Attributes) {
        self.attrs = attrs;
    }

    /// Set the drawing foreground and background colours, leaving the rendition
    /// flags unchanged (the effect of selecting a colour pair).
    pub fn set_colors(&mut self, fg: Color, bg: Color) {
        self.attrs.foreground = fg;
        self.attrs.background = bg;
    }

    /// Enable or disable scrolling when output passes the bottom of the
    /// scrolling region (curses `scrollok`).
    pub fn set_scrolling(&mut self, enabled: bool) {
        self.scrolling = enabled;
    }

    /// Set the scrolling region to rows `top..=bottom` (curses `setscrreg`).
    ///
    /// # Errors
    ///
    /// [`CursesError::OutOfBounds`] if the region is inverted or extends past
    /// the last row.
    pub fn set_scroll_region(&mut self, top: u16, bottom: u16) -> Result<()> {
        if top > bottom || bottom >= self.buf.size().rows {
            return Err(CursesError::OutOfBounds);
        }
        self.scroll_top = top;
        self.scroll_bottom = bottom;
        Ok(())
    }

    /// Write `cell` at `pos` without moving the cursor.
    ///
    /// # Errors
    ///
    /// [`CursesError::OutOfBounds`] if `pos` lies outside the window.
    pub fn put_cell(&mut self, pos: Pos, cell: Cell) -> Result<()> {
        if self.buf.set(pos, cell) {
            Ok(())
        } else {
            Err(CursesError::OutOfBounds)
        }
    }

    /// Write `ch` at the cursor with the current attributes and advance the
    /// cursor, wrapping and scrolling as configured.
    ///
    /// A double-width glyph (see [`char_width`]) occupies two cells: it is
    /// written into the cursor cell and the cell to its right is set to the
    /// [`CONTINUATION`] marker. If only one column remains on the line the
    /// glyph wraps to the next line whole rather than being split.
    pub fn add_char(&mut self, ch: char) {
        if char_width(ch) == 2 {
            self.add_wide_char(ch);
        } else {
            let cursor = self.cursor;
            let _ = self.buf.set(cursor, Cell::styled(ch, self.attrs));
            self.advance_cursor();
        }
    }

    /// Write a double-width glyph and its continuation cell, wrapping first if
    /// the glyph would not fit in the columns left on the current line.
    fn add_wide_char(&mut self, ch: char) {
        let cols = self.buf.size().cols;
        if self.cursor.col + 1 >= cols {
            let cursor = self.cursor;
            let _ = self.buf.set(cursor, self.blank());
            self.wrap_line();
        }
        let lead = self.cursor;
        let _ = self.buf.set(lead, Cell::styled(ch, self.attrs));
        let _ = self.buf.set(
            Pos::new(lead.row, lead.col.saturating_add(1)),
            Cell::styled(CONTINUATION, self.attrs),
        );
        self.advance_cursor();
        self.advance_cursor();
    }

    /// Write each character of `text` with [`Window::add_char`].
    pub fn add_str(&mut self, text: &str) {
        for ch in text.chars() {
            self.add_char(ch);
        }
    }

    /// Move the cursor to `pos`, then write `ch` there (curses `mvwaddch`).
    ///
    /// # Errors
    ///
    /// [`CursesError::OutOfBounds`] if `pos` lies outside the window.
    pub fn move_add_char(&mut self, pos: Pos, ch: char) -> Result<()> {
        self.move_to(pos)?;
        self.add_char(ch);
        Ok(())
    }

    /// Move the cursor to `pos`, then write `text` there (curses `mvwaddstr`).
    ///
    /// # Errors
    ///
    /// [`CursesError::OutOfBounds`] if `pos` lies outside the window.
    pub fn move_add_str(&mut self, pos: Pos, text: &str) -> Result<()> {
        self.move_to(pos)?;
        self.add_str(text);
        Ok(())
    }

    /// Blank every cell with the current background attributes (curses
    /// `werase`), and home the cursor.
    pub fn erase(&mut self) {
        self.buf.fill(self.blank());
        self.cursor = Pos::ORIGIN;
    }

    /// Blank from the cursor to the end of its line (curses `clrtoeol`).
    pub fn clear_to_eol(&mut self) {
        let blank = self.blank();
        let row = self.cursor.row;
        let cols = self.buf.size().cols;
        for col in self.cursor.col..cols {
            let _ = self.buf.set(Pos::new(row, col), blank);
        }
    }

    /// Draw `count` horizontal cells of `ch` rightward from the cursor (curses
    /// `whline`), clipping at the right edge. The cursor does not move.
    pub fn horizontal_line(&mut self, ch: char, count: u16) {
        let blank = Cell::styled(ch, self.attrs);
        let row = self.cursor.row;
        let cols = self.buf.size().cols;
        let end = self.cursor.col.saturating_add(count).min(cols);
        for col in self.cursor.col..end {
            let _ = self.buf.set(Pos::new(row, col), blank);
        }
    }

    /// Draw `count` vertical cells of `ch` downward from the cursor (curses
    /// `wvline`), clipping at the bottom edge. The cursor does not move.
    pub fn vertical_line(&mut self, ch: char, count: u16) {
        let cell = Cell::styled(ch, self.attrs);
        let col = self.cursor.col;
        let rows = self.buf.size().rows;
        let end = self.cursor.row.saturating_add(count).min(rows);
        for row in self.cursor.row..end {
            let _ = self.buf.set(Pos::new(row, col), cell);
        }
    }

    /// Draw a border around the window edge with the default light box glyphs
    /// (curses `box`). The cursor does not move.
    pub fn draw_box(&mut self) {
        self.draw_border(BorderChars::LIGHT);
    }

    /// Draw a border around the window edge with `chars`.
    pub fn draw_border(&mut self, chars: BorderChars) {
        let size = self.buf.size();
        if size.rows < 2 || size.cols < 2 {
            return;
        }
        let last_row = size.rows - 1;
        let last_col = size.cols - 1;
        let horizontal_cell = Cell::styled(chars.horizontal, self.attrs);
        let vertical_cell = Cell::styled(chars.vertical, self.attrs);
        for col in 1..last_col {
            let _ = self.buf.set(Pos::new(0, col), horizontal_cell);
            let _ = self.buf.set(Pos::new(last_row, col), horizontal_cell);
        }
        for row in 1..last_row {
            let _ = self.buf.set(Pos::new(row, 0), vertical_cell);
            let _ = self.buf.set(Pos::new(row, last_col), vertical_cell);
        }
        let _ = self
            .buf
            .set(Pos::ORIGIN, Cell::styled(chars.top_left, self.attrs));
        let _ = self.buf.set(
            Pos::new(0, last_col),
            Cell::styled(chars.top_right, self.attrs),
        );
        let _ = self.buf.set(
            Pos::new(last_row, 0),
            Cell::styled(chars.bottom_left, self.attrs),
        );
        let _ = self.buf.set(
            Pos::new(last_row, last_col),
            Cell::styled(chars.bottom_right, self.attrs),
        );
    }

    /// Scroll the scrolling region by `lines`: positive scrolls content up
    /// (toward the top), negative scrolls it down. Exposed rows are blanked
    /// with the current background. The cursor does not move.
    pub fn scroll(&mut self, lines: i32) {
        for _ in 0..lines.unsigned_abs() {
            if lines > 0 {
                self.scroll_up_once();
            } else {
                self.scroll_down_once();
            }
        }
    }

    /// Resize the window's buffer to `size`, preserving overlapping cells and
    /// clamping the cursor and scrolling region into range.
    pub fn resize(&mut self, size: Size) {
        self.buf.resize(size);
        let max_row = size.rows.saturating_sub(1);
        let max_col = size.cols.saturating_sub(1);
        self.cursor = Pos::new(self.cursor.row.min(max_row), self.cursor.col.min(max_col));
        self.scroll_top = self.scroll_top.min(max_row);
        self.scroll_bottom = max_row;
    }

    /// A blank cell carrying only the current background colour (so an erase
    /// keeps the window's background, as curses does).
    fn blank(&self) -> Cell {
        let mut attrs = Attributes::PLAIN;
        attrs.background = self.attrs.background;
        Cell::styled(' ', attrs)
    }

    /// Advance the cursor after writing a glyph: step right, or wrap to the
    /// next line at the right edge.
    fn advance_cursor(&mut self) {
        let cols = self.buf.size().cols;
        if self.cursor.col + 1 < cols {
            self.cursor.col += 1;
        } else {
            self.wrap_line();
        }
    }

    /// Move the cursor to the start of the next line, scrolling at the bottom
    /// of the region when scrolling is enabled.
    fn wrap_line(&mut self) {
        self.cursor.col = 0;
        if self.cursor.row < self.scroll_bottom {
            self.cursor.row += 1;
        } else if self.scrolling {
            self.scroll_up_once();
        } else if self.cursor.row + 1 < self.buf.size().rows {
            self.cursor.row += 1;
        }
        // At the bottom with scrolling off: the cursor stays put and further
        // output overwrites the last cell, exactly as curses does.
    }

    /// Move every row of the scrolling region up by one, blanking the bottom.
    fn scroll_up_once(&mut self) {
        let blank = self.blank();
        let cols = self.buf.size().cols;
        for row in self.scroll_top..self.scroll_bottom {
            for col in 0..cols {
                if let Some(cell) = self.buf.get(Pos::new(row + 1, col)) {
                    let _ = self.buf.set(Pos::new(row, col), cell);
                }
            }
        }
        for col in 0..cols {
            let _ = self.buf.set(Pos::new(self.scroll_bottom, col), blank);
        }
    }

    /// Move every row of the scrolling region down by one, blanking the top.
    fn scroll_down_once(&mut self) {
        let blank = self.blank();
        let cols = self.buf.size().cols;
        let mut row = self.scroll_bottom;
        while row > self.scroll_top {
            for col in 0..cols {
                if let Some(cell) = self.buf.get(Pos::new(row - 1, col)) {
                    let _ = self.buf.set(Pos::new(row, col), cell);
                }
            }
            row -= 1;
        }
        for col in 0..cols {
            let _ = self.buf.set(Pos::new(self.scroll_top, col), blank);
        }
    }
}
