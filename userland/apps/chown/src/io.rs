//! The seams through which `chown` touches the outside world, and the data
//! they carry.
//!
//! Keeping the filesystem and the terminal behind object-safe traits is what
//! lets the owner-changing logic in [`crate::client`] run against in-memory
//! fixtures with no kernel, mirroring the seam design of the other userland
//! crates (`init`'s `Spawner`/`Reaper`, `login`'s `Prompt`, `sysinfo`'s
//! `Transport`, `cat`'s `FileSource`, `ls`'s `Listing`, `rm`'s `Removal`,
//! `cp`'s and `mv`'s `FileSystem`, `chmod`'s `FileSystem`).

use alloc::string::String;
use rustos_abi::Errno;

/// What kind of object a path or directory entry is, as far as `chown` cares.
///
/// The only distinction `chown` needs is whether an object is a directory,
/// because `-R` descends into one. Everything else — a regular file, a
/// symbolic link, a device node — is treated as a non-directory.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EntryKind {
    /// A directory, whose entries `chown -R` descends into.
    Directory,
    /// Any non-directory object.
    File,
}

/// One directory entry: a name and its [`EntryKind`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Entry {
    /// The entry's name within its directory (not a full path, and never
    /// `.` or `..` — the seam does not surface those).
    pub name: String,
    /// What kind of object the entry is. The recursive descent reuses this so
    /// it never re-inspects a child it already learned the kind of.
    pub kind: EntryKind,
}

/// Inspects paths, changes their owner, and enumerates directories for `-R`.
///
/// The client [`stat`](FileSystem::stat)s each operand to learn whether it is
/// a directory, applies the new owner with
/// [`set_owner`](FileSystem::set_owner), and — for a recursive (`-R`) change
/// of a directory — enumerates it with [`read_dir`](FileSystem::read_dir),
/// calling it with an increasing `index` until it returns [`None`] and
/// recursing into each child.
pub trait FileSystem {
    /// Return the [`EntryKind`] of `path`.
    ///
    /// # Errors
    ///
    /// Any [`Errno`] the filesystem raises — e.g. [`Errno::NotFound`] for a
    /// missing path or [`Errno::PermissionDenied`] when the caller may not
    /// reach it.
    fn stat(&self, path: &str) -> Result<EntryKind, Errno>;

    /// Set the owning user and/or group of `path`.
    ///
    /// `uid` and `gid` are [`Some`] when that field is to change and [`None`]
    /// when it is to be left as it is, so an `OWNER`-only or `:GROUP`-only
    /// spec touches only the field it names.
    ///
    /// # Errors
    ///
    /// Any [`Errno`] the filesystem raises — e.g. [`Errno::PermissionDenied`]
    /// when the caller lacks the authority to change ownership.
    fn set_owner(&self, path: &str, uid: Option<u32>, gid: Option<u32>) -> Result<(), Errno>;

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
/// `chown` is silent on success; this seam carries only the usage banner.
pub trait Output {
    /// Write every byte of `bytes` to the terminal.
    ///
    /// # Errors
    ///
    /// Any [`Errno`] the console raises (e.g. a closed terminal).
    fn write_all(&self, bytes: &[u8]) -> Result<(), Errno>;
}
