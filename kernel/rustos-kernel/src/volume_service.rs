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
//! [`rustos_kernel_core::callreg::EndpointVanishObserver`]: when a
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

use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};

use rustos_abi::driver::block::Block;
use rustos_abi::driver::filesystem::{
    DirEntry, FilesystemRead, FilesystemSecurity, FilesystemStats, FilesystemWrite, MountFlags,
    NodeId, NodeInfo, NodeKind, NodeSecurity, VolumeStats,
};
use rustos_abi::sysinfo::MountAvailability;
use rustos_abi::volume::{VolumeAttachRequest, VolumeDetachRequest, VolumeFsType};
use rustos_abi::{DriverError, DriverHandle, Errno};
use rustos_drv_fs_ext4::Ext4;
use rustos_drv_fs_fat32::Fat32;
use rustos_drv_fs_rustfs::{RustFs, SYSTEM_VOLUME_KEY};
use rustos_kernel_core::callreg::EndpointVanishObserver;
use rustos_kernel_core::devres::installed_shared_mem_facility;
use rustos_kernel_core::fs::blkclient::BlkClient;
use rustos_kernel_core::fs::{JournaledBlock, RetainedWrites};
use rustos_kernel_core::sharedreg::kernel_hold;
use rustos_kernel_core::{Metadata, Mode, Path, SleepLock, VolumePublishError, VolumeService};
use rustos_kernel_ipc::EndpointId;
use rustos_kernel_mem::{CacheBudget, MemoryPressure};
use rustos_kernel_sec::{GroupId, UserId};
use rustos_log::{log, Event, EventId, Field, FieldValue, Level, Sink};
use rustos_partition::PartitionBlock;
use rustos_sync::{OnceCell, SpinLock};

