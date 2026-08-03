//! Host tests for the runtime volume attach/detach service.
//!
//! The lifecycle test drives the whole D3b path in-process against the
//! real global boot statics (`LATE_FILESYSTEM`, `VOLUME_FOREST`,
//! `VOLUME_SERVICE`, `LATE_IDENTITY`, `LATE_STORAGE_GID`, the recorded
//! shared-memory facility): a FAT32 image is served over a genuine call
//! endpoint from another thread (the shape a user-space block driver
//! has), attached under the storage-group identity map (D3d), read and
//! written through the production `fs_*` service as a group member —
//! with a non-member's write refused — and detached. It is the **only**
//! test in this crate that touches those statics (the system-mount tests
//! deliberately avoid them), and it runs as one sequential test function
//! so the set-once cells are installed exactly once.

extern crate std;

use std::boxed::Box;
use std::string::String;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc as StdArc, Mutex};
use std::thread;
use std::vec;
use std::vec::Vec;

use tairix_abi::blkio::{
    encode_error_completion, BlkCompletion, BlkDeviceClass, BlkOp, BlkRequest, BLK_COMPLETION_LEN,
    BLK_DATA_LEN, BLK_REQUEST_LEN,
};
use tairix_abi::driver::block::{Block, BlockGeometry};
use tairix_abi::driver::filesystem::{FilesystemRead, FilesystemWrite, NodeKind};
use tairix_abi::sysinfo::{MountAvailability, MountRecord};
use tairix_abi::volume::{VolumeAttachRequest, VolumeDetachRequest, VolumeFsType};
use tairix_abi::{CapabilityId, CapabilityQuery, DriverError, Errno};
use tairix_caps::CapabilitySet;
use tairix_drv_fs_fat32::Fat32;
use tairix_kernel_core::devres::{install_shared_mem_facility, SharedChunk, SharedMemFacility};
use tairix_kernel_core::fs::FilesystemService;
use tairix_kernel_core::{sharedreg, Vfs};
use tairix_kernel_ipc::{CallEndpoint, CallEndpointLimits, EndpointId, RecvCall};
use tairix_kernel_sec::captable::TaskCapabilities;
use tairix_kernel_sec::{GroupId, GroupRecord, IdentityTableBuilder, TaskId, UserId, UserRecord};
use tairix_log::Sink;
use tairix_reclaim::{FreeMemorySource, MemoryPressure};

use crate::root_mount::LATE_IDENTITY;
use crate::system_mount::{FS_SERVICE, LATE_FILESYSTEM, VOLUME_FOREST};
use crate::volume_policy::LATE_STORAGE_GID;
use crate::volume_service::{RuntimeVolumeService, VOLUME_SERVICE};
use tairix_kernel_core::VolumeService as _;

/// A throwaway audit sink.
struct NullSink;
impl Sink for NullSink {
    fn write_event(&self, _event: &tairix_log::Event<'_>) {}
}
static SINK: NullSink = NullSink;

/// The address-space registry endpoint teardown revokes per-endpoint
/// grants against.
///
/// Destroying an endpoint and withdrawing the authority naming its id are
/// one step, so the teardown takes the registry that holds those grants.
/// These scenarios drive the block service directly rather than through a
/// granted task, so it stays empty and nothing is revoked; it exists so the
/// scenarios exercise the production teardown, not a variant of it.
static ASPACES: tairix_sync::RwLock<tairix_kernel_core::aspace::AddressSpaceRegistry> =
    tairix_sync::RwLock::new(tairix_kernel_core::aspace::AddressSpaceRegistry::new());

/// A fixed, ample memory source for the pressure gauge.
struct AmpleSource;
impl FreeMemorySource for AmpleSource {
    fn free_bytes(&self) -> usize {
        1 << 30
    }
    fn total_bytes(&self) -> usize {
        1 << 30
    }
}
static AMPLE: AmpleSource = AmpleSource;

/// A caller that holds no capability: the per-inode records on the test
/// volume gate nothing, so the secured read needs none.
struct NoCaps;
impl CapabilityQuery for NoCaps {
    fn holds(&self, _cap: CapabilityId) -> bool {
        false
    }
}

/// The storage group's gid on this test system, and the two ordinary
/// principals the identity-map assertions run as: a member of the group
/// and a non-member.
const STORAGE_GID: u32 = 100;
const MEMBER_UID: u32 = 1000;
const OUTSIDER_UID: u32 = 2000;

const BLOCK_SIZE: usize = 512;
/// `BLOCK_SIZE` as the wire-width type the geometry carries.
const BLOCK_SIZE_U32: u32 = 512;
/// 64 MiB — comfortably past the FAT32 minimum cluster count.
const SECTORS_64MIB: u64 = (64 << 20) / 512;

/// The shared-memory facility double behind the kernel window: one leaked
/// `BLK_DATA_LEN` buffer serves every translation (the test creates one
/// region).
struct TestFacility {
    window: *mut u8,
}
// SAFETY: the window is leaked process-lifetime memory; the facility only
// hands the raw pointer out and frees nothing.
unsafe impl Sync for TestFacility {}

impl SharedMemFacility for TestFacility {
    fn alloc_region(&self, pages: u64) -> Result<Vec<SharedChunk>, Errno> {
        // A single contiguous chunk (the block-service data window is small).
        Ok(vec![SharedChunk {
            phys_base: 0x4000_0000,
            order: 0,
            pages,
        }])
    }
    fn map_region(&self, chunks: &[SharedChunk]) -> Result<u64, Errno> {
        Ok(0x9000_0000 + chunks[0].phys_base)
    }
    fn unmap_region(&self, _base: u64, _len: usize) -> Result<(), Errno> {
        Ok(())
    }
    fn free_region(&self, _chunks: &[SharedChunk]) {}
    fn kernel_window(&self, chunks: &[SharedChunk], _len: usize) -> Option<core::ptr::NonNull<u8>> {
        // The single-chunk region resolves to the one leaked test window.
        if chunks.len() == 1 {
            core::ptr::NonNull::new(self.window)
        } else {
            None
        }
    }
}

/// A `Vec`-backed 512-byte-block device.
struct RamBlock {
    data: Vec<u8>,
}

impl RamBlock {
    fn new(sectors: u64) -> Self {
        Self {
            data: vec![0u8; usize::try_from(sectors).expect("fits") * BLOCK_SIZE],
        }
    }
}

impl Block for RamBlock {
    fn device_class(&self) -> BlkDeviceClass {
        // These scenarios attach a removable stick, so the fixture declares
        // that medium rather than leaving the trait's paravirtual default:
        // a mount that reports it can only have learned it from here.
        BlkDeviceClass::Removable
    }

