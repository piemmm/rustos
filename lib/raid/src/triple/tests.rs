//! Host tests for the RAID-TP triple-parity array over a fault-injecting
//! [`Block`] double.

use super::{TripleParityArray, TripleParityError, TripleParityMember, SCRATCH_BLOCKS};
use crate::superblock::ArrayProgress;
use core::cell::RefCell;
use tairix_abi::driver::block::{Block, BlockGeometry, DeviceHealth, HealthSnapshot};
use tairix_abi::driver::{BufferClass, DriverError};
use tairix_abi::raid::{ArrayHealth, MemberState};

// A small block size keeps the fault-injecting device doubles off the stack's
// large-array lint while the array's byte-wise GF math is size-agnostic.
const BS: u32 = 64;
/// Logical blocks per member.
const MB: u64 = 8;
/// One member's byte capacity (`BS * MB`).
const CAP: usize = 64 * 8;
/// The stripe unit used across the tests, in logical blocks.
const CHUNK: u32 = 2;
/// The number of members in the standard test array (2 data + P + Q + R).
const MEMBERS: usize = 5;
/// The array's usable logical block count (`MB * (MEMBERS - 3)`).
const LOGICAL: u64 = MB * (MEMBERS as u64 - 3);

/// An in-memory block device with injectable faults. Interior mutability keeps
/// the whole device behind `&mut self` methods while a test flips faults
/// through the shared borrow [`TripleParityArray::member`] hands back.
struct MemBlock {
    inner: RefCell<Inner>,
}

struct Inner {
    store: [u8; CAP],
    present: bool,
    read_fault: Option<DriverError>,
    write_fault: Option<DriverError>,
    /// A single member-local block that returns [`DriverError::MediumError`]
    /// on read — a latent bad sector, not a dead device.
    medium_block: Option<u64>,
    /// The [`BufferClass`] of the most recent `write_blocks_with_class` this
    /// member observed, so a test can prove the caller's class is forwarded.
    last_write_class: Option<BufferClass>,
    /// The health telemetry this member reports.
    health: DeviceHealth,
}

impl MemBlock {
    fn new() -> Self {
        Self {
            inner: RefCell::new(Inner {
                store: [0u8; CAP],
                present: true,
                read_fault: None,
                write_fault: None,
                medium_block: None,
                last_write_class: None,
                health: DeviceHealth::Unavailable,
            }),
        }
    }

    /// Make this member report `media_errors` integrity faults through its
    /// health telemetry.
    fn set_media_errors(&self, media_errors: u64) {
        self.inner.borrow_mut().health = DeviceHealth::Available(HealthSnapshot {
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
        });
    }

    fn set_read_fault(&self, e: Option<DriverError>) {
        self.inner.borrow_mut().read_fault = e;
    }

    fn set_write_fault(&self, e: Option<DriverError>) {
        self.inner.borrow_mut().write_fault = e;
    }

    fn set_medium_block(&self, b: Option<u64>) {
        self.inner.borrow_mut().medium_block = b;
    }

