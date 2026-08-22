//! The pointer cursors a theme selects.
//!
//! A theme names a cursor per [`CursorKind`] by the identifier of a cursor
//! asset under `/System/Graphics`. [`CursorSet`] is a
//! fixed record with one field per kind, so a theme always defines every
//! cursor and a lookup can never miss.

use alloc::string::String;

/// The pointer shapes the desktop uses.
///
/// `Ord` orders the cache-invalidation candidates a reclaim cache indexes,
/// not a meaningful pointer-shape ordering — the window manager's cursor
/// cache (`plans/SMARTRAM.md` section 6.4) needs `CursorKind` as a
/// `BTreeMap` key.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash, PartialOrd, Ord)]
pub enum CursorKind {
    /// The default arrow pointer.
    Arrow,
    /// The I-beam shown over editable text.
    Text,
    /// The hand shown over a clickable link or control.
    Pointer,
    /// The four-way move cursor shown while dragging a window.
    Move,
    /// The busy/wait cursor.
    Busy,
    /// The left-right double arrow shown on a window's left or right resize
    /// edge.
    ResizeHorizontal,
    /// The up-down double arrow shown on a window's top or bottom resize edge.
    ResizeVertical,
    /// The double arrow along the rising diagonal, shown on a bottom-left or
    /// top-right resize corner.
    ResizeDiagonalRising,
    /// The double arrow along the falling diagonal, shown on a top-left or
    /// bottom-right resize corner.
    ResizeDiagonalFalling,
}

/// Every cursor kind the desktop defines.
///
/// The closed [`CursorKind`] vocabulary as a table, so a loader, a cache, or a
/// test iterates every kind without restating the list.
pub const CURSOR_KINDS: [CursorKind; 9] = [
    CursorKind::Arrow,
    CursorKind::Text,
    CursorKind::Pointer,
    CursorKind::Move,
    CursorKind::Busy,
    CursorKind::ResizeHorizontal,
    CursorKind::ResizeVertical,
    CursorKind::ResizeDiagonalRising,
    CursorKind::ResizeDiagonalFalling,
];

/// One cursor asset identifier per [`CursorKind`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CursorSet {
    /// Asset for [`CursorKind::Arrow`].
    pub arrow: String,
    /// Asset for [`CursorKind::Text`].
    pub text: String,
    /// Asset for [`CursorKind::Pointer`].
    pub pointer: String,
    /// Asset for [`CursorKind::Move`].
    pub move_: String,
    /// Asset for [`CursorKind::Busy`].
    pub busy: String,
    /// Asset for [`CursorKind::ResizeHorizontal`].
    pub resize_horizontal: String,
    /// Asset for [`CursorKind::ResizeVertical`].
    pub resize_vertical: String,
    /// Asset for [`CursorKind::ResizeDiagonalRising`].
    pub resize_diagonal_rising: String,
    /// Asset for [`CursorKind::ResizeDiagonalFalling`].
    pub resize_diagonal_falling: String,
}

impl CursorSet {
    /// The asset identifier for `kind`.
    #[must_use]
    pub fn asset(&self, kind: CursorKind) -> &str {
        match kind {
            CursorKind::Arrow => &self.arrow,
            CursorKind::Text => &self.text,
            CursorKind::Pointer => &self.pointer,
            CursorKind::Move => &self.move_,
            CursorKind::Busy => &self.busy,
            CursorKind::ResizeHorizontal => &self.resize_horizontal,
            CursorKind::ResizeVertical => &self.resize_vertical,
            CursorKind::ResizeDiagonalRising => &self.resize_diagonal_rising,
            CursorKind::ResizeDiagonalFalling => &self.resize_diagonal_falling,
        }
    }
}
