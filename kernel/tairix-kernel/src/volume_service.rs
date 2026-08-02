//! The runtime volume attach/detach service (`plans/DEVICES.md` D3b).
//!
//! The production [`VolumeService`] behind the `volume_attach` /
//! `volume_detach` syscalls: it connects the kernel blkio client to the
//! block-service endpoint + shared window a user-space block driver serves
//! (the USB mass-storage per-LUN nodes), windows the probed partition
//! extent, opens the matched filesystem, mounts it under its
//! `/Storage/<name>` catalog view location with the removable-media
//! flags, and publishes the volume's stable identity into the volume
//! forest so `id::<volume-id>/…` paths resolve. Detach reverses it:
//! flush, unmount, unregister, unpublish — failing closed (the volume
//! stays attached) rather than discarding uncommitted data.
//!
//! The syscall layer has already verified `CAP_FS_MOUNT` and the caller's
//! endpoint/window resource grants; this service re-validates everything
//! against live state and audits every attach/detach decision with a
//! stable event id (the drives.md `fs.hotplug.root_{added,removed}`
//! events; root publication itself flows through the shared
//! `publish_volume_identity` definition in `crate::system_mount`).
//!
//! Attach and detach are rare, whole-volume operations; they serialise on
//! one sleeping lock (they park on device I/O), so two concurrent
//! requests can never interleave a half-built volume. The attached-volume
//! registry itself lives behind a spin lock so the surprise-removal path
//! can walk it from the endpoint-teardown context without parking.
//!
//! # Surprise removal (`plans/DEVICES.md` D4)
//!
//! Every attach threads the device through a
//! [`JournaledBlock`], so the volume carries a bounded [`RetainedWrites`]
//! journal of the writes the device has accepted since its last committed
//! flush. The service implements
//! [`tairix_kernel_core::callreg::EndpointVanishObserver`]: when a
//! volume's serving block driver dies (the stick was yanked and the HCD
//! retracted its nodes), the vanished endpoint drives the transition —
//!
//! * **clean** (nothing uncommitted): the volume is simply retracted —
//!   unmounted, unregistered, unpublished — with one audit event; no
//!   drama (drives.md §10).
//! * **dirty**: the volume enters *unavailable-dirty* — the root, alias,
//!   and mount stay visible but every new operation fails closed with a
//!   typed device fault (the dead endpoint guarantees it), the retained
//!   write set is kept for the verified re-insert replay or an explicit
//!   force-discard, and the event records how many bytes are held.
//! * **lost** (retention was abandoned under the budget/pressure gate, or
//!   a failed write left the medium state unknown): *unavailable-lost*,
//!   and the event says so — uncommitted data existed that is not held.
//!
//! A plain `volume_detach` of an unavailable volume is refused: discarding
//! the retained set is the deliberate, separately-audited **force-unmount**
//! (`plans/DEVICES.md` D4b), never an implicit side effect. A force detach
//! (`VolumeDetachRequest::force`) still commits a healthy volume cleanly
//! when it can; only when nothing can be committed — the volume is
//! unavailable, or its flush fails — does it discard the retained set,
//! logging the deliberate data loss with its own event id.
//!
//! # Verified re-insert (`plans/DEVICES.md` D4c)
//!
//! An attach whose probed identity (`lib/fsprobe`) matches an unavailable
//! volume is a **re-insert** and is recovered in place rather than
//! re-attached as a duplicate. The journal carries a dual-acceptance
//! shadow of the volume's mutation-evidence window (the region any
//! foreign mutation must rewrite: the `ARXFS` superblock ring, the ext4
//! superblock, the FAT32 boot+`FSInfo` head — honestly weaker for weaker
//! formats), seeded at attach and maintained on every write. When the
//! re-read window proves non-mutation — every evidence block equals its
//! last-committed or latest shadow copy — the retained writes are
//! replayed, the device cache committed, and the volume returns to full
//! service under its original mount, name, and published root. Any doubt
//! (foreign mutation, a moved extent, abandoned retention, a replay or
//! flush failure) fails closed: the volume returns **read-only** in the
//! *recovery-conflict* state with the retained set still held for
//! explicit salvage or the audited force-discard — never a silent merge.

use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};

use tairix_abi::driver::block::Block;
use tairix_abi::driver::filesystem::{
    DirEntry, FilesystemAttrsProvider, FilesystemRead, FilesystemSecurity, FilesystemStats,
    FilesystemWrite, MountFlags, NodeId, NodeInfo, NodeKind, NodeSecurity, VolumeStats,
};
use tairix_abi::sysinfo::MountAvailability;
use tairix_abi::volume::{VolumeAttachRequest, VolumeDetachRequest, VolumeFsType};
use tairix_abi::{DriverError, DriverHandle, Errno};
use tairix_drv_fs_arxfs::{ARXFS, SYSTEM_VOLUME_KEY};
use tairix_drv_fs_ext4::Ext4;
use tairix_drv_fs_fat32::Fat32;
use tairix_kernel_core::callreg::EndpointVanishObserver;
use tairix_kernel_core::devres::installed_shared_mem_facility;
use tairix_kernel_core::fs::blkclient::{BlkClient, VolumeHealthSource};
use tairix_kernel_core::fs::{JournaledBlock, RetainedWrites};
use tairix_kernel_core::sharedreg::kernel_hold;
use tairix_kernel_core::{Metadata, Mode, Path, SleepLock, Vfs, VolumePublishError, VolumeService};
use tairix_kernel_ipc::EndpointId;
use tairix_kernel_sec::{GroupId, UserId};
use tairix_log::{log, Event, EventId, Field, FieldValue, Level, Sink};
use tairix_partition::PartitionBlock;
use tairix_reclaim::{CacheBudget, MemoryPressure};
use tairix_sync::{OnceCell, SpinLock};

use crate::kernel_fs::KernelFs;
use crate::shared_block::{OwnedBlockWindow, SharedBlock};
use crate::system_mount::{cached, publish_volume_identity, LATE_FILESYSTEM, VOLUME_FOREST};
use crate::volume_policy::{GroupMappedFs, LATE_STORAGE_GID};

/// Audit event: a runtime volume was attached, mounted, and its root
/// published (drives.md `fs.hotplug.root_added`).
const VOLUME_ATTACHED: EventId = EventId(4172);

/// Audit event: a runtime volume attach was refused; nothing was mounted
/// (drives.md `fs.hotplug.root_added` deny half). The `cause` field names
/// the check that declined, secret-free.
const VOLUME_ATTACH_REFUSED: EventId = EventId(4173);

/// Audit event: a runtime volume was flushed, unmounted, and its root
/// withdrawn (drives.md `fs.hotplug.root_removed`).
const VOLUME_DETACHED: EventId = EventId(4174);

/// Audit event: a runtime volume detach was refused; the volume stays
/// attached and no data was discarded.
const VOLUME_DETACH_REFUSED: EventId = EventId(4175);

/// Audit event: a volume's serving driver vanished with nothing
/// uncommitted; the volume was retracted cleanly (drives.md §10 — a
/// clean surprise removal is no drama).
const VOLUME_SURPRISE_REMOVED_CLEAN: EventId = EventId(4176);

/// Audit event: a volume's serving driver vanished with uncommitted
/// writes; the volume is unavailable-dirty and the retained set is held
/// for verified re-insert or an explicit force-discard. Carries the
/// retained byte count.
const VOLUME_SURPRISE_REMOVED_DIRTY: EventId = EventId(4177);

