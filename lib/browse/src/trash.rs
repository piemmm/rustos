//! The **move-to-Trash** model (`plans/NEW-FILEMANAGER.md` `FM10`): the pure,
//! host-provable core of a *recoverable* delete.
//!
//! A delete should be reversible when that costs nothing (§2.24). Removing an
//! item on the same volume as the user's Trash directory is exactly that cheap
//! case: a single [`fs_rename`] moves the item into Trash intact, recoverable
//! until the user empties it, in place of an irreversible recursive
//! `fs_unlink`. Only a cross-volume removal (a rename cannot span volumes,
//! exactly as `mv` decides from `st_dev`) or the absence of a usable Trash
//! forces the irreversible unlink — the existing [`DeleteWalk`](crate::delete::DeleteWalk)
//! path (§2.2, reused, not duplicated).
//!
//! This module is the pure decision behind that: it names *whether* an item can
//! be trashed cheaply and *where in Trash it lands* without clobbering anything
//! already there. It touches no filesystem and holds no authority — the
//! `files.app` `Run` binary performs the `fs_stat` / `fs_rename` / `fs_unlink`
//! under the user's own identity in its own capability-checked tail (§4, §5.4),
//! so composing this model grants nothing and the read-only picker never runs
//! it.
//!
//! # Same volume, or unlink
//!
//! [`trash_strategy`] makes the move-vs-unlink decision from the item's and the
//! Trash directory's [`VolumeId`]s — the same 16-byte `fs_stat` volume identity
//! [`paste_strategy`](crate::execute::paste_strategy) compares, so the two
//! decisions share one definition of "can a single rename carry this" (§2.2).
//!
//! # A collision-free home in Trash
//!
//! Trash accumulates removed items, so a file named `notes.txt` may be deleted
//! while a previously-trashed `notes.txt` is still there. [`trash_dest_path`]
//! resolves a destination *inside* the Trash directory that no existing entry
//! carries: the original leaf when it is free, otherwise the smallest
//! ` (n)` disambiguation inserted before the extension (`notes (2).txt`,
//! `notes (3).txt`, …). It never overwrites an existing trashed item (§2.24, no
//! silent clobber) and is fail closed: it refuses a Trash directory that names
//! the root, an invalid original name, a disambiguation that would exceed the
//! per-name length limit, and a search that cannot find a free name within
//! [`MAX_TRASH_NAME_ATTEMPTS`] (§5.4).
//!
//! The extension split reuses the one shared [`crate::icon`] extension rule, so
//! the disambiguation lands before the same extension the icon and "Open With…"
//! classifiers recognise (§2.2).
//!
//! [`fs_rename`]: tairix_abi::SyscallNumber::FS_RENAME

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use tairix_path::validate_file_name;

use crate::execute::VolumeId;
use crate::icon::extension;

/// How a single delete target must be removed: a cheap recoverable move into
/// Trash, or the irreversible unlink.
///
/// Exhaustive over the one distinction that matters — whether the item and the
/// Trash directory share a volume, so a single rename can carry the item intact.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum TrashStrategy {
    /// The item is on the same volume as Trash: one [`fs_rename`] moves it into
    /// Trash, recoverable until the user empties it — nothing is destroyed.
    ///
    /// [`fs_rename`]: tairix_abi::SyscallNumber::FS_RENAME
    Move,
    /// The item is on a different volume from Trash (a rename cannot span
    /// volumes): fall back to the irreversible recursive
    /// [`fs_unlink`](tairix_abi::SyscallNumber::FS_UNLINK), the existing
    /// [`DeleteWalk`](crate::delete::DeleteWalk) path.
    Unlink,
}

/// The name of the per-user library directory the Trash lives inside — the
/// fixed `/Users/<u>/Library/` subtree (never a new sibling of `Library`), the
/// one the charter's home layout reserves for per-user state.
pub const TRASH_LIBRARY_DIR: &str = "Library";

