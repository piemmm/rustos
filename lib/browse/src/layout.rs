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
//! own line unit (list rows, or whichever axis the grid's [`GridFlow`] scrolls
//! along) — is owned by the [`Browser`] and clamped here through the shared
//! [`ScrollRange`] geometry, so the browser and every other viewport agree on
//! the offset math; [`reveal`](ListView::reveal) is the one rule that keeps the
//! selection on screen.
//!
//! The grid is deliberately not a *file manager* grid: a [`GridFlow`] chooses
//! whether tiles wrap along a row from the leading edge (the manager's
//! scrolling view) or down a column from the trailing edge (the desktop's icon
//! column, which grows a new column inward as it fills). Both are the same cell
//! maths and the same hit-test, so the desktop needs no second grid.
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

impl ViewMode {
    /// The other view — the mode the list/grid toggle switches to.
    #[must_use]
    pub const fn toggled(self) -> Self {
        match self {
            Self::List => Self::Grid,
            Self::Grid => Self::List,
        }
    }
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
            self.viewport.origin.y.saturating_add_unsigned(y),
            self.viewport.width,
            self.row_height,
        ))
    }

    /// The index of the entry at window-local pixel `(x, y)` for the desired
    /// scroll `offset`, or `None` for the header, the empty space below the
    /// last entry, the scrollbar gutter (any `x` at or past the content
    /// width), and any coordinate outside the viewport.
    ///
    /// The point is taken in the same space [`Self::row_rect`] returns its
    /// rectangles in, so a viewport placed at a non-zero origin (the item area
    /// inset by the places rail) hit-tests exactly where it paints.
    #[must_use]
    pub fn index_at(&self, offset: u64, x: u32, y: u32) -> Option<usize> {
        let visible = self.visible_rows();
        let (x, y) = view_local(self.viewport, x, y)?;
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

/// The order a [`GridView`] fills its tiles in, and the edge its first tile
/// is anchored to.
///
/// The two icon views in this system differ *only* in this: the file
/// manager's grid reads like text and scrolls vertically, while the desktop's
/// column hugs the screen's trailing edge and grows a new column inward. Both
/// therefore share one set of cell maths and one hit-test, parameterised
/// here, rather than a second grid written beside the first.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash, Default)]
pub enum GridFlow {
    /// Fill a row left-to-right from the leading edge, then wrap down onto
    /// the next row; the scroll axis is vertical, counted in rows. The file
    /// manager's grid.
    #[default]
    RowsFromLeading,
    /// Fill a column top-to-bottom, then start a new column one pitch
    /// *inward* from the trailing edge; the scroll axis is horizontal,
    /// counted in columns. The desktop's icon column.
    ColumnsFromTrailing,
}

impl GridFlow {
    /// Whether tiles wrap down a column (rather than along a row) — the one
    /// place the two flows' axis assignment is decided.
    const fn wraps_down_a_column(self) -> bool {
        matches!(self, Self::ColumnsFromTrailing)
    }
}

/// The layout of the wrapped icon grid within a content viewport.
///
/// Tiles of `cell_width`×`cell_height` pixels are laid out below a
/// `header_height`-pixel header, separated by a uniform `gap` on both axes,
/// wrapping into as many *lines* as the viewport holds. A [`GridFlow`] picks
/// which axis a line runs along and which edge the first line is anchored to,
/// so the file manager's scrolling row-major grid and the desktop's
/// trailing-edge column-major one are the same geometry with one parameter
/// changed. Like [`ListView`] it holds no selection or scroll state: the
/// desired scroll offset (in *line* units) is passed to the accessors that
/// need it, so the view's owner stays the single owner of that cursor.
///
/// All arithmetic saturates and every accessor is total: a viewport too small
/// for even one tile simply has no visible tiles rather than panicking.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct GridView {
    viewport: Rect,
    cell_width: u32,
    cell_height: u32,
    gap: u32,
    header_height: u32,
    entry_count: usize,
    flow: GridFlow,
}

