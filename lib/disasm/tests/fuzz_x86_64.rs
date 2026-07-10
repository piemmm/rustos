//! Deterministic fuzz harness for the `lib/disasm` x86_64 decoder (a
//! decoder of untrusted executable-file bytes).
//!
//! Harness invariants, checked over random byte streams:
//!
//! * decoding any byte string never panics;
//! * a decoded instruction always makes forward progress (`1 ≤ length ≤`
//!   min(remaining, 15)), so a walk over any input terminates;
//! * the retained bytes are exactly the leading encoding bytes.
//!
//! RustOS pulls in no external fuzz runner: a per-run-seeded LCG produces
//! the streams. A plain `cargo test` runs the [`SMOKE_ITERATIONS`] sweep
//! once from a fresh, logged seed; `cargo xtask fuzz` exports
//! `RUSTOS_FUZZ_BUDGET_SECS` to extend the loop to a wall-clock budget.

use rustos_disasm::x86_64;

/// Fixed-iteration sweep run once by a plain `cargo test` (no budget set).
const SMOKE_ITERATIONS: u64 = 20_000;

/// Largest byte stream fed to the decoder per iteration.
const MAX_STREAM: usize = 256;

/// The x86_64 architectural instruction-length limit.
const MAX_INSN: usize = 15;

/// A valid prologue to mutate: push rbp ; mov rbp,rsp ; sub rsp,0x10 ;
/// lea rdi,[rip+0x0] ; call +0 ; leave ; ret.
const TEMPLATE: &[u8] = &[
    0x55, 0x48, 0x89, 0xe5, 0x48, 0x83, 0xec, 0x10, 0x48, 0x8d, 0x3d, 0x00, 0x00, 0x00, 0x00, 0xe8,
    0x00, 0x00, 0x00, 0x00, 0xc9, 0xc3,
];

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
        let insn = x86_64::decode(rest, u64::try_from(offset).unwrap_or(0))
            .expect("non-empty input always decodes");
        assert!(insn.length >= 1, "no forward progress at offset {offset}");
        assert!(
            insn.length <= rest.len(),
            "overran the input at offset {offset}"
        );
        assert!(
            insn.length <= MAX_INSN,
            "over the 15-byte limit at offset {offset}"
        );
        assert_eq!(insn.bytes, rest[..insn.length], "retained bytes mismatch");
        assert!(
            !insn.mnemonic.is_empty(),
            "empty mnemonic at offset {offset}"
        );
        offset += insn.length;
    }
    assert!(x86_64::decode(&[], 0).is_none());
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

        // 2. The valid template with a handful of bytes flipped, so real
        //    opcodes see hostile ModRM/SIB/immediate fields.
        let mut mutated = TEMPLATE.to_vec();
        for _ in 0..bounded(next(), 6) {
            let pos = bounded(next(), mutated.len() - 1);
            mutated[pos] ^= low_byte(next() >> 17);
        }
        walk(&mutated);

        // 3. Prefix storms: long runs of legacy/REX prefixes ending in a
        //    random opcode, straddling the 15-byte limit.
        let mut storm = Vec::new();
        let prefixes = [
            0x66u8, 0x67, 0xf0, 0xf2, 0xf3, 0x2e, 0x3e, 0x64, 0x65, 0x48, 0x41,
        ];
        for _ in 0..bounded(next(), 20) {
            storm.push(prefixes[bounded(next(), prefixes.len() - 1)]);
        }
        storm.push(low_byte(next() >> 19));
        storm.push(low_byte(next() >> 23));
        walk(&storm);

        iteration += 1;
        if !rustos_fuzzseed::within_budget(deadline) && iteration >= SMOKE_ITERATIONS {
            break;
        }
    }
}
