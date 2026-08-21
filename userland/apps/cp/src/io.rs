//! The seams through which `cp` touches the outside world, and the data they
//! carry.
//!
//! Keeping the filesystem and the terminal behind object-safe traits is what
//! lets the copy logic in [`crate::client`] run against in-memory fixtures
//! with no kernel, mirroring the seam design of the other userland crates
//! (`init`'s `Spawner`/`Reaper`, `login`'s `Prompt`, `sysinfo`'s `Transport`,
//! `cat`'s `FileSource`, `ls`'s `Listing`, `rm`'s `Removal`).

use alloc::string::String;
use tairix_abi::{Errno, FileId};

/// What kind of object a path or directory entry is, as far as `cp` cares.
///
/// A directory is reproduced (with `-r`) by [`FileSystem::mkdir`] and a
/// recursive descent; a symbolic link is a third kind because `-P`/`-d`
/// reproduce the *link* rather than what it names, and `-s` creates one;
/// everything else — a regular file, a device node, a link followed to its
/// target — is copied as a stream of bytes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EntryKind {
    /// A directory, whose entries `cp -r` reproduces under the destination.
    Directory,
    /// Any non-directory, non-link object, copied as a byte stream.
    File,
    /// A symbolic link. Only ever reported under [`Follow::Keep`]: a
    /// following probe reports what the link names.
    Symlink,
}

/// Whether a probe describes the final component as typed or what it names.
///
/// `cp`'s default follows a final symbolic link — a copy of a link to a file
/// is a copy of the file — while `-P`/`-d` keep it, so the *link* is
/// reproduced. One operand rather than two seam methods, so the two
/// postures cannot drift apart.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Follow {
    /// Resolve a final symbolic link and describe what it names.
    Target,
    /// Describe the final component itself, so a link reports
    /// [`EntryKind::Symlink`].
    Keep,
}

/// What one probe learned about a path: what it is, and the identity that
/// tells a second *name* for one node from a second node.
///
/// The identity and name count ride along because the one `fs_stat` (or the
/// one directory listing) already reported them, and `--preserve=links`
/// needs exactly them: without the identity, two sources naming one node are
/// indistinguishable from two files and the copy would duplicate the data.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Probe {
    /// What kind of object it is.
    pub kind: EntryKind,
    /// Its stable system-wide identity. [`FileId::NONE`] for a backing that
    /// offers none, which is therefore never compared for equality.
    pub id: FileId,
    /// How many directory entries name the node. A node named once cannot
    /// be reached twice, which is what bounds `--preserve=links`' map to
    /// the hard links a copy actually meets.
    pub nlink: u32,
}

/// One directory entry: a name and what a probe of it found.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Entry {
    /// The entry's name within its directory (not a full path, and never
    /// `.` or `..` — the seam does not surface those).
    pub name: String,
    /// What the listing reported for it, as a [`Follow::Keep`] probe would:
    /// a listing describes each child itself, never what a link names.
    pub probe: Probe,
}

/// Inspects paths, reads sources, and creates destinations.
///
/// The client first asks [`probe`](FileSystem::probe) what a source is. A
/// non-directory source is streamed through [`read`](FileSystem::read) into a
/// destination created with [`create`](FileSystem::create) and filled with
/// [`write`](FileSystem::write) — unless a switch replaces the byte copy with
/// a link: `-l` calls [`link`](FileSystem::link), `-s` calls
/// [`symlink`](FileSystem::symlink), and `-P`/`-d` reproduce a link source
/// through [`read_link`](FileSystem::read_link) plus
/// [`symlink`](FileSystem::symlink). A directory source is reproduced by
/// creating the destination directory with [`mkdir`](FileSystem::mkdir) (when
/// it does not already exist), enumerating the source with
/// [`read_dir`](FileSystem::read_dir) — calling it with an increasing `index`
/// until it returns [`None`] — and recursing.
/// [`remove_file`](FileSystem::remove_file)
/// backs `-f`: a destination that cannot be created is removed and the create
/// retried once.
pub trait FileSystem {
    /// Describe `path` under the given follow posture.
    ///
    /// Under [`Follow::Target`] a final symbolic link is resolved, so a link
    /// to a regular file reports [`EntryKind::File`]; under [`Follow::Keep`]
    /// it reports [`EntryKind::Symlink`]. A missing path reports
    /// [`Errno::NotFound`], which the client treats as "absent" when probing
    /// a destination.
    ///
    /// # Errors
    ///
    /// Any [`Errno`] the filesystem raises — e.g. [`Errno::NotFound`] for a
    /// missing path or [`Errno::PermissionDenied`] when the caller may not
    /// reach it.
    fn probe(&self, path: &str, follow: Follow) -> Result<Probe, Errno>;

    /// The target the symbolic link at `path` stores, exactly as stored.
    ///
    /// Called only for a source a [`Follow::Keep`] probe reported as
    /// [`EntryKind::Symlink`], so `-P`/`-d` can reproduce the link by
    /// storing the same target. The target is data: it may be relative, may
    /// carry `..`, and may name nothing, and it is reproduced verbatim
    /// rather than resolved.
    ///
    /// # Errors
    ///
    /// Any [`Errno`] the filesystem raises while reading the link.
    fn read_link(&self, path: &str) -> Result<String, Errno>;

    /// Create the symbolic link `link` storing `target` verbatim.
    ///
    /// Backs `-s` (a link to each source) and the reproduction half of
    /// `-P`/`-d` (a link storing what the source link stored).
    ///
    /// # Errors
    ///
    /// Any [`Errno`] the filesystem raises — [`Errno::AlreadyExists`] for a
    /// taken name (a create never replaces one), or [`Errno::NotSupported`]
    /// on a format that stores no links.
    fn symlink(&self, target: &str, link: &str) -> Result<(), Errno>;

    /// Add `new` as a second directory entry for the node `existing` names.
    ///
    /// Backs `-l` (link each source instead of copying it) and
    /// `--preserve=links` (a second source naming one node becomes a second
    /// name at the destination rather than a second copy). Neither name is
    /// followed.
    ///
    /// # Errors
    ///
    /// Any [`Errno`] the filesystem raises — [`Errno::AlreadyExists`] for a
    /// taken name, [`Errno::CrossVolume`] when the two names are on
    /// different volumes, or [`Errno::NotSupported`] on a format that stores
    /// one name per node.
    fn link(&self, existing: &str, new: &str) -> Result<(), Errno>;

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
