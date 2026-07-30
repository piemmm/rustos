//! Host tests for the RAID array superblock codec and reassembly logic.

use super::{
    ArrayIdentity, ArraySuperblock, AssemblyError, Candidate, CandidateVerdict, RaidLevel,
    RejectReason, SlotDisposition, SuperblockError, MAGIC, OFF_CHECKSUM, OFF_LEVEL, OFF_MAGIC,
    OFF_VERSION, WIRE_LEN,
};
use tairix_abi::driver::block::BlockGeometry;
use tairix_abi::time::Time64;

const UUID_A: [u8; 16] = [0xA1; 16];
const UUID_B: [u8; 16] = [0xB2; 16];
const GEO: BlockGeometry = BlockGeometry {
    block_size: 512,
    block_count: 2048,
};

/// A superblock for `array`, `slot` of `count`, at `generation`.
fn sb(array: [u8; 16], count: u16, slot: u16, generation: u64) -> ArraySuperblock {
    ArraySuperblock {
        array_uuid: array,
        raid_level: RaidLevel::Mirror,
        member_count: count,
        member_slot: slot,
        geometry: GEO,
        generation,
        updated_at: Time64::from_secs(1_700_000_000),
    }
}

fn candidate(tag: usize, superblock: ArraySuperblock) -> Candidate {
    Candidate { tag, superblock }
}

#[test]
fn encode_decode_round_trips_every_field() {
    let original = ArraySuperblock {
        array_uuid: UUID_A,
        raid_level: RaidLevel::Mirror,
        member_count: 4,
        member_slot: 2,
        geometry: BlockGeometry {
            block_size: 4096,
            block_count: 0x1_0000_0001,
        },
        generation: 0xDEAD_BEEF_0000_0007,
        updated_at: Time64::new(1_800_000_000, 123_456_789).unwrap(),
    };
    let bytes = original.encode();
    assert_eq!(bytes.len(), WIRE_LEN);
    assert_eq!(ArraySuperblock::decode(&bytes).unwrap(), original);
}

#[test]
fn decode_accepts_a_larger_buffer_reading_only_the_leading_record() {
    let original = sb(UUID_A, 2, 0, 9);
    let mut block = [0u8; 512];
    block[..WIRE_LEN].copy_from_slice(&original.encode());
    assert_eq!(ArraySuperblock::decode(&block).unwrap(), original);
}

#[test]
fn decode_rejects_a_short_buffer() {
    let bytes = sb(UUID_A, 2, 0, 1).encode();
    assert_eq!(
        ArraySuperblock::decode(&bytes[..WIRE_LEN - 1]),
        Err(SuperblockError::TooSmall)
    );
}

#[test]
fn decode_rejects_a_bad_magic() {
    let mut bytes = sb(UUID_A, 2, 0, 1).encode();
    bytes[OFF_MAGIC] ^= 0xFF;
    // The magic corruption also breaks the CRC, but the magic check runs
    // first and is the reported reason.
    assert_eq!(
        ArraySuperblock::decode(&bytes),
        Err(SuperblockError::BadMagic)
    );
    assert_eq!(&bytes[OFF_MAGIC + 1..OFF_MAGIC + 8], &MAGIC[1..]);
}

#[test]
fn decode_rejects_an_unknown_version() {
    let mut bytes = sb(UUID_A, 2, 0, 1).encode();
    bytes[OFF_VERSION] = 0xFE;
    bytes[OFF_VERSION + 1] = 0xCA;
    assert_eq!(
        ArraySuperblock::decode(&bytes),
        Err(SuperblockError::UnsupportedVersion)
    );
}

#[test]
fn decode_rejects_a_corrupt_body_via_the_checksum() {
    let mut bytes = sb(UUID_A, 2, 0, 1).encode();
    // Flip a payload bit that leaves magic and version intact, so only the
    // CRC catches it.
    bytes[super::OFF_GENERATION] ^= 0x01;
    assert_eq!(
        ArraySuperblock::decode(&bytes),
        Err(SuperblockError::BadChecksum)
    );
}

#[test]
fn decode_rejects_an_unknown_raid_level() {
    let mut bytes = sb(UUID_A, 2, 0, 1).encode();
    bytes[OFF_LEVEL] = 0; // no level is 0
    reseal(&mut bytes);
    assert_eq!(
        ArraySuperblock::decode(&bytes),
        Err(SuperblockError::UnknownRaidLevel)
    );
}

#[test]
fn decode_rejects_a_zero_member_count() {
    let mut bytes = sb(UUID_A, 2, 0, 1).encode();
    bytes[super::OFF_MEMBER_COUNT] = 0;
    bytes[super::OFF_MEMBER_COUNT + 1] = 0;
    reseal(&mut bytes);
    assert_eq!(
        ArraySuperblock::decode(&bytes),
        Err(SuperblockError::ZeroMembers)
    );
}

