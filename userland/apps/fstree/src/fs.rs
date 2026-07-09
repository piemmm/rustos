//! The [`Fs`] seam: everything the file manager asks of the filesystem.
//!
//! The model is I/O-free; every directory read and free-space query goes
//! through this trait. The `Run` binary implements it over the
//! kernel-authorised `fs_*` syscalls (every per-inode and mount check stays
//! kernel-side, and a refusal comes back as the frozen [`Errno`] the model
//! surfaces on its message line); the tests implement it over an in-memory
//! tree, so the whole session is drivable without a kernel.

use alloc::string::String;
use alloc::vec::Vec;

use rustos_abi::time::Time64;
use rustos_abi::{Errno, FileKind};

/// One directory entry as the listing reports it: exactly the fields the
/// kernel's `fs_readdir` stream carries per entry, so a listing never costs
/// a per-child stat.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FsEntry {
    /// The entry's name within its directory (a single component).
    pub name: String,
    /// Whether the entry is a regular file or a directory.
    pub kind: FileKind,
    /// Apparent length in bytes; `0` for a directory.
    pub size: u64,
    /// Last contents-modification instant as the mounted format stores it;
    /// [`Time64::UNIX_EPOCH`] means the backing keeps no stamp and renders
    /// as absent, never as a fabricated date.
    pub modified: Time64,
}

/// Free/total byte counts of the volume backing a path, for the status
/// line. `None` when the query is unavailable (no sysinfo service, or the
/// caller lacks the query's capability) — the status line then simply
/// omits the figure; absence is never an error.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct VolumeSpace {
    /// Bytes still allocatable on the volume.
    pub free_bytes: u64,
    /// Total capacity of the volume in bytes.
    pub total_bytes: u64,
}

/// The filesystem operations the landed stages perform. Later stages
/// extend this trait in place with the operations they introduce (copy,
/// move, rename, delete, …) together with their callers.
pub trait Fs {
    /// List every entry of the directory `path`, in any order (the model
    /// sorts them).
    ///
    /// # Errors
    ///
    /// Any [`Errno`] the filesystem raises — e.g. [`Errno::NotFound`] for a
    /// vanished path or [`Errno::PermissionDenied`] when the caller may not
    /// list it. The model surfaces the error and keeps its previous state.
    fn list_dir(&mut self, path: &str) -> Result<Vec<FsEntry>, Errno>;

    /// Report the free/total space of the volume backing `path`, or `None`
    /// when the figure is unavailable (best-effort; never an error).
    fn volume_space(&mut self, path: &str) -> Option<VolumeSpace>;

    /// The current permission bits of the entry at `path` (a resolve-only
    /// stat — no read authority is requested), for the mode editor's
    /// pre-filled prompt.
    ///
    /// # Errors
    ///
    /// Any [`Errno`] the filesystem raises; the model surfaces it and
    /// opens no prompt.
    fn stat_mode(&mut self, path: &str) -> Result<u32, Errno>;

    /// Set the permission bits of the entry at `path` to `mode` (at most
    /// [`rustos_abi::FS_MODE_MASK`]). The kernel owns the authorisation:
    /// only the entry's owner may change its mode, and a refusal comes
    /// back as the frozen [`Errno`] the model surfaces unchanged.
    ///
    /// # Errors
    ///
    /// Any [`Errno`] the filesystem raises; the model reports it and
    /// changes nothing.
    fn set_mode(&mut self, path: &str, mode: u32) -> Result<(), Errno>;
}
