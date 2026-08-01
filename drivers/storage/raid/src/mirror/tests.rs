//! Host tests for the RAID1 mirror over a fault-injecting [`Block`] double.

use super::{ArrayHealth, MemberRole, MemberState, MirrorArray, MirrorError, MirrorMember};
use crate::superblock::ArrayProgress;
use crate::SlotDisposition;
use core::cell::Cell;
use tairix_abi::blkio::BlkDeviceClass;
use tairix_abi::driver::block::{Block, BlockGeometry, DeviceHealth, HealthSnapshot};
use tairix_abi::driver::{BufferClass, DriverError};

const BS: u32 = 512;
const NBLK: u64 = 8;
// The device's byte capacity: block size times block count (`NBLK`), written
// with a literal count so the constant needs no `u64`->`usize` cast.
const CAP: usize = BS as usize * 8;

/// An in-memory block device with programmable, post-assembly-injectable
/// faults (via [`Cell`], so a test can flip a fault through a shared borrow
/// while the array owns the member).
struct FaultBlock {
    store: [u8; CAP],
    geo: Cell<BlockGeometry>,
    present: Cell<bool>,
    read_fault: Cell<Option<DriverError>>,
    write_fault: Cell<Option<DriverError>>,
    flush_fault: Cell<bool>,
    health: Cell<DeviceHealth>,
    health_fault: Cell<bool>,
    class: Cell<BlkDeviceClass>,
    reads: Cell<u32>,
    writes: Cell<u32>,
}

impl FaultBlock {
    fn new(fill: u8) -> Self {
        Self {
            store: [fill; CAP],
            geo: Cell::new(BlockGeometry {
                block_size: BS,
                block_count: NBLK,
            }),
            present: Cell::new(true),
            read_fault: Cell::new(None),
            write_fault: Cell::new(None),
            flush_fault: Cell::new(false),
            health: Cell::new(DeviceHealth::Unavailable),
            health_fault: Cell::new(false),
            class: Cell::new(BlkDeviceClass::SolidState),
            reads: Cell::new(0),
            writes: Cell::new(0),
        }
    }

    /// A device declaring `class` as its performance/behaviour envelope.
    fn with_class(self, class: BlkDeviceClass) -> Self {
        self.class.set(class);
        self
    }

    /// A device reporting `media_errors` integrity faults through its
    /// health telemetry.
    fn with_media_errors(self, media_errors: u64) -> Self {
        self.health.set(DeviceHealth::Available(HealthSnapshot {
            power_on_hours: 0,
            unsafe_shutdowns: 0,
            media_errors,
            reallocated_sectors: 0,
            pending_sectors: 0,
            uncorrectable_sectors: 0,
            crc_errors: 0,
            percentage_used: 0,
            available_spare: 100,
            temperature_kelvin: 300,
            critical_warning: false,
        }));
        self
    }

    fn absent() -> Self {
        let d = Self::new(0);
        d.present.set(false);
        d
    }

    fn with_geometry(self, geo: BlockGeometry) -> Self {
        self.geo.set(geo);
        self
    }

    fn span(lba: u64, len: usize) -> Result<(usize, usize), DriverError> {
        let bs = BS as usize;
        if len == 0 || !len.is_multiple_of(bs) {
            return Err(DriverError::BufferTooSmall);
        }
        let start = usize::try_from(lba)
            .ok()
            .and_then(|l| l.checked_mul(bs))
            .ok_or(DriverError::LengthOutOfRange)?;
        let end = start
            .checked_add(len)
            .ok_or(DriverError::LengthOutOfRange)?;
        if end > CAP {
            return Err(DriverError::LengthOutOfRange);
        }
        Ok((start, end))
    }
}

impl Block for FaultBlock {
    fn geometry(&self) -> Result<BlockGeometry, DriverError> {
        if self.present.get() {
            Ok(self.geo.get())
        } else {
            Err(DriverError::DeviceOffline)
        }
    }

    fn read_blocks(&mut self, lba: u64, buf: &mut [u8]) -> Result<(), DriverError> {
        self.reads.set(self.reads.get() + 1);
        if let Some(e) = self.read_fault.get() {
            return Err(e);
        }
        let (start, end) = Self::span(lba, buf.len())?;
        buf.copy_from_slice(&self.store[start..end]);
        Ok(())
    }

    fn write_blocks(&mut self, lba: u64, buf: &[u8]) -> Result<(), DriverError> {
        self.writes.set(self.writes.get() + 1);
        if let Some(e) = self.write_fault.get() {
            return Err(e);
        }
        let (start, end) = Self::span(lba, buf.len())?;
        self.store[start..end].copy_from_slice(buf);
        // A successful write heals a previously bad sector (the device
        // reallocates it), so a repaired copy reads back correctly.
        self.read_fault.set(None);
        Ok(())
    }

    fn flush(&mut self) -> Result<(), DriverError> {
        if self.flush_fault.get() {
            Err(DriverError::DeviceFault)
        } else {
            Ok(())
        }
    }

    fn device_health(&self) -> Result<DeviceHealth, DriverError> {
        if self.health_fault.get() {
            Err(DriverError::DeviceFault)
        } else {
            Ok(self.health.get())
        }
    }

    fn device_class(&self) -> BlkDeviceClass {
        self.class.get()
    }
}

/// Convenience: a block-sized buffer filled with `v`.
fn block(v: u8) -> [u8; BS as usize] {
    [v; BS as usize]
}

/// Borrow the device behind a *present* member slot. The tests that call it
/// only ever inspect slots they know hold a device, so an absent slot is a
/// test bug and panics here.
fn dev<'a>(array: &'a MirrorArray<'_, FaultBlock>, index: usize) -> &'a FaultBlock {
    array
        .member(index)
        .expect("member index in range")
        .device()
        .expect("present member slot holds a device")
}

#[test]
fn assemble_two_healthy_members_is_optimal() {
    let mut members = [
        MirrorMember::new(FaultBlock::new(0)),
        MirrorMember::new(FaultBlock::new(0)),
    ];
    let array = MirrorArray::assemble(&mut members).expect("assembles");
    assert_eq!(array.health(), ArrayHealth::Optimal);
    assert_eq!(array.array_geometry().block_count, NBLK);
    assert_eq!(array.member_count(), 2);
}

#[test]
fn assemble_rejects_an_empty_member_set() {
    let mut members: [MirrorMember<FaultBlock>; 0] = [];
    assert_eq!(
        MirrorArray::assemble(&mut members).err(),
        Some(MirrorError::NoMembers)
    );
}

#[test]
fn assemble_rejects_geometry_mismatch() {
    let other = BlockGeometry {
        block_size: BS,
        block_count: NBLK + 1,
    };
    let mut members = [
        MirrorMember::new(FaultBlock::new(0)),
        MirrorMember::new(FaultBlock::new(0).with_geometry(other)),
    ];
    assert_eq!(
        MirrorArray::assemble(&mut members).err(),
        Some(MirrorError::GeometryMismatch)
    );
}

#[test]
fn assemble_with_one_absent_member_comes_up_degraded() {
    let mut members = [
        MirrorMember::new(FaultBlock::new(0)),
        MirrorMember::new(FaultBlock::absent()),
    ];
    let array = MirrorArray::assemble(&mut members).expect("assembles degraded");
    assert_eq!(array.member_state(0), Some(MemberState::InSync));
    assert_eq!(array.member_state(1), Some(MemberState::Faulted));
    assert_eq!(array.health(), ArrayHealth::Degraded);
}

#[test]
fn assemble_with_no_usable_member_fails_closed() {
    let mut members = [
        MirrorMember::new(FaultBlock::absent()),
        MirrorMember::new(FaultBlock::absent()),
    ];
    assert_eq!(
        MirrorArray::assemble(&mut members).err(),
        Some(MirrorError::NoUsableMember)
    );
}

