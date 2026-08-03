//! Host tests for the RAID10 stripe of mirrors over a fault-injecting
//! [`Block`] double.

use super::{Raid10Array, Raid10Error};
use crate::mirror::MirrorMember;
use crate::superblock::ArrayProgress;
use core::cell::Cell;
use tairix_abi::blkio::BlkDeviceClass;
use tairix_abi::driver::block::{Block, BlockGeometry, DeviceHealth, HealthSnapshot};
use tairix_abi::driver::{BufferClass, DriverError};
use tairix_abi::raid::{ArrayHealth, MemberState};

// Small blocks keep the test doubles' backing arrays well under clippy's
// large-stack-array threshold even for a four-member array.
const BS: u32 = 64;
const NBLK: u64 = 8;
const CAP: usize = BS as usize * 8;
const CHUNK: u32 = 2;

/// An in-memory block device with programmable, post-assembly-injectable
/// faults (via [`Cell`], so a test can flip a fault through a shared borrow
/// while the array owns the member).
struct FaultBlock {
    store: [u8; CAP],
    geo: Cell<BlockGeometry>,
    present: Cell<bool>,
    read_fault: Cell<Option<DriverError>>,
    write_fault: Cell<Option<DriverError>>,
    health: Cell<DeviceHealth>,
    class: Cell<BlkDeviceClass>,
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
            health: Cell::new(DeviceHealth::Unavailable),
            class: Cell::new(BlkDeviceClass::SolidState),
        }
    }

    /// A device declaring `class` as its performance/behaviour envelope.
    fn with_class(self, class: BlkDeviceClass) -> Self {
        self.class.set(class);
        self
    }

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
        if let Some(e) = self.read_fault.get() {
            return Err(e);
        }
        let (start, end) = Self::span(lba, buf.len())?;
        buf.copy_from_slice(&self.store[start..end]);
        Ok(())
    }

    fn write_blocks(&mut self, lba: u64, buf: &[u8]) -> Result<(), DriverError> {
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
        Ok(())
    }

    fn device_class(&self) -> BlkDeviceClass {
        self.class.get()
    }

    fn device_health(&self) -> Result<DeviceHealth, DriverError> {
        Ok(self.health.get())
    }
}

/// Four healthy members (two pairs), each block pre-filled with `fill`.
fn four(fill: u8) -> [MirrorMember<FaultBlock>; 4] {
    [
        MirrorMember::new(FaultBlock::new(fill)),
        MirrorMember::new(FaultBlock::new(fill)),
        MirrorMember::new(FaultBlock::new(fill)),
        MirrorMember::new(FaultBlock::new(fill)),
    ]
}

/// A block-sized buffer filled with `v`.
fn block(v: u8) -> [u8; BS as usize] {
    [v; BS as usize]
}

/// A deterministic, distinct byte for each logical block (the array has at
/// most `NBLK * 2` = 16 logical blocks, so the pattern never wraps).
fn pat(lba: u64) -> u8 {
    0xA0 + u8::try_from(lba).expect("small test array")
}

/// Borrow the device behind a present member slot (a test bug if absent).
fn dev<'a>(array: &'a Raid10Array<'_, FaultBlock>, index: usize) -> &'a FaultBlock {
    array
        .member(index)
        .expect("member index in range")
        .device()
        .expect("present member slot holds a device")
}

/// Write a distinct pattern to every logical block of the array.
fn write_all(array: &mut Raid10Array<'_, FaultBlock>) {
    let blocks = array.array_geometry().block_count;
    for lba in 0..blocks {
        array.write_blocks(lba, &block(pat(lba))).expect("write ok");
    }
}

/// Assert every logical block reads back its written pattern.
fn assert_reads_all(array: &mut Raid10Array<'_, FaultBlock>) {
    let blocks = array.array_geometry().block_count;
    let mut buf = block(0);
    for lba in 0..blocks {
        array.read_blocks(lba, &mut buf).expect("read ok");
        assert!(buf.iter().all(|&b| b == pat(lba)), "block {lba} wrong");
    }
}

#[test]
fn four_healthy_members_assemble_optimal_and_round_trip() {
    let mut members = four(0);
    let mut array = Raid10Array::assemble(&mut members, CHUNK).expect("assembles");
    assert_eq!(array.health(), ArrayHealth::Optimal);
    assert_eq!(array.member_count(), 4);
    // Two pairs of NBLK-block members => half the members' summed capacity.
    assert_eq!(array.array_geometry().block_count, NBLK * 2);
    write_all(&mut array);
    assert_reads_all(&mut array);
    // Both copies of each pair received the write (mirror fan-out).
    for i in 0..4 {
        assert_eq!(array.member_state(i), Some(MemberState::InSync));
    }
}