/// Audit event: a volume's serving driver vanished after retention was
/// abandoned (budget/pressure refusal or a failed write): uncommitted
/// data existed that is **not** held. The volume is unavailable-lost.
const VOLUME_SURPRISE_REMOVED_LOST: EventId = EventId(4178);

/// Audit event: a volume was force-unmounted, deliberately discarding
/// whatever uncommitted data its journal retained (or had already lost).
/// Carries the discarded byte count and the reason a clean commit was
/// impossible (drives.md `fs.hotplug.force_unmount`).
const VOLUME_FORCE_UNMOUNTED: EventId = EventId(4179);

/// Audit event: a re-inserted volume was proven unmutated and its
/// retained uncommitted writes were replayed and committed; the volume is
/// back in service (drives.md `fs.hotplug.reinsert_replayed`). Carries the
/// replayed byte count.
const VOLUME_REINSERT_REPLAYED: EventId = EventId(4185);

/// Audit event: a re-inserted volume's non-mutation could not be proven
/// (or retention had been abandoned): it is mounted fresh and read-only
/// with the retained set kept for explicit salvage or force-discard
/// (drives.md `fs.hotplug.reinsert_conflict`). Carries the refusing cause
/// and the retained byte count still held.
const VOLUME_REINSERT_CONFLICT: EventId = EventId(4186);

/// Base of the runtime volume mount-handle space (`"VOL"` tagged). Fresh
/// handles are minted from here, disjoint from the boot volumes' fixed
/// handles by construction.
const VOLUME_HANDLE_BASE: u64 = 0x564F_4C00_0000_0000;

/// One attached runtime volume's availability.
#[derive(Copy, Clone, Eq, PartialEq)]
enum Availability {
    /// The serving driver is live; the volume operates normally.
    Available,
    /// The serving driver vanished with uncommitted writes retained in
    /// the journal: the root stays visible, new I/O fails closed, and
    /// the retained set awaits verified re-insert or force-discard.
    UnavailableDirty,
    /// The serving driver vanished after retention was abandoned:
    /// uncommitted data existed that the journal does not hold.
    UnavailableLost,
    /// The volume was re-inserted but non-mutation could not be proven:
    /// it is mounted fresh and read-only over a live driver while the
    /// retained set stays held for the audited force-discard (D4c).
    RecoveryConflict,
}

/// One attached runtime volume: the facts detach and surprise removal
/// need.
struct AttachedVolume {
    /// The volume's published 16-byte identity.
    id: [u8; 16],
    /// The catalog name the root is projected under.
    name: String,
    /// The mount-table entry's path (`/Storage/<name>`).
    path: Path,
    /// The mount's driver handle in the registry.
    handle: DriverHandle,
    /// The block-service endpoint, for the detach-time device flush and
    /// the surprise-removal match.
    endpoint: u64,
    /// The shared data window's region id, for the detach-time device
    /// flush.
    window: u64,
    /// The volume's uncommitted-write journal, shared with the
    /// [`JournaledBlock`] under the mounted filesystem.
    journal: Arc<SpinLock<RetainedWrites>>,
    /// The registry's filesystem-type label, for the unavailable-stub
    /// re-registration on a dirty surprise removal.
    fstype: &'static str,
    /// The attached extent's first device LBA — the frame of reference
    /// the journal's retained LBAs and evidence window live in. A
    /// re-insert may replay only onto the identical extent.
    first_lba: u64,
    /// The attached extent's block count.
    blocks: u64,
    /// Whether the serving driver is still live.
    availability: Availability,
}

/// The fail-closed stand-in registered under an unavailable volume's
/// handle: every operation reports a device fault, so no cached bytes,
/// listings, or metadata of the vanished medium are ever served as live
/// (drives.md §10 — removal fails existing handles per their object
/// semantics). Replacing the real driver also drops its plaintext cache
/// with it.
struct UnavailableFs;

impl FilesystemRead for UnavailableFs {
    fn root(&self) -> NodeId {
        // A real-looking root, so an operation is routed *into* the stub
        // and faults honestly (`NodeId::NONE` would read as "volume not
        // online yet" instead of "device gone").
        NodeId::from_raw(1)
    }

    fn node_info(&mut self, _node: NodeId) -> Result<NodeInfo, DriverError> {
        Err(DriverError::DeviceFault)
    }

    fn lookup(&mut self, _dir: NodeId, _name: &[u8]) -> Result<NodeId, DriverError> {
        Err(DriverError::DeviceFault)
    }

    fn read_at(
        &mut self,
        _file: NodeId,
        _offset: u64,
        _buf: &mut [u8],
    ) -> Result<usize, DriverError> {
        Err(DriverError::DeviceFault)
    }

    fn read_dir(
        &mut self,
        _dir: NodeId,
        _cursor: u64,
        _name_out: &mut [u8],
    ) -> Result<Option<DirEntry>, DriverError> {
        Err(DriverError::DeviceFault)
    }
}

impl FilesystemWrite for UnavailableFs {
    fn create(
        &mut self,
        _dir: NodeId,
        _name: &[u8],
        _kind: NodeKind,
    ) -> Result<NodeId, DriverError> {
        Err(DriverError::DeviceFault)
    }

    fn write_at(
        &mut self,
        _dir: NodeId,
        _name: &[u8],
        _offset: u64,
        _data: &[u8],
    ) -> Result<usize, DriverError> {
        Err(DriverError::DeviceFault)
    }

    fn truncate(&mut self, _dir: NodeId, _name: &[u8], _size: u64) -> Result<(), DriverError> {
        Err(DriverError::DeviceFault)
    }

    fn remove(&mut self, _dir: NodeId, _name: &[u8]) -> Result<(), DriverError> {
        Err(DriverError::DeviceFault)
    }

    fn rename(
        &mut self,
        _src_dir: NodeId,
        _src_name: &[u8],
        _dst_dir: NodeId,
        _dst_name: &[u8],
    ) -> Result<(), DriverError> {
        Err(DriverError::DeviceFault)
    }

    fn flush(&mut self) -> Result<(), DriverError> {
        Err(DriverError::DeviceFault)
    }
}

impl FilesystemSecurity for UnavailableFs {
    fn security(&mut self, _node: NodeId) -> Result<NodeSecurity, DriverError> {
        Err(DriverError::DeviceFault)
    }

    fn set_security(&mut self, _node: NodeId, _security: NodeSecurity) -> Result<(), DriverError> {
        Err(DriverError::DeviceFault)
    }
}

impl FilesystemStats for UnavailableFs {
    fn stats(&mut self) -> Result<VolumeStats, DriverError> {
        Err(DriverError::DeviceFault)
    }
}

/// A detached volume serves no attribute store; the default facet answer
/// (`None`) refuses the `fs_attr_*` surface with the typed
/// unsupported-backing error, consistent with every other operation's
/// fail-closed fault.
impl FilesystemAttrsProvider for UnavailableFs {}

/// The boot-installed wiring the service operates with.
struct Wiring {
    audit: &'static (dyn Sink + Sync),
    pressure: &'static MemoryPressure,
}

/// The production runtime volume attach/detach service. One static
/// instance ([`VOLUME_SERVICE`]) is handed to the boot handover; until
/// [`install`](Self::install) wires it, every operation fails closed.
pub struct RuntimeVolumeService {
    /// Set-once boot wiring; fail-closed `NotImplemented` before install.
    wiring: OnceCell<Wiring>,
    /// The whole-operation serialisation lock: attach/detach park on
    /// device I/O, so the lock sleeps, never spins.
    op: SleepLock<()>,
    /// The attached-volume registry. A spin lock, so the surprise-removal
    /// path can walk it from the endpoint-teardown context without
    /// parking; guards are held only for lookups and brief mutations,
    /// never across device I/O.
    state: SpinLock<Vec<AttachedVolume>>,
    /// The served block devices the attached volumes live on, one entry
    /// per block-service endpoint ([`SharedDevice`]). Mutated only under
    /// the operation lock; the guard is never held across device I/O.
    devices: SpinLock<Vec<SharedDevice>>,
    /// The next mount handle to mint.
    next_handle: AtomicU64,
}