#[test]
fn a_write_fans_out_to_every_copy() {
    let mut members = [
        MirrorMember::new(FaultBlock::new(0)),
        MirrorMember::new(FaultBlock::new(0)),
    ];
    let mut array = MirrorArray::assemble(&mut members).expect("assembles");
    let data = block(0x33);
    array.write_blocks(1, &data).expect("write ok");
    assert_eq!(&dev(&array, 0).store[512..1024], &data[..]);
    assert_eq!(&dev(&array, 1).store[512..1024], &data[..]);
}

#[test]
fn a_read_is_served_from_the_first_copy_without_touching_the_rest() {
    let mut members = [
        MirrorMember::new(FaultBlock::new(0)),
        MirrorMember::new(FaultBlock::new(0)),
    ];
    let mut array = MirrorArray::assemble(&mut members).expect("assembles");
    let mut buf = block(0);
    array.read_blocks(0, &mut buf).expect("read ok");
    assert_eq!(dev(&array, 0).reads.get(), 1);
    assert_eq!(dev(&array, 1).reads.get(), 0);
}

#[test]
fn a_bad_sector_is_recovered_from_a_copy_and_the_bad_copy_repaired() {
    let mut members = [
        MirrorMember::new(FaultBlock::new(0)),
        MirrorMember::new(FaultBlock::new(0)),
    ];
    let mut array = MirrorArray::assemble(&mut members).expect("assembles");
    let data = block(0x5A);
    array.write_blocks(2, &data).expect("seed both copies");
    // The first copy develops a permanent bad sector at this block.
    dev(&array, 0)
        .read_fault
        .set(Some(DriverError::MediumError));

    let mut buf = block(0);
    array
        .read_blocks(2, &mut buf)
        .expect("recovers from the good copy");
    assert_eq!(buf, data, "the good copy's data is returned");
    // The bad copy was repaired in place (sector reallocated) and stays a
    // member — a single bad sector never kills a mirror leg.
    assert_eq!(array.member_state(0), Some(MemberState::InSync));
    assert_eq!(dev(&array, 0).read_fault.get(), None);
    assert_eq!(array.health(), ArrayHealth::Optimal);
}

#[test]
fn a_whole_device_read_fault_drops_the_copy_and_degrades_the_array() {
    let mut members = [
        MirrorMember::new(FaultBlock::new(0)),
        MirrorMember::new(FaultBlock::new(0)),
    ];
    let mut array = MirrorArray::assemble(&mut members).expect("assembles");
    dev(&array, 0)
        .read_fault
        .set(Some(DriverError::DeviceOffline));
    let mut buf = block(0);
    array
        .read_blocks(0, &mut buf)
        .expect("served from the surviving copy");
    assert_eq!(array.member_state(0), Some(MemberState::Faulted));
    assert_eq!(array.member_state(1), Some(MemberState::InSync));
    assert_eq!(array.health(), ArrayHealth::Degraded);
}

#[test]
fn a_read_with_no_good_copy_fails_closed_without_faulting_medium_copies() {
    let mut members = [
        MirrorMember::new(FaultBlock::new(0)),
        MirrorMember::new(FaultBlock::new(0)),
    ];
    let mut array = MirrorArray::assemble(&mut members).expect("assembles");
    for i in 0..2 {
        dev(&array, i)
            .read_fault
            .set(Some(DriverError::MediumError));
    }
    let mut buf = block(0);
    assert_eq!(
        array.read_blocks(0, &mut buf).unwrap_err(),
        DriverError::MediumError
    );
    // A per-block error never kills a copy: the data is gone, the devices
    // are not.
    assert_eq!(array.member_state(0), Some(MemberState::InSync));
    assert_eq!(array.member_state(1), Some(MemberState::InSync));
}

#[test]
fn a_write_error_drops_the_copy_but_the_write_still_succeeds() {
    let mut members = [
        MirrorMember::new(FaultBlock::new(0)),
        MirrorMember::new(FaultBlock::new(0)),
    ];
    let mut array = MirrorArray::assemble(&mut members).expect("assembles");
    dev(&array, 0)
        .write_fault
        .set(Some(DriverError::DeviceFault));
    let data = block(0x77);
    array
        .write_blocks(0, &data)
        .expect("still durable on the surviving copy");
    assert_eq!(array.member_state(0), Some(MemberState::Faulted));
    assert_eq!(array.member_state(1), Some(MemberState::InSync));
    assert_eq!(array.health(), ArrayHealth::Degraded);
    assert_eq!(&dev(&array, 1).store[0..512], &data[..]);
}

#[test]
fn a_write_no_copy_accepts_fails_closed() {
    let mut members = [
        MirrorMember::new(FaultBlock::new(0)),
        MirrorMember::new(FaultBlock::new(0)),
    ];
    let mut array = MirrorArray::assemble(&mut members).expect("assembles");
    for i in 0..2 {
        dev(&array, i)
            .write_fault
            .set(Some(DriverError::DeviceFault));
    }
    assert!(array.write_blocks(0, &block(1)).is_err());
    assert_eq!(array.health(), ArrayHealth::Failed);
    // A failed array fails a subsequent read closed rather than serving.
    let mut buf = block(0);
    assert_eq!(
        array.read_blocks(0, &mut buf).unwrap_err(),
        DriverError::DeviceOffline
    );
}

#[test]
fn a_flush_commits_every_copy_and_drops_one_that_cannot() {
    let mut members = [
        MirrorMember::new(FaultBlock::new(0)),
        MirrorMember::new(FaultBlock::new(0)),
    ];
    let mut array = MirrorArray::assemble(&mut members).expect("assembles");
    dev(&array, 0).flush_fault.set(true);
    array.flush().expect("still durable on the surviving copy");
    assert_eq!(array.member_state(0), Some(MemberState::Faulted));
    dev(&array, 1).flush_fault.set(true);
    assert!(array.flush().is_err());
}

#[test]
fn a_malformed_or_out_of_range_request_is_a_request_error() {
    let mut members = [
        MirrorMember::new(FaultBlock::new(0)),
        MirrorMember::new(FaultBlock::new(0)),
    ];
    let mut array = MirrorArray::assemble(&mut members).expect("assembles");
    let mut short = [0u8; 100];
    assert_eq!(
        array.read_blocks(0, &mut short).unwrap_err(),
        DriverError::BufferTooSmall
    );
    let mut past = [0u8; CAP + BS as usize];
    assert_eq!(
        array.read_blocks(0, &mut past).unwrap_err(),
        DriverError::LengthOutOfRange
    );
    // A request error never faults a healthy copy.
    assert_eq!(array.health(), ArrayHealth::Optimal);
}

/// The per-block data pattern the fill helpers write, distinct per LBA.
fn pat(lba: u64) -> u8 {
    0xA0 + u8::try_from(lba).expect("small array")
}

/// Write a distinct pattern to every block of the array.
fn fill<B: Block>(array: &mut MirrorArray<'_, B>) {
    for lba in 0..NBLK {
        array
            .write_blocks(lba, &block(pat(lba)))
            .expect("seed the array");
    }
}

