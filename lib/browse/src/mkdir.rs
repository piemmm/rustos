//! The **new-folder** model (`plans/NEW-FILEMANAGER.md` FM7b): the pure,
//! host-tested core of creating a directory in the current listing.
//!
//! Making a folder is modelled here so every rule that decides whether a typed
//! name is acceptable — the spelling ([`tairix_path::validate_file_name`], the
//! *one* shared name rule, §2.2) and a clash with an existing sibling — runs in
//! `cargo test` with no kernel. The app supplies only the `fs_mkdir` seam and
//! the inline text editor; the decision of *whether* to call the VFS, and
//! *what* the new folder's path is, lives in
//! [`Browser::create_directory`](crate::Browser::create_directory).
//!
//! Authority is unchanged: the create is an ordinary permission-checked VFS
//! call under the caller's own identity (no new capability), so the engine adds
//! nothing — the trusted picker composes the same [`Browser`](crate::Browser)
//! and simply never calls the write path. Validation is *spelling only*: a name
//! this module accepts may still be refused by the VFS (a permission denial, a
//! read-only mount, a lost race, a name already taken between the check and the
//! call), which surfaces as [`MkdirError::Refused`] with the kernel's own
//! [`Errno`].

use tairix_abi::Errno;
use tairix_path::PathError;

use crate::entry::Entry;

/// Why a new folder was not created.
///
/// The spelling failures ([`Empty`](Self::Empty), [`Reserved`](Self::Reserved),
/// [`Separator`](Self::Separator), [`Invalid`](Self::Invalid),
/// [`TooLong`](Self::TooLong)) and the [`Clash`](Self::Clash) are decided
/// *before* any syscall, so the listing is untouched. [`Refused`](Self::Refused)
/// and [`Source`](Self::Source) carry the kernel's own reason for a failure at
/// or after the VFS call.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum MkdirError {
    /// The name is empty.
    Empty,
    /// The name is `.` or `..` — the navigation names, never a real name.
    Reserved,
    /// The name contains a `/`: it spells a path, not one name.
    Separator,
    /// The name contains a control character (including NUL) or a `:` (the
    /// reserved path delimiter).
    Invalid,
    /// The name is longer than the filesystem allows (the per-name bound, or
    /// the resulting absolute path past its own limit).
    TooLong,
    /// A sibling in the same directory already has this name.
    Clash,
    /// The VFS refused the create (a permission denial, a read-only mount, a
    /// lost race); the listing is unchanged.
    Refused(Errno),
    /// The folder was created but the directory could no longer be re-listed to
    /// refresh the view.
    Source(Errno),
}

impl MkdirError {
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

    /// A terse, human-readable reason for the in-UI refusal line (§2.24 — a
    /// denied action is an honest answer, never a silent failure). It names no
    /// path and carries no secret.
    #[must_use]
    pub const fn message(self) -> &'static str {
        match self {
            Self::Empty => "The name cannot be empty.",
            Self::Reserved => "\".\" and \"..\" are not valid names.",
            Self::Separator => "A name cannot contain \"/\".",
            Self::Invalid => "That name contains a character that is not allowed.",
            Self::TooLong => "That name is too long.",
            Self::Clash => "An item with that name already exists here.",
            Self::Refused(_) => "The folder could not be created.",
            Self::Source(_) => "Created, but the folder could not be reloaded.",
        }
    }
}

/// Validate `name` as a new folder in a directory whose listing is `siblings`.
///
/// Pure and fail-closed: the name is spelled through the one shared
/// [`tairix_path::validate_file_name`] rule and a name already taken by a
/// sibling is a [`MkdirError::Clash`]. It performs no I/O and makes no
/// permission decision — that is the VFS's, at create time.
///
/// # Errors
///
/// The [`MkdirError`] naming the first rule the name breaks (a spelling
/// variant or [`Clash`](MkdirError::Clash)).
pub fn validate_new_dir_name(name: &str, siblings: &[Entry]) -> Result<(), MkdirError> {
    tairix_path::validate_file_name(name).map_err(MkdirError::from_path)?;
    if siblings.iter().any(|entry| entry.name() == name) {
        return Err(MkdirError::Clash);
    }
    Ok(())
}
