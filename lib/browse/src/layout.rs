//! Pure geometry of the scrolling item views.
//!
//! [`ListView`] (a column of full-width rows) and [`GridView`] (a wrapped grid
//! of icon tiles) are the one definition of *where* each entry is drawn within
//! the content viewport and *which* entries are visible for a given scroll
//! offset — the single source both the renderer ([`render`](mod@crate::render))
//! and the pointer hit-test ([`entry_index_at`](crate::render::entry_index_at))
//! consume, so a click can never resolve to a different item than the one the
//! user saw (§2.2). [`ViewLayout`] is the dispatch that lets a caller treat the
//! two uniformly without branching on the browser's [`ViewMode`].
//!
//! Each view is a fixed-height header (the path bar) followed by the scrolling
//! item area. The scroll offset — a *desired first visible line*, in the view's
//! own line unit (list rows or grid rows) — is owned by the [`Browser`] and
//! clamped here through the shared [`ScrollRange`] geometry, so the browser and
//! every other viewport agree on the offset math; [`reveal`](ListView::reveal)
//! is the one rule that keeps the selection on screen.
//!
//! All arithmetic saturates and every accessor is total: a degenerate viewport
//! (too short for even one row, too narrow for even one tile, or a zero cell
//! size) simply has no visible items, never a panic.
//!
//! [`Browser`]: crate::Browser
//! [`ViewMode`]: crate::ViewMode

use tairix_controls::scroll::ScrollRange;
use tairix_geometry::Rect;

/// Which of the two item views the browser is showing.
///
/// The two views share one selection cursor, one scroll offset, and one
/// listing; only the geometry (a column of full-width rows vs. a wrapped grid
/// of tiles) differs. Switching mode is a pure toggle on the
/// [`Browser`](crate::Browser) — it never re-reads the directory or moves the
/// selection to a different entry.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash, Default)]
pub enum ViewMode {
    /// A vertical column of full-width rows (the default).
    #[default]
    List,
    /// A wrapped grid of icon tiles.
    Grid,
}

/// The layout of the scrolling entry list within a content viewport.
///
/// Constructed per paint/hit-test from the viewport dimensions, the row
/// height the caller renders with, the header height reserved for the path
/// bar, and the number of entries. It holds no selection or scroll state of
/// its own: the selection is passed to the accessors that need it, keeping the
/// browser the single owner of that cursor.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct ListView {
    viewport: Rect,
    row_height: u32,
    header_height: u32,
    entry_count: usize,
}

impl ListView {
    /// Lay a list of `entry_count` rows of `row_height` pixels out below a
    /// `header_height`-pixel header within `viewport`.
    #[must_use]
    pub const fn new(
        viewport: Rect,
        row_height: u32,
        header_height: u32,
        entry_count: usize,
    ) -> Self {
        Self {
            viewport,
            row_height,
            header_height,
            entry_count,
        }
    }

    /// The top of the entry list, in view-local pixels: the header height,
    /// clamped so it never exceeds the viewport.
    #[must_use]
    pub fn list_top(&self) -> u32 {
        self.header_height.min(self.viewport.height)
    }

    /// The height in pixels available to the entry list below the header.
    #[must_use]
    pub fn list_height(&self) -> u32 {
        self.viewport.height.saturating_sub(self.list_top())
    }

    /// How many entry rows fit in the list area at once.
    #[must_use]
    pub fn visible_rows(&self) -> usize {
        if self.row_height == 0 {
            return 0;
        }
        usize::try_from(self.list_height() / self.row_height).unwrap_or(usize::MAX)
    }

    /// The clamped scroll window for the desired first-visible row `offset`,
    /// expressed in row units: the content extent (total rows), the viewport
    /// extent (visible rows), and the offset. The clamp is the shared
    /// [`ScrollRange`] normalisation, so the offset can never exceed what the
    /// content allows.
    #[must_use]
    pub fn scroll_range(&self, offset: u64) -> ScrollRange {
        ScrollRange::new(
            u64::try_from(self.entry_count).unwrap_or(u64::MAX),
            u64::try_from(self.visible_rows()).unwrap_or(u64::MAX),
            offset,
        )
    }