    fn geometry(&self) -> Result<BlockGeometry, DriverError> {
        Ok(BlockGeometry {
            block_size: BLOCK_SIZE_U32,
            block_count: (self.data.len() / BLOCK_SIZE) as u64,
        })
    }

    fn read_blocks(&mut self, lba: u64, buf: &mut [u8]) -> Result<(), DriverError> {
        let start = usize::try_from(lba).map_err(|_| DriverError::LengthOutOfRange)? * BLOCK_SIZE;
        let end = start
            .checked_add(buf.len())
            .filter(|&end| {
                end <= self.data.len() && !buf.is_empty() && buf.len().is_multiple_of(BLOCK_SIZE)
            })
            .ok_or(DriverError::LengthOutOfRange)?;
        buf.copy_from_slice(&self.data[start..end]);
        Ok(())
    }

    fn write_blocks(&mut self, lba: u64, buf: &[u8]) -> Result<(), DriverError> {
        let start = usize::try_from(lba).map_err(|_| DriverError::LengthOutOfRange)? * BLOCK_SIZE;
        let end = start
            .checked_add(buf.len())
            .filter(|&end| {
                end <= self.data.len() && !buf.is_empty() && buf.len().is_multiple_of(BLOCK_SIZE)
            })
            .ok_or(DriverError::LengthOutOfRange)?;
        self.data[start..end].copy_from_slice(buf);
        Ok(())
    }

    fn flush(&mut self) -> Result<(), DriverError> {
        Ok(())
    }
}

/// The served device: image bytes, the storage medium its geometry reply
/// declares, a flush counter, and a count of the geometry queries served.
///
/// The geometry count is how many times a consumer *connected* to this
/// device: connecting is the only thing that asks for the geometry, so the
/// count is the observable form of "how many block clients exist for this
/// one device" — the fact every volume on a disk must share one
/// (`plans/FIX-IO.md`).
struct ServedDevice {
    block: RamBlock,
    class: Option<BlkDeviceClass>,
    flushes: usize,
    geometries: usize,
}

impl ServedDevice {
    /// A device whose geometry reply declares the medium its block backing
    /// reports.
    fn new(block: RamBlock) -> Self {
        Self {
            class: Some(block.device_class()),
            block,
            flushes: 0,
            geometries: 0,
        }
    }

    /// A device whose geometry reply carries a class word this ABI does not
    /// define — a driver naming a medium the client cannot recognise.
    fn unclassified(block: RamBlock) -> Self {
        Self {
            class: None,
            block,
            flushes: 0,
            geometries: 0,
        }
    }
}

/// Serve blkio requests over `endpoint` against `device`, moving data
/// through the raw `window` pointer, until `stop` is set.
fn serve(
    endpoint: StdArc<CallEndpoint>,
    window: usize,
    device: StdArc<Mutex<ServedDevice>>,
    stop: StdArc<AtomicBool>,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        let window = window as *mut u8;
        while !stop.load(Ordering::Relaxed) {
            let RecvCall::Received(call) = endpoint.recv_call(BLK_REQUEST_LEN) else {
                thread::yield_now();
                continue;
            };
            let mut reply = [0u8; BLK_COMPLETION_LEN];
            let len = match BlkRequest::decode(&call.request) {
                Err(err) => encode_error_completion(&mut reply, err).unwrap(),
                Ok(request) => {
                    let mut device = device.lock().unwrap();
                    let bytes = request.blocks as usize * BLOCK_SIZE;
                    match request.op {
                        BlkOp::Geometry => {
                            device.geometries += 1;
                            let geometry = device.block.geometry().unwrap();
                            BlkCompletion {
                                block_size: geometry.block_size,
                                block_count: geometry.block_count,
                                flags: 0,
                                class: device.class,
                            }
                            .encode(&mut reply)
                            .unwrap()
                        }
                        BlkOp::Read => {
                            let mut buf = vec![0u8; bytes];
                            match device.block.read_blocks(request.lba, &mut buf) {
                                // SAFETY: test window of BLK_DATA_LEN bytes,
                                // protocol-alternated access, bytes bounded
                                // by the serving contract.
                                Ok(()) => unsafe {
                                    core::ptr::copy_nonoverlapping(buf.as_ptr(), window, bytes);
                                    BlkCompletion::default().encode(&mut reply).unwrap()
                                },
                                Err(err) => {
                                    encode_error_completion(&mut reply, err.as_errno()).unwrap()
                                }
                            }
                        }
                        BlkOp::Write => {
                            let mut buf = vec![0u8; bytes];
                            // SAFETY: as for the read arm.
                            unsafe {
                                core::ptr::copy_nonoverlapping(window, buf.as_mut_ptr(), bytes);
                            }
                            match device.block.write_blocks(request.lba, &buf) {
                                Ok(()) => BlkCompletion::default().encode(&mut reply).unwrap(),
                                Err(err) => {
                                    encode_error_completion(&mut reply, err.as_errno()).unwrap()
                                }
                            }
                        }
                        BlkOp::Flush => {
                            device.flushes += 1;
                            BlkCompletion::default().encode(&mut reply).unwrap()
                        }
                    }
                }
            };
            endpoint.reply(call.ticket, &reply[..len], &SINK).unwrap();
        }
    })
}

