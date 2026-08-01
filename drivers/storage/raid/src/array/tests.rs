//! Host tests for the unified [`RaidArray`] composed-device dispatch.
//!
//! These prove the dispatch layer forwards to the right engine and applies the
//! honest level-agnostic surface (`AGENTS.md` §27): a stripe fails
//! redundancy-only operations closed, the redundant levels forward
//! maintenance, and the [`Block`] I/O path reaches the inner engine (including
//! through a `&mut dyn Block`). The engines' own recovery behaviour is proven
//! in their sibling test modules; here we test only the composition.

use super::{RaidArray, RaidError};
use crate::dualparity::DualParityError;
use crate::mirror::{ArrayHealth, MemberState, MirrorArray, MirrorError, MirrorMember};
use crate::parity::{ParityArray, ParityError, ParityMember};
use crate::raid10::{Raid10Array, Raid10Error};
use crate::stripe::{StripeArray, StripeMember};
use crate::superblock::{ArrayProgress, RaidLevel};
use crate::triple::TripleParityError;
use tairix_abi::driver::block::{Block, BlockGeometry, DeviceHealth};
use tairix_abi::driver::DriverError;

const BS: u32 = 64;
/// Logical blocks per backing member device.
const MB: u64 = 8;
/// One member's byte capacity (`BS * MB`). Kept small so the test doubles'
/// backing arrays stay well under clippy's large-stack-array threshold even
/// for the four-member RAID10 dispatch case.
const CAP: usize = 64 * 8;
/// The stripe unit for the striped-level arrays, in logical blocks.
const CHUNK: u32 = 2;

/// A plain in-memory block device. No fault injection is needed: the dispatch
/// layer's correctness is independent of member faults, which the engines'
/// own tests cover.
struct RamBlock {
    store: [u8; CAP],
}

impl RamBlock {
    const fn new(fill: u8) -> Self {
        Self { store: [fill; CAP] }
    }

    fn range(lba: u64, len: usize) -> Result<(usize, usize), DriverError> {
        let bs = BS as usize;
        if len == 0 || !len.is_multiple_of(bs) {
            return Err(DriverError::BufferTooSmall);
        }
        let blocks = (len / bs) as u64;
        let end = lba
            .checked_add(blocks)
            .ok_or(DriverError::LengthOutOfRange)?;
        if end > MB {
            return Err(DriverError::LengthOutOfRange);
        }
        let start = usize::try_from(lba).map_err(|_| DriverError::LengthOutOfRange)? * bs;
        Ok((start, start + len))
    }
}

impl Block for RamBlock {
    fn geometry(&self) -> Result<BlockGeometry, DriverError> {
        Ok(BlockGeometry {
            block_size: BS,
            block_count: MB,
        })
    }

    fn read_blocks(&mut self, lba: u64, buf: &mut [u8]) -> Result<(), DriverError> {
        let (start, end) = Self::range(lba, buf.len())?;
        buf.copy_from_slice(&self.store[start..end]);
        Ok(())
    }

    fn write_blocks(&mut self, lba: u64, buf: &[u8]) -> Result<(), DriverError> {
        let (start, end) = Self::range(lba, buf.len())?;
        self.store[start..end].copy_from_slice(buf);
        Ok(())
    }

    fn flush(&mut self) -> Result<(), DriverError> {
        Ok(())
    }
}

/// One block of `0xAB` bytes, the payload every round-trip test writes.
fn one_block() -> [u8; BS as usize] {
    [0xAB; BS as usize]
}

// -- Mirror arm ----------------------------------------------------------

#[test]
fn mirror_arm_reports_level_health_geometry_and_round_trips_io() {
    let mut members = [
        MirrorMember::new(RamBlock::new(0)),
        MirrorMember::new(RamBlock::new(0)),
    ];
    let mut array = RaidArray::Mirror(MirrorArray::assemble(&mut members).expect("assembles"));

    assert_eq!(array.level(), RaidLevel::Mirror);
    assert_eq!(array.health(), ArrayHealth::Optimal);
    assert_eq!(array.member_count(), 2);
    assert_eq!(array.array_geometry().block_count, MB);
    assert_eq!(array.member_state(0), Some(MemberState::InSync));
    assert_eq!(array.member_state(9), None);

    let payload = one_block();
    array
        .write_blocks(1, &payload)
        .expect("write through mirror");
    let mut back = [0u8; BS as usize];
    array
        .read_blocks(1, &mut back)
        .expect("read through mirror");
    assert_eq!(back, payload);
}

