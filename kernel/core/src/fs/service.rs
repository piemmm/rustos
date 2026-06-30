//! The kernel-side filesystem-operation seam the userland `fs_*` syscalls
//! route through (`PREREQUISITES.md` P-A).
//!
//! The mounted volume's filesystem driver is owned for the life of the
//! system by the single disk-owning kthread (`rustos-kernel`'s driver-store
//! service); the block device cannot be shared and an operation may park on
//! the device completion IRQ. The `fs_*` syscall handlers therefore do not
//! borrow the filesystem directly — they call this [`FilesystemService`],
//! whose production implementation routes each request to that disk-owning
//! service. The handler supplies the caller's **kernel-attested** identity
//! (the owning uid and effective capability set, taken from the task's
//! [`rustos_kernel_sec::TaskCapabilities`], never anything the caller
//! supplied); the service resolves the caller's groups from the system
//! identity table and authorises every operation through the secured VFS, so
//! every per-inode owner/mode/ACL/capability and mount-flag check stays
//! kernel-side and fails closed.
//!
//! Until the boot path installs a real service the handlers hold
//! [`NULL_FILESYSTEM`], whose every operation fails closed with
//! [`Errno::NotImplemented`] — a kernel with no mounted filesystem never
//! fabricates a result.

use alloc::string::String;
use alloc::vec::Vec;

use rustos_abi::{CapabilityQuery, Errno, FileKind, FileStat, OpenFlags};

/// The set of secured filesystem operations a userland `fs_*` syscall needs.
///
/// Each method receives the caller's **attested** identity — `uid` is the
/// task's owning user id and `caps` its effective capability set, both
/// kernel-sourced — and the implementation builds the full VFS
/// [`Credentials`](crate::fs::Credentials) (resolving the caller's primary
/// and supplementary groups from the system identity table) before
/// authorising the operation. The implementation never trusts a
/// caller-supplied identity, and fails closed on every error and
/// uninitialised path.
pub trait FilesystemService: Send + Sync {
    /// Resolve `path` with `flags` under the caller's attested identity,
    /// applying the create/exclusive/truncate/directory semantics
    /// [`OpenFlags`] encodes, and confirm the access is permitted.
    ///
    /// The handler records the resulting handle only after this succeeds, so
    /// a refused open never produces a descriptor.
    ///
    /// # Errors
    ///
    /// The stable [`Errno`] the secured VFS maps the refusal to (a missing
    /// path, a permission or mount-flag denial, an existing path under
    /// [`OpenFlags::EXCLUSIVE`], a non-directory under
    /// [`OpenFlags::DIRECTORY`]), or [`Errno::NotImplemented`] when no
    /// filesystem is mounted.
    fn open(
        &self,
        uid: u32,
        caps: &dyn CapabilityQuery,
        path: &str,
        flags: OpenFlags,
    ) -> Result<(), Errno>;

    /// Read up to `buf.len()` bytes from `path` at byte `offset`, returning
    /// the number read (`0` at end of file).
    ///
    /// # Errors
    ///
    /// The stable [`Errno`] for the VFS refusal, or [`Errno::NotImplemented`]
    /// when no filesystem is mounted.
    fn read(
        &self,
        uid: u32,
        caps: &dyn CapabilityQuery,
        path: &str,
        offset: u64,
        buf: &mut [u8],
    ) -> Result<usize, Errno>;

    /// Write `data` to `path` at byte `offset`, or at the current end of file
    /// when `append`, returning the number of bytes written.
    ///
    /// # Errors
    ///
    /// The stable [`Errno`] for the VFS refusal (a read-only mount, a
    /// permission denial), or [`Errno::NotImplemented`] when no filesystem is
    /// mounted.
    fn write(
        &self,
        uid: u32,
        caps: &dyn CapabilityQuery,
        path: &str,
        offset: u64,
        append: bool,
        data: &[u8],
    ) -> Result<usize, Errno>;

    /// List the entries of the directory at `path` as `(kind, name)` pairs.
    ///
    /// # Errors
    ///
    /// The stable [`Errno`] for the VFS refusal (not a directory, permission
    /// denied), or [`Errno::NotImplemented`] when no filesystem is mounted.
    fn readdir(
        &self,
        uid: u32,
        caps: &dyn CapabilityQuery,
        path: &str,
    ) -> Result<Vec<(FileKind, String)>, Errno>;

    /// Report the structural metadata of the node at `path`.
    ///
    /// # Errors
    ///
    /// The stable [`Errno`] for the VFS refusal, or [`Errno::NotImplemented`]
    /// when no filesystem is mounted.
    fn stat(&self, uid: u32, caps: &dyn CapabilityQuery, path: &str) -> Result<FileStat, Errno>;