/// The one production service instance the boot handover installs.
pub static VOLUME_SERVICE: RuntimeVolumeService = RuntimeVolumeService::new();

impl RuntimeVolumeService {
    /// An unwired service; every operation fails closed until
    /// [`install`](Self::install).
    #[must_use]
    pub const fn new() -> Self {
        Self {
            wiring: OnceCell::new(),
            op: SleepLock::new(()),
            state: SpinLock::new(Vec::new()),
            devices: SpinLock::new(Vec::new()),
            next_handle: AtomicU64::new(VOLUME_HANDLE_BASE + 1),
        }
    }

    /// Wire the service with the boot path's audit sink and pressure
    /// gauge. First-wins and idempotent, like the other late-installed
    /// seams.
    pub fn install(&self, audit: &'static (dyn Sink + Sync), pressure: &'static MemoryPressure) {
        let _ = self.wiring.set(Wiring { audit, pressure });
    }

    /// The installed wiring, or fail-closed before boot installs it.
    fn wiring(&self) -> Result<&Wiring, Errno> {
        match self.wiring.get() {
            Ok(Some(wiring)) => Ok(wiring),
            _ => Err(Errno::NotImplemented),
        }
    }

    /// The shared device serving `request`'s endpoint, connecting it on
    /// first use.
    ///
    /// Every volume on one disk shares the single connected client, because
    /// they share the one staging window the transfers move bytes through
    /// (see [`SharedDevice`]). Entries are keyed by the endpoint **and** the
    /// window: sharing is only safe for consumers staging bytes in the same
    /// buffer, and a device presenting a different window is a different
    /// staging buffer, so it gets its own client rather than being folded
    /// onto a stale one. Entries no attached volume names any more are
    /// dropped first, so a torn-down endpoint's client can never be handed
    /// to a device that later re-uses its id.
    ///
    /// Runs under the caller's operation lock; the registry guard is taken
    /// only for the lookup and the insertion, never across the connect.
    ///
    /// # Errors
    ///
    /// `(cause, errno)` pairs the attach path audits through its refusal
    /// helper.
    fn shared_device(
        &self,
        request: &VolumeAttachRequest<'_>,
        wiring: &Wiring,
    ) -> Result<SharedDevice, (&'static str, Errno)> {
        self.retire_unused_devices();
        let existing = {
            let devices = self.devices.lock();
            devices
                .iter()
                .find(|d| d.endpoint == request.endpoint && d.window == request.window)
                .cloned()
        };
        if let Some(device) = existing {
            return Ok(device);
        }
        let device = connect_device(request, wiring)?;
        self.devices.lock().push(device.clone());
        Ok(device)
    }

    /// The registered shared device serving `endpoint` over `window`, if it
    /// is still connected.
    fn device_for(&self, endpoint: u64, window: u64) -> Option<SharedDevice> {
        let devices = self.devices.lock();
        devices
            .iter()
            .find(|d| d.endpoint == endpoint && d.window == window)
            .cloned()
    }

    /// Drop every shared device no attached volume names any more, closing
    /// its client and releasing its window hold.
    ///
    /// Called at the head of an attach or detach, both of which hold the
    /// operation lock: the surprise-removal path runs in endpoint-teardown
    /// context and only marks volumes, so it never drops a client there.
    fn retire_unused_devices(&self) {
        let live: Vec<(u64, u64)> = {
            let state = self.state.lock();
            state.iter().map(|v| (v.endpoint, v.window)).collect()
        };
        // Each retired entry is dropped *outside* the registry guard:
        // releasing the last reference closes the client and its window
        // hold, which must not run under a spin lock.
        loop {
            let retired = {
                let mut devices = self.devices.lock();
                devices
                    .iter()
                    .position(|d| !live.contains(&(d.endpoint, d.window)))
                    .map(|index| devices.swap_remove(index))
            };
            if retired.is_none() {
                break;
            }
        }
    }

    /// Audit one attach/detach decision with the volume's catalog name (or
    /// its identity's short hex for a detach that never resolved a name).
    fn audit_event(
        audit: &'static (dyn Sink + Sync),
        id: EventId,
        message: &'static str,
        name: &str,
        cause: Option<&'static str>,
    ) {
        let name_field = Field {
            key: "volume",
            value: FieldValue::Str(name),
        };
        match cause {
            Some(cause) => {
                log(
                    audit,
                    &Event {
                        level: Level::Warn,
                        id,
                        message,
                        fields: &[
                            name_field,
                            Field {
                                key: "cause",
                                value: FieldValue::Str(cause),
                            },
                        ],
                    },
                );
            }
            None => {
                log(
                    audit,
                    &Event {
                        level: Level::Info,
                        id,
                        message,
                        fields: &[name_field],
                    },
                );
            }
        }
    }
}

impl Default for RuntimeVolumeService {
    fn default() -> Self {
        Self::new()
    }
}

/// The filesystem opened over the partition window, together with its
/// identity and registration facts.
struct OpenedVolume {
    driver: alloc::boxed::Box<dyn KernelFs>,
    identity: [u8; 16],
    fstype: &'static str,
}

/// Open the requested filesystem over the extent window, honouring the
/// device's write policy.
///
/// `map_gid` is the storage-group identity map an **ownerless** format is
/// mounted under (`plans/DEVICES.md` D3d): a FAT32 volume is wrapped so
/// every node appears system-owned under that group with group
/// read/write. `None` (the gid cell not yet installed, or a format with
/// a real owner model) leaves the driver unwrapped.
fn open_filesystem(
    window: PartitionBlock<JournaledBlock<OwnedBlockWindow<BlkClient>>>,
    fstype: VolumeFsType,
    read_only: bool,
    map_gid: Option<GroupId>,
    wiring: &Wiring,
    volume_handle: u64,
) -> Result<OpenedVolume, Errno> {
    match fstype {
        VolumeFsType::ARXFS => {
            // A removable ARXFS volume is keyed like any ARXFS volume.
            // The well-known key covers non-secret volumes (the same key
            // the read-only system volume uses); a volume under a private
            // key refuses the open with a typed error, and key-provisioned
            // attach arrives with the volume manager's key policy — the
            // kernel never guesses a secret.
            let fs = if read_only {
                ARXFS::open_read_only(window, &SYSTEM_VOLUME_KEY)
            } else {
                ARXFS::open(window, &SYSTEM_VOLUME_KEY)
            }
            .map_err(DriverError::as_errno)?;
            let identity = fs.volume_uuid();
            Ok(OpenedVolume {
                driver: cached(fs, volume_handle, wiring.pressure, wiring.audit),
                identity,
                fstype: "arxfs",
            })
        }
        VolumeFsType::Ext4 => {
            let fs = Ext4::open(window).map_err(DriverError::as_errno)?;
            let identity = fs.volume_uuid();
            Ok(OpenedVolume {
                driver: cached(fs, volume_handle, wiring.pressure, wiring.audit),
                identity,
                fstype: "ext4",
            })
        }
        VolumeFsType::Fat32 => {
            let fs = Fat32::open(window).map_err(DriverError::as_errno)?;
            let identity = fs.volume_identity();
            // FAT32 stores no owner model; mount it under the storage-group
            // identity map when the group is provisioned, else keep the
            // driver's own restrictive system-owned posture (fail closed).
            let driver = match map_gid {
                Some(gid) => cached(
                    GroupMappedFs::new(fs, gid),
                    volume_handle,
                    wiring.pressure,
                    wiring.audit,
                ),
                None => cached(fs, volume_handle, wiring.pressure, wiring.audit),
            };
            Ok(OpenedVolume {
                driver,
                identity,
                fstype: "fat32",
            })
        }
    }
}