#[test]
fn assemble_fails_closed_on_a_malformed_member_table() {
    let mut empty: [MirrorMember<FaultBlock>; 0] = [];
    assert_eq!(
        Raid10Array::assemble(&mut empty, CHUNK).err(),
        Some(Raid10Error::NoMembers)
    );
    let mut odd = [
        MirrorMember::new(FaultBlock::new(0)),
        MirrorMember::new(FaultBlock::new(0)),
        MirrorMember::new(FaultBlock::new(0)),
    ];
    assert_eq!(
        Raid10Array::assemble(&mut odd, CHUNK).err(),
        Some(Raid10Error::OddMembers)
    );
    let mut two = [
        MirrorMember::new(FaultBlock::new(0)),
        MirrorMember::new(FaultBlock::new(0)),
    ];
    assert_eq!(
        Raid10Array::assemble(&mut two, CHUNK).err(),
        Some(Raid10Error::TooFewMembers)
    );
    let mut zero = four(0);
    assert_eq!(
        Raid10Array::assemble(&mut zero, 0).err(),
        Some(Raid10Error::ZeroChunk)
    );
}

#[test]
fn assemble_rejects_a_geometry_mismatch() {
    let odd_geo = BlockGeometry {
        block_size: BS,
        block_count: NBLK + 2,
    };
    // Pair 0 is at the shared geometry; both copies of pair 1 are the odd
    // one, so pair 1 assembles cleanly at a geometry the array then rejects.
    let mut members = four(0);
    members[2] = MirrorMember::new(FaultBlock {
        geo: Cell::new(odd_geo),
        ..FaultBlock::new(0)
    });
    members[3] = MirrorMember::new(FaultBlock {
        geo: Cell::new(odd_geo),
        ..FaultBlock::new(0)
    });
    assert_eq!(
        Raid10Array::assemble(&mut members, CHUNK).err(),
        Some(Raid10Error::GeometryMismatch)
    );
}

#[test]
fn assemble_rejects_an_unaligned_member() {
    let ragged = BlockGeometry {
        block_size: BS,
        block_count: 7, // not a multiple of CHUNK (2)
    };
    let mut members = [
        MirrorMember::new(FaultBlock {
            geo: Cell::new(ragged),
            ..FaultBlock::new(0)
        }),
        MirrorMember::new(FaultBlock {
            geo: Cell::new(ragged),
            ..FaultBlock::new(0)
        }),
        MirrorMember::new(FaultBlock {
            geo: Cell::new(ragged),
            ..FaultBlock::new(0)
        }),
        MirrorMember::new(FaultBlock {
            geo: Cell::new(ragged),
            ..FaultBlock::new(0)
        }),
    ];
    assert_eq!(
        Raid10Array::assemble(&mut members, CHUNK).err(),
        Some(Raid10Error::UnalignedGeometry)
    );
}

#[test]
fn a_bad_sector_is_recovered_from_the_pair_copy_and_repaired() {
    let mut members = four(0);
    let mut array = Raid10Array::assemble(&mut members, CHUNK).expect("assembles");
    write_all(&mut array);
    // A per-block media error on copy 0 of pair 0 (logical block 0 lives on
    // pair 0, member-local block 0).
    dev(&array, 0)
        .read_fault
        .set(Some(DriverError::MediumError));
    let mut buf = block(0);
    array
        .read_blocks(0, &mut buf)
        .expect("recovered from copy 1");
    assert!(buf.iter().all(|&b| b == pat(0)));
    // The bad copy was repaired: with copy 1 now faulting, copy 0 serves.
    dev(&array, 1)
        .read_fault
        .set(Some(DriverError::DeviceOffline));
    array
        .read_blocks(0, &mut buf)
        .expect("repaired copy 0 serves");
    assert!(buf.iter().all(|&b| b == pat(0)));
}

