//! Host tests for the array maintenance scheduler.
//!
//! These prove the *policy*: which self-healing action a composed array should
//! take next, when it may take it, and — just as importantly — when it must
//! take none. The engines' own recovery behaviour (what a rebuild or a scrub
//! actually does to the members) is proven in their sibling test modules; here
//! the array is only ever used as the observation surface the decision reads.

use core::cell::Cell;

use alloc::vec;

use super::{
    ArrayMaintenance, MaintenanceAction, MaintenanceError, MaintenancePolicy, MemberRetry,
};
use crate::array::{RaidArray, RaidError};
use crate::backoff::RetryCadence;
use crate::mirror::{ArrayHealth, MemberState, MirrorArray, MirrorMember};
use crate::stripe::{StripeArray, StripeMember};
use crate::superblock::ArrayProgress;
use tairix_abi::blkio::BlkDeviceClass;
use tairix_abi::driver::block::{Block, BlockGeometry};
use tairix_abi::driver::DriverError;

const BS: u32 = 64;
/// Logical blocks per backing member device.
const NBLK: u64 = 8;
/// One member's byte capacity (`BS * NBLK`).
const CAP: usize = 64 * 8;
/// The stripe unit for the non-redundant array, in logical blocks.
const CHUNK: u32 = 2;

/// A policy whose constants are small and exact, so a test reasons about the
/// deadlines directly instead of about a real class's multi-second budget.
/// [`MaintenancePolicy::for_class`] is proven separately.
const TEST_POLICY: MaintenancePolicy = MaintenancePolicy {
    scrub_period_ns: 1_000,
    busy_duty_percent: 50,
    foreground_idle_ns: 100,
    checkpoint_period_ns: 200,
    readd: RetryCadence::new(10, 40),
};

/// An in-memory block device whose presence and per-request faults a test
/// flips through [`Cell`] while the array owns the member.
struct FaultBlock {
    store: [u8; CAP],
    present: Cell<bool>,
    read_fault: Cell<Option<DriverError>>,
    write_fault: Cell<Option<DriverError>>,
}

impl FaultBlock {
    const fn new(fill: u8) -> Self {
        Self {
            store: [fill; CAP],
            present: Cell::new(true),
            read_fault: Cell::new(None),
            write_fault: Cell::new(None),
        }
    }

    /// A device that is not currently reachable, so a member built over it
    /// assembles faulted and cannot be re-probed until it returns.
    fn absent() -> Self {
        let device = Self::new(0);
        device.present.set(false);
        device
    }

    fn span(lba: u64, len: usize) -> Result<(usize, usize), DriverError> {
        let bs = BS as usize;
        if len == 0 || !len.is_multiple_of(bs) {
            return Err(DriverError::BufferTooSmall);
        }
        let start = usize::try_from(lba)
            .ok()
            .and_then(|lba| lba.checked_mul(bs))
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
            Ok(BlockGeometry {
                block_size: BS,
                block_count: NBLK,
            })
        } else {
            Err(DriverError::DeviceOffline)
        }
    }

    fn read_blocks(&mut self, lba: u64, buf: &mut [u8]) -> Result<(), DriverError> {
        if let Some(err) = self.read_fault.get() {
            return Err(err);
        }
        let (start, end) = Self::span(lba, buf.len())?;
        buf.copy_from_slice(&self.store[start..end]);
        Ok(())
    }

    fn write_blocks(&mut self, lba: u64, buf: &[u8]) -> Result<(), DriverError> {
        if let Some(err) = self.write_fault.get() {
            return Err(err);
        }
        let (start, end) = Self::span(lba, buf.len())?;
        self.store[start..end].copy_from_slice(buf);
        Ok(())
    }

    fn flush(&mut self) -> Result<(), DriverError> {
        Ok(())
    }
}

/// Borrow the device behind a present mirror slot. Every caller composes a
/// mirror and names a slot it knows holds a device, so anything else is a bug
/// in the test itself.
fn dev<'a>(array: &'a RaidArray<'_, FaultBlock>, index: usize) -> &'a FaultBlock {
    match array {
        RaidArray::Mirror(inner) => inner
            .member(index)
            .expect("member index in range")
            .device()
            .expect("present member slot holds a device"),
        _ => panic!("the maintenance tests compose mirrors"),
    }
}

fn mirror(members: &mut [MirrorMember<FaultBlock>]) -> RaidArray<'_, FaultBlock> {
    RaidArray::Mirror(MirrorArray::assemble(members).expect("assembles"))
}

fn scheduler<'a>(
    array: &RaidArray<'_, FaultBlock>,
    retries: &'a mut [MemberRetry],
    now_ns: u64,
    since_last_scrub_ns: u64,
) -> ArrayMaintenance<&'a mut [MemberRetry]> {
    ArrayMaintenance::new(array, retries, TEST_POLICY, now_ns, since_last_scrub_ns)
        .expect("the retry buffer matches the array width")
}

