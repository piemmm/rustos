//! Host tests for the composer's administration and status control endpoint.
//!
//! These prove the *judgement* of one control request against the composer's
//! state and the caller's kernel-attested authority: that authority is checked
//! before any state is touched, that a device is re-read off its own disk
//! before it is overwritten, that a create writes assemblable metadata or
//! rolls back cleanly, and that grow/shrink/stop refuse closed exactly where
//! they must. The composition arithmetic beneath is proven in the shared
//! engines; here only the decisions built on it are asserted.

use alloc::vec;
use alloc::vec::Vec;

use super::{handle_control, LiveArrays};
use crate::compose::{Admission, MemberRegistry};
use crate::runtime::ArrayRuntime;
use crate::service::assemble_array;
use crate::testkit::{
    candidates, request, superblock, MemberDisk, BLOCK_SIZE, DEVICE_BLOCKS, NOW, UUID_A,
};

use tairix_abi::blkio::{decode_completion, BlkDeviceClass, BlkOp, BLK_COMPLETION_LEN};
use tairix_abi::raid::{MemberState, RaidLevel};
use tairix_abi::raid_admin::{
    decode_create_reply, ArrayUuidBytes, MemberNodeList, RaidArrayRecord, RaidControlOp,
    RaidMemberDisposition, RaidMemberRecord, RAID_CONTROL_MAX_REPLY, RAID_CONTROL_MAX_REQUEST,
    RAID_LIST_LIMIT_MAX, RAID_SLOT_NONE,
};
use tairix_abi::raid_ipc::MemberOffer;
use tairix_abi::reply::{decode_page_reply, decode_status_reply};
use tairix_abi::{CapabilityId, Errno};
use tairix_caps::CapabilitySet;

/// The identity a create is told to mint, planted deterministically so a test
/// can name the array it just made.
const MINTED: ArrayUuidBytes = [0x5C; 16];

/// The monotonic reading operations run at.
const NOW_NS: u64 = 1_000;

/// A live-array set backed by a plain vector of runtimes, so the pure
/// administration logic can be driven over the member doubles.
struct TestArrays(Vec<ArrayRuntime<MemberDisk>>);

impl LiveArrays for TestArrays {
    type Device = MemberDisk;

    fn count(&self) -> usize {
        self.0.len()
    }

    fn runtime_mut(&mut self, index: usize) -> Option<&mut ArrayRuntime<MemberDisk>> {
        self.0.get_mut(index)
    }

    fn position(&self, array: &ArrayUuidBytes) -> Option<usize> {
        self.0
            .iter()
            .position(|runtime| &runtime.identity().array_uuid == array)
    }
}

/// An administrator's full authority: both the read and the mutate grants.
fn admin_caps() -> CapabilitySet {
    let mut caps = CapabilitySet::empty();
    caps.insert(CapabilityId::SYSINFO_HW);
    caps.insert(CapabilityId::STORAGE_ADMIN);
    caps
}

/// The read-only authority the status queries need.
fn read_caps() -> CapabilitySet {
    let mut caps = CapabilitySet::empty();
    caps.insert(CapabilityId::SYSINFO_HW);
    caps
}

/// The node id a device at registry index `index` is offered under. Node ids
/// start at one, so a zero node can never be mistaken for a real device.
fn node_id_of(index: usize) -> u32 {
    u32::try_from(index).expect("a test registry index fits a node id") + 1
}

/// Register a blank candidate device of `blocks` logical blocks, returning the
/// node id it is offered under. The device's index in `disks` is its registry
/// member index, which is what the connect seam is keyed by.
fn register_candidate(
    registry: &mut MemberRegistry,
    disks: &mut Vec<MemberDisk>,
    blocks: u64,
) -> u32 {
    let index = disks.len();
    let node = node_id_of(index);
    let offer = MemberOffer {
        endpoint: 0x100 + index as u64,
        window: 0x200 + index as u64,
        node,
    };
    match registry.admit_candidate(0x9000 + index as u64, offer, BlkDeviceClass::Virtual) {
        Admission::Registered { index: got } => {
            assert_eq!(got, index, "the index-check discipline");
        }
        other => panic!("a blank device must be registered as a candidate, got {other:?}"),
    }
    disks.push(MemberDisk::new(blocks));
    node
}