impl GridView {
    /// Lay `entry_count` tiles of `cell_width`×`cell_height` pixels out below
    /// a `header_height`-pixel header within `viewport`, separated by `gap`
    /// pixels on both axes, flowing as `flow` describes.
    #[must_use]
    pub const fn new(
        viewport: Rect,
        cell_width: u32,
        cell_height: u32,
        gap: u32,
        header_height: u32,
        entry_count: usize,
        flow: GridFlow,
    ) -> Self {
        Self {
            viewport,
            cell_width,
            cell_height,
            gap,
            header_height,
            entry_count,
            flow,
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

    /// How many whole tiles fit an `extent`-pixel run at `cell`-pixel tiles on
    /// a `pitch`-pixel stride: zero when not even one fits, otherwise one plus
    /// however many further pitches the remainder holds.
    fn tiles_in(extent: u32, cell: u32, pitch: u32) -> usize {
        if cell == 0 || pitch == 0 || extent < cell {
            return 0;
        }
        usize::try_from((extent - cell) / pitch)
            .unwrap_or(usize::MAX)
            .saturating_add(1)
    }

    /// How many tiles one line holds — tiles down a column for the desktop's
    /// flow, tiles across a row for the file manager's.
    #[must_use]
    pub fn cells_per_line(&self) -> usize {
        if self.flow.wraps_down_a_column() {
            Self::tiles_in(self.list_height(), self.cell_height, self.row_pitch())
        } else {
            Self::tiles_in(self.viewport.width, self.cell_width, self.col_pitch())
        }
    }

    /// The total number of lines the entries occupy (ceiling division by the
    /// tiles one line holds).
    #[must_use]
    pub fn lines_total(&self) -> usize {
        let per_line = self.cells_per_line();
        if per_line == 0 {
            return 0;
        }
        self.entry_count.div_ceil(per_line)
    }

    /// How many whole lines fit in the tile area at once — grid rows down the
    /// viewport for the file manager's flow, icon columns across it for the
    /// desktop's.
    #[must_use]
    pub fn visible_lines(&self) -> usize {
        if self.cells_per_line() == 0 {
            return 0;
        }
        if self.flow.wraps_down_a_column() {
            Self::tiles_in(self.viewport.width, self.cell_width, self.col_pitch())
        } else {
            Self::tiles_in(self.list_height(), self.cell_height, self.row_pitch())
        }
    }

    /// The clamped scroll window in line units (content lines, visible lines,
    /// offset), through the same [`ScrollRange`] normalisation the list uses.
    #[must_use]
    pub fn scroll_range(&self, offset: u64) -> ScrollRange {
        ScrollRange::new(
            u64::try_from(self.lines_total()).unwrap_or(u64::MAX),
            u64::try_from(self.visible_lines()).unwrap_or(u64::MAX),
            offset,
        )
    }

    /// The first line drawn for the desired `offset` (the clamped offset).
    #[must_use]
    pub fn first_visible(&self, offset: u64) -> usize {
        usize::try_from(self.scroll_range(offset).offset()).unwrap_or(usize::MAX)
    }

    /// The half-open range of entry indices currently drawn for the desired
    /// scroll `offset` — the one definition a renderer iterates, so it never
    /// re-derives the wrap arithmetic and can never disagree with
    /// [`cell_rect`](Self::cell_rect) about which tiles are on screen.
    #[must_use]
    pub fn visible_range(&self, offset: u64) -> core::ops::Range<usize> {
        let per_line = self.cells_per_line();
        let visible = self.visible_lines();
        if per_line == 0 || visible == 0 {
            return 0..0;
        }
        let start = self.first_visible(offset).saturating_mul(per_line);
        let end = self
            .first_visible(offset)
            .saturating_add(visible)
            .saturating_mul(per_line)
            .min(self.entry_count);
        start..end.max(start)
    }

    /// The scroll offset that keeps the tile at `selected` visible while
    /// moving the least, in line units (the selection's line is
    /// `selected / cells_per_line`).
    #[must_use]
    pub fn reveal(&self, offset: u64, selected: Option<usize>) -> u64 {
        let per_line = self.cells_per_line();
        let line = selected.and_then(|sel| (per_line != 0).then_some(sel / per_line));
        reveal_line(
            self.scroll_range(offset).offset(),
            line,
            self.visible_lines(),
        )
    }

    /// The view-local pixel offsets of the tile at line `line` (already
    /// screen-relative) and slot `slot` within it: `(x, y)` from the
    /// viewport's origin. The single place the two flows' anchoring differs —
    /// the trailing-edge flow measures `x` inward from the viewport's right
    /// edge, so its first column hugs the screen edge whatever the width is.
    fn tile_offsets(&self, line: usize, slot: usize) -> Option<(u32, u32)> {
        let along = self.row_pitch().checked_mul(
            u32::try_from(if self.flow.wraps_down_a_column() {
                slot
            } else {
                line
            })
            .ok()?,
        )?;
        let across = self.col_pitch().checked_mul(
            u32::try_from(if self.flow.wraps_down_a_column() {
                line
            } else {
                slot
            })
            .ok()?,
        )?;
        let y = self.list_top().checked_add(along)?;
        let x = if self.flow.wraps_down_a_column() {
            self.viewport
                .width
                .checked_sub(across.checked_add(self.cell_width)?)?
        } else {
            across
        };
        Some((x, y))
    }

    /// The rectangle the tile at `index` occupies for the desired scroll
    /// `offset`, or `None` when it is out of range or not currently visible.
    #[must_use]
    pub fn cell_rect(&self, offset: u64, index: usize) -> Option<Rect> {
        let per_line = self.cells_per_line();
        let visible = self.visible_lines();
        if per_line == 0 || visible == 0 || index >= self.entry_count {
            return None;
        }
        let screen_line = (index / per_line).checked_sub(self.first_visible(offset))?;
        if screen_line >= visible {
            return None;
        }
        let (x, y) = self.tile_offsets(screen_line, index % per_line)?;
        Some(Rect::new(
            self.viewport.origin.x.saturating_add_unsigned(x),
            self.viewport.origin.y.saturating_add_unsigned(y),
            self.cell_width,
            self.cell_height,
        ))
    }

    /// The index of the tile at window-local pixel `(x, y)` for the desired
    /// scroll `offset`, or `None` for the header, a gap between tiles, the
    /// empty space past the last tile, and any coordinate outside the tile
    /// area.
    ///
    /// The point is taken in the same space [`Self::cell_rect`] returns its
    /// rectangles in, so a viewport placed at a non-zero origin (the item area
    /// inset by the places rail) hit-tests exactly where it paints.
    #[must_use]
    pub fn index_at(&self, offset: u64, x: u32, y: u32) -> Option<usize> {
        let per_line = self.cells_per_line();
        let visible = self.visible_lines();
        if per_line == 0 || visible == 0 {
            return None;
        }
        let (x, y) = view_local(self.viewport, x, y)?;
        let top = self.list_top();
        if y < top || y >= self.viewport.height || x >= self.viewport.width {
            return None;
        }
        // Reject the inter-tile gaps so a click resolves only to a tile the
        // user actually saw, never the empty space between them. The
        // trailing-edge flow measures its columns inward from the right edge,
        // exactly as it paints them.
        let from_edge = if self.flow.wraps_down_a_column() {
            self.viewport.width.checked_sub(x)?.checked_sub(1)?
        } else {
            x
        };
        let col = tile_at(from_edge, self.col_pitch(), self.cell_width)?;
        let row = tile_at(y - top, self.row_pitch(), self.cell_height)?;
        let (line, slot) = if self.flow.wraps_down_a_column() {
            (col, row)
        } else {
            (row, col)
        };
        if line >= visible || slot >= per_line {
            return None;
        }
        let index = self
            .first_visible(offset)
            .checked_add(line)?
            .checked_mul(per_line)?
            .checked_add(slot)?;
        (index < self.entry_count).then_some(index)
    }
}

/// The tile a within-area coordinate `pos` falls in along one axis, or `None`
/// when it lands in the gap after a tile. `pitch` is tile-plus-gap and `cell`
/// the tile size along that axis; the rule is identical on both axes, so it
/// has one definition.
fn tile_at(pos: u32, pitch: u32, cell: u32) -> Option<usize> {
    if pitch == 0 {
        return None;
    }
    let within = pos % pitch;
    if within >= cell {
        return None;
    }
    usize::try_from(pos / pitch).ok()
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

    /// The view-local pixel rectangle of the item at `index` for the desired
    /// scroll `offset`, or `None` when the item is out of range or scrolled
    /// out of the visible window. The exact inverse of
    /// [`index_at`](Self::index_at) — the same rect the renderer draws that
    /// item in — so an overlay (the in-place rename editor) sits precisely over
    /// the item the user selected (§2.2).
    #[must_use]
    pub fn item_rect(&self, offset: u64, index: usize) -> Option<Rect> {
        match self {
            Self::List(v) => v.row_rect(offset, index),
            Self::Grid(v) => v.cell_rect(offset, index),
        }
    }

    /// How many whole lines (list rows, or the grid's own line unit) are
    /// visible at once — the natural page step for wheel and scrollbar paging.
    #[must_use]
    pub fn visible_rows(&self) -> usize {
        match self {
            Self::List(v) => v.visible_rows(),
            Self::Grid(v) => v.visible_lines(),
        }
    }
}

/// Convert the window-local pixel `(x, y)` into `viewport`'s own coordinate
/// space, or `None` when the point lies above or to the left of it.
///
/// Every view here places its rectangles at the viewport's origin, so a
/// hit-test must remove that origin before it can invert the placement. One
/// definition, shared by all three views, so a point can never mean one thing
/// to the painter and another to the hit-test.
fn view_local(viewport: Rect, x: u32, y: u32) -> Option<(u32, u32)> {
    let origin_x = u32::try_from(viewport.origin.x).ok()?;
    let origin_y = u32::try_from(viewport.origin.y).ok()?;
    Some((x.checked_sub(origin_x)?, y.checked_sub(origin_y)?))
}

/// Pure geometry of the places rail: the fixed-width column of shortcut rows
/// down the window's leading edge, and the hit-test that inverts it.
///
/// The rail does not scroll — it holds a handful of rows, not a listing — so
/// its geometry is a simple stack: equal-height rows from the top, with one
/// separator band inserted where the mounted volumes begin. A window too short
/// for every row simply draws the ones that fit, and both
/// [`row_rect`](Self::row_rect) and [`index_at`](Self::index_at) agree about
/// which those are, so a click can never land on a row the user could not see.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct SidebarView {
    viewport: Rect,
    width: u32,
    row_height: u32,
    separator_height: u32,
    row_count: usize,
    volume_start: Option<usize>,
}

impl SidebarView {
    /// Lay `row_count` rows of `row_height` pixels out down the leading
    /// `width` pixels of `viewport`, with a `separator_height` band before the
    /// row at `volume_start` (the first mounted volume; `None` when nothing is
    /// mounted and there is nothing to separate).
    #[must_use]
    pub const fn new(
        viewport: Rect,
        width: u32,
        row_height: u32,
        separator_height: u32,
        row_count: usize,
        volume_start: Option<usize>,
    ) -> Self {
        Self {
            viewport,
            width,
            row_height,
            separator_height,
            row_count,
            volume_start,
        }
    }

