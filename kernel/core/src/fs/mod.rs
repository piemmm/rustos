//! Virtual filesystem layer (`PLAN.md` Stage 5).
//!
//! `kernel/core::fs` owns the architecture-neutral VFS: absolute-path
//! resolution ([`path`]), the mount table and its per-mount permission
//! policy ([`mount`]), the per-inode permission model ([`perm`]), and the
//! [`Vfs`] tree that ties them together while enforcing the on-disk layout.
//!
//! The block-device and on-disk-format side of a filesystem lives in the
//! `drivers/filesystem/*` crates behind the
//! [`rustos_abi::driver::filesystem::Filesystem`] trait; this module is
//! the policy layer above them and does not duplicate their I/O. Until a
//! block-backed driver mounts, the [`Vfs`] is backed by an in-RAM node
//! arena — the natural shape of the boot-time root before storage comes
//! online.
//!
//! # Driver delegation
//!
//! A subtree may instead be backed by a `drivers/filesystem/*` driver: the
//! mount carries the driver's
//! [`DriverHandle`], and the
//! [`Vfs::read_via`] / [`Vfs::list_via`] / [`Vfs::stat_via`] methods route
//! resolution below the mount point to a
//! [`rustos_abi::driver::filesystem::FilesystemRead`] driver supplied by the
//! caller (the kernel maps the handle to the live driver). The driver
//! returns *structural* I/O only; the VFS remains the single policy
//! point, authorising every traversal against the mount point's
//! [`Metadata`] before and as it descends ([`DelegatedFs`]).
//!
//! # Layout enforcement
//!
//! * [`Vfs::with_default_layout`] provides exactly the four top-level
//!   directories the charter permits (`/System`, `/Users`, `/Apps`,
//!   `/Storage`) and mounts `/System` read-only with its `/System/Logs`
//!   and `/System/Settings` children as writable child mounts. The OS
//!   never authors the reserved legacy POSIX top-level names; refusing a
//!   user's own request to create one is not the VFS's job — a top-level
//!   create is governed by ordinary write permission on the root
//!   directory like any other.
//! * Writes to a read-only mount fail with [`VfsError::ReadOnly`].
//!
//! # Permission enforcement
//!
//! Every operation routes its access check through
//! [`perm::Metadata::authorize`]: capability gate, then ACL, then POSIX
//! mode bits, failing closed and never branching on `uid == 0`.

pub mod blkclient;
mod delegate;
mod fscache;
#[cfg(test)]
pub(crate) mod memfs;
pub mod mount;
mod mounted;
pub mod path;
pub mod perm;
pub mod retained;
pub mod service;
mod vfs;
pub mod volsvc;
pub mod volumes;

pub use blkclient::BlkClient;
pub use delegate::{DelegatedFs, DelegatedInfo, MetaPolicy, PerInode, Uniform};
pub use fscache::CachedFs;
pub use mount::{MountPoint, MountTable};
pub use mounted::{
    FilesystemAlreadyInstalled, IdentityAlreadyInstalled, LateFilesystem, LateIdentity,
    MountedFilesystemService,
};
pub use path::{
    resolve_machine_alias, Path, MAX_COMPONENT_LEN, MAX_PATH_COMPONENTS, ROOT_TEMPLATE,
};
pub use perm::{Access, AclEntry, AclWho, Credentials, Metadata, Mode};
pub use retained::{FlushBlock, JournaledBlock, RetainedWrites};
pub use service::{FilesystemService, NullFilesystemService, ReaddirEntry, NULL_FILESYSTEM};
pub use vfs::Vfs;
pub use volsvc::{NullVolumeService, VolumeService, NULL_VOLUME_SERVICE};
pub use volumes::{VolumeForest, VolumePublishError, NULL_VOLUME_FOREST};

use core::fmt;

use rustos_abi::driver::DriverHandle;
use rustos_abi::Errno;
use rustos_kernel_sec::{GroupId, UserId};

