//! Boot-time install of the userland `fs_*` filesystem mount table
//! (`PREREQUISITES.md` P-A).
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
//! # The mount layering: writable root, read-only `/System` shadow
//!
//! The system has two `RustFs` volumes on the one bootstrap-floor disk:
//!
//! * the **encrypted, writable root volume** (`RustFsRoot`) — the persistent
//!   home of every writable path: `/` itself, `/Users`, `/Apps`, `/Storage`,
//!   and the two writable `/System` exceptions `/System/Logs` and
//!   `/System/Settings`; and
//! * the **read-only, well-known-keyed `/System` volume** (`RustFsSystem`) —
//!   the immutable kernel image, drivers, and libraries.
//!
//! The writable volume is mounted as `/` (`MountTable::back_root`); the
//! read-only `/System` volume is mounted *over* it at `/System`, and the
//! writable `/System` exceptions are carved back out as rebased sub-mounts of
//! the writable volume. `MountTable` longest-prefix resolution stitches the
//! two into one tree with **no path served by both volumes** — a read of
//! `/System/Drivers/...` resolves to the read-only volume, a write under
//! `/Users`, `/Apps`, `/Storage`, `/System/Logs`, or `/System/Settings`
//! resolves to the writable volume, and everything else resolves to `/` on
//! the writable volume. This is disjoint sub-mounting, never a union/overlay
//! "merge" of two `/System` trees (which would need whiteouts, copy-up, and
//! ambiguous write targets — the opposite of the charter's fail-closed,
//! deterministic resolution).
//!
//! This module owns the mount half: it opens an independent `'static`
//! read-only window onto the `/System` volume (the driver-store serve loop
//! keeps its own window over the same
//! [`SharedBlock`](crate::shared_block::SharedBlock), and concurrent windows
//! are already park-safe through the device's `SleepLock`), publishes the
//! mount-table layout into [`LATE_FILESYSTEM`], and registers the read-only
//! `/System` driver against it. The writable root driver is registered
//! separately by [`register_writable_state`] once the encrypted root is
//! unlocked; the identity half is published by the encrypted-root unlock step
//! (`crate::root_mount`, [`LATE_IDENTITY`]).
//!
//! # Why the driver type is erased
//!
//! The bootstrap-floor disk type `B` (virtio-blk on the QEMU `virt` / x86_64
//! root, EMMC2 on the Raspberry Pi 4) is dynamic in one binary, so the
//! concrete `RustFs<PartitionBlock<SharedBlockHandle<'static, B>>>` differs
//! per board. The boot-time [`LateFilesystem`] / [`MountedFilesystemService`]
//! statics must be a *single* concrete type, so the mounted driver is erased
//! behind [`KernelFs`] (a `Box<dyn KernelFs>`); the forwarding impls below
//! let the boxed driver satisfy the read/write/security/stats-plus-`Send`
//! bound the service requires.
//!
//! Until the writable root driver is registered (after the encrypted root is
//! unlocked), every operation on `/` and its writable subtrees fails closed
//! `NotImplemented`, never a silent fallback to the read-only `/System`. The
//! read-only `/System` volume itself never accepts a write through its
//! handle.

use alloc::boxed::Box;
use alloc::vec::Vec;

use rustos_abi::driver::block::Block;
use rustos_abi::driver::filesystem::{
    FilesystemRead, FilesystemSecurity, FilesystemStats, FilesystemWrite,
};
use rustos_abi::DriverHandle;
use rustos_drv_fs_rustfs::{RustFs, SYSTEM_VOLUME_KEY};
use rustos_kernel_core::{
    CachedFs, LateFilesystem, MountedFilesystemService, Path, SleepLock, Vfs, VfsError,
};
use rustos_kernel_mem::CacheBudget;
use rustos_kernel_sec::{GroupId, UserId};
use rustos_log::{log, Event, EventId, Field, Level, Sink};
use rustos_partition::{parse_partition_table, PartitionBlock, PartitionType};

use crate::root_mount::LATE_IDENTITY;
use crate::shared_block::DriverStoreService;

pub use crate::kernel_fs::KernelFs;

/// The set-once mount cell the `fs_*` syscalls resolve operations against,
/// published by [`install_system_mount`] once the disk is up.
pub static LATE_FILESYSTEM: LateFilesystem<Box<dyn KernelFs>> = LateFilesystem::new();

