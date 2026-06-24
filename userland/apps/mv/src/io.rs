//! The seams through which `mv` touches the outside world, and the data they
//! carry.
//!
//! Keeping the filesystem and the terminal behind object-safe traits is what
//! lets the move logic in [`crate::client`] run against in-memory fixtures
//! with no kernel, mirroring the seam design of the other userland crates
//! (`init`'s `Spawner`/`Reaper`, `login`'s `Prompt`, `sysinfo`'s `Transport`,
//! `cat`'s `FileSource`, `ls`'s `Listing`, `rm`'s `Removal`, `cp`'s
//! `FileSystem`).

use alloc::string::String;
use rustos_abi::Errno;

/// What kind of object a path or directory entry is, as far as `mv` cares.
///
/// The distinction `mv` needs only matters on the cross-device fallback: a
/// directory is reproduced by [`FileSystem::mkdir`] and a recursive descent,
/// while everything else — a regular file, a symbolic link followed to its
/// target, a device node — is copied as a stream of bytes. An in-filesystem
/// rename moves either kind atomically without caring which it is.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EntryKind {
    /// A directory, whose entries the cross-device fallback reproduces under
    /// the destination before removing the source.
    Directory,
    /// Any non-directory object, copied as a byte stream on the fallback.
    File,
}

/// The result of a successful [`FileSystem::rename`].
///
/// A rename within a single filesystem is atomic and is the whole move. A
/// rename whose source and destination live on different filesystems cannot
/// be atomic; rather than overload an [`Errno`], the seam reports
/// [`RenameOutcome::CrossDevice`] so the engine can fall back to the POSIX
/// copy-then-remove relocation. This keeps the boundary case an explicit,
/// non-error outcome rather than a magic error code.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RenameOutcome {
    /// The source was renamed onto the destination atomically; the move is
    /// complete.
    Renamed,
    /// The source and destination are on different filesystems, so an atomic
    /// rename is impossible. The engine must copy the source to the
    /// destination and then remove the source.
    CrossDevice,
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

/// Inspects paths, renames them, and — for the cross-device fallback — reads
/// sources, creates destinations, and removes sources.
///
/// The client first asks [`kind`](FileSystem::kind) what a source is, then
/// tries [`rename`](FileSystem::rename). A [`RenameOutcome::Renamed`] is the
/// whole move. A [`RenameOutcome::CrossDevice`] drives the fallback: a
/// non-directory source is streamed through [`read`](FileSystem::read) into a
/// destination created with [`create`](FileSystem::create) and filled with
/// [`write`](FileSystem::write); a directory source is reproduced with
/// [`mkdir`](FileSystem::mkdir) and a recursive walk
/// ([`read_dir`](FileSystem::read_dir)); then the source is removed depth-first
/// with [`remove_file`](FileSystem::remove_file) and
/// [`remove_dir`](FileSystem::remove_dir).
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

    /// Rename `source` onto `dest`, replacing an existing `dest` of a
    /// compatible kind.
    ///
    /// Returns [`RenameOutcome::Renamed`] when the rename completed
    /// atomically, or [`RenameOutcome::CrossDevice`] when `source` and `dest`
    /// live on different filesystems and the caller must relocate by copying
    /// and removing instead.
    ///
    /// # Errors
    ///
    /// Any [`Errno`] the filesystem raises — e.g. [`Errno::PermissionDenied`]
    /// for an unwritable destination directory.
    fn rename(&self, source: &str, dest: &str) -> Result<RenameOutcome, Errno>;

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
    /// Any [`Errno`] the filesystem raises — e.g. [`Errno::PermissionDenied`].
    fn create(&self, path: &str) -> Result<(), Errno>;

    /// Write every byte of `bytes` to `path` starting at `offset`.
    ///
    /// # Errors
    ///
    /// Any [`Errno`] the filesystem raises while writing.
    fn write(&self, path: &str, offset: u64, bytes: &[u8]) -> Result<(), Errno>;

    /// Remove the non-directory object at `path` (unlink one link).
    ///
    /// # Errors
    ///
    /// Any [`Errno`] the filesystem raises — e.g. [`Errno::PermissionDenied`].
    fn remove_file(&self, path: &str) -> Result<(), Errno>;

    /// Remove the directory `path`, which the client has already emptied.
    ///
    /// # Errors
    ///
    /// Any [`Errno`] the filesystem raises — e.g. [`Errno::PermissionDenied`].
    fn remove_dir(&self, path: &str) -> Result<(), Errno>;
}

/// Writes rendered bytes to the terminal.
///
/// `mv` is silent on success; this seam carries only the usage banner.
pub trait Output {
    /// Write every byte of `bytes` to the terminal.
    ///
    /// # Errors
    ///
    /// Any [`Errno`] the console raises (e.g. a closed terminal).
    fn write_all(&self, bytes: &[u8]) -> Result<(), Errno>;
}