    /// Set the length of the regular file at `path` to `size` bytes.
    ///
    /// # Errors
    ///
    /// The stable [`Errno`] for the VFS refusal (a read-only mount, a
    /// directory, a permission denial), or [`Errno::NotImplemented`] when no
    /// filesystem is mounted.
    fn truncate(
        &self,
        uid: u32,
        caps: &dyn CapabilityQuery,
        path: &str,
        size: u64,
    ) -> Result<(), Errno>;

    /// Flush the mounted filesystem's pending writes to its backing store.
    ///
    /// # Errors
    ///
    /// The stable [`Errno`] for the driver refusal, or
    /// [`Errno::NotImplemented`] when no filesystem is mounted.
    fn sync(&self, uid: u32, caps: &dyn CapabilityQuery) -> Result<(), Errno>;

    /// Create a directory at the absolute `path`.
    ///
    /// # Errors
    ///
    /// The stable [`Errno`] for the VFS refusal (an existing path, a
    /// read-only mount, a permission denial), or [`Errno::NotImplemented`]
    /// when no filesystem is mounted.
    fn mkdir(&self, uid: u32, caps: &dyn CapabilityQuery, path: &str) -> Result<(), Errno>;

    /// Remove the file or empty directory at the absolute `path`.
    ///
    /// # Errors
    ///
    /// The stable [`Errno`] for the VFS refusal (a missing path, a non-empty
    /// directory, a read-only mount, a permission denial), or
    /// [`Errno::NotImplemented`] when no filesystem is mounted.
    fn unlink(&self, uid: u32, caps: &dyn CapabilityQuery, path: &str) -> Result<(), Errno>;

    /// Move the file or directory at absolute `src` to absolute `dst`,
    /// preserving its identity and contents. Both paths must lie under the
    /// same mounted volume.
    ///
    /// # Errors
    ///
    /// The stable [`Errno`] for the VFS refusal (a missing source, a
    /// read-only mount, a permission denial, a non-empty directory
    /// destination, a cross-mount move), or [`Errno::NotImplemented`] when
    /// no filesystem is mounted.
    fn rename(
        &self,
        uid: u32,
        caps: &dyn CapabilityQuery,
        src: &str,
        dst: &str,
    ) -> Result<(), Errno>;
}

/// The fail-closed default filesystem service: every operation reports
/// [`Errno::NotImplemented`].
///
/// Held by the syscall handlers until the boot path installs the real
/// disk-backed service (mirrors [`crate::users::NULL_USERS_DB`] and
/// [`crate::hwtree::NULL_HW_TREE`]). A kernel with no mounted filesystem thus
/// refuses every `fs_*` syscall rather than fabricating a handle or a read.
pub struct NullFilesystemService;

impl FilesystemService for NullFilesystemService {
    fn open(
        &self,
        _uid: u32,
        _caps: &dyn CapabilityQuery,
        _path: &str,
        _flags: OpenFlags,
    ) -> Result<(), Errno> {
        Err(Errno::NotImplemented)
    }

    fn read(
        &self,
        _uid: u32,
        _caps: &dyn CapabilityQuery,
        _path: &str,
        _offset: u64,
        _buf: &mut [u8],
    ) -> Result<usize, Errno> {
        Err(Errno::NotImplemented)
    }

    fn write(
        &self,
        _uid: u32,
        _caps: &dyn CapabilityQuery,
        _path: &str,
        _offset: u64,
        _append: bool,
        _data: &[u8],
    ) -> Result<usize, Errno> {
        Err(Errno::NotImplemented)
    }

    fn readdir(
        &self,
        _uid: u32,
        _caps: &dyn CapabilityQuery,
        _path: &str,
    ) -> Result<Vec<(FileKind, String)>, Errno> {
        Err(Errno::NotImplemented)
    }

    fn stat(&self, _uid: u32, _caps: &dyn CapabilityQuery, _path: &str) -> Result<FileStat, Errno> {
        Err(Errno::NotImplemented)
    }

    fn truncate(
        &self,
        _uid: u32,
        _caps: &dyn CapabilityQuery,
        _path: &str,
        _size: u64,
    ) -> Result<(), Errno> {
        Err(Errno::NotImplemented)
    }

    fn sync(&self, _uid: u32, _caps: &dyn CapabilityQuery) -> Result<(), Errno> {
        Err(Errno::NotImplemented)
    }

    fn mkdir(&self, _uid: u32, _caps: &dyn CapabilityQuery, _path: &str) -> Result<(), Errno> {
        Err(Errno::NotImplemented)
    }

    fn unlink(&self, _uid: u32, _caps: &dyn CapabilityQuery, _path: &str) -> Result<(), Errno> {
        Err(Errno::NotImplemented)
    }

    fn rename(
        &self,
        _uid: u32,
        _caps: &dyn CapabilityQuery,
        _src: &str,
        _dst: &str,
    ) -> Result<(), Errno> {
        Err(Errno::NotImplemented)
    }
}

/// The shared fail-closed filesystem service the handlers hold until the
/// boot path installs the real one.
pub static NULL_FILESYSTEM: NullFilesystemService = NullFilesystemService;
