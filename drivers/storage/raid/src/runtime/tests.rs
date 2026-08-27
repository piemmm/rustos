//! Host tests for one live array: serving it, and the self-maintenance it
//! drives between requests.
//!
//! The composition arithmetic underneath (recover, repair, rebuild, verify) is
//! proven once in the shared engines, and *when* to do each of those once in
//! the shared scheduler. What is asserted here is the join between them and the
//! disks: that a decision becomes real transfers, that a pass measured in days
//! is written down and picked up again after a restart, and that a rebuild
//! which finishes is recorded so it is not run all over again.

use alloc::vec;
use alloc::vec::Vec;
use core::cell::Cell;

use super::{ArrayHealthEvent, ArrayRuntime, MaintenanceStep};
use crate::service::{assemble_array, read_maintenance_record, Assembled, ServiceError};
use crate::testkit::{
    candidates, identity_of, request, stamped, superblock, MemberDisk, BLOCK_SIZE, DATA_BLOCKS,
    NOW, UUID_A,
};

use tairix_abi::blkio::{
    decode_completion, decode_outcome, BlkHealthState, BlkOp, BlkStatus, BLK_COMPLETION_LEN,
};
use tairix_abi::raid::{ArrayHealth, MemberState, RaidLevel};
use tairix_abi::sysinfo::{BlkHealthTransition, MountAvailability};
use tairix_abi::DriverError;
use tairix_raid::{ArraySuperblock, MaintenanceAction, RaidError};
use tairix_raidmeta::{ArrayProgress, MaintenanceRecord, RESERVED_METADATA_BLOCKS};

/// How far the test clock advances on every reading.
///
/// Two readings bracket each maintenance turn, so a turn costs twice this —
/// comfortably past the checkpoint interval below, which is what lets a short
/// test observe a position actually being written down.
const TICK_NS: u64 = 20 * 1_000_000_000;

/// The block-service, window and node ids every runtime here is published on.
const ENDPOINT: u64 = 0x7001;
const WINDOW: u64 = 0x7002;
const NODE: u32 = 42;

/// A monotonic clock a test advances by [`TICK_NS`] on every reading.
struct TestClock(Cell<u64>);

impl TestClock {
    const fn new() -> Self {
        Self(Cell::new(0))
    }

    /// Read the clock, advancing it.
    fn tick(&self) -> u64 {
        self.0.set(self.0.get() + TICK_NS);
        self.0.get()
    }
}

/// Wrap an assembled array as a live runtime on the stated ids.
fn live(assembled: Assembled<MemberDisk>) -> ArrayRuntime<MemberDisk> {
    ArrayRuntime::new(
        assembled.identity,
        assembled.array,
        ENDPOINT,
        WINDOW,
        NODE,
        assembled.resume,
        0,
    )
    .expect("the runtime is built from the array's own width")
}

/// Assemble the mirror `members` describe over `disks` and wrap it live.
fn live_mirror(members: &[ArraySuperblock], disks: &[MemberDisk]) -> ArrayRuntime<MemberDisk> {
    live(assemble_mirror(members, disks))
}

/// Assemble the mirror `members` describe over `disks`, leaving it unwrapped so
/// a test can inspect what assembly itself decided.
fn assemble_mirror(members: &[ArraySuperblock], disks: &[MemberDisk]) -> Assembled<MemberDisk> {
    let mut supply: Vec<Option<MemberDisk>> = disks.iter().cloned().map(Some).collect();
    assemble_array(
        identity_of(UUID_A, members),
        &candidates(members),
        NOW,
        |tag| supply[tag].take(),
    )
    .expect("the members compose the array their metadata describes")
}

/// Run maintenance turns until `done` holds or the budget runs out, returning
/// every step performed. The budget only exists so a test cannot hang.
fn drive(
    array: &mut ArrayRuntime<MemberDisk>,
    clock: &TestClock,
    mut done: impl FnMut(&ArrayRuntime<MemberDisk>) -> bool,
) -> Vec<MaintenanceStep> {
    let mut scratch = vec![0u8; 8 * BLOCK_SIZE as usize];
    let mut steps = Vec::new();
    let mut tick = || clock.tick();
    for _ in 0..512 {
        if done(array) {
            return steps;
        }
        let Some(step) = array.maintain(&mut scratch, NOW, &mut tick) else {
            break;
        };
        steps.push(step);
    }
    assert!(done(array), "the maintenance under test terminates");
    steps
}