use crate::kernel_fs::KernelFs;
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
    window: PartitionBlock<JournaledBlock<BlkClient>>,
    fstype: VolumeFsType,
    read_only: bool,
    map_gid: Option<GroupId>,
    wiring: &Wiring,
    volume_handle: u64,
) -> Result<OpenedVolume, Errno> {
    match fstype {
        VolumeFsType::RustFs => {
            // A removable RustFS volume is keyed like any RustFS volume.
            // The well-known key covers non-secret volumes (the same key
            // the read-only system volume uses); a volume under a private
            // key refuses the open with a typed error, and key-provisioned
            // attach arrives with the volume manager's key policy — the
            // kernel never guesses a secret.
            let fs = if read_only {
                RustFs::open_read_only(window, &SYSTEM_VOLUME_KEY)
            } else {
                RustFs::open(window, &SYSTEM_VOLUME_KEY)
            }
            .map_err(DriverError::as_errno)?;
            let identity = fs.volume_uuid();
            Ok(OpenedVolume {
                driver: cached(fs, volume_handle, wiring.pressure, wiring.audit),
                identity,
                fstype: "rustfs",
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

/// The journalled extent window an attach opens its filesystem over,
/// together with the shared journal handle and the device write policy.
struct AttachedWindow {
    window: PartitionBlock<JournaledBlock<BlkClient>>,
    journal: Arc<SpinLock<RetainedWrites>>,
    read_only: bool,
}

/// Connect the kernel blkio client for `request`, validate the extent
/// against the live geometry, and thread the device through the
/// uncommitted-write journal (the volume's surprise-removal state is
/// decided by what that journal holds when the serving driver dies).
///
/// # Errors
///
/// `(cause, errno)` pairs the attach path audits through its refusal
/// helper.
fn journaled_window(
    request: &VolumeAttachRequest<'_>,
    wiring: &Wiring,
) -> Result<AttachedWindow, (&'static str, Errno)> {
    // Reach the shared data window through the kernel's counted hold.
    let hold = kernel_hold(installed_shared_mem_facility(), request.window)
        .map_err(|err| ("window_unreachable", err))?;
    // Connect the kernel blkio client and validate the device geometry.
    let client = BlkClient::connect(request.endpoint, hold, wiring.audit)
        .map_err(|err| ("endpoint_unusable", err))?;
    let read_only = client.read_only();
    // Bound the requested extent by the live geometry, then window it.
    let geometry = client
        .geometry()
        .map_err(|err| ("geometry_unreadable", err.as_errno()))?;
    if request
        .first_lba
        .checked_add(request.blocks)
        .map_or(true, |end| end > geometry.block_count)
    {
        return Err(("extent_out_of_range", Errno::LengthOutOfRange));
    }
    let journal = Arc::new(SpinLock::new(RetainedWrites::new(
        geometry.block_size,
        CacheBudget::from_backing(rustos_kalloc::HEAP_BYTES),
    )));
    let journaled = JournaledBlock::new(client, Arc::clone(&journal), wiring.pressure);
    let window = PartitionBlock::new(journaled, request.first_lba, request.blocks)
        .map_err(|err| ("window_invalid", err.as_errno()))?;
    Ok(AttachedWindow {
        window,
        journal,
        read_only,
    })
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
        let AttachedWindow {
            window,
            journal,
            read_only,
        } = journaled_window(request, wiring).map_err(|(cause, errno)| refused(cause, errno))?;

        // Open the matched filesystem and take its stable identity. An
        // ownerless format (FAT32) is mounted under the storage-group
        // identity map when the unlock has resolved that group; formats
        // with a real owner model keep their on-disk records.
        let map_gid = match request.fstype {
            VolumeFsType::Fat32 => LATE_STORAGE_GID.get(),
            VolumeFsType::RustFs | VolumeFsType::Ext4 => None,
        };
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
        let path = Path::parse(&format!("/Storage/{name}"))
            .map_err(|_| refused("mount_path_invalid", Errno::OutOfRange))?;
        let mut flags = MountFlags::NOSUID
            .union(MountFlags::NODEV)
            .union(MountFlags::NOEXEC);
        if read_only {
            flags = flags.union(MountFlags::READ_ONLY);
        }
        // The mount carries its own permission template (a runtime mount
        // point has no node in the boot layout tree). An identity-mapped
        // volume's template matches the map — system-owned under the
        // storage group, group-writable — so the mount point itself is as
        // reachable as its content; every other volume gets the
        // system-owned world-traversable default, writes gated per inode
        // by the volume's own records.
        let template = match map_gid {
            Some(gid) => Metadata::new(UserId(0), gid, Mode::from_bits(0o775)),
            None => Metadata::new(UserId(0), GroupId(0), Mode::from_bits(0o755)),
        };
        vfs.mounts_write()
            .mount_with_template(path.clone(), flags, handle, template)
            .map_err(|_| refused("name_in_use", Errno::AlreadyExists))?;

        // Register the live driver, then publish the identity last; each
        // failure unwinds everything already done, so a refused attach
        // leaves no trace.
        if LATE_FILESYSTEM
            .register(handle, opened.driver, name, opened.fstype, opened.identity)
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
        if !rustos_kernel_core::callreg::contains(EndpointId(request.endpoint)) {
            self.handle_vanished(request.endpoint);
        }
        Ok(())
    }

    fn detach(&self, request: &VolumeDetachRequest) -> Result<(), Errno> {
        let wiring = self.wiring()?;
        let audit = wiring.audit;
        // Serialise whole attach/detach operations (see the struct docs).
        let _op = self.op.lock();

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
                commit_for_detach(handle, endpoint, window, &journal, audit).err()
            }
            Availability::UnavailableDirty => {
                Some(("volume_unavailable_dirty", Errno::DeviceFault))
            }
            Availability::UnavailableLost => Some(("volume_unavailable_lost", Errno::DeviceFault)),
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
    endpoint: u64,
    window: u64,
    journal: &Arc<SpinLock<RetainedWrites>>,
    audit: &'static (dyn Sink + Sync),
) -> Result<(), (&'static str, Errno)> {
    let driver = LATE_FILESYSTEM
        .driver(handle)
        .map_err(|err| ("driver_missing", err))?;
    driver
        .lock()
        .flush()
        .map_err(|err| ("filesystem_flush_failed", err.as_errno()))?;
    match kernel_hold(installed_shared_mem_facility(), window)
        .and_then(|hold| BlkClient::connect(endpoint, hold, audit))
    {
        Ok(mut client) => match client.flush() {
            Ok(()) | Err(DriverError::Unsupported) => {
                journal.lock().commit();
                Ok(())
            }
            Err(err) => Err(("device_flush_failed", err.as_errno())),
        },
        Err(Errno::NotFound) => {
            if journal.lock().is_dirty() {
                Err(("device_vanished_dirty", Errno::DeviceFault))
            } else {
                Ok(())
            }
        }
        Err(err) => Err(("device_unreachable", err)),
    }
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
    /// Decide and apply the state transition for the volume served over
    /// the vanished `endpoint`, under the registry guard: a clean volume
    /// leaves the registry, a dirty/lost one is marked unavailable.
    /// `None` when no available volume matches (idempotency).
    fn vanish_outcome(&self, endpoint: u64) -> Option<VanishOutcome> {
        let mut state = self.state.lock();
        let index = state
            .iter()
            .position(|v| v.endpoint == endpoint && v.availability == Availability::Available)?;
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