/// Assert the scheduler asks for the array's current position to be written,
/// record it as written, and report whether the write also had to record a
/// completed verification pass.
fn record_position(
    maintenance: &mut ArrayMaintenance<&mut [MemberRetry]>,
    array: &RaidArray<'_, FaultBlock>,
    now_ns: u64,
) -> bool {
    let action = maintenance.next_action(array, now_ns);
    let MaintenanceAction::Checkpoint {
        progress,
        pass_completed,
    } = action
    else {
        panic!("expected the position to be written at {now_ns}, got {action:?}");
    };
    assert_eq!(
        progress,
        array.progress(),
        "the action carries exactly the position the array is at"
    );
    maintenance.note_step(action, now_ns, now_ns, Ok(()));
    pass_completed
}

/// Assert the scheduler idles at `now_ns`, and return the deadline it asks its
/// caller to park on — checking that the deadline can never be in the past,
/// which is what makes parking on it a wait rather than a spin.
fn idle_at(
    maintenance: &mut ArrayMaintenance<&mut [MemberRetry]>,
    array: &RaidArray<'_, FaultBlock>,
    now_ns: u64,
) -> Option<u64> {
    assert_eq!(
        maintenance.next_action(array, now_ns),
        MaintenanceAction::Idle
    );
    let deadline = maintenance.wait_deadline_ns();
    if let Some(at) = deadline {
        assert!(
            at > now_ns,
            "an idle deadline of {at} at {now_ns} would spin the serve loop"
        );
    }
    deadline
}

/// A two-copy mirror whose second copy is unreachable, so it assembles
/// faulted and degraded — the state a re-add is scheduled against.
fn faulted_pair() -> [MirrorMember<FaultBlock>; 2] {
    [
        MirrorMember::new(FaultBlock::new(0)),
        MirrorMember::new(FaultBlock::absent()),
    ]
}

// -- Policy --------------------------------------------------------------

#[test]
fn the_first_readd_delay_is_the_classs_own_recovery_grace_window() {
    for class in [
        BlkDeviceClass::Rotational,
        BlkDeviceClass::SolidState,
        BlkDeviceClass::Removable,
        BlkDeviceClass::Virtual,
    ] {
        let policy = MaintenancePolicy::for_class(class);
        assert_eq!(
            policy.readd,
            RetryCadence::for_class(class),
            "a member is re-probed on the shared cadence for its class, never a second rule"
        );
        assert_eq!(
            policy.readd.base_ns(),
            class.budget().grace_ns,
            "re-probing before the device's own driver has given up on it asks nothing useful"
        );
    }
}

#[test]
fn a_seek_bound_class_keeps_a_smaller_share_of_a_busy_array_than_a_fast_one() {
    let rotational = MaintenancePolicy::for_class(BlkDeviceClass::Rotational).busy_duty_percent;
    let removable = MaintenancePolicy::for_class(BlkDeviceClass::Removable).busy_duty_percent;
    let paravirtual = MaintenancePolicy::for_class(BlkDeviceClass::Virtual).busy_duty_percent;
    let solid_state = MaintenancePolicy::for_class(BlkDeviceClass::SolidState).busy_duty_percent;

    assert_eq!(rotational, removable);
    assert!(rotational < paravirtual);
    assert!(paravirtual < solid_state);
    for share in [rotational, removable, paravirtual, solid_state] {
        assert!(
            (1..=100).contains(&share),
            "a share outside 1..=100 would stall maintenance or overrun the array"
        );
    }
}

// -- Construction --------------------------------------------------------

#[test]
fn a_retry_buffer_of_the_wrong_width_fails_closed() {
    let mut members = faulted_pair();
    let array = mirror(&mut members);
    let mut too_short = [MemberRetry::new()];
    assert_eq!(
        ArrayMaintenance::new(&array, &mut too_short, TEST_POLICY, 0, 0).err(),
        Some(MaintenanceError::WidthMismatch)
    );
    let mut too_long = [MemberRetry::new(); 3];
    assert_eq!(
        ArrayMaintenance::new(&array, &mut too_long, TEST_POLICY, 0, 0).err(),
        Some(MaintenanceError::WidthMismatch)
    );
}

#[test]
fn an_array_with_no_scrub_record_is_verified_rather_than_assumed_clean() {
    let mut members = [
        MirrorMember::new(FaultBlock::new(0)),
        MirrorMember::new(FaultBlock::new(0)),
    ];
    let array = mirror(&mut members);
    let mut retries = [MemberRetry::new(); 2];
    let mut maintenance = scheduler(&array, &mut retries, 1_000, u64::MAX);

    assert_eq!(
        maintenance.next_action(&array, 1_000),
        MaintenanceAction::BeginScrub
    );
}

