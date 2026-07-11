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

use rustos_abi::blkio::{
    encode_error_completion, BlkCompletion, BlkOp, BlkRequest, BLK_COMPLETION_LEN, BLK_DATA_LEN,
    BLK_REQUEST_LEN,
};
use rustos_abi::driver::block::{Block, BlockGeometry};
use rustos_abi::driver::filesystem::{FilesystemRead, FilesystemWrite, NodeKind};
use rustos_abi::volume::{VolumeAttachRequest, VolumeDetachRequest, VolumeFsType};
use rustos_abi::{CapabilityId, CapabilityQuery, DriverError, Errno};
use rustos_caps::CapabilitySet;
use rustos_drv_fs_fat32::Fat32;
use rustos_kernel_core::devres::{install_shared_mem_facility, SharedMemFacility};
use rustos_kernel_core::fs::FilesystemService;
use rustos_kernel_core::{sharedreg, Vfs};
use rustos_kernel_ipc::{CallEndpoint, CallEndpointLimits, EndpointId, RecvCall};
use rustos_kernel_mem::{FreeMemorySource, MemoryPressure};
use rustos_kernel_sec::captable::TaskCapabilities;
use rustos_kernel_sec::{GroupId, GroupRecord, IdentityTableBuilder, TaskId, UserId, UserRecord};
use rustos_log::Sink;

use crate::root_mount::LATE_IDENTITY;
use crate::system_mount::{FS_SERVICE, LATE_FILESYSTEM, VOLUME_FOREST};
use crate::volume_policy::LATE_STORAGE_GID;
use crate::volume_service::{RuntimeVolumeService, VOLUME_SERVICE};
use rustos_kernel_core::VolumeService as _;

/// A throwaway audit sink.
struct NullSink;
impl Sink for NullSink {
    fn write_event(&self, _event: &rustos_log::Event<'_>) {}
}
static SINK: NullSink = NullSink;

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
    fn alloc_region(&self, _pages: u64) -> Result<(u64, u32), Errno> {
        Ok((0x4000_0000, 0))
    }
    fn map_region(&self, phys_base: u64, _pages: u64) -> Result<u64, Errno> {
        Ok(0x9000_0000 + phys_base)
    }
    fn unmap_region(&self, _base: u64, _len: usize) -> Result<(), Errno> {
        Ok(())
    }
    fn free_region(&self, _phys_base: u64, _order: u32, _pages: u64) {}
    fn kernel_window(&self, _phys_base: u64, _len: usize) -> Option<core::ptr::NonNull<u8>> {
        core::ptr::NonNull::new(self.window)
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
            .filter(|&end| end <= self.data.len() && !buf.is_empty() && buf.len() % BLOCK_SIZE == 0)
            .ok_or(DriverError::LengthOutOfRange)?;
        buf.copy_from_slice(&self.data[start..end]);
        Ok(())
    }

    fn write_blocks(&mut self, lba: u64, buf: &[u8]) -> Result<(), DriverError> {
        let start = usize::try_from(lba).map_err(|_| DriverError::LengthOutOfRange)? * BLOCK_SIZE;
        let end = start
            .checked_add(buf.len())
            .filter(|&end| end <= self.data.len() && !buf.is_empty() && buf.len() % BLOCK_SIZE == 0)
            .ok_or(DriverError::LengthOutOfRange)?;
        self.data[start..end].copy_from_slice(buf);
        Ok(())
    }
}

/// The served device: image bytes plus a flush counter.
struct ServedDevice {
    block: RamBlock,
    flushes: usize,
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
                            let geometry = device.block.geometry().unwrap();
                            BlkCompletion {
                                block_size: geometry.block_size,
                                block_count: geometry.block_count,
                                flags: 0,
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
    rustos_kernel_core::callreg::register(StdArc::clone(&endpoint), &SINK)
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
        service.detach(&VolumeDetachRequest { volume_id: [7; 16] }),
        Err(Errno::NotImplemented)
    );
}

/// The identity table the lifecycle test installs: the system principal,
/// a member of the storage group, and a non-member.
fn identity_with_storage_member() -> rustos_kernel_sec::IdentityTable {
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
    rustos_kernel_core::callreg::install_vanish_observer(&VOLUME_SERVICE);
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
    let device = StdArc::new(Mutex::new(ServedDevice {
        block: image,
        flushes: 0,
    }));
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

    assert_identity_mapped_access();

    // --- Detach. ---
    let detach = VolumeDetachRequest {
        volume_id: expected_identity,
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
    let device = StdArc::new(Mutex::new(ServedDevice {
        block: image,
        flushes: 0,
    }));
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
    rustos_kernel_core::callreg::teardown_owned_by(scenario.owner, &SINK);
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
            volume_id: identity
        }),
        Err(Errno::DeviceFault),
        "a plain detach never discards the retained set"
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
            volume_id: identity
        }),
        Err(Errno::NotFound),
        "nothing remains to detach"
    );
}
