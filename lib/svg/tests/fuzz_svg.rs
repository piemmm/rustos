//! Deterministic fuzz harness for the SVG decoder
//! (`AGENTS.md` §19.5 / §19.6 — the desktop's untrusted image-decoding parser).
//!
//! [`rustos_svg::decode`] parses on-disk `/System/Graphics` assets that, on a
//! real system, may have been written or corrupted by anything. Per §19.6 that
//! decode path is driven by a fuzz harness whose single invariant is:
//!
//! * `decode` never panics for any input — it returns `Ok` for a document in
//!   the supported subset and `Err` (fail closed) for everything else.
//!
//! RustOS pulls in no external fuzz runner (`AGENTS.md` §2.12): a per-run-seeded
//! LCG draws pseudo-random byte strings, mutates real SVG templates, and
//! assembles structured-but-hostile documents. A plain `cargo test` runs the
//! [`SMOKE_ITERATIONS`] sweep once from a fresh, logged seed; `cargo xtask
//! fuzz --soak` exports
//! `RUSTOS_FUZZ_BUDGET_SECS` to extend the PRNG loop to a wall-clock budget.

use rustos_svg::decode;

/// Fixed-iteration sweep run once by a plain `cargo test` (no budget set).
const SMOKE_ITERATIONS: u64 = 100_000;

/// Largest arbitrary byte string fed straight to the decoder.
const MAX_NOISE: usize = 4096;

/// Real templates the harness mutates: each exercises a different decode path.
const TEMPLATES: &[&[u8]] = &[
    br##"<svg viewBox="0 0 24 24"><polygon points="2,2 22,2 12,22" fill="#ff8800"/></svg>"##,
    br##"<svg viewBox="0 0 16 16"><path d="M2 2 h10 v10 h-10 Z" fill="#0a0"/></svg>"##,
    br##"<svg viewBox="0 0 20 20"><rect x="3" y="4" width="10" height="6" fill="#112233"/></svg>"##,
    br##"<svg width="32px" height="32px" data-hotspot-x="1" data-hotspot-y="2"><polygon points="0,0 32,0 32,32" fill="#fff" fill-opacity="0.5"/></svg>"##,
    br#"<?xml version="1.0"?><!-- c --><svg viewBox="0 0 8 8"><polygon points="0,0 8,0 8,8" fill="none"/></svg>"#,
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

/// Decode arbitrary bytes: must never panic, whatever it returns.
fn decode_never_panics(bytes: &[u8]) {
    let _ = decode(bytes);
}

#[test]
fn decode_never_panics_for_any_input() {
    let deadline = rustos_fuzzseed::budget_deadline(rustos_fuzzseed::FUZZ_BUDGET_ENV);

    // The LCG seed is drawn and logged by `rustos_fuzzseed::start`: fresh
    // per run, reproducible from the logged value via `RUSTOS_FUZZ_SEED`.
    let mut state: u64 = rustos_fuzzseed::start(
        "decode_never_panics_for_any_input",
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
        // 1. A real template with a handful of bytes flipped at random.
        let template = TEMPLATES[bounded(next(), TEMPLATES.len() - 1)];
        let mut mutated = template.to_vec();
        let flips = bounded(next(), 8);
        for _ in 0..flips {
            if mutated.is_empty() {
                break;
            }
            let pos = bounded(next(), mutated.len() - 1);
            mutated[pos] ^= low_byte(next() >> 17);
        }
        decode_never_panics(&mutated);

        // 2. A structured-but-hostile document: a valid frame with a random
        //    blob spliced into the middle, exercising the element scanner.
        let blob_len = bounded(next(), 64);
        let blob: Vec<u8> = (0..blob_len).map(|_| low_byte(next() >> 23)).collect();
        let mut spliced = Vec::new();
        spliced.extend_from_slice(br#"<svg viewBox="0 0 16 16">"#);
        spliced.extend_from_slice(&blob);
        spliced.extend_from_slice(br#"<polygon points="0,0 16,0 16,16"/></svg>"#);
        decode_never_panics(&spliced);

        // 3. Pure noise straight into the decoder.
        let nlen = bounded(next(), MAX_NOISE);
        let noise: Vec<u8> = (0..nlen).map(|_| low_byte(next() >> 29)).collect();
        decode_never_panics(&noise);

        iteration += 1;
        if !rustos_fuzzseed::within_budget(deadline) && iteration >= SMOKE_ITERATIONS {
            break;
        }
    }
}
