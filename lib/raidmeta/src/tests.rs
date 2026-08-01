//! Host tests for the RAID array superblock codec and reassembly logic.

use super::{
    distinct_arrays, ArrayIdentity, ArraySuperblock, AssemblyError, Candidate, CandidateVerdict,
    RaidLevel, RejectReason, SlotDisposition, SuperblockError, MAGIC, OFF_CHECKSUM, OFF_LEVEL,
    OFF_MAGIC, OFF_VERSION, WIRE_LEN,
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
        chunk_blocks: 0,
    }
}

/// A striped (RAID0) superblock for `array`, `slot` of `count`, at
/// `generation`, with stripe unit `chunk_blocks`.
fn stripe_sb(
    array: [u8; 16],
    count: u16,
    slot: u16,
    generation: u64,
    chunk_blocks: u32,
) -> ArraySuperblock {
    ArraySuperblock {
        array_uuid: array,
        raid_level: RaidLevel::Stripe,
        member_count: count,
        member_slot: slot,
        geometry: GEO,
        generation,
        updated_at: Time64::from_secs(1_700_000_000),
        chunk_blocks,
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
        chunk_blocks: 0,
    };
    let bytes = original.encode();
    assert_eq!(bytes.len(), WIRE_LEN);
    assert_eq!(ArraySuperblock::decode(&bytes).unwrap(), original);
}

#[test]
fn a_striped_superblock_round_trips_with_its_stripe_unit() {
    let original = stripe_sb(UUID_A, 3, 1, 42, 64);
    let bytes = original.encode();
    let decoded = ArraySuperblock::decode(&bytes).unwrap();
    assert_eq!(decoded, original);
    assert_eq!(decoded.raid_level, RaidLevel::Stripe);
    assert_eq!(decoded.chunk_blocks, 64);
}

#[test]
fn decode_rejects_a_level_stripe_unit_mismatch() {
    // A striped level with a zero stripe unit, and a mirror with a non-zero
    // one, each contradict themselves: both fail closed rather than being
    // trusted (`AGENTS.md` §5.4).
    let mut striped_zero = stripe_sb(UUID_A, 2, 0, 1, 0);
    striped_zero.chunk_blocks = 0;
    assert_eq!(
        ArraySuperblock::decode(&striped_zero.encode()),
        Err(SuperblockError::BadStripeChunk)
    );

    let mut mirror_with_chunk = sb(UUID_A, 2, 0, 1);
    mirror_with_chunk.chunk_blocks = 16;
    assert_eq!(
        ArraySuperblock::decode(&mirror_with_chunk.encode()),
        Err(SuperblockError::BadStripeChunk)
    );
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
    assert_eq!(
        RaidLevel::from_u8(RaidLevel::Stripe.as_u8()),
        Ok(RaidLevel::Stripe)
    );
    assert_eq!(
        RaidLevel::from_u8(RaidLevel::Parity.as_u8()),
        Ok(RaidLevel::Parity)
    );
    assert_eq!(
        RaidLevel::from_u8(RaidLevel::DualParity.as_u8()),
        Ok(RaidLevel::DualParity)
    );
    assert_eq!(
        RaidLevel::from_u8(RaidLevel::TripleParity.as_u8()),
        Ok(RaidLevel::TripleParity)
    );
    assert_eq!(
        RaidLevel::from_u8(RaidLevel::Raid10.as_u8()),
        Ok(RaidLevel::Raid10)
    );
    assert!(!RaidLevel::Mirror.is_striped());
    assert!(RaidLevel::Stripe.is_striped());
    assert!(RaidLevel::Parity.is_striped());
    assert!(RaidLevel::DualParity.is_striped());
    assert!(RaidLevel::TripleParity.is_striped());
    assert!(RaidLevel::Raid10.is_striped());
    for raw in [0u8, 7, 8, 255] {
        assert_eq!(
            RaidLevel::from_u8(raw),
            Err(SuperblockError::UnknownRaidLevel)
        );
    }
}

/// A parity (RAID5) superblock for `array`, `slot` of `count`, at
/// `generation`, with stripe unit `chunk_blocks`.
fn parity_sb(
    array: [u8; 16],
    count: u16,
    slot: u16,
    generation: u64,
    chunk_blocks: u32,
) -> ArraySuperblock {
    ArraySuperblock {
        array_uuid: array,
        raid_level: RaidLevel::Parity,
        member_count: count,
        member_slot: slot,
        geometry: GEO,
        generation,
        updated_at: Time64::from_secs(1_700_000_000),
        chunk_blocks,
    }
}

