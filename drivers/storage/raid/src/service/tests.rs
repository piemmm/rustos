//! Host tests for the composer's assembly half: reading a member's own
//! metadata, and turning a resolved slot table into a composed array.
//!
//! These prove what the composer *does* with a decision the registry next door
//! has already made. The composition arithmetic underneath (recover, repair,
//! rebuild) is proven once in the shared engines and the reassembly arithmetic
//! once in the shared metadata layer; here only the bring-up contract is
//! asserted — a member's metadata is read off its own disk, a degraded start
//! re-stamps its survivors before serving a byte, a member's reserved metadata
//! can never be reached as array data, and an array that is not the one its
//! members' metadata describes is never brought online.

use alloc::vec::Vec;

use super::{
    assemble_array, read_superblock, write_maintenance_record, write_superblock, Assembled,
    ServiceError,
};
use crate::testkit::{
    array_geometry, candidates, identity_of, stamped, stamped_device, superblock, MemberDisk,
    BLOCK_SIZE, DATA_BLOCKS, DEVICE_BLOCKS, NOW, UUID_A, UUID_B,
};

use tairix_abi::driver::block::Block;
use tairix_abi::driver::DriverError;
use tairix_abi::raid::{ArrayHealth, MemberState, RaidLevel};
use tairix_raid::{ArraySuperblock, AssembleError, SuperblockError};
use tairix_raidmeta::{
    ArrayProgress, MaintenanceRecord, MAINTENANCE_BLOCK, RESERVED_METADATA_BLOCKS, SUPERBLOCK_BLOCK,
};

#[test]
fn a_members_metadata_is_read_back_from_its_own_first_block() {
    // The composer believes nothing an offering agent says: which array, which
    // slot, and which generation come from the disk itself. A write followed by
    // a read is the whole of that contract.
    let expected = superblock(RaidLevel::Mirror, UUID_A, 2, 1, 7);
    let disk = stamped(&expected);
    assert_eq!(
        read_superblock(&mut disk.clone()),
        Ok(expected),
        "the superblock read off the device is the one written to it"
    );
    assert!(
        disk.block_is_blank(MAINTENANCE_BLOCK),
        "stamping the superblock leaves the maintenance-record block untouched"
    );
}

#[test]
fn a_device_that_cannot_report_its_geometry_is_neither_read_nor_written() {
    // A disk that will not say what it is cannot be trusted to hold metadata,
    // so both directions fail closed on the geometry query rather than staging
    // a record through a guessed block size.
    let record = superblock(RaidLevel::Mirror, UUID_A, 2, 0, 1);
    let mut disk = stamped(&record).breaking_geometry();
    assert_eq!(
        read_superblock(&mut disk),
        Err(ServiceError::Device(DriverError::DeviceOffline))
    );
    assert_eq!(
        write_superblock(&mut disk, &record),
        Err(ServiceError::Device(DriverError::DeviceOffline))
    );
}

#[test]
fn a_block_size_that_cannot_stage_the_record_fails_closed() {
    // The record is staged through one logical block, so a device whose block
    // is too small to hold it, or larger than the buffer a sane device's block
    // fits in, is refused rather than read through a mis-sized buffer.
    let mut narrow = MemberDisk::new(DEVICE_BLOCKS).with_block_size(64);
    assert_eq!(read_superblock(&mut narrow), Err(ServiceError::BlockSize));

    let mut wide = MemberDisk::new(DEVICE_BLOCKS).with_block_size(8192);
    assert_eq!(read_superblock(&mut wide), Err(ServiceError::BlockSize));
}

#[test]
fn a_device_with_no_valid_metadata_is_refused_never_guessed() {
    // A blank or corrupt first block decodes to nothing: the device is simply
    // not a member, and the decoder's own verdict is surfaced rather than
    // collapsed into one opaque failure.
    let mut blank = MemberDisk::new(DEVICE_BLOCKS);
    assert_eq!(
        read_superblock(&mut blank),
        Err(ServiceError::Superblock(SuperblockError::BadMagic))
    );

    let corrupt = stamped(&superblock(RaidLevel::Mirror, UUID_A, 2, 0, 1));
    corrupt.corrupt_metadata();
    assert_eq!(
        read_superblock(&mut corrupt.clone()),
        Err(ServiceError::Superblock(SuperblockError::BadChecksum))
    );
}