#[test]
fn mirror_arm_dispatches_scrub_and_resync() {
    let mut members = [
        MirrorMember::new(RamBlock::new(0)),
        MirrorMember::new(RamBlock::new(0)),
    ];
    let mut array = RaidArray::Mirror(MirrorArray::assemble(&mut members).expect("assembles"));

    assert!(!array.needs_resync());
    assert!(!array.scrubbing());
    array.begin_scrub().expect("scrub begins");
    assert!(array.scrubbing());
    let mut scratch = [0u8; CAP];
    // A clean scrub of a healthy mirror completes without error.
    while array.scrubbing() {
        array.scrub_step(&mut scratch).expect("scrub step");
    }
    // Nothing is resyncing on a healthy array, so a step is a no-op success.
    array.resync_step(&mut scratch).expect("resync no-op");
}

#[test]
fn maintenance_rejects_a_bad_scratch_buffer() {
    let mut members = [
        MirrorMember::new(RamBlock::new(0)),
        MirrorMember::new(RamBlock::new(0)),
    ];
    let mut array = RaidArray::Mirror(MirrorArray::assemble(&mut members).expect("assembles"));

    assert_eq!(array.scrub_step(&mut []).err(), Some(RaidError::BadScratch));
    let mut ragged = [0u8; BS as usize + 1];
    assert_eq!(
        array.resync_step(&mut ragged).err(),
        Some(RaidError::BadScratch)
    );
}

#[test]
fn mirror_arm_readd_out_of_range_is_unknown_member() {
    let mut members = [
        MirrorMember::new(RamBlock::new(0)),
        MirrorMember::new(RamBlock::new(0)),
    ];
    let mut array = RaidArray::Mirror(MirrorArray::assemble(&mut members).expect("assembles"));
    assert_eq!(array.readd_member(9).err(), Some(RaidError::UnknownMember));
}

// -- Stripe arm ----------------------------------------------------------

#[test]
fn stripe_arm_reports_level_and_round_trips_io() {
    let mut members = [
        StripeMember::new(RamBlock::new(0)),
        StripeMember::new(RamBlock::new(0)),
    ];
    let mut array =
        RaidArray::Stripe(StripeArray::assemble(&mut members, CHUNK).expect("assembles"));

    assert_eq!(array.level(), RaidLevel::Stripe);
    assert_eq!(array.health(), ArrayHealth::Optimal);
    assert_eq!(array.member_count(), 2);
    // A stripe's capacity is the sum of its members'.
    assert_eq!(array.array_geometry().block_count, MB * 2);
    assert_eq!(array.member_state(0), Some(MemberState::InSync));
    assert_eq!(array.member_state(2), None);
    assert!(!array.needs_resync());
    assert!(!array.scrubbing());

    let payload = one_block();
    array
        .write_blocks(3, &payload)
        .expect("write through stripe");
    let mut back = [0u8; BS as usize];
    array
        .read_blocks(3, &mut back)
        .expect("read through stripe");
    assert_eq!(back, payload);
}

#[test]
fn stripe_arm_fails_redundancy_operations_closed() {
    let mut members = [
        StripeMember::new(RamBlock::new(0)),
        StripeMember::new(RamBlock::new(0)),
    ];
    let mut array =
        RaidArray::Stripe(StripeArray::assemble(&mut members, CHUNK).expect("assembles"));

    let mut scratch = [0u8; BS as usize];
    assert_eq!(array.begin_scrub().err(), Some(RaidError::NotRedundant));
    assert_eq!(
        array.scrub_step(&mut scratch).err(),
        Some(RaidError::NotRedundant)
    );
    assert_eq!(
        array.resync_step(&mut scratch).err(),
        Some(RaidError::NotRedundant)
    );
    // A stripe rejects a redundancy op even with an *invalid* scratch: the
    // level check wins over scratch validation, giving the informative reason.
    assert_eq!(
        array.scrub_step(&mut []).err(),
        Some(RaidError::NotRedundant)
    );
    assert_eq!(array.readd_member(0).err(), Some(RaidError::NotRedundant));
    assert_eq!(
        array.add_member(0, RamBlock::new(0)).err(),
        Some(RaidError::NotRedundant)
    );
    assert_eq!(
        array.replace_member(0, RamBlock::new(0)).err(),
        Some(RaidError::NotRedundant)
    );
    assert!(matches!(
        array.remove_member(0),
        Err(RaidError::NotRedundant)
    ));
}

// -- Parity arm (block-budget maintenance dispatch) ----------------------