    /// The index of the first entry drawn for the desired `offset` (the
    /// clamped scroll offset).
    #[must_use]
    pub fn first_visible(&self, offset: u64) -> usize {
        usize::try_from(self.scroll_range(offset).offset()).unwrap_or(usize::MAX)
    }

    /// The scroll offset that keeps `selected` visible while moving the least:
    /// the current `offset` when the selection is already on screen, otherwise
    /// the nearest offset that brings it to the top or bottom edge (clamped to
    /// the content).
    #[must_use]
    pub fn reveal(&self, offset: u64, selected: Option<usize>) -> u64 {
        reveal_line(
            self.scroll_range(offset).offset(),
            selected,
            self.visible_rows(),
        )
    }

    /// The rectangle the entry at `index` occupies for the desired scroll
    /// `offset`, or `None` when that entry is not currently visible (out of
    /// range, scrolled off, or the list area is too short for any row).
    #[must_use]
    pub fn row_rect(&self, offset: u64, index: usize) -> Option<Rect> {
        let visible = self.visible_rows();
        if visible == 0 || index >= self.entry_count {
            return None;
        }
        let row = index.checked_sub(self.first_visible(offset))?;
        if row >= visible {
            return None;
        }
        let step = u32::try_from(row).ok()?;
        let y = self
            .list_top()
            .checked_add(self.row_height.checked_mul(step)?)?;
        Some(Rect::new(
            self.viewport.origin.x,
            i32::try_from(y).unwrap_or(i32::MAX),
            self.viewport.width,
            self.row_height,
        ))
    }

    /// The index of the entry at view-local pixel `(x, y)` for the desired
    /// scroll `offset`, or `None` for the header, the empty space below the
    /// last entry, the scrollbar gutter (any `x` at or past the content
    /// width), and any coordinate outside the viewport.
    #[must_use]
    pub fn index_at(&self, offset: u64, x: u32, y: u32) -> Option<usize> {
        let visible = self.visible_rows();
        if self.row_height == 0 || visible == 0 || x >= self.viewport.width {
            return None;
        }
        let top = self.list_top();
        if y < top || y >= self.viewport.height {
            return None;
        }
        let row = usize::try_from((y - top) / self.row_height).unwrap_or(usize::MAX);
        if row >= visible {
            return None;
        }
        let index = self.first_visible(offset).checked_add(row)?;
        (index < self.entry_count).then_some(index)
    }
}

/// The scroll offset (in the view's line unit) that keeps line index
/// `selected` within a `visible`-line window while moving the least: the
/// current `offset` when the line is already on screen, the line itself when
/// it sits above the window, and the offset that puts it on the bottom edge
/// when it sits below. Shared by both views so the reveal rule is one
/// definition (§2.2).
fn reveal_line(offset: u64, selected: Option<usize>, visible: usize) -> u64 {
    let (Some(sel), true) = (selected, visible > 0) else {
        return offset;
    };
    let sel = u64::try_from(sel).unwrap_or(u64::MAX);
    let visible = u64::try_from(visible).unwrap_or(u64::MAX);
    if sel < offset {
        sel
    } else if sel >= offset.saturating_add(visible) {
        sel.saturating_add(1).saturating_sub(visible)
    } else {
        offset
    }
}

/// The layout of the wrapped icon grid within a content viewport.
///
/// Tiles of `cell_width`×`cell_height` pixels are laid out left-to-right,
/// wrapping into as many columns as fit the content width, below a
/// `header_height`-pixel header, separated by a uniform `gap` on both axes.
/// Like [`ListView`] it holds no selection or scroll state: the desired
/// scroll offset (in *grid-row* units) is passed to the accessors that need
/// it, so the browser stays the single owner of that cursor.
///
/// All arithmetic saturates and every accessor is total: a viewport too
/// narrow for even one column, or too short for even one row, simply has no
/// visible tiles rather than panicking.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct GridView {
    viewport: Rect,
    cell_width: u32,
    cell_height: u32,
    gap: u32,
    header_height: u32,
    entry_count: usize,
}