#[test]
fn a_freshly_scrubbed_array_defers_its_next_pass_a_full_period() {
    let mut members = [
        MirrorMember::new(FaultBlock::new(0)),
        MirrorMember::new(FaultBlock::new(0)),
    ];
    let array = mirror(&mut members);
    let mut retries = [MemberRetry::new(); 2];
    let mut maintenance = scheduler(&array, &mut retries, 1_000, 0);

    assert_eq!(idle_at(&mut maintenance, &array, 1_000), Some(2_000));
    assert_eq!(idle_at(&mut maintenance, &array, 1_999), Some(2_000));
    assert_eq!(
        maintenance.next_action(&array, 2_000),
        MaintenanceAction::BeginScrub
    );
}

// -- Nothing to drive ----------------------------------------------------

#[test]
fn a_non_redundant_stripe_is_never_given_maintenance() {
    let mut members = [
        StripeMember::new(FaultBlock::new(0)),
        StripeMember::new(FaultBlock::new(0)),
    ];
    let array = RaidArray::Stripe(StripeArray::assemble(&mut members, CHUNK).expect("assembles"));
    let mut retries = [MemberRetry::new(); 2];
    let mut maintenance = scheduler(&array, &mut retries, 0, u64::MAX);

    assert_eq!(idle_at(&mut maintenance, &array, 0), None);
    assert_eq!(idle_at(&mut maintenance, &array, 100_000), None);
}

#[test]
fn a_failed_array_is_left_to_the_operator() {
    let mut members = [
        MirrorMember::new(FaultBlock::new(0)),
        MirrorMember::new(FaultBlock::new(0)),
    ];
    let mut array = mirror(&mut members);
    for index in 0..2 {
        dev(&array, index)
            .write_fault
            .set(Some(DriverError::DeviceFault));
    }
    assert!(array.write_blocks(0, &[0u8; BS as usize]).is_err());
    assert_eq!(array.health(), ArrayHealth::Failed);

    let mut retries = [MemberRetry::new(); 2];
    let mut maintenance = scheduler(&array, &mut retries, 0, u64::MAX);

    // Both copies returning would not help: with no in-sync member there is
    // nothing to rebuild either from, so the array waits for a re-resolution
    // of its members' superblocks rather than guessing which copy is current.
    for index in 0..2 {
        dev(&array, index).write_fault.set(None);
        maintenance.note_member_returned(index, 100);
    }
    assert_eq!(idle_at(&mut maintenance, &array, 100), None);
}

#[test]
fn a_degraded_array_waits_for_a_spare_rather_than_scrubbing() {
    // The second slot is *absent* — defined but holding no device — so there
    // is nothing to re-probe and nothing to verify with reduced redundancy.
    let mut members = [
        MirrorMember::new(FaultBlock::new(0)),
        MirrorMember::absent(),
    ];
    let array = mirror(&mut members);
    assert_eq!(array.member_state(1), Some(MemberState::Absent));
    assert_eq!(array.health(), ArrayHealth::Degraded);

    let mut retries = [MemberRetry::new(); 2];
    let mut maintenance = scheduler(&array, &mut retries, 0, u64::MAX);

    assert_eq!(idle_at(&mut maintenance, &array, 0), None);
    assert_eq!(idle_at(&mut maintenance, &array, 100_000), None);
}

// -- Priority ------------------------------------------------------------

#[test]
fn a_faulted_member_is_readmitted_before_an_overdue_scrub_is_started() {
    let mut members = faulted_pair();
    let array = mirror(&mut members);
    assert_eq!(array.member_state(1), Some(MemberState::Faulted));

    let mut retries = [MemberRetry::new(); 2];
    let mut maintenance = scheduler(&array, &mut retries, 0, u64::MAX);

    assert_eq!(idle_at(&mut maintenance, &array, 0), Some(10));
    assert_eq!(
        maintenance.next_action(&array, 10),
        MaintenanceAction::Readd { member: 1 },
        "restoring redundancy outranks verifying it"
    );
}

#[test]
fn a_rebuild_outranks_an_overdue_scrub() {
    let mut members = faulted_pair();
    let mut array = mirror(&mut members);
    dev(&array, 1).present.set(true);
    array.readd_member(1).expect("the rebuild begins");
    assert_eq!(array.member_state(1), Some(MemberState::Resyncing));
    assert!(array.needs_resync());

    let mut retries = [MemberRetry::new(); 2];
    let mut maintenance = scheduler(&array, &mut retries, 0, u64::MAX);

    assert_eq!(
        maintenance.next_action(&array, 0),
        MaintenanceAction::Resync
    );
}

// -- Re-add backoff ------------------------------------------------------

