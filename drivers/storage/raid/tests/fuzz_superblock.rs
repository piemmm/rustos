//! Deterministic fuzz-style integration test for the RAID array-superblock
//! decoder.
//!
//! A member's superblock is read from a disk that may be failing, foreign, or
//! hostile (`AGENTS.md` §26.5, §19.5), so [`ArraySuperblock::decode`] takes an
//! arbitrary byte slice and must refuse a malformed one cleanly — never a
//! panic (`AGENTS.md` §2.9). This harness drives it two ways: random short
//! inputs, and an exhaustive bit-flip sweep of a well-formed record. Every
//! accepted record must survive an encode/decode round-trip unchanged, so a
//! forged input can never decode to something the encoder disowns.
//!
//! Seed selection, the start-of-test seed log, and the smoke/soak loop are the
//! shared `tairix_fuzzseed` seam (one definition, `AGENTS.md` §2.2): a plain
//! `cargo test` runs the fixed [`SMOKE_ITERATIONS`] sweep once from a fresh,
//! logged seed; `cargo xtask fuzz` sets the budget env var and the harness
//! keeps drawing from the same continuing stream until the deadline elapses.

use tairix_abi::driver::block::BlockGeometry;
use tairix_abi::time::Time64;
use tairix_drv_storage_raid::{ArraySuperblock, RaidLevel, WIRE_LEN};

/// Fixed-iteration sweep run once by a plain `cargo test` (no budget set).
const SMOKE_ITERATIONS: u64 = 200_000;

/// Decode `bytes` and, on acceptance, assert the record round-trips: encoding
/// the decoded value and decoding it again yields an identical record. The
/// point of the harness is that no input — accepted or rejected — panics.
fn exercise(bytes: &[u8]) {
    if let Ok(decoded) = ArraySuperblock::decode(bytes) {
        let reencoded = decoded.encode();
        let redecoded = ArraySuperblock::decode(&reencoded).expect("a re-encoded record decodes");
        assert_eq!(decoded, redecoded, "decode is not idempotent");
    }
}

#[test]
fn random_short_inputs_never_panic() {
    let mut rng = tairix_fuzzseed::Lcg::new(tairix_fuzzseed::start(
        "random_short_inputs_never_panic",
        tairix_fuzzseed::FUZZ_SEED_ENV,
    ));
    // A little larger than one record so both the too-short and the
    // trailing-bytes paths are exercised.
    let mut buf = [0u8; WIRE_LEN + 32];
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
fn bit_flipped_well_formed_records_never_panic() {
    // Start from a well-formed superblock and flip each bit in turn, walking
    // the boundary between accepted and rejected. Most flips break the magic,
    // a field bound, or the CRC; none may panic, and any that still decodes
    // must round-trip.
    let mut base = ArraySuperblock {
        array_uuid: [0x5A; 16],
        raid_level: RaidLevel::Mirror,
        member_count: 4,
        member_slot: 2,
        geometry: BlockGeometry {
            block_size: 4096,
            block_count: 0x1_0000_0001,
        },
        generation: 0x0102_0304_0506_0708,
        updated_at: Time64::new(1_800_000_000, 123_456_789).expect("canonical"),
        chunk_blocks: 0,
    }
    .encode();
    for byte in 0..base.len() {
        for bit in 0..8u32 {
            base[byte] ^= 1 << bit;
            exercise(&base);
            base[byte] ^= 1 << bit;
        }
    }
}