#[test]
fn decode_rejects_a_slot_outside_the_array() {
    let mut bytes = sb(UUID_A, 2, 0, 1).encode();
    // slot = 2 in a 2-member array.
    bytes[super::OFF_MEMBER_SLOT] = 2;
    bytes[super::OFF_MEMBER_SLOT + 1] = 0;
    reseal(&mut bytes);
    assert_eq!(
        ArraySuperblock::decode(&bytes),
        Err(SuperblockError::SlotOutOfRange)
    );
}

#[test]
fn decode_rejects_a_degenerate_geometry() {
    for (bs, bc) in [(0u32, 2048u64), (512, 0)] {
        let mut candidate = sb(UUID_A, 2, 0, 1);
        candidate.geometry = BlockGeometry {
            block_size: bs,
            block_count: bc,
        };
        let bytes = candidate.encode();
        assert_eq!(
            ArraySuperblock::decode(&bytes),
            Err(SuperblockError::ZeroGeometry)
        );
    }
}

#[test]
fn decode_rejects_a_non_canonical_timestamp() {
    let mut bytes = sb(UUID_A, 2, 0, 1).encode();
    // Force the nanosecond field out of range (>= 1_000_000_000).
    bytes[super::OFF_UPDATED_AT + 8..super::OFF_UPDATED_AT + 12]
        .copy_from_slice(&2_000_000_000u32.to_le_bytes());
    reseal(&mut bytes);
    assert_eq!(
        ArraySuperblock::decode(&bytes),
        Err(SuperblockError::BadTimestamp)
    );
}

#[test]
fn timestamps_before_1970_and_after_2038_round_trip() {
    for secs in [-3_000_000_000i64, -1, 0, 4_000_000_000, 70_000_000_000] {
        let mut original = sb(UUID_A, 2, 0, 1);
        original.updated_at = Time64::new(secs, 999_999_999).unwrap();
        let bytes = original.encode();
        let decoded = ArraySuperblock::decode(&bytes).unwrap();
        assert_eq!(decoded.updated_at.secs(), secs);
        assert_eq!(decoded.updated_at.subsec_nanos(), 999_999_999);
    }
}

#[test]
fn raid_level_round_trips_and_fails_closed() {
    assert_eq!(
        RaidLevel::from_u8(RaidLevel::Mirror.as_u8()),
        Ok(RaidLevel::Mirror)
    );
    for raw in [0u8, 2, 3, 255] {
        assert_eq!(
            RaidLevel::from_u8(raw),
            Err(SuperblockError::UnknownRaidLevel)
        );
    }
}

#[test]
fn resolve_fails_closed_when_no_member_matches() {
    let candidates = [candidate(0, sb(UUID_B, 2, 0, 5))];
    assert_eq!(
        ArrayIdentity::resolve(UUID_A, &candidates),
        Err(AssemblyError::NoMembers)
    );
    assert_eq!(
        ArrayIdentity::resolve(UUID_A, &[]),
        Err(AssemblyError::NoMembers)
    );
}

#[test]
fn resolve_takes_the_freshest_generation_as_authoritative() {
    let candidates = [
        candidate(0, sb(UUID_A, 2, 0, 5)),
        candidate(1, sb(UUID_A, 2, 1, 8)),
    ];
    let id = ArrayIdentity::resolve(UUID_A, &candidates).unwrap();
    assert_eq!(id.array_uuid, UUID_A);
    assert_eq!(id.member_count, 2);
    assert_eq!(id.geometry, GEO);
    assert_eq!(id.generation, 8);
}

#[test]
fn two_current_members_assemble_all_in_sync() {
    let candidates = [
        candidate(10, sb(UUID_A, 2, 0, 8)),
        candidate(11, sb(UUID_A, 2, 1, 8)),
    ];
    let id = ArrayIdentity::resolve(UUID_A, &candidates).unwrap();
    let mut slots = [SlotDisposition::Missing; 2];
    id.fill_slots(&candidates, &mut slots).unwrap();
    assert_eq!(
        slots,
        [
            SlotDisposition::Present {
                tag: 10,
                in_sync: true
            },
            SlotDisposition::Present {
                tag: 11,
                in_sync: true
            },
        ]
    );
}

#[test]
fn a_member_behind_the_generation_is_marked_stale_for_rebuild() {
    let candidates = [
        candidate(10, sb(UUID_A, 2, 0, 8)), // current
        candidate(11, sb(UUID_A, 2, 1, 6)), // missed two membership changes
    ];
    let id = ArrayIdentity::resolve(UUID_A, &candidates).unwrap();
    let mut slots = [SlotDisposition::Missing; 2];
    id.fill_slots(&candidates, &mut slots).unwrap();
    assert_eq!(
        slots[0],
        SlotDisposition::Present {
            tag: 10,
            in_sync: true
        }
    );
    assert_eq!(
        slots[1],
        SlotDisposition::Present {
            tag: 11,
            in_sync: false
        }
    );
}

