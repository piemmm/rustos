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

use super::delegate::FinalLink;

use tairix_abi::sysinfo::{MountRecord, VolumeIoHealthRecord};
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
    /// `final_link` is the resolution posture of the descriptor the listing
    /// is served for: under [`FinalLink::Keep`] a `path` whose final
    /// component is a symbolic link is *not* a directory, so it is refused
    /// rather than silently listing the link's target.
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
        final_link: FinalLink,
    ) -> Result<Vec<ReaddirEntry>, Errno>;

    /// Report the structural metadata of the node at `path`.
    ///
    /// `final_link` selects between the POSIX `stat` and `lstat` readings:
    /// [`FinalLink::Keep`] — the posture an [`OpenFlags::NO_FOLLOW`]
    /// descriptor carries — reports a final symbolic link itself, including
    /// a dangling one that following would report as absent.
    ///
    /// # Errors
    ///
    /// The stable [`Errno`] for the VFS refusal, or [`Errno::NotImplemented`]
    /// when no filesystem is mounted.
    fn stat(
        &self,
        uid: u32,
        caps: &dyn CapabilityQuery,
        path: &str,
        final_link: FinalLink,
    ) -> Result<FileStat, Errno>;

    /// Create a symbolic link at the absolute `path` whose stored target is
    /// `target`.
    ///
    /// `target` is stored verbatim and is never resolved here — it is data,
    /// not a path the kernel walks — so the call authorises only the right
    /// to create a name in the link's own parent, and the link may
    /// legitimately dangle. Its grammar is checked before anything is
    /// written.
    ///
    /// # Errors
    ///
    /// The stable [`Errno`] for the VFS refusal (an existing name, a
    /// read-only mount, a permission denial), [`Errno::OutOfRange`] for a
    /// target that fails the link-target grammar,
    /// [`Errno::NotSupported`] when the covering mount's format has no link
    /// object type, or [`Errno::NotImplemented`] when no filesystem is
    /// mounted.
    fn symlink(
        &self,
        uid: u32,
        caps: &dyn CapabilityQuery,
        target: &str,
        path: &str,
    ) -> Result<(), Errno>;

    /// Read the stored target of the symbolic link at the absolute `path`.
    ///
    /// The final component is never followed and the target comes back
    /// exactly as it was stored, still unresolved.
    ///
    /// # Errors
    ///
    /// The stable [`Errno`] for the VFS refusal, [`Errno::OutOfRange`] when
    /// `path` names something other than a symbolic link,
    /// [`Errno::NotSupported`] when the covering mount's format stores no
    /// links, or [`Errno::NotImplemented`] when no filesystem is mounted.
    fn readlink(&self, uid: u32, caps: &dyn CapabilityQuery, path: &str) -> Result<String, Errno>;

    /// Add the absolute `link` as a second name for the node the absolute
    /// `existing` already names — a hard link.
    ///
    /// `existing_link` selects whether the existing name's final symbolic
    /// link is resolved ([`FinalLink::Follow`], `ln -L`) or the link itself
    /// gains the second name ([`FinalLink::Keep`], POSIX `link()`); the new
    /// name is never followed. Both paths must lie under one mounted volume,
    /// and the new name is authorised as a create in its own parent,
    /// conferring no authority the caller did not already hold.
    ///
    /// # Errors
    ///
    /// The stable [`Errno`] for the VFS refusal (an existing name, a
    /// read-only mount, a permission denial), [`Errno::IsADirectory`] for a
    /// directory, [`Errno::CrossVolume`] for two different volumes,
    /// [`Errno::TooManyLinks`] when the format's name count would overflow,
    /// [`Errno::NotSupported`] when the covering mount's format holds one
    /// name per node, or [`Errno::NotImplemented`] when no filesystem is
    /// mounted.
    fn link(
        &self,
        uid: u32,
        caps: &dyn CapabilityQuery,
        existing: &str,
        link: &str,
        existing_link: FinalLink,
    ) -> Result<(), Errno>;

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

    /// Snapshot every fault-aware block-backed volume's live I/O health as
    /// wire-ready [`VolumeIoHealthRecord`]s, one per volume whose backing
    /// device the kernel observes, for the `sysinfo` volume-health query
    /// (`plans/FIX-IO.md` IO5).
    ///
    /// Like [`mount_snapshot`](Self::mount_snapshot) this is a read-only,
    /// system-wide, secret-free observation (the per-device outcome tallies
    /// and current availability, never file contents); the
    /// `sysinfo_introspect` syscall that reaches it is capability-gated and
    /// the `sysinfod` broker applies the per-client `CAP_SYSINFO_KERNEL`
    /// policy. The default returns an empty snapshot, so a service owning no
    /// mount table (the fail-closed [`NullFilesystemService`]) truthfully
    /// reports "no volumes" rather than fabricating one.
    fn volume_io_health_snapshot(&self) -> Vec<VolumeIoHealthRecord> {
        Vec::new()
    }

    /// Run `remove` — the caller's hardware-tree removal closure — but only
    /// while no attached volume is served from one of `endpoints`, deciding
    /// the busy check and the removal together so an attach cannot race in
    /// between.
    ///
    /// This backs the orderly (stop-if-idle) `hw_remove_node`: `endpoints`
    /// are the block-service endpoint base ids the node being retired
    /// declares. A real disk-backed service holds its mount registry's lock
    /// across *both* the busy check and `remove`, so the check and the
    /// removal are atomic with respect to a concurrent attach (which
    /// registers under the same lock). If any attached volume is served from
    /// an endpoint in `endpoints` the service returns [`Errno::Busy`] and
    /// **never calls `remove`** (fail closed, nothing removed); otherwise it
    /// calls `remove` and returns its result — the ids of every node the
    /// removal retired.
    ///
    /// The default holds no volume registry, so it can never be busy: it
    /// simply calls `remove`. A service owning no mount table (the
    /// fail-closed [`NullFilesystemService`]) thus never spuriously refuses a
    /// removal.
    ///
    /// # Errors
    ///
    /// [`Errno::Busy`] when a volume is still attached on one of `endpoints`;
    /// otherwise whatever `remove` returns.
    fn remove_if_endpoints_idle(
        &self,
        _endpoints: &[u64],
        remove: &mut dyn FnMut() -> Result<Vec<u32>, Errno>,
    ) -> Result<Vec<u32>, Errno> {
        remove()
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
        _final_link: FinalLink,
    ) -> Result<Vec<ReaddirEntry>, Errno> {
        Err(Errno::NotImplemented)
    }

    fn stat(
        &self,
        _uid: u32,
        _caps: &dyn CapabilityQuery,
        _path: &str,
        _final_link: FinalLink,
    ) -> Result<FileStat, Errno> {
        Err(Errno::NotImplemented)
    }

    fn symlink(
        &self,
        _uid: u32,
        _caps: &dyn CapabilityQuery,
        _target: &str,
        _path: &str,
    ) -> Result<(), Errno> {
        Err(Errno::NotImplemented)
    }

    fn readlink(
        &self,
        _uid: u32,
        _caps: &dyn CapabilityQuery,
        _path: &str,
    ) -> Result<String, Errno> {
        Err(Errno::NotImplemented)
    }

    fn link(
        &self,
        _uid: u32,
        _caps: &dyn CapabilityQuery,
        _existing: &str,
        _link: &str,
        _existing_link: FinalLink,
    ) -> Result<(), Errno> {
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
