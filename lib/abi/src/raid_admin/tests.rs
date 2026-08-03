//! Unit tests for the array-administration and array-reporting protocol.

use super::{
    decode_create_reply, encode_create_reply, MemberNodeList, RaidArrayRecord, RaidControlOp,
    RaidMemberDisposition, RaidMemberRecord, RAID_ARRAY_FLAG_RESYNCING, RAID_ARRAY_FLAG_SCRUBBING,
    RAID_CONTROL_ENDPOINT, RAID_CONTROL_HEADER_LEN, RAID_CONTROL_MAGIC, RAID_CONTROL_MAX_REPLY,
    RAID_CONTROL_MAX_REQUEST, RAID_CREATE_MAX_MEMBERS, RAID_CREATE_REPLY_LEN, RAID_LIST_LIMIT_MAX,
    RAID_SLOT_NONE,
};
use crate::ipc::is_reserved_endpoint;
use crate::raid::{ArrayHealth, MemberState, RaidLevel};
use crate::reply::{PAGE_HEADER_LEN, STATUS_REPLY_LEN};
use crate::{CapabilityId, Errno};

/// A distinguishable array identity: never all-zero, so it is a legal name.
fn uuid() -> [u8; 16] {
    [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16]
}

/// Encode `op` into a fresh buffer and return the frame.
fn frame(op: &RaidControlOp) -> [u8; RAID_CONTROL_MAX_REQUEST] {
    let mut buf = [0u8; RAID_CONTROL_MAX_REQUEST];
    let len = op.encode(&mut buf).expect("frame fits the widest request");
    assert!(len <= RAID_CONTROL_MAX_REQUEST);
    buf
}

/// The encoded length of `op`.
fn frame_len(op: &RaidControlOp) -> usize {
    let mut buf = [0u8; RAID_CONTROL_MAX_REQUEST];
    op.encode(&mut buf).expect("frame fits")
}

/// Every operation, for the round-trip and vocabulary sweeps.
fn every_op() -> [RaidControlOp; 6] {
    [
        RaidControlOp::ListArrays {
            offset: 7,
            limit: RAID_LIST_LIMIT_MAX,
        },
        RaidControlOp::ListMembers {
            offset: 0,
            limit: 1,
        },
        RaidControlOp::Create {
            level: RaidLevel::Mirror,
            chunk_blocks: 0,
            members: MemberNodeList::new(&[11, 12]).expect("two distinct devices"),
        },
        RaidControlOp::Stop { array: uuid() },
        RaidControlOp::Add {
            array: uuid(),
            node: 13,
        },
        RaidControlOp::Remove {
            array: uuid(),
            node: 14,
        },
    ]
}

#[test]
fn the_control_endpoint_is_reserved_so_only_a_privileged_binder_can_claim_it() {
    // A squatter that bound this id would be handed every administrative
    // request on the machine, so the id must demand the privileged bind.
    assert!(is_reserved_endpoint(RAID_CONTROL_ENDPOINT));
    assert_ne!(
        RAID_CONTROL_ENDPOINT,
        crate::raid_ipc::RAID_REGISTRY_ENDPOINT
    );
}

#[test]
fn magic_is_the_ascii_tag() {
    assert_eq!(RAID_CONTROL_MAGIC, u32::from_le_bytes(*b"RAC1"));
}

#[test]
fn every_operation_round_trips() {
    for op in every_op() {
        let bytes = frame(&op);
        let len = frame_len(&op);
        assert_eq!(RaidControlOp::decode(&bytes[..len]), Ok(op));
    }
}

/// `N` distinct, non-zero node ids.
fn nodes<const N: usize>() -> [u32; N] {
    let mut out = [0u32; N];
    for (index, node) in out.iter_mut().enumerate() {
        *node = u32::try_from(index).expect("a test membership fits") + 1;
    }
    out
}

#[test]
fn the_frame_bound_covers_every_level_that_has_a_structural_ceiling() {
    // The bound exists to stop a request growing without limit, so it must
    // still admit the widest array any level can actually be composed as.
    assert_eq!(
        RAID_CREATE_MAX_MEMBERS,
        RaidLevel::TripleParity.max_members() as usize
    );
    assert!(RAID_CREATE_MAX_MEMBERS >= RaidLevel::DualParity.max_members() as usize);
}

