//! The in-place **rename** model: the pure, host-tested core of the file
//! manager's first write operation.
//!
//! Renaming an item is modelled here so every rule that decides whether a typed
//! name is acceptable — the spelling ([`tairix_path::validate_file_name`], the
//! *one* shared name rule), a no-op rename to the same name, and a clash with
//! an existing sibling — runs in `cargo test` with no kernel. The app supplies
//! only the `fs_rename` seam and the text editor; the decision of *whether* to
//! call the VFS, and *what* the two paths are, lives in
//! [`Browser::rename_selected`](crate::Browser::rename_selected).
//!
//! Authority is unchanged: the rename is an ordinary permission-checked VFS
//! call under the caller's own identity (no new capability), so the engine
//! adds nothing — the trusted picker composes the same [`Browser`](crate::Browser)
//! and simply never calls the write path. Validation is *spelling only*: a
//! name this module accepts may still be refused by the VFS (a permission
//! denial, a read-only mount, a lost race), which surfaces as
//! [`RenameError::Refused`] with the kernel's own [`Errno`].

use tairix_abi::Errno;
use tairix_path::PathError;

use crate::entry::Entry;

/// Why a rename was not applied.
///
/// The spelling failures ([`Empty`](Self::Empty), [`Reserved`](Self::Reserved),
/// [`Separator`](Self::Separator), [`Invalid`](Self::Invalid),
/// [`TooLong`](Self::TooLong)) and the two model failures
/// ([`Clash`](Self::Clash), [`Unchanged`](Self::Unchanged)) are decided
/// *before* any syscall, so the listing is untouched. [`Refused`](Self::Refused)
/// and [`Source`](Self::Source) carry the kernel's own reason for a failure at
/// or after the VFS call.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum RenameError {
    /// The directory is empty, so there is no selected entry to rename.
    NoSelection,
    /// The new name is empty.
    Empty,
    /// The new name is `.` or `..` — the navigation names, never a real name.
    Reserved,
    /// The new name contains a `/`: it spells a path, not one name.
    Separator,
    /// The new name contains a control character (including NUL) or a `:`
    /// (the reserved path delimiter).
    Invalid,
    /// The new name is longer than the filesystem allows (the per-name bound,
    /// or the resulting absolute path past its own limit).
    TooLong,
    /// A *different* sibling in the same directory already has this name.
    Clash,
    /// The new name equals the current name — nothing to do (the caller closes
    /// the editor without touching the VFS).
    Unchanged,
    /// The VFS refused the rename (a permission denial, a read-only mount, a
    /// cross-volume move, a lost race); the listing is unchanged.
    Refused(Errno),
    /// The rename succeeded but the directory could no longer be re-listed to
    /// refresh the view.
    Source(Errno),
}

impl RenameError {
    /// Map a [`PathError`] from the shared name rule onto the spelling variant.
    #[must_use]
    pub(crate) fn from_path(err: PathError) -> Self {
        match err {
            PathError::EmptyComponent => Self::Empty,
            PathError::ReservedName => Self::Reserved,
            PathError::SeparatorInName => Self::Separator,
            PathError::ComponentTooLong => Self::TooLong,
            _ => Self::Invalid,
        }
    }

    /// A terse, human-readable reason for the in-UI refusal line (a denied
    /// action is an honest answer, never a silent failure). It names no path
    /// and carries no secret.
    #[must_use]
    pub const fn message(self) -> &'static str {
        match self {
            Self::NoSelection => "Nothing selected to rename.",
            Self::Empty => "The name cannot be empty.",
            Self::Reserved => "\".\" and \"..\" are not valid names.",
            Self::Separator => "A name cannot contain \"/\".",
            Self::Invalid => "That name contains a character that is not allowed.",
            Self::TooLong => "That name is too long.",
            Self::Clash => "An item with that name already exists here.",
            Self::Unchanged => "The name is unchanged.",
            Self::Refused(_) => "The rename was refused.",
            Self::Source(_) => "Renamed, but the folder could not be reloaded.",
        }
    }
}

/// Validate `new_name` as a rename for the entry currently named `current`
/// within `siblings` (the directory's whole listing).
///
/// Pure and fail-closed: the name is spelled through the one shared
/// [`tairix_path::validate_file_name`] rule, a rename to the same name is a
/// no-op ([`RenameError::Unchanged`]), and a name already taken by a
/// *different* sibling is a [`RenameError::Clash`]. It performs no I/O and
/// makes no permission decision — that is the VFS's, at commit time.
///
/// # Errors
///
/// The [`RenameError`] naming the first rule the name breaks (a spelling
/// variant, [`Unchanged`](RenameError::Unchanged), or
/// [`Clash`](RenameError::Clash)).
pub fn validate_new_name(
    new_name: &str,
    current: &str,
    siblings: &[Entry],
) -> Result<(), RenameError> {
    tairix_path::validate_file_name(new_name).map_err(RenameError::from_path)?;
    if new_name == current {
        return Err(RenameError::Unchanged);
    }
    if siblings.iter().any(|entry| entry.name() == new_name) {
        return Err(RenameError::Clash);
    }
    Ok(())
}