/// A live two-copy mirror runtime on stated ids, for the serving assertions.
fn runtime() -> ArrayRuntime<MemberDisk> {
    let members = [
        superblock(RaidLevel::Mirror, UUID_A, 2, 0, 3),
        superblock(RaidLevel::Mirror, UUID_A, 2, 1, 3),
    ];
    let mut supply = [Some(stamped(&members[0])), Some(stamped(&members[1]))];
    let assembled = assemble_array(
        identity_of(UUID_A, &members),
        &candidates(&members),
        NOW,
        |tag| supply[tag].take(),
    )
    .expect("assembles");
    live(assembled)
}

#[test]
fn a_live_array_answers_block_requests_through_the_shared_serve_engine() {
    // An array is a block device like any other: the same request engine, the
    // same completion frame, the same health fold. That it is a composition is
    // invisible to its consumer.
    let mut array = runtime();
    let mut window = vec![0u8; BLOCK_SIZE as usize];
    let mut reply = [0u8; BLK_COMPLETION_LEN];

    let len = array.serve(&request(BlkOp::Geometry, 0, 0), &mut window, &mut reply, 0);
    let geometry = decode_completion(&reply[..len]).expect("a geometry completion");
    assert_eq!(geometry.block_size, BLOCK_SIZE);
    assert_eq!(geometry.block_count, DATA_BLOCKS);

    window.fill(0x5A);
    let len = array.serve(&request(BlkOp::Write, 1, 1), &mut window, &mut reply, 0);
    assert_eq!(
        decode_completion(&reply[..len]).map(|_| ()),
        Ok(()),
        "the array is served read/write"
    );
    window.fill(0);
    let len = array.serve(&request(BlkOp::Read, 1, 1), &mut window, &mut reply, 0);
    assert_eq!(decode_completion(&reply[..len]).map(|_| ()), Ok(()));
    assert!(
        window.iter().all(|&byte| byte == 0x5A),
        "the bytes written to the array read back from it"
    );
}

#[test]
fn a_degraded_array_serves_its_reads_while_telling_its_consumer_so() {
    // An array short of redundancy answers every read perfectly well, so a
    // consumer told only "the transfer succeeded" would mount it and report a
    // clean bill on sand — and a filesystem on it would spend discretionary
    // scrub bandwidth the array needs to rebuild. The completion carries what
    // the array can promise instead.
    let present = superblock(RaidLevel::Mirror, UUID_A, 2, 0, 5);
    let mut supply = [Some(stamped(&present))];
    let assembled = assemble_array(
        identity_of(UUID_A, &[present]),
        &candidates(&[present]),
        NOW,
        |tag| supply[tag].take(),
    )
    .expect("one copy of a two-copy mirror still serves");
    let mut array = live(assembled);
    let mut window = vec![0u8; BLOCK_SIZE as usize];
    let mut reply = [0u8; BLK_COMPLETION_LEN];

    let len = array.serve(&request(BlkOp::Read, 1, 1), &mut window, &mut reply, 0);
    let outcome = decode_outcome(&reply[..len]);
    assert_eq!(outcome.status, BlkStatus::Degraded);
    assert!(
        outcome.status.data_valid(),
        "the read did succeed: the survivor served it"
    );
    assert!(
        !outcome.status.is_retryable(),
        "reissuing a good answer would waste the bandwidth the rebuild needs"
    );
    assert_eq!(
        MountAvailability::from_block_status(outcome.status),
        Some(MountAvailability::Degraded),
        "so the consumer's mount reads as at-risk rather than healthy"
    );
    assert_eq!(
        array.health().state(),
        BlkHealthState::Degraded,
        "and the array's own health machine records it, not a fault"
    );

    // A copy coming back makes the array whole, and the next completion says so
    // without any consumer having to ask.
    let returning = stamped(&superblock(RaidLevel::Mirror, UUID_A, 2, 1, 5));
    array.place_member(1, returning).expect("the copy returns");
    let clock = TestClock::new();
    drive(&mut array, &clock, |array| {
        array.array_health() == ArrayHealth::Optimal
    });
    let len = array.serve(&request(BlkOp::Read, 1, 1), &mut window, &mut reply, 0);
    assert_eq!(decode_outcome(&reply[..len]).status, BlkStatus::Ok);
    assert_eq!(array.health().state(), BlkHealthState::Healthy);
}