#[test]
fn a_returned_member_is_rebuilt_with_current_data_including_degraded_writes() {
    let mut members = [
        MirrorMember::new(FaultBlock::new(0)),
        MirrorMember::new(FaultBlock::new(0)),
    ];
    let mut array = MirrorArray::assemble(&mut members).expect("assembles");
    fill(&mut array);

    // The second copy faults on a write, so a degraded-window write reaches
    // only the survivor.
    dev(&array, 1)
        .write_fault
        .set(Some(DriverError::DeviceFault));
    array
        .write_blocks(3, &block(0xB0))
        .expect("degraded write lands on the survivor");
    assert_eq!(array.member_state(1), Some(MemberState::Faulted));
    assert_eq!(array.health(), ArrayHealth::Degraded);

    // The copy recovers; clear its fault and re-add it.
    dev(&array, 1).write_fault.set(None);
    array.readd_member(1).expect("re-add begins the rebuild");
    assert_eq!(array.member_state(1), Some(MemberState::Resyncing));
    assert_eq!(array.health(), ArrayHealth::Recovering);

    // Bounded, interruptible rebuild — one block per step here.
    let mut scratch = block(0);
    let mut steps = 0u32;
    while array.needs_resync() {
        array.resync_step(&mut scratch).expect("resync step");
        steps += 1;
        assert!(steps <= 100, "the rebuild terminates");
    }
    assert_eq!(array.member_state(1), Some(MemberState::InSync));
    assert_eq!(array.health(), ArrayHealth::Optimal);

    // Prove the rebuilt copy holds current data: fault the source and read
    // every block back from the rebuilt copy.
    dev(&array, 0)
        .read_fault
        .set(Some(DriverError::DeviceOffline));
    for lba in 0..NBLK {
        let mut buf = block(0);
        array
            .read_blocks(lba, &mut buf)
            .expect("served from the rebuilt copy");
        let want = if lba == 3 { 0xB0 } else { pat(lba) };
        assert_eq!(buf, block(want), "block {lba} rebuilt with current data");
    }
}

#[test]
fn the_rebuild_is_incremental_and_advances_a_cursor() {
    let mut members = [
        MirrorMember::new(FaultBlock::new(0)),
        MirrorMember::new(FaultBlock::absent()),
    ];
    let mut array = MirrorArray::assemble(&mut members).expect("assembles degraded");
    fill(&mut array);
    // The absent copy returns and is re-added.
    dev(&array, 1).present.set(true);
    array.readd_member(1).expect("rebuild begins");

    let mut scratch = [0u8; 2 * BS as usize];
    array.resync_step(&mut scratch).expect("one chunk");
    assert_eq!(array.member(1).unwrap().resync_cursor(), 2);
    assert_eq!(array.member_state(1), Some(MemberState::Resyncing));
    assert!(array.needs_resync());

    while array.needs_resync() {
        array.resync_step(&mut scratch).expect("resync step");
    }
    assert_eq!(array.member_state(1), Some(MemberState::InSync));
    assert_eq!(array.member(1).unwrap().resync_cursor(), 0);
}

#[test]
fn a_write_during_rebuild_reaches_only_the_already_synced_region() {
    let mut members = [
        MirrorMember::new(FaultBlock::new(0)),
        MirrorMember::new(FaultBlock::absent()),
    ];
    let mut array = MirrorArray::assemble(&mut members).expect("assembles degraded");
    fill(&mut array);
    dev(&array, 1).present.set(true);
    array.readd_member(1).expect("rebuild begins");

    let mut scratch = [0u8; 2 * BS as usize];
    array
        .resync_step(&mut scratch)
        .expect("sync blocks 0 and 1");
    assert_eq!(array.member(1).unwrap().resync_cursor(), 2);

    // A write inside the already-synced region reaches the rebuilding copy.
    array
        .write_blocks(0, &block(0xCC))
        .expect("synced-region write");
    // A write above the cursor is left to the rebuild to copy.
    array
        .write_blocks(5, &block(0xDD))
        .expect("unsynced-region write");
    assert_eq!(&dev(&array, 1).store[0..512], &block(0xCC));
    assert_eq!(
        &dev(&array, 1).store[5 * 512..6 * 512],
        &block(0),
        "the unsynced block is not written ahead of the rebuild"
    );

    while array.needs_resync() {
        array.resync_step(&mut scratch).expect("resync step");
    }
    // The rebuild copies the current source contents, so the above-cursor
    // write lands on the rebuilt copy from the source.
    assert_eq!(&dev(&array, 1).store[5 * 512..6 * 512], &block(0xDD));
    assert_eq!(&dev(&array, 1).store[0..512], &block(0xCC));
}

#[test]
fn a_rebuild_target_that_cannot_be_written_drops_back_to_faulted() {
    let mut members = [
        MirrorMember::new(FaultBlock::new(0)),
        MirrorMember::new(FaultBlock::absent()),
    ];
    let mut array = MirrorArray::assemble(&mut members).expect("assembles degraded");
    fill(&mut array);
    dev(&array, 1).present.set(true);
    array.readd_member(1).expect("rebuild begins");
    dev(&array, 1)
        .write_fault
        .set(Some(DriverError::DeviceFault));

    let mut scratch = block(0);
    array
        .resync_step(&mut scratch)
        .expect("step returns ok, target dropped");
    assert_eq!(array.member_state(1), Some(MemberState::Faulted));
    assert!(!array.needs_resync());
    assert_eq!(array.health(), ArrayHealth::Degraded);
}

#[test]
fn readd_fails_closed_on_bad_index_state_or_geometry() {
    let mut members = [
        MirrorMember::new(FaultBlock::new(0)),
        MirrorMember::new(FaultBlock::absent()),
    ];
    let mut array = MirrorArray::assemble(&mut members).expect("assembles degraded");
    assert_eq!(
        array.readd_member(9).unwrap_err(),
        MirrorError::UnknownMember
    );
    assert_eq!(array.readd_member(0).unwrap_err(), MirrorError::NotFaulted);
    // Member 1 is faulted and still absent: cannot be probed.
    assert_eq!(array.readd_member(1).unwrap_err(), MirrorError::ProbeFailed);
    // It returns with a different geometry: refused rather than truncated.
    dev(&array, 1).present.set(true);
    dev(&array, 1).geo.set(BlockGeometry {
        block_size: BS,
        block_count: NBLK + 4,
    });
    assert_eq!(
        array.readd_member(1).unwrap_err(),
        MirrorError::GeometryMismatch
    );
    assert_eq!(array.member_state(1), Some(MemberState::Faulted));
}

#[test]
fn a_permanently_faulted_member_never_stops_the_survivor_serving() {
    let mut members = [
        MirrorMember::new(FaultBlock::new(0)),
        MirrorMember::new(FaultBlock::new(0)),
    ];
    let mut array = MirrorArray::assemble(&mut members).expect("assembles");
    fill(&mut array);
    // The second copy faults for good and is never re-added.
    dev(&array, 1)
        .write_fault
        .set(Some(DriverError::DeviceOffline));
    array
        .write_blocks(0, &block(0x01))
        .expect("survivor still serves");
    assert_eq!(array.member_state(1), Some(MemberState::Faulted));

    // Every subsequent operation keeps working on the survivor.
    for lba in 1..NBLK {
        let mut buf = block(0);
        array.read_blocks(lba, &mut buf).expect("survivor reads");
        assert_eq!(buf, block(pat(lba)));
        array
            .write_blocks(lba, &block(0x02))
            .expect("survivor writes");
        assert_eq!(array.health(), ArrayHealth::Degraded);
    }
}

#[test]
fn a_replaced_member_is_rebuilt_from_the_survivor() {
    let mut members = [
        MirrorMember::new(FaultBlock::new(0)),
        MirrorMember::new(FaultBlock::absent()),
    ];
    let mut array = MirrorArray::assemble(&mut members).expect("assembles degraded");
    fill(&mut array);
    // A physically fresh replacement disk is swapped in for the faulted slot.
    array
        .replace_member(1, FaultBlock::new(0xEE))
        .expect("replacement begins the rebuild");
    assert_eq!(array.member_state(1), Some(MemberState::Resyncing));
    let mut scratch = block(0);
    while array.needs_resync() {
        array.resync_step(&mut scratch).expect("resync step");
    }
    assert_eq!(array.health(), ArrayHealth::Optimal);
    assert_eq!(&dev(&array, 1).store[0..512], &block(pat(0)));
}

#[test]
fn the_class_carrying_read_and_write_thread_the_sensitivity_class() {
    let mut members = [
        MirrorMember::new(FaultBlock::new(0)),
        MirrorMember::new(FaultBlock::new(0)),
    ];
    let mut array = MirrorArray::assemble(&mut members).expect("assembles");
    let secret = block(0x9);
    array
        .write_blocks_with_class(0, &secret, BufferClass::Sensitive)
        .expect("sensitive write");
    let mut buf = block(0);
    array
        .read_blocks_with_class(0, &mut buf, BufferClass::Sensitive)
        .expect("sensitive read");
    assert_eq!(buf, secret);
}