#[test]
fn a_missing_slot_is_reported_missing() {
    let candidates = [candidate(10, sb(UUID_A, 3, 1, 8))];
    let id = ArrayIdentity::resolve(UUID_A, &candidates).unwrap();
    let mut slots = [SlotDisposition::Missing; 3];
    id.fill_slots(&candidates, &mut slots).unwrap();
    assert_eq!(slots[0], SlotDisposition::Missing);
    assert_eq!(
        slots[1],
        SlotDisposition::Present {
            tag: 10,
            in_sync: true
        }
    );
    assert_eq!(slots[2], SlotDisposition::Missing);
}

#[test]
fn a_foreign_member_is_rejected_and_never_placed() {
    let candidates = [
        candidate(10, sb(UUID_A, 2, 0, 8)),
        candidate(11, sb(UUID_B, 2, 1, 8)), // different array
    ];
    let id = ArrayIdentity::resolve(UUID_A, &candidates).unwrap();
    assert_eq!(
        id.verdict_of(&candidates, 1),
        CandidateVerdict::Rejected(RejectReason::WrongArray)
    );
    let mut slots = [SlotDisposition::Missing; 2];
    id.fill_slots(&candidates, &mut slots).unwrap();
    assert_eq!(slots[1], SlotDisposition::Missing);
}

#[test]
fn a_shape_mismatched_member_is_rejected() {
    let mut odd = sb(UUID_A, 2, 1, 8);
    odd.geometry = BlockGeometry {
        block_size: 4096,
        block_count: 2048,
    };
    let candidates = [candidate(10, sb(UUID_A, 2, 0, 8)), candidate(11, odd)];
    let id = ArrayIdentity::resolve(UUID_A, &candidates).unwrap();
    // Authoritative shape came from the freshest self-consistent member set;
    // the geometry-divergent member is refused rather than corrupting the
    // array.
    assert_eq!(
        id.verdict_of(&candidates, 1),
        CandidateVerdict::Rejected(RejectReason::Mismatched)
    );
}

#[test]
fn a_duplicate_slot_keeps_the_fresher_copy() {
    let candidates = [
        candidate(10, sb(UUID_A, 2, 0, 6)), // stale duplicate of slot 0
        candidate(11, sb(UUID_A, 2, 0, 9)), // fresher duplicate of slot 0
        candidate(12, sb(UUID_A, 2, 1, 9)),
    ];
    let id = ArrayIdentity::resolve(UUID_A, &candidates).unwrap();
    assert_eq!(id.generation, 9);
    assert_eq!(
        id.verdict_of(&candidates, 0),
        CandidateVerdict::Rejected(RejectReason::Duplicate)
    );
    assert_eq!(
        id.verdict_of(&candidates, 1),
        CandidateVerdict::Placed {
            slot: 0,
            in_sync: true
        }
    );
    let mut slots = [SlotDisposition::Missing; 2];
    id.fill_slots(&candidates, &mut slots).unwrap();
    assert_eq!(
        slots[0],
        SlotDisposition::Present {
            tag: 11,
            in_sync: true
        }
    );
}

#[test]
fn a_duplicate_tie_breaks_on_the_lower_tag_deterministically() {
    // Two equally-fresh claimants of slot 0: the lower tag wins, the other is
    // the duplicate. The outcome must not depend on candidate order.
    let candidates = [
        candidate(20, sb(UUID_A, 2, 0, 9)),
        candidate(5, sb(UUID_A, 2, 0, 9)),
        candidate(30, sb(UUID_A, 2, 1, 9)),
    ];
    let id = ArrayIdentity::resolve(UUID_A, &candidates).unwrap();
    assert_eq!(
        id.verdict_of(&candidates, 0),
        CandidateVerdict::Rejected(RejectReason::Duplicate)
    );
    assert_eq!(
        id.verdict_of(&candidates, 1),
        CandidateVerdict::Placed {
            slot: 0,
            in_sync: true
        }
    );
}

#[test]
fn verdict_of_an_out_of_range_index_is_total_and_fails_closed() {
    let candidates = [candidate(10, sb(UUID_A, 2, 0, 8))];
    let id = ArrayIdentity::resolve(UUID_A, &candidates).unwrap();
    assert_eq!(
        id.verdict_of(&candidates, 99),
        CandidateVerdict::Rejected(RejectReason::WrongArray)
    );
}

#[test]
fn fill_slots_rejects_a_wrongly_sized_buffer() {
    let candidates = [candidate(10, sb(UUID_A, 2, 0, 8))];
    let id = ArrayIdentity::resolve(UUID_A, &candidates).unwrap();
    let mut slots = [SlotDisposition::Missing; 3];
    assert_eq!(
        id.fill_slots(&candidates, &mut slots),
        Err(AssemblyError::NoMembers)
    );
}

/// Recompute and rewrite the trailing CRC after a test mutates the body, so a
/// test that targets a *semantic* rejection is not masked by the checksum
/// check firing first.
fn reseal(bytes: &mut [u8; WIRE_LEN]) {
    let crc = tairix_crc32c::checksum(&bytes[..OFF_CHECKSUM]);
    bytes[OFF_CHECKSUM..OFF_CHECKSUM + 4].copy_from_slice(&crc.to_le_bytes());
}
