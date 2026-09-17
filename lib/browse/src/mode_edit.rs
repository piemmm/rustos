//! The **permission-edit** model (`plans/NEW-FILEMANAGER.md` FM8b): the pure,
//! host-tested core of committing a new permission mode to the selected node.
//!
//! Changing a node's permissions is modelled here so the one rule that decides
//! whether a requested mode word is acceptable — it may carry only the bits
//! [`fs_set_mode`](tairix_abi::SyscallNumber::FS_SET_MODE) itself accepts,
//! the [`FS_MODE_MASK`] `rwx`/setuid/setgid/sticky
//! bits — runs in `cargo test` with no kernel. The app supplies only the
//! `fs_set_mode` seam and the permission control; [`set_mode`] is what
//! validates and then calls it.
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
/// [`Invalid`](Self::Invalid) is decided *before* any syscall, so nothing is
/// changed. [`Refused`](Self::Refused) carries the kernel's own reason for a
/// failure at the VFS call.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum ModeError {
    /// The requested mode carries a bit above [`FS_MODE_MASK`] — a file-type
    /// or other bit that is not part of the settable permission word.
    Invalid,
    /// The VFS refused the change (the user does not own the node, a read-only
    /// mount, a lost race); the node's mode is unchanged.
    Refused(Errno),
}

impl ModeError {
    /// A terse, human-readable reason for the in-UI refusal line (a denied
    /// action is an honest answer, never a silent failure). It names no path
    /// and carries no secret.
    #[must_use]
    pub const fn message(self) -> &'static str {
        match self {
            Self::Invalid => "Those permission bits are not allowed.",
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

/// Change the node at absolute `path` to permission `mode`, applying it
/// through the injected `set_mode` seam (the `chmod(2)` shape).
///
/// Transactional and fail closed: the word is validated ([`validate_mode`])
/// *before* any syscall, so an out-of-range request changes nothing rather
/// than being masked into a different mode. `set_mode` performs the
/// `fs_set_mode` syscall under the caller's own identity — the per-inode
/// owner/mode/ACL model gates it and this adds no authority. A VFS refusal
/// leaves the node's mode exactly as it was.
///
/// # Errors
///
/// [`ModeError::Invalid`] for a word carrying a bit outside [`FS_MODE_MASK`]
/// (decided before the syscall), or [`ModeError::Refused`] when the VFS
/// refuses the change.
pub fn set_mode<F>(path: &str, mode: u32, set_mode: F) -> Result<(), ModeError>
where
    F: FnOnce(&str, u32) -> Result<(), Errno>,
{
    validate_mode(mode)?;
    set_mode(path, mode).map_err(ModeError::Refused)
}