    /// The rail's drawn width: the requested width, clamped so a window
    /// narrower than the rail is filled rather than overrun.
    #[must_use]
    pub fn width(&self) -> u32 {
        self.width.min(self.viewport.width)
    }

    /// The whole rail's rectangle — the band the content area is inset by.
    #[must_use]
    pub fn rail_rect(&self) -> Rect {
        Rect::new(
            self.viewport.origin.x,
            self.viewport.origin.y,
            self.width(),
            self.viewport.height,
        )
    }

    /// The top of row `index` in rail-local pixels, including the separator
    /// band once the volumes begin.
    fn row_top(&self, index: usize) -> Option<u32> {
        let step = u32::try_from(index).ok()?;
        let base = self.row_height.checked_mul(step)?;
        match self.volume_start {
            Some(first) if index >= first => base.checked_add(self.separator_height),
            _ => Some(base),
        }
    }

    /// The rectangle row `index` occupies, or `None` when there is no such row
    /// or the window is too short to draw it in full.
    #[must_use]
    pub fn row_rect(&self, index: usize) -> Option<Rect> {
        if self.row_height == 0 || index >= self.row_count {
            return None;
        }
        let top = self.row_top(index)?;
        if top.checked_add(self.row_height)? > self.viewport.height {
            return None;
        }
        Some(Rect::new(
            self.viewport.origin.x,
            self.viewport.origin.y.saturating_add_unsigned(top),
            self.width(),
            self.row_height,
        ))
    }

