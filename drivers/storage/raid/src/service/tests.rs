//! Host tests for the composer's live-array half: reading a member's own
//! metadata, assembling a resolved array, and serving it fault-aware.
//!
//! These prove what the composer *does* with a decision the registry next door
//! has already made. The composition arithmetic underneath (recover, repair,
//! rebuild) is proven once in the shared engines and the reassembly arithmetic
//! once in the shared metadata layer; here only the bring-up contract is
//! asserted — a member's metadata is read off its own disk, a degraded start
//! re-stamps its survivors before serving a byte, a member's reserved metadata
//! can never be reached as array data, and an array that is not the one its
//! members' metadata describes is never brought online.

use alloc::rc::Rc;
use alloc::vec;
use alloc::vec::Vec;
use core::cell::RefCell;

use super::{assemble_array, read_superblock, write_superblock, ArrayRuntime, ServiceError};

use tairix_abi::blkio::{
    decode_completion, BlkHealthState, BlkOp, BlkRequest, BLK_COMPLETION_LEN, BLK_REQUEST_LEN,
};
use tairix_abi::driver::block::{Block, BlockGeometry};
use tairix_abi::driver::DriverError;
use tairix_abi::time::Time64;
use tairix_raid::{
    ArrayHealth, ArrayIdentity, ArraySuperblock, ArrayUuid, AssembleError, Candidate, MemberState,
    RaidLevel, SuperblockError,
};
use tairix_raidmeta::{MAINTENANCE_BLOCK, RESERVED_METADATA_BLOCKS, SUPERBLOCK_BLOCK};

const UUID_A: ArrayUuid = [0xA1; 16];
const UUID_B: ArrayUuid = [0xB2; 16];

/// Logical block size every member in these tests reports.
const BLOCK_SIZE: u32 = 512;

/// Blocks each member device holds in total, its reserved metadata included.
/// Chosen so the data region left past that metadata is a whole number of
/// stripe chunks, which a striped level requires.
const DEVICE_BLOCKS: u64 = 66;

/// Blocks of a member left for array data once its reserved metadata is
/// excluded — the span the composed view covers.
const DATA_BLOCKS: u64 = DEVICE_BLOCKS - RESERVED_METADATA_BLOCKS;

/// The stripe unit every striped array in these tests uses.
const CHUNK: u32 = 8;

/// When a member's metadata was last written before assembly.
const STAMPED_AT: Time64 = Time64::from_secs(1_700_000_000);

/// The instant assembly runs at, distinct from [`STAMPED_AT`] so a re-stamp is
/// visible on the disk rather than having to be taken on trust.
const NOW: Time64 = Time64::from_secs(1_700_000_999);

/// The backing store of one member device double.
struct DiskState {
    bytes: Vec<u8>,
    block_size: u32,
    /// Reject every geometry query: a device that cannot say what it is.
    geometry_fails: bool,
    /// Reject every write: a disk that cannot be re-stamped.
    write_fails: bool,
}

/// A handle to a member device double.
///
/// Cloning it yields a second handle to the *same* disk. Assembly moves a
/// member device into the composed array, so that is how a test inspects what
/// actually landed on a disk the array now owns — the on-disk re-stamp of a
/// degraded start is the assertion that matters most here, and taking it on
/// trust would prove nothing.
#[derive(Clone)]
struct MemberDisk(Rc<RefCell<DiskState>>);

impl MemberDisk {
    /// An empty device of `device_blocks` logical blocks.
    fn new(device_blocks: u64) -> Self {
        let len = usize::try_from(device_blocks).expect("a test device fits the host")
            * BLOCK_SIZE as usize;
        Self(Rc::new(RefCell::new(DiskState {
            bytes: vec![0u8; len],
            block_size: BLOCK_SIZE,
            geometry_fails: false,
            write_fails: false,
        })))
    }

    /// Report a block size of `block_size` rather than the real one.
    fn with_block_size(self, block_size: u32) -> Self {
        self.0.borrow_mut().block_size = block_size;
        self
    }

    /// Fail every geometry query from here on.
    fn breaking_geometry(self) -> Self {
        self.0.borrow_mut().geometry_fails = true;
        self
    }

    /// Fail every write from here on.
    fn refusing_writes(self) -> Self {
        self.0.borrow_mut().write_fails = true;
        self
    }

    /// The byte offset of device block `lba`.
    fn offset(state: &DiskState, lba: u64) -> usize {
        usize::try_from(lba).expect("a test lba fits the host") * state.block_size as usize
    }

    /// Fill device block `lba` with `fill`, bypassing the [`Block`] surface, so
    /// a test can plant data at a known *device* LBA.
    fn plant(&self, lba: u64, fill: u8) {
        let mut state = self.0.borrow_mut();
        let at = Self::offset(&state, lba);
        let end = at + state.block_size as usize;
        state.bytes[at..end].fill(fill);
    }

    /// Whether device block `lba` is entirely zero.
    fn block_is_blank(&self, lba: u64) -> bool {
        let state = self.0.borrow();
        let at = Self::offset(&state, lba);
        state.bytes[at..at + state.block_size as usize]
            .iter()
            .all(|&byte| byte == 0)
    }

    /// Corrupt one byte inside the sealed superblock record.
    fn corrupt_metadata(&self) {
        self.0.borrow_mut().bytes[16] ^= 0xFF;
    }