#[test]
fn a_parity_superblock_round_trips_and_rejects_a_zero_stripe_unit() {
    let original = parity_sb(UUID_A, 4, 2, 7, 128);
    let decoded = ArraySuperblock::decode(&original.encode()).unwrap();
    assert_eq!(decoded, original);
    assert_eq!(decoded.raid_level, RaidLevel::Parity);
    assert_eq!(decoded.chunk_blocks, 128);

    // A parity level with a zero stripe unit contradicts itself: fail closed.
    let mut parity_zero = parity_sb(UUID_A, 3, 0, 1, 0);
    parity_zero.chunk_blocks = 0;
    assert_eq!(
        ArraySuperblock::decode(&parity_zero.encode()),
        Err(SuperblockError::BadStripeChunk)
    );
}

#[test]
fn a_dual_parity_superblock_round_trips_and_rejects_a_zero_stripe_unit() {
    let mut original = parity_sb(UUID_A, 5, 3, 9, 64);
    original.raid_level = RaidLevel::DualParity;
    let decoded = ArraySuperblock::decode(&original.encode()).unwrap();
    assert_eq!(decoded, original);
    assert_eq!(decoded.raid_level, RaidLevel::DualParity);
    assert_eq!(decoded.chunk_blocks, 64);

    // A double-parity level with a zero stripe unit contradicts itself.
    let mut zero = parity_sb(UUID_A, 4, 0, 1, 0);
    zero.raid_level = RaidLevel::DualParity;
    zero.chunk_blocks = 0;
    assert_eq!(
        ArraySuperblock::decode(&zero.encode()),
        Err(SuperblockError::BadStripeChunk)
    );
}

#[test]
fn a_triple_parity_superblock_round_trips_and_rejects_a_zero_stripe_unit() {
    let mut original = parity_sb(UUID_A, 6, 4, 11, 64);
    original.raid_level = RaidLevel::TripleParity;
    let decoded = ArraySuperblock::decode(&original.encode()).unwrap();
    assert_eq!(decoded, original);
    assert_eq!(decoded.raid_level, RaidLevel::TripleParity);
    assert_eq!(decoded.chunk_blocks, 64);

    // A triple-parity level with a zero stripe unit contradicts itself.
    let mut zero = parity_sb(UUID_A, 5, 0, 1, 0);
    zero.raid_level = RaidLevel::TripleParity;
    zero.chunk_blocks = 0;
    assert_eq!(
        ArraySuperblock::decode(&zero.encode()),
        Err(SuperblockError::BadStripeChunk)
    );
}

#[test]
fn decode_rejects_too_few_and_too_many_members_for_triple_parity() {
    // RAID-TP needs five (two data + P + Q + R); four is too few.
    let mut triple_four = parity_sb(UUID_A, 4, 0, 1, 64);
    triple_four.raid_level = RaidLevel::TripleParity;
    assert_eq!(
        ArraySuperblock::decode(&triple_four.encode()),
        Err(SuperblockError::MemberCountOutOfRange)
    );
    // 259 slots is 256 data members, one more than the syndromes can keep
    // distinct coefficients for.
    let mut triple_many = parity_sb(UUID_A, 259, 0, 1, 64);
    triple_many.raid_level = RaidLevel::TripleParity;
    assert_eq!(
        ArraySuperblock::decode(&triple_many.encode()),
        Err(SuperblockError::MemberCountOutOfRange)
    );
}