impl GridView {
    /// Lay `entry_count` tiles of `cell_width`×`cell_height` pixels out below
    /// a `header_height`-pixel header within `viewport`, separated by `gap`
    /// pixels on both axes.
    #[must_use]
    pub const fn new(
        viewport: Rect,
        cell_width: u32,
        cell_height: u32,
        gap: u32,
        header_height: u32,
        entry_count: usize,
    ) -> Self {
        Self {
            viewport,
            cell_width,
            cell_height,
            gap,
            header_height,
            entry_count,
        }
    }

    /// The top of the tile area, in view-local pixels.
    #[must_use]
    pub fn list_top(&self) -> u32 {
        self.header_height.min(self.viewport.height)
    }

    /// The height available to the tile area below the header.
    #[must_use]
    pub fn list_height(&self) -> u32 {
        self.viewport.height.saturating_sub(self.list_top())
    }

    /// The centre-to-centre pitch of a column (tile plus one gap).
    fn col_pitch(&self) -> u32 {
        self.cell_width.saturating_add(self.gap)
    }

    /// The centre-to-centre pitch of a grid row (tile plus one gap).
    fn row_pitch(&self) -> u32 {
        self.cell_height.saturating_add(self.gap)
    }

    /// How many tile columns fit the content width (at least one whenever a
    /// single tile fits, zero when not even one does).
    #[must_use]
    pub fn columns(&self) -> usize {
        if self.cell_width == 0 || self.viewport.width < self.cell_width {
            return 0;
        }
        // One tile always fits here; each further column costs one pitch.
        let extra = (self.viewport.width - self.cell_width) / self.col_pitch();
        usize::try_from(extra)
            .unwrap_or(usize::MAX)
            .saturating_add(1)
    }

    /// The total number of grid rows the entries occupy (ceiling division by
    /// the column count).
    #[must_use]
    pub fn rows_total(&self) -> usize {
        let columns = self.columns();
        if columns == 0 {
            return 0;
        }
        self.entry_count.div_ceil(columns)
    }

    /// How many whole grid rows fit in the tile area at once.
    #[must_use]
    pub fn visible_rows(&self) -> usize {
        if self.cell_height == 0 || self.columns() == 0 {
            return 0;
        }
        let height = self.list_height();
        if height < self.cell_height {
            return 0;
        }
        // One row always fits here; each further row costs one pitch.
        let extra = (height - self.cell_height) / self.row_pitch();
        usize::try_from(extra)
            .unwrap_or(usize::MAX)
            .saturating_add(1)
    }

    /// The clamped scroll window in grid-row units (content rows, visible
    /// rows, offset), through the same [`ScrollRange`] normalisation the list
    /// uses.
    #[must_use]
    pub fn scroll_range(&self, offset: u64) -> ScrollRange {
        ScrollRange::new(
            u64::try_from(self.rows_total()).unwrap_or(u64::MAX),
            u64::try_from(self.visible_rows()).unwrap_or(u64::MAX),
            offset,
        )
    }

    /// The first grid row drawn for the desired `offset` (the clamped offset).
    #[must_use]
    pub fn first_visible(&self, offset: u64) -> usize {
        usize::try_from(self.scroll_range(offset).offset()).unwrap_or(usize::MAX)
    }

    /// The scroll offset that keeps the tile at `selected` visible while
    /// moving the least, in grid-row units (the selection's row is
    /// `selected / columns`).
    #[must_use]
    pub fn reveal(&self, offset: u64, selected: Option<usize>) -> u64 {
        let columns = self.columns();
        let row = selected.and_then(|sel| (columns != 0).then_some(sel / columns));
        reveal_line(self.scroll_range(offset).offset(), row, self.visible_rows())
    }