/// Build, register, and return the per-LUN block-service endpoint, owned
/// by the task `owner` (the serving driver's identity — the one the
/// endpoint-teardown path matches on unplug).
fn register_endpoint(id: u64, owner: u64) -> StdArc<CallEndpoint> {
    let creator = TaskCapabilities::derive(
        TaskId(owner),
        UserId(0),
        CapabilitySet::empty(),
        CapabilitySet::empty(),
        &SINK,
    );
    let endpoint = StdArc::new(
        CallEndpoint::create(
            EndpointId(id),
            &creator,
            CapabilitySet::empty(),
            CapabilitySet::empty(),
            CallEndpointLimits {
                max_request: u32::try_from(BLK_REQUEST_LEN).unwrap(),
                max_reply: u32::try_from(BLK_COMPLETION_LEN).unwrap(),
                capacity: 4,
            },
            &SINK,
        )
        .expect("endpoint"),
    );
    tairix_kernel_core::callreg::register(StdArc::clone(&endpoint), &SINK)
        .expect("register endpoint");
    endpoint
}

#[test]
fn an_unwired_service_fails_closed() {
    let service = RuntimeVolumeService::new();
    let attach = VolumeAttachRequest {
        endpoint: 1,
        window: 1,
        first_lba: 0,
        blocks: 8,
        fstype: VolumeFsType::Fat32,
        name: b"usb1",
    };
    assert_eq!(service.attach(&attach), Err(Errno::NotImplemented));
    assert_eq!(
        service.detach(&VolumeDetachRequest {
            volume_id: [7; 16],
            force: false
        }),
        Err(Errno::NotImplemented)
    );
}

/// The identity table the lifecycle test installs: the system principal,
/// a member of the storage group, and a non-member.
fn identity_with_storage_member() -> tairix_kernel_sec::IdentityTable {
    let mut identity = IdentityTableBuilder::new();
    for gid in [0, STORAGE_GID, OUTSIDER_UID] {
        identity.push_group(GroupRecord { gid: GroupId(gid) });
    }
    for (uid, gid) in [
        (0, 0),
        (MEMBER_UID, STORAGE_GID),
        (OUTSIDER_UID, OUTSIDER_UID),
    ] {
        identity.push_user(UserRecord {
            uid: UserId(uid),
            primary_gid: GroupId(gid),
            supplementary_gids: Vec::new(),
            capability_grants: CapabilitySet::empty(),
        });
    }
    identity.verify(&SINK).expect("identity table")
}

/// Format a FAT32 image carrying `hello.txt` with `payload` under the
/// caller-minted BPB `serial`, returning the image and its volume
/// identity.
fn fat32_image(serial: u32, payload: &[u8]) -> (RamBlock, [u8; 16]) {
    let mut fs = Fat32::format(RamBlock::new(SECTORS_64MIB), serial).expect("format");
    let root = fs.root();
    fs.create(root, b"hello.txt", NodeKind::RegularFile)
        .expect("create");
    fs.write_at(root, b"hello.txt", 0, payload).expect("write");
    let image = fs.into_block();
    let identity = Fat32::open(RamBlock {
        data: image.data.clone(),
    })
    .expect("probe")
    .volume_identity();
    (image, identity)
}

/// The mount-table record the System Information mount list publishes for
/// the volume attached under `source`.
fn mount_record(source: &[u8]) -> MountRecord {
    FS_SERVICE
        .mount_snapshot()
        .into_iter()
        .find(|record| record.source_bytes() == source)
        .unwrap_or_else(|| std::panic!("no mount snapshot record for {source:?}"))
}

/// The storage-group identity map governs ordinary users on the attached
/// volume: every node appears system-owned under the storage group, files
/// `rw-rw-r--`, so a member reads and writes, a non-member reads but is
/// refused a write — and the on-disk FAT volume never stores any of it.
fn assert_identity_mapped_access() {
    let mut buf = [0u8; 64];
    let stat = FS_SERVICE
        .stat(MEMBER_UID, &NoCaps, "/Storage/usb1/hello.txt")
        .expect("member stats the file");
    assert_eq!(
        (stat.mode, stat.uid, stat.gid),
        (0o664, 0, STORAGE_GID),
        "the identity map presents system ownership under the storage group"
    );
    let n = FS_SERVICE
        .read(MEMBER_UID, &NoCaps, "/Storage/usb1/hello.txt", 0, &mut buf)
        .expect("member reads");
    assert_eq!(&buf[..n], b"runtime volume payload");
    FS_SERVICE
        .write(
            MEMBER_UID,
            &NoCaps,
            "/Storage/usb1/hello.txt",
            0,
            false,
            b"R",
        )
        .expect("member writes through the group grant");
    assert_eq!(
        FS_SERVICE
            .write(
                OUTSIDER_UID,
                &NoCaps,
                "/Storage/usb1/hello.txt",
                0,
                false,
                b"X"
            )
            .err(),
        Some(Errno::PermissionDenied),
        "a non-member's write is refused"
    );
    let n = FS_SERVICE
        .read(
            OUTSIDER_UID,
            &NoCaps,
            "/Storage/usb1/hello.txt",
            0,
            &mut buf,
        )
        .expect("a non-member still reads (other class)");
    assert_eq!(&buf[..1], b"R", "the member's write landed");
    assert!(n >= 1);
}

