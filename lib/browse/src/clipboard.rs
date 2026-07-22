//! The cut / copy clipboard and its paste-target validation
//! (`plans/NEW-FILEMANAGER.md` `FM7`).
//!
//! Where the [`selection`](crate::select) is the *per-listing* set of marked
//! entries, the [`Clipboard`] is the *cross-directory* set the management verbs
//! act on: it captures the absolute paths of the items being moved or copied at
//! the moment of cut / copy, so it stays valid after the user navigates to the
//! directory they want to paste into. It is the one pure, host-provable model
//! behind Move / Copy / Paste — the engine names *what* would move where and
//! *why a paste is refused*; the app performs the capability-checked
//! `fs_rename` / streamed copy under the user's own identity in its own tail
//! (§4, §5.4). Composing this model grants nothing, so the read-only picker
//! never builds a clipboard.
//!
//! Paths are root-first component lists — the same
//! [`components`](crate::Browser::components) vocabulary the whole engine uses —
//! so the ancestor test that forbids pasting a directory into itself is an
//! exact component-prefix comparison, never a fragile string prefix (which
//! would confuse `/ab` with a child of `/a`).
//!
//! [`plan_paste`] is **fail closed**: a paste that would move an item into
//! itself or its own subtree returns a [`PasteError`] and nothing is planned.
//! A paste that would land an item back on its own current path is not refused
//! — it is flagged
//! ([`PasteItem::overwrites_source`]) so the app can ask for confirmation (or
//! auto-rename a copy) rather than silently clobbering the original (§2.24).

use alloc::string::String;
use alloc::vec::Vec;

/// Whether a clipboard's items are being *moved* (removed from the source once
/// pasted) or *copied* (left in place).
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum ClipboardOp {
    /// Copy: the source items stay where they are; paste duplicates them.
    Copy,
    /// Cut: the source items are removed once the paste succeeds (a move).
    Cut,
}

/// The set of items cut or copied, ready to be pasted into another directory.
///
/// Each item is a non-empty root-first component list — the absolute path of a
/// source entry. The filesystem root (an empty component list) can never be an
/// item, so [`new`](Self::new) refuses one.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Clipboard {
    op: ClipboardOp,
    items: Vec<Vec<String>>,
}

impl Clipboard {
    /// Build a clipboard for `op` over `items` (each a root-first component
    /// path), or `None` when there is nothing to hold — an empty item list, or
    /// any item that names the root (an empty component list).
    ///
    /// Refusing an empty clipboard here means [`plan_paste`] never has to plan
    /// from nothing, and a "paste" with an empty clipboard is simply
    /// unavailable rather than a silent no-op.
    #[must_use]
    pub fn new(op: ClipboardOp, items: Vec<Vec<String>>) -> Option<Self> {
        if items.is_empty() || items.iter().any(Vec::is_empty) {
            return None;
        }
        Some(Self { op, items })
    }

    /// Whether the clipboard's items are moved (`Cut`) or copied (`Copy`).
    #[must_use]
    pub const fn op(&self) -> ClipboardOp {
        self.op
    }

    /// The source items, each a root-first component path.
    #[must_use]
    pub fn items(&self) -> &[Vec<String>] {
        &self.items
    }

    /// The number of items on the clipboard.
    #[must_use]
    pub fn len(&self) -> usize {
        self.items.len()
    }

    /// `true` when the clipboard holds no items. A constructed [`Clipboard`] is
    /// never empty (see [`new`](Self::new)); this exists for callers holding an
    /// `Option<&Clipboard>` that has already been unwrapped.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }
}

/// Why a paste cannot proceed — a fail-closed refusal: nothing is moved or
/// copied.
///
/// A [`Clipboard`] is never empty by construction (see [`Clipboard::new`]), so
/// "nothing to paste" is the absence of a clipboard, not a paste error; the
/// only way a *held* clipboard cannot be pasted is that the target lies inside
/// one of its items.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum PasteError {
    /// The paste target is one of the moved/copied directories itself or a
    /// location inside it, so completing it would move a directory into its own
    /// subtree — an impossible, self-consuming operation. Refused outright; no
    /// confirmation can make it valid.
    WouldRecurse,
}

impl core::fmt::Display for PasteError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::WouldRecurse => f.write_str("cannot paste a folder into itself"),
        }
    }
}

/// One resolved move/copy: the source path and the path it would take in the
/// target directory.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PasteItem {
    source: Vec<String>,
    dest: Vec<String>,
    overwrites_source: bool,
}

impl PasteItem {
    /// The source entry's root-first component path.
    #[must_use]
    pub fn source(&self) -> &[String] {
        &self.source
    }

    /// The root-first component path the item would take in the target
    /// directory (the target components followed by the source's leaf name).
    #[must_use]
    pub fn dest(&self) -> &[String] {
        &self.dest
    }

    /// `true` when the destination path equals the source path — the item would
    /// be pasted back into the directory it already lives in.
    ///
    /// For a `Cut` this is a no-op the app can skip; for a `Copy` it is a
    /// copy-onto-self the app resolves by confirming or by giving the copy a new
    /// name. Either way the app decides — the model only flags it, never
    /// silently clobbers the original (§2.24).
    #[must_use]
    pub const fn overwrites_source(&self) -> bool {
        self.overwrites_source
    }
}

/// The resolved plan for pasting a [`Clipboard`] into a target directory: the
/// operation and one [`PasteItem`] per source, in the clipboard's order.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PastePlan {
    op: ClipboardOp,
    items: Vec<PasteItem>,
}

impl PastePlan {
    /// Whether the plan moves (`Cut`) or copies (`Copy`) its items.
    #[must_use]
    pub const fn op(&self) -> ClipboardOp {
        self.op
    }

    /// The resolved items, one per clipboard source, in order.
    #[must_use]
    pub fn items(&self) -> &[PasteItem] {
        &self.items
    }
}

/// Plan pasting `clipboard` into the directory named by root-first `target`.
///
/// Each source is mapped to a destination in `target` under its own leaf name,
/// and the result is fail-closed:
///
/// * a `target` that is one of the source directories itself or lies inside one
///   is [`PasteError::WouldRecurse`] (an exact component-prefix test);
/// * otherwise every item is planned, with [`PasteItem::overwrites_source`] set
///   for any item whose destination equals its source (a paste back into the
///   same directory) so the app can confirm rather than clobber.
///
/// # Errors
///
/// [`PasteError::WouldRecurse`] as above.
pub fn plan_paste(clipboard: &Clipboard, target: &[String]) -> Result<PastePlan, PasteError> {
    let mut items = Vec::with_capacity(clipboard.items.len());
    for source in &clipboard.items {
        if is_within(target, source) {
            return Err(PasteError::WouldRecurse);
        }
        let leaf = match source.last() {
            Some(leaf) => leaf.clone(),
            // A constructed clipboard never holds an empty (root) item, so a
            // source always has a leaf; refuse rather than fabricate one.
            None => return Err(PasteError::WouldRecurse),
        };
        let mut dest = target.to_vec();
        dest.push(leaf);
        let overwrites_source = dest == *source;
        items.push(PasteItem {
            source: source.clone(),
            dest,
            overwrites_source,
        });
    }
    Ok(PastePlan {
        op: clipboard.op,
        items,
    })
}

/// Whether `path` is `ancestor` itself or lies inside it — an exact root-first
/// component-prefix test (so `/a/b` is within `/a`, but `/ab` is not).
fn is_within(path: &[String], ancestor: &[String]) -> bool {
    path.len() >= ancestor.len() && path[..ancestor.len()] == *ancestor
}