    /// The rectangle the tile at `index` occupies for the desired scroll
    /// `offset`, or `None` when it is out of range or not currently visible.
    #[must_use]
    pub fn cell_rect(&self, offset: u64, index: usize) -> Option<Rect> {
        let columns = self.columns();
        let visible = self.visible_rows();
        if columns == 0 || visible == 0 || index >= self.entry_count {
            return None;
        }
        let row = index / columns;
        let col = index % columns;
        let screen_row = row.checked_sub(self.first_visible(offset))?;
        if screen_row >= visible {
            return None;
        }
        let y = self.list_top().checked_add(
            self.row_pitch()
                .checked_mul(u32::try_from(screen_row).ok()?)?,
        )?;
        let x = self.col_pitch().checked_mul(u32::try_from(col).ok()?)?;
        Some(Rect::new(
            self.viewport.origin.x.saturating_add_unsigned(x),
            i32::try_from(y).unwrap_or(i32::MAX),
            self.cell_width,
            self.cell_height,
        ))
    }

    /// The index of the tile at view-local pixel `(x, y)` for the desired
    /// scroll `offset`, or `None` for the header, a gap between tiles, the
    /// empty space past the last tile, and any coordinate outside the tile
    /// area.
    #[must_use]
    pub fn index_at(&self, offset: u64, x: u32, y: u32) -> Option<usize> {
        let columns = self.columns();
        let visible = self.visible_rows();
        if columns == 0 || visible == 0 {
            return None;
        }
        let top = self.list_top();
        if y < top || y >= self.viewport.height || x >= self.viewport.width {
            return None;
        }
        // Reject the inter-tile gaps so a click resolves only to a tile the
        // user actually saw, never the empty space between them.
        let col = column_at(x, self.col_pitch(), self.cell_width)?;
        if col >= columns {
            return None;
        }
        let screen_row = row_at(y - top, self.row_pitch(), self.cell_height)?;
        if screen_row >= visible {
            return None;
        }
        let row = self.first_visible(offset).checked_add(screen_row)?;
        let index = row.checked_mul(columns)?.checked_add(col)?;
        (index < self.entry_count).then_some(index)
    }
}

/// The tile column a within-area coordinate `pos` falls in, or `None` when it
/// lands in the gap after a tile. `pitch` is tile-plus-gap and `cell` the tile
/// size along that axis.
fn column_at(pos: u32, pitch: u32, cell: u32) -> Option<usize> {
    if pitch == 0 {
        return None;
    }
    let within = pos % pitch;
    if within >= cell {
        return None;
    }
    usize::try_from(pos / pitch).ok()
}

/// The tile row a within-area coordinate `pos` falls in, or `None` in a gap
/// (the vertical twin of [`column_at`]).
fn row_at(pos: u32, pitch: u32, cell: u32) -> Option<usize> {
    column_at(pos, pitch, cell)
}

/// One of the two item-view geometries, chosen by the browser's [`ViewMode`].
///
/// This is the single dispatch both the renderer and the pointer hit-test go
/// through, so the list and the grid expose one scrolling/hit-testing contract
/// and a caller never has to branch on the mode itself (§2.2).
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum ViewLayout {
    /// The full-width row list.
    List(ListView),
    /// The wrapped icon grid.
    Grid(GridView),
}

impl ViewLayout {
    /// The clamped scroll window for the desired `offset`, in the active
    /// view's line unit (list rows or grid rows).
    #[must_use]
    pub fn scroll_range(&self, offset: u64) -> ScrollRange {
        match self {
            Self::List(v) => v.scroll_range(offset),
            Self::Grid(v) => v.scroll_range(offset),
        }
    }

    /// The scroll offset that reveals `selected` while moving the least.
    #[must_use]
    pub fn reveal(&self, offset: u64, selected: Option<usize>) -> u64 {
        match self {
            Self::List(v) => v.reveal(offset, selected),
            Self::Grid(v) => v.reveal(offset, selected),
        }
    }

    /// The index of the item at view-local pixel `(x, y)` for the desired
    /// scroll `offset`.
    #[must_use]
    pub fn index_at(&self, offset: u64, x: u32, y: u32) -> Option<usize> {
        match self {
            Self::List(v) => v.index_at(offset, x, y),
            Self::Grid(v) => v.index_at(offset, x, y),
        }
    }

