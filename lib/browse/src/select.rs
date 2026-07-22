//! Multi-entry selection: the set of entries the user has marked in the
//! current listing (`plans/NEW-FILEMANAGER.md` `FM7`).
//!
//! The [`Browser`](crate::Browser) has always carried a single *focus* cursor
//! — the entry `Enter`, rename, and activate act on. A file manager also needs
//! a *set* of selected entries so the management verbs (cut / copy / delete)
//! can operate on many items at once. [`Selection`] is that set, modelled here
//! as the one pure, host-provable definition the file manager and the trusted
//! picker share (§2.2) — the picker composes the same [`Browser`](crate::Browser) and simply
//! never invokes the write verbs the selection feeds.
//!
//! The selection is **per-listing**: its members are indices into the entries
//! the browser is currently showing, so the browser collapses it to the single
//! focused entry whenever the listing changes (a navigation, a refresh, or a
//! re-sort). Persisting a *cross-directory* set of items for a move or copy is
//! the [`clipboard`](crate::clipboard)'s job, which captures absolute paths at
//! the moment of cut / copy and so survives navigating away.
//!
//! This module holds no authority and touches no filesystem: it is bookkeeping
//! over `usize` indices, bounds-checked by the browser against the live listing
//! before every mutation.

use alloc::collections::BTreeSet;

/// The set of entries selected in the current listing, plus the *anchor* a
/// range extension (`Shift`-click) grows from.
///
/// Indices are positions in the browser's current [`entries`](crate::Browser::entries);
/// the browser validates every index against the live listing before calling a
/// mutator here, so a [`Selection`] never holds an out-of-range member. It is
/// ordered (a [`BTreeSet`]), so [`iter`](Self::iter) always yields members
/// low-to-high regardless of the order they were added.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Selection {
    indices: BTreeSet<usize>,
    anchor: Option<usize>,
}

impl Selection {
    /// An empty selection with no anchor.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            indices: BTreeSet::new(),
            anchor: None,
        }
    }

    /// The number of selected entries.
    #[must_use]
    pub fn len(&self) -> usize {
        self.indices.len()
    }

    /// `true` when no entry is selected.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.indices.is_empty()
    }

    /// Whether `index` is currently selected.
    #[must_use]
    pub fn contains(&self, index: usize) -> bool {
        self.indices.contains(&index)
    }

    /// The selected indices, low-to-high.
    pub fn iter(&self) -> impl Iterator<Item = usize> + '_ {
        self.indices.iter().copied()
    }

    /// The anchor a [`range_to`](Self::range_to) extension grows from — the
    /// last index a plain-click ([`single`](Self::single)) or toggle
    /// ([`toggle`](Self::toggle)) set. `None` only while the selection is empty.
    #[must_use]
    pub const fn anchor(&self) -> Option<usize> {
        self.anchor
    }

    /// Select exactly `index`, dropping any previous selection — a plain click
    /// or an unmodified keyboard move. `index` becomes the anchor.
    pub fn single(&mut self, index: usize) {
        self.indices.clear();
        self.indices.insert(index);
        self.anchor = Some(index);
    }

    /// Add `index` to the selection if absent, or remove it if present — a
    /// `Ctrl`-click. `index` becomes the new anchor either way (the next
    /// range extension grows from the entry the user just acted on).
    pub fn toggle(&mut self, index: usize) {
        if !self.indices.remove(&index) {
            self.indices.insert(index);
        }
        self.anchor = Some(index);
    }

    /// Select the contiguous range between the current anchor and `index`
    /// (inclusive), replacing any previous selection — a `Shift`-click. The
    /// anchor is left unchanged, so successive `Shift`-clicks all grow from the
    /// same origin. With no anchor yet this is a plain [`single`](Self::single).
    pub fn range_to(&mut self, index: usize) {
        let Some(anchor) = self.anchor else {
            self.single(index);
            return;
        };
        let (lo, hi) = if anchor <= index {
            (anchor, index)
        } else {
            (index, anchor)
        };
        self.indices.clear();
        for i in lo..=hi {
            self.indices.insert(i);
        }
        self.anchor = Some(anchor);
    }

    /// Select every entry `0..count` — Select All. The first entry becomes the
    /// anchor; an empty listing (`count == 0`) leaves the selection empty.
    pub fn select_all(&mut self, count: usize) {
        self.indices.clear();
        for i in 0..count {
            self.indices.insert(i);
        }
        self.anchor = if count == 0 { None } else { Some(0) };
    }

    /// Drop the whole selection and the anchor.
    pub fn clear(&mut self) {
        self.indices.clear();
        self.anchor = None;
    }
}