#[test]
fn a_malformed_request_is_refused_without_touching_the_arrays_health() {
    // A request-level rejection says nothing about the hardware, so it must not
    // be able to push a healthy array into recovery — otherwise any consumer
    // could fault an array by framing nonsense at it.
    let mut array = runtime();
    let mut window = vec![0u8; BLOCK_SIZE as usize];
    let mut reply = [0u8; BLK_COMPLETION_LEN];

    let len = array.serve(
        &request(BlkOp::Read, DATA_BLOCKS + 1, 1),
        &mut window,
        &mut reply,
        0,
    );
    assert!(
        decode_completion(&reply[..len]).is_err(),
        "a read past the end of the array is refused"
    );
    assert_eq!(
        array.health().state(),
        BlkHealthState::Healthy,
        "and the array is still healthy"
    );
    assert_eq!(
        array.poll(u64::MAX),
        BlkHealthState::Healthy,
        "so no grace window was ever armed to expire"
    );
}

#[test]
fn a_runtime_reports_the_ids_it_was_published_on() {
    // The serve loop keys its wait-set, its staging window, and its audit trail
    // off these, so they are part of the runtime's contract rather than
    // incidental bookkeeping.
    let array = runtime();
    assert_eq!(array.endpoint(), 0x7001);
    assert_eq!(array.window_id(), 0x7002);
    assert_eq!(array.node_id(), 42);
    assert_eq!(array.identity().array_uuid, UUID_A);
    assert_eq!(array.identity().generation, 3);
}

#[test]
fn a_returning_member_is_placed_past_its_own_metadata_and_only_once() {
    // A disk that came back is installed into the slot it left as a rebuild
    // target — and through the same metadata-offset view, so its superblock is
    // not exposed as array data. A second placement into an occupied slot is
    // refused rather than displacing the copy already rebuilding there.
    let present = superblock(RaidLevel::Mirror, UUID_A, 2, 0, 5);
    let mut supply = [Some(stamped(&present))];
    let assembled = assemble_array(
        identity_of(UUID_A, &[present]),
        &candidates(&[present]),
        NOW,
        |tag| supply[tag].take(),
    )
    .expect("assembles degraded");
    let mut array = live(assembled);

    let returning = stamped(&superblock(RaidLevel::Mirror, UUID_A, 2, 1, 5));
    assert_eq!(array.place_member(1, returning.clone()), Ok(()));
    assert_eq!(
        array.place_member(1, returning),
        Err(ServiceError::Assembly),
        "a slot that already holds a device is not overwritten"
    );
}

#[test]
fn placing_a_member_outside_the_array_is_refused() {
    // A slot the array does not have cannot be made to exist by asking for it.
    let mut array = runtime();
    let spare = stamped(&superblock(RaidLevel::Mirror, UUID_A, 2, 0, 3));
    assert_eq!(array.place_member(9, spare), Err(ServiceError::Assembly));
}

#[test]
fn placing_a_member_too_small_for_its_own_metadata_is_refused() {
    // A replacement disk is wrapped through the same metadata-offset view as an
    // assembled member, so one with nothing past its reserved metadata is
    // refused there too rather than installed as a zero-length copy.
    let present = superblock(RaidLevel::Mirror, UUID_A, 2, 0, 5);
    let mut supply = [Some(stamped(&present))];
    let assembled = assemble_array(
        identity_of(UUID_A, &[present]),
        &candidates(&[present]),
        NOW,
        |tag| supply[tag].take(),
    )
    .expect("assembles degraded");
    let mut array = live(assembled);

    let tiny = MemberDisk::new(RESERVED_METADATA_BLOCKS);
    assert_eq!(
        array.place_member(1, tiny),
        Err(ServiceError::MemberTooSmall)
    );
}
// -- Self-maintenance ----------------------------------------------------

/// The two superblocks of a two-copy mirror whose second copy is `behind`
/// generations out of date (zero for a whole array).
fn mirror_pair(behind: u64) -> [ArraySuperblock; 2] {
    [
        superblock(RaidLevel::Mirror, UUID_A, 2, 0, 9),
        superblock(RaidLevel::Mirror, UUID_A, 2, 1, 9 - behind),
    ]
}

/// Plant `record` on `disk`, as a previous run of the composer would have.
fn plant_record(disk: &MemberDisk, record: &MaintenanceRecord) {
    crate::service::write_maintenance_record(&mut disk.clone(), record)
        .expect("a test disk accepts a maintenance record");
}

/// The maintenance record on `disk`, as the next assembly would read it.
fn record_on(disk: &MemberDisk) -> Option<MaintenanceRecord> {
    read_maintenance_record(&mut disk.clone())
}

/// Whether a rebuilt copy has been stamped current on its own disk, which is
/// both how a finished rebuild becomes durable and how a test sees it finish.
fn rebuilt_copy_is_recorded_current(array: &ArrayRuntime<MemberDisk>, disk: &MemberDisk) -> bool {
    disk.on_disk_metadata()
        .is_ok_and(|record| record.generation == array.identity().generation)
}