#[test]
fn attach_read_detach_lifecycle_over_a_served_fat32_volume() {
    // --- One-time global wiring (this is the only test that touches the
    // boot statics). ---
    let pressure: &'static MemoryPressure = Box::leak(Box::new(MemoryPressure::over(&AMPLE)));
    VOLUME_SERVICE.install(&SINK, pressure);
    // The production boot wiring also registers the service as the
    // endpoint-vanish observer (the surprise-removal trigger).
    tairix_kernel_core::callreg::install_vanish_observer(&VOLUME_SERVICE);
    // Arm the storage-group identity map (production: the root unlock
    // resolves the group by name and installs its gid) and install the
    // identity table the `fs_*` group resolution reads.
    LATE_STORAGE_GID.install(GroupId(STORAGE_GID));
    LATE_IDENTITY
        .install(identity_with_storage_member())
        .expect("install identity once");
    let window: &'static mut [u8] = Box::leak(vec![0u8; BLK_DATA_LEN].into_boxed_slice());
    let window_ptr = window.as_mut_ptr();
    let facility: &'static TestFacility = Box::leak(Box::new(TestFacility { window: window_ptr }));
    install_shared_mem_facility(facility);
    LATE_FILESYSTEM
        .install_vfs(Vfs::with_default_layout(UserId(0), GroupId(0)))
        .expect("install the default-layout mount table once");

    // --- The volume: a formatted FAT32 image carrying one file. ---
    let (image, expected_identity) = fat32_image(0x0D15_C001, b"runtime volume payload");
    let device_blocks = (image.data.len() / BLOCK_SIZE) as u64;

    // --- The served block service. ---
    let endpoint_id = 0xB1D0_5E17_0000_0001_u64;
    let endpoint = register_endpoint(endpoint_id, 0x70_0001);
    let device = StdArc::new(Mutex::new(ServedDevice::new(image)));
    let stop = StdArc::new(AtomicBool::new(false));
    let server = serve(
        StdArc::clone(&endpoint),
        window_ptr as usize,
        StdArc::clone(&device),
        StdArc::clone(&stop),
    );

    // The shared region the request names: created through the recorded
    // facility so the kernel hold resolves it.
    let (_va, region_id) = sharedreg::create(facility, TaskId(0x70_0002), 8).expect("region");

    // --- Attach. ---
    let attach = VolumeAttachRequest {
        endpoint: endpoint_id,
        window: region_id,
        first_lba: 0,
        blocks: device_blocks,
        fstype: VolumeFsType::Fat32,
        name: b"usb1",
    };
    VOLUME_SERVICE.attach(&attach).expect("attach");
    assert_eq!(
        VOLUME_FOREST.resolve(&expected_identity),
        Some(vec![String::from("Storage"), String::from("usb1")]),
        "the volume's durable root is published under the catalog"
    );
    // A second attach of the same volume under another name is refused
    // (duplicate identity) and unwinds cleanly.
    let dup = VolumeAttachRequest {
        name: b"usb2",
        ..attach
    };
    assert_eq!(VOLUME_SERVICE.attach(&dup), Err(Errno::AlreadyExists));

    // --- Use: the mounted volume serves reads through the production
    // `fs_*` service under the system principal. ---
    let mut buf = [0u8; 64];
    let n = FS_SERVICE
        .read(0, &NoCaps, "/Storage/usb1/hello.txt", 0, &mut buf)
        .expect("read through the mounted volume");
    assert_eq!(&buf[..n], b"runtime volume payload");

    // The mount snapshot reports the live volume as available and carries
    // its stable identity — the facts the unmount tooling resolves by.
    let record = mount_record(b"usb1");
    assert_eq!(record.availability(), MountAvailability::Available);
    assert_eq!(record.volume_id(), expected_identity);
    // ...and the storage medium the served device declared, which is how a
    // file manager picks a drive icon without guessing one.
    assert_eq!(
        record.medium(),
        Some(device.lock().unwrap().block.device_class()),
        "the mount reports the medium the block device declared"
    );

    assert_identity_mapped_access();

    // --- Detach. ---
    let detach = VolumeDetachRequest {
        volume_id: expected_identity,
        force: false,
    };
    VOLUME_SERVICE.detach(&detach).expect("detach");
    assert_eq!(
        VOLUME_FOREST.resolve(&expected_identity),
        None,
        "the withdrawn identity no longer resolves"
    );
    assert_eq!(
        FS_SERVICE
            .read(0, &NoCaps, "/Storage/usb1/hello.txt", 0, &mut buf)
            .err(),
        Some(Errno::NotFound),
        "the retracted mount fails closed"
    );
    // A repeated detach fails closed.
    assert_eq!(VOLUME_SERVICE.detach(&detach), Err(Errno::NotFound));

    stop.store(true, Ordering::Relaxed);
    server.join().expect("server thread");
    assert!(
        device.lock().unwrap().flushes >= 1,
        "detach commits the device cache"
    );

    dirty_surprise_removal_scenario(window_ptr, facility);
    clean_surprise_removal_scenario(window_ptr, facility);
    forced_unmount_of_a_healthy_volume_scenario(window_ptr, facility);
    verified_reinsert_replays_scenario(window_ptr, facility);
    mutated_reinsert_conflicts_scenario(window_ptr, facility);
    sibling_volumes_share_one_client_scenario(window_ptr, facility);
    unrecognised_medium_mounts_as_unknown_scenario(window_ptr, facility);
}

/// The per-scenario facts for [`attach_then_yank`]: the owning task, the
/// endpoint id, the image's minted serial, the shared-region creator, the
/// catalog name, and whether the journal is dirtied before the yank.
struct YankScenario {
    owner: u64,
    endpoint_id: u64,
    serial: u32,
    region_task: u64,
    name: &'static [u8],
    dirty: bool,
}

/// One served, attached FAT32 volume for the surprise-removal scenarios,
/// yanked by tearing its endpoint's owner down. Returns the volume's
/// identity after the server thread has ended.
fn attach_then_yank(
    window_ptr: *mut u8,
    facility: &'static TestFacility,
    scenario: &YankScenario,
) -> [u8; 16] {
    let endpoint = register_endpoint(scenario.endpoint_id, scenario.owner);
    let (image, identity) = fat32_image(scenario.serial, b"scenario payload");
    let blocks = (image.data.len() / BLOCK_SIZE) as u64;
    let device = StdArc::new(Mutex::new(ServedDevice::new(image)));
    let stop = StdArc::new(AtomicBool::new(false));
    let server = serve(
        StdArc::clone(&endpoint),
        window_ptr as usize,
        StdArc::clone(&device),
        StdArc::clone(&stop),
    );
    let (_va, region) =
        sharedreg::create(facility, TaskId(scenario.region_task), 8).expect("region");
    VOLUME_SERVICE
        .attach(&VolumeAttachRequest {
            endpoint: scenario.endpoint_id,
            window: region,
            first_lba: 0,
            blocks,
            fstype: VolumeFsType::Fat32,
            name: scenario.name,
        })
        .expect("attach scenario volume");
    if scenario.dirty {
        // Dirty the volume's journal through the production write path.
        let path = std::format!(
            "/Storage/{}/hello.txt",
            core::str::from_utf8(scenario.name).expect("ascii name")
        );
        FS_SERVICE
            .write(0, &NoCaps, &path, 0, false, b"D")
            .expect("write dirties the journal");
    }
    // Yank the stick: the serving driver dies and the kernel tears its
    // endpoint down, which drives the vanish observer synchronously.
    tairix_kernel_core::callreg::teardown_owned_by(scenario.owner, &ASPACES, &SINK);
    stop.store(true, Ordering::Relaxed);
    server.join().expect("scenario server thread");
    identity
}

