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
//! requests can never interleave a half-built volume.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};

use rustos_abi::driver::block::Block;
use rustos_abi::driver::filesystem::MountFlags;
use rustos_abi::volume::{VolumeAttachRequest, VolumeDetachRequest, VolumeFsType};
use rustos_abi::{DriverError, DriverHandle, Errno};
use rustos_drv_fs_ext4::Ext4;
use rustos_drv_fs_fat32::Fat32;
use rustos_drv_fs_rustfs::{RustFs, SYSTEM_VOLUME_KEY};
use rustos_kernel_core::devres::installed_shared_mem_facility;
use rustos_kernel_core::fs::blkclient::BlkClient;
use rustos_kernel_core::sharedreg::kernel_hold;
use rustos_kernel_core::{Metadata, Mode, Path, SleepLock, VolumePublishError, VolumeService};
use rustos_kernel_mem::MemoryPressure;
use rustos_kernel_sec::{GroupId, UserId};
use rustos_log::{log, Event, EventId, Field, FieldValue, Level, Sink};
use rustos_partition::PartitionBlock;
use rustos_sync::OnceCell;

use crate::kernel_fs::KernelFs;
use crate::system_mount::{cached, publish_volume_identity, LATE_FILESYSTEM, VOLUME_FOREST};

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

/// Base of the runtime volume mount-handle space (`"VOL"` tagged). Fresh
/// handles are minted from here, disjoint from the boot volumes' fixed
/// handles by construction.
const VOLUME_HANDLE_BASE: u64 = 0x564F_4C00_0000_0000;

/// One attached runtime volume: the facts detach needs.
struct AttachedVolume {
    /// The volume's published 16-byte identity.
    id: [u8; 16],
    /// The catalog name the root is projected under.
    name: String,
    /// The mount-table entry's path (`/Storage/<name>`).
    path: Path,
    /// The mount's driver handle in the registry.
    handle: DriverHandle,
    /// The block-service endpoint, for the detach-time device flush.
    endpoint: u64,
    /// The shared data window's region id, for the detach-time device
    /// flush.
    window: u64,
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
    /// The attached-volume registry, doubling as the whole-operation
    /// serialisation lock: attach/detach park on device I/O, so the lock
    /// sleeps, never spins.
    state: SleepLock<Vec<AttachedVolume>>,
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
            state: SleepLock::new(Vec::new()),
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
fn open_filesystem(
    window: PartitionBlock<BlkClient>,
    fstype: VolumeFsType,
    read_only: bool,
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
            Ok(OpenedVolume {
                driver: cached(fs, volume_handle, wiring.pressure, wiring.audit),
                identity,
                fstype: "fat32",
            })
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
        let mut state = self.state.lock();

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
        // Reach the shared data window through the kernel's counted hold.
        let hold = kernel_hold(installed_shared_mem_facility(), request.window)
            .map_err(|err| refused("window_unreachable", err))?;
        // Connect the kernel blkio client and validate the device geometry.
        let client = BlkClient::connect(request.endpoint, hold, audit)
            .map_err(|err| refused("endpoint_unusable", err))?;
        let read_only = client.read_only();
        // Bound the requested extent by the live geometry, then window it.
        let device_blocks = client
            .geometry()
            .map_err(|err| refused("geometry_unreadable", err.as_errno()))?
            .block_count;
        if request
            .first_lba
            .checked_add(request.blocks)
            .map_or(true, |end| end > device_blocks)
        {
            return Err(refused("extent_out_of_range", Errno::LengthOutOfRange));
        }
        let window = PartitionBlock::new(client, request.first_lba, request.blocks)
            .map_err(|err| refused("window_invalid", err.as_errno()))?;

        // Open the matched filesystem and take its stable identity.
        let handle_raw = self.next_handle.fetch_add(1, Ordering::Relaxed);
        let opened = open_filesystem(window, request.fstype, read_only, wiring, handle_raw)
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
        // point has no node in the boot layout tree): system-owned,
        // world-traversable, writes gated per inode by the volume's own
        // records. The volume manager's mount policy (the storage-group
        // identity map) refines this when it lands.
        let template = Metadata::new(UserId(0), GroupId(0), Mode::from_bits(0o755));
        vfs.mounts_write()
            .mount_with_template(path.clone(), flags, handle, template)
            .map_err(|_| refused("name_in_use", Errno::AlreadyExists))?;

        // Register the live driver, then publish the identity last; each
        // failure unwinds everything already done, so a refused attach
        // leaves no trace.
        if LATE_FILESYSTEM
            .register(handle, opened.driver, name, opened.fstype)
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

        state.push(AttachedVolume {
            id: opened.identity,
            name: String::from(name),
            path,
            handle,
            endpoint: request.endpoint,
            window: request.window,
        });
        RuntimeVolumeService::audit_event(
            audit,
            VOLUME_ATTACHED,
            "volume-service: runtime volume attached and published",
            name,
            None,
        );
        Ok(())
    }

