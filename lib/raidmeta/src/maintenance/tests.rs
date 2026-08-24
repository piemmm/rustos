//! Host tests for the array maintenance record.
//!
//! The record is read from a disk that may be failing, foreign, recycled, or
//! hostile, and its whole job is to let an array *skip* work it has already
//! done. Every test here therefore checks one of two things: that a good record
//! round-trips exactly, or that a record which cannot be fully vouched for is
//! discarded rather than half-trusted.

use super::{ArrayProgress, MaintenanceRecord, MaintenanceRecordError};
use crate::{ArrayIdentity, RaidLevel};
use tairix_abi::driver::block::BlockGeometry;
use tairix_abi::time::{Duration64, Time64};

/// The array every test resolves against: a four-member mirror at generation
/// seven.
fn identity() -> ArrayIdentity {
    ArrayIdentity {
        array_uuid: [0x5A; 16],
        raid_level: RaidLevel::Mirror,
        member_count: 4,
        geometry: BlockGeometry {
            block_size: 4096,
            block_count: 1_000_000,
        },
        generation: 7,
        chunk_blocks: 0,
    }
}

/// A record for [`identity`] with both cursors and a completion stamp set.
fn full_record() -> MaintenanceRecord {
    MaintenanceRecord::checkpoint(
        &identity(),
        42,
        ArrayProgress {
            scrub_cursor: Some(123_456),
            resync_cursor: Some(7_890),
        },
        Some(Time64::new(1_800_000_000, 123_456_789).expect("canonical")),
    )
}

#[test]
fn a_full_record_round_trips() {
    let record = full_record();
    let bytes = record.encode();
    assert_eq!(bytes.len(), MaintenanceRecord::WIRE_LEN);
    assert_eq!(MaintenanceRecord::decode(&bytes), Ok(record));
    // Trailing bytes beyond the record (the rest of the block) are ignored.
    let mut block = [0u8; 512];
    block[..MaintenanceRecord::WIRE_LEN].copy_from_slice(&bytes);
    assert_eq!(MaintenanceRecord::decode(&block), Ok(record));
}

#[test]
fn an_idle_record_round_trips_with_no_cursors() {
    // Nothing in progress and nothing ever verified: the all-absent case must
    // encode and decode as cleanly as the full one.
    let record = MaintenanceRecord::checkpoint(&identity(), 0, ArrayProgress::IDLE, None);
    assert!(!record.progress.is_active());
    let decoded = MaintenanceRecord::decode(&record.encode()).expect("idle record decodes");
    assert_eq!(decoded, record);
    assert_eq!(decoded.progress, ArrayProgress::IDLE);
    assert_eq!(decoded.last_scrub_completed, None);
}

#[test]
fn each_optional_field_survives_independently() {
    // The flags byte is only correct if each field's presence is carried
    // independently of the others, so exercise every combination.
    let stamp = Time64::new(1_750_000_000, 42).expect("canonical");
    for scrub in [None, Some(0), Some(u64::MAX)] {
        for resync in [None, Some(0), Some(999)] {
            for completed in [None, Some(stamp)] {
                let record = MaintenanceRecord::checkpoint(
                    &identity(),
                    1,
                    ArrayProgress {
                        scrub_cursor: scrub,
                        resync_cursor: resync,
                    },
                    completed,
                );
                let decoded = MaintenanceRecord::decode(&record.encode()).expect("record decodes");
                assert_eq!(decoded, record, "{scrub:?}/{resync:?}/{completed:?}");
            }
        }
    }
}

#[test]
fn a_cursor_inside_the_array_fits_and_one_past_the_end_does_not() {
    let span = 1_000u64;
    // The last real block is `span - 1`; `span` itself is the idle position,
    // which is spelled as no cursor at all.
    for cursor in [0u64, 1, span - 1] {
        assert!(ArrayProgress {
            scrub_cursor: Some(cursor),
            resync_cursor: Some(cursor),
        }
        .fits_span(span));
    }
    for cursor in [span, span + 1, u64::MAX] {
        assert!(
            !ArrayProgress {
                scrub_cursor: Some(cursor),
                resync_cursor: None,
            }
            .fits_span(span),
            "scrub cursor {cursor} must not fit a {span}-block array"
        );
        assert!(
            !ArrayProgress {
                scrub_cursor: None,
                resync_cursor: Some(cursor),
            }
            .fits_span(span),
            "resync cursor {cursor} must not fit a {span}-block array"
        );
    }
    // An absent cursor constrains nothing, even on a degenerate array.
    assert!(ArrayProgress::IDLE.fits_span(0));
    // …but on an array with no blocks there is no cursor that does fit.
    assert!(!ArrayProgress {
        scrub_cursor: Some(0),
        resync_cursor: None,
    }
    .fits_span(0));
}