    /// How many whole lines (list rows or grid rows) are visible at once — the
    /// natural page step for wheel and scrollbar paging.
    #[must_use]
    pub fn visible_rows(&self) -> usize {
        match self {
            Self::List(v) => v.visible_rows(),
            Self::Grid(v) => v.visible_rows(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{GridView, ListView};
    use tairix_geometry::Rect;

    const ROW: u32 = 10;
    const WIDTH: u32 = 200;

    /// A view `rows` rows tall (plus the one-row header) holding `count`
    /// entries.
    fn view(rows: u32, count: usize) -> ListView {
        let height = ROW * (rows + 1);
        ListView::new(Rect::new(0, 0, WIDTH, height), ROW, ROW, count)
    }

    /// A full-width row rectangle at view-local top `y`.
    fn rect_at(y: u32) -> Rect {
        Rect::new(0, i32::try_from(y).unwrap(), WIDTH, ROW)
    }

    #[test]
    fn visible_rows_excludes_the_header() {
        assert_eq!(view(4, 10).visible_rows(), 4);
    }

    #[test]
    fn a_viewport_too_short_for_a_row_shows_nothing() {
        let short = ListView::new(Rect::new(0, 0, WIDTH, ROW), ROW, ROW, 10);
        assert_eq!(short.visible_rows(), 0);
        assert_eq!(short.row_rect(0, 0), None);
        assert_eq!(short.index_at(0, 0, ROW), None);
    }

    #[test]
    fn a_zero_row_height_shows_nothing_rather_than_dividing_by_zero() {
        let degenerate = ListView::new(Rect::new(0, 0, WIDTH, 100), 0, 0, 5);
        assert_eq!(degenerate.visible_rows(), 0);
        assert_eq!(degenerate.index_at(0, 0, 0), None);
    }

    #[test]
    fn rows_stack_below_the_header_when_everything_fits() {
        let v = view(4, 3);
        assert_eq!(v.first_visible(0), 0);
        assert_eq!(v.row_rect(0, 0), Some(rect_at(ROW)));
        assert_eq!(v.row_rect(0, 2), Some(rect_at(ROW * 3)));
        // The fourth entry does not exist.
        assert_eq!(v.row_rect(0, 3), None);
    }

    #[test]
    fn reveal_scrolls_the_window_to_keep_the_selection_visible() {
        let v = view(3, 10);
        // Revealing the seventh entry (index 6) with three visible rows
        // anchors the window to rows 4..=6.
        let offset = v.reveal(0, Some(6));
        assert_eq!(offset, 4);
        assert_eq!(v.first_visible(offset), 4);
        assert_eq!(v.row_rect(offset, 6), Some(rect_at(ROW * 3)));
        assert_eq!(v.row_rect(offset, 3), None);
        assert_eq!(v.row_rect(offset, 4), Some(rect_at(ROW)));
        // A selection already on screen does not move the window.
        assert_eq!(v.reveal(4, Some(5)), 4);
        // A selection above the window scrolls up to it exactly.
        assert_eq!(v.reveal(4, Some(2)), 2);
    }

    #[test]
    fn hit_test_mirrors_the_row_rects() {
        let v = view(3, 10);
        let offset = v.reveal(0, Some(6));
        // The header never resolves to an entry.
        assert_eq!(v.index_at(offset, 0, 0), None);
        assert_eq!(v.index_at(offset, 0, ROW - 1), None);
        // The three visible rows map to indices 4, 5, 6.
        assert_eq!(v.index_at(offset, 0, ROW), Some(4));
        assert_eq!(v.index_at(offset, 0, ROW * 2), Some(5));
        assert_eq!(v.index_at(offset, 0, ROW * 3), Some(6));
        // Below the last visible row is empty space.
        assert_eq!(v.index_at(offset, 0, ROW * 4), None);
        // A click in the scrollbar gutter (at or past the content width)
        // resolves to no row.
        assert_eq!(v.index_at(offset, WIDTH, ROW), None);
    }

    #[test]
    fn the_offset_is_clamped_to_the_content() {
        // A desired offset past the end cannot scroll the window beyond the
        // last full page (`ScrollRange` clamps `max_offset`).
        let v = view(3, 5);
        assert_eq!(v.scroll_range(4).offset(), 2);
        assert_eq!(v.first_visible(4), 2);
    }

    // --- The icon grid -------------------------------------------------

    const CELL: u32 = 40;
    const GAP: u32 = 10;

    /// A grid `cols` columns wide and `rows` rows tall (plus a one-`CELL`
    /// header) holding `count` tiles. Width fits exactly `cols` columns:
    /// `cols` tiles plus `cols - 1` gaps.
    fn grid(cols: u32, rows: u32, count: usize) -> GridView {
        let width = CELL * cols + GAP * cols.saturating_sub(1);
        let height = CELL + (CELL + GAP) * rows;
        GridView::new(Rect::new(0, 0, width, height), CELL, CELL, GAP, CELL, count)
    }

    #[test]
    fn grid_columns_and_rows_wrap_the_entries() {
        let g = grid(3, 2, 7);
        assert_eq!(g.columns(), 3);
        // Seven tiles across three columns need three rows (ceil).
        assert_eq!(g.rows_total(), 3);
        // The header plus two row pitches leaves room for two whole rows.
        assert_eq!(g.visible_rows(), 2);
    }

    #[test]
    fn a_grid_too_narrow_or_short_shows_nothing() {
        let narrow = GridView::new(Rect::new(0, 0, CELL - 1, 500), CELL, CELL, GAP, CELL, 5);
        assert_eq!(narrow.columns(), 0);
        assert_eq!(narrow.visible_rows(), 0);
        assert_eq!(narrow.cell_rect(0, 0), None);
        assert_eq!(narrow.index_at(0, 0, CELL), None);
        let short = GridView::new(Rect::new(0, 0, 500, CELL), CELL, CELL, GAP, CELL, 5);
        assert_eq!(short.visible_rows(), 0);
    }

    #[test]
    fn grid_tiles_lay_out_left_to_right_then_wrap() {
        let g = grid(3, 2, 7);
        // Row 0: tiles 0,1,2 at x = 0, 50, 100; y = header (CELL).
        assert_eq!(
            g.cell_rect(0, 0),
            Some(Rect::new(0, i32::try_from(CELL).unwrap(), CELL, CELL))
        );
        assert_eq!(
            g.cell_rect(0, 2),
            Some(Rect::new(
                i32::try_from((CELL + GAP) * 2).unwrap(),
                i32::try_from(CELL).unwrap(),
                CELL,
                CELL
            ))
        );
        // Tile 3 wraps to row 1 (x = 0, y = header + one pitch).
        assert_eq!(
            g.cell_rect(0, 3),
            Some(Rect::new(
                0,
                i32::try_from(CELL + CELL + GAP).unwrap(),
                CELL,
                CELL
            ))
        );
    }

    #[test]
    fn grid_hit_test_mirrors_the_tile_rects_and_rejects_gaps() {
        let g = grid(3, 2, 7);
        let header = CELL;
        // The centre of tile 0.
        assert_eq!(g.index_at(0, CELL / 2, header + CELL / 2), Some(0));
        // The centre of tile 4 (row 1, column 1).
        let x = (CELL + GAP) + CELL / 2;
        let y = header + (CELL + GAP) + CELL / 2;
        assert_eq!(g.index_at(0, x, y), Some(4));
        // The gap between column 0 and column 1 resolves to nothing.
        assert_eq!(g.index_at(0, CELL + GAP / 2, header + CELL / 2), None);
        // The header resolves to nothing.
        assert_eq!(g.index_at(0, CELL / 2, 0), None);
    }

    #[test]
    fn grid_reveal_scrolls_by_grid_rows() {
        // Three columns, one visible row, nine tiles → three grid rows.
        let g = GridView::new(
            Rect::new(0, 0, CELL * 3 + GAP * 2, CELL + CELL),
            CELL,
            CELL,
            GAP,
            CELL,
            9,
        );
        assert_eq!(g.columns(), 3);
        assert_eq!(g.visible_rows(), 1);
        // Tile 8 sits on grid row 2; revealing it scrolls to offset 2.
        assert_eq!(g.reveal(0, Some(8)), 2);
        // A tile already on the visible row does not move the window.
        assert_eq!(g.reveal(1, Some(4)), 1);
    }
}