#[test]
fn a_whole_device_fault_degrades_a_pair_while_the_survivor_serves() {
    let mut members = four(0);
    let mut array = Raid10Array::assemble(&mut members, CHUNK).expect("assembles");
    write_all(&mut array);
    dev(&array, 0)
        .read_fault
        .set(Some(DriverError::DeviceOffline));
    let mut buf = block(0);
    array.read_blocks(0, &mut buf).expect("survivor serves");
    assert!(buf.iter().all(|&b| b == pat(0)));
    assert_eq!(array.member_state(0), Some(MemberState::Faulted));
    assert_eq!(array.health(), ArrayHealth::Degraded);
    // Every other block still reads correctly.
    assert_reads_all(&mut array);
}

#[test]
fn a_pair_losing_both_copies_fails_closed_but_the_other_pair_serves() {
    let mut members = four(0);
    let mut array = Raid10Array::assemble(&mut members, CHUNK).expect("assembles");
    write_all(&mut array);
    // Both copies of pair 0 (members 0 and 1) go offline.
    dev(&array, 0)
        .read_fault
        .set(Some(DriverError::DeviceOffline));
    dev(&array, 1)
        .read_fault
        .set(Some(DriverError::DeviceOffline));
    let mut buf = block(0);
    // Logical block 0 lives on pair 0 -> fails closed.
    assert!(array.read_blocks(0, &mut buf).is_err());
    assert_eq!(array.health(), ArrayHealth::Failed);
    // Logical block 2 lives on pair 1 -> still served (head-of-line freedom).
    array
        .read_blocks(2, &mut buf)
        .expect("the healthy pair keeps serving");
    assert!(buf.iter().all(|&b| b == pat(2)));
}

#[test]
fn a_write_fans_out_and_drops_a_failing_copy_while_still_succeeding() {
    let mut members = four(0);
    let mut array = Raid10Array::assemble(&mut members, CHUNK).expect("assembles");
    // Copy 0 of pair 0 fails writes.
    dev(&array, 0)
        .write_fault
        .set(Some(DriverError::DeviceOffline));
    array
        .write_blocks(0, &block(pat(0)))
        .expect("write succeeds via the surviving copy");
    assert_eq!(array.member_state(0), Some(MemberState::Faulted));
    assert_eq!(array.member_state(1), Some(MemberState::InSync));
    // The survivor holds the data.
    let mut buf = block(0);
    dev(&array, 0)
        .read_fault
        .set(Some(DriverError::DeviceOffline));
    array.read_blocks(0, &mut buf).expect("survivor serves");
    assert!(buf.iter().all(|&b| b == pat(0)));
}

#[test]
fn a_fully_absent_pair_assembles_failed_and_fails_that_region_closed() {
    // Pair 0 present, pair 1 (members 2,3) both absent.
    let mut members = [
        MirrorMember::new(FaultBlock::new(0)),
        MirrorMember::new(FaultBlock::new(0)),
        MirrorMember::new(FaultBlock::absent()),
        MirrorMember::new(FaultBlock::absent()),
    ];
    let mut array = Raid10Array::assemble(&mut members, CHUNK).expect("assembles from pair 0");
    assert_eq!(array.array_geometry().block_count, NBLK * 2);
    assert_eq!(array.health(), ArrayHealth::Failed);
    // Pair 0 (logical block 0) still serves; pair 1 (logical block 2) fails closed.
    array
        .write_blocks(0, &block(pat(0)))
        .expect("pair 0 writes");
    let mut buf = block(0);
    array.read_blocks(0, &mut buf).expect("pair 0 reads");
    assert!(buf.iter().all(|&b| b == pat(0)));
    assert!(array.read_blocks(2, &mut buf).is_err());
}

#[test]
fn a_removed_member_is_replaced_and_rebuilt_with_current_data() {
    let mut members = four(0);
    let mut array = Raid10Array::assemble(&mut members, CHUNK).expect("assembles");
    write_all(&mut array);
    // Fault copy 0 of pair 0, then hot-swap it for a fresh spare.
    dev(&array, 0)
        .read_fault
        .set(Some(DriverError::DeviceOffline));
    let mut buf = block(0);
    array.read_blocks(0, &mut buf).expect("survivor serves");
    assert_eq!(array.member_state(0), Some(MemberState::Faulted));
    array
        .replace_member(0, FaultBlock::new(0xAA))
        .expect("replace faulted member");
    assert_eq!(array.member_state(0), Some(MemberState::Resyncing));
    assert_eq!(array.health(), ArrayHealth::Recovering);
    assert!(array.needs_resync());
    // Rebuild it, a chunk at a time, until in sync.
    let mut scratch = [0u8; BS as usize];
    let mut guard = 0;
    while array.needs_resync() {
        array.resync_step(&mut scratch).expect("resync step");
        guard += 1;
        assert!(guard < 1000, "resync should terminate");
    }
    assert_eq!(array.member_state(0), Some(MemberState::InSync));
    assert_eq!(array.health(), ArrayHealth::Optimal);
    // The rebuilt copy holds current data: with the survivor now faulting,
    // the rebuilt copy serves correctly.
    dev(&array, 1)
        .read_fault
        .set(Some(DriverError::DeviceOffline));
    assert_reads_all(&mut array);
}

