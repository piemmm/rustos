//! Deterministic fuzz harness for the first-party TrueType reader and
//! scanline rasteriser [`tairix_fontface`].
//!
//! The font parser is the untrusted-input parser the charter (§19.5, §19.6)
//! names explicitly: a face is bytes the outside world can supply, and the
//! sandboxed font service (`fontd`) is the one process that parses one. A
//! malformed face must fail closed — a typed `Err`, never a panic, an
//! out-of-bounds read, or a runaway loop — and a *well-formed* face's outlines
//! must rasterise into exactly the bitmap the caller sized, whatever glyph and
//! cell height are asked for. This harness drives both:
//!
//! * **The parser, against adversarial bytes.** Pure-random buffers and
//!   bit-flipped copies of the committed face are fed to [`Face::parse`]. A
//!   parse that succeeds is then exercised — `mapped`, `glyph_for` on random
//!   code points, `uniform_advance`, geometry derivation, and
//!   `rasterise_glyph` — so a face that parses but is internally inconsistent
//!   cannot panic a downstream reader.
//! * **The rasteriser, over the whole glyph and size space.** The pristine
//!   committed face is parsed once and random glyphs are rasterised at random
//!   (bounded) cell heights, asserting the output is exactly
//!   `bitmap_width * height` bytes of 4-bit coverage.
//!
//! No external fuzz runner: a per-run-seeded LCG (seed drawn and logged by
//! `tairix_fuzzseed`) drives the loop. A plain `cargo test` runs the fixed
//! [`SMOKE_ITERATIONS`] sweep once; `cargo xtask fuzz` extends it to a
//! wall-clock budget.

use std::path::PathBuf;

use tairix_fontface::{CellGeometry, Face, ATLAS_EM_PX};

/// Fixed-iteration sweep run once by a plain `cargo test` (no budget set).
/// Rasterising an outline is heavier than a wire decode, so the smoke count
/// is smaller than the pure-decode harnesses' while still exercising every
/// path many times.
const SMOKE_ITERATIONS: u64 = 20_000;

/// Largest cell height the harness rasterises at — a bound that keeps a
/// fuzzed request cheap while still exercising the scaling arithmetic (the
/// service's own bound is far larger; this is a harness-speed cap, not a
/// contract).
const MAX_FUZZ_HEIGHT: u32 = 48;

/// `x` reduced into `0..len` as an index, without a narrowing `as` cast.
fn index(x: u64, len: usize) -> usize {
    let modulus = u64::try_from(len).unwrap_or(1).max(1);
    usize::try_from(x % modulus).unwrap_or(0)
}

/// `x` reduced into `0..=max`, without a narrowing `as` cast.
fn bounded(x: u64, max: u32) -> u32 {
    u32::try_from(x % (u64::from(max) + 1)).unwrap_or(0)
}

/// The committed primary face — a known-good corpus to mutate and to
/// rasterise from.
fn committed_face() -> Vec<u8> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../font/assets/Inconsolata-EX.ttf");
    std::fs::read(&path).expect("committed Inconsolata-EX face")
}

/// Rasterise `glyph` from `face` at a `height`-tall cell, asserting the
/// output is exactly the sized bitmap and every sample is 4-bit coverage.
fn exercise_rasterise(face: &Face<'_>, glyph: u16, height: u32) {
    let Ok(advance) = face.uniform_advance() else {
        return;
    };
    let Ok(native) = CellGeometry::derive(face, advance, ATLAS_EM_PX) else {
        return;
    };
    let nh = native.height.max(1);
    let scale = |value: u32| (value.saturating_mul(height) + nh / 2) / nh;
    let geometry = CellGeometry {
        width: scale(native.width).max(1),
        height,
        baseline: scale(native.baseline),
    };
    let bitmap_width = geometry.width.saturating_mul(2);
    let px_per_em = f64::from(ATLAS_EM_PX) * f64::from(height) / f64::from(nh);
    if let Ok(coverage) = face.rasterise_glyph(glyph, &geometry, px_per_em, bitmap_width) {
        assert_eq!(
            coverage.len(),
            (bitmap_width as usize) * (height as usize),
            "rasterised coverage is not the sized bitmap"
        );
        assert!(
            coverage.iter().all(|&sample| sample <= 15),
            "coverage sample exceeds 4-bit range"
        );
    }
}

#[test]
fn parsing_any_bytes_fails_closed_and_rasterising_stays_in_bounds() {
    let deadline = tairix_fuzzseed::budget_deadline(tairix_fuzzseed::FUZZ_BUDGET_ENV);
    let mut state: u64 = tairix_fuzzseed::start(
        "parsing_any_bytes_fails_closed_and_rasterising_stays_in_bounds",
        tairix_fuzzseed::FUZZ_SEED_ENV,
    );
    let mut next = || {
        state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        state
    };

    let base = committed_face();

    // The pristine face parses once; its glyph range and metrics drive the
    // rasteriser fuzzing.
    let good = Face::parse(&base).expect("committed face parses");
    let mapped: Vec<(u32, u16)> = good.mapped().to_vec();
    assert!(!mapped.is_empty(), "committed face maps no code points");

    let mut iteration: u64 = 0;
    let mut scratch = base.clone();
    loop {
        // (1) Fuzz the rasteriser over the whole glyph and size space with the
        // known-good face.
        let (_, glyph) = mapped[index(next(), mapped.len())];
        let height = 8 + bounded(next(), MAX_FUZZ_HEIGHT - 8);
        exercise_rasterise(&good, glyph, height);

        // (2) Fuzz the parser: on odd iterations a handful of bit flips into a
        // copy of the committed face (structurally near-valid, the hard case);
        // on even iterations a fully random short buffer.
        let candidate: &[u8] = if next() & 1 == 0 {
            let len = index(next(), 4096);
            scratch.clear();
            scratch.resize(len, 0);
            for byte in &mut scratch {
                *byte = next().to_le_bytes()[0];
            }
            &scratch
        } else {
            scratch.clear();
            scratch.extend_from_slice(&base);
            let flips = 1 + index(next(), 8);
            for _ in 0..flips {
                let pos = index(next(), scratch.len());
                scratch[pos] ^= 1u8 << (next() % 8);
            }
            &scratch
        };

        if let Ok(face) = Face::parse(candidate) {
            // A face that parses must not panic any accessor. Probe the map
            // and rasterise a couple of its glyphs at a random height.
            let _ = face.uniform_advance();
            for probe in 0..4u64 {
                let code = u32::try_from(next().wrapping_add(probe) & 0x1F_FFFF).unwrap_or(0);
                if let Some(glyph) = face.glyph_for(code) {
                    let height = 8 + bounded(next(), 24);
                    exercise_rasterise(&face, glyph, height);
                }
            }
        }

        iteration += 1;
        if !tairix_fuzzseed::within_budget(deadline) && iteration >= SMOKE_ITERATIONS {
            break;
        }
    }
}
