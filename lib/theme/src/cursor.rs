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
}

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
        }
    }
}