/// Mount the volume served by `handle` at `/Storage/<name>` with the
/// removable-media flags (the device's write policy carried as `ro`),
/// returning the mount `Path` for the caller's unwind. Fails closed with
/// the `(cause, errno)` pair the attach path audits.
fn mount_storage_volume(
    vfs: &Vfs,
    name: &str,
    read_only: bool,
    handle: DriverHandle,
    map_gid: Option<GroupId>,
) -> Result<Path, (&'static str, Errno)> {
    let path = Path::parse(&format!("/Storage/{name}"))
        .map_err(|_| ("mount_path_invalid", Errno::OutOfRange))?;
    vfs.mounts_write()
        .mount_with_template(
            path.clone(),
            mount_flags(read_only),
            handle,
            mount_template(map_gid),
        )
        .map_err(|_| ("name_in_use", Errno::AlreadyExists))?;
    Ok(path)
}

/// Register `driver` under `handle` and attach the live block-health
/// overlay the mount snapshot reads (`plans/FIX-IO.md` IO2/IO3), the one
/// place both the attach and recover paths register a served volume so the
/// health wiring cannot diverge between them. Returns `Err(())` when the
/// handle is already registered (fail closed — the caller unwinds); the
/// `set_health_source` cannot miss, the handle having just been registered
/// under the operation lock.
fn register_with_health(
    handle: DriverHandle,
    driver: alloc::boxed::Box<dyn KernelFs>,
    source: &str,
    fstype: &'static str,
    volume_id: [u8; 16],
    health: VolumeHealthSource,
) -> Result<(), ()> {
    LATE_FILESYSTEM
        .register(handle, driver, source, fstype, volume_id)
        .map_err(|_| ())?;
    let _ = LATE_FILESYSTEM.set_health_source(handle, health);
    Ok(())
}

/// One served block device, connected once and shared by every runtime
/// volume that lives on it.
///
/// A disk carries several volumes (its partitions), and each of them drives
/// the **same** served device over the **same** single shared data window:
/// the block-service protocol stages each transfer's bytes in that window, so
/// two clients issuing requests over it concurrently would overwrite each
/// other's staged bytes and hand a reader another volume's data. The device
/// is therefore connected once per block-service endpoint and reached through
/// [`SharedBlock`], whose sleeping lock serialises whole device operations —
/// so the window holds exactly one transfer at a time by construction.
///
/// Sharing the client also gives a disk one health fold rather than a
/// divergent copy per volume: the reported-health overlay and the I/O
/// counters are properties of the device, not of a mount.
#[derive(Clone)]
struct SharedDevice {
    /// The block-service endpoint the device is served over; the registry
    /// key.
    endpoint: u64,
    /// The shared data window's region id, as the attach declared it.
    window: u64,
    /// The one client, shared and serialised.
    device: Arc<SharedBlock<BlkClient>>,
    /// The device's write policy, read once at connect.
    read_only: bool,
    /// The device's reported-health overlay, folded once for every volume
    /// on the device.
    health: VolumeHealthSource,
}

impl SharedDevice {
    /// A fresh window onto the shared device for one mount to own.
    fn window(&self) -> OwnedBlockWindow<BlkClient> {
        self.device.owned_handle()
    }

    /// The device's block size, from the geometry cached at connect.
    fn block_size(&self) -> u32 {
        self.device.geometry().block_size
    }
}

/// Connect the kernel blkio client for `request` and validate the
/// requested extent against the live geometry.
///
/// # Errors
///
/// `(cause, errno)` pairs the attach path audits through its refusal
/// helper.
fn connect_device(
    request: &VolumeAttachRequest<'_>,
    wiring: &Wiring,
) -> Result<SharedDevice, (&'static str, Errno)> {
    // Reach the shared data window through the kernel's counted hold.
    let hold = kernel_hold(installed_shared_mem_facility(), request.window)
        .map_err(|err| ("window_unreachable", err))?;
    // Connect the kernel blkio client and validate the device geometry.
    let client = BlkClient::connect(request.endpoint, hold, wiring.audit)
        .map_err(|err| ("endpoint_unusable", err))?;
    let read_only = client.read_only();
    let health = client.health_source();
    // Wrapping caches the geometry, so the shared device answers it
    // lock-free; a device whose geometry cannot be read is never wrapped.
    let device = SharedBlock::new(client).map_err(|err| ("geometry_unreadable", err.as_errno()))?;
    Ok(SharedDevice {
        endpoint: request.endpoint,
        window: request.window,
        device: Arc::new(device),
        read_only,
        health,
    })
}

/// Bound the requested extent by the device's live geometry.
///
/// # Errors
///
/// The `(cause, errno)` pair the attach path audits when the extent runs
/// past the end of the device (fail closed — never a clamped window).
fn validate_extent(
    request: &VolumeAttachRequest<'_>,
    device: &SharedDevice,
) -> Result<(), (&'static str, Errno)> {
    let block_count = device.device.geometry().block_count;
    if request
        .first_lba
        .checked_add(request.blocks)
        .is_none_or(|end| end > block_count)
    {
        return Err(("extent_out_of_range", Errno::LengthOutOfRange));
    }
    Ok(())
}

/// Read the first `len` bytes of the requested extent, rounded up to
/// whole blocks (the trailing partial block is read too — the buffer
/// length is the rounded figure). `None` when the geometry is degenerate,
/// the extent is too short, the allocation is refused, or the device
/// refuses the read — the caller fails closed, never guesses.
fn read_extent_head<B: Block>(
    client: &mut B,
    request: &VolumeAttachRequest<'_>,
    block_size: u32,
    len: usize,
) -> Option<Vec<u8>> {
    let bs = usize::try_from(block_size).ok().filter(|&bs| bs > 0)?;
    let blocks = len.div_ceil(bs).max(1);
    if u64::try_from(blocks).ok()? > request.blocks {
        return None;
    }
    let mut buf = Vec::new();
    buf.try_reserve_exact(blocks.checked_mul(bs)?).ok()?;
    buf.resize(blocks * bs, 0);
    client.read_blocks(request.first_lba, &mut buf).ok()?;
    Some(buf)
}

/// The mount-policy flags every runtime volume carries: the removable-
/// media restrictions, plus `ro` when the device's write policy or the
/// recovery posture demands it.
fn mount_flags(read_only: bool) -> MountFlags {
    let flags = MountFlags::NOSUID
        .union(MountFlags::NODEV)
        .union(MountFlags::NOEXEC);
    if read_only {
        flags.union(MountFlags::READ_ONLY)
    } else {
        flags
    }
}

/// The mount point's permission template (a runtime mount point has no
/// node in the boot layout tree, so the template travels with the
/// mount). An identity-mapped volume's template matches the map —
/// system-owned under the storage group, group-writable — so the mount
/// point itself is as reachable as its content; every other volume gets
/// the system-owned world-traversable default, writes gated per inode by
/// the volume's own records.
fn mount_template(map_gid: Option<GroupId>) -> Metadata {
    match map_gid {
        Some(gid) => Metadata::new(UserId(0), gid, Mode::from_bits(0o775)),
        None => Metadata::new(UserId(0), GroupId(0), Mode::from_bits(0o755)),
    }
}