/// Register an already-affiliated member device carrying `superblock`,
/// returning the node id it is offered under. As with a candidate, the
/// device's index in `disks` is its registry member index.
fn register_member(
    registry: &mut MemberRegistry,
    disks: &mut Vec<MemberDisk>,
    superblock: &tairix_raid::ArraySuperblock,
) -> u32 {
    let index = disks.len();
    let node = node_id_of(index);
    let offer = MemberOffer {
        endpoint: 0x100 + index as u64,
        window: 0x200 + index as u64,
        node,
    };
    match registry.admit(
        0x9000 + index as u64,
        offer,
        BlkDeviceClass::Virtual,
        *superblock,
        NOW_NS,
    ) {
        Admission::Registered { index: got } => {
            assert_eq!(got, index, "the index-check discipline");
        }
        other => panic!("an affiliated device must be registered, got {other:?}"),
    }
    disks.push(crate::testkit::stamped(superblock));
    node
}

/// Encode `op` into a request frame.
fn frame_of(op: &RaidControlOp) -> Vec<u8> {
    let mut buf = vec![0u8; RAID_CONTROL_MAX_REQUEST];
    let len = op.encode(&mut buf).expect("the op encodes");
    buf.truncate(len);
    buf
}

/// A random source that plants the deterministic [`MINTED`] identity.
fn plant_minted(buf: &mut [u8; 16]) -> bool {
    buf.copy_from_slice(&MINTED);
    true
}

/// A node remover that permits the removal, for the operations whose decision
/// does not turn on the kernel's answer. `Stop` is the only operation that
/// reaches it, so every other test supplying this one is asserting that the
/// published node tree is left alone regardless.
fn no_remove() -> impl FnMut(u32) -> Result<(), Errno> {
    |_node| Ok(())
}

#[test]
fn a_candidate_is_held_and_reported_available() {
    let mut registry = MemberRegistry::new();
    let mut disks = Vec::new();
    let node = register_candidate(&mut registry, &mut disks, DEVICE_BLOCKS);
    let mut arrays = TestArrays(Vec::new());
    let mut out = [0u8; RAID_CONTROL_MAX_REPLY];

    let frame = frame_of(&RaidControlOp::ListMembers {
        offset: 0,
        limit: 8,
    });
    let effects = handle_control(
        &mut registry,
        &mut arrays,
        &read_caps(),
        &frame,
        NOW,
        NOW_NS,
        |index| disks.get(index).cloned(),
        plant_minted,
        no_remove(),
        &mut out,
    );
    let (count, body) = decode_page_reply(&out[..effects.reply_len], RaidMemberRecord::WIRE_LEN, 8)
        .expect("a page of members");
    assert_eq!(count, 1, "the one held candidate is reported");
    let record = RaidMemberRecord::from_bytes(body).expect("a member record");
    assert_eq!(record.disposition(), RaidMemberDisposition::Candidate);
    assert!(record.is_unaffiliated(), "it belongs to no array");
    assert_eq!(record.slot(), RAID_SLOT_NONE);
    assert_eq!(record.node(), node);
    assert!(
        record.disposition().is_available(),
        "a candidate is available to create or add"
    );
}

/// Build a live mirror array from two fresh candidates via a create, returning
/// the minted identity, the live-array set, the registry, and the disks.
fn created_mirror() -> (MemberRegistry, Vec<MemberDisk>, TestArrays, ArrayUuidBytes) {
    let mut registry = MemberRegistry::new();
    let mut disks = Vec::new();
    let n1 = register_candidate(&mut registry, &mut disks, DEVICE_BLOCKS);
    let n2 = register_candidate(&mut registry, &mut disks, DEVICE_BLOCKS);
    let mut arrays = TestArrays(Vec::new());
    let mut out = [0u8; RAID_CONTROL_MAX_REPLY];

    let frame = frame_of(&RaidControlOp::Create {
        level: RaidLevel::Mirror,
        chunk_blocks: 0,
        members: MemberNodeList::new(&[n1, n2]).unwrap(),
    });
    let effects = handle_control(
        &mut registry,
        &mut arrays,
        &admin_caps(),
        &frame,
        NOW,
        NOW_NS,
        |index| disks.get(index).cloned(),
        plant_minted,
        no_remove(),
        &mut out,
    );
    let uuid = decode_create_reply(&out[..effects.reply_len]).expect("the array is created");
    (registry, disks, arrays, uuid)
}

#[test]
fn a_create_writes_both_superblocks_at_generation_one_and_assembles() {
    let (registry, disks, _arrays, uuid) = created_mirror();
    assert_eq!(uuid, MINTED, "the reply carries the minted identity");
    for disk in &disks {
        assert_eq!(
            disk.on_disk_metadata()
                .expect("a member superblock")
                .generation,
            1,
            "each member is stamped at generation one"
        );
    }
    // The disks now genuinely compose the array their new metadata describes.
    let identity = registry.identity(uuid).expect("the created array resolves");
    let mut supply: Vec<Option<MemberDisk>> = disks.iter().cloned().map(Some).collect();
    assemble_array(identity, registry.candidates(), NOW, |tag| {
        supply[tag].take()
    })
    .expect("the created array assembles");
}

