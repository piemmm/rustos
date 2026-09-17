//! A list of rows, which row is current, and where it is scrolled to.
//!
//! Every scrolling row list in this engine wants the same four things: a
//! cursor that clamps to the rows that exist, an offset that clamps to what
//! the list holds, the least scroll that brings the cursor back into view, and
//! a drawn bar carrying its own hover and drag state. [`RowList`] is that
//! model once — the *Open With…* chooser and the Properties window's
//! attribute list both hold one — so a keyboard traversal reveals the same way
//! in both and neither re-derives the clamp.
//!
//! It is a pure model over the shared [`ScrollRange`] normalisation: it reads
//! no rows, owns no content, and performs nothing. The `visible` row count is
//! passed in rather than stored, because it is a property of the surface the
//! list is drawn on and changes with every resize.

use tairix_controls::scroll::{ScrollModel, ScrollOrientation, ScrollRange};
use tairix_controls::ScrollBar;

/// A cursor and scroll offset over `len` rows, with the bar that draws them.
#[derive(Clone, Debug)]
pub struct RowList {
    len: usize,
    cursor: usize,
    offset: u64,
    bar: ScrollBar,
}

impl RowList {
    /// A list of `len` rows, scrolled to the top with the first row current.
    #[must_use]
    pub fn new(len: usize) -> Self {
        Self {
            len,
            cursor: 0,
            offset: 0,
            bar: ScrollBar::new(
                ScrollOrientation::Vertical,
                ScrollModel::new(ScrollRange::EMPTY, 1, 1),
            ),
        }
    }

    /// How many rows the list holds.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.len
    }

    /// Whether the list holds no rows at all.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Adopt a refreshed row count, keeping the cursor and offset where they
    /// are wherever the new set still reaches them.
    ///
    /// A set that shrank past either is clamped rather than left pointing off
    /// the end: a removed row must not leave the cursor naming a row that is
    /// now somebody else's.
    pub fn resize(&mut self, len: usize) {
        self.len = len;
        self.cursor = self.cursor.min(len.saturating_sub(1));
        self.offset = self.offset.min(u64::try_from(len).unwrap_or(u64::MAX));
    }

    /// Which row is current.
    #[must_use]
    pub const fn cursor(&self) -> usize {
        self.cursor
    }

    /// Make `index` current, clamped to the rows that exist, reporting whether
    /// the cursor moved.
    pub fn select(&mut self, index: usize) -> bool {
        let clamped = index.min(self.len.saturating_sub(1));
        let moved = clamped != self.cursor;
        self.cursor = clamped;
        moved
    }

    /// Move the cursor by `delta` rows (positive moves toward the end),
    /// stopping at either end, reporting whether it moved.
    pub fn step(&mut self, delta: i64) -> bool {
        let from = i64::try_from(self.cursor).unwrap_or(i64::MAX);
        let to = from.saturating_add(delta).max(0);
        self.select(usize::try_from(to).unwrap_or(usize::MAX))
    }

    /// The first row the list shows.
    #[must_use]
    pub const fn offset(&self) -> u64 {
        self.offset
    }

    /// The row index the visible `slot` draws, which is only a row of this
    /// list while it is below [`len`](Self::len).
    #[must_use]
    pub fn index_at(&self, slot: usize) -> Option<usize> {
        let index = usize::try_from(self.offset)
            .unwrap_or(usize::MAX)
            .saturating_add(slot);
        (index < self.len).then_some(index)
    }

    /// The scroll geometry for a surface showing `visible` rows at a time, in
    /// row units, over the shared [`ScrollRange`] normalisation — so an offset
    /// can never exceed what the list holds.
    #[must_use]
    pub fn scroll_range(&self, visible: usize) -> ScrollRange {
        ScrollRange::new(
            u64::try_from(self.len).unwrap_or(u64::MAX),
            u64::try_from(visible).unwrap_or(u64::MAX),
            self.offset,
        )
    }

    /// The scroll model the drawn bar and the wheel both move through: one row
    /// per line, one surface-full per page.
    #[must_use]
    pub fn scroll_model(&self, visible: usize) -> ScrollModel {
        let page = u64::try_from(visible.max(1)).unwrap_or(u64::MAX);
        ScrollModel::new(self.scroll_range(visible), 1, page)
    }

    /// Scroll so `offset` is the first visible row, clamped through
    /// [`scroll_range`](Self::scroll_range), reporting whether it moved.
    pub fn set_offset(&mut self, offset: u64, visible: usize) -> bool {
        let clamped = self.scroll_range(visible).with_offset(offset).offset();
        let moved = clamped != self.offset;
        self.offset = clamped;
        moved
    }

    /// Scroll by `delta` rows (positive scrolls toward the end), clamped,
    /// reporting whether it moved.
    pub fn scroll_by(&mut self, delta: i64, visible: usize) -> bool {
        let offset = self.scroll_model(visible).scroll_by(delta).offset();
        self.set_offset(offset, visible)
    }

    /// Scroll the least that brings the cursor into a surface showing
    /// `visible` rows, reporting whether it moved.
    ///
    /// The one rule keyboard traversal reveals through, so a cursor can never
    /// sit outside the drawn list.
    pub fn reveal(&mut self, visible: usize) -> bool {
        let rows = u64::try_from(visible.max(1)).unwrap_or(u64::MAX);
        let cursor = u64::try_from(self.cursor).unwrap_or(u64::MAX);
        let target = if cursor < self.offset {
            cursor
        } else if cursor >= self.offset.saturating_add(rows) {
            cursor.saturating_sub(rows.saturating_sub(1))
        } else {
            self.offset
        };
        self.set_offset(target, visible)
    }

    /// The list's own drawn scrollbar, carrying its live hover/drag state.
    #[must_use]
    pub const fn scrollbar(&self) -> &ScrollBar {
        &self.bar
    }

    /// Mutable access to the drawn scrollbar, for the pointer routing that
    /// drives it.
    pub const fn scrollbar_mut(&mut self) -> &mut ScrollBar {
        &mut self.bar
    }
}

