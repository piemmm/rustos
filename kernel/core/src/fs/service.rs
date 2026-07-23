//! The kernel-side filesystem-operation seam the userland `fs_*` syscalls
//! route through (`PREREQUISITES.md` P-A).
//!
//! The mounted volume's filesystem driver is owned for the life of the
//! system by the single disk-owning kthread (`tairix-kernel`'s driver-store
//! service); the block device cannot be shared and an operation may park on
//! the device completion IRQ. The `fs_*` syscall handlers therefore do not
//! borrow the filesystem directly — they call this [`FilesystemService`],
//! whose production implementation routes each request to that disk-owning
//! service. The handler supplies the caller's **kernel-attested** identity
//! (the owning uid and effective capability set, taken from the task's
//! [`tairix_kernel_sec::TaskCapabilities`], never anything the caller
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

use tairix_abi::sysinfo::MountRecord;
use tairix_abi::time::Time64;
use tairix_abi::{CapabilityQuery, Errno, FileKind, FileStat, OpenFlags, UnlinkFlags};

/// One directory entry as [`FilesystemService::readdir`] reports it: the
/// child's kind, its apparent and allocated sizes, and its name.
///
/// The sizes ride along with the listing because the mounted filesystem
/// already holds each child's metadata while producing the entry; a
/// consumer that needs them (`du`) reads the one listing instead of
/// re-resolving every child by path, which on an uncached, authenticated
/// volume is a fresh full walk per child.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReaddirEntry {
    /// Whether the entry names a regular file or a directory.
    pub kind: FileKind,
    /// Apparent length in bytes; `0` for a directory.
    pub size: u64,
    /// Bytes of on-disk storage the entry's data occupies, as the mounted
    /// format's own allocation tracking reports it.
    pub allocated: u64,
    /// The entry's last contents-modification instant, as the mounted
    /// format stores it ([`Time64::UNIX_EPOCH`] for a backing with no
    /// per-node stamp).
    pub modified: Time64,
    /// The entry's name (a single component, never `.`/`..`).
    pub name: String,
}

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

    /// List the entries of the directory at `path`, each with the kind and
    /// sizes the mounted filesystem reports for it.
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
    ) -> Result<Vec<ReaddirEntry>, Errno>;

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
    /// With [`UnlinkFlags::DIRECTORY`] the removal succeeds only when the
    /// name is an (empty) directory — decided atomically by the filesystem
    /// under its own lock (the `rmdir` posture); a non-directory fails
    /// closed with [`Errno::NotADirectory`].
    ///
    /// # Errors
    ///
    /// The stable [`Errno`] for the VFS refusal (a missing path, a non-empty
    /// directory, a non-directory under [`UnlinkFlags::DIRECTORY`], a
    /// read-only mount, a permission denial), or [`Errno::NotImplemented`]
    /// when no filesystem is mounted.
    fn unlink(
        &self,
        uid: u32,
        caps: &dyn CapabilityQuery,
        path: &str,
        flags: UnlinkFlags,
    ) -> Result<(), Errno>;

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

    /// Set the permission bits of the node at the absolute `path` to `mode`
    /// (the `chmod(2)` shape), leaving ownership, ACL, and capability gate
    /// untouched.
    ///
    /// Only the node's owner may change its mode; holding a capability
    /// grants no override.
    ///
    /// # Errors
    ///
    /// The stable [`Errno`] for the VFS refusal (a missing path, a
    /// non-owner caller, a read-only mount), [`Errno::OutOfRange`] for a
    /// mode carrying a bit above the permission mask, or
    /// [`Errno::NotImplemented`] when no filesystem is mounted.
    fn set_mode(
        &self,
        uid: u32,
        caps: &dyn CapabilityQuery,
        path: &str,
        mode: u32,
    ) -> Result<(), Errno>;

    /// Set the owning user and/or group of the node at the absolute `path`
    /// (the `chown(2)` / `chgrp(2)` shape), leaving its mode's permission
    /// triads, ACL, and capability gate otherwise untouched. Either of
    /// `uid` / `gid` may be [`tairix_abi::fs::FS_OWNER_UNCHANGED`] to leave
    /// that field.
    ///
    /// The secured VFS owns the authorisation: reassigning the uid, or
    /// setting a gid the caller is not a member of, requires
    /// [`tairix_abi::CapabilityId::FS_CHOWN`]; otherwise only the node's
    /// owner may change the group, and only to a group they belong to. Any
    /// change clears the set-*id* bits.
    ///
    /// Defaults to [`Errno::NotImplemented`] so a service (the boot-time
    /// null service, a mount whose format stores no per-node ownership)
    /// that cannot honour the change fails closed; the real per-inode
    /// filesystem overrides it.
    ///
    /// # Errors
    ///
    /// The stable [`Errno`] for the VFS refusal (a missing path, an
    /// unauthorised caller, a read-only mount), or [`Errno::NotImplemented`]
    /// when no filesystem is mounted or the format keeps no ownership.
    fn set_owner(
        &self,
        _uid: u32,
        _caps: &dyn CapabilityQuery,
        _path: &str,
        _new_uid: u32,
        _new_gid: u32,
    ) -> Result<(), Errno> {
        Err(Errno::NotImplemented)
    }

    /// Read the extended attribute `key` of the node at the absolute
    /// `path` into `value_out`, returning the value's byte count (the
    /// `getxattr(2)` shape).
    ///
    /// The secured VFS owns the authorisation: the key must satisfy the
    /// shared `lib/fsmeta` grammar, the caller needs read permission on
    /// the node, and the privileged namespaces are refused.
    ///
    /// # Errors
    ///
    /// The stable [`Errno`] for the VFS refusal, [`Errno::NoData`] when
    /// the node carries no such attribute, [`Errno::BufferTooSmall`] when
    /// the value does not fit, [`Errno::NotSupported`] when the covering
    /// mount's format stores no attributes, or [`Errno::NotImplemented`]
    /// when no filesystem is mounted.
    fn attr_get(
        &self,
        uid: u32,
        caps: &dyn CapabilityQuery,
        path: &str,
        key: &[u8],
        value_out: &mut [u8],
    ) -> Result<usize, Errno>;

    /// Set (insert or replace) the extended attribute `key` of the node at
    /// the absolute `path` to `value`, in one copy-on-write transaction
    /// (the `setxattr(2)` shape). Needs write permission on the node and a
    /// writable mount.
    ///
    /// # Errors
    ///
    /// The stable [`Errno`] for the VFS refusal, [`Errno::NoSpace`] at the
    /// per-inode attribute bounds, [`Errno::NotSupported`] when the
    /// covering mount's format stores no attributes, or
    /// [`Errno::NotImplemented`] when no filesystem is mounted.
    fn attr_set(
        &self,
        uid: u32,
        caps: &dyn CapabilityQuery,
        path: &str,
        key: &[u8],
        value: &[u8],
    ) -> Result<(), Errno>;

    /// Yield the `index`-th visible extended-attribute key of the node at
    /// the absolute `path` into `key_out`, returning its byte count, or
    /// `None` once `index` is past the last visible attribute. Keys the
    /// caller may not read are omitted, never revealed.
    ///
    /// # Errors
    ///
    /// The stable [`Errno`] for the VFS refusal, [`Errno::BufferTooSmall`]
    /// when the selected key does not fit, [`Errno::NotSupported`] when
    /// the covering mount's format stores no attributes, or
    /// [`Errno::NotImplemented`] when no filesystem is mounted.
    fn attr_list(
        &self,
        uid: u32,
        caps: &dyn CapabilityQuery,
        path: &str,
        index: u64,
        key_out: &mut [u8],
    ) -> Result<Option<usize>, Errno>;

    /// Remove the extended attribute `key` from the node at the absolute
    /// `path`, in one copy-on-write transaction (the `removexattr(2)`
    /// shape). Needs write permission on the node and a writable mount.
    ///
    /// # Errors
    ///
    /// The stable [`Errno`] for the VFS refusal, [`Errno::NoData`] when
    /// the node carries no such attribute, [`Errno::NotSupported`] when
    /// the covering mount's format stores no attributes, or
    /// [`Errno::NotImplemented`] when no filesystem is mounted.
    fn attr_remove(
        &self,
        uid: u32,
        caps: &dyn CapabilityQuery,
        path: &str,
        key: &[u8],
    ) -> Result<(), Errno>;

    /// Snapshot the system mount table as wire-ready [`MountRecord`]s, in a
    /// stable order (the permanent root mount first), for the System
    /// Information introspection feed.
    ///
    /// This is a read-only, system-wide, secret-free observation — it names
    /// the mounted volumes and their permission flags, never file contents —
    /// so it is ungated at this seam (the `sysinfo_introspect` syscall that
    /// reaches it is itself capability-gated, and the `sysinfod` broker
    /// applies any per-client policy). The default returns an empty snapshot,
    /// so a service that owns no mount table (the fail-closed
    /// [`NullFilesystemService`]) truthfully reports "no mounts" rather than
    /// fabricating one.
    fn mount_snapshot(&self) -> Vec<MountRecord> {
        Vec::new()
    }
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
    ) -> Result<Vec<ReaddirEntry>, Errno> {
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

    fn unlink(
        &self,
        _uid: u32,
        _caps: &dyn CapabilityQuery,
        _path: &str,
        _flags: UnlinkFlags,
    ) -> Result<(), Errno> {
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

    fn set_mode(
        &self,
        _uid: u32,
        _caps: &dyn CapabilityQuery,
        _path: &str,
        _mode: u32,
    ) -> Result<(), Errno> {
        Err(Errno::NotImplemented)
    }

    fn attr_get(
        &self,
        _uid: u32,
        _caps: &dyn CapabilityQuery,
        _path: &str,
        _key: &[u8],
        _value_out: &mut [u8],
    ) -> Result<usize, Errno> {
        Err(Errno::NotImplemented)
    }

    fn attr_set(
        &self,
        _uid: u32,
        _caps: &dyn CapabilityQuery,
        _path: &str,
        _key: &[u8],
        _value: &[u8],
    ) -> Result<(), Errno> {
        Err(Errno::NotImplemented)
    }

    fn attr_list(
        &self,
        _uid: u32,
        _caps: &dyn CapabilityQuery,
        _path: &str,
        _index: u64,
        _key_out: &mut [u8],
    ) -> Result<Option<usize>, Errno> {
        Err(Errno::NotImplemented)
    }

    fn attr_remove(
        &self,
        _uid: u32,
        _caps: &dyn CapabilityQuery,
        _path: &str,
        _key: &[u8],
    ) -> Result<(), Errno> {
        Err(Errno::NotImplemented)
    }
}

/// The shared fail-closed filesystem service the handlers hold until the
/// boot path installs the real one.
pub static NULL_FILESYSTEM: NullFilesystemService = NullFilesystemService;
