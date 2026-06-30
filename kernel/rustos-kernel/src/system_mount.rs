//! Boot-time install of the read-only `/System` volume as the userland
//! `fs_*` filesystem mount (`PREREQUISITES.md` P-A).
//!
//! The `fs_*` syscalls route through a single
//! [`MountedFilesystemService`] the dispatch hook holds from boot (installed
//! via `BootInfo::with_filesystem`).
//! That service serves operations against a late-installed [`LateFilesystem`]
//! mount and resolves caller groups through a late-installed
//! [`LateIdentity`](rustos_kernel_core::LateIdentity) table; until the boot
//! path installs both, every `fs_*` syscall fails closed
//! ([`Errno::NotImplemented`](rustos_abi::Errno::NotImplemented)).
//!
//! This module owns the mount half: it opens a second, independent
//! `'static` read-only window onto the `/System` volume on the one
//! bootstrap-floor disk (the driver-store serve loop keeps its own window
//! over the same [`SharedBlock`](crate::shared_block::SharedBlock), and
//! concurrent windows are already park-safe through the device's
//! `SleepLock`), mounts it through the real `RustFs` driver, and publishes it
//! into [`LATE_FILESYSTEM`]. The identity half is published by the
//! encrypted-root unlock step (`crate::root_mount`, [`LATE_IDENTITY`]).
//!
//! # Why the driver type is erased
//!
//! The bootstrap-floor disk type `B` (virtio-blk on the QEMU `virt` / x86_64
//! root, EMMC2 on the Raspberry Pi 4) is dynamic in one binary, so the
//! concrete `RustFs<PartitionBlock<SharedBlockHandle<'static, B>>>` differs
//! per board. The boot-time [`LateFilesystem`] / [`MountedFilesystemService`]
//! statics must be a *single* concrete type, so the mounted driver is erased
//! behind [`KernelFs`] (a `Box<dyn KernelFs>`); the forwarding impls below
//! let the boxed driver satisfy the
//! `FilesystemRead + FilesystemWrite + FilesystemSecurity + Send` bound the
//! service requires.
//!
//! Writes still fail closed on the read-only `/System` mount until
//! `PREREQUISITES.md` P-B lands the writable `/System/Logs` subtree; only
//! `open`(read) / `read` / `readdir` / `stat` exercise a live path now,
//! which is correct, not a stub.

use alloc::boxed::Box;

use rustos_abi::driver::block::Block;
use rustos_abi::driver::filesystem::{
    DirEntry, FilesystemRead, FilesystemSecurity, FilesystemWrite, MountFlags, NodeId, NodeInfo,
    NodeKind, NodeSecurity,
};
use rustos_abi::{DriverError, DriverHandle};
use rustos_drv_fs_rustfs::{RustFs, SYSTEM_VOLUME_KEY};
use rustos_kernel_core::{LateFilesystem, MountedFilesystemService, Path, Vfs, VfsError};
use rustos_kernel_sec::{GroupId, UserId};
use rustos_log::{log, Event, EventId, Field, Level, Sink};
use rustos_partition::{parse_partition_table, PartitionBlock, PartitionType};

use crate::root_mount::LATE_IDENTITY;
use crate::shared_block::DriverStoreService;

/// The mounted-volume filesystem driver, type-erased.
///
/// A blanket trait over the three structural surfaces the secured VFS
/// delegates to, plus [`Send`] (the mount lives behind a sleeping lock shared
/// across the per-CPU syscall handlers). The blanket impl makes every
/// concrete `RustFs<…>` a `KernelFs`; the `Box<dyn KernelFs>` forwarding
/// impls below let the boxed, board-specific driver be the single concrete
/// type the boot-time statics name.
pub trait KernelFs: FilesystemRead + FilesystemWrite + FilesystemSecurity + Send {}

impl<T> KernelFs for T where T: FilesystemRead + FilesystemWrite + FilesystemSecurity + Send {}

impl FilesystemRead for Box<dyn KernelFs> {
    fn root(&self) -> NodeId {
        (**self).root()
    }

    fn node_info(&mut self, node: NodeId) -> Result<NodeInfo, DriverError> {
        (**self).node_info(node)
    }

    fn lookup(&mut self, dir: NodeId, name: &[u8]) -> Result<NodeId, DriverError> {
        (**self).lookup(dir, name)
    }

