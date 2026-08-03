//! Deterministic fuzz-style integration test for the RAID array
//! maintenance-record decoder.
//!
//! The record is read from the reserved metadata region of a disk that may be
//! failing, foreign, or hostile (`AGENTS.md` §26.5, §19.5), so
//! [`MaintenanceRecord::decode`] takes an arbitrary byte slice and must refuse
//! a malformed one cleanly — never a panic (`AGENTS.md` §2.9). This harness
//! drives it two ways: random short inputs, and an exhaustive bit-flip sweep
//! of a well-formed record. Every accepted record must survive an
//! encode/decode round-trip unchanged, which is what proves the encoding is
//! canonical: a forged input can never decode to something the encoder
//! disowns.
//!
//! Seed selection, the start-of-test seed log, and the smoke/soak loop are the
//! shared `tairix_fuzzseed` seam (one definition, `AGENTS.md` §2.2): a plain
//! `cargo test` runs the fixed [`SMOKE_ITERATIONS`] sweep once from a fresh,
//! logged seed; `cargo xtask fuzz` sets the budget env var and the harness
//! keeps drawing from the same continuing stream until the deadline elapses.

use tairix_abi::driver::block::BlockGeometry;
use tairix_abi::raid::RaidLevel;
use tairix_abi::time::Time64;
use tairix_raidmeta::{ArrayIdentity, ArrayProgress, MaintenanceRecord};

/// Fixed-iteration sweep run once by a plain `cargo test` (no budget set).
const SMOKE_ITERATIONS: u64 = 200_000;

/// Decode `bytes` and, on acceptance, assert the record round-trips: encoding
/// the decoded value and decoding it again yields an identical record. The
/// point of the harness is that no input — accepted or rejected — panics.
fn exercise(bytes: &[u8]) {
    if let Ok(decoded) = MaintenanceRecord::decode(bytes) {
        let reencoded = decoded.encode();
        assert_eq!(
            &reencoded[..],
            &bytes[..MaintenanceRecord::WIRE_LEN],
            "an accepted record must re-encode to the bytes it came from"
        );
        let redecoded = MaintenanceRecord::decode(&reencoded).expect("a re-encoded record decodes");
        assert_eq!(decoded, redecoded, "decode is not idempotent");
        // Whatever the bytes claimed, reading the elapsed span is total: it
        // never panics and never wraps, whichever way the two instants order.
        for now in [Time64::UNIX_EPOCH, Time64::from_secs(i64::MAX)] {
            let _ = decoded.since_last_scrub_ns(now);
        }
    }
}

/// A well-formed record to flip bits in: every optional field present, so the
/// sweep walks the flags byte in both directions.
fn well_formed() -> [u8; MaintenanceRecord::WIRE_LEN] {
    let identity = ArrayIdentity {
        array_uuid: [0x5A; 16],
        raid_level: RaidLevel::Mirror,
        member_count: 4,
        geometry: BlockGeometry {
            block_size: 4096,
            block_count: 0x1_0000_0001,
        },
        generation: 0x0102_0304_0506_0708,
        chunk_blocks: 0,
    };
    MaintenanceRecord::checkpoint(
        &identity,
        0x0A0B_0C0D_0E0F_1011,
        ArrayProgress {
            scrub_cursor: Some(0x2222_3333),
            resync_cursor: Some(0x4444_5555),
        },
        Some(Time64::new(1_800_000_000, 123_456_789).expect("canonical")),
    )
    .encode()
}

#[test]
fn random_short_inputs_never_panic() {
    let mut rng = tairix_fuzzseed::Lcg::new(tairix_fuzzseed::start(
        "maintenance_random_short_inputs_never_panic",
        tairix_fuzzseed::FUZZ_SEED_ENV,
    ));
    // A little larger than one record so both the too-short and the
    // trailing-bytes paths are exercised.
    let mut buf = [0u8; MaintenanceRecord::WIRE_LEN + 32];
    let deadline = tairix_fuzzseed::budget_deadline(tairix_fuzzseed::FUZZ_BUDGET_ENV);
    loop {
        for _ in 0..SMOKE_ITERATIONS {
            let size = ((rng.next_u64() & 0xFFFF) as usize) % (buf.len() + 1);
            rng.fill(&mut buf[..size]);
            exercise(&buf[..size]);
        }
        if !tairix_fuzzseed::within_budget(deadline) {
            break;
        }
    }
}

#[test]
fn well_sealed_but_corrupted_records_never_panic() {
    // Random bytes carry the magic essentially never, and a random trailing
    // CRC matches essentially never, so a purely random sweep only ever
    // exercises the first two checks. This models the adversary that matters
    // instead: a writer that *can* produce a properly sealed record — a
    // hostile disk, or a foreign implementation — by corrupting one field of a
    // well-formed record and resealing it. Every interior check (flags,
    // canonical absent fields, the timestamp, the identity binding) is then
    // genuinely reached, and any record still accepted must round-trip.
    let mut rng = tairix_fuzzseed::Lcg::new(tairix_fuzzseed::start(
        "maintenance_well_sealed_but_corrupted_records_never_panic",
        tairix_fuzzseed::FUZZ_SEED_ENV,
    ));
    let template = well_formed();
    let body = MaintenanceRecord::WIRE_LEN - 4;
    let deadline = tairix_fuzzseed::budget_deadline(tairix_fuzzseed::FUZZ_BUDGET_ENV);
    loop {
        for _ in 0..SMOKE_ITERATIONS {
            let mut buf = template;
            let start = ((rng.next_u64() & 0xFFFF) as usize) % body;
            let len = 1 + ((rng.next_u64() & 0xFFFF) as usize) % (body - start);
            rng.fill(&mut buf[start..start + len]);
            let crc = tairix_crc32c::checksum(&buf[..body]);
            buf[body..].copy_from_slice(&crc.to_le_bytes());
            exercise(&buf);
        }
        if !tairix_fuzzseed::within_budget(deadline) {
            break;
        }
    }
}

#[test]
fn bit_flipped_well_formed_records_never_panic() {
    // Start from a well-formed record and flip each bit in turn, walking the
    // boundary between accepted and rejected. Most flips break the magic, a
    // flag bit, or the CRC; none may panic, and any that still decodes must
    // round-trip.
    let mut base = well_formed();
    for byte in 0..base.len() {
        for bit in 0..8u32 {
            base[byte] ^= 1 << bit;
            exercise(&base);
            base[byte] ^= 1 << bit;
        }
    }
}
