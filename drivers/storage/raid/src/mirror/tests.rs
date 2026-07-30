//! Host tests for the RAID1 mirror over a fault-injecting [`Block`] double.

use super::{ArrayHealth, MemberState, MirrorArray, MirrorError, MirrorMember};
use core::cell::Cell;
use tairix_abi::driver::block::{Block, BlockGeometry};
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
            reads: Cell::new(0),
            writes: Cell::new(0),
        }
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
}

/// Convenience: a block-sized buffer filled with `v`.
fn block(v: u8) -> [u8; BS as usize] {
    [v; BS as usize]
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
    assert_eq!(
        &array.member(0).unwrap().device().store[512..1024],
        &data[..]
    );
    assert_eq!(
        &array.member(1).unwrap().device().store[512..1024],
        &data[..]
    );
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
    assert_eq!(array.member(0).unwrap().device().reads.get(), 1);
    assert_eq!(array.member(1).unwrap().device().reads.get(), 0);
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
    array
        .member(0)
        .unwrap()
        .device()
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
    assert_eq!(array.member(0).unwrap().device().read_fault.get(), None);
    assert_eq!(array.health(), ArrayHealth::Optimal);
}

#[test]
fn a_whole_device_read_fault_drops_the_copy_and_degrades_the_array() {
    let mut members = [
        MirrorMember::new(FaultBlock::new(0)),
        MirrorMember::new(FaultBlock::new(0)),
    ];
    let mut array = MirrorArray::assemble(&mut members).expect("assembles");
    array
        .member(0)
        .unwrap()
        .device()
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
        array
            .member(i)
            .unwrap()
            .device()
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
    array
        .member(0)
        .unwrap()
        .device()
        .write_fault
        .set(Some(DriverError::DeviceFault));
    let data = block(0x77);
    array
        .write_blocks(0, &data)
        .expect("still durable on the surviving copy");
    assert_eq!(array.member_state(0), Some(MemberState::Faulted));
    assert_eq!(array.member_state(1), Some(MemberState::InSync));
    assert_eq!(array.health(), ArrayHealth::Degraded);
    assert_eq!(&array.member(1).unwrap().device().store[0..512], &data[..]);
}

#[test]
fn a_write_no_copy_accepts_fails_closed() {
    let mut members = [
        MirrorMember::new(FaultBlock::new(0)),
        MirrorMember::new(FaultBlock::new(0)),
    ];
    let mut array = MirrorArray::assemble(&mut members).expect("assembles");
    for i in 0..2 {
        array
            .member(i)
            .unwrap()
            .device()
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
    array.member(0).unwrap().device().flush_fault.set(true);
    array.flush().expect("still durable on the surviving copy");
    assert_eq!(array.member_state(0), Some(MemberState::Faulted));
    array.member(1).unwrap().device().flush_fault.set(true);
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
    array
        .member(1)
        .unwrap()
        .device()
        .write_fault
        .set(Some(DriverError::DeviceFault));
    array
        .write_blocks(3, &block(0xB0))
        .expect("degraded write lands on the survivor");
    assert_eq!(array.member_state(1), Some(MemberState::Faulted));
    assert_eq!(array.health(), ArrayHealth::Degraded);

    // The copy recovers; clear its fault and re-add it.
    array.member(1).unwrap().device().write_fault.set(None);
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
    array
        .member(0)
        .unwrap()
        .device()
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
    array.member(1).unwrap().device().present.set(true);
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
    array.member(1).unwrap().device().present.set(true);
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
    assert_eq!(
        &array.member(1).unwrap().device().store[0..512],
        &block(0xCC)
    );
    assert_eq!(
        &array.member(1).unwrap().device().store[5 * 512..6 * 512],
        &block(0),
        "the unsynced block is not written ahead of the rebuild"
    );

    while array.needs_resync() {
        array.resync_step(&mut scratch).expect("resync step");
    }
    // The rebuild copies the current source contents, so the above-cursor
    // write lands on the rebuilt copy from the source.
    assert_eq!(
        &array.member(1).unwrap().device().store[5 * 512..6 * 512],
        &block(0xDD)
    );
    assert_eq!(
        &array.member(1).unwrap().device().store[0..512],
        &block(0xCC)
    );
}

#[test]
fn a_rebuild_target_that_cannot_be_written_drops_back_to_faulted() {
    let mut members = [
        MirrorMember::new(FaultBlock::new(0)),
        MirrorMember::new(FaultBlock::absent()),
    ];
    let mut array = MirrorArray::assemble(&mut members).expect("assembles degraded");
    fill(&mut array);
    array.member(1).unwrap().device().present.set(true);
    array.readd_member(1).expect("rebuild begins");
    array
        .member(1)
        .unwrap()
        .device()
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
    array.member(1).unwrap().device().present.set(true);
    array.member(1).unwrap().device().geo.set(BlockGeometry {
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
    array
        .member(1)
        .unwrap()
        .device()
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
    assert_eq!(
        &array.member(1).unwrap().device().store[0..512],
        &block(pat(0))
    );
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