#[test]
fn a_create_naming_the_widest_membership_still_fits_the_request_frame() {
    let nodes = nodes::<RAID_CREATE_MAX_MEMBERS>();
    let op = RaidControlOp::Create {
        level: RaidLevel::DualParity,
        chunk_blocks: 64,
        members: MemberNodeList::new(&nodes).expect("the widest legal membership"),
    };
    let len = frame_len(&op);
    assert_eq!(len, RAID_CONTROL_MAX_REQUEST);
    let bytes = frame(&op);
    assert_eq!(RaidControlOp::decode(&bytes[..len]), Ok(op));
}

#[test]
fn reads_and_mutations_declare_the_authority_they_need() {
    for op in every_op() {
        let expected = if op.is_mutation() {
            CapabilityId::STORAGE_ADMIN
        } else {
            // A read of how storage is composed is the hardware view.
            CapabilityId::SYSINFO_HW
        };
        assert_eq!(op.required_capability(), expected, "{}", op.name());
    }
    assert!(!RaidControlOp::ListArrays {
        offset: 0,
        limit: 1
    }
    .is_mutation());
    assert!(!RaidControlOp::ListMembers {
        offset: 0,
        limit: 1
    }
    .is_mutation());
    assert!(RaidControlOp::Stop { array: uuid() }.is_mutation());
}

#[test]
fn a_foreign_or_truncated_frame_is_refused() {
    let op = RaidControlOp::Stop { array: uuid() };
    let len = frame_len(&op);
    let good = frame(&op);

    assert_eq!(
        RaidControlOp::decode(&good[..RAID_CONTROL_HEADER_LEN - 1]),
        Err(Errno::LengthOutOfRange)
    );

    let mut wrong_magic = good;
    wrong_magic[0] ^= 0xff;
    assert_eq!(
        RaidControlOp::decode(&wrong_magic[..len]),
        Err(Errno::BadMagic)
    );

    let mut wrong_version = good;
    wrong_version[4] = 2;
    assert_eq!(
        RaidControlOp::decode(&wrong_version[..len]),
        Err(Errno::BadMagic)
    );

    let mut unknown_op = good;
    unknown_op[6] = 99;
    assert_eq!(
        RaidControlOp::decode(&unknown_op[..len]),
        Err(Errno::NotImplemented)
    );
}

#[test]
fn a_frame_that_is_not_exactly_its_operations_width_is_refused() {
    // Trailing bytes are never ignored: a frame a future reader might
    // interpret differently is not one to act on.
    for op in every_op() {
        let bytes = frame(&op);
        let len = frame_len(&op);
        assert_eq!(
            RaidControlOp::decode(&bytes[..len - 1]),
            Err(Errno::LengthOutOfRange),
            "{} accepted a short frame",
            op.name()
        );
        let mut longer = [0u8; RAID_CONTROL_MAX_REQUEST + 1];
        longer[..len].copy_from_slice(&bytes[..len]);
        assert_eq!(
            RaidControlOp::decode(&longer[..=len]),
            Err(Errno::LengthOutOfRange),
            "{} accepted a padded frame",
            op.name()
        );
    }
}

#[test]
fn a_dirty_reserved_field_is_refused_rather_than_ignored() {
    let list = RaidControlOp::ListArrays {
        offset: 1,
        limit: 4,
    };
    let mut bytes = frame(&list);
    bytes[RAID_CONTROL_HEADER_LEN + 6] = 1;
    assert_eq!(
        RaidControlOp::decode(&bytes[..frame_len(&list)]),
        Err(Errno::BadMagic)
    );

    let create = RaidControlOp::Create {
        level: RaidLevel::Mirror,
        chunk_blocks: 0,
        members: MemberNodeList::new(&[3, 4]).expect("two devices"),
    };
    let mut bytes = frame(&create);
    bytes[RAID_CONTROL_HEADER_LEN + 1] = 1;
    assert_eq!(
        RaidControlOp::decode(&bytes[..frame_len(&create)]),
        Err(Errno::BadMagic)
    );

    let add = RaidControlOp::Add {
        array: uuid(),
        node: 5,
    };
    let mut bytes = frame(&add);
    bytes[RAID_CONTROL_HEADER_LEN + 20] = 1;
    assert_eq!(
        RaidControlOp::decode(&bytes[..frame_len(&add)]),
        Err(Errno::BadMagic)
    );
}