#[test]
fn a_raid10_superblock_round_trips_and_rejects_an_odd_member_count() {
    let mut original = parity_sb(UUID_A, 4, 2, 9, 128);
    original.raid_level = RaidLevel::Raid10;
    let decoded = ArraySuperblock::decode(&original.encode()).unwrap();
    assert_eq!(decoded, original);
    assert_eq!(decoded.raid_level, RaidLevel::Raid10);
    assert_eq!(decoded.chunk_blocks, 128);

    // A RAID10 with a zero stripe unit contradicts its striped nature.
    let mut zero = parity_sb(UUID_A, 4, 0, 1, 0);
    zero.raid_level = RaidLevel::Raid10;
    zero.chunk_blocks = 0;
    assert_eq!(
        ArraySuperblock::decode(&zero.encode()),
        Err(SuperblockError::BadStripeChunk)
    );

    // An odd member count cannot pair copies: refused at the decode boundary
    // exactly as the engine's `assemble` would.
    let mut odd = parity_sb(UUID_A, 5, 0, 1, 64);
    odd.raid_level = RaidLevel::Raid10;
    assert_eq!(
        ArraySuperblock::decode(&odd.encode()),
        Err(SuperblockError::MemberCountOutOfRange)
    );

    // A two-member RAID10 is a plain mirror, not a stripe of mirrors: below
    // the four-member floor it is refused.
    let mut too_small = parity_sb(UUID_A, 2, 0, 1, 64);
    too_small.raid_level = RaidLevel::Raid10;
    assert_eq!(
        ArraySuperblock::decode(&too_small.encode()),
        Err(SuperblockError::MemberCountOutOfRange)
    );
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

#[test]
fn bump_generation_increments_and_preserves_shape() {
    let candidates = [candidate(10, sb(UUID_A, 2, 0, 8))];
    let id = ArrayIdentity::resolve(UUID_A, &candidates).unwrap();
    let next = id.bump_generation();
    assert_eq!(next.generation, id.generation + 1);
    assert_eq!(next.array_uuid, id.array_uuid);
    assert_eq!(next.raid_level, id.raid_level);
    assert_eq!(next.member_count, id.member_count);
    assert_eq!(next.geometry, id.geometry);
}

#[test]
fn bump_generation_saturates_at_the_ceiling_rather_than_wrapping() {
    let candidates = [candidate(10, sb(UUID_A, 2, 0, u64::MAX))];
    let id = ArrayIdentity::resolve(UUID_A, &candidates).unwrap();
    assert_eq!(id.generation, u64::MAX);
    // Saturates rather than wrapping to 0, which an already-written member
    // could match and be wrongly trusted as current.
    assert_eq!(id.bump_generation().generation, u64::MAX);
}

#[test]
fn member_superblock_round_trips_through_decode_and_resolves_current() {
    let candidates = [candidate(10, sb(UUID_A, 3, 0, 8))];
    let id = ArrayIdentity::resolve(UUID_A, &candidates).unwrap();
    let stamp = Time64::new(1_800_000_000, 42).unwrap();
    let written = id.member_superblock(2, stamp).unwrap();
    assert_eq!(written.array_uuid, UUID_A);
    assert_eq!(written.member_slot, 2);
    assert_eq!(written.member_count, 3);
    assert_eq!(written.generation, id.generation);
    assert_eq!(written.updated_at, stamp);
    // It survives an encode/decode round trip (a real on-disk write) …
    assert_eq!(ArraySuperblock::decode(&written.encode()).unwrap(), written);
    // … and resolves as a current (in-sync) member of the same array.
    let round = [candidate(10, written)];
    let id2 = ArrayIdentity::resolve(UUID_A, &round).unwrap();
    assert_eq!(
        id2.verdict_of(&round, 0),
        CandidateVerdict::Placed {
            slot: 2,
            in_sync: true
        }
    );
}

#[test]
fn member_superblock_fails_closed_on_an_out_of_range_slot() {
    let candidates = [candidate(10, sb(UUID_A, 2, 0, 8))];
    let id = ArrayIdentity::resolve(UUID_A, &candidates).unwrap();
    let stamp = Time64::from_secs(1_700_000_100);
    assert_eq!(id.member_superblock(2, stamp), None);
    assert_eq!(id.member_superblock(u16::MAX, stamp), None);
    assert!(id.member_superblock(1, stamp).is_some());
}

#[test]
fn a_member_absent_for_a_membership_bump_returns_stale() {
    // A two-member array at generation 8; both current.
    let id = ArrayIdentity::resolve(
        UUID_A,
        &[
            candidate(10, sb(UUID_A, 2, 0, 8)),
            candidate(11, sb(UUID_A, 2, 1, 8)),
        ],
    )
    .unwrap();
    assert_eq!(id.generation, 8);

    // Member 1 faults and drops out. The array records the membership change
    // and re-stamps only the *surviving* member (slot 0) at the new
    // generation; the absent member is left at generation 8.
    let stamp = Time64::from_secs(1_700_000_200);
    let next = id.bump_generation();
    let survivor = next.member_superblock(0, stamp).unwrap();
    assert_eq!(next.generation, 9);
    assert_eq!(survivor.generation, 9);

    // Member 1 comes back still carrying its stale generation-8 superblock.
    let candidates = [candidate(10, survivor), candidate(11, sb(UUID_A, 2, 1, 8))];
    let reassembled = ArrayIdentity::resolve(UUID_A, &candidates).unwrap();
    assert_eq!(reassembled.generation, 9);
    let mut slots = [SlotDisposition::Missing; 2];
    reassembled.fill_slots(&candidates, &mut slots).unwrap();
    assert_eq!(
        slots[0],
        SlotDisposition::Present {
            tag: 10,
            in_sync: true
        }
    );
    // The returned member missed a write while it was gone: it is a stale
    // rebuild target, never trusted as a current read source.
    assert_eq!(
        slots[1],
        SlotDisposition::Present {
            tag: 11,
            in_sync: false
        }
    );
}

#[test]
fn promoting_a_rebuilt_member_makes_it_current_again() {
    // The array is at generation 9 with one current member; the other has been
    // rebuilt and is being promoted back to current.
    let id = ArrayIdentity::resolve(
        UUID_A,
        &[
            candidate(10, sb(UUID_A, 2, 0, 9)),
            candidate(11, sb(UUID_A, 2, 1, 6)), // stale rebuild target
        ],
    )
    .unwrap();
    assert_eq!(id.generation, 9);
    let stamp = Time64::from_secs(1_700_000_300);
    let promoted = id.member_superblock(1, stamp).unwrap();
    let candidates = [candidate(10, sb(UUID_A, 2, 0, 9)), candidate(11, promoted)];
    let reassembled = ArrayIdentity::resolve(UUID_A, &candidates).unwrap();
    let mut slots = [SlotDisposition::Missing; 2];
    reassembled.fill_slots(&candidates, &mut slots).unwrap();
    // Both members are now current; the array is whole again.
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
            in_sync: true
        }
    );
}