/// The storage-group identity map an **ownerless** format is mounted
/// under (`plans/DEVICES.md` D3d); formats with a real owner model keep
/// their on-disk records.
fn identity_map_gid(fstype: VolumeFsType) -> Option<GroupId> {
    match fstype {
        VolumeFsType::Fat32 => LATE_STORAGE_GID.get(),
        VolumeFsType::ARXFS | VolumeFsType::Ext4 => None,
    }
}

/// A fresh uncommitted-write journal for the extent, its
/// mutation-evidence shadow seeded from the probed extent `head` (D4c).
/// A volume with no declared window (or an unreadable one) simply holds
/// no evidence; a later re-insert then fails closed to the conflict path.
fn seeded_journal<B: Block>(
    client: &mut B,
    request: &VolumeAttachRequest<'_>,
    block_size: u32,
    head: Option<&[u8]>,
) -> Arc<SpinLock<RetainedWrites>> {
    let journal = Arc::new(SpinLock::new(RetainedWrites::new(
        block_size,
        // Budget from discovered physical RAM (the growable kernel heap's
        // bootstrap size is no longer the memory to size a cache against);
        // falls back to the bootstrap size before RAM is published.
        CacheBudget::from_backing(tairix_kernel_core::memstats::cache_backing_bytes()),
    )));
    if let Some(evidence_len) = head
        .and_then(tairix_fsprobe::evidence_len)
        .and_then(|len| usize::try_from(len).ok())
    {
        if let Some(evidence) = read_extent_head(client, request, block_size, evidence_len) {
            journal.lock().set_evidence(request.first_lba, &evidence);
        }
    }
    journal
}

/// Audit how a re-insert resolved: the replayed recovery (with its byte
/// count) or the read-only conflict (with its cause and the retained
/// byte count still held).
fn audit_recovery_outcome(
    audit: &'static (dyn Sink + Sync),
    name: &str,
    outcome: &RecoveryOutcome,
    journal: &Arc<SpinLock<RetainedWrites>>,
) {
    match outcome {
        RecoveryOutcome::Replayed(bytes) => {
            log(
                audit,
                &Event {
                    level: Level::Info,
                    id: VOLUME_REINSERT_REPLAYED,
                    message: "volume-service: re-inserted volume proven unmutated; \
                              retained writes replayed and committed",
                    fields: &[
                        Field {
                            key: "volume",
                            value: FieldValue::Str(name),
                        },
                        Field {
                            key: "replayed_bytes",
                            value: FieldValue::UnsignedInt(*bytes),
                        },
                    ],
                },
            );
        }
        RecoveryOutcome::Conflict(cause) => {
            let retained = journal.lock().retained_bytes();
            log(
                audit,
                &Event {
                    level: Level::Warn,
                    id: VOLUME_REINSERT_CONFLICT,
                    message: "volume-service: re-inserted volume could not be proven \
                              unmutated; mounted read-only with the retained set kept",
                    fields: &[
                        Field {
                            key: "volume",
                            value: FieldValue::Str(name),
                        },
                        Field {
                            key: "cause",
                            value: FieldValue::Str(cause),
                        },
                        Field {
                            key: "retained_bytes",
                            value: FieldValue::UnsignedInt(retained),
                        },
                    ],
                },
            );
        }
    }
}

impl VolumeService for RuntimeVolumeService {
    fn attach(&self, request: &VolumeAttachRequest<'_>) -> Result<(), Errno> {
        let wiring = self.wiring()?;
        let audit = wiring.audit;
        // The frame validated the name as short ASCII; re-derive the &str.
        let name = core::str::from_utf8(request.name).map_err(|_| Errno::OutOfRange)?;
        // Serialise whole attach/detach operations (see the struct docs).
        let _op = self.op.lock();

        let refused = |cause: &'static str, errno: Errno| -> Errno {
            RuntimeVolumeService::audit_event(
                audit,
                VOLUME_ATTACH_REFUSED,
                "volume-service: runtime volume attach refused",
                name,
                Some(cause),
            );
            errno
        };

        // The live mount table must exist (the boot volumes are up).
        let vfs = LATE_FILESYSTEM
            .vfs()
            .map_err(|err| refused("no_mount_table", err))?;
        // Every volume on this disk drives the one shared client over the
        // one staging window, serialised (see `SharedDevice`).
        let device = self
            .shared_device(request, wiring)
            .map_err(|(cause, errno)| refused(cause, errno))?;
        validate_extent(request, &device).map_err(|(cause, errno)| refused(cause, errno))?;
        let read_only = device.read_only;
        let block_size = device.block_size();
        let mut disk = device.window();

        // A re-insert of a surprise-removed volume is recovered in place
        // — proven unmutated and replayed, or held read-only with its
        // retained set — never re-attached as a duplicate (D4c). The
        // extent head names the volume's durable identity; a head that
        // matches nothing simply attaches fresh.
        let head = read_extent_head(
            &mut disk,
            request,
            block_size,
            tairix_fsprobe::PROBE_HEAD_LEN,
        );
        if let Some(target) = self.reinsert_target(head.as_deref()) {
            return self.recover(request, wiring, &device, &target);
        }

        // Thread the device through the uncommitted-write journal, its
        // mutation-evidence shadow seeded from the extent head.
        let journal = seeded_journal(&mut disk, request, block_size, head.as_deref());
        // The device's reported-health overlay, shared by every volume on
        // it, for the mount snapshot.
        let health = device.health.clone();
        let journaled = JournaledBlock::new(disk, Arc::clone(&journal), wiring.pressure);
        let window = PartitionBlock::new(journaled, request.first_lba, request.blocks)
            .map_err(|err| refused("window_invalid", err.as_errno()))?;

        // Open the matched filesystem and take its stable identity.
        let map_gid = identity_map_gid(request.fstype);
        let handle_raw = self.next_handle.fetch_add(1, Ordering::Relaxed);
        let opened = open_filesystem(
            window,
            request.fstype,
            read_only,
            map_gid,
            wiring,
            handle_raw,
        )
        .map_err(|err| refused("filesystem_unmountable", err))?;
        let handle = DriverHandle::from_raw(handle_raw)
            .map_err(|_| refused("handle_invalid", Errno::OutOfRange))?;

        // Mount under the catalog view location with the removable-media
        // flags; the device's write policy is carried as `ro`.
        let path = mount_storage_volume(vfs, name, read_only, handle, map_gid)
            .map_err(|(cause, errno)| refused(cause, errno))?;

        // Register the live driver, then publish the identity last; each
        // failure unwinds everything already done, so a refused attach
        // leaves no trace.
        if register_with_health(
            handle,
            opened.driver,
            name,
            opened.fstype,
            opened.identity,
            health,
        )
        .is_err()
        {
            let _ = vfs.mounts_write().unmount(&path);
            return Err(refused("handle_in_use", Errno::AlreadyExists));
        }
        if let Err(err) = publish_volume_identity(opened.identity, &["Storage", name], name, audit)
        {
            let _ = LATE_FILESYSTEM.unregister(handle);
            let _ = vfs.mounts_write().unmount(&path);
            let errno = match err {
                VolumePublishError::NilIdentity => Errno::OutOfRange,
                VolumePublishError::AlreadyPublished => Errno::AlreadyExists,
            };
            return Err(refused("identity_unpublishable", errno));
        }