#[test]
fn array_health_maps_onto_the_shared_mount_availability_vocabulary() {
    use tairix_abi::sysinfo::MountAvailability;
    assert_eq!(
        ArrayHealth::Optimal.to_mount_availability(),
        MountAvailability::Available
    );
    assert_eq!(
        ArrayHealth::Degraded.to_mount_availability(),
        MountAvailability::Degraded
    );
    assert_eq!(
        ArrayHealth::Recovering.to_mount_availability(),
        MountAvailability::Recovering
    );
    assert_eq!(
        ArrayHealth::Failed.to_mount_availability(),
        MountAvailability::UnavailableLost
    );
    assert!(ArrayHealth::Degraded.is_serving());
    assert!(!ArrayHealth::Failed.is_serving());
}

#[test]
fn member_role_maps_from_the_reassembly_slot_verdict() {
    // The single mapping from the on-disk reassembly verdict to the composed
    // member's role: a missing slot offers no device, a current copy joins
    // in sync, a behind copy joins as a stale rebuild target.
    assert_eq!(MemberRole::for_slot(SlotDisposition::Missing), None);
    assert_eq!(
        MemberRole::for_slot(SlotDisposition::Present {
            tag: 7,
            in_sync: true,
        }),
        Some(MemberRole::Current)
    );
    assert_eq!(
        MemberRole::for_slot(SlotDisposition::Present {
            tag: 7,
            in_sync: false,
        }),
        Some(MemberRole::Stale)
    );
}

#[test]
fn a_stale_member_joins_resyncing_and_never_serves_a_read() {
    // The reassembly proved copy 1 is behind (a lower generation): it must be
    // rebuilt from the current copy before it can answer a read, so the array
    // can never hand a reader out-of-date bytes.
    let mut members = [
        MirrorMember::new(FaultBlock::new(0)),
        MirrorMember::with_role(FaultBlock::new(0xEE), MemberRole::Stale),
    ];
    let mut array = MirrorArray::assemble(&mut members).expect("assembles recovering");
    assert_eq!(array.member_state(0), Some(MemberState::InSync));
    assert_eq!(array.member_state(1), Some(MemberState::Resyncing));
    assert_eq!(array.health(), ArrayHealth::Recovering);
    fill(&mut array);

    // The current copy holds the real data; a read is served only from it,
    // never from the stale copy that still holds its pre-join fill (0xEE).
    for lba in 0..NBLK {
        let mut buf = block(0);
        array
            .read_blocks(lba, &mut buf)
            .expect("read from the source");
        assert_eq!(buf, block(pat(lba)), "served from the current copy");
    }
    assert_eq!(dev(&array, 1).reads.get(), 0);
    assert_eq!(array.member(1).unwrap().role(), MemberRole::Stale);
}

#[test]
fn a_stale_member_becomes_a_read_source_only_after_it_is_resynced() {
    let mut members = [
        MirrorMember::new(FaultBlock::new(0)),
        MirrorMember::with_role(FaultBlock::new(0xEE), MemberRole::Stale),
    ];
    let mut array = MirrorArray::assemble(&mut members).expect("assembles recovering");
    fill(&mut array);

    let mut scratch = block(0);
    let mut steps = 0u32;
    while array.needs_resync() {
        array.resync_step(&mut scratch).expect("resync step");
        steps += 1;
        assert!(steps <= 100, "the rebuild terminates");
    }
    assert_eq!(array.member_state(1), Some(MemberState::InSync));
    assert_eq!(array.health(), ArrayHealth::Optimal);

    // Now that it is in sync, fault the source and prove the rebuilt copy
    // holds the current data.
    dev(&array, 0)
        .read_fault
        .set(Some(DriverError::DeviceOffline));
    for lba in 0..NBLK {
        let mut buf = block(0);
        array
            .read_blocks(lba, &mut buf)
            .expect("served from the rebuilt copy");
        assert_eq!(
            buf,
            block(pat(lba)),
            "block {lba} rebuilt with current data"
        );
    }
}

#[test]
fn an_all_stale_member_set_fails_closed_with_no_rebuild_source() {
    // Every copy is behind and none is a trusted source: the array assembles
    // but cannot serve or rebuild, and fails closed rather than promoting a
    // stale copy to a read source.
    let mut members = [
        MirrorMember::with_role(FaultBlock::new(0), MemberRole::Stale),
        MirrorMember::with_role(FaultBlock::new(0), MemberRole::Stale),
    ];
    let mut array = MirrorArray::assemble(&mut members).expect("assembles");
    assert_eq!(array.health(), ArrayHealth::Failed);
    let mut buf = block(0);
    assert_eq!(
        array.read_blocks(0, &mut buf).unwrap_err(),
        DriverError::DeviceOffline
    );
    let mut scratch = block(0);
    assert_eq!(
        array.resync_step(&mut scratch).unwrap_err(),
        DriverError::DeviceOffline
    );
}

#[test]
fn a_stale_member_that_cannot_be_probed_joins_faulted() {
    // A stale copy whose device is absent at assembly is admitted faulted (no
    // usable device), exactly like a current copy that cannot be probed —
    // never silently resyncing from a device that is not there.
    let mut members = [
        MirrorMember::new(FaultBlock::new(0)),
        MirrorMember::with_role(FaultBlock::absent(), MemberRole::Stale),
    ];
    let array = MirrorArray::assemble(&mut members).expect("assembles degraded");
    assert_eq!(array.member_state(0), Some(MemberState::InSync));
    assert_eq!(array.member_state(1), Some(MemberState::Faulted));
    assert_eq!(array.health(), ArrayHealth::Degraded);
}

#[test]
fn scrub_verifies_every_copy_and_a_clean_pass_is_a_no_op() {
    let mut members = [
        MirrorMember::new(FaultBlock::new(0)),
        MirrorMember::new(FaultBlock::new(0)),
    ];
    let mut array = MirrorArray::assemble(&mut members).expect("assembles");
    assert!(!array.scrubbing(), "no scrub in progress after assembly");
    array.begin_scrub();
    assert!(array.scrubbing());
    let mut scratch = [0u8; CAP];
    while array.scrubbing() {
        array.scrub_step(&mut scratch).expect("clean scrub step");
    }
    // Unlike a read (which stops at the first serving copy), a scrub reads
    // *every* in-sync copy of the whole array — here one whole-array chunk.
    assert_eq!(dev(&array, 0).reads.get(), 1);
    assert_eq!(dev(&array, 1).reads.get(), 1);
    assert_eq!(array.health(), ArrayHealth::Optimal);
    // A completed pass is idempotent: another step is a no-op.
    array
        .scrub_step(&mut scratch)
        .expect("no-op after completion");
    assert!(!array.scrubbing());
    assert_eq!(array.scrub_cursor(), NBLK);
}