    /// The separator band between the user's own places and the mounted
    /// volumes, or `None` when nothing is mounted or the band does not fit.
    #[must_use]
    pub fn separator_rect(&self) -> Option<Rect> {
        let first = self.volume_start?;
        if self.separator_height == 0 || first >= self.row_count {
            return None;
        }
        let top = self.row_height.checked_mul(u32::try_from(first).ok()?)?;
        if top.checked_add(self.separator_height)? > self.viewport.height {
            return None;
        }
        Some(Rect::new(
            self.viewport.origin.x,
            self.viewport.origin.y.saturating_add_unsigned(top),
            self.width(),
            self.separator_height,
        ))
    }

    /// The row at window-local pixel `(x, y)`, or `None` for a point outside
    /// the rail, inside the separator band, or below the last drawn row.
    ///
    /// The exact inverse of [`row_rect`](Self::row_rect): a point resolves to a
    /// row only when that row's own rectangle would contain it, so the drawn
    /// rail and the hit-test cannot disagree.
    #[must_use]
    pub fn index_at(&self, x: u32, y: u32) -> Option<usize> {
        if self.row_height == 0 {
            return None;
        }
        let (local_x, local_y) = view_local(self.viewport, x, y)?;
        if local_x >= self.width() || local_y >= self.viewport.height {
            return None;
        }
        let index = match self.volume_start {
            Some(first) => {
                let split = self.row_height.checked_mul(u32::try_from(first).ok()?)?;
                if local_y < split {
                    usize::try_from(local_y / self.row_height).ok()?
                } else {
                    // Subtracting the band yields nothing for a point inside
                    // it, so the separation itself is never a row.
                    let below = local_y.checked_sub(split.checked_add(self.separator_height)?)?;
                    first.checked_add(usize::try_from(below / self.row_height).ok()?)?
                }
            }
            None => usize::try_from(local_y / self.row_height).ok()?,
        };
        self.row_rect(index).is_some().then_some(index)
    }
}

#[cfg(test)]
mod tests {
    use super::{GridFlow, GridView, ListView};
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
    /// header) holding `count` tiles, flowing as the file manager's does.
    /// Width fits exactly `cols` columns: `cols` tiles plus `cols - 1` gaps.
    fn grid(cols: u32, rows: u32, count: usize) -> GridView {
        grid_flowing(cols, rows, count, GridFlow::RowsFromLeading)
    }

