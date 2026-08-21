//! The seams through which `du` touches the outside world.
//!
//! Keeping the filesystem walk and the two output streams behind
//! object-safe traits is what lets the usage-summing logic in
//! [`crate::client`] run against in-memory fixtures with no kernel,
//! mirroring the seam design of the other userland tools (`ls`'s
//! `Listing`, `wc`'s `FileSource`/`Output`).

use alloc::string::String;
use alloc::vec::Vec;
use tairix_abi::{Errno, FileId, FileKind};

/// The metadata of one filesystem node, as `du` consumes it: what it is,
/// its apparent byte length, the on-disk bytes its data actually
/// occupies (the `fs_stat` `allocated` field, which the mounted format
/// reports from its own accounting), and the identity and name count that
/// tell a second *name* for the node from a second node.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Metadata {
    /// Whether the node is a regular file or a directory.
    pub kind: FileKind,
    /// Apparent length in bytes (`--apparent-size`, `-b`).
    pub size: u64,
    /// Bytes of on-disk storage the node occupies (the default measure).
    pub allocated: u64,
    /// The node's stable system-wide identity, so a file reached through
    /// two names is summed once. [`FileId::NONE`] for a backing that
    /// offers none, which is therefore never compared for equality.
    pub id: FileId,
    /// How many directory entries name this node, as the format records
    /// it. A node named once cannot be reached twice, which is what keeps
    /// the walk's seen-set to the hard links it actually meets.
    pub nlink: u32,
}

/// One directory entry, as the walk consumes it.
///
/// The entry carries the full [`Metadata`] the listing reported for it, so
/// the walk sums a directory's children from the **one** `fs_readdir`
/// listing instead of opening and statting each child by path — on an
/// uncached, authenticated volume every such stat is a fresh full path
/// resolution.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Entry {
    /// The entry's name (a single path component, never a path).
    pub name: String,
    /// The entry's metadata, as the listing filesystem reported it.
    pub meta: Metadata,
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

    /// The entries of the directory at `path`, each carrying the metadata
    /// the listing reported for it.
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