/// Surprise removal with uncommitted writes (`plans/DEVICES.md` D4a): the
/// volume becomes unavailable-dirty — root visible, every operation
/// failing closed, plain detach refused.
fn dirty_surprise_removal_scenario(window_ptr: *mut u8, facility: &'static TestFacility) {
    let identity = attach_then_yank(
        window_ptr,
        facility,
        &YankScenario {
            owner: 0x70_1002,
            endpoint_id: 0xB1D0_5E17_0000_0002,
            serial: 0x0D15_C002,
            region_task: 0x70_0003,
            name: b"usb2",
            dirty: true,
        },
    );
    let mut buf = [0u8; 32];
    assert_eq!(
        VOLUME_FOREST.resolve(&identity),
        Some(vec![String::from("Storage"), String::from("usb2")]),
        "an unavailable-dirty volume's durable root stays visible"
    );
    assert_eq!(
        FS_SERVICE
            .read(0, &NoCaps, "/Storage/usb2/hello.txt", 0, &mut buf)
            .err(),
        Some(Errno::DeviceFault),
        "new I/O on an unavailable-dirty volume fails closed, cache included"
    );
    assert_eq!(
        VOLUME_SERVICE.detach(&VolumeDetachRequest {
            volume_id: identity,
            force: false,
        }),
        Err(Errno::DeviceFault),
        "a plain detach never discards the retained set"
    );
    // The mount snapshot says the volume is unavailable-dirty — never a
    // volume that looks healthy — and still names its identity.
    let record = FS_SERVICE
        .mount_snapshot()
        .into_iter()
        .find(|r| r.source_bytes() == b"usb2")
        .expect("the unavailable volume stays in the mount snapshot");
    assert_eq!(record.availability(), MountAvailability::UnavailableDirty);
    assert_eq!(record.volume_id(), identity);
    // The audited force-unmount is the deliberate exit: the retained set
    // is discarded, the root withdrawn, and the mount retracted.
    VOLUME_SERVICE
        .detach(&VolumeDetachRequest {
            volume_id: identity,
            force: true,
        })
        .expect("force-unmount discards and retracts the unavailable volume");
    assert_eq!(
        VOLUME_FOREST.resolve(&identity),
        None,
        "the force-unmounted identity no longer resolves"
    );
    assert_eq!(
        FS_SERVICE
            .read(0, &NoCaps, "/Storage/usb2/hello.txt", 0, &mut buf)
            .err(),
        Some(Errno::NotFound),
        "the force-retracted mount fails closed"
    );
    assert!(
        FS_SERVICE
            .mount_snapshot()
            .iter()
            .all(|r| r.source_bytes() != b"usb2"),
        "the force-unmounted volume leaves the mount snapshot"
    );
}

/// Surprise removal with nothing uncommitted: the volume is simply
/// retracted — no drama.
fn clean_surprise_removal_scenario(window_ptr: *mut u8, facility: &'static TestFacility) {
    let identity = attach_then_yank(
        window_ptr,
        facility,
        &YankScenario {
            owner: 0x70_1004,
            endpoint_id: 0xB1D0_5E17_0000_0003,
            serial: 0x0D15_C003,
            region_task: 0x70_0005,
            name: b"usb3",
            dirty: false,
        },
    );
    let mut buf = [0u8; 32];
    assert_eq!(
        VOLUME_FOREST.resolve(&identity),
        None,
        "a clean surprise removal retracts the durable root"
    );
    assert_eq!(
        FS_SERVICE
            .read(0, &NoCaps, "/Storage/usb3/hello.txt", 0, &mut buf)
            .err(),
        Some(Errno::NotFound),
        "the retracted mount fails closed"
    );
    assert_eq!(
        VOLUME_SERVICE.detach(&VolumeDetachRequest {
            volume_id: identity,
            force: false,
        }),
        Err(Errno::NotFound),
        "nothing remains to detach"
    );
}

/// One yank round for the re-insert scenarios: format a FAT32 image
/// carrying `hello.txt`, serve it, attach it under `name`, dirty the
/// journal through the production write path, and yank the serving
/// driver. Returns the volume identity, the image bytes as they stood
/// **before** the dirty write (the state a device that lost its cache
/// presents), and the image bytes at yank time.
struct YankedRound {
    identity: [u8; 16],
    pristine: Vec<u8>,
    at_yank: Vec<u8>,
}