    /// The same grid under an explicit `flow`.
    fn grid_flowing(cols: u32, rows: u32, count: usize, flow: GridFlow) -> GridView {
        let width = CELL * cols + GAP * cols.saturating_sub(1);
        let height = CELL + (CELL + GAP) * rows;
        GridView::new(
            Rect::new(0, 0, width, height),
            CELL,
            CELL,
            GAP,
            CELL,
            count,
            flow,
        )
    }

    #[test]
    fn grid_columns_and_rows_wrap_the_entries() {
        let g = grid(3, 2, 7);
        assert_eq!(g.cells_per_line(), 3);
        // Seven tiles across three columns need three rows (ceil).
        assert_eq!(g.lines_total(), 3);
        // The header plus two row pitches leaves room for two whole rows.
        assert_eq!(g.visible_lines(), 2);
    }

    #[test]
    fn a_grid_too_narrow_or_short_shows_nothing() {
        let narrow = GridView::new(
            Rect::new(0, 0, CELL - 1, 500),
            CELL,
            CELL,
            GAP,
            CELL,
            5,
            GridFlow::RowsFromLeading,
        );
        assert_eq!(narrow.cells_per_line(), 0);
        assert_eq!(narrow.visible_lines(), 0);
        assert_eq!(narrow.cell_rect(0, 0), None);
        assert_eq!(narrow.index_at(0, 0, CELL), None);
        assert_eq!(narrow.visible_range(0), 0..0);
        let short = GridView::new(
            Rect::new(0, 0, 500, CELL),
            CELL,
            CELL,
            GAP,
            CELL,
            5,
            GridFlow::RowsFromLeading,
        );
        assert_eq!(short.visible_lines(), 0);
    }