#[test]
fn a_refused_readd_doubles_the_backoff_up_to_the_ceiling() {
    let mut members = faulted_pair();
    let array = mirror(&mut members);
    let mut retries = [MemberRetry::new(); 2];
    let mut maintenance = scheduler(&array, &mut retries, 0, u64::MAX);

    // The first look arms the slot, one base delay out.
    assert_eq!(idle_at(&mut maintenance, &array, 0), Some(10));

    let refused = Err(RaidError::ProbeFailed);
    let mut at = 10;
    // The base delay, then 2x, then 4x, then the ceiling holds it at 4x.
    for expected_gap in [20, 40, 40] {
        assert_eq!(
            maintenance.next_action(&array, at),
            MaintenanceAction::Readd { member: 1 }
        );
        maintenance.note_step(MaintenanceAction::Readd { member: 1 }, at, at, refused);
        let next = at + expected_gap;
        assert_eq!(idle_at(&mut maintenance, &array, next - 1), Some(next));
        at = next;
    }
    assert_eq!(
        maintenance.next_action(&array, at),
        MaintenanceAction::Readd { member: 1 },
        "a bounded cadence still re-probes forever, so a disk that comes back always rejoins"
    );
}

#[test]
fn a_successful_readd_clears_the_slot_and_hands_over_to_the_rebuild() {
    let mut members = faulted_pair();
    let mut array = mirror(&mut members);
    let mut retries = [MemberRetry::new(); 2];
    let mut maintenance = scheduler(&array, &mut retries, 0, u64::MAX);

    assert_eq!(idle_at(&mut maintenance, &array, 0), Some(10));
    assert_eq!(
        maintenance.next_action(&array, 10),
        MaintenanceAction::Readd { member: 1 }
    );
    dev(&array, 1).present.set(true);
    let outcome = array.readd_member(1);
    assert!(outcome.is_ok());
    maintenance.note_step(MaintenanceAction::Readd { member: 1 }, 10, 10, outcome);

    assert_eq!(
        maintenance.next_action(&array, 10),
        MaintenanceAction::Resync
    );
}

#[test]
fn a_recovery_signal_pulls_an_attempt_forward_but_never_below_the_floor() {
    let mut members = faulted_pair();
    let array = mirror(&mut members);
    let mut retries = [MemberRetry::new(); 2];
    let mut maintenance = scheduler(&array, &mut retries, 0, u64::MAX);

    // Arm the slot, then refuse one attempt so the backoff has escalated.
    assert_eq!(idle_at(&mut maintenance, &array, 0), Some(10));
    assert_eq!(
        maintenance.next_action(&array, 10),
        MaintenanceAction::Readd { member: 1 }
    );
    maintenance.note_step(
        MaintenanceAction::Readd { member: 1 },
        10,
        10,
        Err(RaidError::ProbeFailed),
    );
    assert_eq!(idle_at(&mut maintenance, &array, 11), Some(30));

    // The member announces it is back: the escalated wait collapses to the
    // base delay after the last attempt, and no further.
    maintenance.note_member_returned(1, 12);
    assert_eq!(idle_at(&mut maintenance, &array, 12), Some(20));
    // Repeating the signal cannot pull it earlier still.
    for repeat in 13..20 {
        maintenance.note_member_returned(1, repeat);
        assert_eq!(idle_at(&mut maintenance, &array, repeat), Some(20));
    }
    assert_eq!(
        maintenance.next_action(&array, 20),
        MaintenanceAction::Readd { member: 1 }
    );
}

#[test]
fn a_recovery_signal_for_an_unknown_or_healthy_slot_changes_nothing() {
    let mut members = faulted_pair();
    let array = mirror(&mut members);
    let mut retries = [MemberRetry::new(); 2];
    let mut maintenance = scheduler(&array, &mut retries, 0, u64::MAX);

    assert_eq!(idle_at(&mut maintenance, &array, 0), Some(10));
    maintenance.note_member_returned(0, 1);
    maintenance.note_member_returned(99, 1);
    assert_eq!(idle_at(&mut maintenance, &array, 1), Some(10));
}

#[test]
fn the_soonest_due_slot_is_chosen_and_ties_break_on_the_lowest_slot() {
    let mut members = [
        MirrorMember::new(FaultBlock::new(0)),
        MirrorMember::new(FaultBlock::absent()),
        MirrorMember::new(FaultBlock::absent()),
    ];
    let array = mirror(&mut members);
    let mut retries = [MemberRetry::new(); 3];
    let mut maintenance = scheduler(&array, &mut retries, 0, u64::MAX);

    assert_eq!(idle_at(&mut maintenance, &array, 0), Some(10));
    assert_eq!(
        maintenance.next_action(&array, 10),
        MaintenanceAction::Readd { member: 1 },
        "an exact tie takes the lowest slot, so the choice is deterministic"
    );
    maintenance.note_step(
        MaintenanceAction::Readd { member: 1 },
        10,
        10,
        Err(RaidError::ProbeFailed),
    );
    assert_eq!(
        maintenance.next_action(&array, 10),
        MaintenanceAction::Readd { member: 2 },
        "the slot still due goes next rather than waiting behind the one just refused"
    );
}

// -- Pacing --------------------------------------------------------------