#[test]
fn a_create_naming_a_non_candidate_is_refused_with_nothing_written() {
    // A device already affiliated to an array is not a candidate; naming it in
    // a second create is refused and overwrites nothing.
    let (mut registry, disks, mut arrays, _uuid) = created_mirror();
    let before: Vec<_> = disks.iter().map(MemberDisk::on_disk_metadata).collect();
    let mut out = [0u8; RAID_CONTROL_MAX_REPLY];
    let frame = frame_of(&RaidControlOp::Create {
        level: RaidLevel::Mirror,
        chunk_blocks: 0,
        // Nodes 1 and 2 are now affiliated members, not candidates.
        members: MemberNodeList::new(&[1, 2]).unwrap(),
    });
    let effects = handle_control(
        &mut registry,
        &mut arrays,
        &admin_caps(),
        &frame,
        NOW,
        NOW_NS,
        |index| disks.get(index).cloned(),
        plant_minted,
        no_remove(),
        &mut out,
    );
    assert_eq!(
        decode_create_reply(&out[..effects.reply_len]),
        Err(Errno::Busy)
    );
    let after: Vec<_> = disks.iter().map(MemberDisk::on_disk_metadata).collect();
    assert_eq!(before, after, "nothing was overwritten");
}

#[test]
fn a_create_naming_an_unknown_node_is_refused() {
    let mut registry = MemberRegistry::new();
    let mut disks = Vec::new();
    let node = register_candidate(&mut registry, &mut disks, DEVICE_BLOCKS);
    let _ = register_candidate(&mut registry, &mut disks, DEVICE_BLOCKS);
    let mut arrays = TestArrays(Vec::new());
    let mut out = [0u8; RAID_CONTROL_MAX_REPLY];
    // 999 names no held device.
    let frame = frame_of(&RaidControlOp::Create {
        level: RaidLevel::Mirror,
        chunk_blocks: 0,
        members: MemberNodeList::new(&[node, 999]).unwrap(),
    });
    let effects = handle_control(
        &mut registry,
        &mut arrays,
        &admin_caps(),
        &frame,
        NOW,
        NOW_NS,
        |index| disks.get(index).cloned(),
        plant_minted,
        no_remove(),
        &mut out,
    );
    assert_eq!(
        decode_create_reply(&out[..effects.reply_len]),
        Err(Errno::NotFound)
    );
    assert!(disks.iter().all(|d| d.block_is_blank(0)), "nothing written");
}

#[test]
fn a_create_over_a_no_longer_blank_device_is_refused() {
    let mut registry = MemberRegistry::new();
    let mut disks = Vec::new();
    let n1 = register_candidate(&mut registry, &mut disks, DEVICE_BLOCKS);
    let n2 = register_candidate(&mut registry, &mut disks, DEVICE_BLOCKS);
    // The second candidate quietly acquired another array's metadata since it
    // was offered; the create must re-read the disk and refuse.
    crate::service::write_superblock(
        &mut disks[1].clone(),
        &superblock(RaidLevel::Mirror, [0x77; 16], 2, 1, 4),
    )
    .expect("plant foreign metadata");
    let mut arrays = TestArrays(Vec::new());
    let mut out = [0u8; RAID_CONTROL_MAX_REPLY];
    let frame = frame_of(&RaidControlOp::Create {
        level: RaidLevel::Mirror,
        chunk_blocks: 0,
        members: MemberNodeList::new(&[n1, n2]).unwrap(),
    });
    let effects = handle_control(
        &mut registry,
        &mut arrays,
        &admin_caps(),
        &frame,
        NOW,
        NOW_NS,
        |index| disks.get(index).cloned(),
        plant_minted,
        no_remove(),
        &mut out,
    );
    assert_eq!(
        decode_create_reply(&out[..effects.reply_len]),
        Err(Errno::Busy)
    );
    assert!(disks[0].block_is_blank(0), "the blank member is untouched");
}