/// The name of the Trash directory itself, inside [`TRASH_LIBRARY_DIR`].
pub const TRASH_LEAF_DIR: &str = "Trash";

/// The per-user Trash directory: the fixed `Library/Trash` subtree under the
/// user's `home` (root-first component path). One definition so the file
/// manager and its tests agree on exactly where a trashed item lands, and a
/// change to the location cannot silently diverge between them (§2.2).
///
/// `home` is the user's home as root-first components (e.g. `["Users",
/// "root"]`); the returned path appends [`TRASH_LIBRARY_DIR`] then
/// [`TRASH_LEAF_DIR`]. It spells only a location — it performs no I/O, creates
/// nothing, and grants no authority (the app creates and writes it under the
/// user's own identity).
#[must_use]
pub fn trash_dir(home: &[String]) -> Vec<String> {
    let mut dir = Vec::with_capacity(home.len() + 2);
    dir.extend_from_slice(home);
    dir.push(String::from(TRASH_LIBRARY_DIR));
    dir.push(String::from(TRASH_LEAF_DIR));
    dir
}

/// Whether a confirmed delete will move its targets to Trash (recoverable) or
/// remove them permanently — the honest distinction the confirmation dialog
/// states so the user knows which will happen (§2.24).
///
/// A file-manager selection lives in one directory, hence on one volume, so a
/// whole delete plan is uniform: either every target can be moved to Trash (the
/// current directory is on Trash's volume and a usable Trash exists) or the
/// removal is permanent (a cross-volume target — a mounted volume under the
/// current directory — or an unresolved/unavailable Trash forces the
/// irreversible unlink). The app computes this from the targets' and Trash's
/// [`VolumeId`]s via [`trash_strategy`] and never shows a wording its execution
/// will not honour.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum DeleteDisposition {
    /// Every target moves into the user's Trash — a same-volume rename per
    /// item, recoverable until the Trash is emptied.
    Trash,
    /// The targets are removed permanently (the existing irreversible
    /// [`DeleteWalk`](crate::delete::DeleteWalk) unlink), because Trash is
    /// cross-volume or unavailable.
    Permanent,
}

/// Decide how to remove a delete target, given the [`VolumeId`] of the item and
/// of the user's Trash directory.
///
/// * equal volumes → [`TrashStrategy::Move`] (a cheap recoverable rename).
/// * different volumes → [`TrashStrategy::Unlink`] (a rename cannot cross a
///   volume boundary, so the removal is the irreversible unlink).
#[must_use]
pub fn trash_strategy(item: VolumeId, trash: VolumeId) -> TrashStrategy {
    if item == trash {
        TrashStrategy::Move
    } else {
        TrashStrategy::Unlink
    }
}

/// The most disambiguation candidates [`trash_dest_path`] will try before it
/// gives up with [`TrashError::NoFreeName`].
///
/// A fixed fail-closed *bound*, not a hardware-scaled capacity (§24.4): it caps
/// the search for a free ` (n)` name so a Trash directory already holding a
/// pathological run of same-named items can never make the resolution loop
/// without limit (§26.6). Reaching it refuses the trash move (the app falls
/// back to the irreversible unlink or reports the refusal) rather than spinning.
/// Chosen far beyond any plausible number of same-named trashed items.
pub const MAX_TRASH_NAME_ATTEMPTS: usize = 100_000;

/// Why a Trash destination could not be resolved — every case fail closed
/// (§5.4), so the app never fabricates a move that would lose or overwrite data.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum TrashError {
    /// The Trash directory path was empty (it named the root): a Trash location
    /// must be a real directory, never the root.
    RootTrash,
    /// The item's original leaf name is not a valid single filename (empty,
    /// `.`/`..`, or containing a `/`, control character, or `:`), so no
    /// destination can be spelled from it.
    InvalidName,
    /// A disambiguated name would exceed the per-name length limit
    /// (`FS_NAME_MAX`); refused rather than silently truncated (§21-style
    /// no-silent-loss discipline).
    TooLong,
    /// No collision-free name was found within [`MAX_TRASH_NAME_ATTEMPTS`]; the
    /// app falls back to the irreversible unlink rather than looping without
    /// bound.
    NoFreeName,
}

