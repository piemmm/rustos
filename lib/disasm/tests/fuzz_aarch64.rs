//! Deterministic fuzz harness for the `lib/disasm` A64 decoder (a decoder
//! of untrusted executable-file bytes).
//!
//! Harness invariants, checked over random byte streams:
//!
//! * decoding any byte string never panics;
//! * a full word always decodes to four bytes (named or `.inst`), and a
//!   short tail to exactly the remaining bytes, so a walk terminates;
//! * the retained bytes are exactly the leading encoding bytes.
//!
//! TAIRiX pulls in no external fuzz runner: a per-run-seeded `Prng` produces
//! the streams. A plain `cargo test` runs the [`SMOKE_ITERATIONS`] sweep
//! once from a fresh, logged seed; `cargo xtask fuzz` exports
//! `TAIRIX_FUZZ_BUDGET_SECS` to extend the loop to a wall-clock budget.

use tairix_disasm::aarch64;
use tairix_fuzzseed::Prng;

/// Fixed-iteration sweep run once by a plain `cargo test` (no budget set).
const SMOKE_ITERATIONS: u64 = 20_000;

/// Largest byte stream fed to the decoder per iteration.
const MAX_STREAM: usize = 256;

/// Walks `stream` to the end, asserting the forward-progress invariants.
fn walk(stream: &[u8]) {
    let mut offset = 0usize;
    while offset < stream.len() {
        let rest = &stream[offset..];
        let insn = aarch64::decode(rest, u64::try_from(offset).unwrap_or(0))
            .expect("non-empty input always decodes");
        let expected = if rest.len() < 4 { rest.len() } else { 4 };
        assert_eq!(insn.length, expected, "wrong length at offset {offset}");
        assert_eq!(insn.bytes, rest[..insn.length], "retained bytes mismatch");
        assert!(
            !insn.mnemonic.is_empty(),
            "empty mnemonic at offset {offset}"
        );
        offset += insn.length;
    }
    assert!(aarch64::decode(&[], 0).is_none());
}

#[test]
fn decode_never_panics_and_always_advances() {
    let deadline = tairix_fuzzseed::budget_deadline(tairix_fuzzseed::FUZZ_BUDGET_ENV);
    let mut rng = Prng::new(tairix_fuzzseed::start(
        "decode_never_panics_and_always_advances",
        tairix_fuzzseed::FUZZ_SEED_ENV,
    ));

    let mut iteration: u64 = 0;
    loop {
        // 1. Pure noise, including unaligned tails.
        let mut noise = vec![0u8; rng.at_most(MAX_STREAM)];
        rng.fill(&mut noise);
        walk(&noise);

        // 2. Every top-level encoding group: a random word forced into each
        //    op0 slot, so all group decoders see hostile fields.
        let mut words = Vec::new();
        for group in 0u32..16 {
            let raw = rng.next_u64();
            let word =
                (u32::try_from(raw & 0xffff_ffff).unwrap_or(0) & !(0xf << 25)) | (group << 25);
            words.extend_from_slice(&word.to_le_bytes());
        }
        walk(&words);

        iteration += 1;
        if !tairix_fuzzseed::within_budget(deadline) && iteration >= SMOKE_ITERATIONS {
            break;
        }
    }
}