#[test]
fn a_create_with_a_width_outside_the_level_floor_is_refused() {
    // RAID10 needs at least two mirrored pairs; two devices is below the floor.
    let mut registry = MemberRegistry::new();
    let mut disks = Vec::new();
    let n1 = register_candidate(&mut registry, &mut disks, DEVICE_BLOCKS);
    let n2 = register_candidate(&mut registry, &mut disks, DEVICE_BLOCKS);
    let mut arrays = TestArrays(Vec::new());
    let mut out = [0u8; RAID_CONTROL_MAX_REPLY];
    let frame = frame_of(&RaidControlOp::Create {
        level: RaidLevel::Raid10,
        chunk_blocks: 8,
        members: MemberNodeList::new(&[n1, n2]).unwrap(),
    });
    let effects = handle_control(
        &mut registry,
        &mut arrays,
        &admin_caps(),
        &frame,
        NOW,
        NOW_NS,
        |index| disks.get(index).cloned(),
        plant_minted,
        no_remove(),
        &mut out,
    );
    assert_eq!(
        decode_create_reply(&out[..effects.reply_len]),
        Err(Errno::OutOfRange)
    );
    assert!(disks[0].block_is_blank(0), "nothing written");
}

#[test]
fn a_create_whose_stripe_unit_contradicts_the_level_is_refused() {
    // A mirror does not stripe, so a non-zero stripe unit is contradictory.
    let mut registry = MemberRegistry::new();
    let mut disks = Vec::new();
    let n1 = register_candidate(&mut registry, &mut disks, DEVICE_BLOCKS);
    let n2 = register_candidate(&mut registry, &mut disks, DEVICE_BLOCKS);
    let mut arrays = TestArrays(Vec::new());
    let mut out = [0u8; RAID_CONTROL_MAX_REPLY];
    let frame = frame_of(&RaidControlOp::Create {
        level: RaidLevel::Mirror,
        chunk_blocks: 8,
        members: MemberNodeList::new(&[n1, n2]).unwrap(),
    });
    let effects = handle_control(
        &mut registry,
        &mut arrays,
        &admin_caps(),
        &frame,
        NOW,
        NOW_NS,
        |index| disks.get(index).cloned(),
        plant_minted,
        no_remove(),
        &mut out,
    );
    assert_eq!(
        decode_create_reply(&out[..effects.reply_len]),
        Err(Errno::OutOfRange)
    );
    assert!(disks.iter().all(|d| d.block_is_blank(0)), "nothing written");
}

#[test]
fn a_create_over_mismatched_geometries_is_refused() {
    let mut registry = MemberRegistry::new();
    let mut disks = Vec::new();
    let n1 = register_candidate(&mut registry, &mut disks, DEVICE_BLOCKS);
    // A member of a different size cannot share the array geometry.
    let n2 = register_candidate(&mut registry, &mut disks, DEVICE_BLOCKS + 8);
    let mut arrays = TestArrays(Vec::new());
    let mut out = [0u8; RAID_CONTROL_MAX_REPLY];
    let frame = frame_of(&RaidControlOp::Create {
        level: RaidLevel::Mirror,
        chunk_blocks: 0,
        members: MemberNodeList::new(&[n1, n2]).unwrap(),
    });
    let effects = handle_control(
        &mut registry,
        &mut arrays,
        &admin_caps(),
        &frame,
        NOW,
        NOW_NS,
        |index| disks.get(index).cloned(),
        plant_minted,
        no_remove(),
        &mut out,
    );
    assert_eq!(
        decode_create_reply(&out[..effects.reply_len]),
        Err(Errno::OutOfRange)
    );
    assert!(disks.iter().all(|d| d.block_is_blank(0)), "nothing written");
}

#[test]
fn a_create_whose_second_write_fails_leaves_no_whole_array() {
    let mut registry = MemberRegistry::new();
    let mut disks = Vec::new();
    let n1 = register_candidate(&mut registry, &mut disks, DEVICE_BLOCKS);
    let n2 = register_candidate(&mut registry, &mut disks, DEVICE_BLOCKS);
    // The second member's disk refuses writes, so its superblock cannot land.
    let _ = disks[1].clone().refusing_writes();
    let mut arrays = TestArrays(Vec::new());
    let mut out = [0u8; RAID_CONTROL_MAX_REPLY];
    let frame = frame_of(&RaidControlOp::Create {
        level: RaidLevel::Mirror,
        chunk_blocks: 0,
        members: MemberNodeList::new(&[n1, n2]).unwrap(),
    });
    let effects = handle_control(
        &mut registry,
        &mut arrays,
        &admin_caps(),
        &frame,
        NOW,
        NOW_NS,
        |index| disks.get(index).cloned(),
        plant_minted,
        no_remove(),
        &mut out,
    );
    assert!(
        decode_create_reply(&out[..effects.reply_len]).is_err(),
        "the create fails closed"
    );
    assert!(
        disks[0].block_is_blank(0),
        "the first member's superblock is rolled back, so no partial array is whole"
    );
    assert!(
        registry.identity(MINTED).is_none(),
        "the registry knows of no such array"
    );
}

