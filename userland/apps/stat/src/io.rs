//! The seams through which `stat` reaches the outside world, and the data
//! they carry.
//!
//! `stat` is a *reporter*: it reads facts and renders them. Keeping every
//! fact behind an object-safe trait is what lets the whole rendering surface
//! — every specifier, both vocabularies, and each refusal — run against
//! in-memory fixtures with no kernel, the seam discipline of the sibling
//! tools (`df`'s `Mounts`, `du`'s `Walk`, `ln`'s `FileSystem`).

use alloc::string::String;
use alloc::vec::Vec;

use tairix_abi::driver::filesystem::VolumeStats;
use tairix_abi::{Errno, FileStat};

/// The facts a filesystem answers about one path.
pub trait Filesystem {
    /// The node `path` names, described.
    ///
    /// `dereference` is `-L`: with it a final symbolic link is resolved and
    /// the report describes what it names; without it the link itself is
    /// described, which is what makes `stat` the tool that can see a link at
    /// all.
    ///
    /// # Errors
    ///
    /// Any [`Errno`] the kernel raises — [`Errno::NotFound`] for an absent
    /// name, or a permission refusal on a directory the resolution passes
    /// through.
    fn stat(&self, path: &str, dereference: bool) -> Result<FileStat, Errno>;

    /// The stored target of the symbolic link at `path`, exactly as stored.
    ///
    /// Read only for `%N`, which shows a link beside what it names.
    ///
    /// # Errors
    ///
    /// Any [`Errno`] the kernel raises, including [`Errno::OutOfRange`] when
    /// `path` is not a symbolic link.
    fn read_link(&self, path: &str) -> Result<String, Errno>;

    /// The canonical path of `path` — every link followed, every `..`
    /// applied — as the kernel itself resolves it.
    ///
    /// `%m` needs it: the mount point holding a path is the longest mount
    /// prefix of the path's *canonical* spelling, so a link into another
    /// volume reports the volume it really lands on rather than the one its
    /// name was typed under.
    ///
    /// # Errors
    ///
    /// Any [`Errno`] the kernel raises.
    fn canonicalize(&self, path: &str) -> Result<String, Errno>;
}

/// One mounted volume, as the report needs it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Mount {
    /// The mount point's path in the caller's own namespace.
    pub target: String,
    /// The filesystem type the mount was registered with, empty for a mount
    /// whose backing declares none.
    pub fstype: String,
    /// The volume's live usage and geometry, as its driver reports it.
    pub usage: VolumeStats,
}

/// Reads the system's mount table.
pub trait Mounts {
    /// Every mount the caller may see, in the order the system reports them.
    ///
    /// # Errors
    ///
    /// Any [`Errno`] the System Information service raises.
    fn list(&self) -> Result<Vec<Mount>, Errno>;
}

/// Resolves a numeric owner id to an account name (`%U`).
pub trait Names {
    /// The account name owning `uid`, or [`None`] when the directory holds
    /// no entry for it.
    ///
    /// A uid with no name renders as GNU's `UNKNOWN` rather than as the
    /// number, so a name field never quietly becomes a numeric one.
    fn user(&self, uid: u32) -> Option<String>;
}

/// Writes rendered bytes to one of the tool's output streams.
pub trait Output {
    /// Write all of `bytes`.
    ///
    /// # Errors
    ///
    /// Any [`Errno`] the stream raises; a partial write is an error.
    fn write_all(&self, bytes: &[u8]) -> Result<(), Errno>;
}