/// A mirror mid-rebuild, the state every pacing test measures a chunk against.
fn rebuilding(members: &mut [MirrorMember<FaultBlock>]) -> RaidArray<'_, FaultBlock> {
    let mut array = mirror(members);
    dev(&array, 1).present.set(true);
    array.readd_member(1).expect("the rebuild begins");
    array
}

#[test]
fn an_idle_array_runs_its_rebuild_at_full_speed() {
    let mut members = faulted_pair();
    let array = rebuilding(&mut members);
    let mut retries = [MemberRetry::new(); 2];
    let mut maintenance = scheduler(&array, &mut retries, 0, u64::MAX);

    assert_eq!(
        maintenance.next_action(&array, 0),
        MaintenanceAction::Resync
    );
    maintenance.note_step(MaintenanceAction::Resync, 0, 50, Ok(()));
    assert_eq!(
        maintenance.next_action(&array, 50),
        MaintenanceAction::Resync,
        "with no foreground traffic the rebuild is not held back at all"
    );
}

#[test]
fn a_busy_array_holds_maintenance_to_its_duty_share() {
    let mut members = faulted_pair();
    let array = rebuilding(&mut members);
    let mut retries = [MemberRetry::new(); 2];
    let mut maintenance = scheduler(&array, &mut retries, 0, u64::MAX);

    assert_eq!(
        maintenance.next_action(&array, 0),
        MaintenanceAction::Resync
    );
    maintenance.note_foreground(50);
    // A 50 ns chunk at a 50% share buys the workload the next 50 ns.
    maintenance.note_step(MaintenanceAction::Resync, 0, 50, Ok(()));
    assert_eq!(idle_at(&mut maintenance, &array, 99), Some(100));
    assert_eq!(
        maintenance.next_action(&array, 100),
        MaintenanceAction::Resync
    );
}

#[test]
fn a_workload_that_has_gone_quiet_hands_the_array_back_to_maintenance() {
    let mut members = faulted_pair();
    let array = rebuilding(&mut members);
    let mut retries = [MemberRetry::new(); 2];
    let mut maintenance = scheduler(&array, &mut retries, 0, u64::MAX);

    maintenance.note_foreground(0);
    // The busy window is 100 ns, and the chunk ended 150 ns after the last
    // foreground request, so the array counts as idle again.
    maintenance.note_step(MaintenanceAction::Resync, 100, 150, Ok(()));
    assert_eq!(
        maintenance.next_action(&array, 150),
        MaintenanceAction::Resync
    );
}

#[test]
fn a_failed_maintenance_chunk_backs_off_instead_of_hammering_the_members() {
    let mut members = faulted_pair();
    let array = rebuilding(&mut members);
    let mut retries = [MemberRetry::new(); 2];
    let mut maintenance = scheduler(&array, &mut retries, 0, u64::MAX);

    maintenance.note_step(
        MaintenanceAction::Resync,
        0,
        50,
        Err(RaidError::Io(DriverError::DeviceFault)),
    );
    assert_eq!(idle_at(&mut maintenance, &array, 59), Some(60));
    assert_eq!(
        maintenance.next_action(&array, 60),
        MaintenanceAction::Resync
    );
}

#[test]
fn a_mis_set_duty_share_is_clamped_so_maintenance_can_never_stall() {
    let mut members = faulted_pair();
    let array = rebuilding(&mut members);

    let mut starved = [MemberRetry::new(); 2];
    let mut maintenance = ArrayMaintenance::new(
        &array,
        starved.as_mut_slice(),
        MaintenancePolicy {
            busy_duty_percent: 0,
            ..TEST_POLICY
        },
        0,
        u64::MAX,
    )
    .expect("width matches");
    maintenance.note_foreground(50);
    maintenance.note_step(MaintenanceAction::Resync, 0, 50, Ok(()));
    // Clamped to a 1% share: a long but finite hold-off, never a stall.
    assert_eq!(idle_at(&mut maintenance, &array, 50), Some(5_000));

    let mut greedy = [MemberRetry::new(); 2];
    let mut maintenance = ArrayMaintenance::new(
        &array,
        greedy.as_mut_slice(),
        MaintenancePolicy {
            busy_duty_percent: 1_000,
            ..TEST_POLICY
        },
        0,
        u64::MAX,
    )
    .expect("width matches");
    maintenance.note_foreground(50);
    maintenance.note_step(MaintenanceAction::Resync, 0, 50, Ok(()));
    assert_eq!(
        maintenance.next_action(&array, 50),
        MaintenanceAction::Resync,
        "clamped to a 100% share: no hold-off, but still a valid answer"
    );
}

// -- Scrub lifecycle -----------------------------------------------------