#[test]
fn a_caller_without_the_mutate_capability_is_refused_before_touching_state() {
    let mut registry = MemberRegistry::new();
    let mut disks = Vec::new();
    let n1 = register_candidate(&mut registry, &mut disks, DEVICE_BLOCKS);
    let n2 = register_candidate(&mut registry, &mut disks, DEVICE_BLOCKS);
    let mut arrays = TestArrays(Vec::new());
    let mut out = [0u8; RAID_CONTROL_MAX_REPLY];
    let frame = frame_of(&RaidControlOp::Create {
        level: RaidLevel::Mirror,
        chunk_blocks: 0,
        members: MemberNodeList::new(&[n1, n2]).unwrap(),
    });
    // Only the read capability is held; create needs the mutate one.
    let effects = handle_control(
        &mut registry,
        &mut arrays,
        &read_caps(),
        &frame,
        NOW,
        NOW_NS,
        |index| disks.get(index).cloned(),
        plant_minted,
        no_remove(),
        &mut out,
    );
    assert_eq!(
        decode_create_reply(&out[..effects.reply_len]),
        Err(Errno::PermissionDenied)
    );
    assert_eq!(
        effects.outcome,
        Err(Errno::PermissionDenied),
        "audited as refused"
    );
    assert!(
        disks.iter().all(|d| d.block_is_blank(0)),
        "no device was read or written"
    );
    assert!(registry.identity(MINTED).is_none());
}

#[test]
fn a_read_without_the_read_capability_is_refused() {
    let mut registry = MemberRegistry::new();
    let mut arrays = TestArrays(Vec::new());
    let disks: Vec<MemberDisk> = Vec::new();
    let mut out = [0u8; RAID_CONTROL_MAX_REPLY];
    let frame = frame_of(&RaidControlOp::ListArrays {
        offset: 0,
        limit: 8,
    });
    let effects = handle_control(
        &mut registry,
        &mut arrays,
        &CapabilitySet::empty(),
        &frame,
        NOW,
        NOW_NS,
        |index| disks.get(index).cloned(),
        plant_minted,
        no_remove(),
        &mut out,
    );
    assert_eq!(
        decode_page_reply(&out[..effects.reply_len], RaidArrayRecord::WIRE_LEN, 8),
        Err(Errno::PermissionDenied)
    );
}

#[test]
fn a_malformed_frame_is_refused_without_touching_state() {
    let mut registry = MemberRegistry::new();
    let mut disks = Vec::new();
    let _ = register_candidate(&mut registry, &mut disks, DEVICE_BLOCKS);
    let mut arrays = TestArrays(Vec::new());
    let mut out = [0u8; RAID_CONTROL_MAX_REPLY];
    // A frame that is not a valid control frame at all.
    let frame = [0xFFu8; 12];
    let effects = handle_control(
        &mut registry,
        &mut arrays,
        &admin_caps(),
        &frame,
        NOW,
        NOW_NS,
        |index| disks.get(index).cloned(),
        plant_minted,
        no_remove(),
        &mut out,
    );
    assert!(
        decode_status_reply(&out[..effects.reply_len]).is_err(),
        "a malformed frame is refused"
    );
    assert!(disks[0].block_is_blank(0), "no state was touched");
    assert_eq!(registry.members().len(), 1, "the registry is unchanged");
}

#[test]
fn list_paging_returns_the_right_records_and_clamps_an_over_large_limit() {
    let mut registry = MemberRegistry::new();
    let mut disks = Vec::new();
    for _ in 0..3 {
        register_candidate(&mut registry, &mut disks, DEVICE_BLOCKS);
    }
    let mut arrays = TestArrays(Vec::new());
    let mut out = [0u8; RAID_CONTROL_MAX_REPLY];

    // One record per page from offset 1: the second candidate only.
    let frame = frame_of(&RaidControlOp::ListMembers {
        offset: 1,
        limit: 1,
    });
    let effects = handle_control(
        &mut registry,
        &mut arrays,
        &read_caps(),
        &frame,
        NOW,
        NOW_NS,
        |index| disks.get(index).cloned(),
        plant_minted,
        no_remove(),
        &mut out,
    );
    let (count, body) = decode_page_reply(&out[..effects.reply_len], RaidMemberRecord::WIRE_LEN, 1)
        .expect("a one-record page");
    assert_eq!(count, 1);
    assert_eq!(
        RaidMemberRecord::from_bytes(body).unwrap().node(),
        2,
        "offset one names the second member"
    );

    // A limit above the protocol ceiling is clamped, never honoured verbatim.
    let frame = frame_of(&RaidControlOp::ListMembers {
        offset: 0,
        limit: RAID_LIST_LIMIT_MAX,
    });
    let effects = handle_control(
        &mut registry,
        &mut arrays,
        &read_caps(),
        &frame,
        NOW,
        NOW_NS,
        |index| disks.get(index).cloned(),
        plant_minted,
        no_remove(),
        &mut out,
    );
    let (count, _) = decode_page_reply(
        &out[..effects.reply_len],
        RaidMemberRecord::WIRE_LEN,
        RAID_LIST_LIMIT_MAX,
    )
    .expect("a full page");
    assert_eq!(count, 3, "every held device is on the one page");
}

