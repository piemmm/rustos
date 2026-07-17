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
//! TAIRiX pulls in no external fuzz runner: a per-run-seeded LCG produces
//! the streams. A plain `cargo test` runs the [`SMOKE_ITERATIONS`] sweep
//! once from a fresh, logged seed; `cargo xtask fuzz` exports
//! `TAIRIX_FUZZ_BUDGET_SECS` to extend the loop to a wall-clock budget.

use tairix_disasm::aarch64;

/// Fixed-iteration sweep run once by a plain `cargo test` (no budget set).
const SMOKE_ITERATIONS: u64 = 20_000;

/// Largest byte stream fed to the decoder per iteration.
const MAX_STREAM: usize = 256;

/// Low byte of `x`, without a narrowing `as` cast.
fn low_byte(x: u64) -> u8 {
    x.to_le_bytes()[0]
}

/// `x` reduced into `0..=max` as a `usize`, without a narrowing `as` cast.
fn bounded(x: u64, max: usize) -> usize {
    let span = u64::try_from(max).unwrap_or(u64::MAX).saturating_add(1);
    usize::try_from(x % span).unwrap_or(0)
}

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
    let mut state: u64 = tairix_fuzzseed::start(
        "decode_never_panics_and_always_advances",
        tairix_fuzzseed::FUZZ_SEED_ENV,
    );
    let mut next = || {
        state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        state
    };

    let mut iteration: u64 = 0;
    loop {
        // 1. Pure noise, including unaligned tails.
        let noise: Vec<u8> = (0..bounded(next(), MAX_STREAM))
            .map(|_| low_byte(next() >> 29))
            .collect();
        walk(&noise);

        // 2. Every top-level encoding group: a random word forced into each
        //    op0 slot, so all group decoders see hostile fields.
        let mut words = Vec::new();
        for group in 0u32..16 {
            let raw = next();
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
