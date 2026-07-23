//! The **permission-edit** model (`plans/NEW-FILEMANAGER.md` FM8b): the pure,
//! host-tested core of committing a new permission mode to the selected node.
//!
//! Changing a node's permissions is modelled here so the one rule that decides
//! whether a requested mode word is acceptable — it may carry only the bits
//! [`fs_set_mode`](tairix_abi::SyscallNumber::FS_SET_MODE) itself accepts,
//! the [`FS_MODE_MASK`] `rwx`/setuid/setgid/sticky
//! bits — runs in `cargo test` with no kernel. The app supplies only the
//! `fs_set_mode` seam and the permission control; the decision of *whether* to
//! call the VFS, and *what* the target path is, lives in
//! [`Browser::set_mode_selected`](crate::Browser::set_mode_selected).
//!
//! Authority is unchanged: the change is an ordinary permission-checked VFS
//! call under the caller's own identity (no new capability), so the engine
//! adds nothing — the trusted picker composes the same [`Browser`](crate::Browser)
//! and simply never calls the write path. Validation is *spelling only*: a mode
//! this module accepts may still be refused by the VFS (the user does not own
//! the node, a read-only mount, a lost race), which surfaces as
//! [`ModeError::Refused`] with the kernel's own [`Errno`]. It never silently
//! masks an out-of-range request into a different mode — an unacceptable word
//! is refused honestly, so the mode applied is always exactly the one asked
//! for.

use tairix_abi::fs::FS_MODE_MASK;
use tairix_abi::Errno;

/// Why a permission change was not applied.
///
/// The precondition failures ([`NoSelection`](Self::NoSelection),
/// [`Invalid`](Self::Invalid)) are decided *before* any syscall, so nothing is
/// changed. [`Refused`](Self::Refused) carries the kernel's own reason for a
/// failure at the VFS call, and [`Path`](Self::Path) the reason the selected
/// node could not be named as a valid absolute path.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum ModeError {
    /// The directory is empty, so there is no selected entry to change.
    NoSelection,
    /// The requested mode carries a bit above [`FS_MODE_MASK`] — a file-type
    /// or other bit that is not part of the settable permission word.
    Invalid,
    /// The selected entry could not be spelled as a valid, bounded absolute
    /// path (the same fail-closed outcome opening it already produces).
    Path(Errno),
    /// The VFS refused the change (the user does not own the node, a read-only
    /// mount, a lost race); the node's mode is unchanged.
    Refused(Errno),
}

impl ModeError {
    /// A terse, human-readable reason for the in-UI refusal line (§2.24 — a
    /// denied action is an honest answer, never a silent failure). It names no
    /// path and carries no secret.
    #[must_use]
    pub const fn message(self) -> &'static str {
        match self {
            Self::NoSelection => "Nothing selected to change.",
            Self::Invalid => "Those permission bits are not allowed.",
            Self::Path(_) => "That item's location could not be resolved.",
            Self::Refused(_) => "The permission change was refused.",
        }
    }
}

/// Validate `mode` as a settable permission word.
///
/// Pure and fail-closed: a word carrying any bit above
/// [`FS_MODE_MASK`] is refused with
/// [`ModeError::Invalid`] rather than masked, so the mode a caller commits is
/// always exactly the one it asked for — the same rule the `fs_set_mode`
/// dispatcher enforces, checked here before the syscall. It performs no I/O
/// and makes no permission decision — that is the VFS's, at commit time.
///
/// # Errors
///
/// [`ModeError::Invalid`] if `mode` carries a bit outside [`FS_MODE_MASK`].
pub const fn validate_mode(mode: u32) -> Result<(), ModeError> {
    if mode & !FS_MODE_MASK != 0 {
        return Err(ModeError::Invalid);
    }
    Ok(())
}