    #[test]
    fn a_desktop_grid_too_narrow_or_short_shows_nothing() {
        let narrow = grid_flowing(0, 3, 5, GridFlow::ColumnsFromTrailing);
        assert_eq!(narrow.visible_lines(), 0);
        assert_eq!(narrow.cell_rect(0, 0), None);
        assert_eq!(narrow.index_at(0, 0, CELL), None);
        let short = GridView::new(
            Rect::new(0, 0, 500, CELL),
            CELL,
            CELL,
            GAP,
            CELL,
            5,
            GridFlow::ColumnsFromTrailing,
        );
        assert_eq!(short.cells_per_line(), 0);
        assert_eq!(short.visible_range(0), 0..0);
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
            GridFlow::RowsFromLeading,
        );
        assert_eq!(g.cells_per_line(), 3);
        assert_eq!(g.visible_lines(), 1);
        // Tile 8 sits on grid row 2; revealing it scrolls to offset 2.
        assert_eq!(g.reveal(0, Some(8)), 2);
        // A tile already on the visible row does not move the window.
        assert_eq!(g.reveal(1, Some(4)), 1);
    }

    #[test]
    fn the_visible_range_is_exactly_the_tiles_with_rects() {
        for flow in [GridFlow::RowsFromLeading, GridFlow::ColumnsFromTrailing] {
            let g = grid_flowing(3, 2, 7, flow);
            for offset in 0..3 {
                let range = g.visible_range(offset);
                for index in 0..7 {
                    assert_eq!(
                        g.cell_rect(offset, index).is_some(),
                        range.contains(&index),
                        "{flow:?} offset {offset} index {index}"
                    );
                }
            }
        }
    }

    // --- The desktop's trailing-edge icon column ------------------------

    #[test]
    fn the_desktop_column_fills_downward_from_the_trailing_edge() {
        // Three columns' worth of width, two tiles per column, five icons.
        let g = grid_flowing(3, 2, 5, GridFlow::ColumnsFromTrailing);
        assert_eq!(g.cells_per_line(), 2, "two icons fit down one column");
        assert_eq!(g.visible_lines(), 3, "three columns fit across");
        assert_eq!(g.lines_total(), 3, "five icons need three columns");
        let width = CELL * 3 + GAP * 2;
        let right = i32::try_from(width - CELL).unwrap();
        let header = i32::try_from(CELL).unwrap();
        // The first icon hugs the trailing edge, below the header.
        assert_eq!(
            g.cell_rect(0, 0),
            Some(Rect::new(right, header, CELL, CELL))
        );
        // The second falls directly beneath it, in the same column.
        assert_eq!(
            g.cell_rect(0, 1),
            Some(Rect::new(
                right,
                header + i32::try_from(CELL + GAP).unwrap(),
                CELL,
                CELL
            ))
        );
        // The third starts a new column one pitch further inward.
        assert_eq!(
            g.cell_rect(0, 2),
            Some(Rect::new(
                right - i32::try_from(CELL + GAP).unwrap(),
                header,
                CELL,
                CELL
            ))
        );
    }

    #[test]
    fn the_desktop_hit_test_mirrors_its_tile_rects_and_rejects_gaps() {
        let g = grid_flowing(3, 2, 5, GridFlow::ColumnsFromTrailing);
        for index in 0..5 {
            let rect = g.cell_rect(0, index).expect("every icon is on screen");
            let x = u32::try_from(rect.origin.x).unwrap() + CELL / 2;
            let y = u32::try_from(rect.origin.y).unwrap() + CELL / 2;
            assert_eq!(g.index_at(0, x, y), Some(index));
        }
        let width = CELL * 3 + GAP * 2;
        // The gap between the trailing column and the one inside it.
        assert_eq!(g.index_at(0, width - CELL - GAP / 2, CELL + CELL / 2), None);
        // The header band above the first icon.
        assert_eq!(g.index_at(0, width - CELL / 2, 0), None);
        // The empty slot past the last icon (column 2 holds only icon 4).
        assert_eq!(g.index_at(0, CELL / 2, CELL + CELL + GAP + CELL / 2), None);
    }
}
