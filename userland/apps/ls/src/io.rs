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
use tairix_abi::fs::{FileId, FileKind};
use tairix_abi::{Errno, NodeTimes};

/// Whether a stat describes a final symbolic link itself or what it resolves
/// to — POSIX `lstat` versus `stat`.
///
/// `ls` selects this **per path**, not once per listing: the GNU `-H` posture
/// dereferences command-line operands while describing the links inside a
/// directory as themselves, so one listing takes both readings. The spelling
/// is the VFS's own (`docs/src/filesystem/overview.md`), so the tool and the
/// kernel name the same distinction the same way.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FinalLink {
    /// Describe the link itself: the `l` type letter, the target the long
    /// format prints, and the only reading under which a *dangling* link can
    /// be described at all.
    Keep,
    /// Describe what the link resolves to. A dangling link is then simply
    /// absent, exactly as `stat(2)` reports it.
    Follow,
}

/// The metadata `ls` renders for a path: its kind, permission bits, size,
/// stable node number, and four timestamps — the
/// [`tairix_abi::fs::FileStat`] fields the listing actually shows.
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
    /// The node's stable identity: the volume it lives on and that volume's
    /// node number. `-i` / `--inode` renders the node number, and `-R`
    /// compares the whole pair to recognise a directory a link points back
    /// at — a node number alone repeats across volumes.
    pub id: FileId,
    /// The node's four `Time64` timestamps (created/modified/accessed/
    /// changed). The long format renders one of them (selected by
    /// `-c`/`-u`/`--time`) and `-t` sorts by it.
    pub times: NodeTimes,
}

/// One directory entry: a name and its kind — exactly what the kernel's
/// `fs_readdir` stream carries per entry. The long format's mode and size
/// come from a per-entry [`Listing::stat`], paid only when `-l` asks for
/// them.
///
/// The stream always reports a child's **own** kind, so a symbolic link
/// arrives as [`FileKind::Symlink`] whatever the listing's dereference
/// posture is; resolving it is the per-entry stat's job.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Entry {
    /// The entry's name within its directory (not a full path).
    pub name: String,
    /// What kind of object the entry is.
    pub kind: FileKind,
}

/// Inspects paths, reads directories, and reads a link's target.
///
/// The client first [`stat`](Listing::stat)s each operand to learn whether
/// it is a directory, then — for directories — calls
/// [`read_dir`](Listing::read_dir) once for the whole listing, mirroring
/// the kernel's own one-shot `fs_readdir` contract. A row the listing shows
/// as a link has its target read with [`read_link`](Listing::read_link),
/// which is the only way to learn it: a link's content is a path, not bytes.
pub trait Listing {
    /// Return the [`Metadata`] of `path`, describing a final symbolic link
    /// itself or its target as `links` selects.
    ///
    /// # Errors
    ///
    /// Any [`Errno`] the filesystem raises — e.g. [`Errno::NotFound`] for a
    /// missing path (or, under [`FinalLink::Follow`], a link that dangles)
    /// or [`Errno::PermissionDenied`] when the caller may not reach it.
    fn stat(&self, path: &str, links: FinalLink) -> Result<Metadata, Errno>;

    /// Return the stored target of the symbolic link at `path`, verbatim.
    ///
    /// The final component is never followed — the call is about the link —
    /// and the target comes back exactly as it was stored, still unresolved,
    /// which is what the long format prints after `name -> `.
    ///
    /// # Errors
    ///
    /// Any [`Errno`] the filesystem raises — [`Errno::OutOfRange`] when
    /// `path` names anything but a symbolic link, or [`Errno::NotSupported`]
    /// on a mount whose format stores no links.
    fn read_link(&self, path: &str) -> Result<String, Errno>;

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

    /// Write one diagnostic line to the error stream (fd 2), best-effort.
    ///
    /// This is how a listing *keeps going* past an entry it could not
    /// inspect — a dangling link under `-L`, a name the caller may not reach
    /// — reporting the reason where a reader and a script both see it while
    /// the rest of the listing still reaches standard output. The caller
    /// records that it happened and exits non-zero; a failure to write the
    /// diagnostic itself is not worth failing the listing over.
    fn error(&self, message: &str);

    /// Emit one framed `stdinfo` record on fd 3, best-effort: advisory by
    /// contract, so a missing consumer or short write is silently a no-op
    /// and never affects the listing or the exit status.
    fn info(&self, record: &[u8]);

    /// The character-cell width of the terminal backing standard output, if
    /// it is an attestable text console; `None` when standard output is a
    /// pipe, a file, or a console whose width the kernel cannot attest (a
    /// UART / remote terminal).
    ///
    /// This is the single signal that decides the GNU default arrangement:
    /// multiple columns (`-C`) when output is a terminal, one name per line
    /// otherwise. It also supplies the column budget when a width is not
    /// given with `-w` / `--width`. It is derived from the kernel's
    /// fail-closed geometry attestation, so an unattested console reports
    /// `None` and the listing degrades to plain one-per-line output rather
    /// than guessing a width.
    fn terminal_width(&self) -> Option<usize>;
}