#[test]
fn a_complete_array_starts_clean_and_leaves_every_members_metadata_alone() {
    // Every slot present and current: there is nothing to record, so the array
    // comes up at the generation already on disk and no member is rewritten.
    // Re-stamping here would be a pointless write to every disk on every boot.
    let members = [
        superblock(RaidLevel::Mirror, UUID_A, 2, 0, 5),
        superblock(RaidLevel::Mirror, UUID_A, 2, 1, 5),
    ];
    let disks = [stamped(&members[0]), stamped(&members[1])];
    let mut supply = [Some(disks[0].clone()), Some(disks[1].clone())];

    let mut assembled = assemble_array(
        identity_of(UUID_A, &members),
        &candidates(&members),
        NOW,
        |tag| supply[tag].take(),
    )
    .expect("a complete mirror assembles");

    assert!(!assembled.degraded, "nothing is missing or behind");
    assert_eq!(assembled.identity.generation, 5, "the generation stands");
    assert_eq!(assembled.array.health(), ArrayHealth::Optimal);
    assert_eq!(
        assembled.array.array_geometry(),
        array_geometry(RaidLevel::Mirror, 2)
    );
    for slot in 0..2 {
        assert_eq!(
            assembled.array.member_state(slot),
            Some(MemberState::InSync)
        );
    }
    for (slot, disk) in disks.iter().enumerate() {
        assert_eq!(
            disk.on_disk_metadata(),
            Ok(members[slot]),
            "a clean start rewrites nothing, so the stamp instant is untouched too"
        );
    }
}

#[test]
fn a_start_that_cannot_see_a_member_fences_it_on_every_survivor() {
    // The stale-read hole this closes: a mirror brought up without one copy
    // keeps serving *and accepting writes*, so the absent copy is behind from
    // the first write onward — while its own superblock still claims to be
    // current, because it was not there to be told otherwise. Advancing the
    // array's event count and recording it on the survivors is what makes that
    // copy resolve as a rebuild target when it returns.
    let present = superblock(RaidLevel::Mirror, UUID_A, 2, 0, 5);
    let away = superblock(RaidLevel::Mirror, UUID_A, 2, 1, 5);
    let survivor = stamped(&present);
    let mut supply = [Some(survivor.clone())];

    let mut assembled = assemble_array(
        identity_of(UUID_A, &[present]),
        &candidates(&[present]),
        NOW,
        |tag| supply[tag].take(),
    )
    .expect("a mirror serves on one copy");

    assert!(assembled.degraded, "a missing slot is a degraded start");
    assert_eq!(
        assembled.identity.generation, 6,
        "the event count advanced, fencing the disk that is not here"
    );
    assert_eq!(assembled.array.health(), ArrayHealth::Degraded);

    let recorded = survivor
        .on_disk_metadata()
        .expect("the survivor's metadata is still valid");
    assert_eq!(
        recorded.generation, 6,
        "the survivor's own disk carries the advanced generation"
    );
    assert_eq!(
        recorded.updated_at, NOW,
        "and the instant it was recorded at"
    );
    assert!(
        recorded.generation > away.generation,
        "so the copy that was away is strictly behind and comes back a rebuild target"
    );
    assert_eq!(
        recorded.member_slot, 0,
        "each survivor is re-stamped as the slot it actually holds"
    );
}

#[test]
fn a_survivor_that_cannot_be_restamped_fails_the_whole_bring_up_closed() {
    // If the array cannot record that it started degraded, it must not start.
    // Serving writes the absent copy misses while its metadata still claims to
    // be current *is* the stale-read hole, so refusing is the only honest
    // answer — a disk that will not take the write is unwell anyway.
    let present = superblock(RaidLevel::Mirror, UUID_A, 2, 0, 5);
    let mut supply = [Some(stamped(&present).refusing_writes())];

    assert_eq!(
        assemble_array(
            identity_of(UUID_A, &[present]),
            &candidates(&[present]),
            NOW,
            |tag| supply[tag].take()
        )
        .map(|_| ()),
        Err(ServiceError::Device(DriverError::MediumError))
    );
}