    fn read_at(&mut self, file: NodeId, offset: u64, buf: &mut [u8]) -> Result<usize, DriverError> {
        (**self).read_at(file, offset, buf)
    }

    fn read_dir(
        &mut self,
        dir: NodeId,
        index: u64,
        name_out: &mut [u8],
    ) -> Result<Option<DirEntry>, DriverError> {
        (**self).read_dir(dir, index, name_out)
    }
}

impl FilesystemWrite for Box<dyn KernelFs> {
    fn create(&mut self, dir: NodeId, name: &[u8], kind: NodeKind) -> Result<NodeId, DriverError> {
        (**self).create(dir, name, kind)
    }

    fn write_at(
        &mut self,
        dir: NodeId,
        name: &[u8],
        offset: u64,
        data: &[u8],
    ) -> Result<usize, DriverError> {
        (**self).write_at(dir, name, offset, data)
    }

    fn truncate(&mut self, dir: NodeId, name: &[u8], size: u64) -> Result<(), DriverError> {
        (**self).truncate(dir, name, size)
    }

    fn remove(&mut self, dir: NodeId, name: &[u8]) -> Result<(), DriverError> {
        (**self).remove(dir, name)
    }

    fn rename(
        &mut self,
        src_dir: NodeId,
        src_name: &[u8],
        dst_dir: NodeId,
        dst_name: &[u8],
    ) -> Result<(), DriverError> {
        (**self).rename(src_dir, src_name, dst_dir, dst_name)
    }

    fn flush(&mut self) -> Result<(), DriverError> {
        (**self).flush()
    }
}

impl FilesystemSecurity for Box<dyn KernelFs> {
    fn security(&mut self, node: NodeId) -> Result<NodeSecurity, DriverError> {
        (**self).security(node)
    }
}

/// The set-once mount cell the `fs_*` syscalls resolve operations against,
/// published by [`install_system_mount`] once the disk is up.
pub static LATE_FILESYSTEM: LateFilesystem<Box<dyn KernelFs>> = LateFilesystem::new();

/// The production `fs_*` service the dispatch hook holds from boot
/// (`BootInfo::with_filesystem`): it routes each operation through the
/// secured VFS against [`LATE_FILESYSTEM`], resolving caller groups against
/// the unlock-installed [`LATE_IDENTITY`]. Both cells fail closed until
/// installed, so wiring the hook at it changes no behaviour until the boot
/// path publishes the mount and the identity table.
pub static FS_SERVICE: MountedFilesystemService<Box<dyn KernelFs>> =
    MountedFilesystemService::new(&LATE_FILESYSTEM, &LATE_IDENTITY);

/// Opaque driver handle the `/System` mount carries in the mount table.
///
/// The [`MountedFilesystemService`] supplies the live driver per operation,
/// so the handle is only the marker that makes the `/System` mount
/// driver-backed; its concrete value is never resolved against a registry.
const SYSTEM_MOUNT_HANDLE: u64 = 0x5959_5359; // "YYSY"

/// Audit event: the read-only `/System` volume was published as the `fs_*`
/// mount, so userland file reads under `/System` now resolve to a live
/// volume.
const SYSTEM_FS_MOUNTED: EventId = EventId(4143);

/// Audit event: no read-only `/System` volume could be published as the
/// `fs_*` mount (no `RustFsSystem` partition, an out-of-range window, an
/// unmountable volume, or a VFS/install refusal). The `fs_*` syscalls keep
/// failing closed; this is never fatal to boot. The `cause` field names the
/// check that declined, secret-free.
const SYSTEM_FS_UNAVAILABLE: EventId = EventId(4144);