/// Assemble a live runtime over `disks` for the array `superblocks` describe,
/// taking a supplier over clones so the caller keeps its own handles.
fn live_runtime(
    superblocks: &[tairix_raid::ArraySuperblock],
    disks: &[MemberDisk],
) -> ArrayRuntime<MemberDisk> {
    let mut supply: Vec<Option<MemberDisk>> = disks.iter().cloned().map(Some).collect();
    let array_uuid = superblocks[0].array_uuid;
    let identity = tairix_raid::ArrayIdentity::resolve(array_uuid, &candidates(superblocks))
        .expect("the members resolve their array");
    let assembled = assemble_array(identity, &candidates(superblocks), NOW, |tag| {
        supply[tag].take()
    })
    .expect("the members compose their array");
    ArrayRuntime::new(
        assembled.identity,
        assembled.array,
        0x7001,
        0x7002,
        42,
        assembled.resume,
        NOW_NS,
    )
    .expect("the runtime is built from the array's width")
}

#[test]
fn add_refuses_an_occupied_slot_and_a_non_candidate() {
    // A whole two-copy mirror has no absent slot to admit a device into, and a
    // device that is not a candidate cannot be admitted anyway.
    let members = [
        superblock(RaidLevel::Mirror, UUID_A, 2, 0, 3),
        superblock(RaidLevel::Mirror, UUID_A, 2, 1, 3),
    ];
    let member_disks = [
        crate::testkit::stamped(&members[0]),
        crate::testkit::stamped(&members[1]),
    ];
    let runtime = live_runtime(&members, &member_disks);

    let mut registry = MemberRegistry::new();
    let mut disks = Vec::new();
    let candidate_node = register_candidate(&mut registry, &mut disks, DEVICE_BLOCKS);
    let mut arrays = TestArrays(vec![runtime]);
    let mut out = [0u8; RAID_CONTROL_MAX_REPLY];

    // A candidate, but the array is whole: no absent slot.
    let frame = frame_of(&RaidControlOp::Add {
        array: UUID_A,
        node: candidate_node,
    });
    let effects = handle_control(
        &mut registry,
        &mut arrays,
        &admin_caps(),
        &frame,
        NOW,
        NOW_NS,
        |index| disks.get(index).cloned(),
        plant_minted,
        no_remove(),
        &mut out,
    );
    assert_eq!(
        decode_status_reply(&out[..effects.reply_len]),
        Err(Errno::Busy)
    );

    // A node the composer does not hold as a candidate.
    let frame = frame_of(&RaidControlOp::Add {
        array: UUID_A,
        node: 999,
    });
    let effects = handle_control(
        &mut registry,
        &mut arrays,
        &admin_caps(),
        &frame,
        NOW,
        NOW_NS,
        |index| disks.get(index).cloned(),
        plant_minted,
        no_remove(),
        &mut out,
    );
    assert_eq!(
        decode_status_reply(&out[..effects.reply_len]),
        Err(Errno::NotFound)
    );
}