    /// The metadata currently on the disk, decoded exactly as a later
    /// discovery would decode it.
    fn on_disk_metadata(&self) -> Result<ArraySuperblock, SuperblockError> {
        ArraySuperblock::decode(&self.0.borrow().bytes)
    }
}

impl Block for MemberDisk {
    fn geometry(&self) -> Result<BlockGeometry, DriverError> {
        let state = self.0.borrow();
        if state.geometry_fails {
            return Err(DriverError::DeviceOffline);
        }
        Ok(BlockGeometry {
            block_size: state.block_size,
            block_count: (state.bytes.len() / state.block_size as usize) as u64,
        })
    }

    fn read_blocks(&mut self, lba: u64, buf: &mut [u8]) -> Result<(), DriverError> {
        let state = self.0.borrow();
        let at = Self::offset(&state, lba);
        let end = at.checked_add(buf.len()).ok_or(DriverError::OutOfRange)?;
        if end > state.bytes.len() {
            return Err(DriverError::OutOfRange);
        }
        buf.copy_from_slice(&state.bytes[at..end]);
        Ok(())
    }

    fn write_blocks(&mut self, lba: u64, buf: &[u8]) -> Result<(), DriverError> {
        let mut state = self.0.borrow_mut();
        if state.write_fails {
            return Err(DriverError::MediumError);
        }
        let at = Self::offset(&state, lba);
        let end = at.checked_add(buf.len()).ok_or(DriverError::OutOfRange)?;
        if end > state.bytes.len() {
            return Err(DriverError::OutOfRange);
        }
        state.bytes[at..end].copy_from_slice(buf);
        Ok(())
    }

    fn flush(&mut self) -> Result<(), DriverError> {
        Ok(())
    }
}

/// The array geometry a `count`-member array of `level` over these member
/// devices genuinely presents, sized through the shared capacity oracle rather
/// than a restated literal.
fn array_geometry(level: RaidLevel, count: u16) -> BlockGeometry {
    BlockGeometry {
        block_size: BLOCK_SIZE,
        block_count: level
            .logical_block_count(DATA_BLOCKS, u64::from(count))
            .expect("a composable width"),
    }
}

/// A superblock claiming `slot` of a `count`-member array of `level` at
/// `generation`, declaring the array geometry such an array really presents.
fn superblock(
    level: RaidLevel,
    array: ArrayUuid,
    count: u16,
    slot: u16,
    generation: u64,
) -> ArraySuperblock {
    ArraySuperblock {
        array_uuid: array,
        raid_level: level,
        member_count: count,
        member_slot: slot,
        geometry: array_geometry(level, count),
        generation,
        updated_at: STAMPED_AT,
        chunk_blocks: if level.is_striped() { CHUNK } else { 0 },
    }
}

/// A member device of the stated size carrying `superblock` in its first
/// block, exactly as a discovered array member does.
fn stamped_device(superblock: &ArraySuperblock, device_blocks: u64) -> MemberDisk {
    let disk = MemberDisk::new(device_blocks);
    write_superblock(&mut disk.clone(), superblock)
        .expect("a fresh device accepts its own superblock");
    disk
}

/// A full-size member device carrying `superblock`.
fn stamped(superblock: &ArraySuperblock) -> MemberDisk {
    stamped_device(superblock, DEVICE_BLOCKS)
}

/// The reassembly view of `members`: candidate `i` describes device `i`, which
/// is the correspondence [`assemble_array`]'s supplier is keyed by.
fn candidates(members: &[ArraySuperblock]) -> Vec<Candidate> {
    members
        .iter()
        .enumerate()
        .map(|(tag, superblock)| Candidate {
            tag,
            superblock: *superblock,
        })
        .collect()
}

/// The authoritative shape `members` agree on.
fn identity_of(array: ArrayUuid, members: &[ArraySuperblock]) -> ArrayIdentity {
    ArrayIdentity::resolve(array, &candidates(members)).expect("a claimed array resolves")
}

/// Encode one block request frame.
fn request(op: BlkOp, lba: u64, blocks: u32) -> [u8; BLK_REQUEST_LEN] {
    let mut frame = [0u8; BLK_REQUEST_LEN];
    BlkRequest { op, lba, blocks }
        .encode(&mut frame)
        .expect("the frame is exactly wide enough");
    frame
}

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
fn a_degraded_start_records_a_new_generation_on_every_surviving_member() {
    // The stale-read hole this closes: a mirror brought up without one copy
    // keeps serving *and accepting writes*, so the absent copy is behind from
    // the first write onward. Advancing the array's event count and recording
    // it on the survivors is what makes that copy resolve as a rebuild target
    // when it returns, instead of a copy trusted as current.
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
    assert_eq!(assembled.identity.generation, 6, "the event count advanced");
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
        "a member that is behind makes this a degraded start"
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
    ArrayRuntime::new(assembled.identity, assembled.array, 0x7001, 0x7002, 42)
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
    let mut array = ArrayRuntime::new(assembled.identity, assembled.array, 0x7001, 0x7002, 42);

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
    let mut array = ArrayRuntime::new(assembled.identity, assembled.array, 0x7001, 0x7002, 42);

    let tiny = MemberDisk::new(RESERVED_METADATA_BLOCKS);
    assert_eq!(
        array.place_member(1, tiny),
        Err(ServiceError::MemberTooSmall)
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