        self.state.lock().push(AttachedVolume {
            id: opened.identity,
            name: String::from(name),
            path,
            handle,
            endpoint: request.endpoint,
            window: request.window,
            journal,
            fstype: opened.fstype,
            first_lba: request.first_lba,
            blocks: request.blocks,
            availability: Availability::Available,
        });
        RuntimeVolumeService::audit_event(
            audit,
            VOLUME_ATTACHED,
            "volume-service: runtime volume attached and published",
            name,
            None,
        );
        // Close the attach/unplug race: if the endpoint was torn down
        // between the connect and the registration above, no vanish
        // notification can be outstanding for this entry — run the same
        // transition the observer would have.
        if !tairix_kernel_core::callreg::contains(EndpointId(request.endpoint)) {
            self.handle_vanished(request.endpoint);
        }
        Ok(())
    }

    fn detach(&self, request: &VolumeDetachRequest) -> Result<(), Errno> {
        let wiring = self.wiring()?;
        let audit = wiring.audit;
        // Serialise whole attach/detach operations (see the struct docs).
        let _op = self.op.lock();
        self.retire_unused_devices();

        // Snapshot the entry's facts; the registry guard is never held
        // across the device I/O below.
        let snapshot = {
            let state = self.state.lock();
            state.iter().find(|v| v.id == request.volume_id).map(|v| {
                (
                    v.name.clone(),
                    v.path.clone(),
                    v.handle,
                    v.endpoint,
                    v.window,
                    Arc::clone(&v.journal),
                    v.availability,
                )
            })
        };
        let Some((name, path, handle, endpoint, window, journal, availability)) = snapshot else {
            RuntimeVolumeService::audit_event(
                audit,
                VOLUME_DETACH_REFUSED,
                "volume-service: runtime volume detach refused",
                "unknown",
                Some("identity_not_attached"),
            );
            return Err(Errno::NotFound);
        };
        let refused = |cause: &'static str, errno: Errno| -> Errno {
            RuntimeVolumeService::audit_event(
                audit,
                VOLUME_DETACH_REFUSED,
                "volume-service: runtime volume detach refused",
                &name,
                Some(cause),
            );
            errno
        };

        // A surprise-removed volume never detaches implicitly: discarding
        // its retained (or already lost) uncommitted data is the
        // deliberate, separately-audited force-unmount operation. A force
        // detach still commits a healthy volume cleanly when it can; only
        // when nothing can be committed does it discard.
        let commit_refusal = match availability {
            Availability::Available => {
                commit_for_detach(handle, self.device_for(endpoint, window).as_ref(), &journal)
                    .err()
            }
            Availability::UnavailableDirty => {
                Some(("volume_unavailable_dirty", Errno::DeviceFault))
            }
            Availability::UnavailableLost => Some(("volume_unavailable_lost", Errno::DeviceFault)),
            // A conflicted volume's device is live, but its journal still
            // holds the retained set: a plain detach would discard it
            // silently, so only the audited force is the exit.
            Availability::RecoveryConflict => Some(("volume_recovery_conflict", Errno::NotEmpty)),
        };
        let discarded = match commit_refusal {
            None => None,
            Some((cause, errno)) => {
                if !request.force {
                    return Err(refused(cause, errno));
                }
                // The audited force-discard: read what is being given up
                // before wiping it, so the loss is never silent. A lost
                // journal reports zero retained bytes — that loss was
                // already on record at the surprise-removal transition.
                let mut journal = journal.lock();
                let bytes = journal.retained_bytes();
                journal.discard_all();
                Some((cause, bytes))
            }
        };

        // Retract: unmount (new resolutions fail closed), unregister (the
        // registry's driver reference drops; in-flight operations finish
        // on their own clones), and withdraw the published identity. A
        // concurrent surprise removal may have retracted the clean volume
        // while the flush parked; the goal state is then already reached.
        let mut state = self.state.lock();
        let Some(index) = state.iter().position(|v| v.id == request.volume_id) else {
            return Ok(());
        };
        state.remove(index);
        drop(state);
        let vfs = LATE_FILESYSTEM
            .vfs()
            .map_err(|err| refused("no_mount_table", err))?;
        let _ = vfs.mounts_write().unmount(&path);
        let _ = LATE_FILESYSTEM.unregister(handle);
        let _ = VOLUME_FOREST.unpublish(&request.volume_id);
        // The disk's last volume just went: close the shared client and
        // release its window hold rather than holding them until the next
        // storage operation.
        self.retire_unused_devices();
        audit_detach_outcome(audit, &name, discarded);
        Ok(())
    }
}