/// Handle for the kernel's *private root mount* — the in-memory [`Vfs`] a
/// boot-time reader builds to delegate to the mounted root volume's
/// driver.
///
/// The value only needs to be non-zero (the reader maps the handle to the
/// borrowed driver itself); it spells `root` so it is legible in a log.
/// It is defined here, once, so every boot reader that builds a
/// root-backed [`Vfs`] shares the same handle rather than carrying its own
/// copy.
pub(crate) const PRIVATE_ROOT_HANDLE: u64 = 0x726F_6F74;

/// Build a minimal [`Vfs`] whose root mount is backed by the caller's root
/// volume driver, ready for the `*_via_secured` delegation methods.
///
/// This is the shared shape of the real root volume — which carries the
/// whole tree from its own root directory — used by every boot-time
/// reader that resolves a path off the mounted root before the full mount
/// table exists (: one definition, no per-reader copy).
///
/// # Errors
///
/// [`VfsError::Io`] if the fixed [`PRIVATE_ROOT_HANDLE`] is somehow
/// rejected as a [`DriverHandle`] (it never is — the value is non-zero),
/// or the underlying [`MountTable::back_root`] refusal.
pub(crate) fn root_backed_vfs() -> Result<Vfs, VfsError> {
    let vfs = Vfs::new(Metadata::new(UserId(0), GroupId(0), Mode::from_bits(0o755)));
    let handle = DriverHandle::from_raw(PRIVATE_ROOT_HANDLE).map_err(|_| VfsError::Io)?;
    vfs.mounts_write().back_root(handle)?;
    Ok(vfs)
}

/// Why [`read_bootstrap_file`] could not return a file's exact bytes.
///
/// The structural refusals every boot-time reader of a `/System/Security`
/// database shares; each reader maps these onto its own load-error type.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub(crate) enum BootstrapReadError {
    /// Resolving, stat-ing, or reading the path failed (missing file,
    /// permission refusal, driver fault, …).
    Vfs(VfsError),
    /// The path names a directory, not a regular file.
    NotAFile,
    /// The file exceeds the caller's format bound; it is refused before any
    /// byte is read.
    TooLarge,
    /// The driver returned fewer bytes than the file's reported size; a
    /// truncated database is never parsed.
    ShortRead,
}

impl From<VfsError> for BootstrapReadError {
    fn from(err: VfsError) -> Self {
        Self::Vfs(err)
    }
}