#[test]
fn add_admits_a_candidate_into_an_absent_slot_and_stamps_it() {
    // A degraded mirror serving on one copy has an absent slot a candidate can
    // fill.
    let present = superblock(RaidLevel::Mirror, UUID_A, 2, 0, 3);
    let present_disk = crate::testkit::stamped(&present);
    let runtime = live_runtime(&[present], &[present_disk]);

    let mut registry = MemberRegistry::new();
    let mut disks = Vec::new();
    let candidate_node = register_candidate(&mut registry, &mut disks, DEVICE_BLOCKS);
    let mut arrays = TestArrays(vec![runtime]);
    let mut out = [0u8; RAID_CONTROL_MAX_REPLY];

    let frame = frame_of(&RaidControlOp::Add {
        array: UUID_A,
        node: candidate_node,
    });
    let effects = handle_control(
        &mut registry,
        &mut arrays,
        &admin_caps(),
        &frame,
        NOW,
        NOW_NS,
        |index| disks.get(index).cloned(),
        plant_minted,
        no_remove(),
        &mut out,
    );
    assert_eq!(decode_status_reply(&out[..effects.reply_len]), Ok(()));
    // The admitted disk now carries the array's metadata so a restart finds it.
    let stamped = disks[0]
        .on_disk_metadata()
        .expect("the admitted member is stamped");
    assert_eq!(stamped.array_uuid, UUID_A);
    assert_eq!(stamped.member_slot, 1, "it filled the absent slot");
    // And it is a rebuild target in the live array, not a trusted copy.
    assert_eq!(
        arrays.0[0].member_state(1),
        Some(MemberState::Resyncing),
        "the newly added copy rebuilds before it serves reads"
    );
}

#[test]
fn remove_refuses_a_live_member() {
    // Retiring a healthy member would drop a working copy; it is refused.
    let (mut registry, disks, mut arrays, uuid) = created_mirror();
    // Bring the created array live so its members have live state.
    let member_sbs = [
        disks[0].on_disk_metadata().unwrap(),
        disks[1].on_disk_metadata().unwrap(),
    ];
    arrays.0.push(live_runtime(&member_sbs, &disks));
    // The created array resolved under the minted uuid; re-point the live one.
    assert_eq!(uuid, MINTED);
    // Node 1 is a live in-sync member.
    let mut out = [0u8; RAID_CONTROL_MAX_REPLY];
    let frame = frame_of(&RaidControlOp::Remove {
        array: member_sbs[0].array_uuid,
        node: 1,
    });
    let effects = handle_control(
        &mut registry,
        &mut arrays,
        &admin_caps(),
        &frame,
        NOW,
        NOW_NS,
        |index| disks.get(index).cloned(),
        plant_minted,
        no_remove(),
        &mut out,
    );
    assert_eq!(
        decode_status_reply(&out[..effects.reply_len]),
        Err(Errno::Busy)
    );
    assert!(effects.released.is_empty(), "no membership is released");
}

#[test]
fn stop_refuses_when_the_node_removal_reports_busy_and_releases_nothing() {
    let members = [
        superblock(RaidLevel::Mirror, UUID_A, 2, 0, 3),
        superblock(RaidLevel::Mirror, UUID_A, 2, 1, 3),
    ];
    let member_disks = [
        crate::testkit::stamped(&members[0]),
        crate::testkit::stamped(&members[1]),
    ];
    let runtime = live_runtime(&members, &member_disks);
    let mut registry = MemberRegistry::new();
    let disks: Vec<MemberDisk> = Vec::new();
    let mut arrays = TestArrays(vec![runtime]);
    let mut out = [0u8; RAID_CONTROL_MAX_REPLY];

    let frame = frame_of(&RaidControlOp::Stop { array: UUID_A });
    let effects = handle_control(
        &mut registry,
        &mut arrays,
        &admin_caps(),
        &frame,
        NOW,
        NOW_NS,
        |index| disks.get(index).cloned(),
        plant_minted,
        // The kernel refuses the orderly removal: a volume is still attached.
        |_node| Err(Errno::Busy),
        &mut out,
    );
    assert_eq!(
        decode_status_reply(&out[..effects.reply_len]),
        Err(Errno::Busy)
    );
    assert!(effects.stopped.is_none(), "the array is not torn down");
    assert!(effects.released.is_empty(), "and no member is released");
    assert_eq!(arrays.count(), 1, "the array is still live");
}

#[test]
fn stop_retires_the_node_and_releases_every_member() {
    let (mut registry, disks, mut arrays, _uuid) = created_mirror();
    let uuid = disks[0].on_disk_metadata().unwrap().array_uuid;
    arrays.0.push(live_runtime(
        &[
            disks[0].on_disk_metadata().unwrap(),
            disks[1].on_disk_metadata().unwrap(),
        ],
        &disks,
    ));
    let mut out = [0u8; RAID_CONTROL_MAX_REPLY];
    let frame = frame_of(&RaidControlOp::Stop { array: uuid });
    let effects = handle_control(
        &mut registry,
        &mut arrays,
        &admin_caps(),
        &frame,
        NOW,
        NOW_NS,
        |index| disks.get(index).cloned(),
        plant_minted,
        no_remove(),
        &mut out,
    );
    assert_eq!(decode_status_reply(&out[..effects.reply_len]), Ok(()));
    assert_eq!(effects.stopped, Some(0), "the live array is torn down");
    assert_eq!(
        effects.released,
        vec![1, 0],
        "both members are released, in descending index order"
    );
}

