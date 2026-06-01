//! The seams through which `chmod` touches the outside world, and the data
//! they carry.
//!
//! Keeping the filesystem and the terminal behind object-safe traits is what
//! lets the mode-changing logic in [`crate::client`] run against in-memory
//! fixtures with no kernel, mirroring the seam design of the other userland
//! crates (`init`'s `Spawner`/`Reaper`, `login`'s `Prompt`, `sysinfo`'s
//! `Transport`, `cat`'s `FileSource`, `ls`'s `Listing`, `rm`'s `Removal`,
//! `cp`'s and `mv`'s `FileSystem`).

use alloc::string::String;
use rustos_abi::Errno;

/// What kind of object a path or directory entry is, as far as `chmod` cares.
///
/// The distinction `chmod` needs is twofold: a directory is descended into by
/// `-R`, and the symbolic `X` permission grants execute to a directory (or to
/// a file that already carries an execute bit). Everything else — a regular
/// file, a symbolic link, a device node — is treated as a non-directory.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EntryKind {
    /// A directory, whose entries `chmod -R` descends into.
    Directory,
    /// Any non-directory object.
    File,
}

/// The metadata `chmod` needs about a path: its kind and current mode.
///
/// A symbolic mode (`g+w`, `o-x`, `a=rx`, …) transforms the *current* mode, so
/// the engine reads it here before computing the new value. An octal mode is
/// absolute and ignores the current mode, but the kind is still used to decide
/// whether `-R` descends and how the `X` permission resolves.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Metadata {
    /// What kind of object the path is.
    pub kind: EntryKind,
    /// The current POSIX mode bits. Only the low twelve bits (`& 0o7777` —
    /// the `rwx` triples plus the setuid/setgid/sticky bits) are meaningful to
    /// `chmod`; the file-type bits are carried separately as [`EntryKind`].
    pub mode: u32,
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

/// Inspects paths, changes their mode, and enumerates directories for `-R`.
///
/// The client [`stat`](FileSystem::stat)s each operand to learn its kind and
/// current mode, computes the new mode, and applies it with
/// [`set_mode`](FileSystem::set_mode). For a recursive (`-R`) change of a
/// directory it then enumerates the directory with
/// [`read_dir`](FileSystem::read_dir) — calling it with an increasing `index`
/// until it returns [`None`] — and recurses into each child.
pub trait FileSystem {
    /// Return the [`Metadata`] of `path`.
    ///
    /// # Errors
    ///
    /// Any [`Errno`] the filesystem raises — e.g. [`Errno::NotFound`] for a
    /// missing path or [`Errno::PermissionDenied`] when the caller may not
    /// reach it.
    fn stat(&self, path: &str) -> Result<Metadata, Errno>;

    /// Set the low twelve mode bits (`mode & 0o7777`) of `path`.
    ///
    /// The file-type bits are not the caller's to change and must be ignored
    /// by an implementation.
    ///
    /// # Errors
    ///
    /// Any [`Errno`] the filesystem raises — e.g. [`Errno::PermissionDenied`]
    /// when the caller does not own the file.
    fn set_mode(&self, path: &str, mode: u32) -> Result<(), Errno>;

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
}

/// Writes rendered bytes to the terminal.
///
/// `chmod` is silent on success; this seam carries only the usage banner.
pub trait Output {
    /// Write every byte of `bytes` to the terminal.
    ///
    /// # Errors
    ///
    /// Any [`Errno`] the console raises (e.g. a closed terminal).
    fn write_all(&self, bytes: &[u8]) -> Result<(), Errno>;
}