/// Read the exact-size, fully-read bytes of `path` off the mounted root
/// volume under the kernel's capability-less `uid 0` bootstrap identity,
/// applying the permission check and the `max_len` size bound *before* a
/// single byte is read.
///
/// `uid 0` carries no ambient power: a read succeeds only because the
/// target's stored record makes it owner-readable, never because the kernel
/// bypasses the check. This is the one definition shared by every
/// `/System/Security` boot reader ([`crate::users`], [`crate::groups`]), so
/// the bounded, fail-closed read is not copied per file. The returned buffer
/// may carry credential bytes; the caller is responsible for zeroing it if
/// it does not retain it.
///
/// # Errors
///
/// The [`BootstrapReadError`] naming the first check that refused.
pub(crate) fn read_bootstrap_file<F>(
    fs: &mut F,
    path: &str,
    max_len: usize,
) -> Result<alloc::vec::Vec<u8>, BootstrapReadError>
where
    F: rustos_abi::driver::filesystem::FilesystemRead
        + rustos_abi::driver::filesystem::FilesystemSecurity
        + ?Sized,
{
    use rustos_abi::driver::filesystem::NodeKind;

    let vfs = root_backed_vfs()?;
    let caps = rustos_caps::CapabilitySet::empty();
    let cred = Credentials {
        uid: UserId(0),
        gid: GroupId(0),
        supplementary_gids: &[],
        caps: &caps,
    };
    let path = Path::parse(path)?;

    // Bound the file against the format's own maximum before reading a
    // single byte.
    let info = vfs.stat_via_secured(&cred, &path, fs)?;
    if info.kind != NodeKind::RegularFile {
        return Err(BootstrapReadError::NotAFile);
    }
    if info.size > max_len as u64 {
        return Err(BootstrapReadError::TooLarge);
    }
    let size = usize::try_from(info.size).map_err(|_| BootstrapReadError::TooLarge)?;

    let mut buf = alloc::vec![0u8; size];
    let read = vfs.read_via_secured(&cred, &path, fs, 0, &mut buf)?;
    if read != size {
        // A truncated file is never parsed; zero the partial read before
        // release in case it held credential bytes.
        buf.fill(0);
        return Err(BootstrapReadError::ShortRead);
    }
    Ok(buf)
}

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
    /// The covering mount is read-only.
    ReadOnly,
    /// A rename names a source and destination on different mounted
    /// volumes. A rename preserves the node's identity, which cannot span
    /// two independent backings; the mover falls back to copy-then-remove
    /// on exactly this refusal.
    CrossVolume,
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
    /// The conditions a userland tool must tell apart carry their own
    /// dedicated codes: an existing name is [`Errno::AlreadyExists`]
    /// (`EEXIST` — `mkdir` reports "File exists" and `mkdir -p` tolerates an
    /// existing directory), a non-directory where a directory is required is
    /// [`Errno::NotADirectory`] (`ENOTDIR`), and a populated directory is
    /// [`Errno::NotEmpty`] (`ENOTEMPTY` — `rmdir --ignore-fail-on-non-empty`
    /// tolerates exactly this). `abi-v1` has no dedicated `EISDIR`/`EINVAL`,
    /// so those collapse onto [`Errno::OutOfRange`]; the read-only refusal
    /// is reported as [`Errno::PermissionDenied`]. An unrecoverable backing
    /// fault ([`Self::Io`]) is [`Errno::DeviceFault`] — the `EIO` analogue,
    /// and what a surprise-removed volume's operations report — mirroring
    /// how [`DriverError::DeviceFault`](rustos_abi::driver::DriverError)
    /// maps. The precise [`VfsError`] is retained in-kernel for logging.
    #[must_use]
    pub const fn to_errno(self) -> Errno {
        match self {
            Self::NotFound => Errno::NotFound,
            Self::PermissionDenied | Self::ReadOnly => Errno::PermissionDenied,
            Self::InvalidPath | Self::IsADirectory => Errno::OutOfRange,
            Self::NotADirectory => Errno::NotADirectory,
            Self::AlreadyExists => Errno::AlreadyExists,
            Self::NotEmpty => Errno::NotEmpty,
            Self::CrossVolume => Errno::CrossVolume,
            // An unrecoverable backing fault is reported as what it is: the
            // device failed (or vanished), never "interface not implemented".
            Self::Io => Errno::DeviceFault,
        }
    }
}

impl fmt::Display for VfsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match self {
            Self::InvalidPath => "invalid path",
            Self::NotFound => "not found",
            Self::NotADirectory => "not a directory",
            Self::IsADirectory => "is a directory",
            Self::AlreadyExists => "already exists",
            Self::NotEmpty => "directory not empty",
            Self::PermissionDenied => "permission denied",
            Self::ReadOnly => "read-only mount",
            Self::CrossVolume => "paths on different volumes",
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
        assert_eq!(
            VfsError::PermissionDenied.to_errno(),
            Errno::PermissionDenied
        );
        assert_eq!(VfsError::InvalidPath.to_errno(), Errno::OutOfRange);
        assert_eq!(VfsError::IsADirectory.to_errno(), Errno::OutOfRange);
        assert_eq!(VfsError::AlreadyExists.to_errno(), Errno::AlreadyExists);
        assert_eq!(VfsError::NotADirectory.to_errno(), Errno::NotADirectory);
        assert_eq!(VfsError::NotEmpty.to_errno(), Errno::NotEmpty);
        assert_eq!(VfsError::CrossVolume.to_errno(), Errno::CrossVolume);
        assert_eq!(VfsError::Io.to_errno(), Errno::DeviceFault);
    }

    #[test]
    fn display_is_non_empty_for_every_variant() {
        for e in [
            VfsError::InvalidPath,
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