    fn detach(&self, request: &VolumeDetachRequest) -> Result<(), Errno> {
        let wiring = self.wiring()?;
        let audit = wiring.audit;
        let mut state = self.state.lock();

        let Some(index) = state.iter().position(|v| v.id == request.volume_id) else {
            RuntimeVolumeService::audit_event(
                audit,
                VOLUME_DETACH_REFUSED,
                "volume-service: runtime volume detach refused",
                "unknown",
                Some("identity_not_attached"),
            );
            return Err(Errno::NotFound);
        };

        // Split the refusal helper from the entry borrow so both can live.
        let (name, path, handle, endpoint, window) = {
            let entry = &state[index];
            (
                entry.name.clone(),
                entry.path.clone(),
                entry.handle,
                entry.endpoint,
                entry.window,
            )
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

        // Flush the filesystem's own state first, while it is still
        // registered — a failure leaves the volume fully attached (data is
        // never silently discarded; forced discard is the future
        // force-unmount operation).
        let driver = LATE_FILESYSTEM
            .driver(handle)
            .map_err(|err| refused("driver_missing", err))?;
        driver
            .lock()
            .flush()
            .map_err(|err| refused("filesystem_flush_failed", err.as_errno()))?;

        // Commit the device's own cache. A vanished endpoint or window
        // (`NotFound` — the device was already unplugged and its driver
        // exited) is not a refusal: there is no medium left to flush, and
        // the retraction below is exactly what a surprise removal needs.
        match kernel_hold(installed_shared_mem_facility(), window)
            .and_then(|hold| BlkClient::connect(endpoint, hold, audit))
        {
            Ok(mut client) => match client.flush() {
                Ok(()) | Err(DriverError::Unsupported) => {}
                Err(err) => return Err(refused("device_flush_failed", err.as_errno())),
            },
            Err(Errno::NotFound) => {}
            Err(err) => return Err(refused("device_unreachable", err)),
        }

        // Retract: unmount (new resolutions fail closed), unregister (the
        // registry's driver reference drops; in-flight operations finish
        // on their own clones), and withdraw the published identity.
        let vfs = LATE_FILESYSTEM
            .vfs()
            .map_err(|err| refused("no_mount_table", err))?;
        vfs.mounts_write()
            .unmount(&path)
            .map_err(|_| refused("mount_missing", Errno::NotFound))?;
        let _ = LATE_FILESYSTEM.unregister(handle);
        let _ = VOLUME_FOREST.unpublish(&request.volume_id);
        state.remove(index);
        RuntimeVolumeService::audit_event(
            audit,
            VOLUME_DETACHED,
            "volume-service: runtime volume flushed, unmounted, and withdrawn",
            &name,
            None,
        );
        Ok(())
    }
}

#[cfg(test)]
#[path = "volume_service_tests.rs"]
mod tests;