/// Reassemble the array from what its disks say *now*, as the composer does
/// when it restarts: every superblock is read back off the device rather than
/// remembered, so anything the last run recorded is what the next one sees.
fn reassemble_from_disks(disks: &[MemberDisk]) -> Assembled<MemberDisk> {
    let members: Vec<ArraySuperblock> = disks
        .iter()
        .map(|disk| {
            crate::service::read_superblock(&mut disk.clone())
                .expect("every member still carries its own metadata")
        })
        .collect();
    assemble_mirror(&members, disks)
}

#[test]
fn an_array_verified_recently_enough_asks_for_no_maintenance() {
    // The whole point of recording a completed pass is that the next start
    // reads it and leaves the array alone. An array that verified itself a
    // moment ago must not verify itself again now.
    let members = mirror_pair(0);
    let disks = [stamped(&members[0]), stamped(&members[1])];
    let identity = identity_of(UUID_A, &members);
    let verified = MaintenanceRecord::checkpoint(&identity, 4, ArrayProgress::IDLE, Some(NOW));
    plant_record(&disks[0], &verified);

    let mut array = live_mirror(&members, &disks);
    let mut scratch = vec![0u8; 8 * BLOCK_SIZE as usize];
    let clock = TestClock::new();
    let mut tick = || clock.tick();
    assert!(
        array.maintain(&mut scratch, NOW, &mut tick).is_none(),
        "an array with nothing to heal and nothing to verify does no work"
    );
    assert!(
        array.maintenance_deadline_ns().is_some(),
        "but it does say when to look again, so the loop parks rather than polls"
    );
}

#[test]
fn an_array_of_unknown_history_verifies_itself_and_records_the_pass() {
    // No record means the array's verification history is unknown, which is
    // treated as overdue rather than clean. Finishing the pass must reach the
    // members, or the next start would read it as unknown all over again.
    let members = mirror_pair(0);
    let disks = [stamped(&members[0]), stamped(&members[1])];
    let mut array = live_mirror(&members, &disks);
    let clock = TestClock::new();

    let steps = drive(&mut array, &clock, |array| {
        record_on(&disks[0]).is_some_and(|record| record.last_scrub_completed.is_some())
            && array.maintenance_deadline_ns().is_some()
    });
    assert!(
        steps
            .iter()
            .any(|step| step.action == MaintenanceAction::BeginScrub),
        "the array began verifying itself"
    );
    assert!(
        steps.iter().all(|step| step.outcome.is_ok()),
        "and every turn of it was served"
    );

    let recorded = record_on(&disks[0]).expect("the pass reached the member's record");
    assert_eq!(
        recorded.last_scrub_completed,
        Some(NOW),
        "the record says when the array was last verified"
    );
    assert_eq!(
        recorded.progress,
        ArrayProgress::IDLE,
        "and that nothing is now in progress"
    );
}

#[test]
fn a_rebuild_records_its_position_on_current_members_only() {
    // The record's generation must never outrun the generation of the member
    // it sits on, or a copy that was away could return carrying a position for
    // a shape it was never part of. Writing only to current members is what
    // guarantees that, so the rebuilding copy must stay untouched.
    let members = mirror_pair(3);
    let disks = [stamped(&members[0]), stamped(&members[1])];
    let mut array = live_mirror(&members, &disks);
    let clock = TestClock::new();

    let steps = drive(&mut array, &clock, |_| {
        record_on(&disks[0]).is_some_and(|record| record.progress.resync_cursor.is_some())
    });
    assert!(
        steps
            .iter()
            .any(|step| step.action == MaintenanceAction::Resync),
        "the rebuild ran"
    );

    let recorded = record_on(&disks[0]).expect("the current copy carries the position");
    assert_eq!(
        recorded.generation,
        identity_of(UUID_A, &members).generation
    );
    assert!(
        record_on(&disks[1]).is_none(),
        "the copy being rebuilt carries none: its own superblock is older, and a \
         record newer than its superblock is exactly the lie this prevents"
    );
}