#[test]
fn a_checkpoint_binds_the_record_to_its_array() {
    let record = full_record();
    assert_eq!(record.array_uuid, identity().array_uuid);
    assert_eq!(record.generation, identity().generation);
}

#[test]
fn progress_is_restored_for_the_same_array_at_the_same_generation() {
    let record = full_record();
    assert_eq!(record.progress_for(&identity()), record.progress);
}

#[test]
fn progress_from_a_foreign_array_is_ignored() {
    // A recycled or mis-cabled disk carrying another array's record must never
    // inject cursors into this array.
    let mut foreign = full_record();
    foreign.array_uuid = [0x11; 16];
    assert_eq!(foreign.progress_for(&identity()), ArrayProgress::IDLE);
}

#[test]
fn a_record_says_plainly_which_array_it_belongs_to() {
    // A consumer that reads more than the cursors — the completion stamp above
    // all — needs this before believing any of it: a foreign record claiming a
    // recent verification would otherwise talk an array out of verifying
    // itself, which is the same defect as injecting a cursor, in the opposite
    // direction.
    let mine = full_record();
    assert!(mine.belongs_to(&identity()));
    let mut foreign = full_record();
    foreign.array_uuid = [0x11; 16];
    assert!(!foreign.belongs_to(&identity()));
    // A record of this array from another generation still belongs to it: the
    // generation bounds the cursors, not the ownership.
    let mut older = full_record();
    older.generation = identity().generation - 1;
    assert!(older.belongs_to(&identity()));
}

#[test]
fn progress_from_an_earlier_generation_is_ignored() {
    // The record predates a membership change: a member has joined or left
    // since, so resuming its cursors could skip data the new member never
    // received. Start the passes afresh instead.
    let mut stale = full_record();
    stale.generation = identity().generation - 1;
    assert_eq!(stale.progress_for(&identity()), ArrayProgress::IDLE);
    // A record from a *later* generation than the assembled array is equally
    // not this array's business.
    let mut ahead = full_record();
    ahead.generation = identity().generation + 1;
    assert_eq!(ahead.progress_for(&identity()), ArrayProgress::IDLE);
}

#[test]
fn the_completion_stamp_survives_a_generation_change() {
    // Verification is a property of the data, not of the member set: a
    // membership change invalidates the cursors but not the knowledge that the
    // array was verified.
    let mut stale = full_record();
    stale.generation = identity().generation - 1;
    assert_eq!(stale.progress_for(&identity()), ArrayProgress::IDLE);
    let now = Time64::new(1_800_000_060, 123_456_789).expect("canonical");
    assert_eq!(stale.since_last_scrub_ns(now), 60_000_000_000);
}

#[test]
fn the_freshest_copy_of_several_members_wins() {
    let base = full_record();
    let mut later_sequence = base;
    later_sequence.sequence = base.sequence + 1;
    let mut later_generation = base;
    later_generation.generation = base.generation + 1;
    // A later checkpoint of the same membership.
    assert!(later_sequence.is_fresher_than(&base));
    assert!(!base.is_fresher_than(&later_sequence));
    // A newer membership always outranks an older one, however many
    // checkpoints the older one accumulated.
    let mut old_but_busy = base;
    old_but_busy.sequence = u64::MAX;
    assert!(later_generation.is_fresher_than(&old_but_busy));
    assert!(!old_but_busy.is_fresher_than(&later_generation));
    // Identical copies: neither is fresher, so a tie never flip-flops.
    assert!(!base.is_fresher_than(&base));
}