#[test]
fn a_member_the_metadata_proved_stale_joins_as_a_rebuild_target() {
    // The generation counter is the authority: a copy behind the freshest
    // member is admitted only as something to rebuild, so a read is never
    // served from a disk that missed writes.
    let current = superblock(RaidLevel::Mirror, UUID_A, 2, 0, 9);
    let behind = superblock(RaidLevel::Mirror, UUID_A, 2, 1, 4);
    let members = [current, behind];
    let mut supply = [Some(stamped(&current)), Some(stamped(&behind))];

    let mut assembled = assemble_array(
        identity_of(UUID_A, &members),
        &candidates(&members),
        NOW,
        |tag| supply[tag].take(),
    )
    .expect("a mirror with a stale copy still serves");

    assert!(
        assembled.degraded,
        "a member that is behind means the array starts short of full redundancy"
    );
    assert_eq!(
        assembled.array.member_state(0),
        Some(MemberState::InSync),
        "the current copy serves"
    );
    assert_eq!(
        assembled.array.member_state(1),
        Some(MemberState::Resyncing),
        "the copy that is behind is a rebuild target, not a read source"
    );
    assert_eq!(assembled.array.health(), ArrayHealth::Recovering);
}

#[test]
fn an_array_its_members_cannot_serve_is_never_composed() {
    // A stripe holds nothing spare, so a punctured one has holes no redundancy
    // can fill. Composing it would hand a filesystem a device that silently
    // cannot read parts of itself.
    let members = [
        superblock(RaidLevel::Stripe, UUID_A, 3, 0, 1),
        superblock(RaidLevel::Stripe, UUID_A, 3, 2, 1),
    ];
    let mut supply = [Some(stamped(&members[0])), Some(stamped(&members[1]))];

    assert_eq!(
        assemble_array(
            identity_of(UUID_A, &members),
            &candidates(&members),
            NOW,
            |tag| supply[tag].take()
        )
        .map(|_| ()),
        Err(ServiceError::Unservable)
    );
    assert!(
        supply.iter().all(Option::is_some),
        "a refusal reached before any device is taken leaves them all with the caller"
    );
}

#[test]
fn a_present_slot_whose_device_cannot_be_supplied_fails_closed() {
    // The slot table says a slot is filled but the caller cannot produce the
    // device. Composing the rest would silently drop a member the array counts
    // on, so the bring-up is refused and names the tag that went missing.
    let members = [
        superblock(RaidLevel::Mirror, UUID_A, 2, 0, 1),
        superblock(RaidLevel::Mirror, UUID_A, 2, 1, 1),
    ];
    let mut supply = [Some(stamped(&members[0])), None];

    assert_eq!(
        assemble_array(
            identity_of(UUID_A, &members),
            &candidates(&members),
            NOW,
            |tag| supply[tag].take()
        )
        .map(|_| ()),
        Err(ServiceError::Fill(AssembleError::MissingDevice { tag: 1 }))
    );
}

#[test]
fn a_member_with_nothing_past_its_metadata_is_refused() {
    // A device whose whole capacity is its reserved metadata has no array data
    // to contribute; composing it would build a zero-length view.
    let record = superblock(RaidLevel::Mirror, UUID_A, 2, 0, 1);
    let mut supply = [Some(stamped_device(&record, RESERVED_METADATA_BLOCKS))];

    assert_eq!(
        assemble_array(
            identity_of(UUID_A, &[record]),
            &candidates(&[record]),
            NOW,
            |tag| supply[tag].take()
        )
        .map(|_| ()),
        Err(ServiceError::MemberTooSmall)
    );
}

#[test]
fn an_array_that_is_not_the_size_its_own_metadata_records_is_refused() {
    // The members agree on the array's shape, but the disks are smaller than
    // that shape describes. Publishing them anyway would give a filesystem a
    // device shorter than the one it was made on — every address past the end
    // silently unreachable — so the disagreement fails the bring-up closed
    // rather than truncating the array.
    let record = superblock(RaidLevel::Mirror, UUID_A, 2, 0, 1);
    let mut supply = [Some(stamped_device(&record, DEVICE_BLOCKS - 8))];

    assert_eq!(
        assemble_array(
            identity_of(UUID_A, &[record]),
            &candidates(&[record]),
            NOW,
            |tag| supply[tag].take()
        )
        .map(|_| ()),
        Err(ServiceError::GeometryMismatch)
    );
}