#[test]
fn a_rebuild_resumes_where_a_restart_left_it() {
    // A rebuild of a large array outlives a reboot, so the position it reached
    // must survive one. Losing it would mean a machine restarted often enough
    // never finishes rebuilding at all.
    let members = mirror_pair(3);
    let disks = [stamped(&members[0]), stamped(&members[1])];
    let clock = TestClock::new();
    let mut array = live_mirror(&members, &disks);

    drive(&mut array, &clock, |_| {
        record_on(&disks[0]).is_some_and(|record| record.progress.resync_cursor.is_some())
    });
    let recorded = record_on(&disks[0])
        .expect("the position was written down")
        .progress;
    let reached = recorded.resync_cursor.expect("a rebuild was in progress");
    assert!(reached > 0, "the rebuild had made progress worth keeping");
    assert!(
        reached < DATA_BLOCKS,
        "and had not finished, so there is something to resume"
    );
    drop(array);

    // The composer restarts and reassembles the very same disks.
    let mut resumed = reassemble_from_disks(&disks);
    assert_eq!(
        resumed.resume.progress, recorded,
        "the array picks the rebuild up where it stopped"
    );
    assert_eq!(
        resumed.array.progress(),
        recorded,
        "and the composed device really is planted at that position"
    );
    assert_eq!(
        resumed.identity.generation,
        identity_of(UUID_A, &members).generation,
        "a copy that is present and already recorded as behind needs no fencing, so \
         the array does not move out from under its own record"
    );
}

#[test]
fn a_finished_rebuild_is_recorded_so_it_is_not_run_again() {
    // An array whole in memory but still short a copy on disk would rebuild
    // that copy from scratch on every restart — hours of it, on a large array,
    // for ever.
    let members = mirror_pair(3);
    let disks = [stamped(&members[0]), stamped(&members[1])];
    let clock = TestClock::new();
    let mut array = live_mirror(&members, &disks);

    drive(&mut array, &clock, |array| {
        rebuilt_copy_is_recorded_current(array, &disks[1])
    });
    drop(array);

    let mut restarted = reassemble_from_disks(&disks);
    assert_eq!(
        restarted.array.member_state(1),
        Some(MemberState::InSync),
        "the rebuilt copy comes back current, not as a rebuild target again"
    );
    assert!(
        !restarted.degraded,
        "so the array starts whole rather than short a copy"
    );
}

#[test]
fn a_position_the_members_refuse_is_reported_and_still_owed() {
    // A member that will not take the record must not leave the scheduler
    // believing the position is safely on disk.
    let members = mirror_pair(3);
    let disks = [stamped(&members[0]), stamped(&members[1])];
    let mut array = live_mirror(&members, &disks);
    let clock = TestClock::new();
    let _ = disks[0].clone().refusing_writes();

    let mut scratch = vec![0u8; 8 * BLOCK_SIZE as usize];
    let mut tick = || clock.tick();
    let mut refusals = 0u32;
    for _ in 0..8 {
        let Some(step) = array.maintain(&mut scratch, NOW, &mut tick) else {
            break;
        };
        if matches!(step.action, MaintenanceAction::Checkpoint { .. }) {
            assert_eq!(
                step.outcome,
                Err(RaidError::Io(DriverError::MediumError)),
                "the member's own refusal is reported, not swallowed"
            );
            refusals += 1;
        }
    }
    assert!(refusals > 0, "the position was attempted");
    assert!(
        record_on(&disks[0]).is_none(),
        "and nothing was recorded, so it is still owed"
    );
}

#[test]
fn an_array_that_regains_a_copy_reports_rebuilding_then_whole() {
    // An operator watching the log should see the array lose redundancy, get a
    // copy back, and become whole — in the same words a leaf disk's health uses.
    let members = mirror_pair(0);
    let disks = [stamped(&members[0]), stamped(&members[1])];
    let mut supply = [Some(disks[0].clone())];
    let assembled = assemble_array(
        identity_of(UUID_A, &[members[0]]),
        &candidates(&[members[0]]),
        NOW,
        |tag| supply[tag].take(),
    )
    .expect("one copy of a two-copy mirror still serves");
    let mut array = live(assembled);

    assert_eq!(
        array.health_event(),
        None,
        "an array that was already short a copy when it started reports no change"
    );
    array
        .place_member(1, disks[1].clone())
        .expect("the returning copy is installed");
    assert_eq!(
        array.health_event(),
        Some(ArrayHealthEvent::Health(BlkHealthTransition::Recovering)),
        "getting a copy back is a recovery in progress"
    );
    assert_eq!(
        array.health_event(),
        None,
        "and is reported once, not twice"
    );

    let clock = TestClock::new();
    drive(&mut array, &clock, |array| {
        rebuilt_copy_is_recorded_current(array, &disks[1])
    });
    assert_eq!(
        array.health_event(),
        Some(ArrayHealthEvent::Health(BlkHealthTransition::Recovered)),
        "and finishing it makes the array whole again"
    );
}