/// Recompute and rewrite the trailing CRC after a test mutates the body, so a
/// test that targets a *semantic* rejection is not masked by the checksum
/// check firing first.
fn reseal(bytes: &mut [u8; WIRE_LEN]) {
    let crc = tairix_crc32c::checksum(&bytes[..OFF_CHECKSUM]);
    bytes[OFF_CHECKSUM..OFF_CHECKSUM + 4].copy_from_slice(&crc.to_le_bytes());
}

const UUID_C: [u8; 16] = [0xC3; 16];

#[test]
fn distinct_arrays_of_empty_set_is_empty() {
    let candidates: [Candidate; 0] = [];
    assert_eq!(distinct_arrays(&candidates).count(), 0);
}

#[test]
fn distinct_arrays_of_one_array_yields_it_once() {
    // Three members of the same array collapse to a single distinct identity.
    let candidates = [
        candidate(0, sb(UUID_A, 3, 0, 5)),
        candidate(1, sb(UUID_A, 3, 1, 5)),
        candidate(2, sb(UUID_A, 3, 2, 5)),
    ];
    assert!(distinct_arrays(&candidates).eq([UUID_A]));
}

#[test]
fn distinct_arrays_partitions_a_mixed_set_in_first_appearance_order() {
    // Members of three arrays are interleaved across the discovered devices;
    // each array is enumerated exactly once, in the order it first appears.
    let candidates = [
        candidate(0, sb(UUID_B, 2, 0, 1)),
        candidate(1, sb(UUID_A, 2, 0, 9)),
        candidate(2, sb(UUID_B, 2, 1, 1)),
        candidate(3, sb(UUID_C, 1, 0, 3)),
        candidate(4, sb(UUID_A, 2, 1, 9)),
    ];
    assert!(distinct_arrays(&candidates).eq([UUID_B, UUID_A, UUID_C]));
}

#[test]
fn distinct_arrays_composes_with_resolve_for_every_array() {
    // The intended use: enumerate the arrays, then resolve each one. Every
    // yielded UUID resolves (never `NoMembers`) because it came from a member.
    let candidates = [
        candidate(0, sb(UUID_A, 2, 0, 4)),
        candidate(1, sb(UUID_B, 1, 0, 7)),
        candidate(2, sb(UUID_A, 2, 1, 4)),
    ];
    let mut resolved = 0;
    for uuid in distinct_arrays(&candidates) {
        let id = ArrayIdentity::resolve(uuid, &candidates).expect("a yielded array has members");
        assert_eq!(id.array_uuid, uuid);
        resolved += 1;
    }
    assert_eq!(resolved, 2);
}