#[test]
fn a_paging_limit_outside_the_page_bound_is_refused() {
    let op = RaidControlOp::ListMembers {
        offset: 0,
        limit: 1,
    };
    let len = frame_len(&op);

    let mut zero = frame(&op);
    zero[RAID_CONTROL_HEADER_LEN + 4] = 0;
    zero[RAID_CONTROL_HEADER_LEN + 5] = 0;
    assert_eq!(RaidControlOp::decode(&zero[..len]), Err(Errno::OutOfRange));

    let mut over = frame(&op);
    let raw = (RAID_LIST_LIMIT_MAX + 1).to_le_bytes();
    over[RAID_CONTROL_HEADER_LEN + 4] = raw[0];
    over[RAID_CONTROL_HEADER_LEN + 5] = raw[1];
    assert_eq!(RaidControlOp::decode(&over[..len]), Err(Errno::OutOfRange));
}

#[test]
fn a_request_naming_nothing_is_refused() {
    // An all-zero identity names no array and a zero node id names no
    // discovered device; neither is a request to guess the intent of.
    let stop = RaidControlOp::Stop { array: uuid() };
    let mut bytes = frame(&stop);
    bytes[RAID_CONTROL_HEADER_LEN..RAID_CONTROL_HEADER_LEN + 16].fill(0);
    assert_eq!(
        RaidControlOp::decode(&bytes[..frame_len(&stop)]),
        Err(Errno::NotFound)
    );

    let remove = RaidControlOp::Remove {
        array: uuid(),
        node: 9,
    };
    let mut bytes = frame(&remove);
    bytes[RAID_CONTROL_HEADER_LEN + 16..RAID_CONTROL_HEADER_LEN + 20].fill(0);
    assert_eq!(
        RaidControlOp::decode(&bytes[..frame_len(&remove)]),
        Err(Errno::NotFound)
    );
}

#[test]
fn a_create_naming_an_unknown_level_is_refused() {
    let op = RaidControlOp::Create {
        level: RaidLevel::Mirror,
        chunk_blocks: 0,
        members: MemberNodeList::new(&[1, 2]).expect("two devices"),
    };
    let mut bytes = frame(&op);
    bytes[RAID_CONTROL_HEADER_LEN] = 0xfe;
    assert_eq!(
        RaidControlOp::decode(&bytes[..frame_len(&op)]),
        Err(Errno::OutOfRange)
    );
}

#[test]
fn a_create_whose_member_count_does_not_match_its_frame_is_refused() {
    let op = RaidControlOp::Create {
        level: RaidLevel::Mirror,
        chunk_blocks: 0,
        members: MemberNodeList::new(&[1, 2]).expect("two devices"),
    };
    let len = frame_len(&op);

    let mut claims_more = frame(&op);
    claims_more[RAID_CONTROL_HEADER_LEN + 6] = 3;
    assert_eq!(
        RaidControlOp::decode(&claims_more[..len]),
        Err(Errno::LengthOutOfRange)
    );

    let mut claims_none = frame(&op);
    claims_none[RAID_CONTROL_HEADER_LEN + 6] = 0;
    assert_eq!(
        RaidControlOp::decode(&claims_none[..len]),
        Err(Errno::LengthOutOfRange)
    );
}

#[test]
fn a_create_naming_one_device_twice_is_refused() {
    // Composing an array from one disk pretending to be two would present
    // redundancy that does not exist.
    let op = RaidControlOp::Create {
        level: RaidLevel::Mirror,
        chunk_blocks: 0,
        members: MemberNodeList::new(&[8, 9]).expect("two devices"),
    };
    let mut bytes = frame(&op);
    bytes[RAID_CONTROL_HEADER_LEN + 12] = 8;
    assert_eq!(
        RaidControlOp::decode(&bytes[..frame_len(&op)]),
        Err(Errno::AlreadyExists)
    );
}