fn dirty_yank_round(
    window_ptr: *mut u8,
    facility: &'static TestFacility,
    scenario: &YankScenario,
) -> YankedRound {
    assert!(
        scenario.dirty,
        "a re-insert round always dirties the journal"
    );
    let endpoint = register_endpoint(scenario.endpoint_id, scenario.owner);
    let (image, identity) = fat32_image(scenario.serial, b"scenario payload");
    let pristine = image.data.clone();
    let blocks = (image.data.len() / BLOCK_SIZE) as u64;
    let device = StdArc::new(Mutex::new(ServedDevice::new(image)));
    let stop = StdArc::new(AtomicBool::new(false));
    let server = serve(
        StdArc::clone(&endpoint),
        window_ptr as usize,
        StdArc::clone(&device),
        StdArc::clone(&stop),
    );
    let (_va, region) =
        sharedreg::create(facility, TaskId(scenario.region_task), 8).expect("region");
    VOLUME_SERVICE
        .attach(&VolumeAttachRequest {
            endpoint: scenario.endpoint_id,
            window: region,
            first_lba: 0,
            blocks,
            fstype: VolumeFsType::Fat32,
            name: scenario.name,
        })
        .expect("attach re-insert round volume");
    let path = std::format!(
        "/Storage/{}/hello.txt",
        core::str::from_utf8(scenario.name).expect("ascii name")
    );
    FS_SERVICE
        .write(0, &NoCaps, &path, 0, false, b"D")
        .expect("write dirties the journal");
    tairix_kernel_core::callreg::teardown_owned_by(scenario.owner, &ASPACES, &SINK);
    stop.store(true, Ordering::Relaxed);
    server.join().expect("re-insert round server thread");
    let at_yank = device.lock().unwrap().block.data.clone();
    YankedRound {
        identity,
        pristine,
        at_yank,
    }
}

/// The served re-inserted device and its round facts: the device (for
/// post-recovery image assertions), the stop/join pair the caller ends
/// the round with, and the attach outcome.
struct ReinsertRound {
    device: StdArc<Mutex<ServedDevice>>,
    stop: StdArc<AtomicBool>,
    server: thread::JoinHandle<()>,
    outcome: Result<(), Errno>,
}

/// Serve `image` as the re-inserted device and re-attach it.
fn reinsert(
    window_ptr: *mut u8,
    facility: &'static TestFacility,
    endpoint_id: u64,
    owner: u64,
    region_task: u64,
    name: &'static [u8],
    image: Vec<u8>,
) -> ReinsertRound {
    let endpoint = register_endpoint(endpoint_id, owner);
    let blocks = (image.len() / BLOCK_SIZE) as u64;
    let device = StdArc::new(Mutex::new(ServedDevice::new(RamBlock { data: image })));
    let stop = StdArc::new(AtomicBool::new(false));
    let server = serve(
        StdArc::clone(&endpoint),
        window_ptr as usize,
        StdArc::clone(&device),
        StdArc::clone(&stop),
    );
    let (_va, region) = sharedreg::create(facility, TaskId(region_task), 8).expect("region");
    let outcome = VOLUME_SERVICE.attach(&VolumeAttachRequest {
        endpoint: endpoint_id,
        window: region,
        first_lba: 0,
        blocks,
        fstype: VolumeFsType::Fat32,
        name,
    });
    ReinsertRound {
        device,
        stop,
        server,
        outcome,
    }
}

/// Verified re-insert (`plans/DEVICES.md` D4c): the device lost its
/// cached writes (the medium is the pre-write image), non-mutation is
/// proven from the evidence window, and the retained writes replay — the
/// dirty write is back on the medium and the volume returns to full
/// service under its original mount and published root.
fn verified_reinsert_replays_scenario(window_ptr: *mut u8, facility: &'static TestFacility) {
    let round = dirty_yank_round(
        window_ptr,
        facility,
        &YankScenario {
            owner: 0x70_1007,
            endpoint_id: 0xB1D0_5E17_0000_0005,
            serial: 0x0D15_C005,
            region_task: 0x70_0007,
            name: b"usb5",
            dirty: true,
        },
    );
    // Re-insert the pre-write image: the device accepted the writes into
    // its volatile cache and lost them with the power.
    let inserted = reinsert(
        window_ptr,
        facility,
        0xB1D0_5E17_0000_0006,
        0x70_1008,
        0x70_0008,
        b"usb5",
        round.pristine,
    );
    inserted.outcome.expect("the re-insert recovers the volume");

    // The volume is back in full service: available, its root published,
    // and the replayed write visible through the production service.
    let record = FS_SERVICE
        .mount_snapshot()
        .into_iter()
        .find(|r| r.source_bytes() == b"usb5")
        .expect("the recovered volume appears in the mount snapshot");
    assert_eq!(record.availability(), MountAvailability::Available);
    assert_eq!(record.volume_id(), round.identity);
    assert_eq!(
        VOLUME_FOREST.resolve(&round.identity),
        Some(vec![String::from("Storage"), String::from("usb5")]),
        "the durable root survived the whole unplug/re-insert cycle"
    );
    let mut buf = [0u8; 32];
    let n = FS_SERVICE
        .read(0, &NoCaps, "/Storage/usb5/hello.txt", 0, &mut buf)
        .expect("the recovered volume serves reads");
    assert_eq!(
        &buf[..1],
        b"D",
        "the retained dirty write was replayed onto the medium"
    );
    assert!(n >= 1);
    // The recovered volume is writable again.
    FS_SERVICE
        .write(0, &NoCaps, "/Storage/usb5/hello.txt", 0, false, b"E")
        .expect("the recovered volume accepts writes");

    // A clean detach ends the round: the replay left nothing uncommitted
    // that a flush cannot commit.
    VOLUME_SERVICE
        .detach(&VolumeDetachRequest {
            volume_id: round.identity,
            force: false,
        })
        .expect("the recovered volume detaches cleanly");
    inserted.stop.store(true, Ordering::Relaxed);
    inserted.server.join().expect("reinsert server thread");
    assert!(
        inserted.device.lock().unwrap().flushes >= 1,
        "the replay committed the device cache"
    );
}

