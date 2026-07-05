//! The seams through which `ls` touches the outside world, and the data they
//! carry.
//!
//! Keeping the filesystem and the terminal behind object-safe traits is what
//! lets the listing logic in [`crate::client`] run against in-memory fixtures
//! with no kernel, mirroring the seam design of the other userland crates
//! (`init`'s `Spawner`/`Reaper`, `login`'s `LoginView`, `sysinfo`'s `Transport`,
//! `cat`'s `FileSource`, `man`'s `BundleStore`).
//!
//! The vocabulary is the frozen `abi-v1` one: an entry's kind is the VFS's
//! own [`FileKind`] — the tool defines no parallel kind enum that could
//! drift from what the kernel actually reports.

use alloc::string::String;
use alloc::vec::Vec;
use rustos_abi::fs::FileKind;
use rustos_abi::Errno;

/// The metadata `ls` renders for a path: its kind, permission bits, and
/// size — the [`rustos_abi::fs::FileStat`] fields the listing actually
/// shows.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Metadata {
    /// What kind of object the path is.
    pub kind: FileKind,
    /// The POSIX mode bits. Only the low permission bits (`& 0o777`) are
    /// rendered by the long format; higher bits are ignored.
    pub mode: u32,
    /// The size of the object in bytes.
    pub size: u64,
    /// Bytes of on-disk storage the object's data occupies, as the
    /// filesystem reports it (`-s` and the `total` line render this,
    /// never a value derived from `size`).
    pub allocated: u64,
    /// The owning user id, rendered numerically by the long format.
    pub uid: u32,
    /// The owning group id, rendered numerically by the long format.
    pub gid: u32,
}

/// One directory entry: a name and its kind — exactly what the kernel's
/// `fs_readdir` stream carries per entry. The long format's mode and size
/// come from a per-entry [`Listing::stat`], paid only when `-l` asks for
/// them.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Entry {
    /// The entry's name within its directory (not a full path).
    pub name: String,
    /// What kind of object the entry is.
    pub kind: FileKind,
}

/// Inspects paths and reads directories.
///
/// The client first [`stat`](Listing::stat)s each operand to learn whether
/// it is a directory, then — for directories — calls
/// [`read_dir`](Listing::read_dir) once for the whole listing, mirroring
/// the kernel's own one-shot `fs_readdir` contract.
pub trait Listing {
    /// Return the [`Metadata`] of `path`.
    ///
    /// # Errors
    ///
    /// Any [`Errno`] the filesystem raises — e.g. [`Errno::NotFound`] for a
    /// missing path or [`Errno::PermissionDenied`] when the caller may not
    /// reach it.
    fn stat(&self, path: &str) -> Result<Metadata, Errno>;

    /// Return every entry of the directory `path`, in any order (the client
    /// sorts them).
    ///
    /// # Errors
    ///
    /// Any [`Errno`] the filesystem raises while reading the directory.
    fn read_dir(&self, path: &str) -> Result<Vec<Entry>, Errno>;
}

/// Writes rendered bytes to the terminal, and advisory records to the
/// standard information stream.
pub trait Output {
    /// Write every byte of `bytes` to the terminal.
    ///
    /// # Errors
    ///
    /// Any [`Errno`] the console raises (e.g. a closed terminal).
    fn write_all(&self, bytes: &[u8]) -> Result<(), Errno>;

    /// Emit one framed `stdinfo` record on fd 3, best-effort: advisory by
    /// contract, so a missing consumer or short write is silently a no-op
    /// and never affects the listing or the exit status.
    fn info(&self, record: &[u8]);
}