#[test]
fn parity_arm_reports_level_and_dispatches_block_budget_maintenance() {
    let mut members = [
        ParityMember::new(RamBlock::new(0)),
        ParityMember::new(RamBlock::new(0)),
        ParityMember::new(RamBlock::new(0)),
    ];
    let mut scratch = [0u8; 2 * BS as usize];
    let mut array = RaidArray::Parity(
        ParityArray::assemble(&mut members, &mut scratch, CHUNK).expect("assembles"),
    );

    assert_eq!(array.level(), RaidLevel::Parity);
    assert_eq!(array.health(), ArrayHealth::Optimal);
    // Capacity of `member_count - 1` members.
    assert_eq!(array.array_geometry().block_count, MB * 2);

    array.begin_scrub().expect("scrub begins");
    assert!(array.scrubbing());
    // The unified scratch sizes the block budget; the parity engine uses its
    // own assemble-time scratch for reconstruction.
    let mut budget = [0u8; BS as usize];
    while array.scrubbing() {
        array.scrub_step(&mut budget).expect("scrub step");
    }
}

// -- RAID10 arm (stripe-of-mirrors, mirror-style scratch dispatch) -------

#[test]
fn raid10_arm_reports_level_and_dispatches_maintenance() {
    let mut members = [
        MirrorMember::new(RamBlock::new(0)),
        MirrorMember::new(RamBlock::new(0)),
        MirrorMember::new(RamBlock::new(0)),
        MirrorMember::new(RamBlock::new(0)),
    ];
    let mut array =
        RaidArray::Raid10(Raid10Array::assemble(&mut members, CHUNK).expect("assembles"));

    assert_eq!(array.level(), RaidLevel::Raid10);
    assert_eq!(array.health(), ArrayHealth::Optimal);
    assert_eq!(array.member_count(), 4);
    // Two pairs of MB-block members => half the summed capacity.
    assert_eq!(array.array_geometry().block_count, MB * 2);
    assert_eq!(array.member_state(0), Some(MemberState::InSync));

    // Block I/O round-trips through the dispatch.
    let payload = one_block();
    array
        .write_blocks(3, &payload)
        .expect("write through raid10");
    let mut back = [0u8; BS as usize];
    array
        .read_blocks(3, &mut back)
        .expect("read through raid10");
    assert_eq!(back, payload);

    // A redundant level forwards maintenance (not `NotRedundant`); RAID10, like
    // the mirror, takes the scratch byte buffer rather than a block budget.
    array.begin_scrub().expect("scrub begins");
    let mut scratch = [0u8; BS as usize];
    array.scrub_step(&mut scratch).expect("scrub step");
    array.resync_step(&mut scratch).expect("resync step");
    // A bad scratch buffer fails closed at the dispatch boundary.
    assert_eq!(array.scrub_step(&mut []).err(), Some(RaidError::BadScratch));
}

// -- Block trait object --------------------------------------------------

#[test]
fn composed_device_forwards_as_a_block_trait_object() {
    let mut members = [
        MirrorMember::new(RamBlock::new(0)),
        MirrorMember::new(RamBlock::new(0)),
    ];
    let mut array = RaidArray::Mirror(MirrorArray::assemble(&mut members).expect("assembles"));

    let dev: &mut dyn Block = &mut array;
    assert_eq!(dev.geometry().expect("geometry").block_count, MB);
    let payload = one_block();
    dev.write_blocks(0, &payload).expect("write via dyn Block");
    let mut back = [0u8; BS as usize];
    dev.read_blocks(0, &mut back).expect("read via dyn Block");
    assert_eq!(back, payload);
    dev.flush().expect("flush via dyn Block");
    // A composed mirror over RAM members exposes no telemetry of its own.
    assert_eq!(
        dev.device_health().expect("health"),
        DeviceHealth::Unavailable
    );
}

// -- RaidError mapping ---------------------------------------------------

#[test]
fn raid_error_maps_member_faults_and_catches_policy() {
    assert_eq!(
        RaidError::from(MirrorError::UnknownMember),
        RaidError::UnknownMember
    );
    assert_eq!(
        RaidError::from(MirrorError::NotFaulted),
        RaidError::NotFaulted
    );
    assert_eq!(
        RaidError::from(MirrorError::ProbeFailed),
        RaidError::ProbeFailed
    );
    assert_eq!(
        RaidError::from(MirrorError::GeometryMismatch),
        RaidError::GeometryMismatch
    );
    assert_eq!(
        RaidError::from(MirrorError::SlotOccupied),
        RaidError::SlotOccupied
    );
    // An assembly-time reason a reconfiguration path does not produce maps to
    // the defensive catch rather than being silently discarded.
    assert_eq!(RaidError::from(MirrorError::NoMembers), RaidError::Policy);
}

// -- Maintenance progress (durable scrub/rebuild position) ---------------