#[test]
fn scrub_finds_and_repairs_a_latent_bad_sector_on_a_non_primary_copy() {
    let mut members = [
        MirrorMember::new(FaultBlock::new(0)),
        MirrorMember::new(FaultBlock::new(0)),
    ];
    let mut array = MirrorArray::assemble(&mut members).expect("assembles");
    let data = block(0xC4);
    array.write_blocks(0, &data).expect("seed both copies");
    // The *second* copy (never the read source) develops a latent bad sector.
    dev(&array, 1)
        .read_fault
        .set(Some(DriverError::MediumError));
    // The read path never consults the second copy, so the latent error stays
    // invisible — the gap a scrub exists to close.
    let mut buf = block(0);
    array.read_blocks(0, &mut buf).expect("served from copy 0");
    assert_eq!(
        dev(&array, 1).read_fault.get(),
        Some(DriverError::MediumError),
        "the read path did not touch — nor repair — the second copy"
    );
    let seed_writes_0 = dev(&array, 0).writes.get();
    let seed_writes_1 = dev(&array, 1).writes.get();
    // A scrub proactively reads the second copy, finds the bad sector, and
    // repairs it from the good copy.
    array.begin_scrub();
    let mut scratch = [0u8; CAP];
    while array.scrubbing() {
        array
            .scrub_step(&mut scratch)
            .expect("scrub repairs the bad sector");
    }
    assert_eq!(
        dev(&array, 1).read_fault.get(),
        None,
        "the bad sector was reallocated by the repair write-back"
    );
    assert_eq!(array.member_state(1), Some(MemberState::InSync));
    assert_eq!(array.health(), ArrayHealth::Optimal);
    // Only the bad copy was written (repaired); the good source was not.
    assert_eq!(dev(&array, 0).writes.get(), seed_writes_0);
    assert_eq!(dev(&array, 1).writes.get(), seed_writes_1 + 1);
}

#[test]
fn scrub_drops_a_whole_device_faulting_copy() {
    let mut members = [
        MirrorMember::new(FaultBlock::new(0)),
        MirrorMember::new(FaultBlock::new(0)),
    ];
    let mut array = MirrorArray::assemble(&mut members).expect("assembles");
    dev(&array, 1)
        .read_fault
        .set(Some(DriverError::DeviceOffline));
    array.begin_scrub();
    let mut scratch = [0u8; CAP];
    while array.scrubbing() {
        array
            .scrub_step(&mut scratch)
            .expect("scrub survives a dropped copy");
    }
    assert_eq!(array.member_state(1), Some(MemberState::Faulted));
    assert_eq!(array.member_state(0), Some(MemberState::InSync));
    assert_eq!(array.health(), ArrayHealth::Degraded);
}

#[test]
fn scrub_reports_a_block_bad_on_every_copy_and_still_advances() {
    let mut members = [
        MirrorMember::new(FaultBlock::new(0)),
        MirrorMember::new(FaultBlock::new(0)),
    ];
    let mut array = MirrorArray::assemble(&mut members).expect("assembles");
    for i in 0..2 {
        dev(&array, i)
            .read_fault
            .set(Some(DriverError::MediumError));
    }
    array.begin_scrub();
    let mut scratch = [0u8; CAP];
    // Bad on every copy: the loss is surfaced, but the cursor advances past it
    // so the pass makes progress rather than looping on the unrepairable block.
    assert_eq!(
        array.scrub_step(&mut scratch).unwrap_err(),
        DriverError::MediumError
    );
    assert!(!array.scrubbing(), "cursor advanced past the whole array");
    // A per-block error on every copy is a data loss, not a device fault: no
    // copy is dropped.
    assert_eq!(array.member_state(0), Some(MemberState::InSync));
    assert_eq!(array.member_state(1), Some(MemberState::InSync));
}

#[test]
fn scrub_is_bounded_and_interruptible() {
    let mut members = [
        MirrorMember::new(FaultBlock::new(0)),
        MirrorMember::new(FaultBlock::new(0)),
    ];
    let mut array = MirrorArray::assemble(&mut members).expect("assembles");
    array.begin_scrub();
    // A two-block scratch advances the cursor two blocks at a time, yielding
    // between chunks — a 100 TB+ array never scrubs in one unbounded sweep.
    let mut two = [0u8; 2 * BS as usize];
    let mut steps = 0u64;
    let mut expected = 2u64;
    while array.scrubbing() {
        array.scrub_step(&mut two).expect("bounded scrub step");
        assert_eq!(array.scrub_cursor(), expected.min(NBLK));
        expected += 2;
        steps += 1;
    }
    assert_eq!(steps, NBLK / 2, "one step per two-block chunk");
    // begin_scrub restarts the pass from block 0.
    array.begin_scrub();
    assert_eq!(array.scrub_cursor(), 0);
    assert!(array.scrubbing());
}

#[test]
fn scrub_on_a_failed_array_fails_closed_without_advancing() {
    let mut members = [
        MirrorMember::with_role(FaultBlock::new(0), MemberRole::Stale),
        MirrorMember::with_role(FaultBlock::new(0), MemberRole::Stale),
    ];
    let mut array = MirrorArray::assemble(&mut members).expect("assembles");
    assert_eq!(array.health(), ArrayHealth::Failed);
    array.begin_scrub();
    let mut scratch = [0u8; CAP];
    assert_eq!(
        array.scrub_step(&mut scratch).unwrap_err(),
        DriverError::DeviceOffline
    );
    assert_eq!(array.scrub_cursor(), 0, "a failed array does not advance");
}

#[test]
fn scrub_step_rejects_a_bad_scratch_buffer() {
    let mut members = [
        MirrorMember::new(FaultBlock::new(0)),
        MirrorMember::new(FaultBlock::new(0)),
    ];
    let mut array = MirrorArray::assemble(&mut members).expect("assembles");
    array.begin_scrub();
    assert_eq!(
        array.scrub_step(&mut []).unwrap_err(),
        DriverError::BufferTooSmall
    );
    let mut ragged = [0u8; BS as usize - 12];
    assert_eq!(
        array.scrub_step(&mut ragged).unwrap_err(),
        DriverError::BufferTooSmall
    );
}

// --- Missing (absent) member slots: md-style "removed" slots -----------------

#[test]
fn an_absent_slot_counts_toward_the_width_and_reports_degraded_not_optimal() {
    // A slot the array is *defined* to have but which holds no device is a
    // first-class missing member: the array comes up on its width and reports
    // the reduced redundancy, never masquerading as a smaller, optimal array.
    let mut members = [
        MirrorMember::new(FaultBlock::new(0)),
        MirrorMember::absent(),
    ];
    let array = MirrorArray::assemble(&mut members).expect("assembles degraded");
    assert_eq!(
        array.member_count(),
        2,
        "the absent slot counts toward width"
    );
    assert_eq!(array.member_state(0), Some(MemberState::InSync));
    assert_eq!(array.member_state(1), Some(MemberState::Absent));
    assert_eq!(
        array.health(),
        ArrayHealth::Degraded,
        "a missing member reduces redundancy"
    );
    // The absent slot holds no device and never fabricates one.
    assert!(array.member(1).unwrap().device().is_none());
}

#[test]
fn an_all_absent_member_set_fails_closed_with_no_geometry() {
    // No slot holds a device, so no geometry can be established: the array
    // fails closed rather than inventing an empty one.
    let mut members: [MirrorMember<FaultBlock>; 2] =
        [MirrorMember::absent(), MirrorMember::absent()];
    assert_eq!(
        MirrorArray::assemble(&mut members).err(),
        Some(MirrorError::NoUsableMember)
    );
}

#[test]
fn reads_and_writes_serve_from_the_present_copy_with_an_absent_slot() {
    let mut members = [
        MirrorMember::new(FaultBlock::new(0)),
        MirrorMember::absent(),
    ];
    let mut array = MirrorArray::assemble(&mut members).expect("assembles degraded");
    let data = block(0x44);
    array
        .write_blocks(1, &data)
        .expect("write to the present copy");
    let mut buf = block(0);
    array
        .read_blocks(1, &mut buf)
        .expect("read from the present copy");
    assert_eq!(buf, data);
    // The write reached the present copy and the array stayed degraded (never
    // faulting the absent slot, which had no device to fault).
    assert_eq!(&dev(&array, 0).store[512..1024], &data[..]);
    assert_eq!(array.member_state(1), Some(MemberState::Absent));
    assert_eq!(array.health(), ArrayHealth::Degraded);
}