#[test]
fn a_foreign_record_never_wins_a_freshness_comparison() {
    let mine = full_record();
    let mut foreign = full_record();
    foreign.array_uuid = [0x11; 16];
    foreign.generation = u64::MAX;
    foreign.sequence = u64::MAX;
    // Records of different arrays are not comparable; neither direction is
    // fresher, so a foreign copy can never be adopted by a tournament.
    assert!(!foreign.is_fresher_than(&mine));
    assert!(!mine.is_fresher_than(&foreign));
}

#[test]
fn time_since_the_last_scrub_measures_the_elapsed_span() {
    let completed = Time64::new(1_800_000_000, 500_000_000).expect("canonical");
    let record =
        MaintenanceRecord::checkpoint(&identity(), 1, ArrayProgress::IDLE, Some(completed));
    let now = Time64::new(1_800_000_090, 750_000_000).expect("canonical");
    assert_eq!(record.since_last_scrub_ns(now), 90_250_000_000);
    // Exactly at the completion instant nothing has elapsed yet.
    assert_eq!(record.since_last_scrub_ns(completed), 0);
}

#[test]
fn an_array_never_verified_is_due_at_once() {
    let record = MaintenanceRecord::checkpoint(&identity(), 1, ArrayProgress::IDLE, None);
    assert_eq!(
        record.since_last_scrub_ns(Time64::from_secs(1_800_000_000)),
        u64::MAX
    );
}

#[test]
fn a_completion_stamp_from_the_future_is_not_credible() {
    // An unset or stepped wall clock, or a forged record, must not be able to
    // suppress verification: an implausible stamp reads as "unknown", which
    // makes a pass due immediately rather than never.
    let completed = Time64::new(2_000_000_000, 0).expect("canonical");
    let record =
        MaintenanceRecord::checkpoint(&identity(), 1, ArrayProgress::IDLE, Some(completed));
    assert_eq!(
        record.since_last_scrub_ns(Time64::from_secs(1_800_000_000)),
        u64::MAX
    );
    // The boot case: no wall time established yet, so "now" is the epoch.
    assert_eq!(record.since_last_scrub_ns(Time64::UNIX_EPOCH), u64::MAX);
    // One nanosecond ahead is still ahead: the boundary is exact, not fuzzy.
    let one_ns_earlier = Time64::new(1_999_999_999, 999_999_999).expect("canonical");
    assert_eq!(record.since_last_scrub_ns(one_ns_earlier), u64::MAX);
}

#[test]
fn a_completion_stamp_before_1970_or_after_2038_is_measured_exactly() {
    // 64-bit-native time: the stamp is a full `Time64`, so neither boundary
    // truncates.
    for completed in [
        Time64::new(-2_147_483_648, 0).expect("canonical"),
        Time64::new(2_147_483_648, 0).expect("canonical"),
        Time64::from_secs(4_294_967_296),
    ] {
        let record =
            MaintenanceRecord::checkpoint(&identity(), 1, ArrayProgress::IDLE, Some(completed));
        let decoded = MaintenanceRecord::decode(&record.encode()).expect("record decodes");
        assert_eq!(decoded.last_scrub_completed, Some(completed));
        let now = completed.saturating_add(Duration64::from_secs(3600));
        assert_eq!(decoded.since_last_scrub_ns(now), 3_600_000_000_000);
    }
}

#[test]
fn a_short_input_is_refused() {
    let bytes = full_record().encode();
    assert_eq!(
        MaintenanceRecord::decode(&bytes[..MaintenanceRecord::WIRE_LEN - 1]),
        Err(MaintenanceRecordError::TooSmall)
    );
    assert_eq!(
        MaintenanceRecord::decode(&[]),
        Err(MaintenanceRecordError::TooSmall)
    );
}

#[test]
fn a_blank_block_is_refused_as_not_a_record() {
    // The overwhelmingly common case: the block has never been written, or
    // holds array data. It must be recognised as "no record", not decoded.
    assert_eq!(
        MaintenanceRecord::decode(&[0u8; 512]),
        Err(MaintenanceRecordError::BadMagic)
    );
    assert_eq!(
        MaintenanceRecord::decode(&[0xFF; 512]),
        Err(MaintenanceRecordError::BadMagic)
    );
}

