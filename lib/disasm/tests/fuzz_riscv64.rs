//! Deterministic fuzz harness for the `lib/disasm` RV64GC decoder (a
//! decoder of untrusted executable-file bytes).
//!
//! Harness invariants, checked over random byte streams:
//!
//! * decoding any byte string never panics;
//! * a decoded instruction always makes forward progress (`1 ≤ length ≤`
//!   remaining bytes), so a walk over any input terminates;
//! * the retained bytes are exactly the leading encoding bytes.
//!
//! RustOS pulls in no external fuzz runner: a per-run-seeded LCG produces
//! the streams. A plain `cargo test` runs the [`SMOKE_ITERATIONS`] sweep
//! once from a fresh, logged seed; `cargo xtask fuzz` exports
//! `RUSTOS_FUZZ_BUDGET_SECS` to extend the loop to a wall-clock budget.

use rustos_disasm::riscv64;

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
        let insn = riscv64::decode(rest, u64::try_from(offset).unwrap_or(0))
            .expect("non-empty input always decodes");
        assert!(insn.length >= 1, "no forward progress at offset {offset}");
        assert!(
            insn.length <= rest.len(),
            "overran the input at offset {offset}"
        );
        assert_eq!(
            insn.bytes,
            rest[..insn.bytes.len()],
            "retained bytes mismatch"
        );
        assert!(
            !insn.mnemonic.is_empty(),
            "empty mnemonic at offset {offset}"
        );
        offset += insn.length;
    }
    assert!(riscv64::decode(&[], 0).is_none());
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
        // 1. Pure noise.
        let noise: Vec<u8> = (0..bounded(next(), MAX_STREAM))
            .map(|_| low_byte(next() >> 29))
            .collect();
        walk(&noise);

        // 2. Aligned random parcels (every 16-bit slot random but aligned),
        //    the shape a real, corrupted text section takes.
        let mut parcels = Vec::new();
        for _ in 0..bounded(next(), MAX_STREAM / 2) {
            parcels.push(low_byte(next() >> 11));
            parcels.push(low_byte(next() >> 37));
        }
        walk(&parcels);

        // 3. Truncations of a valid tail: a full-width opcode byte followed
        //    by too few bytes.
        let mut short = vec![0x13u8, 0x05];
        short.truncate(1 + bounded(next(), 1));
        walk(&short);

        iteration += 1;
        if !rustos_fuzzseed::within_budget(deadline) && iteration >= SMOKE_ITERATIONS {
            break;
        }
    }
}
