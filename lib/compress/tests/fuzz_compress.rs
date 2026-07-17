//! Deterministic fuzz harness for the first-party LZ decoder
//! (`docs/src/filesystem/arxfs-spec.md` §10 — the
//! required "compression decode" fuzz target).
//!
//! [`rustos_compress::decompress`] parses a byte stream that, on a real
//! system, may have been written or corrupted by anything: it is the
//! untrusted-input parser `ARXFS` runs over every compressed data record. Per
//! that decode path is driven by a fuzz harness whose invariants are:
//!
//! * `decompress` never panics for any input — it returns `Ok` for a valid
//!   frame and `Err` (fail closed) for everything else; and
//! * the codec round-trips: `decompress(compress(x)) == x` for every drawn
//!   `x` (a malformed frame can only ever come from corruption, never from the
//!   encoder).
//!
//! RustOS pulls in no external fuzz runner: a per-run-seeded
//! LCG draws pseudo-random inputs and corrupts real frames. A plain `cargo
//! test` runs the [`SMOKE_ITERATIONS`] sweep once from a fresh, logged seed;
//! `cargo xtask fuzz --soak` exports
//! `RUSTOS_FUZZ_BUDGET_SECS` to extend the PRNG loop to a wall-clock budget.

use rustos_compress::{compress, decompress, max_compressed_len};

/// Fixed-iteration sweep run once by a plain `cargo test` (no budget set).
const SMOKE_ITERATIONS: u64 = 100_000;

/// Largest plaintext drawn by the round-trip sweep.
const MAX_INPUT: usize = 8192;

/// Largest arbitrary byte string fed straight to the decoder.
const MAX_FRAME: usize = 4096;

/// Low byte of `x`, without a narrowing `as` cast.
fn low_byte(x: u64) -> u8 {
    x.to_le_bytes()[0]
}

/// `x` reduced into `0..=max` as a `usize`, without a narrowing `as` cast.
fn bounded(x: u64, max: usize) -> usize {
    let span = u64::try_from(max).unwrap_or(u64::MAX).saturating_add(1);
    usize::try_from(x % span).unwrap_or(0)
}

/// Decompress arbitrary bytes into a bounded destination: must never panic.
fn decode_never_panics(frame: &[u8]) {
    let mut out = vec![0u8; MAX_INPUT];
    let _ = decompress(frame, &mut out);
}

/// Compress then decompress `input`: must be the identity.
fn round_trips(input: &[u8]) {
    let mut packed = vec![0u8; max_compressed_len(input.len())];
    let n = compress(input, &mut packed).expect("compress fits its bound buffer");
    let mut out = vec![0u8; input.len()];
    let m = decompress(&packed[..n], &mut out).expect("decompress a self-produced frame");
    assert_eq!(m, input.len(), "round-trip length matches");
    assert_eq!(&out[..], input, "round-trip is the identity");
}

#[test]
fn decompress_never_panics_and_codec_round_trips() {
    let deadline = rustos_fuzzseed::budget_deadline(rustos_fuzzseed::FUZZ_BUDGET_ENV);

    // The LCG seed is drawn and logged by `rustos_fuzzseed::start`: fresh
    // per run, reproducible from the logged value via `RUSTOS_FUZZ_SEED`.
    let mut state: u64 = rustos_fuzzseed::start(
        "decompress_never_panics_and_codec_round_trips",
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
        // 1. A structured input that mixes runs (compressible) with noise
        //    (incompressible), then round-trips through the codec.
        let len = bounded(next(), MAX_INPUT);
        let mut input = Vec::with_capacity(len);
        while input.len() < len {
            if next() & 1 == 0 {
                let run = bounded(next(), 64).min(len - input.len());
                let byte = low_byte(next());
                input.extend(std::iter::repeat(byte).take(run));
            } else {
                input.push(low_byte(next() >> 11));
            }
        }
        round_trips(&input);

        // 2. Corrupt a real frame at random offsets and feed it to the decoder.
        let mut packed = vec![0u8; max_compressed_len(input.len())];
        if let Ok(n) = compress(&input, &mut packed) {
            let mut frame = packed[..n].to_vec();
            let flips = bounded(next(), 8);
            for _ in 0..flips {
                if frame.is_empty() {
                    break;
                }
                let pos = bounded(next(), frame.len() - 1);
                frame[pos] ^= low_byte(next() >> 19);
            }
            decode_never_panics(&frame);
        }

        // 3. Pure noise straight into the decoder.
        let nlen = bounded(next(), MAX_FRAME);
        let noise: Vec<u8> = (0..nlen).map(|_| low_byte(next() >> 23)).collect();
        decode_never_panics(&noise);

        iteration += 1;
        if !rustos_fuzzseed::within_budget(deadline) && iteration >= SMOKE_ITERATIONS {
            break;
        }
    }
}