#[test]
fn a_superblock_is_not_mistaken_for_a_maintenance_record() {
    // The two records live in adjacent blocks, so a misdirected read must be
    // rejected by the magic rather than half-decoded.
    let superblock = crate::ArraySuperblock {
        array_uuid: [0x5A; 16],
        raid_level: RaidLevel::Mirror,
        member_count: 4,
        member_slot: 0,
        geometry: identity().geometry,
        generation: 7,
        updated_at: Time64::from_secs(1_800_000_000),
        chunk_blocks: 0,
    }
    .encode();
    // Read as a whole block, exactly as a misdirected read would return it, so
    // the magic decides rather than the length.
    let mut block = [0u8; 512];
    block[..superblock.len()].copy_from_slice(&superblock);
    assert_eq!(
        MaintenanceRecord::decode(&block),
        Err(MaintenanceRecordError::BadMagic)
    );
}

#[test]
fn an_unknown_version_is_refused() {
    let mut bytes = full_record().encode();
    bytes[8..10].copy_from_slice(&(MaintenanceRecord::FORMAT_VERSION + 1).to_le_bytes());
    reseal(&mut bytes);
    assert_eq!(
        MaintenanceRecord::decode(&bytes),
        Err(MaintenanceRecordError::UnsupportedVersion)
    );
}

#[test]
fn a_corrupt_record_is_refused() {
    // A torn or bit-rotted checkpoint must be discarded, not resumed from: a
    // wrong cursor would skip real work.
    let good = full_record().encode();
    for byte in 0..MaintenanceRecord::WIRE_LEN {
        let mut bytes = good;
        bytes[byte] ^= 0x01;
        let verdict = MaintenanceRecord::decode(&bytes);
        assert_ne!(
            verdict,
            Ok(full_record()),
            "a flipped bit at {byte} must not decode to the original record"
        );
        // Whatever the first check to catch it, the record is never accepted
        // as a different-but-valid one: only a refusal is acceptable here.
        assert!(verdict.is_err(), "flipped bit at {byte} was accepted");
    }
}

#[test]
fn an_undefined_flag_bit_is_refused() {
    // A record whose meaning is partly unknown is not partly trusted.
    for bit in 3..8u32 {
        let mut bytes = full_record().encode();
        bytes[10] |= 1 << bit;
        reseal(&mut bytes);
        assert_eq!(
            MaintenanceRecord::decode(&bytes),
            Err(MaintenanceRecordError::UnknownFlags),
            "flag bit {bit}"
        );
    }
}

#[test]
fn a_field_the_flags_declare_absent_must_be_zero() {
    // The encoding is canonical, so a record carrying data in a field it says
    // is absent is malformed — refuse it rather than pick one of the two
    // contradictory readings.
    let idle = MaintenanceRecord::checkpoint(&identity(), 3, ArrayProgress::IDLE, None);
    for offset in [
        super::OFF_SCRUB_CURSOR,
        super::OFF_RESYNC_CURSOR,
        super::OFF_LAST_SCRUB,
    ] {
        let mut bytes = idle.encode();
        bytes[offset] = 1;
        reseal(&mut bytes);
        assert_eq!(
            MaintenanceRecord::decode(&bytes),
            Err(MaintenanceRecordError::NonCanonicalField),
            "offset {offset}"
        );
    }
}

#[test]
fn a_non_canonical_timestamp_is_refused() {
    let mut bytes = full_record().encode();
    // A nanosecond field at or beyond one second is not a canonical `Time64`.
    bytes[super::OFF_LAST_SCRUB + 8..super::OFF_LAST_SCRUB + 12]
        .copy_from_slice(&1_000_000_000u32.to_le_bytes());
    reseal(&mut bytes);
    assert_eq!(
        MaintenanceRecord::decode(&bytes),
        Err(MaintenanceRecordError::BadTimestamp)
    );
}

/// Recompute the trailing CRC-32C over a hand-edited record, so a test can
/// prove a *field* check fires rather than merely tripping the checksum.
fn reseal(bytes: &mut [u8; MaintenanceRecord::WIRE_LEN]) {
    let crc = tairix_crc32c::checksum(&bytes[..super::OFF_CHECKSUM]);
    bytes[super::OFF_CHECKSUM..super::OFF_CHECKSUM + 4].copy_from_slice(&crc.to_le_bytes());
}