/// Wrap a volume's driver in the clean, rebuildable filesystem cache
/// (`plans/SMARTRAM.md` section 6.1), budgeted from the kernel heap
/// arena the cache lives in.
///
/// Every mounted volume is registered through this one helper, so both
/// the read path and every mutation (including the account-administration
/// engine's, which shares the registered lock) flow through the same
/// cache and its invalidation.
fn cached<F>(driver: F) -> Box<dyn KernelFs>
where
    F: FilesystemRead + FilesystemWrite + FilesystemSecurity + FilesystemStats + Send + 'static,
{
    Box::new(CachedFs::new(
        driver,
        CacheBudget::from_backing(rustos_kalloc::HEAP_BYTES),
    ))
}

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

/// Opaque driver handle the **writable root volume** carries in the mount
/// table — the root mount `/` and every writable sub-mount of it (`/Users`,
/// `/Apps`, `/Storage`, `/System/Logs`, `/System/Settings`).
///
/// All of these are the *one* encrypted root volume (the persistent writable
/// partition), so they share a single handle — one driver, one per-mount
/// [`SleepLock`](rustos_kernel_core) serialising every operation on the
/// volume. The driver is registered by [`register_writable_state`] after the
/// encrypted root is unlocked; until then `/` and its writable subtrees
/// resolve to no driver and every write/read fails closed (`NotImplemented`),
/// never a silent fallback to the read-only `/System`.
const ROOT_VOLUME_HANDLE: u64 = 0x524F_4F54; // "ROOT"

/// Audit event: the read-only `/System` volume was published as the `fs_*`
/// mount, so userland file reads under `/System` now resolve to a live
/// volume.
const SYSTEM_FS_MOUNTED: EventId = EventId(4143);

/// Audit event: the writable encrypted-root driver was registered as the
/// backing for `/` and its writable subtrees (`/Users`, `/Apps`,
/// `/Storage`, `/System/Logs`, `/System/Settings`), so userland writes to
/// the persistent volume now resolve to a live driver.
const SYSTEM_FS_WRITABLE_MOUNTED: EventId = EventId(4145);

/// Audit event: no read-only `/System` volume could be published as the
/// `fs_*` mount (no `RustFsSystem` partition, an out-of-range window, an
/// unmountable volume, or a VFS/install refusal). The `fs_*` syscalls keep
/// failing closed; this is never fatal to boot. The `cause` field names the
/// check that declined, secret-free.
const SYSTEM_FS_UNAVAILABLE: EventId = EventId(4144);

