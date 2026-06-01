//! The seams through which `ls` touches the outside world, and the data they
//! carry.
//!
//! Keeping the filesystem and the terminal behind object-safe traits is what
//! lets the listing logic in [`crate::client`] run against in-memory fixtures
//! with no kernel, mirroring the seam design of the other userland crates
//! (`init`'s `Spawner`/`Reaper`, `login`'s `Prompt`, `sysinfo`'s `Transport`,
//! `cat`'s `FileSource`).

use alloc::string::String;
use rustos_abi::Errno;

/// What kind of object a directory entry or operand is.
///
/// The set is deliberately small — `ls` only needs enough to choose the
/// type character of the long format and to decide whether an operand is a
/// directory to descend into.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EntryKind {
    /// A directory, whose entries `ls` lists.
    Directory,
    /// A regular file.
    RegularFile,
    /// A symbolic link.
    Symlink,
    /// Anything else the filesystem reports (device node, socket, …).
    Other,
}

/// The metadata `ls` renders for a path: its kind, permission bits, and size.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Metadata {
    /// What kind of object the path is.
    pub kind: EntryKind,
    /// The POSIX mode bits. Only the low permission bits (`& 0o777`) are
    /// rendered by the long format; higher bits are ignored.
    pub mode: u32,
    /// The size of the object in bytes.
    pub size: u64,
}

/// One directory entry: a name and its [`Metadata`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Entry {
    /// The entry's name within its directory (not a full path).
    pub name: String,
    /// The entry's metadata.
    pub meta: Metadata,
}

/// Inspects paths and reads directories.
///
/// The client first [`stat`](Listing::stat)s each operand to learn whether it
/// is a directory, then — for directories — calls
/// [`read_dir`](Listing::read_dir) with an increasing `index` until it returns
/// [`None`], which marks the end of the directory.
pub trait Listing {
    /// Return the [`Metadata`] of `path`.
    ///
    /// # Errors
    ///
    /// Any [`Errno`] the filesystem raises — e.g. [`Errno::NotFound`] for a
    /// missing path or [`Errno::PermissionDenied`] when the caller may not
    /// reach it.
    fn stat(&self, path: &str) -> Result<Metadata, Errno>;

    /// Return the entry at position `index` in the directory `path`, or
    /// [`None`] once `index` is past the last entry.
    ///
    /// The client reads a directory by calling this with `index` `0, 1, 2, …`
    /// until it returns [`None`]; the order in which entries are returned is
    /// not significant because the client sorts them.
    ///
    /// # Errors
    ///
    /// Any [`Errno`] the filesystem raises while reading the directory.
    fn read_dir(&self, path: &str, index: u64) -> Result<Option<Entry>, Errno>;
}

/// Writes rendered bytes to the terminal.
///
/// The client hands [`write_all`](Output::write_all) the whole rendered
/// listing in one call, so a fixture can capture the exact byte stream.
pub trait Output {
    /// Write every byte of `bytes` to the terminal.
    ///
    /// # Errors
    ///
    /// Any [`Errno`] the console raises (e.g. a closed terminal).
    fn write_all(&self, bytes: &[u8]) -> Result<(), Errno>;
}