#[test]
fn raid_level_member_bounds_are_the_shared_source() {
    assert_eq!(RaidLevel::Mirror.min_members(), 1);
    assert_eq!(RaidLevel::Stripe.min_members(), 1);
    assert_eq!(RaidLevel::Parity.min_members(), 3);
    assert_eq!(RaidLevel::DualParity.min_members(), 4);
    assert_eq!(RaidLevel::TripleParity.min_members(), 5);
    // RAID10 needs two two-copy pairs (four members) to be a stripe of
    // mirrors rather than a plain mirror.
    assert_eq!(RaidLevel::Raid10.min_members(), 4);
    // Only the GF(2^8) parity levels have a real ceiling: 255 data members
    // plus their syndrome chunks (RAID6 = 257 slots, RAID-TP = 258). Every
    // other level is bounded only by the on-disk `u16` member-count field.
    assert_eq!(RaidLevel::Mirror.max_members(), u16::MAX);
    assert_eq!(RaidLevel::Stripe.max_members(), u16::MAX);
    assert_eq!(RaidLevel::Parity.max_members(), u16::MAX);
    assert_eq!(RaidLevel::DualParity.max_members(), 257);
    assert_eq!(RaidLevel::TripleParity.max_members(), 258);
    assert_eq!(RaidLevel::Raid10.max_members(), u16::MAX);
}

#[test]
fn decode_rejects_too_few_members_for_the_level() {
    // RAID5 needs three members (two data + a parity chunk); two describes an
    // array that cannot exist and is refused at the decode boundary.
    let parity_two = parity_sb(UUID_A, 2, 0, 1, 64);
    assert_eq!(
        ArraySuperblock::decode(&parity_two.encode()),
        Err(SuperblockError::MemberCountOutOfRange)
    );
    // RAID6 needs four (two data + P + Q); three is too few.
    let mut dual_three = parity_sb(UUID_A, 3, 0, 1, 64);
    dual_three.raid_level = RaidLevel::DualParity;
    assert_eq!(
        ArraySuperblock::decode(&dual_three.encode()),
        Err(SuperblockError::MemberCountOutOfRange)
    );
}

#[test]
fn decode_rejects_too_many_members_for_double_parity() {
    // 258 slots is 256 data members, one more than the Q syndrome can keep
    // distinct coefficients for — an unbuildable array, refused up front.
    let mut too_many = parity_sb(UUID_A, 258, 0, 1, 64);
    too_many.raid_level = RaidLevel::DualParity;
    assert_eq!(
        ArraySuperblock::decode(&too_many.encode()),
        Err(SuperblockError::MemberCountOutOfRange)
    );
}

#[test]
fn decode_admits_the_minimum_and_boundary_member_counts_per_level() {
    // Each level's smallest valid array decodes cleanly.
    assert!(ArraySuperblock::decode(&sb(UUID_A, 1, 0, 1).encode()).is_ok());
    assert!(ArraySuperblock::decode(&stripe_sb(UUID_A, 1, 0, 1, 32).encode()).is_ok());
    assert!(ArraySuperblock::decode(&parity_sb(UUID_A, 3, 0, 1, 64).encode()).is_ok());
    let mut dual_min = parity_sb(UUID_A, 4, 0, 1, 64);
    dual_min.raid_level = RaidLevel::DualParity;
    assert!(ArraySuperblock::decode(&dual_min.encode()).is_ok());
    // The RAID6 upper boundary (257 slots = 255 data + P + Q) is admitted.
    let mut dual_max = parity_sb(UUID_A, 257, 0, 1, 64);
    dual_max.raid_level = RaidLevel::DualParity;
    assert!(ArraySuperblock::decode(&dual_max.encode()).is_ok());
    // RAID-TP's minimum (five: two data + P + Q + R) and upper boundary (258
    // slots = 255 data + P + Q + R) both decode.
    let mut triple_min = parity_sb(UUID_A, 5, 0, 1, 64);
    triple_min.raid_level = RaidLevel::TripleParity;
    assert!(ArraySuperblock::decode(&triple_min.encode()).is_ok());
    let mut triple_max = parity_sb(UUID_A, 258, 0, 1, 64);
    triple_max.raid_level = RaidLevel::TripleParity;
    assert!(ArraySuperblock::decode(&triple_max.encode()).is_ok());
}