/// Build the production VFS policy layer: the writable root volume mounted
/// as `/`, the read-only `/System` volume shadowing it, and the writable
/// `/System` exceptions plus the flag-bearing top-level subtrees carved back
/// out of the writable volume.
///
/// Starts from the shared default layout (the four top-level directories and
/// the §16.2 / §16.3 mount policy — one definition of the paths *and* their
/// `ro`/`nosuid`/`nodev`/`noexec` flags) and attaches a backing volume to
/// each mount, leaving the layout's flags untouched:
///
/// * `/` — the encrypted, writable root volume ([`ROOT_VOLUME_HANDLE`]): the
///   persistent home of `/Users`, `/Apps`, `/Storage`, and `/` itself.
/// * `/System` — read-only, the `RustFsSystem` volume
///   ([`SYSTEM_MOUNT_HANDLE`], whole-volume so no rebasing): the kernel
///   image, drivers, and libraries are immutable at runtime and shadow the
///   writable root at `/System` (longest-prefix resolution).
/// * `/System/Logs`, `/System/Settings`, `/Users`, `/Apps`, `/Storage` — the
///   *same* writable root volume ([`ROOT_VOLUME_HANDLE`]), each rebased onto
///   its own same-named path on that volume so the one driver resolves from
///   its own root. They exist as their own mounts only to carry stricter
///   flags than `/` and, for `/System/Logs` / `/System/Settings`, to shadow
///   the read-only `/System` (the only writable paths beneath it).
///
/// The writable root driver is registered by [`register_writable_state`]
/// only after the encrypted root is unlocked; until then `/` and its
/// writable subtrees fail closed `NotImplemented`, never a silent fallback
/// to the read-only `/System`.
///
/// # Errors
///
/// A [`VfsError`] if a fixed path fails to parse or a mount cannot be backed
/// (a default-layout mount unexpectedly absent, or a refused double-backing)
/// — all wiring defects, surfaced (fail closed) rather than panicked.
fn system_vfs() -> Result<Vfs, VfsError> {
    let mut vfs = Vfs::with_default_layout(UserId(0), GroupId(0));
    let system_handle = DriverHandle::from_raw(SYSTEM_MOUNT_HANDLE).map_err(|_| VfsError::Io)?;
    let root_handle = DriverHandle::from_raw(ROOT_VOLUME_HANDLE).map_err(|_| VfsError::Io)?;
    let mounts = vfs.mounts_mut();
    // The encrypted, writable root volume *is* `/`.
    mounts.back_root(root_handle)?;
    // The read-only `/System` volume shadows `/` at `/System`; its content is
    // the volume's own root, so it is a whole-volume mount (no rebasing).
    mounts.set_backing(&Path::parse("/System")?, system_handle, Vec::new())?;
    // The writable `/System` exceptions and the flag-bearing top-level
    // subtrees are the *same* writable root volume, each rebased onto its own
    // same-named path there so the one driver resolves from its own root.
    for sub in [
        "/System/Logs",
        "/System/Settings",
        "/Users",
        "/Apps",
        "/Storage",
    ] {
        let path = Path::parse(sub)?;
        let subtree = path.components().to_vec();
        mounts.set_backing(&path, root_handle, subtree)?;
    }
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
    let Ok(system_handle) = DriverHandle::from_raw(SYSTEM_MOUNT_HANDLE) else {
        unavailable(audit, "system_handle_invalid");
        return;
    };
    // Publish the shared VFS layout once, then register the read-only
    // `/System` driver against its handle. The writable root driver (the
    // encrypted root volume backing `/` and its writable subtrees) is
    // registered later by `register_writable_state`, once the root is
    // unlocked.
    if LATE_FILESYSTEM.install_vfs(vfs).is_err() {
        unavailable(audit, "already_installed");
        return;
    }
    let driver = cached(fs);
    if LATE_FILESYSTEM
        .register(system_handle, driver, "RustFsSystem", "rustfs")
        .is_err()
    {
        // Registered once per boot; a refusal is a logic error. The
        // returned lock is not needed here: nothing else in the kernel
        // writes the read-only `/System` volume.
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
    // The on-disk application store is now readable: resolve the readiness
    // latch so a `spawn` parked on a pending store wakes and proceeds
    // through the load gate (`plans/APPS.md` deliverable 8).
    crate::app_store::APP_STORE.note_available();
}

/// Register the live, writable encrypted-root driver as the backing for the
/// writable root volume — the root mount `/` and every writable sub-mount of
/// it (`/Users`, `/Apps`, `/Storage`, `/System/Logs`, `/System/Settings`),
/// which all share the one `ROOT_VOLUME_HANDLE`.
///
/// Called once by the encrypted-root unlock path after the root volume is
/// unlocked, with a read-write [`KernelFs`] over that volume (a second,
/// independent `'static` window, park-safe through the device `SleepLock`,
/// distinct from the unlock task's own read window). Until this lands, every
/// write/read under `/` and its writable subtrees fails closed
/// `NotImplemented` — never a silent fallback to the read-only `/System`.
///
/// Fail-soft and audited: a refusal (the writable volume was already
/// registered, a logic error since this runs once) leaves the writable tree
/// failing closed and never aborts the boot. The VFS itself is published by
/// [`install_system_mount`]; this only attaches the driver, so it is safe to
/// call after that step regardless of ordering.
///
/// Returns the registered driver's `'static` lock so the caller can share
/// the volume's **single** writer with the account-administration engine
/// (`RootAdminBacking`) — never a second independent window over the same
/// device — or `None` when registration was refused.
pub fn register_writable_state(
    driver: Box<dyn KernelFs>,
    audit: &dyn Sink,
) -> Option<&'static SleepLock<Box<dyn KernelFs>>> {
    let Ok(handle) = DriverHandle::from_raw(ROOT_VOLUME_HANDLE) else {
        unavailable(audit, "writable_handle_invalid");
        return None;
    };
    let Ok(shared) = LATE_FILESYSTEM.register(handle, cached(driver), "RustFsRoot", "rustfs")
    else {
        unavailable(audit, "writable_already_installed");
        return None;
    };
    log(
        audit,
        &Event {
            level: Level::Info,
            id: SYSTEM_FS_WRITABLE_MOUNTED,
            message: "system-mount: writable root volume backing registered",
            fields: &[],
        },
    );
    Some(shared)
}

/// Audit a declined `/System` `fs_*` mount with a stable, secret-free
/// `cause`. Fail-soft: the `fs_*` syscalls keep failing closed and the boot
/// proceeds. Also resolves the application-store readiness latch
/// *unavailable*, so a `spawn` parked on a pending store wakes and fails
/// closed rather than waiting forever.
fn unavailable(audit: &dyn Sink, cause: &'static str) {
    crate::app_store::APP_STORE.note_unavailable();
    log(
        audit,
        &Event {
            level: Level::Info,
            id: SYSTEM_FS_UNAVAILABLE,
            message: "system-mount: no /System volume published as the fs_* mount",
            fields: &[Field {
                key: "cause",
                value: rustos_log::FieldValue::Str(cause),
            }],
        },
    );
}

#[cfg(test)]
#[path = "system_mount_tests.rs"]
mod tests;