#[test]
fn a_completed_scrub_pass_rearms_the_period() {
    let mut members = [
        MirrorMember::new(FaultBlock::new(0)),
        MirrorMember::new(FaultBlock::new(0)),
    ];
    let mut array = mirror(&mut members);
    let mut retries = [MemberRetry::new(); 2];
    let mut maintenance = scheduler(&array, &mut retries, 0, u64::MAX);

    assert_eq!(
        maintenance.next_action(&array, 0),
        MaintenanceAction::BeginScrub
    );
    let outcome = array.begin_scrub();
    maintenance.note_step(MaintenanceAction::BeginScrub, 0, 0, outcome);
    assert!(array.scrubbing());

    let mut scratch = [0u8; BS as usize];
    let mut passes = 0u32;
    while array.scrubbing() {
        assert_eq!(maintenance.next_action(&array, 0), MaintenanceAction::Scrub);
        let outcome = array.scrub_step(&mut scratch);
        maintenance.note_step(MaintenanceAction::Scrub, 0, 0, outcome);
        passes += 1;
        assert!(passes <= 100, "the scrub pass terminates");
    }
    assert_eq!(u64::from(passes), NBLK, "one block per chunk of scratch");

    // The pass ran inside one checkpoint interval, so it left the position
    // exactly where an idle array's sits — yet the members must still be told
    // the array has been verified, or every restart would read its history as
    // unknown and verify it all over again.
    assert_eq!(array.progress(), ArrayProgress::IDLE);
    assert_eq!(
        idle_at(&mut maintenance, &array, 0),
        Some(200),
        "a finished pass is owed a write before the period matters again"
    );
    assert!(
        record_position(&mut maintenance, &array, 200),
        "the write records that the array has been verified"
    );
    assert_eq!(
        idle_at(&mut maintenance, &array, 200),
        Some(1_000),
        "the next pass is a full period after this one finished"
    );
}

#[test]
fn a_refused_scrub_start_defers_the_period_instead_of_retrying_at_once() {
    let mut members = [
        MirrorMember::new(FaultBlock::new(0)),
        MirrorMember::new(FaultBlock::new(0)),
    ];
    let array = mirror(&mut members);
    let mut retries = [MemberRetry::new(); 2];
    let mut maintenance = scheduler(&array, &mut retries, 0, u64::MAX);

    assert_eq!(
        maintenance.next_action(&array, 0),
        MaintenanceAction::BeginScrub
    );
    maintenance.note_step(
        MaintenanceAction::BeginScrub,
        0,
        0,
        Err(RaidError::NotRedundant),
    );
    assert_eq!(idle_at(&mut maintenance, &array, 0), Some(1_000));
}

#[test]
fn a_scrub_pauses_while_redundancy_is_reduced_and_resumes_where_it_stopped() {
    let mut members = [
        MirrorMember::new(FaultBlock::new(0)),
        MirrorMember::new(FaultBlock::new(0)),
    ];
    let mut array = mirror(&mut members);
    let mut retries = [MemberRetry::new(); 2];
    let mut maintenance = scheduler(&array, &mut retries, 0, u64::MAX);
    let mut scratch = [0u8; BS as usize];

    assert_eq!(
        maintenance.next_action(&array, 0),
        MaintenanceAction::BeginScrub
    );
    let outcome = array.begin_scrub();
    maintenance.note_step(MaintenanceAction::BeginScrub, 0, 0, outcome);
    assert_eq!(maintenance.next_action(&array, 0), MaintenanceAction::Scrub);
    let outcome = array.scrub_step(&mut scratch);
    maintenance.note_step(MaintenanceAction::Scrub, 0, 0, outcome);
    let paused_at = array.scrub_cursor();
    assert!(paused_at > 0, "the pass has made progress to preserve");

    // The copy the read path reaches first drops out, so it is dropped from
    // the array. The bandwidth now belongs to getting it back, and there is no
    // second copy to repair a scrub finding from anyway.
    dev(&array, 0)
        .read_fault
        .set(Some(DriverError::DeviceOffline));
    let mut buf = [0u8; BS as usize];
    array
        .read_blocks(0, &mut buf)
        .expect("served from the survivor");
    assert_eq!(array.member_state(0), Some(MemberState::Faulted));
    assert_eq!(array.health(), ArrayHealth::Degraded);
    assert!(array.scrubbing(), "the pass is paused, not abandoned");
    assert_eq!(
        idle_at(&mut maintenance, &array, 0),
        Some(10),
        "the array waits on the faulted copy's re-add, not on a scrub chunk"
    );
    assert_eq!(array.scrub_cursor(), paused_at);

    // The copy returns and is rebuilt; the pass picks up where it stopped
    // rather than starting the whole array over.
    dev(&array, 0).read_fault.set(None);
    assert_eq!(
        maintenance.next_action(&array, 10),
        MaintenanceAction::Readd { member: 0 }
    );
    let outcome = array.readd_member(0);
    maintenance.note_step(MaintenanceAction::Readd { member: 0 }, 10, 10, outcome);
    let mut steps = 0u32;
    while array.needs_resync() {
        array.resync_step(&mut scratch).expect("resync step");
        steps += 1;
        assert!(steps <= 100, "the rebuild terminates");
    }
    assert_eq!(array.health(), ArrayHealth::Optimal);

    assert_eq!(
        maintenance.next_action(&array, 10),
        MaintenanceAction::Scrub
    );
    assert_eq!(array.scrub_cursor(), paused_at);
}