#[test]
fn is_redundant_is_the_shared_answer_for_every_level() {
    // Only the RAID0 stripe holds nothing spare, so only it has nothing to
    // scrub from, rebuild from, or hot-swap.
    assert!(!RaidLevel::Stripe.is_redundant());
    assert!(RaidLevel::Mirror.is_redundant());
    assert!(RaidLevel::Parity.is_redundant());
    assert!(RaidLevel::DualParity.is_redundant());
    assert!(RaidLevel::TripleParity.is_redundant());
    assert!(RaidLevel::Raid10.is_redundant());
}

#[test]
fn data_members_is_the_shared_usable_width_per_level() {
    // A mirror presents one copy's worth regardless of how many copies exist.
    assert_eq!(RaidLevel::Mirror.data_members(1), Some(1));
    assert_eq!(RaidLevel::Mirror.data_members(4), Some(1));
    // A stripe concatenates every member.
    assert_eq!(RaidLevel::Stripe.data_members(1), Some(1));
    assert_eq!(RaidLevel::Stripe.data_members(6), Some(6));
    // Single parity reserves one member's chunk for parity.
    assert_eq!(RaidLevel::Parity.data_members(3), Some(2));
    assert_eq!(RaidLevel::Parity.data_members(8), Some(7));
    // Double parity reserves two (P and Q).
    assert_eq!(RaidLevel::DualParity.data_members(4), Some(2));
    assert_eq!(RaidLevel::DualParity.data_members(10), Some(8));
    // Triple parity reserves three (P, Q, and R).
    assert_eq!(RaidLevel::TripleParity.data_members(5), Some(2));
    assert_eq!(RaidLevel::TripleParity.data_members(10), Some(7));
    // A RAID10 stripe of two-copy mirrors presents half its members.
    assert_eq!(RaidLevel::Raid10.data_members(4), Some(2));
    assert_eq!(RaidLevel::Raid10.data_members(10), Some(5));
}

#[test]
fn data_members_fails_closed_below_the_structural_floor() {
    // A width with no data member at all yields `None` rather than underflow:
    // an empty stripe, and parity levels below the count that leaves any data.
    assert_eq!(RaidLevel::Stripe.data_members(0), None);
    assert_eq!(RaidLevel::Parity.data_members(1), None);
    assert_eq!(RaidLevel::Parity.data_members(0), None);
    assert_eq!(RaidLevel::DualParity.data_members(2), None);
    assert_eq!(RaidLevel::DualParity.data_members(0), None);
    assert_eq!(RaidLevel::TripleParity.data_members(3), None);
    assert_eq!(RaidLevel::TripleParity.data_members(0), None);
    // A RAID10 with an odd member count cannot pair its copies.
    assert_eq!(RaidLevel::Raid10.data_members(5), None);
    assert_eq!(RaidLevel::Raid10.data_members(0), None);
    // The mirror is the identity case: always one copy's worth, even at zero
    // (an empty mirror is rejected earlier by `assemble`, not here).
    assert_eq!(RaidLevel::Mirror.data_members(0), Some(1));
}

#[test]
fn logical_block_count_is_per_member_times_data_members() {
    // Capacity is each member's block count times the usable width.
    assert_eq!(RaidLevel::Mirror.logical_block_count(1000, 3), Some(1000));
    assert_eq!(RaidLevel::Stripe.logical_block_count(1000, 3), Some(3000));
    assert_eq!(RaidLevel::Parity.logical_block_count(1000, 4), Some(3000));
    assert_eq!(
        RaidLevel::DualParity.logical_block_count(1000, 5),
        Some(3000)
    );
}

#[test]
fn logical_block_count_fails_closed_on_overflow_and_underwidth() {
    // A product that would overflow `u64` fails closed rather than wrapping to
    // a smaller array that would truncate addresses.
    assert_eq!(RaidLevel::Stripe.logical_block_count(u64::MAX, 2), None);
    assert_eq!(
        RaidLevel::Parity.logical_block_count(u64::MAX, 3),
        None,
        "u64::MAX * 2 overflows"
    );
    // Below the structural floor there is no data member to multiply by.
    assert_eq!(RaidLevel::DualParity.logical_block_count(1000, 2), None);
    assert_eq!(RaidLevel::Stripe.logical_block_count(1000, 0), None);
}