#[test]
fn a_member_list_validates_what_it_is_given() {
    assert_eq!(MemberNodeList::new(&[]), Err(Errno::LengthOutOfRange));
    assert_eq!(MemberNodeList::new(&[1, 0]), Err(Errno::NotFound));
    assert_eq!(MemberNodeList::new(&[4, 4]), Err(Errno::AlreadyExists));
    let too_many = nodes::<{ RAID_CREATE_MAX_MEMBERS + 1 }>();
    assert_eq!(MemberNodeList::new(&too_many), Err(Errno::LengthOutOfRange));
    let list = MemberNodeList::new(&[5, 6, 7]).expect("three devices");
    assert_eq!(list.as_slice(), &[5, 6, 7]);
    assert_eq!(list.len(), 3);
    assert!(!list.is_empty());
}

#[test]
fn a_create_reply_carries_the_identity_the_composer_minted() {
    let bytes = encode_create_reply(Ok(uuid()));
    assert_eq!(bytes.len(), RAID_CREATE_REPLY_LEN);
    assert_eq!(decode_create_reply(&bytes), Ok(uuid()));

    assert_eq!(
        decode_create_reply(&encode_create_reply(Err(Errno::PermissionDenied))),
        Err(Errno::PermissionDenied)
    );
    assert_eq!(
        decode_create_reply(&[0u8; RAID_CREATE_REPLY_LEN - 1]),
        Err(Errno::BufferTooSmall)
    );
    // A success that names no array is not believed.
    assert_eq!(
        decode_create_reply(&[0u8; RAID_CREATE_REPLY_LEN]),
        Err(Errno::NotFound)
    );
}

/// A fully-populated array record, for the record round-trip sweeps.
fn array_record() -> RaidArrayRecord {
    RaidArrayRecord::new(
        uuid(),
        RaidLevel::Parity,
        ArrayHealth::Recovering,
        RAID_ARRAY_FLAG_SCRUBBING | RAID_ARRAY_FLAG_RESYNCING,
        4,
        3,
        512,
        128,
        1 << 40,
        0x5241_2001,
        42,
        1 << 20,
        1 << 21,
        7,
    )
}

#[test]
fn an_array_record_round_trips() {
    let record = array_record();
    let bytes = record.to_le_bytes();
    assert_eq!(bytes.len(), RaidArrayRecord::WIRE_LEN);
    let decoded = RaidArrayRecord::from_bytes(&bytes).expect("its own encoding");
    assert_eq!(decoded, record);
    assert_eq!(decoded.array(), uuid());
    assert_eq!(decoded.level(), RaidLevel::Parity);
    assert_eq!(decoded.health(), ArrayHealth::Recovering);
    assert!(decoded.scrubbing());
    assert!(decoded.resyncing());
    assert_eq!(decoded.member_count(), 4);
    assert_eq!(decoded.active_members(), 3);
    assert_eq!(decoded.block_size(), 512);
    assert_eq!(decoded.chunk_blocks(), 128);
    assert_eq!(decoded.block_count(), 1 << 40);
    assert_eq!(decoded.endpoint(), 0x5241_2001);
    assert_eq!(decoded.node(), 42);
    assert_eq!(decoded.scrub_cursor(), 1 << 20);
    assert_eq!(decoded.resync_cursor(), 1 << 21);
    assert_eq!(decoded.generation(), 7);
}

#[test]
fn an_array_record_fails_closed_on_every_malformed_form() {
    let good = array_record().to_le_bytes();
    assert_eq!(
        RaidArrayRecord::from_bytes(&good[..RaidArrayRecord::WIRE_LEN - 1]),
        Err(Errno::BufferTooSmall)
    );

    let mut bad_level = good;
    bad_level[16] = 0xfe;
    assert_eq!(
        RaidArrayRecord::from_bytes(&bad_level),
        Err(Errno::OutOfRange)
    );

    let mut bad_health = good;
    bad_health[17] = 0xfe;
    assert_eq!(
        RaidArrayRecord::from_bytes(&bad_health),
        Err(Errno::OutOfRange)
    );

    let mut reserved_flag = good;
    reserved_flag[18] |= 0x80;
    assert_eq!(
        RaidArrayRecord::from_bytes(&reserved_flag),
        Err(Errno::BadMagic)
    );

    for index in [19usize, 52, 53, 54, 55] {
        let mut dirty = good;
        dirty[index] = 1;
        assert_eq!(
            RaidArrayRecord::from_bytes(&dirty),
            Err(Errno::BadMagic),
            "byte {index} was ignored"
        );
    }
}