#[test]
fn remove_then_add_is_the_full_disk_replacement_workflow() {
    let mut members = four(0);
    let mut array = Raid10Array::assemble(&mut members, CHUNK).expect("assembles");
    write_all(&mut array);
    // Fault copy 2 (pair 1, member-local 0 — the first read source of pair
    // 1), pull it, install a spare.
    dev(&array, 2)
        .read_fault
        .set(Some(DriverError::DeviceOffline));
    let mut buf = block(0);
    array
        .read_blocks(2, &mut buf)
        .expect("survivor serves pair 1");
    let pulled = array.remove_member(2).expect("vacate the faulted slot");
    // The returned device is the real faulted one (its read fault persists).
    assert!(pulled.read_fault.get().is_some());
    assert_eq!(array.member_state(2), Some(MemberState::Absent));
    assert_eq!(array.health(), ArrayHealth::Degraded);
    array
        .add_member(2, FaultBlock::new(0x55))
        .expect("install a spare into the absent slot");
    assert_eq!(array.member_state(2), Some(MemberState::Resyncing));
    let mut scratch = [0u8; 2 * BS as usize];
    while array.needs_resync() {
        array.resync_step(&mut scratch).expect("resync step");
    }
    assert_eq!(array.health(), ArrayHealth::Optimal);
    assert_reads_all(&mut array);
}

#[test]
fn member_ops_fail_closed_on_an_out_of_range_slot() {
    let mut members = four(0);
    let mut array = Raid10Array::assemble(&mut members, CHUNK).expect("assembles");
    assert_eq!(
        array.readd_member(9).err(),
        Some(Raid10Error::UnknownMember)
    );
    assert_eq!(
        array.remove_member(9).err(),
        Some(Raid10Error::UnknownMember)
    );
    assert_eq!(
        array.add_member(9, FaultBlock::new(0)).err(),
        Some(Raid10Error::UnknownMember)
    );
    assert_eq!(
        array.replace_member(9, FaultBlock::new(0)).err(),
        Some(Raid10Error::UnknownMember)
    );
    assert_eq!(array.member_state(9), None);
}

#[test]
fn a_scrub_finds_and_heals_a_latent_error_the_read_path_never_touches() {
    let mut members = four(0);
    let mut array = Raid10Array::assemble(&mut members, CHUNK).expect("assembles");
    write_all(&mut array);
    // A latent media error on copy 1 of pair 0 (never the read source, since
    // the read path serves from copy 0 first).
    dev(&array, 1)
        .read_fault
        .set(Some(DriverError::MediumError));
    // A full scrub pass reads and repairs every copy of every block.
    let mut scratch = [0u8; BS as usize];
    array.begin_scrub();
    let mut guard = 0;
    while array.scrubbing() {
        array.scrub_step(&mut scratch).expect("scrub step");
        guard += 1;
        assert!(guard < 1000, "scrub should terminate");
    }
    // The latent error on copy 1 was healed: with copy 0 now faulting, copy 1
    // serves pair 0's blocks correctly.
    dev(&array, 0)
        .read_fault
        .set(Some(DriverError::DeviceOffline));
    let mut buf = block(0);
    array
        .read_blocks(0, &mut buf)
        .expect("healed copy 1 serves");
    assert!(buf.iter().all(|&b| b == pat(0)));
}

#[test]
fn scrub_on_a_failed_array_fails_closed_without_advancing() {
    let mut members = four(0);
    let mut array = Raid10Array::assemble(&mut members, CHUNK).expect("assembles");
    write_all(&mut array);
    // Kill both copies of pair 0.
    dev(&array, 0)
        .read_fault
        .set(Some(DriverError::DeviceOffline));
    dev(&array, 1)
        .read_fault
        .set(Some(DriverError::DeviceOffline));
    let mut buf = block(0);
    let _ = array.read_blocks(0, &mut buf);
    assert_eq!(array.health(), ArrayHealth::Failed);
    array.begin_scrub();
    let before = array.scrub_cursor();
    assert_eq!(
        array.scrub_step(&mut block(0)).err(),
        Some(DriverError::DeviceOffline)
    );
    assert_eq!(array.scrub_cursor(), before);
}