/// Build the production VFS policy layer for the read-only `/System` mount.
///
/// Starts from the shared default layout (the four top-level directories and
/// the §16.2 / §16.3 mount policy, one definition) and replaces the in-RAM
/// read-only `/System` mount with one **driver-backed** by the live volume.
/// The default layout's writable `/System/Logs` and `/System/Settings`
/// submounts are removed here because they carry no backing driver yet
/// (`PREREQUISITES.md` P-B installs the writable subtree); leaving them would
/// shadow the driver-backed `/System` with backing-less mounts and refuse
/// reads under them. Until P-B, the whole of `/System` is one driver-backed
/// read-only mount.
///
/// # Errors
///
/// A [`VfsError`] if a fixed path fails to parse, the default-layout submount
/// is unexpectedly absent, or the driver-backed mount cannot be registered —
/// all of which are wiring defects, surfaced (fail closed) rather than
/// panicked.
fn system_vfs() -> Result<Vfs, VfsError> {
    let mut vfs = Vfs::with_default_layout(UserId(0), GroupId(0));
    let system = Path::parse("/System")?;
    let logs = Path::parse("/System/Logs")?;
    let settings = Path::parse("/System/Settings")?;
    let handle = DriverHandle::from_raw(SYSTEM_MOUNT_HANDLE).map_err(|_| VfsError::Io)?;
    let mounts = vfs.mounts_mut();
    mounts.unmount(&logs)?;
    mounts.unmount(&settings)?;
    mounts.unmount(&system)?;
    mounts.mount(system, MountFlags::READ_ONLY, Some(handle))?;
    Ok(vfs)
}

/// Open a second, independent `'static` read-only window onto the `/System`
/// volume on the bootstrap-floor disk and publish it as the userland `fs_*`
/// mount ([`LATE_FILESYSTEM`]).
///
/// Called once by the driver-store serve task (`crate::aarch64::root_unlock`'s
/// `finish_unlock`) **before** it enters its never-returning serve loop, over
/// the same `'static`-leaked [`DriverStoreService`] that backs the store. The
/// window is independent of the store's own window — concurrent windows over
/// one disk are serialised park-safely by the device `SleepLock` — and of the
/// encrypted-root unlock window.
///
/// Every step is fail-soft and fail-closed (`AGENTS.md` §5.4 / §2.9): a disk
/// with no `RustFsSystem` partition, an out-of-range window, an unmountable
/// volume, or a VFS/install refusal leaves the `fs_*` syscalls failing closed
/// (`NotImplemented`) and is audited, never panicked and never a silent
/// device fallback. No secret is consumed or logged — the `/System` volume is
/// keyed by the non-secret well-known [`SYSTEM_VOLUME_KEY`].
pub fn install_system_mount<B: Block + 'static>(
    store: &'static DriverStoreService<B>,
    audit: &dyn Sink,
) {
    // Locate the `/System` extent on a first window, then drop it so the
    // second, owned window is the one promoted into the `'static` mount.
    let extent = {
        let mut probe = store.window();
        let Ok(table) = parse_partition_table(&mut probe) else {
            unavailable(audit, "partition_table_invalid");
            return;
        };
        let Some(extent) = table.first_of_type(PartitionType::RustFsSystem) else {
            unavailable(audit, "no_system_partition");
            return;
        };
        extent
    };

    // A bounds-checked, owned `'static` window onto the `/System` extent.
    let Ok(window) = PartitionBlock::from_partition(store.window(), &extent) else {
        unavailable(audit, "system_window_out_of_range");
        return;
    };
    // Mount read-only under the non-secret well-known key; the volume carries
    // no secrets and the kernel can never mutate it through this handle.
    let Ok(fs) = RustFs::open_read_only(window, &SYSTEM_VOLUME_KEY) else {
        unavailable(audit, "system_mount_failed");
        return;
    };
    let Ok(vfs) = system_vfs() else {
        unavailable(audit, "system_vfs_build_failed");
        return;
    };
    let driver: Box<dyn KernelFs> = Box::new(fs);
    if LATE_FILESYSTEM.install(vfs, driver).is_err() {
        // The cell is set-once; a refusal means the mount was already
        // published (this runs once per boot, so that is a logic error).
        unavailable(audit, "already_installed");
        return;
    }
    log(
        audit,
        &Event {
            level: Level::Info,
            id: SYSTEM_FS_MOUNTED,
            message: "system-mount: read-only /System volume published as the fs_* mount",
            fields: &[],
        },
    );
}

/// Audit a declined `/System` `fs_*` mount with a stable, secret-free
/// `cause`. Fail-soft: the `fs_*` syscalls keep failing closed and the boot
/// proceeds.
fn unavailable(audit: &dyn Sink, cause: &'static str) {
    log(
        audit,
        &Event {
            level: Level::Info,
            id: SYSTEM_FS_UNAVAILABLE,
            message: "system-mount: no /System volume published as the fs_* mount",
            fields: &[Field {
                key: "cause",
                value: cause,
            }],
        },
    );
}

#[cfg(test)]
#[path = "system_mount_tests.rs"]
mod tests;