#[test]
fn a_pass_resumed_from_the_records_position_still_rearms_the_period() {
    // A verification pass restored from the array's persisted maintenance
    // record was begun before this scheduler existed. It must still be
    // recognised as running, or its completion goes unnoticed: the period —
    // already overdue, which is exactly why the pass was outstanding — would
    // start the whole pass again immediately, and the array would verify
    // itself back-to-back forever, spending I/O it should be giving the
    // workload.
    let mut members = [
        MirrorMember::new(FaultBlock::new(0)),
        MirrorMember::new(FaultBlock::new(0)),
    ];
    let mut array = mirror(&mut members);
    let mut scratch = [0u8; BS as usize];

    // Assembly restores a pass that was part-way through when the service
    // last stopped.
    array.begin_scrub().expect("scrub begins");
    array.scrub_step(&mut scratch).expect("one chunk");
    let resumed = array.progress();
    array.begin_scrub().expect("a fresh assembly starts over");
    array
        .restore_progress(resumed)
        .expect("the record's position is adopted");
    assert!(array.scrubbing());

    // The scheduler is built over that already-running array, with no record
    // of a *completed* pass.
    let mut retries = [MemberRetry::new(); 2];
    let mut maintenance = scheduler(&array, &mut retries, 0, u64::MAX);

    // It carries the resumed pass to completion rather than restarting it.
    let mut chunks = 0u32;
    while array.scrubbing() {
        assert_eq!(maintenance.next_action(&array, 0), MaintenanceAction::Scrub);
        let outcome = array.scrub_step(&mut scratch);
        maintenance.note_step(MaintenanceAction::Scrub, 0, 0, outcome);
        chunks += 1;
        assert!(chunks <= 100, "the resumed pass terminates");
    }
    assert_eq!(
        u64::from(chunks),
        NBLK - 1,
        "the pass resumed rather than starting over"
    );

    // Finishing it re-arms the period: the array now waits a full period
    // instead of being told to verify itself again at once.
    assert_eq!(
        maintenance.next_action(&array, 0),
        MaintenanceAction::Idle,
        "the array has just been verified, so there is nothing to do"
    );
    assert!(record_position(&mut maintenance, &array, 200));
    assert_eq!(
        idle_at(&mut maintenance, &array, 200),
        Some(1_000),
        "the next pass is a full period after the resumed one finished"
    );
}

// -- Durable position ----------------------------------------------------

#[test]
fn an_advancing_pass_writes_its_position_down_once_an_interval_has_passed() {
    let mut members = [
        MirrorMember::new(FaultBlock::new(0)),
        MirrorMember::new(FaultBlock::new(0)),
    ];
    let mut array = mirror(&mut members);
    let mut retries = [MemberRetry::new(); 2];
    let mut maintenance = scheduler(&array, &mut retries, 0, u64::MAX);
    let mut scratch = [0u8; BS as usize];

    assert_eq!(
        maintenance.next_action(&array, 0),
        MaintenanceAction::BeginScrub
    );
    let outcome = array.begin_scrub();
    maintenance.note_step(MaintenanceAction::BeginScrub, 0, 0, outcome);
    assert_eq!(maintenance.next_action(&array, 0), MaintenanceAction::Scrub);
    let outcome = array.scrub_step(&mut scratch);
    maintenance.note_step(MaintenanceAction::Scrub, 0, 0, outcome);
    assert_eq!(
        maintenance.next_action(&array, 199),
        MaintenanceAction::Scrub,
        "writing the position after every chunk would burn the members for nothing"
    );

    let written = array.progress();
    assert!(!record_position(&mut maintenance, &array, 200));

    assert_eq!(
        maintenance.next_action(&array, 399),
        MaintenanceAction::Scrub,
        "a position the members already hold is not written again"
    );
    let outcome = array.scrub_step(&mut scratch);
    maintenance.note_step(MaintenanceAction::Scrub, 399, 399, outcome);
    assert_ne!(array.progress(), written, "the pass moved on");
    assert_eq!(
        maintenance.next_action(&array, 399),
        MaintenanceAction::Scrub,
        "and is not written again until an interval after the last write"
    );
    assert!(!record_position(&mut maintenance, &array, 400));
}

