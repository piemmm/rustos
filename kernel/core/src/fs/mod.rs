//! Virtual filesystem layer (`PLAN.md` Stage 5).
//!
//! `kernel/core::fs` owns the architecture-neutral VFS: absolute-path
//! resolution ([`path`]), the mount table and its per-mount permission
//! policy ([`mount`]), the per-inode permission model ([`perm`]), and the
//! [`Vfs`] tree that ties them together while enforcing the `AGENTS.md`
//! §16 on-disk layout.
//!
//! The block-device and on-disk-format side of a filesystem lives in the
//! `drivers/filesystem/*` crates behind the
//! [`rustos_abi::driver::filesystem::Filesystem`] trait; this module is
//! the policy layer above them and does not duplicate their I/O. Until a
//! block-backed driver mounts, the [`Vfs`] is backed by an in-RAM node
//! arena — the natural shape of the boot-time root before storage comes
//! online.
//!
//! # Driver delegation (`AGENTS.md` §2.4 / §5.4)
//!
//! A subtree may instead be backed by a `drivers/filesystem/*` driver: the
//! mount carries the driver's
//! [`DriverHandle`](rustos_abi::driver::DriverHandle), and the
//! [`Vfs::read_via`] / [`Vfs::list_via`] / [`Vfs::stat_via`] methods route
//! resolution below the mount point to a
//! [`rustos_abi::driver::filesystem::FilesystemRead`] driver supplied by the
//! caller (the kernel maps the handle to the live driver). The driver
//! returns *structural* I/O only; the VFS remains the single §5.3 policy
//! point, authorising every traversal against the mount point's
//! [`Metadata`] before and as it descends ([`DelegatedFs`]).
//!
//! # Layout enforcement (`AGENTS.md` §16)
//!
//! * The VFS refuses to create any reserved legacy POSIX top-level name
//!   ([`path::RESERVED_TOP_LEVEL`]) directly under the root, returning
//!   [`VfsError::ReservedPath`].
//! * [`Vfs::with_default_layout`] provides exactly the four top-level
//!   directories `AGENTS.md` §16.1 permits (`/System`, `/Users`, `/Apps`,
//!   `/Storage`) and mounts `/System` read-only with its `/System/Logs`
//!   and `/System/Settings` children as writable child mounts (§16.2).
//! * Writes to a read-only mount fail with [`VfsError::ReadOnly`].
//!
//! # Permission enforcement (`AGENTS.md` §5.3)
//!
//! Every operation routes its access check through
//! [`perm::Metadata::authorize`]: capability gate, then ACL, then POSIX
//! mode bits, failing closed and never branching on `uid == 0`.

mod delegate;
pub mod mount;
pub mod path;
pub mod perm;
mod vfs;

pub use delegate::{DelegatedFs, DelegatedInfo};
pub use mount::{MountPoint, MountTable};
pub use path::{
    is_reserved_top_level, Path, MAX_COMPONENT_LEN, MAX_PATH_COMPONENTS, RESERVED_TOP_LEVEL,
    ROOT_TEMPLATE,
};
pub use perm::{Access, AclEntry, AclWho, Credentials, Metadata, Mode};
pub use vfs::Vfs;

use core::fmt;

use rustos_abi::Errno;