#[cfg(test)]
mod tests {
    use super::RowList;

    #[test]
    fn a_cursor_clamps_to_the_rows_that_exist() {
        let mut list = RowList::new(3);
        assert_eq!(list.cursor(), 0);
        assert!(list.select(2));
        assert!(!list.select(9), "clamped to the last row, so nothing moved");
        assert_eq!(list.cursor(), 2);
        assert!(list.step(-1));
        assert_eq!(list.cursor(), 1);
        assert!(!list.step(-5) || list.cursor() == 0);
        assert_eq!(list.cursor(), 0);
    }

    #[test]
    fn an_empty_list_has_no_row_to_make_current() {
        let mut list = RowList::new(0);
        assert!(list.is_empty());
        assert!(!list.select(4));
        assert_eq!(list.cursor(), 0);
        assert_eq!(list.index_at(0), None);
    }

    #[test]
    fn a_shrunk_set_never_leaves_the_cursor_naming_somebody_elses_row() {
        let mut list = RowList::new(8);
        assert!(list.select(7));
        assert!(list.set_offset(5, 2));
        list.resize(3);
        assert_eq!(list.cursor(), 2);
        assert_eq!(list.len(), 3);
        // The offset clamps as soon as it is used against a visible count.
        assert_eq!(list.scroll_range(3).offset(), 0);
    }

    #[test]
    fn a_slot_names_a_row_only_while_the_list_reaches_it() {
        let mut list = RowList::new(5);
        assert!(list.set_offset(2, 2));
        assert_eq!(list.index_at(0), Some(2));
        assert_eq!(list.index_at(2), Some(4));
        assert_eq!(list.index_at(3), None);
    }

    #[test]
    fn reveal_scrolls_the_least_that_brings_the_cursor_back_into_view() {
        let mut list = RowList::new(10);
        assert!(list.select(7));
        assert!(list.reveal(3));
        assert_eq!(list.offset(), 5, "the cursor sits on the last visible row");
        assert!(list.select(1));
        assert!(list.reveal(3));
        assert_eq!(list.offset(), 1, "and on the first when it moves back up");
        assert!(!list.reveal(3), "a cursor already in view scrolls nothing");
    }

    #[test]
    fn an_offset_never_exceeds_what_the_list_holds() {
        let mut list = RowList::new(4);
        list.set_offset(u64::MAX, 2);
        assert_eq!(list.offset(), 2, "two rows visible of four");
        list.scroll_by(i64::MAX, 2);
        assert_eq!(list.offset(), 2);
        list.scroll_by(i64::MIN, 2);
        assert_eq!(list.offset(), 0);
    }
}