#[test]
fn remove_member_vacates_a_faulted_slot_to_absent_and_returns_the_device() {
    let mut members = [
        MirrorMember::new(FaultBlock::new(0)),
        MirrorMember::new(FaultBlock::new(0)),
    ];
    let mut array = MirrorArray::assemble(&mut members).expect("assembles");
    fill(&mut array);
    // Only a faulted member may be removed: a live one is still participating.
    // (`.err()` rather than `unwrap_err()` so the device double needs no
    // `Debug`; the removed device is returned by value on success.)
    assert_eq!(array.remove_member(0).err(), Some(MirrorError::NotFaulted));
    assert_eq!(
        array.remove_member(9).err(),
        Some(MirrorError::UnknownMember)
    );
    // Fault the second copy on a write, then pull it out of the array.
    dev(&array, 1)
        .write_fault
        .set(Some(DriverError::DeviceOffline));
    array
        .write_blocks(0, &block(0x01))
        .expect("survivor still serves");
    assert_eq!(array.member_state(1), Some(MemberState::Faulted));
    let removed = array.remove_member(1).expect("the faulted disk is pulled");
    // The returned device is the real one: it still holds its pre-fault fill
    // for block 0 (the faulting write never landed on it).
    assert_eq!(&removed.store[0..512], &block(pat(0))[..]);
    assert_eq!(array.member_state(1), Some(MemberState::Absent));
    assert_eq!(array.health(), ArrayHealth::Degraded);
    // The vacated slot no longer holds a device.
    assert!(array.member(1).unwrap().device().is_none());
    // A survivor keeps serving through the vacancy.
    let mut buf = block(0);
    array.read_blocks(2, &mut buf).expect("survivor reads");
    assert_eq!(buf, block(pat(2)));
}

#[test]
fn add_member_installs_a_spare_into_an_absent_slot_and_rebuilds_current_data() {
    let mut members = [
        MirrorMember::new(FaultBlock::new(0)),
        MirrorMember::absent(),
    ];
    let mut array = MirrorArray::assemble(&mut members).expect("assembles degraded");
    fill(&mut array);
    assert_eq!(array.health(), ArrayHealth::Degraded);

    // A spare is added into the empty slot and begins rebuilding at once.
    array
        .add_member(1, FaultBlock::new(0xEE))
        .expect("spare joins the empty slot");
    assert_eq!(array.member_state(1), Some(MemberState::Resyncing));
    assert_eq!(array.health(), ArrayHealth::Recovering);

    let mut scratch = block(0);
    let mut steps = 0u32;
    while array.needs_resync() {
        array.resync_step(&mut scratch).expect("resync step");
        steps += 1;
        assert!(steps <= 100, "the rebuild terminates");
    }
    assert_eq!(array.member_state(1), Some(MemberState::InSync));
    assert_eq!(array.health(), ArrayHealth::Optimal);

    // Prove the spare rebuilt with current data: fault the source and read
    // every block back from the freshly-rebuilt copy.
    dev(&array, 0)
        .read_fault
        .set(Some(DriverError::DeviceOffline));
    for lba in 0..NBLK {
        let mut buf = block(0);
        array
            .read_blocks(lba, &mut buf)
            .expect("served from the rebuilt spare");
        assert_eq!(
            buf,
            block(pat(lba)),
            "block {lba} rebuilt with current data"
        );
    }
}

#[test]
fn add_member_fails_closed_on_an_occupied_slot_or_bad_geometry() {
    let mut members = [
        MirrorMember::new(FaultBlock::new(0)),
        MirrorMember::absent(),
    ];
    let mut array = MirrorArray::assemble(&mut members).expect("assembles degraded");
    // The live slot 0 already holds a device: refuse rather than clobber it.
    assert_eq!(
        array.add_member(0, FaultBlock::new(1)).unwrap_err(),
        MirrorError::SlotOccupied
    );
    assert_eq!(
        array.add_member(9, FaultBlock::new(1)).unwrap_err(),
        MirrorError::UnknownMember
    );
    // A spare with the wrong geometry is refused and the slot is left faulted
    // holding it (present but unusable), never admitted as a rebuild source.
    let other = BlockGeometry {
        block_size: BS,
        block_count: NBLK + 1,
    };
    assert_eq!(
        array
            .add_member(1, FaultBlock::new(1).with_geometry(other))
            .unwrap_err(),
        MirrorError::GeometryMismatch
    );
    assert_eq!(array.member_state(1), Some(MemberState::Faulted));
}

#[test]
fn remove_then_add_is_the_full_disk_replacement_workflow() {
    // The end-to-end Linux-md workflow: a failed disk is removed (vacating the
    // slot) and a fresh spare added into it, which rebuilds to full redundancy
    // without a reboot.
    let mut members = [
        MirrorMember::new(FaultBlock::new(0)),
        MirrorMember::new(FaultBlock::new(0)),
    ];
    let mut array = MirrorArray::assemble(&mut members).expect("assembles");
    fill(&mut array);
    dev(&array, 1)
        .write_fault
        .set(Some(DriverError::DeviceOffline));
    array
        .write_blocks(0, &block(0x01))
        .expect("survivor still serves");
    assert_eq!(array.member_state(1), Some(MemberState::Faulted));

    let _pulled = array.remove_member(1).expect("pull the failed disk");
    assert_eq!(array.member_state(1), Some(MemberState::Absent));
    assert_eq!(array.health(), ArrayHealth::Degraded);

    array
        .add_member(1, FaultBlock::new(0x99))
        .expect("insert a fresh spare");
    let mut scratch = block(0);
    while array.needs_resync() {
        array.resync_step(&mut scratch).expect("resync step");
    }
    assert_eq!(array.health(), ArrayHealth::Optimal);
    // The new disk holds the array's current contents (block 0 = 0x01 written
    // while degraded, the rest the original fill).
    dev(&array, 0)
        .read_fault
        .set(Some(DriverError::DeviceOffline));
    let mut buf = block(0);
    array
        .read_blocks(0, &mut buf)
        .expect("served from the spare");
    assert_eq!(buf, block(0x01));
    for lba in 1..NBLK {
        let mut b = block(0);
        array
            .read_blocks(lba, &mut b)
            .expect("served from the spare");
        assert_eq!(b, block(pat(lba)));
    }
}

#[test]
fn device_health_sums_in_sync_members() {
    // A composed mirror surfaces its members' telemetry rather than the
    // trait default (`Unavailable`): independent integrity faults sum.
    let mut members = [
        MirrorMember::new(FaultBlock::new(0).with_media_errors(3)),
        MirrorMember::new(FaultBlock::new(0).with_media_errors(5)),
    ];
    let array = MirrorArray::assemble(&mut members).expect("assembles");
    let DeviceHealth::Available(h) = array.device_health().expect("health read") else {
        panic!("expected aggregated telemetry, not Unavailable");
    };
    assert_eq!(h.media_errors, 8);
}

#[test]
fn device_health_excludes_faulted_and_absent_and_includes_resyncing() {
    // Slot 0 in-sync (3 media errors), slot 1 a live device (5) we will fault
    // then re-add, slot 2 an empty slot with no device.
    let mut members = [
        MirrorMember::new(FaultBlock::new(0).with_media_errors(3)),
        MirrorMember::new(FaultBlock::new(0).with_media_errors(5)),
        MirrorMember::absent(),
    ];
    let mut array = MirrorArray::assemble(&mut members).expect("assembles");

    // A whole-device write fault on slot 1 drops it; the write still succeeds
    // through slot 0. An absent slot never held a device.
    dev(&array, 1)
        .write_fault
        .set(Some(DriverError::DeviceOffline));
    array
        .write_blocks(0, &block(9))
        .expect("write succeeds via the healthy copy");
    assert_eq!(array.member_state(1), Some(MemberState::Faulted));

    // A faulted slot and an absent slot contribute nothing: only slot 0.
    let DeviceHealth::Available(h) = array.device_health().expect("health read") else {
        panic!("expected aggregated telemetry");
    };
    assert_eq!(h.media_errors, 3);

    // Re-adding slot 1 makes it a resyncing (live) member again, so its
    // telemetry rejoins the aggregate.
    dev(&array, 1).write_fault.set(None);
    array.readd_member(1).expect("re-add begins the rebuild");
    assert_eq!(array.member_state(1), Some(MemberState::Resyncing));
    let DeviceHealth::Available(h) = array.device_health().expect("health read") else {
        panic!("expected aggregated telemetry");
    };
    assert_eq!(h.media_errors, 8);
}