#[test]
fn device_health_aggregates_live_members() {
    let mut members = [
        MirrorMember::new(FaultBlock::new(0).with_media_errors(2)),
        MirrorMember::new(FaultBlock::new(0).with_media_errors(3)),
        MirrorMember::new(FaultBlock::new(0).with_media_errors(5)),
        MirrorMember::new(FaultBlock::new(0).with_media_errors(7)),
    ];
    let array = Raid10Array::assemble(&mut members, CHUNK).expect("assembles");
    let DeviceHealth::Available(snap) = array.device_health().expect("health") else {
        panic!("expected aggregated telemetry");
    };
    // Independent integrity faults sum across every live member.
    assert_eq!(snap.media_errors, 2 + 3 + 5 + 7);
}

#[test]
fn a_dropped_member_no_longer_speaks_for_the_array() {
    // A member the array has faulted out is no longer a device the array is
    // driving, so its telemetry must stop counting — exactly as in every
    // sibling composition, which all select through the one shared
    // participation predicate.
    let mut members = [
        MirrorMember::new(FaultBlock::new(0).with_media_errors(2)),
        MirrorMember::new(FaultBlock::new(0).with_media_errors(3)),
        MirrorMember::new(FaultBlock::new(0).with_media_errors(5)),
        MirrorMember::new(FaultBlock::new(0).with_media_errors(7)),
    ];
    let mut array = Raid10Array::assemble(&mut members, CHUNK).expect("assembles");
    write_all(&mut array);
    // Drop copy 0 of pair 0 through a whole-device read fault.
    dev(&array, 0)
        .read_fault
        .set(Some(DriverError::DeviceOffline));
    let mut buf = block(0);
    array.read_blocks(0, &mut buf).expect("survivor serves");
    assert_eq!(array.member_state(0), Some(MemberState::Faulted));

    let DeviceHealth::Available(snap) = array.device_health().expect("health") else {
        panic!("expected aggregated telemetry");
    };
    assert_eq!(snap.media_errors, 3 + 5 + 7);
}

#[test]
fn device_class_declares_the_most_patient_live_member() {
    // The array can only answer as fast as the member it waits on, so one
    // spinning disk earns the whole array a spinning disk's patience.
    let mut members = [
        MirrorMember::new(FaultBlock::new(0)),
        MirrorMember::new(FaultBlock::new(0).with_class(BlkDeviceClass::Rotational)),
        MirrorMember::new(FaultBlock::new(0)),
        MirrorMember::new(FaultBlock::new(0)),
    ];
    let mut array = Raid10Array::assemble(&mut members, CHUNK).expect("assembles");
    assert_eq!(array.device_class(), BlkDeviceClass::Rotational);

    // Once that member is dropped, the array is no longer waiting on it and
    // stops claiming its patience. A write fans out to both copies, so a
    // write fault drops the slow copy while its mirror still commits.
    dev(&array, 1)
        .write_fault
        .set(Some(DriverError::DeviceOffline));
    array
        .write_blocks(0, &block(pat(0)))
        .expect("the surviving copy commits");
    assert_eq!(array.member_state(1), Some(MemberState::Faulted));
    assert_eq!(array.device_class(), BlkDeviceClass::SolidState);
}

#[test]
fn a_malformed_request_is_a_request_error() {
    let mut members = four(0);
    let mut array = Raid10Array::assemble(&mut members, CHUNK).expect("assembles");
    // A buffer that is not a block multiple.
    let mut ragged = [0u8; BS as usize + 1];
    assert_eq!(
        array.read_blocks(0, &mut ragged).err(),
        Some(DriverError::BufferTooSmall)
    );
    // A request past the end of the array.
    let mut buf = block(0);
    assert_eq!(
        array
            .read_blocks(array.array_geometry().block_count, &mut buf)
            .err(),
        Some(DriverError::LengthOutOfRange)
    );
}

#[test]
fn the_class_carrying_read_and_write_thread_the_sensitivity_class() {
    let mut members = four(0);
    let mut array = Raid10Array::assemble(&mut members, CHUNK).expect("assembles");
    let mut buf = block(0);
    array
        .write_blocks_with_class(0, &block(pat(0)), BufferClass::Sensitive)
        .expect("class write");
    array
        .read_blocks_with_class(0, &mut buf, BufferClass::Sensitive)
        .expect("class read");
    assert!(buf.iter().all(|&b| b == pat(0)));
}

