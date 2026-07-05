//! The seams through which `rm` touches the outside world, and the data they
//! carry.
//!
//! Keeping the filesystem and the terminal behind object-safe traits is what
//! lets the removal logic in [`crate::client`] run against in-memory fixtures
//! with no kernel, mirroring the seam design of the other userland crates
//! (`init`'s `Spawner`/`Reaper`, `login`'s `Prompt`, `sysinfo`'s `Transport`,
//! `cat`'s `FileSource`, `ls`'s `Listing`).

use alloc::string::String;
use rustos_abi::Errno;

/// What kind of object a path or directory entry is, as far as `rm` cares.
///
/// The distinction `rm` needs is only "directory or not": a directory is
/// descended (with `-r`) and removed with [`Removal::remove_dir`], while
/// everything else — a regular file, a symbolic link (removed, never
/// followed), a device node — is removed with [`Removal::remove_file`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EntryKind {
    /// A directory, whose contents `rm -r` removes before the directory.
    Directory,
    /// Any non-directory object, removed in place as a single link.
    Other,
}

/// One directory entry: a name and its [`EntryKind`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Entry {
    /// The entry's name within its directory (not a full path, and never
    /// `.` or `..` — the seam does not surface those).
    pub name: String,
    /// What kind of object the entry is.
    pub kind: EntryKind,
}

/// Inspects paths, reads directories, and removes objects.
///
/// The client first asks [`kind`](Removal::kind) what an operand is. For a
/// directory it removes (with `-r`), it reads the entries with
/// [`read_dir`](Removal::read_dir) — calling it with an increasing `index`
/// until it returns [`None`] — recurses, and then removes the now-empty
/// directory with [`remove_dir`](Removal::remove_dir). A non-directory is
/// removed with [`remove_file`](Removal::remove_file).
pub trait Removal {
    /// Return the [`EntryKind`] of `path`.
    ///
    /// This does not follow a final symbolic link: a symlink reports
    /// [`EntryKind::Other`] so `rm` removes the link itself.
    ///
    /// # Errors
    ///
    /// Any [`Errno`] the filesystem raises — e.g. [`Errno::NotFound`] for a
    /// missing path or [`Errno::PermissionDenied`] when the caller may not
    /// reach it.
    fn kind(&self, path: &str) -> Result<EntryKind, Errno>;

    /// Return the entry at position `index` in the directory `path`, or
    /// [`None`] once `index` is past the last entry.
    ///
    /// The client reads a directory by calling this with `index` `0, 1, 2, …`
    /// until it returns [`None`]. The entries `.` and `..` are never
    /// returned.
    ///
    /// # Errors
    ///
    /// Any [`Errno`] the filesystem raises while reading the directory.
    fn read_dir(&self, path: &str, index: u64) -> Result<Option<Entry>, Errno>;

    /// Remove the non-directory object at `path` (unlink one link).
    ///
    /// # Errors
    ///
    /// Any [`Errno`] the filesystem raises — e.g. [`Errno::PermissionDenied`].
    fn remove_file(&self, path: &str) -> Result<(), Errno>;

    /// Remove the directory at `path`, which the client has already emptied.
    ///
    /// # Errors
    ///
    /// Any [`Errno`] the filesystem raises — e.g. [`Errno::PermissionDenied`].
    fn remove_dir(&self, path: &str) -> Result<(), Errno>;
}

/// Writes rendered bytes to the terminal.
///
/// `rm` is silent on success unless `-v` reports each removal; this seam
/// also carries the usage banner.
pub trait Output {
    /// Write every byte of `bytes` to the terminal.
    ///
    /// # Errors
    ///
    /// Any [`Errno`] the console raises (e.g. a closed terminal).
    fn write_all(&self, bytes: &[u8]) -> Result<(), Errno>;
}

/// Asks the interactive confirmation questions (`-i` / `-I`).
///
/// The production implementation writes `rm: <question> ` to standard
/// error and reads one line from standard input, answering `true` only
/// for an affirmative reply (a leading `y`/`Y`), matching the GNU tool.
/// A declined question skips the object; an unanswerable one fails
/// closed — it is never treated as consent.
pub trait Prompt {
    /// Ask `question` and return whether the user consented.
    ///
    /// # Errors
    ///
    /// Any [`Errno`] the console raises — the caller fails closed.
    fn confirm(&self, question: &str) -> Result<bool, Errno>;
}