#[test]
fn device_health_is_unavailable_when_no_member_reports_and_skips_an_errored_member() {
    // No member exposes telemetry -> the array reports Unavailable, never a
    // zeroed snapshot that would look perfectly healthy.
    let mut none = [
        MirrorMember::new(FaultBlock::new(0)),
        MirrorMember::new(FaultBlock::new(0)),
    ];
    let array = MirrorArray::assemble(&mut none).expect("assembles");
    assert_eq!(
        array.device_health().expect("health read"),
        DeviceHealth::Unavailable
    );

    // A member whose telemetry read errors is skipped, never failing the
    // whole array-level query: the readable member still speaks.
    let mut mixed = [
        MirrorMember::new(FaultBlock::new(0).with_media_errors(7)),
        MirrorMember::new(FaultBlock::new(0)),
    ];
    let array = MirrorArray::assemble(&mut mixed).expect("assembles");
    dev(&array, 1).health_fault.set(true);
    let DeviceHealth::Available(h) = array.device_health().expect("health read") else {
        panic!("expected the readable member's telemetry");
    };
    assert_eq!(h.media_errors, 7);
}

#[test]
fn device_class_is_the_most_patient_live_member() {
    // A mirror answers only as fast as the copy it is waiting on, so an
    // array pairing an SSD with a spinning disk must be served the spinning
    // disk's spin-up budget — reporting the SSD's would have a consumer time
    // out a perfectly healthy array whenever the slow copy answered.
    let mut members = [
        MirrorMember::new(FaultBlock::new(0).with_class(BlkDeviceClass::SolidState)),
        MirrorMember::new(FaultBlock::new(0).with_class(BlkDeviceClass::Rotational)),
    ];
    let array = MirrorArray::assemble(&mut members).expect("assembles");
    assert_eq!(array.device_class(), BlkDeviceClass::Rotational);

    // Member order cannot change the answer.
    let mut reversed = [
        MirrorMember::new(FaultBlock::new(0).with_class(BlkDeviceClass::Rotational)),
        MirrorMember::new(FaultBlock::new(0).with_class(BlkDeviceClass::SolidState)),
    ];
    let array = MirrorArray::assemble(&mut reversed).expect("assembles");
    assert_eq!(array.device_class(), BlkDeviceClass::Rotational);
}

#[test]
fn a_dropped_member_no_longer_speaks_for_the_arrays_class() {
    // The slow copy faults out: the array is no longer waiting on it, so it
    // stops buying its patience and reports the surviving SSD's envelope.
    let mut members = [
        MirrorMember::new(FaultBlock::new(0).with_class(BlkDeviceClass::SolidState)),
        MirrorMember::new(FaultBlock::new(0).with_class(BlkDeviceClass::Rotational)),
    ];
    let mut array = MirrorArray::assemble(&mut members).expect("assembles");
    assert_eq!(array.device_class(), BlkDeviceClass::Rotational);

    dev(&array, 1)
        .write_fault
        .set(Some(DriverError::DeviceFault));
    array
        .write_blocks(0, &block(0x5A))
        .expect("still durable on the surviving copy");
    assert_eq!(array.member_state(1), Some(MemberState::Faulted));
    assert_eq!(array.device_class(), BlkDeviceClass::SolidState);
}

#[test]
fn an_array_with_no_live_member_declares_the_bounded_envelope() {
    // Every copy gone: the array can serve nothing, so it declares the
    // bounded unclassified envelope rather than the widest one — its callers
    // fail closed sooner instead of waiting out disks that are not there.
    let mut members = [
        MirrorMember::<FaultBlock>::absent(),
        MirrorMember::<FaultBlock>::absent(),
    ];
    assert!(MirrorArray::assemble(&mut members).is_err());

    let mut members = [
        MirrorMember::new(FaultBlock::new(0).with_class(BlkDeviceClass::Rotational)),
        MirrorMember::<FaultBlock>::absent(),
    ];
    let mut array = MirrorArray::assemble(&mut members).expect("assembles degraded");
    dev(&array, 0)
        .write_fault
        .set(Some(DriverError::DeviceFault));
    assert!(array.write_blocks(0, &block(0x5A)).is_err());
    assert_eq!(array.member_state(0), Some(MemberState::Faulted));
    assert_eq!(array.device_class(), BlkDeviceClass::Virtual);
}

#[test]
fn an_array_with_nothing_running_reports_no_progress() {
    let mut members = [
        MirrorMember::new(FaultBlock::new(0)),
        MirrorMember::new(FaultBlock::new(0)),
    ];
    let array = MirrorArray::assemble(&mut members).expect("assembles");
    assert_eq!(array.progress(), ArrayProgress::IDLE);
    assert!(!array.progress().is_active());
}

#[test]
fn a_rebuild_resumes_where_a_restart_left_it() {
    // A rebuild of a 100 TB+ array runs for hours, so it will meet a restart.
    // Losing the cursor would mean starting over every time, and an array
    // rebooted often enough would never finish rebuilding at all.
    let mut members = [
        MirrorMember::new(FaultBlock::new(0)),
        MirrorMember::with_role(FaultBlock::new(0), MemberRole::Stale),
    ];
    let mut scratch = [0u8; 2 * BS as usize];
    let checkpoint = {
        let mut array = MirrorArray::assemble(&mut members).expect("assembles");
        fill(&mut array);
        assert_eq!(array.member_state(1), Some(MemberState::Resyncing));
        array.resync_step(&mut scratch).expect("one rebuild chunk");
        assert_eq!(array.member(1).unwrap().resync_cursor(), 2);
        let checkpoint = array.progress();
        assert_eq!(checkpoint.resync_cursor, Some(2));
        assert_eq!(checkpoint.scrub_cursor, None);
        checkpoint
    };

    // The serving process restarts: the array is assembled afresh from the
    // same devices, which by itself starts the rebuild over.
    let mut array = MirrorArray::assemble(&mut members).expect("re-assembles");
    assert_eq!(array.member(1).unwrap().resync_cursor(), 0);
    array
        .restore_progress(checkpoint)
        .expect("the checkpointed position is adopted");
    assert_eq!(array.member(1).unwrap().resync_cursor(), 2);

    // Six blocks remain in two-block chunks: exactly three more steps. A
    // rebuild that had silently restarted would need four.
    let mut steps = 0u32;
    while array.needs_resync() {
        array.resync_step(&mut scratch).expect("rebuild chunk");
        steps += 1;
        assert!(steps <= 100, "the rebuild terminates");
    }
    assert_eq!(steps, 3, "the rebuild resumed rather than starting over");
    assert_eq!(array.member_state(1), Some(MemberState::InSync));
    assert_eq!(array.health(), ArrayHealth::Optimal);

    // The resumed rebuild is complete and correct: with the source gone, the
    // rebuilt copy serves every block, including the two copied before the
    // restart.
    dev(&array, 0)
        .read_fault
        .set(Some(DriverError::DeviceOffline));
    for lba in 0..NBLK {
        let mut buf = block(0);
        array
            .read_blocks(lba, &mut buf)
            .expect("served from the rebuilt copy");
        assert_eq!(buf, block(pat(lba)), "block {lba} holds current data");
    }
}

