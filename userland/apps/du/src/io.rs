//! The seams through which `du` touches the outside world.
//!
//! Keeping the filesystem walk and the two output streams behind
//! object-safe traits is what lets the usage-summing logic in
//! [`crate::client`] run against in-memory fixtures with no kernel,
//! mirroring the seam design of the other userland tools (`ls`'s
//! `Listing`, `wc`'s `FileSource`/`Output`).

use alloc::string::String;
use alloc::vec::Vec;
use rustos_abi::{Errno, FileKind};

/// The metadata of one filesystem node, as `du` consumes it: what it is,
/// its apparent byte length, and the on-disk bytes its data actually
/// occupies (the `fs_stat` `allocated` field, which the mounted format
/// reports from its own accounting).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Metadata {
    /// Whether the node is a regular file or a directory.
    pub kind: FileKind,
    /// Apparent length in bytes (`--apparent-size`, `-b`).
    pub size: u64,
    /// Bytes of on-disk storage the node occupies (the default measure).
    pub allocated: u64,
}

/// One directory entry, as the walk consumes it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Entry {
    /// The entry's name (a single path component, never a path).
    pub name: String,
    /// Whether the entry is a regular file or a directory.
    pub kind: FileKind,
}

/// Stats paths and lists directories for the usage walk.
///
/// The implementation performs no authorisation of its own: the secured
/// VFS checks every path per-inode under the caller's attested identity,
/// and a refusal surfaces as the exact [`Errno`] the kernel chose.
pub trait Walk {
    /// The metadata of `path`.
    ///
    /// # Errors
    ///
    /// Any [`Errno`] the filesystem raises — e.g. [`Errno::NotFound`] for
    /// a missing path or [`Errno::PermissionDenied`] when the caller may
    /// not reach it.
    fn stat(&self, path: &str) -> Result<Metadata, Errno>;

    /// The entries of the directory at `path`.
    ///
    /// # Errors
    ///
    /// Any [`Errno`] the filesystem raises — e.g.
    /// [`Errno::PermissionDenied`] when the caller may not read it.
    fn read_dir(&self, path: &str) -> Result<Vec<Entry>, Errno>;
}

/// Writes bytes to one of the tool's output streams.
///
/// The client uses two instances: standard output for the usage rows, and
/// standard error for the per-path diagnostics it reports before moving on.
pub trait Output {
    /// Write every byte of `bytes` to the stream.
    ///
    /// # Errors
    ///
    /// Any [`Errno`] the stream raises (e.g. a closed terminal).
    fn write_all(&self, bytes: &[u8]) -> Result<(), Errno>;
}