/// Mutated re-insert (`plans/DEVICES.md` D4c): the medium's evidence
/// window changed while unplugged (a foreign mount touched `FSInfo`), so
/// replay is refused — the volume returns read-only in the
/// recovery-conflict state with the retained set kept; a plain detach
/// stays refused and the audited force-unmount is the exit.
fn mutated_reinsert_conflicts_scenario(window_ptr: *mut u8, facility: &'static TestFacility) {
    let round = dirty_yank_round(
        window_ptr,
        facility,
        &YankScenario {
            owner: 0x70_1009,
            endpoint_id: 0xB1D0_5E17_0000_0007,
            serial: 0x0D15_C006,
            region_task: 0x70_0009,
            name: b"usb6",
            dirty: true,
        },
    );
    // A foreign mount updated the FSInfo free-cluster hint (sector 1,
    // offset 488) — inside the evidence window, outside the identity.
    let mut mutated = round.at_yank;
    mutated[512 + 488..512 + 492].copy_from_slice(&1234u32.to_le_bytes());
    let inserted = reinsert(
        window_ptr,
        facility,
        0xB1D0_5E17_0000_0008,
        0x70_100A,
        0x70_000A,
        b"usb6",
        mutated,
    );
    inserted
        .outcome
        .expect("the conflicted re-insert still mounts (read-only)");

    // The volume is visible but conflicted: reads serve, writes are
    // refused by the read-only mount, and the snapshot says why.
    let record = FS_SERVICE
        .mount_snapshot()
        .into_iter()
        .find(|r| r.source_bytes() == b"usb6")
        .expect("the conflicted volume appears in the mount snapshot");
    assert_eq!(record.availability(), MountAvailability::RecoveryConflict);
    let mut buf = [0u8; 32];
    FS_SERVICE
        .read(0, &NoCaps, "/Storage/usb6/hello.txt", 0, &mut buf)
        .expect("the conflicted volume serves reads");
    assert_eq!(
        FS_SERVICE
            .write(0, &NoCaps, "/Storage/usb6/hello.txt", 0, false, b"X")
            .err(),
        Some(Errno::PermissionDenied),
        "the conflicted volume is read-only until acknowledged"
    );
    // The retained set is still held: a plain detach would discard it
    // silently, so it is refused; the audited force is the exit.
    assert_eq!(
        VOLUME_SERVICE.detach(&VolumeDetachRequest {
            volume_id: round.identity,
            force: false,
        }),
        Err(Errno::NotEmpty),
        "a plain detach never discards the conflicted volume's retained set"
    );
    VOLUME_SERVICE
        .detach(&VolumeDetachRequest {
            volume_id: round.identity,
            force: true,
        })
        .expect("force-unmount discards and retracts the conflicted volume");
    assert_eq!(VOLUME_FOREST.resolve(&round.identity), None);
    inserted.stop.store(true, Ordering::Relaxed);
    inserted
        .server
        .join()
        .expect("conflict reinsert server thread");
}

/// A force detach of a **healthy** volume commits cleanly — the flush
/// still runs and nothing is discarded; force only changes the outcome
/// when a clean commit is impossible (`plans/DEVICES.md` D4b).
fn forced_unmount_of_a_healthy_volume_scenario(
    window_ptr: *mut u8,
    facility: &'static TestFacility,
) {
    let endpoint_id = 0xB1D0_5E17_0000_0004_u64;
    let endpoint = register_endpoint(endpoint_id, 0x70_1006);
    let (image, identity) = fat32_image(0x0D15_C004, b"healthy force payload");
    let blocks = (image.data.len() / BLOCK_SIZE) as u64;
    let device = StdArc::new(Mutex::new(ServedDevice::new(image)));
    let stop = StdArc::new(AtomicBool::new(false));
    let server = serve(
        StdArc::clone(&endpoint),
        window_ptr as usize,
        StdArc::clone(&device),
        StdArc::clone(&stop),
    );
    let (_va, region) = sharedreg::create(facility, TaskId(0x70_0006), 8).expect("region");
    VOLUME_SERVICE
        .attach(&VolumeAttachRequest {
            endpoint: endpoint_id,
            window: region,
            first_lba: 0,
            blocks,
            fstype: VolumeFsType::Fat32,
            name: b"usb4",
        })
        .expect("attach healthy volume");
    // Dirty the journal so a discard would be observable, then force.
    FS_SERVICE
        .write(0, &NoCaps, "/Storage/usb4/hello.txt", 0, false, b"F")
        .expect("write dirties the journal");
    VOLUME_SERVICE
        .detach(&VolumeDetachRequest {
            volume_id: identity,
            force: true,
        })
        .expect("a force detach of a healthy volume succeeds");
    stop.store(true, Ordering::Relaxed);
    server.join().expect("healthy-force server thread");
    assert!(
        device.lock().unwrap().flushes >= 1,
        "a force detach of a healthy volume still commits the device cache"
    );
    assert_eq!(
        VOLUME_FOREST.resolve(&identity),
        None,
        "the force-detached identity no longer resolves"
    );
}

/// Assert the file on the mounted volume at `path` holds exactly `expected`.
fn assert_volume_reads(path: &str, expected: &[u8]) {
    let mut buf = [0u8; 64];
    let n = FS_SERVICE
        .read(0, &NoCaps, path, 0, &mut buf)
        .unwrap_or_else(|err| std::panic!("read {path}: {err:?}"));
    assert_eq!(&buf[..n], expected, "{path}");
}

/// Both mounted siblings of one disk serve, and rewrite, their own extent's
/// bytes: the staging window they share never hands one volume the other's
/// data (`plans/FIX-IO.md` invariant 9).
fn assert_each_sibling_serves_its_own_extent() {
    assert_volume_reads("/Storage/part1/hello.txt", b"first partition payload");
    assert_volume_reads("/Storage/part2/hello.txt", b"second partition payload");

    // Interleaved writes through both mounts stay on their own extents.
    FS_SERVICE
        .write(0, &NoCaps, "/Storage/part1/hello.txt", 0, false, b"1")
        .expect("write the first partition");
    FS_SERVICE
        .write(0, &NoCaps, "/Storage/part2/hello.txt", 0, false, b"2")
        .expect("write the second partition");
    assert_volume_reads("/Storage/part1/hello.txt", b"1irst partition payload");
    assert_volume_reads("/Storage/part2/hello.txt", b"2econd partition payload");
}