#[test]
fn a_scrub_pass_resumes_where_a_restart_left_it() {
    let mut members = [
        MirrorMember::new(FaultBlock::new(0)),
        MirrorMember::new(FaultBlock::new(0)),
    ];
    let mut scratch = [0u8; 2 * BS as usize];
    let checkpoint = {
        let mut array = MirrorArray::assemble(&mut members).expect("assembles");
        fill(&mut array);
        array.begin_scrub();
        array.scrub_step(&mut scratch).expect("one scrub chunk");
        assert_eq!(array.scrub_cursor(), 2);
        let checkpoint = array.progress();
        assert_eq!(checkpoint.scrub_cursor, Some(2));
        assert_eq!(checkpoint.resync_cursor, None);
        checkpoint
    };

    let mut array = MirrorArray::assemble(&mut members).expect("re-assembles");
    assert!(!array.scrubbing(), "a fresh assembly is not mid-pass");
    array
        .restore_progress(checkpoint)
        .expect("the checkpointed position is adopted");
    assert!(array.scrubbing());
    assert_eq!(array.scrub_cursor(), 2);

    let mut steps = 0u32;
    while array.scrubbing() {
        array.scrub_step(&mut scratch).expect("scrub chunk");
        steps += 1;
        assert!(steps <= 100, "the pass terminates");
    }
    assert_eq!(steps, 3, "the pass resumed rather than starting over");
}

#[test]
fn a_restored_cursor_never_un_syncs_a_current_member() {
    // A rebuild cursor is meaningless for a member that is not rebuilding, and
    // planting one on a current copy would describe a rebuild that is not
    // happening. Every copy must stay a trusted read source.
    let mut members = [
        MirrorMember::new(FaultBlock::new(0)),
        MirrorMember::new(FaultBlock::new(0)),
    ];
    let mut array = MirrorArray::assemble(&mut members).expect("assembles");
    fill(&mut array);
    array
        .restore_progress(ArrayProgress {
            scrub_cursor: None,
            resync_cursor: Some(3),
        })
        .expect("accepted: the cursor is inside the array");
    assert_eq!(array.health(), ArrayHealth::Optimal);
    for index in 0..2 {
        assert_eq!(array.member_state(index), Some(MemberState::InSync));
        assert_eq!(array.member(index).unwrap().resync_cursor(), 0);
    }
    assert!(!array.needs_resync());
}

#[test]
fn a_cursor_outside_the_array_is_refused_and_changes_nothing() {
    // A cursor past the end cannot have come from this array. Adopted as a
    // rebuild position it would mark the copy fully rebuilt without its tail
    // ever being written, leaving stale data trusted as current — so it is
    // refused outright rather than clamped.
    let mut members = [
        MirrorMember::new(FaultBlock::new(0)),
        MirrorMember::with_role(FaultBlock::new(0), MemberRole::Stale),
    ];
    let mut array = MirrorArray::assemble(&mut members).expect("assembles");
    fill(&mut array);
    assert_eq!(array.member_state(1), Some(MemberState::Resyncing));

    for cursor in [NBLK, NBLK + 1, u64::MAX] {
        assert_eq!(
            array.restore_progress(ArrayProgress {
                scrub_cursor: Some(cursor),
                resync_cursor: None,
            }),
            Err(MirrorError::CursorOutOfRange)
        );
        assert_eq!(
            array.restore_progress(ArrayProgress {
                scrub_cursor: None,
                resync_cursor: Some(cursor),
            }),
            Err(MirrorError::CursorOutOfRange)
        );
    }
    // Nothing moved: the array is still at its fresh-start position.
    assert!(!array.scrubbing());
    assert_eq!(array.member(1).unwrap().resync_cursor(), 0);

    // The last real block is accepted, so the refusal is exactly at the end
    // and not one block early.
    array
        .restore_progress(ArrayProgress {
            scrub_cursor: Some(NBLK - 1),
            resync_cursor: Some(NBLK - 1),
        })
        .expect("the last block is a valid position");
    assert_eq!(array.scrub_cursor(), NBLK - 1);
    assert_eq!(array.member(1).unwrap().resync_cursor(), NBLK - 1);
}

#[test]
fn a_lost_record_costs_time_and_never_correctness() {
    // The record was absent or unreadable, so the caller restores the idle
    // position: the array simply starts its passes from the beginning.
    let mut members = [
        MirrorMember::new(FaultBlock::new(0)),
        MirrorMember::with_role(FaultBlock::new(0), MemberRole::Stale),
    ];
    let mut array = MirrorArray::assemble(&mut members).expect("assembles");
    fill(&mut array);
    array
        .restore_progress(ArrayProgress::IDLE)
        .expect("an empty position is always acceptable");
    assert!(!array.scrubbing());
    assert_eq!(array.member(1).unwrap().resync_cursor(), 0);

    let mut scratch = [0u8; 2 * BS as usize];
    while array.needs_resync() {
        array.resync_step(&mut scratch).expect("rebuild chunk");
    }
    dev(&array, 0)
        .read_fault
        .set(Some(DriverError::DeviceOffline));
    for lba in 0..NBLK {
        let mut buf = block(0);
        array
            .read_blocks(lba, &mut buf)
            .expect("rebuilt copy serves");
        assert_eq!(buf, block(pat(lba)));
    }
}

#[test]
fn the_reported_rebuild_position_is_the_least_advanced_copy() {
    // Two copies rebuilding at different cursors, one record: reporting the
    // furthest ahead would leave the other's outstanding blocks never copied,
    // so the least advanced is reported and a resume merely re-copies a little.
    let mut members = [
        MirrorMember::new(FaultBlock::new(0)),
        MirrorMember::with_role(FaultBlock::new(0), MemberRole::Stale),
        MirrorMember::new(FaultBlock::new(0)),
    ];
    let mut array = MirrorArray::assemble(&mut members).expect("assembles");
    fill(&mut array);
    let mut scratch = [0u8; 2 * BS as usize];
    array.resync_step(&mut scratch).expect("one rebuild chunk");
    assert_eq!(array.member(1).unwrap().resync_cursor(), 2);

    // The third copy now fails a write, drops out, recovers, and starts its
    // own rebuild from the beginning — so the two rebuilds are at different
    // cursors, which is the case one record has to represent.
    dev(&array, 2)
        .write_fault
        .set(Some(DriverError::DeviceFault));
    array
        .write_blocks(0, &block(0xD1))
        .expect("the write still lands on a surviving copy");
    assert_eq!(array.member_state(2), Some(MemberState::Faulted));
    dev(&array, 2).write_fault.set(None);
    array
        .readd_member(2)
        .expect("the copy rejoins as a rebuild");
    assert_eq!(array.member(2).unwrap().resync_cursor(), 0);

    // The reported position is the *least* advanced of the two, not the
    // furthest ahead: block 0 is still outstanding on the copy that just
    // rejoined, and a record claiming block 0 was done would leave it holding
    // pre-fault data while the array counted it fully rebuilt.
    let checkpoint = array.progress();
    assert_eq!(checkpoint.resync_cursor, Some(0));

    // Restoring it puts both rebuilds there: the copy that was further ahead
    // merely re-copies blocks it already holds, which a rebuild write makes
    // harmless, and no outstanding block is skipped.
    array
        .restore_progress(checkpoint)
        .expect("both rebuilds resume at the shared position");
    assert_eq!(array.member(1).unwrap().resync_cursor(), 0);
    assert_eq!(array.member(2).unwrap().resync_cursor(), 0);

    // Both finish, and both hold current data — including the block written
    // while one of them was out.
    while array.needs_resync() {
        array.resync_step(&mut scratch).expect("rebuild chunk");
    }
    for source in [1usize, 2] {
        for other in [0usize, 1, 2] {
            let fault = (other != source).then_some(DriverError::DeviceOffline);
            dev(&array, other).read_fault.set(fault);
        }
        for lba in 0..NBLK {
            let mut buf = block(0);
            array
                .read_blocks(lba, &mut buf)
                .expect("served from the rebuilt copy");
            let want = if lba == 0 { 0xD1 } else { pat(lba) };
            assert_eq!(buf, block(want), "copy {source} block {lba}");
        }
    }
}