/// An error returned by a VFS operation.
///
/// This is the kernel-internal error type; [`VfsError::to_errno`] maps it
/// to the stable user/kernel [`Errno`] for the syscall boundary. The
/// mapping is intentionally many-to-one: several structural refusals share
/// the closest stable code because `abi-v1` has no dedicated errno for
/// each, and the precise reason is preserved here for in-kernel logging.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub enum VfsError {
    /// The path is not absolute, has an empty/over-long component, or
    /// contains a `.`/`..`/NUL token.
    InvalidPath,
    /// The path names a reserved legacy POSIX top-level directory
    /// (`AGENTS.md` §16.1).
    ReservedPath,
    /// The named object does not exist.
    NotFound,
    /// A path component that must be a directory is not one.
    NotADirectory,
    /// The target is a directory where a file was required.
    IsADirectory,
    /// An object already exists at the target path.
    AlreadyExists,
    /// A directory targeted for removal still has entries.
    NotEmpty,
    /// The caller's credentials do not satisfy the inode's permission
    /// check (capability gate, ACL, or mode bits).
    PermissionDenied,
    /// The covering mount is read-only (`AGENTS.md` §16.2).
    ReadOnly,
    /// A driver backing a delegated mount reported an unrecoverable
    /// device fault, or returned a structurally invalid response (e.g. a
    /// directory entry whose name is not valid UTF-8). The in-RAM tree
    /// never produces this; it is reachable only through the
    /// driver-delegation path ([`Vfs::read_via`] and friends).
    Io,
}

impl VfsError {
    /// Map to the stable user/kernel [`Errno`].
    ///
    /// `abi-v1` has no dedicated code for `ENOTDIR`/`EISDIR`/`EEXIST`/
    /// `ENOTEMPTY`/`EINVAL`, so those collapse onto [`Errno::OutOfRange`];
    /// the read-only and reserved-name refusals are reported as
    /// [`Errno::PermissionDenied`]. `abi-v1` likewise has no dedicated
    /// `EIO`, so [`Self::Io`] collapses onto [`Errno::NotImplemented`],
    /// mirroring how [`DriverError::DeviceFault`](rustos_abi::driver::DriverError)
    /// already maps. The precise [`VfsError`] is retained in-kernel for
    /// logging.
    #[must_use]
    pub const fn to_errno(self) -> Errno {
        match self {
            Self::NotFound => Errno::NotFound,
            Self::PermissionDenied | Self::ReadOnly | Self::ReservedPath => Errno::PermissionDenied,
            Self::InvalidPath
            | Self::NotADirectory
            | Self::IsADirectory
            | Self::AlreadyExists
            | Self::NotEmpty => Errno::OutOfRange,
            Self::Io => Errno::NotImplemented,
        }
    }
}

impl fmt::Display for VfsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match self {
            Self::InvalidPath => "invalid path",
            Self::ReservedPath => "reserved top-level name",
            Self::NotFound => "not found",
            Self::NotADirectory => "not a directory",
            Self::IsADirectory => "is a directory",
            Self::AlreadyExists => "already exists",
            Self::NotEmpty => "directory not empty",
            Self::PermissionDenied => "permission denied",
            Self::ReadOnly => "read-only mount",
            Self::Io => "filesystem driver i/o error",
        };
        f.write_str(message)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn errno_mapping_is_stable() {
        assert_eq!(VfsError::NotFound.to_errno(), Errno::NotFound);
        assert_eq!(VfsError::ReadOnly.to_errno(), Errno::PermissionDenied);
        assert_eq!(VfsError::ReservedPath.to_errno(), Errno::PermissionDenied);
        assert_eq!(
            VfsError::PermissionDenied.to_errno(),
            Errno::PermissionDenied
        );
        assert_eq!(VfsError::InvalidPath.to_errno(), Errno::OutOfRange);
        assert_eq!(VfsError::AlreadyExists.to_errno(), Errno::OutOfRange);
        assert_eq!(VfsError::Io.to_errno(), Errno::NotImplemented);
    }

    #[test]
    fn display_is_non_empty_for_every_variant() {
        for e in [
            VfsError::InvalidPath,
            VfsError::ReservedPath,
            VfsError::NotFound,
            VfsError::NotADirectory,
            VfsError::IsADirectory,
            VfsError::AlreadyExists,
            VfsError::NotEmpty,
            VfsError::PermissionDenied,
            VfsError::ReadOnly,
            VfsError::Io,
        ] {
            assert!(!alloc::format!("{e}").is_empty());
        }
    }
}