#[test]
fn the_dispatch_reports_and_restores_progress_on_every_redundant_level() {
    // The serving process checkpoints and resumes through this one surface,
    // whatever level assembled the array, so the dispatch must forward both
    // halves for each of them.
    let mut mirror_members = [
        MirrorMember::new(RamBlock::new(0)),
        MirrorMember::new(RamBlock::new(0)),
    ];
    let mut parity_members = [
        ParityMember::new(RamBlock::new(0)),
        ParityMember::new(RamBlock::new(0)),
        ParityMember::new(RamBlock::new(0)),
    ];
    let mut parity_scratch = [0u8; 2 * BS as usize];
    let mut raid10_members = [
        MirrorMember::new(RamBlock::new(0)),
        MirrorMember::new(RamBlock::new(0)),
        MirrorMember::new(RamBlock::new(0)),
        MirrorMember::new(RamBlock::new(0)),
    ];
    let arrays = [
        RaidArray::Mirror(MirrorArray::assemble(&mut mirror_members).expect("mirror assembles")),
        RaidArray::Parity(
            ParityArray::assemble(&mut parity_members, &mut parity_scratch, CHUNK)
                .expect("parity assembles"),
        ),
        RaidArray::Raid10(
            Raid10Array::assemble(&mut raid10_members, CHUNK).expect("raid10 assembles"),
        ),
    ];
    let mut scratch = [0u8; BS as usize];
    for mut array in arrays {
        let level = array.level();
        assert_eq!(array.progress(), ArrayProgress::IDLE, "{level:?} idle");

        array.begin_scrub().expect("scrub begins");
        array.scrub_step(&mut scratch).expect("one scrub chunk");
        let checkpoint = array.progress();
        assert_eq!(
            checkpoint.scrub_cursor,
            Some(array.scrub_cursor()),
            "{level:?} reports its live cursor"
        );

        // Abandon the pass as a fresh assembly would, then resume it.
        array.begin_scrub().expect("scrub restarts");
        assert_eq!(array.scrub_cursor(), 0);
        array
            .restore_progress(checkpoint)
            .expect("the checkpointed position is adopted");
        assert_eq!(Some(array.scrub_cursor()), checkpoint.scrub_cursor);

        // A cursor the array cannot have produced is refused with its own
        // class, not folded into the defensive catch-all.
        assert_eq!(
            array.restore_progress(ArrayProgress {
                scrub_cursor: Some(u64::MAX),
                resync_cursor: None,
            }),
            Err(RaidError::CursorOutOfRange),
            "{level:?} refuses an impossible cursor"
        );
        assert_eq!(Some(array.scrub_cursor()), checkpoint.scrub_cursor);
    }
}

#[test]
fn a_stripe_has_no_progress_to_report_or_restore() {
    let mut members = [
        StripeMember::new(RamBlock::new(0)),
        StripeMember::new(RamBlock::new(0)),
    ];
    let mut array =
        RaidArray::Stripe(StripeArray::assemble(&mut members, CHUNK).expect("assembles"));

    // Reporting is an observation, answered for every level: a stripe never
    // scrubs or rebuilds, so it has nothing in progress.
    assert_eq!(array.progress(), ArrayProgress::IDLE);
    // Restoring is a redundancy-only operation, refused like the rest — and
    // the level check wins even over an impossible cursor, giving the
    // informative reason.
    assert_eq!(
        array.restore_progress(ArrayProgress::IDLE).err(),
        Some(RaidError::NotRedundant)
    );
    assert_eq!(
        array
            .restore_progress(ArrayProgress {
                scrub_cursor: Some(u64::MAX),
                resync_cursor: None,
            })
            .err(),
        Some(RaidError::NotRedundant)
    );
}

#[test]
fn an_out_of_range_cursor_keeps_its_class_through_every_engine_mapping() {
    // The mapping into the unified error is per-engine, so a missed arm would
    // silently degrade this to the defensive policy catch and lose the reason.
    assert_eq!(
        RaidError::from(MirrorError::CursorOutOfRange),
        RaidError::CursorOutOfRange
    );
    assert_eq!(
        RaidError::from(ParityError::CursorOutOfRange),
        RaidError::CursorOutOfRange
    );
    assert_eq!(
        RaidError::from(DualParityError::CursorOutOfRange),
        RaidError::CursorOutOfRange
    );
    assert_eq!(
        RaidError::from(TripleParityError::CursorOutOfRange),
        RaidError::CursorOutOfRange
    );
    assert_eq!(
        RaidError::from(Raid10Error::CursorOutOfRange),
        RaidError::CursorOutOfRange
    );
    // The stripe-of-mirrors carries the mirror's reason through unchanged.
    assert_eq!(
        Raid10Error::from(MirrorError::CursorOutOfRange),
        Raid10Error::CursorOutOfRange
    );
}