#[test]
fn a_verification_pass_resumes_where_a_restart_left_it() {
    // A pass over a 100 TB+ array runs for hours, so it will meet a restart.
    // Losing the cursor would restart the pass every time, and an array
    // rebooted often enough would never finish verifying itself at all.
    let mut members = four(0);
    let mut scrub_scratch = [0u8; BS as usize];
    let checkpoint = {
        let mut array = Raid10Array::assemble(&mut members, CHUNK).expect("assembles");
        array.begin_scrub();
        array
            .scrub_step(&mut scrub_scratch)
            .expect("one scrub chunk");
        assert!(array.scrubbing());
        let checkpoint = array.progress();
        assert_eq!(checkpoint.scrub_cursor, Some(array.scrub_cursor()));
        assert_eq!(checkpoint.resync_cursor, None);
        checkpoint
    };

    // The serving process restarts and the array is assembled afresh, which by
    // itself abandons the pass; the checkpointed position resumes it.
    let mut array = Raid10Array::assemble(&mut members, CHUNK).expect("re-assembles");
    assert!(!array.scrubbing(), "a fresh assembly is not mid-pass");
    assert_eq!(array.progress(), ArrayProgress::IDLE);
    array
        .restore_progress(checkpoint)
        .expect("the checkpointed position is adopted");
    assert!(array.scrubbing());
    assert_eq!(Some(array.scrub_cursor()), checkpoint.scrub_cursor);

    let mut steps = 0u32;
    while array.scrubbing() {
        array.scrub_step(&mut scrub_scratch).expect("scrub chunk");
        steps += 1;
        assert!(steps <= 100, "the pass terminates");
    }
}

#[test]
fn a_restored_cursor_outside_the_array_is_refused_and_changes_nothing() {
    // A cursor past the end cannot have come from this array. Adopted as a
    // rebuild position it would mark a member fully rebuilt without its tail
    // ever being written, leaving stale data trusted as current — so it is
    // refused outright rather than clamped.
    let mut members = four(0);
    let mut array = Raid10Array::assemble(&mut members, CHUNK).expect("assembles");
    for cursor in [NBLK, NBLK + 1, u64::MAX] {
        assert_eq!(
            array.restore_progress(ArrayProgress {
                scrub_cursor: Some(cursor),
                resync_cursor: None,
            }),
            Err(Raid10Error::CursorOutOfRange)
        );
        assert_eq!(
            array.restore_progress(ArrayProgress {
                scrub_cursor: None,
                resync_cursor: Some(cursor),
            }),
            Err(Raid10Error::CursorOutOfRange)
        );
    }
    assert!(!array.scrubbing());
    assert_eq!(array.progress(), ArrayProgress::IDLE);

    // The last real block is accepted, so the refusal is exactly at the end
    // and not one block early.
    array
        .restore_progress(ArrayProgress {
            scrub_cursor: Some(NBLK - 1),
            resync_cursor: None,
        })
        .expect("the last block is a valid position");
    assert_eq!(array.scrub_cursor(), NBLK - 1);
}

#[test]
fn member_device_mut_reaches_the_named_slots_own_device() {
    let mut members = [
        MirrorMember::new(FaultBlock::new(0)),
        MirrorMember::new(FaultBlock::new(0)),
        MirrorMember::new(FaultBlock::new(0)),
        MirrorMember::absent(),
    ];
    let mut array = Raid10Array::assemble(&mut members, CHUNK).expect("assembles degraded");

    // The borrowed device is the member's whole disk, below the array's data
    // view: a write through it lands on that copy alone, which is how a
    // caller reaches a member's reserved array-metadata blocks.
    let bs = BS as usize;
    array
        .member_device_mut(2)
        .expect("slot 2 holds a device")
        .write_blocks(1, &block(0x5A))
        .expect("the member's own write");
    assert_eq!(&dev(&array, 2).store[bs..2 * bs], &block(0x5A));
    assert_eq!(
        &dev(&array, 0).store[bs..2 * bs],
        &block(0),
        "the write reached only the named slot's device"
    );

    assert!(
        array.member_device_mut(3).is_none(),
        "an absent slot holds no device"
    );
    assert!(
        array.member_device_mut(4).is_none(),
        "an index outside the array has no slot"
    );
}
