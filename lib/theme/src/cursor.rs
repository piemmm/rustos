//! The pointer cursors a theme selects, and the identity of a cursor *set*.
//!
//! Two independent selections meet in the cursor store, and the names of
//! both live here rather than beside the artwork:
//!
//! * a [`CursorSetId`] names one **set** — a family of artwork the user
//!   chooses, a directory in the shipped store;
//! * a [`CursorSet`] names one **asset per kind** *within* whichever set is
//!   active, so a theme may point at artwork of its own. It is a fixed
//!   record with one field per kind, so a theme always defines every cursor
//!   and a lookup can never miss.
//!
//! An asset therefore resolves as `<set>/<asset id>.svg`, and
//! [`CursorSet::canonical`] is the naming the shipped sets are authored
//! against.

use alloc::string::String;
use core::fmt;

use tairix_abi::desktop::CURSOR_SET_NAME_MAX;
use tairix_inline::ArrayString;

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

impl CursorKind {
    /// The canonical asset identifier for this kind.
    ///
    /// The one spelling a shipped cursor set files its artwork under, so
    /// the image build can judge a file name against the closed kind
    /// vocabulary and [`CursorSet::canonical`] needs no second table.
    #[must_use]
    pub const fn asset_id(self) -> &'static str {
        match self {
            Self::Arrow => "cursor.arrow",
            Self::Text => "cursor.text",
            Self::Pointer => "cursor.pointer",
            Self::Move => "cursor.move",
            Self::Busy => "cursor.busy",
            Self::ResizeHorizontal => "cursor.resize-horizontal",
            Self::ResizeVertical => "cursor.resize-vertical",
            Self::ResizeDiagonalRising => "cursor.resize-diagonal-rising",
            Self::ResizeDiagonalFalling => "cursor.resize-diagonal-falling",
        }
    }
}

/// The stable identity of a cursor set: its directory name in the shipped
/// store, which is also the label a chooser draws and the value the
/// desktop's `cursor.set` setting holds.
///
/// **The name is the label, verbatim** — exactly as a wallpaper category's
/// is — so a set is authored by naming a directory what the user should
/// read, and no second spelling of it can drift.
///
/// Held inline rather than as a `String`: the window manager's cursor cache
/// compares its epoch on every pointer refresh, so an owned heap name would
/// put an allocation on the compositing path. It is [`Copy`] for the same
/// reason.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash, PartialOrd, Ord)]
pub struct CursorSetId(ArrayString<CURSOR_SET_NAME_MAX>);

impl CursorSetId {
    /// The name of the always-present built-in set.
    ///
    /// A real name rather than an absence: the built-in set is one of the
    /// choices a reader is offered, so it needs a label of its own.
    pub const BUILTIN_NAME: &'static str = "Standard";

    /// The id of the always-present built-in cursor set.
    ///
    /// Spelled directly rather than through [`new`](Self::new) so it needs
    /// no error path; that the name is one `new` would also accept is
    /// pinned by a test.
    #[must_use]
    pub fn builtin() -> Self {
        Self(ArrayString::from_str_truncating(Self::BUILTIN_NAME))
    }

    /// The id of the set named `name`, or `None` when the name is not one a
    /// set may carry.
    ///
    /// Fails closed on anything that is not a plain directory leaf name
    /// within [`CURSOR_SET_NAME_MAX`] bytes: a name with a separator could
    /// widen the store path it is spliced into, and an over-long one could
    /// not be carried to a chooser.
    #[must_use]
    pub fn new(name: &str) -> Option<Self> {
        if tairix_path::validate_file_name(name).is_err() || name.len() > CURSOR_SET_NAME_MAX {
            return None;
        }
        Some(Self(ArrayString::from_str_truncating(name)))
    }

    /// The set's name.
    #[must_use]
    pub fn name(&self) -> &str {
        self.0.as_str()
    }

    /// Whether this is the built-in set.
    #[must_use]
    pub fn is_builtin(&self) -> bool {
        self.name() == Self::BUILTIN_NAME
    }
}

impl fmt::Display for CursorSetId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
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
    /// Every kind under its own [`CursorKind::asset_id`].
    ///
    /// What the shipped themes name, and the naming a shipped cursor set is
    /// authored against — so a set's files and the assets a theme asks for
    /// are one definition.
    #[must_use]
    pub fn canonical() -> Self {
        Self {
            arrow: String::from(CursorKind::Arrow.asset_id()),
            text: String::from(CursorKind::Text.asset_id()),
            pointer: String::from(CursorKind::Pointer.asset_id()),
            move_: String::from(CursorKind::Move.asset_id()),
            busy: String::from(CursorKind::Busy.asset_id()),
            resize_horizontal: String::from(CursorKind::ResizeHorizontal.asset_id()),
            resize_vertical: String::from(CursorKind::ResizeVertical.asset_id()),
            resize_diagonal_rising: String::from(CursorKind::ResizeDiagonalRising.asset_id()),
            resize_diagonal_falling: String::from(CursorKind::ResizeDiagonalFalling.asset_id()),
        }
    }

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
