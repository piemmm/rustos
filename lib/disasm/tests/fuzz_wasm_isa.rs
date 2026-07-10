//! Deterministic fuzz harness for the `lib/disasm` wasm body decoder (a
//! decoder of untrusted executable-file bytes).
//!
//! Named `fuzz_wasm_isa` to stay distinct from `lib/binfmt`'s `fuzz_wasm`
//! (the module-structure view); this harness targets the instruction
//! stream inside a code-section body.
//!
//! Harness invariants, checked over random byte streams:
//!
//! * decoding any byte string at any depth never panics;
//! * a decoded instruction always makes forward progress (`1 ≤ length ≤`
//!   remaining bytes), so a walk over any input terminates;
//! * the retained bytes prefix the encoding and the returned depth only
//!   moves by at most one level per instruction.
//!
//! RustOS pulls in no external fuzz runner: a per-run-seeded LCG produces
//! the streams. A plain `cargo test` runs the [`SMOKE_ITERATIONS`] sweep
//! once from a fresh, logged seed; `cargo xtask fuzz` exports
//! `RUSTOS_FUZZ_BUDGET_SECS` to extend the loop to a wall-clock budget.

use rustos_disasm::wasm;

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
fn walk(stream: &[u8], start_depth: u32) {
    let mut offset = 0usize;
    let mut depth = start_depth;
    while offset < stream.len() {
        let rest = &stream[offset..];
        let (insn, next_depth) = wasm::decode(rest, u64::try_from(offset).unwrap_or(0), depth)
            .expect("non-empty input always decodes");
        assert!(insn.length >= 1, "no forward progress at offset {offset}");
        assert!(
            insn.length <= rest.len(),
            "overran the input at offset {offset}"
        );
        let kept = insn.bytes.len();
        assert_eq!(insn.bytes, rest[..kept], "retained bytes mismatch");
        assert!(
            next_depth.abs_diff(depth) <= 1,
            "depth jumped from {depth} to {next_depth} at offset {offset}"
        );
        depth = next_depth;
        offset += insn.length;
    }
    assert!(wasm::decode(&[], 0, start_depth).is_none());
}

#[test]
fn decode_never_panics_and_always_advances() {
    let deadline = rustos_fuzzseed::budget_deadline(rustos_fuzzseed::FUZZ_BUDGET_ENV);
    let mut state: u64 = rustos_fuzzseed::start(
        "decode_never_panics_and_always_advances",
        rustos_fuzzseed::FUZZ_SEED_ENV,
    );
    let mut next = || {
        state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        state
    };

    let mut iteration: u64 = 0;
    loop {
        // 1. Pure noise at a random starting depth (including extremes).
        let noise: Vec<u8> = (0..bounded(next(), MAX_STREAM))
            .map(|_| low_byte(next() >> 29))
            .collect();
        let depth = u32::try_from(next() & 0xffff).unwrap_or(0);
        walk(&noise, depth);

        // 2. Structure-heavy streams: blocks, branches, and LEB-carrying
        //    opcodes with hostile immediates.
        let mut body = Vec::new();
        for _ in 0..bounded(next(), 64) {
            body.push([0x02, 0x03, 0x04, 0x05, 0x0b, 0x0c, 0x0e, 0x41, 0xfc][bounded(next(), 8)]);
            body.push(low_byte(next() >> 13));
            if next() & 1 == 0 {
                body.push(low_byte(next() >> 41) | 0x80);
                body.push(low_byte(next() >> 47));
            }
        }
        walk(&body, 1);

        iteration += 1;
        if !rustos_fuzzseed::within_budget(deadline) && iteration >= SMOKE_ITERATIONS {
            break;
        }
    }
}