#[test]
fn a_members_reserved_metadata_is_never_reachable_as_array_data() {
    // The security property of the composed view. Array block 0 is the
    // member's first *data* block, so no request a consumer can frame reaches
    // the superblock or the maintenance record: reading them would hand array
    // metadata back as file content, and writing them would let a filesystem
    // scribble over the array's own identity.
    let record = superblock(RaidLevel::Mirror, UUID_A, 1, 0, 1);
    let disk = stamped(&record);
    disk.plant(RESERVED_METADATA_BLOCKS, 0xDD);
    let mut supply = [Some(disk.clone())];

    let mut assembled = assemble_array(
        identity_of(UUID_A, &[record]),
        &candidates(&[record]),
        NOW,
        |tag| supply[tag].take(),
    )
    .expect("a single-copy mirror assembles");

    let mut block = [0u8; BLOCK_SIZE as usize];
    assembled
        .array
        .read_blocks(0, &mut block)
        .expect("array block 0 reads");
    assert!(
        block.iter().all(|&byte| byte == 0xDD),
        "array block 0 is the member's first data block, not its superblock"
    );
    assert_eq!(
        assembled.array.array_geometry().block_count,
        DATA_BLOCKS,
        "and the array spans that data region and nothing more, so the reserved \
         blocks cannot be reached by over-running the end either"
    );

    // Writing array block 0 must leave the member's own metadata intact.
    block.fill(0x11);
    assembled
        .array
        .write_blocks(0, &block)
        .expect("array block 0 writes");
    assert_eq!(
        disk.on_disk_metadata(),
        Ok(record),
        "a write to the array never reaches the member's superblock"
    );
    assert!(
        disk.block_is_blank(MAINTENANCE_BLOCK),
        "nor its maintenance-record block"
    );
    assert_eq!(
        SUPERBLOCK_BLOCK, 0,
        "the record sits below the view's block 0"
    );
}
#[test]
fn a_member_of_another_array_is_never_drawn_into_this_one() {
    // Two arrays' members are offered together; resolving one must place only
    // its own. Leaving the foreign member untouched is what stops a corrupt or
    // hostile disk from joining an array it has nothing to do with.
    let mine = superblock(RaidLevel::Mirror, UUID_A, 2, 0, 4);
    let theirs = superblock(RaidLevel::Mirror, UUID_B, 2, 1, 4);
    let members = [mine, theirs];
    let mut supply = [Some(stamped(&mine)), Some(stamped(&theirs))];

    let assembled = assemble_array(
        identity_of(UUID_A, &members),
        &candidates(&members),
        NOW,
        |tag| supply[tag].take(),
    )
    .expect("my array assembles on its own copy");

    assert!(assembled.degraded, "my second copy is missing — not theirs");
    assert!(
        supply[1].is_some(),
        "the other array's member was never taken"
    );
    assert_eq!(
        assembled.slots.len(),
        2,
        "the slot table is my array's width, not the offered set's size"
    );
}

// -- Resuming a pass the members recorded --------------------------------

/// A two-copy mirror whose second copy is `behind` generations out of date.
fn pair_behind(behind: u64) -> [ArraySuperblock; 2] {
    [
        superblock(RaidLevel::Mirror, UUID_A, 2, 0, 9),
        superblock(RaidLevel::Mirror, UUID_A, 2, 1, 9 - behind),
    ]
}

/// Plant `record` on `disk` as a previous run of the composer would have.
fn plant(disk: &MemberDisk, record: &MaintenanceRecord) {
    write_maintenance_record(&mut disk.clone(), record).expect("a test disk records it");
}

/// Assemble the mirror `members` describe over `disks`.
fn assemble_over(members: &[ArraySuperblock], disks: &[MemberDisk]) -> Assembled<MemberDisk> {
    let mut supply: Vec<Option<MemberDisk>> = disks.iter().cloned().map(Some).collect();
    assemble_array(
        identity_of(UUID_A, members),
        &candidates(members),
        NOW,
        |tag| supply[tag].take(),
    )
    .expect("the members compose their array")
}