/// Two volumes on **one** disk share one block client, and that client is
/// released when the last of them goes (`plans/FIX-IO.md`).
///
/// A disk's volumes are served over one endpoint and one shared data
/// window, and the blkio protocol stages every transfer's bytes in that
/// window. A second client on the same device would therefore be a second
/// concurrent user of the one staging buffer: two transfers in flight
/// would overwrite each other's bytes and hand a reader another extent's
/// data — silent corruption, not a fault. So the service connects the
/// device once and serialises every volume's operations onto it.
///
/// Connecting is the only thing that queries the geometry, so the served
/// device's geometry count *is* the number of clients that ever existed for
/// it: this scenario pins that count rather than racing two mounts and
/// hoping to observe a corruption.
fn sibling_volumes_share_one_client_scenario(window_ptr: *mut u8, facility: &'static TestFacility) {
    // One disk carrying two distinct FAT32 volumes back to back — the
    // ordinary partitioned stick.
    let (first, first_identity) = fat32_image(0x0D15_C007, b"first partition payload");
    let (second, second_identity) = fat32_image(0x0D15_C008, b"second partition payload");
    let extent_blocks = (first.data.len() / BLOCK_SIZE) as u64;
    let mut data = first.data;
    data.extend_from_slice(&second.data);
    let disk = RamBlock { data };

    let endpoint_id = 0xB1D0_5E17_0000_0007_u64;
    let endpoint = register_endpoint(endpoint_id, 0x70_1007);
    let device = StdArc::new(Mutex::new(ServedDevice::new(disk)));
    let stop = StdArc::new(AtomicBool::new(false));
    let server = serve(
        StdArc::clone(&endpoint),
        window_ptr as usize,
        StdArc::clone(&device),
        StdArc::clone(&stop),
    );
    let (_va, region) = sharedreg::create(facility, TaskId(0x70_0008), 8).expect("region");

    let attach_extent = |name: &'static [u8], first_lba: u64| VolumeAttachRequest {
        endpoint: endpoint_id,
        window: region,
        first_lba,
        blocks: extent_blocks,
        fstype: VolumeFsType::Fat32,
        name,
    };
    VOLUME_SERVICE
        .attach(&attach_extent(b"part1", 0))
        .expect("attach the first partition");
    VOLUME_SERVICE
        .attach(&attach_extent(b"part2", extent_blocks))
        .expect("attach the second partition");

    assert_eq!(
        device.lock().unwrap().geometries,
        1,
        "both volumes on one disk drive one shared client, not one each"
    );

    assert_each_sibling_serves_its_own_extent();

    // Detaching commits through the same shared client: the device-cache
    // flush no longer opens a client of its own behind the sibling's back.
    VOLUME_SERVICE
        .detach(&VolumeDetachRequest {
            volume_id: first_identity,
            force: false,
        })
        .expect("detach the first partition");
    assert_eq!(
        device.lock().unwrap().geometries,
        1,
        "the detach-time device flush reuses the shared client"
    );
    // The surviving sibling still serves: retiring one volume never closes
    // a device another volume is still mounted on.
    assert_volume_reads("/Storage/part2/hello.txt", b"2econd partition payload");
    assert_eq!(
        device.lock().unwrap().geometries,
        1,
        "the surviving volume kept the one client, it did not reconnect"
    );

    VOLUME_SERVICE
        .detach(&VolumeDetachRequest {
            volume_id: second_identity,
            force: false,
        })
        .expect("detach the second partition");

    // The disk's last volume went, so the shared client was released: a
    // fresh attach connects anew rather than reusing a retired one.
    VOLUME_SERVICE
        .attach(&attach_extent(b"part1", 0))
        .expect("re-attach the first partition");
    assert_eq!(
        device.lock().unwrap().geometries,
        2,
        "the released client is re-connected, not resurrected"
    );
    VOLUME_SERVICE
        .detach(&VolumeDetachRequest {
            volume_id: first_identity,
            force: false,
        })
        .expect("detach the re-attached partition");

    stop.store(true, Ordering::Relaxed);
    server.join().expect("sibling scenario server thread");
}

/// A device whose geometry reply names a storage medium this ABI does not
/// define mounts as *unknown*, and still serves.
///
/// The mount record is what a file manager picks a drive icon from, so
/// answering with a class nobody declared would put a specific medium on
/// screen on the strength of a word the client could not read. Unknown is
/// the honest answer, and it resolves to the generic drive icon. The medium
/// is advisory, so failing to read it never costs the volume its mount.
fn unrecognised_medium_mounts_as_unknown_scenario(
    window_ptr: *mut u8,
    facility: &'static TestFacility,
) {
    let (image, identity) = fat32_image(0x0D15_C009, b"unclassified payload");
    let blocks = (image.data.len() / BLOCK_SIZE) as u64;
    let endpoint_id = 0xB1D0_5E17_0000_0009_u64;
    let endpoint = register_endpoint(endpoint_id, 0x70_1009);
    let device = StdArc::new(Mutex::new(ServedDevice::unclassified(image)));
    let stop = StdArc::new(AtomicBool::new(false));
    let server = serve(
        StdArc::clone(&endpoint),
        window_ptr as usize,
        StdArc::clone(&device),
        StdArc::clone(&stop),
    );
    let (_va, region) = sharedreg::create(facility, TaskId(0x70_0009), 8).expect("region");

    VOLUME_SERVICE
        .attach(&VolumeAttachRequest {
            endpoint: endpoint_id,
            window: region,
            first_lba: 0,
            blocks,
            fstype: VolumeFsType::Fat32,
            name: b"odd1",
        })
        .expect("attach the volume whose class word is unreadable");

    assert_eq!(
        mount_record(b"odd1").medium(),
        None,
        "an unreadable class word is reported unknown, never guessed"
    );
    assert_volume_reads("/Storage/odd1/hello.txt", b"unclassified payload");

    VOLUME_SERVICE
        .detach(&VolumeDetachRequest {
            volume_id: identity,
            force: false,
        })
        .expect("detach the volume whose class word is unreadable");

    stop.store(true, Ordering::Relaxed);
    server
        .join()
        .expect("unrecognised-medium scenario server thread");
}
