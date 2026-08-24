//! The **delete** model (`plans/NEW-FILEMANAGER.md` `FM7b`): the pure,
//! host-provable core of removing the selected entries.
//!
//! Where the [`clipboard`](crate::clipboard) captures the selection for a move
//! or copy, a [`DeletePlan`] captures it for removal: the absolute path of each
//! marked entry and whether that entry is directory-backed on disk, in listing
//! order. It is the one model behind the Delete verb — the engine names *what*
//! would be removed and *how many* items (so the app can confirm honestly, a
//! `lib/controls` `Dialog` with truthful action warmth); the app performs the
//! capability-checked `fs_unlink` (with
//! [`UnlinkFlags::DIRECTORY`](tairix_abi::UnlinkFlags::DIRECTORY) and a
//! depth-bounded recursion for a directory-backed target) under the user's own
//! identity in its own tail. Composing this model grants nothing, so the
//! read-only picker never builds a delete plan.
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
//!
//! # Carrying the plan out — [`DeleteWalk`]
//!
//! Where a [`DeletePlan`] names *what* would be removed, a [`DeleteWalk`]
//! models *how* the removal is carried out: the depth-first traversal that
//! removes a directory's contents before the directory itself. It is the
//! delete-side analogue of the paste-side
//! [`CopyCursor`](crate::execute::CopyCursor) — a pure, host-provable, driven
//! cursor that touches no filesystem: the `files.app` `Run` binary reads each
//! directory (`fs_readdir`) and unlinks each node (`fs_unlink`) with its own
//! capability-checked syscalls under the user's own identity, feeding the
//! results back to the walk. Composing the walk grants nothing, so the
//! read-only picker never runs one.
//!
//! The walk keeps its own explicit stack rather than recursing on the call
//! stack, so a deeply nested tree cannot overflow the kernel stack, and it is
//! **bounded** ([`MAX_DELETE_DEPTH`]) and fail closed: a tree deeper than the
//! bound is refused ([`DeleteError::TooDeep`]) rather than descended without
//! limit. It is **interruptible**: the app may stop between any two steps (a
//! Cancel, or a preemption) and the walk holds exactly where it stopped — no
//! unbounded buffer and no spin.
//!
//! This is the browser engine's *own* traversal over the root-first component
//! paths it navigates, driven by an injected directory read — a different model
//! from `rm`'s coreutils removal engine, which recurses natively over its own
//! raw-path removal seam with its own prompt/force/verbose semantics. They are
//! two consumers with two data models, not one algorithm copied twice.

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
    /// whole-root removal.
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
    /// confirmation reports.
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
    /// than treating every deletion as a single file.
    #[must_use]
    pub fn has_directories(&self) -> bool {
        self.targets.iter().any(DeleteTarget::is_directory)
    }
}

/// The deepest directory nesting a single recursive removal will descend,
/// counted in root-first path components.
///
/// A fixed fail-closed *bound*, not a hardware-scaled capacity: it caps how far
/// a [`DeleteWalk`] recurses so a pathological or adversarial tree cannot make
/// the traversal descend without limit. A tree deeper than this is refused
/// ([`DeleteError::TooDeep`]) rather than followed. It is the shared
/// `MAX_WALK_DEPTH` value both the recursive removal and the recursive copy
/// ([`CopyWalk`](crate::execute::CopyWalk)) obey, held in one place so the two
/// walks' bounds cannot drift.
pub const MAX_DELETE_DEPTH: usize = crate::MAX_WALK_DEPTH;

/// Why a [`DeleteWalk`] cannot take the requested step — a fail-closed refusal.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum DeleteError {
    /// Expanding a directory would name a child deeper than
    /// [`MAX_DELETE_DEPTH`] components: refused rather than recursing without
    /// bound.
    TooDeep,
    /// The walk was driven against the wrong action — [`expand`](DeleteWalk::expand)
    /// on a leaf or an already-expanded directory, or
    /// [`complete_removal`](DeleteWalk::complete_removal) on a directory whose
    /// contents have not been listed yet, or either on a finished walk. Refused
    /// rather than silently corrupting the traversal.
    OutOfStep,
}

impl core::fmt::Display for DeleteError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::TooDeep => f.write_str("directory nested deeper than the delete-recursion bound"),
            Self::OutOfStep => f.write_str("delete walk driven against the wrong step"),
        }
    }
}

/// The next step a [`DeleteWalk`] requires of its driver.
///
/// Borrowed from the walk's current position; the driver performs the named
/// filesystem operation and then reports back with
/// [`expand`](DeleteWalk::expand) (for a [`List`](Self::List)) or
/// [`complete_removal`](DeleteWalk::complete_removal) (for a
/// [`Remove`](Self::Remove)).
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum DeleteAction<'a> {
    /// List this directory's children (an `fs_readdir`) and feed them back with
    /// [`expand`](DeleteWalk::expand), so its contents are removed before it.
    List(&'a [String]),
    /// Remove this node — a leaf file, or a directory whose contents have
    /// already been removed (an `fs_unlink`, with
    /// [`UnlinkFlags::DIRECTORY`](tairix_abi::UnlinkFlags::DIRECTORY) when
    /// `is_directory`) — then report it with
    /// [`complete_removal`](DeleteWalk::complete_removal).
    Remove {
        /// The node's root-first absolute component path.
        path: &'a [String],
        /// `true` when the node is directory-backed (a directory or a bundle),
        /// so it is unlinked with the directory flag.
        is_directory: bool,
    },
}