#[test]
fn an_array_whose_members_recorded_nothing_is_verified_rather_than_assumed_clean() {
    let members = pair_behind(0);
    let disks = [stamped(&members[0]), stamped(&members[1])];

    let assembled = assemble_over(&members, &disks);

    assert_eq!(assembled.resume.progress, ArrayProgress::IDLE);
    assert_eq!(
        assembled.resume.since_last_scrub_ns,
        u64::MAX,
        "an unknown verification history makes a pass due at once, not never"
    );
    assert_eq!(
        assembled.resume.sequence, 0,
        "and the first record it writes starts the sequence"
    );
}

#[test]
fn the_freshest_record_among_the_members_is_the_one_resumed_from() {
    // Members are written one at a time, so a stale copy of the record is
    // normal; resuming from it would redo work the array has already done.
    let members = pair_behind(0);
    let disks = [stamped(&members[0]), stamped(&members[1])];
    let identity = identity_of(UUID_A, &members);
    let stale = MaintenanceRecord::checkpoint(
        &identity,
        4,
        ArrayProgress {
            scrub_cursor: Some(1),
            resync_cursor: None,
        },
        None,
    );
    let fresh = MaintenanceRecord::checkpoint(
        &identity,
        9,
        ArrayProgress {
            scrub_cursor: Some(7),
            resync_cursor: None,
        },
        None,
    );
    plant(&disks[0], &stale);
    plant(&disks[1], &fresh);

    let assembled = assemble_over(&members, &disks);

    assert_eq!(assembled.resume.progress.scrub_cursor, Some(7));
    assert_eq!(
        assembled.resume.sequence, 10,
        "and the next record it writes outranks the freshest already down"
    );
}

#[test]
fn a_record_belonging_to_another_array_is_never_resumed_from() {
    // A disk that was part of some other array is not evidence about this one.
    let members = pair_behind(0);
    let disks = [stamped(&members[0]), stamped(&members[1])];
    let foreign = identity_of(UUID_B, &[superblock(RaidLevel::Mirror, UUID_B, 1, 0, 3)]);
    plant(
        &disks[0],
        &MaintenanceRecord::checkpoint(
            &foreign,
            6,
            ArrayProgress {
                scrub_cursor: Some(5),
                resync_cursor: None,
            },
            Some(NOW),
        ),
    );

    let assembled = assemble_over(&members, &disks);

    assert_eq!(
        assembled.resume.progress,
        ArrayProgress::IDLE,
        "another array's position says nothing about this one"
    );
    assert_eq!(assembled.resume.last_scrub_completed, None);
}

#[test]
fn a_recorded_cursor_the_array_will_not_accept_is_dropped_not_refused() {
    // The position only says where to resume, so a cursor the composed device
    // rejects costs a pass from the beginning — never the array itself.
    let members = pair_behind(0);
    let disks = [stamped(&members[0]), stamped(&members[1])];
    let identity = identity_of(UUID_A, &members);
    plant(
        &disks[0],
        &MaintenanceRecord::checkpoint(
            &identity,
            2,
            ArrayProgress {
                scrub_cursor: Some(DATA_BLOCKS * 4),
                resync_cursor: None,
            },
            None,
        ),
    );

    let assembled = assemble_over(&members, &disks);

    assert_eq!(
        assembled.resume.progress,
        ArrayProgress::IDLE,
        "a cursor outside the array is refused by the engine and dropped here"
    );
}

#[test]
fn a_copy_already_recorded_as_behind_does_not_move_the_array_again() {
    // Bumping the generation fences a disk the composer cannot see. A copy
    // that is present and already behind is fenced by its own superblock, and
    // moving the array on its account would invalidate the maintenance record
    // of the very rebuild it is the target of — so a restart mid-rebuild would
    // start the rebuild over every time.
    let members = pair_behind(3);
    let disks = [stamped(&members[0]), stamped(&members[1])];
    let before = disks[0]
        .on_disk_metadata()
        .expect("the survivor is stamped");

    let assembled = assemble_over(&members, &disks);

    assert!(
        assembled.degraded,
        "the array is still short a current copy"
    );
    assert_eq!(
        assembled.identity.generation,
        identity_of(UUID_A, &members).generation,
        "but its generation does not move"
    );
    assert_eq!(
        disks[0].on_disk_metadata(),
        Ok(before),
        "and no member's metadata is rewritten"
    );
}