impl TrashError {
    /// A terse, human-readable reason for an in-UI refusal line (§2.24 — a
    /// denied action is an honest answer). It names no path and carries no
    /// secret.
    #[must_use]
    pub const fn message(self) -> &'static str {
        match self {
            Self::RootTrash => "The Trash location is not a valid folder.",
            Self::InvalidName => "That item cannot be moved to Trash.",
            Self::TooLong => "That name is too long to move to Trash.",
            Self::NoFreeName => "Too many items of that name are already in Trash.",
        }
    }
}

/// Resolve a collision-free destination path inside `trash_dir` for an item
/// whose original leaf name is `leaf`, given the leaf names already present in
/// the Trash directory (`taken`).
///
/// Returns the full root-first component path the item should be renamed to:
/// `trash_dir` with the resolved leaf appended. When `leaf` is free the leaf is
/// used unchanged; otherwise the smallest ` (n)` disambiguation (n ≥ 2) that no
/// entry in `taken` carries is inserted before the extension.
///
/// Pure and fail closed: it performs no I/O, makes no permission decision (that
/// is the VFS's at rename time), and never returns a path that would overwrite
/// an existing trashed item.
///
/// # Errors
///
/// * [`TrashError::RootTrash`] — `trash_dir` is empty (names the root).
/// * [`TrashError::InvalidName`] — `leaf` is not a valid single filename.
/// * [`TrashError::TooLong`] — a disambiguated candidate would exceed the
///   per-name length limit.
/// * [`TrashError::NoFreeName`] — no free name within [`MAX_TRASH_NAME_ATTEMPTS`].
pub fn trash_dest_path(
    trash_dir: &[String],
    leaf: &str,
    taken: &[String],
) -> Result<Vec<String>, TrashError> {
    if trash_dir.is_empty() {
        return Err(TrashError::RootTrash);
    }
    if validate_file_name(leaf).is_err() {
        return Err(TrashError::InvalidName);
    }
    let name = resolve_trash_name(leaf, taken)?;
    let mut dest = Vec::with_capacity(trash_dir.len() + 1);
    dest.extend_from_slice(trash_dir);
    dest.push(name);
    Ok(dest)
}

/// Whether `taken` already lists `name` (an exact match — the VFS is
/// case-preserving, so a differently-cased sibling is a different name).
fn is_taken(taken: &[String], name: &str) -> bool {
    taken.iter().any(|existing| existing == name)
}

/// Resolve the collision-free leaf name for [`trash_dest_path`].
fn resolve_trash_name(leaf: &str, taken: &[String]) -> Result<String, TrashError> {
    if !is_taken(taken, leaf) {
        return Ok(String::from(leaf));
    }
    // Split the extension off with the one shared rule so a disambiguation lands
    // before the same extension the icon/"Open With…" classifiers recognise.
    let (stem, ext) = match extension(leaf) {
        // `leaf` ends in `.<ext>`; the stem is everything before the final dot.
        Some(ext) => (&leaf[..leaf.len() - ext.len() - 1], Some(ext)),
        None => (leaf, None),
    };
    for suffix in 2..=(MAX_TRASH_NAME_ATTEMPTS + 1) {
        let candidate = match ext {
            Some(ext) => format!("{stem} ({suffix}).{ext}"),
            None => format!("{stem} ({suffix})"),
        };
        // A very long original name can push a disambiguation past the per-name
        // limit; refuse rather than truncate to a name that could collide.
        if validate_file_name(&candidate).is_err() {
            return Err(TrashError::TooLong);
        }
        if !is_taken(taken, &candidate) {
            return Ok(candidate);
        }
    }
    Err(TrashError::NoFreeName)
}