/// Prove a read request is served without the mutate capability, so the two
/// authority tiers are genuinely distinct.
#[test]
fn a_reader_may_list_arrays() {
    let members = [
        superblock(RaidLevel::Mirror, UUID_A, 2, 0, 3),
        superblock(RaidLevel::Mirror, UUID_A, 2, 1, 3),
    ];
    let member_disks = [
        crate::testkit::stamped(&members[0]),
        crate::testkit::stamped(&members[1]),
    ];
    let runtime = live_runtime(&members, &member_disks);
    let mut registry = MemberRegistry::new();
    let disks: Vec<MemberDisk> = Vec::new();
    let mut arrays = TestArrays(vec![runtime]);
    let mut out = [0u8; RAID_CONTROL_MAX_REPLY];

    let frame = frame_of(&RaidControlOp::ListArrays {
        offset: 0,
        limit: 8,
    });
    let effects = handle_control(
        &mut registry,
        &mut arrays,
        &read_caps(),
        &frame,
        NOW,
        NOW_NS,
        |index| disks.get(index).cloned(),
        plant_minted,
        no_remove(),
        &mut out,
    );
    let (count, body) = decode_page_reply(&out[..effects.reply_len], RaidArrayRecord::WIRE_LEN, 8)
        .expect("a page of arrays");
    assert_eq!(count, 1);
    let record = RaidArrayRecord::from_bytes(body).expect("an array record");
    assert_eq!(record.array(), UUID_A);
    assert_eq!(record.level(), RaidLevel::Mirror);
    assert_eq!(record.member_count(), 2);
    assert_eq!(record.active_members(), 2, "both copies are in sync");
}

#[test]
fn remove_vacates_a_faulted_member_and_bumps_the_survivors_generation() {
    // A three-copy mirror, so dropping one copy still leaves the array
    // redundant and the retirement is a real administrative choice rather than
    // the last copy being thrown away.
    let members: Vec<tairix_raid::ArraySuperblock> = (0..3)
        .map(|slot| superblock(RaidLevel::Mirror, UUID_A, 3, slot, 5))
        .collect();
    let mut registry = MemberRegistry::new();
    let mut disks = Vec::new();
    let nodes: Vec<u32> = members
        .iter()
        .map(|member| register_member(&mut registry, &mut disks, member))
        .collect();
    let mut arrays = TestArrays(vec![live_runtime(&members, &disks)]);

    // Fault the middle copy the way the hardware would: its disk starts
    // refusing writes, and the next write to the array drops it. Nothing
    // reaches into the array's private state to declare it faulted.
    let _refusing = disks[1].clone().refusing_writes();
    let mut window = vec![0u8; BLOCK_SIZE as usize];
    let mut completion = [0u8; BLK_COMPLETION_LEN];
    let len = arrays.0[0].serve(
        &request(BlkOp::Write, 1, 1),
        &mut window,
        &mut completion,
        NOW_NS,
    );
    assert_eq!(
        decode_completion(&completion[..len]).map(|_| ()),
        Ok(()),
        "the write is still served by the two healthy copies"
    );
    assert_eq!(
        arrays.0[0].member_state(1),
        Some(MemberState::Faulted),
        "the copy that refused the write is faulted"
    );

    let mut out = [0u8; RAID_CONTROL_MAX_REPLY];
    let frame = frame_of(&RaidControlOp::Remove {
        array: UUID_A,
        node: nodes[1],
    });
    let effects = handle_control(
        &mut registry,
        &mut arrays,
        &admin_caps(),
        &frame,
        NOW,
        NOW_NS,
        |index| disks.get(index).cloned(),
        plant_minted,
        no_remove(),
        &mut out,
    );
    assert_eq!(decode_status_reply(&out[..effects.reply_len]), Ok(()));
    assert_eq!(
        effects.released,
        vec![1],
        "the retired disk's membership is handed back so its agent re-offers it"
    );
    assert_eq!(
        arrays.0[0].member_state(1),
        Some(MemberState::Absent),
        "its slot is vacated, ready for a replacement"
    );

    // The fence: the survivors move to generation 6, so the retired disk —
    // still stamped at 5 — can never come back claiming to be current.
    for (index, disk) in disks.iter().enumerate() {
        let stamped = disk.on_disk_metadata().expect("every disk stays readable");
        let expected = if index == 1 { 5 } else { 6 };
        assert_eq!(
            stamped.generation, expected,
            "disk {index} is stamped at generation {expected}"
        );
    }
}