/// Audit a completed detach: the clean withdrawal, or the force-unmount
/// that deliberately discarded `bytes` of retained uncommitted data
/// because `cause` made a clean commit impossible.
fn audit_detach_outcome(
    audit: &'static (dyn Sink + Sync),
    name: &str,
    discarded: Option<(&'static str, u64)>,
) {
    match discarded {
        None => RuntimeVolumeService::audit_event(
            audit,
            VOLUME_DETACHED,
            "volume-service: runtime volume flushed, unmounted, and withdrawn",
            name,
            None,
        ),
        Some((cause, bytes)) => {
            log(
                audit,
                &Event {
                    level: Level::Warn,
                    id: VOLUME_FORCE_UNMOUNTED,
                    message: "volume-service: volume force-unmounted; retained uncommitted \
                              writes deliberately discarded",
                    fields: &[
                        Field {
                            key: "volume",
                            value: FieldValue::Str(name),
                        },
                        Field {
                            key: "cause",
                            value: FieldValue::Str(cause),
                        },
                        Field {
                            key: "discarded_bytes",
                            value: FieldValue::UnsignedInt(bytes),
                        },
                    ],
                },
            );
        }
    }
}

/// Commit everything a departing volume holds: flush the filesystem's own
/// state while it is still registered, then the device's cache, and empty
/// the journal — after a committed flush nothing is uncommitted. A
/// vanished endpoint or window (`NotFound` — the device was unplugged
/// mid-detach) is tolerated only when the journal is clean.
///
/// # Errors
///
/// `(cause, errno)` pairs the detach path audits through its refusal
/// helper; a refusal leaves the volume's state untouched (nothing is
/// discarded here — that is the caller's audited force path).
fn commit_for_detach(
    handle: DriverHandle,
    device: Option<&SharedDevice>,
    journal: &Arc<SpinLock<RetainedWrites>>,
) -> Result<(), (&'static str, Errno)> {
    let driver = LATE_FILESYSTEM
        .driver(handle)
        .map_err(|err| ("driver_missing", err))?;
    driver
        .lock()
        .flush()
        .map_err(|err| ("filesystem_flush_failed", err.as_errno()))?;
    // The device cache is flushed through the volume's own shared window,
    // so the flush is serialised against the sibling volumes still driving
    // the same disk rather than racing them over the one staging buffer.
    //
    // A device that vanished mid-detach cannot be committed to at all, which
    // is tolerable only when nothing was uncommitted. That is asked of the
    // endpoint registry rather than inferred from the failure class, so a
    // genuine flush failure on a *live* device is never mistaken for a
    // vanished one and quietly tolerated.
    let Some(device) = device else {
        return vanished_commit(journal);
    };
    match device.window().flush() {
        Ok(()) | Err(DriverError::Unsupported) => {
            journal.lock().commit();
            Ok(())
        }
        Err(err) => {
            if tairix_kernel_core::callreg::contains(EndpointId(device.endpoint)) {
                Err(("device_flush_failed", err.as_errno()))
            } else {
                vanished_commit(journal)
            }
        }
    }
}

/// The commit verdict for a volume whose device vanished before its cache
/// could be flushed: tolerated when nothing was uncommitted, refused when
/// the journal still holds writes the medium never took.
fn vanished_commit(journal: &Arc<SpinLock<RetainedWrites>>) -> Result<(), (&'static str, Errno)> {
    if journal.lock().is_dirty() {
        Err(("device_vanished_dirty", Errno::DeviceFault))
    } else {
        Ok(())
    }
}

/// The facts of an unavailable volume a re-insert may recover
/// (`plans/DEVICES.md` D4c), snapshotted out of the registry so the
/// device I/O below never runs under the registry guard.
struct RecoveryTarget {
    /// The catalog name the volume keeps through recovery.
    name: String,
    /// The existing mount-table path, remounted in place.
    path: Path,
    /// The existing registry handle, re-pointed at the live driver.
    handle: DriverHandle,
    /// The published 16-byte identity the re-insert matched.
    id: [u8; 16],
    /// The journal holding the retained set and the evidence shadow.
    journal: Arc<SpinLock<RetainedWrites>>,
    /// The original extent — the frame of reference the retained LBAs
    /// live in; replay requires the identical extent.
    first_lba: u64,
    blocks: u64,
    /// Whether retention had been abandoned (the unavailable-lost state).
    lost: bool,
}

/// How a re-insert resolved: the retained writes replayed onto the
/// proven-unmutated medium, or the read-only conflict fallback with the
/// cause that forbade replay.
enum RecoveryOutcome {
    /// Non-mutation was proven; this many retained bytes were replayed
    /// and committed.
    Replayed(u64),
    /// Replay was refused for the named cause; the volume returns
    /// read-only with its retained set kept.
    Conflict(&'static str),
}

/// The applied surprise-removal transition [`RuntimeVolumeService::
/// handle_vanished`] reports on: the volume either retracted cleanly or
/// became unavailable with its journal's outcome on record.
enum VanishOutcome {
    /// Nothing was uncommitted: the whole entry retracts.
    Clean(AttachedVolume),
    /// Uncommitted writes exist. `retained` is the held byte count, or
    /// `None` when retention was abandoned (the lost state).
    Unavailable {
        name: String,
        handle: DriverHandle,
        fstype: &'static str,
        id: [u8; 16],
        retained: Option<u64>,
    },
}

impl RuntimeVolumeService {
    /// The recovery target an attach's probed extent `head` re-inserts,
    /// if any: the head must carry a supported signature whose identity
    /// matches an unavailable volume.
    fn reinsert_target(&self, head: Option<&[u8]>) -> Option<RecoveryTarget> {
        let identity = head.and_then(tairix_fsprobe::probe)?.identity;
        self.unavailable_target(&identity)
    }

    /// The unavailable volume the probed `identity` re-inserts, if any.
    /// Only a volume whose serving driver is gone is a recovery target: a
    /// live volume with the same identity is a duplicate, refused by the
    /// normal attach path's publish step.
    fn unavailable_target(&self, identity: &[u8; 16]) -> Option<RecoveryTarget> {
        let state = self.state.lock();
        state
            .iter()
            .find(|v| {
                v.id == *identity
                    && matches!(
                        v.availability,
                        Availability::UnavailableDirty | Availability::UnavailableLost
                    )
            })
            .map(|v| RecoveryTarget {
                name: v.name.clone(),
                path: v.path.clone(),
                handle: v.handle,
                id: v.id,
                journal: Arc::clone(&v.journal),
                first_lba: v.first_lba,
                blocks: v.blocks,
                lost: v.availability == Availability::UnavailableLost,
            })
    }

    /// Prove the re-inserted medium was not mutated elsewhere and replay
    /// the retained writes onto it, or name the conflict that forbids
    /// replay (`plans/DEVICES.md` D4c). Any doubt is a conflict — never a
    /// silent merge.
    fn attempt_replay<B: Block>(
        request: &VolumeAttachRequest<'_>,
        client: &mut B,
        block_size: u32,
        target: &RecoveryTarget,
        device_read_only: bool,
    ) -> RecoveryOutcome {
        // Nothing retained can be replayed after retention was abandoned.
        if target.lost || target.journal.lock().is_lost() {
            return RecoveryOutcome::Conflict("retention_lost");
        }
        // The retained LBAs live in the original extent's frame of
        // reference; a moved or resized extent can never accept them.
        if target.first_lba != request.first_lba || target.blocks != request.blocks {
            return RecoveryOutcome::Conflict("extent_changed");
        }
        // The retained writes cannot land on a now write-protected medium.
        if device_read_only {
            return RecoveryOutcome::Conflict("device_read_only");
        }
        let Some((evidence_lba, evidence_len)) = target.journal.lock().evidence_window() else {
            return RecoveryOutcome::Conflict("no_evidence");
        };
        if evidence_lba != request.first_lba {
            return RecoveryOutcome::Conflict("no_evidence");
        }
        let Some(current) = read_extent_head(client, request, block_size, evidence_len) else {
            return RecoveryOutcome::Conflict("evidence_unreadable");
        };
        if !target.journal.lock().verify_evidence(&current) {
            return RecoveryOutcome::Conflict("evidence_mismatch");
        }
        // Proven unmutated: replay the retained set, commit it to the
        // medium, and empty the journal. A failure mid-replay falls back
        // to the conflict path with the set still held (the journal is
        // only committed after the device confirms the flush).
        let Some(snapshot) = target.journal.lock().retained_snapshot() else {
            return RecoveryOutcome::Conflict("replay_failed");
        };
        let mut replayed = 0u64;
        for (lba, data) in snapshot.blocks() {
            if client.write_blocks(*lba, data).is_err() {
                return RecoveryOutcome::Conflict("replay_failed");
            }
            replayed += data.len() as u64;
        }
        match client.flush() {
            Ok(()) | Err(DriverError::Unsupported) => {}
            Err(_) => return RecoveryOutcome::Conflict("replay_failed"),
        }
        target.journal.lock().commit();
        RecoveryOutcome::Replayed(replayed)
    }

    /// Recover the unavailable volume `target` over the re-inserted
    /// device: verify and replay ([`Self::attempt_replay`]), then rebuild
    /// the volume in place — same handle, mount path, catalog name, and
    /// published root — live and writable when proven, read-only with the
    /// retained set kept when not. Runs under the caller's operation
    /// lock.
    fn recover(
        &self,
        request: &VolumeAttachRequest<'_>,
        wiring: &Wiring,
        device: &SharedDevice,
        target: &RecoveryTarget,
    ) -> Result<(), Errno> {
        let device_read_only = device.read_only;
        let block_size = device.block_size();
        let mut disk = device.window();
        let audit = wiring.audit;
        let refused = |cause: &'static str, errno: Errno| -> Errno {
            RuntimeVolumeService::audit_event(
                audit,
                VOLUME_ATTACH_REFUSED,
                "volume-service: runtime volume attach refused",
                &target.name,
                Some(cause),
            );
            errno
        };
        let vfs = LATE_FILESYSTEM
            .vfs()
            .map_err(|err| refused("no_mount_table", err))?;

        let outcome =
            Self::attempt_replay(request, &mut disk, block_size, target, device_read_only);
        let conflict = matches!(outcome, RecoveryOutcome::Conflict(_));
        // A conflicted volume returns read-only until its retained set is
        // explicitly discarded; a proven one returns per the device.
        let read_only = device_read_only || conflict;

        // Rebuild the volume over the retained journal and the re-inserted
        // device's shared window, carrying the device's reported-health
        // overlay onto the remounted volume.
        let health = device.health.clone();
        let journaled = JournaledBlock::new(disk, Arc::clone(&target.journal), wiring.pressure);
        let window = PartitionBlock::new(journaled, request.first_lba, request.blocks)
            .map_err(|err| refused("window_invalid", err.as_errno()))?;
        let map_gid = identity_map_gid(request.fstype);
        let opened = open_filesystem(
            window,
            request.fstype,
            read_only,
            map_gid,
            wiring,
            target.handle.as_u64(),
        )
        .map_err(|err| refused("recovery_open_failed", err))?;
        if opened.identity != target.id {
            // The head that matched the probe does not govern the opened
            // volume: never adopt a different identity into this entry.
            return Err(refused("recovery_identity_mismatch", Errno::BadMagic));
        }

        // Swap the fail-closed stand-in for the live driver and remount
        // with the recovered posture. A refusal here leaves the handle
        // and path unresolved — fail closed, audited — and cannot occur
        // under the operation lock (the handle was just freed, the path
        // just unmounted).
        let _ = LATE_FILESYSTEM.unregister(target.handle);
        if register_with_health(
            target.handle,
            opened.driver,
            &target.name,
            opened.fstype,
            target.id,
            health,
        )
        .is_err()
        {
            return Err(refused("handle_in_use", Errno::AlreadyExists));
        }
        // A conflicted volume stays read-only `RecoveryConflict`, over which
        // the health overlay never competes; a cleanly recovered one reads
        // `Available` and reflects its device's ongoing health.
        let _ = LATE_FILESYSTEM.set_availability(
            target.handle,
            if conflict {
                MountAvailability::RecoveryConflict
            } else {
                MountAvailability::Available
            },
        );
        {
            let mut mounts = vfs.mounts_write();
            let _ = mounts.unmount(&target.path);
            if mounts
                .mount_with_template(
                    target.path.clone(),
                    mount_flags(read_only),
                    target.handle,
                    mount_template(map_gid),
                )
                .is_err()
            {
                return Err(refused("mount_path_invalid", Errno::AlreadyExists));
            }
        }
        {
            let mut state = self.state.lock();
            if let Some(entry) = state.iter_mut().find(|v| v.id == target.id) {
                entry.endpoint = request.endpoint;
                entry.window = request.window;
                entry.fstype = opened.fstype;
                entry.availability = if conflict {
                    Availability::RecoveryConflict
                } else {
                    Availability::Available
                };
            }
        }
        audit_recovery_outcome(audit, &target.name, &outcome, &target.journal);
        // Close the recover/unplug race exactly as attach does: if the
        // endpoint was torn down while the volume was rebuilt, run the
        // transition the observer would have.
        if !tairix_kernel_core::callreg::contains(EndpointId(request.endpoint)) {
            self.handle_vanished(request.endpoint);
        }
        Ok(())
    }

    /// Decide and apply the state transition for the volume served over
    /// the vanished `endpoint`, under the registry guard: a clean volume
    /// leaves the registry, a dirty/lost one is marked unavailable.
    /// `None` when no available volume matches (idempotency).
    fn vanish_outcome(&self, endpoint: u64) -> Option<VanishOutcome> {
        let mut state = self.state.lock();
        // A conflicted volume is served by a live driver too: when that
        // driver dies its cache-bearing stand-in must be replaced and the
        // (still-dirty) journal re-decides the unavailable state.
        let index = state.iter().position(|v| {
            v.endpoint == endpoint
                && matches!(
                    v.availability,
                    Availability::Available | Availability::RecoveryConflict
                )
        })?;
        let (dirty, lost, retained) = {
            let journal = state[index].journal.lock();
            (
                journal.is_dirty(),
                journal.is_lost(),
                journal.retained_bytes(),
            )
        };
        if !dirty {
            return Some(VanishOutcome::Clean(state.remove(index)));
        }
        state[index].availability = if lost {
            Availability::UnavailableLost
        } else {
            Availability::UnavailableDirty
        };
        Some(VanishOutcome::Unavailable {
            name: state[index].name.clone(),
            handle: state[index].handle,
            fstype: state[index].fstype,
            id: state[index].id,
            retained: (!lost).then_some(retained),
        })
    }

    /// The surprise-removal transition for the volume attached over the
    /// vanished block-service endpoint `endpoint` (`plans/DEVICES.md`
    /// D4): clean volumes retract, dirty ones become unavailable with
    /// their retained set held, lost ones become unavailable with the
    /// loss on record. Idempotent — a volume already transitioned (or
    /// detached) is left alone. Takes no sleeping lock, so it is safe
    /// from the endpoint-teardown context.
    fn handle_vanished(&self, endpoint: u64) {
        let Ok(wiring) = self.wiring() else {
            return;
        };
        let audit = wiring.audit;
        match self.vanish_outcome(endpoint) {
            None => {}
            Some(VanishOutcome::Clean(entry)) => {
                if let Ok(vfs) = LATE_FILESYSTEM.vfs() {
                    let _ = vfs.mounts_write().unmount(&entry.path);
                }
                let _ = LATE_FILESYSTEM.unregister(entry.handle);
                let _ = VOLUME_FOREST.unpublish(&entry.id);
                RuntimeVolumeService::audit_event(
                    audit,
                    VOLUME_SURPRISE_REMOVED_CLEAN,
                    "volume-service: surprise removal with nothing uncommitted; volume retracted",
                    &entry.name,
                    None,
                );
            }
            Some(VanishOutcome::Unavailable {
                name,
                handle,
                fstype,
                id,
                retained,
            }) => {
                // The unavailable volume's registry slot is re-pointed at
                // the fail-closed stand-in: the real driver (and its
                // plaintext cache) drops, so nothing of the vanished
                // medium is ever served as live; the mount and the
                // published root stay visible — and the registry entry is
                // marked unavailable, so the mount snapshot never shows
                // the vanished volume as healthy.
                let _ = LATE_FILESYSTEM.unregister(handle);
                let _ = LATE_FILESYSTEM.register(
                    handle,
                    alloc::boxed::Box::new(UnavailableFs),
                    &name,
                    fstype,
                    id,
                );
                let _ = LATE_FILESYSTEM.set_availability(
                    handle,
                    match retained {
                        Some(_) => MountAvailability::UnavailableDirty,
                        None => MountAvailability::UnavailableLost,
                    },
                );
                match retained {
                    Some(retained) => {
                        log(
                            audit,
                            &Event {
                                level: Level::Warn,
                                id: VOLUME_SURPRISE_REMOVED_DIRTY,
                                message: "volume-service: surprise removal with uncommitted \
                                          writes retained; volume unavailable-dirty",
                                fields: &[
                                    Field {
                                        key: "volume",
                                        value: FieldValue::Str(&name),
                                    },
                                    Field {
                                        key: "retained_bytes",
                                        value: FieldValue::UnsignedInt(retained),
                                    },
                                ],
                            },
                        );
                    }
                    None => RuntimeVolumeService::audit_event(
                        audit,
                        VOLUME_SURPRISE_REMOVED_LOST,
                        "volume-service: surprise removal after retention was abandoned; \
                         uncommitted data was not retained",
                        &name,
                        Some("retention_abandoned"),
                    ),
                }
            }
        }
    }
}

impl EndpointVanishObserver for RuntimeVolumeService {
    fn endpoint_vanished(&self, id: EndpointId) {
        self.handle_vanished(id.0);
    }
}

#[cfg(test)]
#[path = "volume_service_tests.rs"]
mod tests;