#[test]
fn a_verified_idle_array_writes_no_metadata_at_all() {
    let mut members = [
        MirrorMember::new(FaultBlock::new(0)),
        MirrorMember::new(FaultBlock::new(0)),
    ];
    let array = mirror(&mut members);
    let mut retries = [MemberRetry::new(); 2];
    // Verified just now, so no pass is due for a full period.
    let mut maintenance = scheduler(&array, &mut retries, 0, 0);

    for now_ns in [0, 200, 500, 999] {
        assert_eq!(
            idle_at(&mut maintenance, &array, now_ns),
            Some(1_000),
            "an array whose position has not moved owes its members nothing"
        );
    }
}

#[test]
fn a_rebuild_running_flat_out_still_yields_to_a_due_position_write() {
    // An idle array paces nothing, so a rebuild chunk is runnable on every
    // turn. Deciding the chunk first would mean the position is never written
    // until the rebuild ends — which on a large array is days away, and is
    // exactly the work a restart would discard.
    let mut members = faulted_pair();
    let mut array = rebuilding(&mut members);
    let mut retries = [MemberRetry::new(); 2];
    let mut maintenance = scheduler(&array, &mut retries, 0, u64::MAX);
    let mut scratch = [0u8; BS as usize];

    assert_eq!(
        maintenance.next_action(&array, 0),
        MaintenanceAction::Resync
    );
    let outcome = array.resync_step(&mut scratch);
    maintenance.note_step(MaintenanceAction::Resync, 0, 0, outcome);
    assert_eq!(
        maintenance.next_action(&array, 0),
        MaintenanceAction::Resync,
        "the rebuild keeps the array at full speed"
    );

    record_position(&mut maintenance, &array, 200);
    assert_eq!(
        maintenance.next_action(&array, 200),
        MaintenanceAction::Resync,
        "and carries straight on afterwards"
    );
}

#[test]
fn a_refused_position_write_holds_off_and_still_owes_the_same_position() {
    let mut members = faulted_pair();
    let mut array = rebuilding(&mut members);
    let mut retries = [MemberRetry::new(); 2];
    let mut maintenance = scheduler(&array, &mut retries, 0, u64::MAX);
    let mut scratch = [0u8; BS as usize];

    let outcome = array.resync_step(&mut scratch);
    maintenance.note_step(MaintenanceAction::Resync, 0, 0, outcome);
    let owed = array.progress();
    assert_eq!(
        maintenance.next_action(&array, 200),
        MaintenanceAction::Checkpoint {
            progress: owed,
            pass_completed: false
        }
    );
    maintenance.note_step(
        MaintenanceAction::Checkpoint {
            progress: owed,
            pass_completed: false,
        },
        200,
        200,
        Err(RaidError::Io(DriverError::DeviceFault)),
    );

    assert_eq!(
        maintenance.next_action(&array, 200),
        MaintenanceAction::Resync,
        "asking again at once would spin the serve loop on a member that just refused"
    );
    assert_eq!(
        maintenance.next_action(&array, 210),
        MaintenanceAction::Checkpoint {
            progress: owed,
            pass_completed: false
        },
        "a position that was not recorded is still owed"
    );
}

#[test]
fn an_unwritten_position_wakes_a_parked_loop() {
    // Nothing else is pending: a scrub paused behind reduced redundancy has no
    // chunk to run and no period to wait on, and an *absent* slot arms no
    // re-add of its own, so only the owed write can change the answer.
    let mut members = [
        MirrorMember::new(FaultBlock::new(0)),
        MirrorMember::absent(),
    ];
    let mut array = mirror(&mut members);
    let mut retries = [MemberRetry::new(); 2];
    let mut maintenance = scheduler(&array, &mut retries, 0, u64::MAX);
    let mut scratch = [0u8; BS as usize];

    assert_eq!(array.health(), ArrayHealth::Degraded);
    array.begin_scrub().expect("the pass begins");
    array.scrub_step(&mut scratch).expect("one chunk");
    assert_eq!(
        idle_at(&mut maintenance, &array, 100),
        Some(200),
        "a loop that parked past the interval would leave the position unwritten"
    );
}

#[test]
fn a_scheduler_can_own_its_retry_records() {
    // A serve process keeps its scheduler beside the array it owns across
    // turns of an event loop, so the per-member records cannot be a borrowed
    // stack slice the way a single call's can.
    let mut members = faulted_pair();
    let array = rebuilding(&mut members);
    let retries = vec![MemberRetry::new(); array.member_count()];
    let mut maintenance = ArrayMaintenance::new(&array, retries, TEST_POLICY, 0, u64::MAX)
        .expect("the retry buffer matches the array width");

    assert_eq!(
        maintenance.next_action(&array, 0),
        MaintenanceAction::Resync,
        "an owning scheduler decides exactly as a borrowing one does"
    );

    let narrow = vec![MemberRetry::new(); array.member_count() - 1];
    assert_eq!(
        ArrayMaintenance::new(&array, narrow, TEST_POLICY, 0, u64::MAX).err(),
        Some(MaintenanceError::WidthMismatch),
        "owned storage is width-checked exactly as borrowed storage is"
    );
}