/// One node still to be traversed: its absolute path, whether it is
/// directory-backed, and — for a directory — whether its children have been
/// pushed yet.
#[derive(Clone, Debug, Eq, PartialEq)]
struct Frame {
    path: Vec<String>,
    is_directory: bool,
    expanded: bool,
}

/// A resumable, interruptible cursor over a depth-first recursive removal.
///
/// Built from a [`DeletePlan`], it drives the app through the removal one step
/// at a time (see [`DeleteAction`]): it asks for a directory's listing before
/// removing that directory, so contents are always removed before the directory
/// that holds them. It does no I/O — the app performs each read and unlink —
/// and it never recurses on the call stack (its own explicit stack cannot
/// overflow), stays within [`MAX_DELETE_DEPTH`], and holds its exact position
/// between steps so the app can cancel or be preempted without losing or
/// repeating work.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DeleteWalk {
    stack: Vec<Frame>,
    removed: usize,
}

impl DeleteWalk {
    /// Begin a removal of every target in `plan`, in listing order.
    #[must_use]
    pub fn from_plan(plan: &DeletePlan) -> Self {
        let mut stack = Vec::with_capacity(plan.targets.len());
        // Push targets in reverse so the first target is on top of the stack and
        // the walk processes them in listing order.
        for target in plan.targets.iter().rev() {
            stack.push(Frame {
                path: target.path.clone(),
                is_directory: target.is_directory,
                expanded: false,
            });
        }
        Self { stack, removed: 0 }
    }

    /// The number of nodes removed so far — the honest figure a progress
    /// indicator reports. The total is not known in advance (it depends on the
    /// tree the reads reveal), so progress is a rising count, never a
    /// fabricated percentage.
    #[must_use]
    pub const fn removed(&self) -> usize {
        self.removed
    }

    /// `true` once every reachable node has been removed and nothing remains.
    #[must_use]
    pub fn is_complete(&self) -> bool {
        self.stack.is_empty()
    }

    /// The next step the driver must take, or `None` when the walk is complete.
    ///
    /// A directory whose contents have not been listed yet yields
    /// [`DeleteAction::List`]; a leaf file, or a directory already emptied by
    /// prior steps, yields [`DeleteAction::Remove`].
    #[must_use]
    pub fn next_action(&self) -> Option<DeleteAction<'_>> {
        let top = self.stack.last()?;
        if top.is_directory && !top.expanded {
            Some(DeleteAction::List(&top.path))
        } else {
            Some(DeleteAction::Remove {
                path: &top.path,
                is_directory: top.is_directory,
            })
        }
    }

    /// Report the children read for the directory the last [`DeleteAction::List`]
    /// named, so they are removed before it.
    ///
    /// `children` is each child's leaf name and whether it is directory-backed,
    /// in the source's listing order; they are removed in that order. An empty
    /// listing simply leaves the (now-known-empty) directory ready to be
    /// removed on the next step.
    ///
    /// # Errors
    ///
    /// * [`DeleteError::TooDeep`] — a child would sit deeper than
    ///   [`MAX_DELETE_DEPTH`] components; the walk is left unchanged.
    /// * [`DeleteError::OutOfStep`] — the current step is not a
    ///   [`List`](DeleteAction::List) (a leaf, an already-expanded directory, or
    ///   a finished walk).
    pub fn expand(&mut self, children: &[(String, bool)]) -> Result<(), DeleteError> {
        let top = self
            .stack
            .len()
            .checked_sub(1)
            .ok_or(DeleteError::OutOfStep)?;
        if !self.stack[top].is_directory || self.stack[top].expanded {
            return Err(DeleteError::OutOfStep);
        }
        if self.stack[top].path.len() + 1 > MAX_DELETE_DEPTH {
            return Err(DeleteError::TooDeep);
        }
        let parent = self.stack[top].path.clone();
        self.stack[top].expanded = true;
        // Push children in reverse so they are removed in the source's listing
        // order (the first child ends up on top of the stack).
        for (name, is_directory) in children.iter().rev() {
            let mut path = parent.clone();
            path.push(name.clone());
            self.stack.push(Frame {
                path,
                is_directory: *is_directory,
                expanded: false,
            });
        }
        Ok(())
    }

    /// Report that the node the last [`DeleteAction::Remove`] named has been
    /// unlinked, advancing the walk past it.
    ///
    /// # Errors
    ///
    /// [`DeleteError::OutOfStep`] when the current step is not a
    /// [`Remove`](DeleteAction::Remove) — a directory whose contents have not
    /// been listed yet, or a finished walk; the walk is left unchanged.
    pub fn complete_removal(&mut self) -> Result<(), DeleteError> {
        match self.stack.last() {
            Some(top) if top.is_directory && !top.expanded => Err(DeleteError::OutOfStep),
            Some(_) => {
                self.stack.pop();
                self.removed += 1;
                Ok(())
            }
            None => Err(DeleteError::OutOfStep),
        }
    }
}