/// A fully-populated member record.
fn member_record() -> RaidMemberRecord {
    RaidMemberRecord::new(
        uuid(),
        RaidMemberDisposition::Resyncing,
        2,
        77,
        0x5241_3001,
        1 << 33,
        4096,
        9,
    )
}

#[test]
fn a_member_record_round_trips() {
    let record = member_record();
    let bytes = record.to_le_bytes();
    assert_eq!(bytes.len(), RaidMemberRecord::WIRE_LEN);
    let decoded = RaidMemberRecord::from_bytes(&bytes).expect("its own encoding");
    assert_eq!(decoded, record);
    assert_eq!(decoded.disposition(), RaidMemberDisposition::Resyncing);
    assert_eq!(decoded.slot(), 2);
    assert_eq!(decoded.node(), 77);
    assert_eq!(decoded.endpoint(), 0x5241_3001);
    assert_eq!(decoded.block_count(), 1 << 33);
    assert_eq!(decoded.block_size(), 4096);
    assert_eq!(decoded.generation(), 9);
    assert!(!decoded.is_unaffiliated());
}

#[test]
fn an_unaffiliated_candidate_names_no_array_and_no_slot() {
    let candidate = RaidMemberRecord::new(
        [0u8; 16],
        RaidMemberDisposition::Candidate,
        RAID_SLOT_NONE,
        5,
        0x5241_3002,
        1 << 20,
        512,
        0,
    );
    let decoded = RaidMemberRecord::from_bytes(&candidate.to_le_bytes()).expect("its own encoding");
    assert!(decoded.is_unaffiliated());
    assert_eq!(decoded.slot(), RAID_SLOT_NONE);
    assert!(decoded.disposition().is_available());
}

#[test]
fn a_member_record_fails_closed_on_every_malformed_form() {
    let good = member_record().to_le_bytes();
    assert_eq!(
        RaidMemberRecord::from_bytes(&good[..RaidMemberRecord::WIRE_LEN - 1]),
        Err(Errno::BufferTooSmall)
    );

    let mut bad_disposition = good;
    bad_disposition[16] = 0xfe;
    assert_eq!(
        RaidMemberRecord::from_bytes(&bad_disposition),
        Err(Errno::OutOfRange)
    );

    for index in [17usize, 44, 45, 46, 47] {
        let mut dirty = good;
        dirty[index] = 1;
        assert_eq!(
            RaidMemberRecord::from_bytes(&dirty),
            Err(Errno::BadMagic),
            "byte {index} was ignored"
        );
    }
}

#[test]
fn a_disposition_round_trips_and_only_a_candidate_is_available() {
    for disposition in [
        RaidMemberDisposition::Candidate,
        RaidMemberDisposition::Held,
        RaidMemberDisposition::InSync,
        RaidMemberDisposition::Resyncing,
        RaidMemberDisposition::Faulted,
    ] {
        assert_eq!(
            RaidMemberDisposition::from_u8(disposition.as_u8()),
            Ok(disposition)
        );
        assert_eq!(
            disposition.is_available(),
            disposition == RaidMemberDisposition::Candidate
        );
    }
    assert_eq!(RaidMemberDisposition::from_u8(5), Err(Errno::OutOfRange));
}

#[test]
fn a_live_members_state_maps_to_exactly_one_reported_disposition() {
    assert_eq!(
        RaidMemberDisposition::for_member_state(MemberState::InSync),
        Some(RaidMemberDisposition::InSync)
    );
    assert_eq!(
        RaidMemberDisposition::for_member_state(MemberState::Resyncing),
        Some(RaidMemberDisposition::Resyncing)
    );
    assert_eq!(
        RaidMemberDisposition::for_member_state(MemberState::Faulted),
        Some(RaidMemberDisposition::Faulted)
    );
    // An absent slot holds no device, so it has no record at all.
    assert_eq!(
        RaidMemberDisposition::for_member_state(MemberState::Absent),
        None
    );
}

#[test]
fn the_reply_bound_holds_a_full_page_of_the_widest_record() {
    let widest = RaidArrayRecord::WIRE_LEN.max(RaidMemberRecord::WIRE_LEN);
    assert_eq!(
        RAID_CONTROL_MAX_REPLY,
        STATUS_REPLY_LEN + PAGE_HEADER_LEN + RAID_LIST_LIMIT_MAX as usize * widest
    );
}
