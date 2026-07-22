//! Pure geometry of the scrolling item view.
//!
//! [`ListView`] is the one definition of *where* each entry row is drawn
//! within the content viewport and *which* rows are visible for a given
//! selection — the single source both the renderer ([`render`](mod@crate::render))
//! and the pointer hit-test ([`entry_index_at`](crate::render::entry_index_at))
//! consume, so a click can never resolve to a different row than the one the
//! user saw (§2.2).
//!
//! The view is a fixed-height header row (the path bar) followed by a column
//! of fixed-height entry rows. When there are more entries than fit, the list
//! scrolls so the selected entry stays visible. The scroll window is clamped
//! through the shared [`ScrollRange`] geometry rather than a re-derived
//! anchor, so the browser and every other viewport agree on the offset math.
//!
//! All arithmetic saturates and every accessor is total: a degenerate
//! viewport (too short for even one row, or a zero row height) simply has no
//! visible rows, never a panic.

use tairix_controls::scroll::ScrollRange;
use tairix_geometry::Rect;

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

    /// The clamped scroll window for `selected`, expressed in row units: the
    /// content extent (total rows), the viewport extent (visible rows), and
    /// the offset (the first visible row) chosen to keep the selection on
    /// screen. The clamp is the shared [`ScrollRange`] normalisation, so the
    /// offset can never exceed what the content allows.
    #[must_use]
    pub fn scroll_range(&self, selected: Option<usize>) -> ScrollRange {
        let visible = self.visible_rows();
        let desired = desired_first(selected, visible);
        ScrollRange::new(
            u64::try_from(self.entry_count).unwrap_or(u64::MAX),
            u64::try_from(visible).unwrap_or(u64::MAX),
            u64::try_from(desired).unwrap_or(u64::MAX),
        )
    }

    /// The index of the first entry drawn for `selected` (the clamped scroll
    /// offset).
    #[must_use]
    pub fn first_visible(&self, selected: Option<usize>) -> usize {
        usize::try_from(self.scroll_range(selected).offset()).unwrap_or(usize::MAX)
    }

    /// The rectangle the entry at `index` occupies for the given `selected`
    /// scroll window, or `None` when that entry is not currently visible (out
    /// of range, scrolled off, or the list area is too short for any row).
    #[must_use]
    pub fn row_rect(&self, selected: Option<usize>, index: usize) -> Option<Rect> {
        let visible = self.visible_rows();
        if visible == 0 || index >= self.entry_count {
            return None;
        }
        let offset = index.checked_sub(self.first_visible(selected))?;
        if offset >= visible {
            return None;
        }
        let step = u32::try_from(offset).ok()?;
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

    /// The index of the entry drawn at view-local pixel row `y` for the given
    /// `selected` scroll window, or `None` for the header, the empty space
    /// below the last entry, and any `y` outside the viewport.
    #[must_use]
    pub fn row_index_at(&self, selected: Option<usize>, y: u32) -> Option<usize> {
        let visible = self.visible_rows();
        if self.row_height == 0 || visible == 0 {
            return None;
        }
        let top = self.list_top();
        if y < top || y >= self.viewport.height {
            return None;
        }
        let offset = usize::try_from((y - top) / self.row_height).unwrap_or(usize::MAX);
        if offset >= visible {
            return None;
        }
        let index = self.first_visible(selected).checked_add(offset)?;
        (index < self.entry_count).then_some(index)
    }
}

/// The first row to show so that `selected` falls within a `visible_rows`-row
/// window: anchor the selection to the bottom of the window once it scrolls
/// past, and to the top otherwise. The result is the *desired* offset, which
/// [`ListView::scroll_range`] then clamps to what the content allows.
fn desired_first(selected: Option<usize>, visible_rows: usize) -> usize {
    match selected {
        Some(sel) if visible_rows > 0 && sel >= visible_rows => sel + 1 - visible_rows,
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::ListView;
    use tairix_geometry::Rect;

    const ROW: u32 = 10;

    /// A view `rows` rows tall (plus the one-row header) holding `count`
    /// entries.
    fn view(rows: u32, count: usize) -> ListView {
        let height = ROW * (rows + 1);
        ListView::new(Rect::new(0, 0, 200, height), ROW, ROW, count)
    }

    /// A full-width row rectangle at view-local top `y`.
    fn rect_at(y: u32) -> Rect {
        Rect::new(0, i32::try_from(y).unwrap(), 200, ROW)
    }

    #[test]
    fn visible_rows_excludes_the_header() {
        assert_eq!(view(4, 10).visible_rows(), 4);
    }

    #[test]
    fn a_viewport_too_short_for_a_row_shows_nothing() {
        let short = ListView::new(Rect::new(0, 0, 200, ROW), ROW, ROW, 10);
        assert_eq!(short.visible_rows(), 0);
        assert_eq!(short.row_rect(Some(0), 0), None);
        assert_eq!(short.row_index_at(Some(0), ROW), None);
    }

    #[test]
    fn a_zero_row_height_shows_nothing_rather_than_dividing_by_zero() {
        let degenerate = ListView::new(Rect::new(0, 0, 200, 100), 0, 0, 5);
        assert_eq!(degenerate.visible_rows(), 0);
        assert_eq!(degenerate.row_index_at(Some(0), 0), None);
    }

    #[test]
    fn rows_stack_below_the_header_when_everything_fits() {
        let v = view(4, 3);
        assert_eq!(v.first_visible(Some(0)), 0);
        assert_eq!(v.row_rect(Some(0), 0), Some(rect_at(ROW)));
        assert_eq!(v.row_rect(Some(0), 2), Some(rect_at(ROW * 3)));
        // The fourth entry does not exist.
        assert_eq!(v.row_rect(Some(0), 3), None);
    }

    #[test]
    fn the_window_scrolls_to_keep_the_selection_visible() {
        let v = view(3, 10);
        // Selecting the seventh entry (index 6) with three visible rows
        // anchors the window to rows 4..=6.
        assert_eq!(v.first_visible(Some(6)), 4);
        assert_eq!(v.row_rect(Some(6), 6), Some(rect_at(ROW * 3)));
        assert_eq!(v.row_rect(Some(6), 3), None);
        assert_eq!(v.row_rect(Some(6), 4), Some(rect_at(ROW)));
    }

    #[test]
    fn hit_test_mirrors_the_row_rects() {
        let v = view(3, 10);
        // The header never resolves to an entry.
        assert_eq!(v.row_index_at(Some(6), 0), None);
        assert_eq!(v.row_index_at(Some(6), ROW - 1), None);
        // The three visible rows map to indices 4, 5, 6.
        assert_eq!(v.row_index_at(Some(6), ROW), Some(4));
        assert_eq!(v.row_index_at(Some(6), ROW * 2), Some(5));
        assert_eq!(v.row_index_at(Some(6), ROW * 3), Some(6));
        // Below the last visible row is empty space.
        assert_eq!(v.row_index_at(Some(6), ROW * 4), None);
    }

    #[test]
    fn the_offset_is_clamped_to_the_content() {
        // A selection past the end cannot scroll the window beyond the last
        // full page (`ScrollRange` clamps `max_offset`).
        let v = view(3, 5);
        assert_eq!(v.scroll_range(Some(4)).offset(), 2);
        assert_eq!(v.first_visible(Some(4)), 2);
    }
}
