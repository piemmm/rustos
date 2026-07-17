//! The seams through which `cp` touches the outside world, and the data they
//! carry.
//!
//! Keeping the filesystem and the terminal behind object-safe traits is what
//! lets the copy logic in [`crate::client`] run against in-memory fixtures
//! with no kernel, mirroring the seam design of the other userland crates
//! (`init`'s `Spawner`/`Reaper`, `login`'s `Prompt`, `sysinfo`'s `Transport`,
//! `cat`'s `FileSource`, `ls`'s `Listing`, `rm`'s `Removal`).

use alloc::string::String;
use tairix_abi::Errno;

/// What kind of object a path or directory entry is, as far as `cp` cares.
///
/// The distinction `cp` needs is only "directory or not": a directory is
/// reproduced (with `-r`) by [`FileSystem::mkdir`] and a recursive descent,
/// while everything else — a regular file, a symbolic link followed to its
/// target, a device node — is copied as a stream of bytes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EntryKind {
    /// A directory, whose entries `cp -r` reproduces under the destination.
    Directory,
    /// Any non-directory object, copied as a byte stream.
    File,
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

/// Inspects paths, reads sources, and creates destinations.
///
/// The client first asks [`kind`](FileSystem::kind) what a source is. A
/// non-directory source is streamed through [`read`](FileSystem::read) into a
/// destination created with [`create`](FileSystem::create) and filled with
/// [`write`](FileSystem::write). A directory source is reproduced by creating
/// the destination directory with [`mkdir`](FileSystem::mkdir) (when it does
/// not already exist), enumerating the source with
/// [`read_dir`](FileSystem::read_dir) — calling it with an increasing `index`
/// until it returns [`None`] — and recursing. [`remove_file`](FileSystem::remove_file)
/// backs `-f`: a destination that cannot be created is removed and the create
/// retried once.
pub trait FileSystem {
    /// Return the [`EntryKind`] of `path`.
    ///
    /// A final symbolic link is followed: a link to a regular file reports
    /// [`EntryKind::File`]. A missing path reports [`Errno::NotFound`], which
    /// the client treats as "absent" when probing a destination.
    ///
    /// # Errors
    ///
    /// Any [`Errno`] the filesystem raises — e.g. [`Errno::NotFound`] for a
    /// missing path or [`Errno::PermissionDenied`] when the caller may not
    /// reach it.
    fn kind(&self, path: &str) -> Result<EntryKind, Errno>;

    /// Read up to `buf.len()` bytes of `path` starting at `offset`, returning
    /// the number of bytes written into `buf` (`0` at end-of-file).
    ///
    /// An implementation must return at most `buf.len()` bytes and must report
    /// a short read (fewer than requested) only at end-of-file.
    ///
    /// # Errors
    ///
    /// Any [`Errno`] the filesystem raises while reading the file.
    fn read(&self, path: &str, offset: u64, buf: &mut [u8]) -> Result<usize, Errno>;

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

    /// Create the directory `path`.
    ///
    /// The client calls this only after observing that `path` does not exist.
    ///
    /// # Errors
    ///
    /// Any [`Errno`] the filesystem raises — e.g. [`Errno::PermissionDenied`].
    fn mkdir(&self, path: &str) -> Result<(), Errno>;

    /// Create (or truncate to empty) the regular file `path`, ready to be
    /// written from offset `0`.
    ///
    /// # Errors
    ///
    /// Any [`Errno`] the filesystem raises — e.g. [`Errno::PermissionDenied`]
    /// for an unwritable destination (which `-f` recovers from by removing it
    /// and retrying).
    fn create(&self, path: &str) -> Result<(), Errno>;

    /// Write every byte of `bytes` to `path` starting at `offset`.
    ///
    /// # Errors
    ///
    /// Any [`Errno`] the filesystem raises while writing.
    fn write(&self, path: &str, offset: u64, bytes: &[u8]) -> Result<(), Errno>;

    /// Remove the non-directory object at `path` (unlink one link).
    ///
    /// Used to back `-f`: when [`create`](FileSystem::create) fails on an
    /// existing destination, the client removes it and retries the create.
    ///
    /// # Errors
    ///
    /// Any [`Errno`] the filesystem raises — e.g. [`Errno::PermissionDenied`].
    fn remove_file(&self, path: &str) -> Result<(), Errno>;
}

/// Writes rendered bytes to the terminal.
///
/// `cp` is silent on success unless `-v` reports each copy; this seam also
/// carries the usage banner.
pub trait Output {
    /// Write every byte of `bytes` to the terminal.
    ///
    /// # Errors
    ///
    /// Any [`Errno`] the console raises (e.g. a closed terminal).
    fn write_all(&self, bytes: &[u8]) -> Result<(), Errno>;
}

/// Asks the interactive confirmation question (`-i`).
///
/// The production implementation writes `cp: <question> ` to standard
/// error and reads one line from standard input, answering `true` only
/// for an affirmative reply (a leading `y`/`Y`), matching the GNU tool.
/// A declined question skips the copy; an unanswerable one fails closed —
/// it is never treated as consent.
pub trait Prompt {
    /// Ask `question` and return whether the user consented.
    ///
    /// # Errors
    ///
    /// Any [`Errno`] the console raises — the caller fails closed.
    fn confirm(&self, question: &str) -> Result<bool, Errno>;
}
