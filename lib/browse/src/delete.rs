//! The **delete** model (`plans/NEW-FILEMANAGER.md` `FM7b`): the pure,
//! host-provable core of removing the selected entries.
//!
//! Where the [`clipboard`](crate::clipboard) captures the selection for a move
//! or copy, a [`DeletePlan`] captures it for removal: the absolute path of each
//! marked entry and whether that entry is directory-backed on disk, in listing
//! order. It is the one model behind the Delete verb — the engine names *what*
//! would be removed and *how many* items (so the app can confirm honestly, a
//! `lib/controls` `Dialog` with truthful action warmth, §2.24); the app
//! performs the capability-checked `fs_unlink` (with
//! [`UnlinkFlags::DIRECTORY`](tairix_abi::UnlinkFlags::DIRECTORY) and a
//! depth-bounded recursion for a directory-backed target) under the user's own
//! identity in its own tail (§4, §5.4). Composing this model grants nothing, so
//! the read-only picker never builds a delete plan.
//!
//! Paths are root-first component lists — the same
//! [`components`](crate::Browser::components) vocabulary the whole engine uses —
//! so a target names exactly the node the browser shows and can never resolve
//! to a different one.
//!
//! [`DeletePlan::new`] is **fail closed**: a plan is built only from a
//! non-empty set of real entries; an empty selection, or any target that names
//! the filesystem root (an empty component list), yields `None` rather than a
//! plan that could remove nothing or the root.

use alloc::string::String;
use alloc::vec::Vec;

/// One entry queued for removal: its absolute path and whether it is
/// directory-backed on disk.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DeleteTarget {
    path: Vec<String>,
    is_directory: bool,
}

impl DeleteTarget {
    /// The target's root-first component path (never empty in a built
    /// [`DeletePlan`]).
    #[must_use]
    pub fn path(&self) -> &[String] {
        &self.path
    }

    /// `true` when the target is directory-backed on disk — a directory or a
    /// bundle — so the app removes it with
    /// [`UnlinkFlags::DIRECTORY`](tairix_abi::UnlinkFlags::DIRECTORY) and
    /// recurses into it, rather than unlinking it as a leaf file.
    #[must_use]
    pub const fn is_directory(&self) -> bool {
        self.is_directory
    }

    /// The target's leaf name — its last path component.
    ///
    /// A [`DeletePlan`] never holds a root (empty) target, so this is always
    /// present.
    #[must_use]
    pub fn name(&self) -> &str {
        self.path.last().map_or("", String::as_str)
    }
}

/// The resolved plan for deleting a selection: one [`DeleteTarget`] per marked
/// entry, in listing order.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DeletePlan {
    targets: Vec<DeleteTarget>,
}

impl DeletePlan {
    /// Build a plan from `(path, is_directory)` pairs, or `None` when there is
    /// nothing to delete — an empty selection, or any path that names the root
    /// (an empty component list).
    ///
    /// Refusing an empty or root-naming plan here means the Delete verb is
    /// simply unavailable rather than a silent no-op or a catastrophic
    /// whole-root removal (§5.4).
    #[must_use]
    pub fn new(targets: Vec<(Vec<String>, bool)>) -> Option<Self> {
        if targets.is_empty() || targets.iter().any(|(path, _)| path.is_empty()) {
            return None;
        }
        let targets = targets
            .into_iter()
            .map(|(path, is_directory)| DeleteTarget { path, is_directory })
            .collect();
        Some(Self { targets })
    }

    /// The queued targets, one per selected entry, in listing order.
    #[must_use]
    pub fn targets(&self) -> &[DeleteTarget] {
        &self.targets
    }

    /// The number of entries the plan would remove — the count an honest
    /// confirmation reports (§2.24).
    #[must_use]
    pub fn len(&self) -> usize {
        self.targets.len()
    }

    /// `true` when the plan holds no targets. A built [`DeletePlan`] is never
    /// empty (see [`new`](Self::new)); this exists for callers holding an
    /// already-unwrapped plan.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.targets.is_empty()
    }

    /// `true` when any target is directory-backed, so a confirmation can warn
    /// that folders (and their contents) will be removed recursively rather
    /// than treating every deletion as a single file (§2.24).
    #[must_use]
    pub fn has_directories(&self) -> bool {
        self.targets.iter().any(DeleteTarget::is_directory)
    }
}