    /// The [`BufferClass`] of the most recent `write_blocks_with_class` this
    /// member observed (`None` if it was never written through that path).
    fn last_write_class(&self) -> Option<BufferClass> {
        self.inner.borrow().last_write_class
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

impl Block for MemBlock {
    fn geometry(&self) -> Result<BlockGeometry, DriverError> {
        if self.inner.borrow().present {
            Ok(BlockGeometry {
                block_size: BS,
                block_count: MB,
            })
        } else {
            Err(DriverError::DeviceOffline)
        }
    }

    fn read_blocks(&mut self, lba: u64, buf: &mut [u8]) -> Result<(), DriverError> {
        let inner = self.inner.borrow();
        if let Some(e) = inner.read_fault {
            return Err(e);
        }
        let (start, end) = Self::span(lba, buf.len())?;
        if let Some(bad) = inner.medium_block {
            let blocks = (buf.len() / BS as usize) as u64;
            if bad >= lba && bad < lba + blocks {
                return Err(DriverError::MediumError);
            }
        }
        buf.copy_from_slice(&inner.store[start..end]);
        Ok(())
    }

    fn write_blocks(&mut self, lba: u64, buf: &[u8]) -> Result<(), DriverError> {
        let mut inner = self.inner.borrow_mut();
        if let Some(e) = inner.write_fault {
            return Err(e);
        }
        let (start, end) = Self::span(lba, buf.len())?;
        // A successful write to a bad-sector block models the device
        // reallocating the sector: the latent media error is cleared.
        if let Some(bad) = inner.medium_block {
            let blocks = (buf.len() / BS as usize) as u64;
            if bad >= lba && bad < lba + blocks {
                inner.medium_block = None;
            }
        }
        inner.store[start..end].copy_from_slice(buf);
        Ok(())
    }

    fn flush(&mut self) -> Result<(), DriverError> {
        Ok(())
    }

    fn write_blocks_with_class(
        &mut self,
        lba: u64,
        buf: &[u8],
        class: BufferClass,
    ) -> Result<(), DriverError> {
        self.inner.borrow_mut().last_write_class = Some(class);
        self.write_blocks(lba, buf)
    }

    fn device_health(&self) -> Result<DeviceHealth, DriverError> {
        Ok(self.inner.borrow().health)
    }
}

/// A five-member table of fresh empty members.
fn members() -> [TripleParityMember<MemBlock>; MEMBERS] {
    [
        TripleParityMember::new(MemBlock::new()),
        TripleParityMember::new(MemBlock::new()),
        TripleParityMember::new(MemBlock::new()),
        TripleParityMember::new(MemBlock::new()),
        TripleParityMember::new(MemBlock::new()),
    ]
}

/// A scratch buffer of the minimum required blocks.
fn scratch() -> [u8; SCRATCH_BLOCKS * 512] {
    [0u8; SCRATCH_BLOCKS * 512]
}

/// A distinct, non-zero byte per logical block, so a mis-mapped chunk or a
/// wrong reconstruction is caught. Every byte of the block is the same value.
fn val(blk: u64) -> u8 {
    let b = u8::try_from(blk % 256).unwrap();
    b.wrapping_mul(37).wrapping_add(11)
}

/// Write logical block `blk` filled with `val(blk)`.
fn put(array: &mut TripleParityArray<'_, MemBlock>, blk: u64) {
    let buf = [val(blk); BS as usize];
    array.write_blocks(blk, &buf).unwrap();
}

/// Read logical block `blk` and assert it holds `val(blk)`.
fn expect(array: &mut TripleParityArray<'_, MemBlock>, blk: u64) {
    let mut buf = [0u8; BS as usize];
    array.read_blocks(blk, &mut buf).unwrap();
    assert_eq!(buf, [val(blk); BS as usize], "logical block {blk}");
}

/// Fill the whole array with the per-block pattern.
fn fill(array: &mut TripleParityArray<'_, MemBlock>) {
    for blk in 0..LOGICAL {
        put(array, blk);
    }
}

/// Read and verify every logical block.
fn expect_all(array: &mut TripleParityArray<'_, MemBlock>) {
    for blk in 0..LOGICAL {
        expect(array, blk);
    }
}

/// Fault member `idx` by injecting a whole-device read error and touching the
/// array so the fault is observed, then clearing the injection so the member
/// is simply "gone" from the array's point of view.
fn fault_member(array: &mut TripleParityArray<'_, MemBlock>, idx: usize) {
    array
        .member(idx)
        .unwrap()
        .device()
        .unwrap()
        .set_read_fault(Some(DriverError::DeviceOffline));
    for blk in 0..LOGICAL {
        let mut buf = [0u8; BS as usize];
        let _ = array.read_blocks(blk, &mut buf);
    }
    assert_eq!(array.member_state(idx), Some(MemberState::Faulted));
    array
        .member(idx)
        .unwrap()
        .device()
        .unwrap()
        .set_read_fault(None);
}

#[test]
fn five_healthy_members_assemble_optimal_and_round_trip() {
    let mut m = members();
    let mut s = scratch();
    let mut array = TripleParityArray::assemble(&mut m, &mut s, CHUNK).unwrap();
    assert_eq!(array.health(), ArrayHealth::Optimal);
    assert_eq!(array.array_geometry().block_count, LOGICAL);
    fill(&mut array);
    expect_all(&mut array);
}

#[test]
fn survives_any_single_member_loss() {
    for lost in 0..MEMBERS {
        let mut m = members();
        let mut s = scratch();
        let mut array = TripleParityArray::assemble(&mut m, &mut s, CHUNK).unwrap();
        fill(&mut array);
        fault_member(&mut array, lost);
        assert_eq!(array.health(), ArrayHealth::Degraded);
        expect_all(&mut array);
    }
}

#[test]
fn survives_any_two_member_losses() {
    for a in 0..MEMBERS {
        for b in (a + 1)..MEMBERS {
            let mut m = members();
            let mut s = scratch();
            let mut array = TripleParityArray::assemble(&mut m, &mut s, CHUNK).unwrap();
            fill(&mut array);
            fault_member(&mut array, a);
            fault_member(&mut array, b);
            assert_eq!(array.health(), ArrayHealth::Degraded, "lost {a},{b}");
            expect_all(&mut array);
        }
    }
}

#[test]
fn survives_any_three_member_losses() {
    for a in 0..MEMBERS {
        for b in (a + 1)..MEMBERS {
            for c in (b + 1)..MEMBERS {
                let mut m = members();
                let mut s = scratch();
                let mut array = TripleParityArray::assemble(&mut m, &mut s, CHUNK).unwrap();
                fill(&mut array);
                fault_member(&mut array, a);
                fault_member(&mut array, b);
                fault_member(&mut array, c);
                assert_eq!(array.health(), ArrayHealth::Degraded, "lost {a},{b},{c}");
                expect_all(&mut array);
            }
        }
    }
}

#[test]
fn a_fourth_loss_fails_the_array_closed() {
    let mut m = members();
    let mut s = scratch();
    let mut array = TripleParityArray::assemble(&mut m, &mut s, CHUNK).unwrap();
    fill(&mut array);
    fault_member(&mut array, 0);
    fault_member(&mut array, 1);
    fault_member(&mut array, 2);
    fault_member(&mut array, 3);
    assert_eq!(array.health(), ArrayHealth::Failed);
    let mut buf = [0u8; BS as usize];
    assert_eq!(
        array.read_blocks(0, &mut buf),
        Err(DriverError::DeviceOffline),
        "a four-loss stripe must never fabricate data"
    );
}

#[test]
fn a_media_error_is_reconstructed_and_repaired() {
    let mut m = members();
    let mut s = scratch();
    let mut array = TripleParityArray::assemble(&mut m, &mut s, CHUNK).unwrap();
    fill(&mut array);
    // A latent bad sector on the data member that holds logical block 0
    // (member 2's block 0 in this layout): reading it reconstructs the data
    // from the survivors and repairs the sector in place.
    array
        .member(2)
        .unwrap()
        .device()
        .unwrap()
        .set_medium_block(Some(0));
    expect_all(&mut array);
    // The member stays a healthy source (a media error is not a device fault).
    assert_eq!(array.member_state(2), Some(MemberState::InSync));
    assert_eq!(array.health(), ArrayHealth::Optimal);
    // The repair cleared the bad sector.
    assert_eq!(
        array
            .member(2)
            .unwrap()
            .device()
            .unwrap()
            .inner
            .borrow()
            .medium_block,
        None
    );
}

#[test]
fn a_degraded_write_keeps_a_lost_members_data_reconstructable() {
    let mut m = members();
    let mut s = scratch();
    let mut array = TripleParityArray::assemble(&mut m, &mut s, CHUNK).unwrap();
    fault_member(&mut array, 1);
    fault_member(&mut array, 3);
    assert_eq!(array.health(), ArrayHealth::Degraded);
    // Writing with two members gone must still encode the data into the
    // surviving syndromes so it reads back correctly.
    fill(&mut array);
    expect_all(&mut array);
    // A third loss after the degraded write still reconstructs.
    fault_member(&mut array, 4);
    expect_all(&mut array);
}

#[test]
fn a_scrub_heals_a_latent_error_the_read_path_never_touches() {
    let mut m = members();
    let mut s = scratch();
    let mut array = TripleParityArray::assemble(&mut m, &mut s, CHUNK).unwrap();
    fill(&mut array);
    // A latent bad sector on a member's own copy that a plain read of the
    // logical space may never consult (a syndrome member's block); a scrub
    // reads every member and heals it.
    array
        .member(4)
        .unwrap()
        .device()
        .unwrap()
        .set_medium_block(Some(1));
    array.begin_scrub();
    // Two member-local blocks per step: bounded and interruptible.
    while array.scrubbing() {
        array.scrub_step(2).unwrap();
    }
    assert_eq!(
        array
            .member(4)
            .unwrap()
            .device()
            .unwrap()
            .inner
            .borrow()
            .medium_block,
        None,
        "the scrub must have healed the latent sector"
    );
    assert_eq!(array.health(), ArrayHealth::Optimal);
    expect_all(&mut array);
}

#[test]
fn a_returning_member_is_rebuilt_incrementally_with_current_data() {
    let mut m = members();
    let mut s = scratch();
    let mut array = TripleParityArray::assemble(&mut m, &mut s, CHUNK).unwrap();
    fill(&mut array);
    fault_member(&mut array, 2);
    // A write while the member is gone must reach the rebuilt copy once it
    // resyncs, so the rebuild carries *current* data.
    put(&mut array, 0);
    array.readd_member(2).unwrap();
    assert_eq!(array.member_state(2), Some(MemberState::Resyncing));
    assert_eq!(array.health(), ArrayHealth::Recovering);
    // Rebuild one block per step: bounded and interruptible.
    let mut steps = 0;
    while array.needs_resync() {
        array.resync_step(1).unwrap();
        steps += 1;
        assert!(steps <= MB + 1, "resync must terminate");
    }
    assert!(steps > 1, "the rebuild must be incremental, not one sweep");
    assert_eq!(array.member_state(2), Some(MemberState::InSync));
    assert_eq!(array.health(), ArrayHealth::Optimal);
    expect_all(&mut array);
}

#[test]
fn the_remove_add_rebuild_replacement_cycle_restores_redundancy() {
    let mut m = members();
    let mut s = scratch();
    let mut array = TripleParityArray::assemble(&mut m, &mut s, CHUNK).unwrap();
    fill(&mut array);
    fault_member(&mut array, 1);
    // Pull the failed disk: its slot becomes absent, the device is returned.
    let _pulled = array.remove_member(1).unwrap();
    assert_eq!(array.member_state(1), Some(MemberState::Absent));
    assert_eq!(array.health(), ArrayHealth::Degraded);
    // A live member cannot be removed.
    assert!(matches!(
        array.remove_member(0),
        Err(TripleParityError::NotFaulted)
    ));
    // Install a spare into the empty slot and rebuild it.
    array.add_member(1, MemBlock::new()).unwrap();
    assert_eq!(array.member_state(1), Some(MemberState::Resyncing));
    while array.needs_resync() {
        array.resync_step(4).unwrap();
    }
    assert_eq!(array.health(), ArrayHealth::Optimal);
    expect_all(&mut array);
    // A spare cannot be added to an occupied slot.
    assert_eq!(
        array.add_member(0, MemBlock::new()),
        Err(TripleParityError::SlotOccupied)
    );
}

#[test]
fn a_missing_member_slot_assembles_degraded_not_optimal() {
    let mut m = [
        TripleParityMember::new(MemBlock::new()),
        TripleParityMember::absent(),
        TripleParityMember::new(MemBlock::new()),
        TripleParityMember::new(MemBlock::new()),
        TripleParityMember::new(MemBlock::new()),
    ];
    let mut s = scratch();
    let mut array = TripleParityArray::assemble(&mut m, &mut s, CHUNK).unwrap();
    assert_eq!(array.member_count(), MEMBERS);
    assert_eq!(array.member_state(1), Some(MemberState::Absent));
    assert_eq!(array.health(), ArrayHealth::Degraded);
    fill(&mut array);
    expect_all(&mut array);
}

#[test]
fn assemble_fails_closed_on_too_few_members() {
    let mut m = [
        TripleParityMember::new(MemBlock::new()),
        TripleParityMember::new(MemBlock::new()),
        TripleParityMember::new(MemBlock::new()),
        TripleParityMember::new(MemBlock::new()),
    ];
    let mut s = scratch();
    assert_eq!(
        TripleParityArray::assemble(&mut m, &mut s, CHUNK).map(|_| ()),
        Err(TripleParityError::TooFewMembers)
    );
}

#[test]
fn assemble_fails_closed_on_four_lost_members() {
    // Only one present member: four absent slots exceed the three-fault
    // redundancy, so the array cannot serve and fails closed at assembly.
    let mut m = [
        TripleParityMember::new(MemBlock::new()),
        TripleParityMember::absent(),
        TripleParityMember::absent(),
        TripleParityMember::absent(),
        TripleParityMember::absent(),
    ];
    let mut s = scratch();
    assert_eq!(
        TripleParityArray::assemble(&mut m, &mut s, CHUNK).map(|_| ()),
        Err(TripleParityError::InsufficientRedundancy)
    );
}

#[test]
fn assemble_fails_closed_on_a_too_small_scratch() {
    let mut m = members();
    let mut s = [0u8; 4 * 64];
    assert_eq!(
        TripleParityArray::assemble(&mut m, &mut s, CHUNK).map(|_| ()),
        Err(TripleParityError::ScratchTooSmall)
    );
}

#[test]
fn a_data_member_write_honours_the_caller_class_while_syndromes_stay_sensitive() {
    let mut m = members();
    let mut s = scratch();
    let mut array = TripleParityArray::assemble(&mut m, &mut s, CHUNK).unwrap();
    // One NonSensitive single-block write touches its data member (caller's
    // class) plus the P, Q, and R members (Sensitive); the other data member
    // is untouched.
    let buf = [val(0); BS as usize];
    array
        .write_blocks_with_class(0, &buf, BufferClass::NonSensitive)
        .unwrap();
    let classes: [Option<BufferClass>; MEMBERS] = core::array::from_fn(|i| {
        array
            .member(i)
            .unwrap()
            .device()
            .unwrap()
            .last_write_class()
    });
    let nonsensitive = classes
        .iter()
        .filter(|c| **c == Some(BufferClass::NonSensitive))
        .count();
    let sensitive = classes
        .iter()
        .filter(|c| **c == Some(BufferClass::Sensitive))
        .count();
    assert_eq!(
        nonsensitive, 1,
        "the data member must honour the caller's NonSensitive class"
    );
    assert_eq!(
        sensitive, 3,
        "the P, Q, and R writes must stay Sensitive (opaque cross-stripe bytes)"
    );
}

#[test]
fn a_write_error_drops_a_member_while_the_write_still_succeeds() {
    let mut m = members();
    let mut s = scratch();
    let mut array = TripleParityArray::assemble(&mut m, &mut s, CHUNK).unwrap();
    fill(&mut array);
    // Member 4 rejects every write: it is dropped, but the write still lands
    // durably in the surviving members and syndromes.
    array
        .member(4)
        .unwrap()
        .device()
        .unwrap()
        .set_write_fault(Some(DriverError::DeviceOffline));
    put(&mut array, 0);
    array
        .member(4)
        .unwrap()
        .device()
        .unwrap()
        .set_write_fault(None);
    expect_all(&mut array);
}

#[test]
fn device_health_aggregates_live_members() {
    let mut m = members();
    let mut s = scratch();
    let array = TripleParityArray::assemble(&mut m, &mut s, CHUNK).unwrap();
    for i in 0..MEMBERS {
        array
            .member(i)
            .unwrap()
            .device()
            .unwrap()
            .set_media_errors(2);
    }
    match array.device_health().unwrap() {
        DeviceHealth::Available(snap) => {
            assert_eq!(snap.media_errors, 2 * MEMBERS as u64, "counters sum");
        }
        DeviceHealth::Unavailable => panic!("live members expose telemetry"),
    }
}

#[test]
fn a_verification_pass_resumes_where_a_restart_left_it() {
    // A pass over a 100 TB+ array runs for hours, so it will meet a restart.
    // Losing the cursor would restart the pass every time, and an array
    // rebooted often enough would never finish verifying itself at all.
    let checkpoint = {
        let mut m = members();
        let mut s = scratch();
        let mut array = TripleParityArray::assemble(&mut m, &mut s, CHUNK).expect("assembles");
        array.begin_scrub();
        array.scrub_step(1).expect("one scrub chunk");
        assert!(array.scrubbing());
        let checkpoint = array.progress();
        assert_eq!(checkpoint.scrub_cursor, Some(array.scrub_cursor()));
        assert_eq!(checkpoint.resync_cursor, None);
        checkpoint
    };

    // The serving process restarts and the array is assembled afresh, which by
    // itself abandons the pass; the checkpointed position resumes it.
    let mut m = members();
    let mut s = scratch();
    let mut array = TripleParityArray::assemble(&mut m, &mut s, CHUNK).expect("re-assembles");
    assert!(!array.scrubbing(), "a fresh assembly is not mid-pass");
    assert_eq!(array.progress(), ArrayProgress::IDLE);
    array
        .restore_progress(checkpoint)
        .expect("the checkpointed position is adopted");
    assert!(array.scrubbing());
    assert_eq!(Some(array.scrub_cursor()), checkpoint.scrub_cursor);

    let mut steps = 0u32;
    while array.scrubbing() {
        array.scrub_step(1).expect("scrub chunk");
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
    let mut m = members();
    let mut s = scratch();
    let mut array = TripleParityArray::assemble(&mut m, &mut s, CHUNK).expect("assembles");
    for cursor in [MB, MB + 1, u64::MAX] {
        assert_eq!(
            array.restore_progress(ArrayProgress {
                scrub_cursor: Some(cursor),
                resync_cursor: None,
            }),
            Err(TripleParityError::CursorOutOfRange)
        );
        assert_eq!(
            array.restore_progress(ArrayProgress {
                scrub_cursor: None,
                resync_cursor: Some(cursor),
            }),
            Err(TripleParityError::CursorOutOfRange)
        );
    }
    assert!(!array.scrubbing());
    assert_eq!(array.progress(), ArrayProgress::IDLE);

    // The last real block is accepted, so the refusal is exactly at the end
    // and not one block early.
    array
        .restore_progress(ArrayProgress {
            scrub_cursor: Some(MB - 1),
            resync_cursor: None,
        })
        .expect("the last block is a valid position");
    assert_eq!(array.scrub_cursor(), MB - 1);
}

#[test]
fn member_device_mut_reaches_the_named_members_own_device() {
    let mut m = [
        TripleParityMember::new(MemBlock::new()),
        TripleParityMember::absent(),
        TripleParityMember::new(MemBlock::new()),
        TripleParityMember::new(MemBlock::new()),
        TripleParityMember::new(MemBlock::new()),
    ];
    let mut s = scratch();
    let mut array = TripleParityArray::assemble(&mut m, &mut s, CHUNK).unwrap();

    // The borrowed device is the member's whole disk, below the array's data
    // view: a write through it lands on that member alone, which is how a
    // caller reaches a member's reserved array-metadata blocks.
    array
        .member_device_mut(2)
        .expect("slot 2 holds a device")
        .write_blocks(3, &[0x5A; BS as usize])
        .unwrap();
    let mut buf = [0u8; BS as usize];
    array
        .member_device_mut(2)
        .unwrap()
        .read_blocks(3, &mut buf)
        .unwrap();
    assert_eq!(buf, [0x5A; BS as usize]);
    array
        .member_device_mut(0)
        .unwrap()
        .read_blocks(3, &mut buf)
        .unwrap();
    assert_eq!(
        buf, [0u8; BS as usize],
        "the write reached only the named member's device"
    );

    assert!(
        array.member_device_mut(1).is_none(),
        "an absent slot holds no device"
    );
    assert!(
        array.member_device_mut(MEMBERS).is_none(),
        "an index outside the array has no slot"
    );
}
