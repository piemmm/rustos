//! Deterministic fuzz harness for the first-party TrueType reader, the
//! OpenType variable-font instancer, and the scanline rasteriser
//! [`tairix_fontface`].
//!
//! The font parser is the untrusted-input parser the charter (§19.5, §19.6)
//! names explicitly: a face is bytes the outside world can supply, and the
//! sandboxed font service (`fontd`) is the one process that parses one. A
//! malformed face — static *or* variable — must fail closed with a typed
//! `Err`, never a panic, an out-of-bounds read, or a runaway loop, and a
//! *well-formed* face's outlines must rasterise into exactly the bitmap the
//! caller sized, whatever glyph, cell height, and axis settings are asked for.
//! This harness drives all of it:
//!
//! * **The parser, against adversarial bytes.** Pure-random buffers and
//!   bit-flipped copies of the committed static *and* variable faces are fed
//!   to [`Face::parse`] and [`Face::parse_instance`]. A byte-flipped variable
//!   face specifically exercises the `fvar`/`avar`/`gvar`/`HVAR` decoders. A
//!   parse that succeeds is then exercised — `mapped`, `glyph_for`, `advance`,
//!   `axes`, geometry derivation, `rasterise_glyph`, and
//!   `rasterise_proportional` — so a face that parses but is internally
//!   inconsistent cannot panic a downstream reader.
//! * **The instancer, over the whole axis space.** The pristine variable face
//!   is instanced at random axis settings and every draw path is exercised, so
//!   a hostile delta store cannot make a legitimate face panic or read out of
//!   bounds.
//! * **The rasteriser, over the whole glyph and size space.** Random glyphs
//!   are rasterised at random (bounded) cell heights, asserting the output is
//!   exactly the sized bitmap of 4-bit coverage.
//!
//! No external fuzz runner: a per-run-seeded LCG (seed drawn and logged by
//! `tairix_fuzzseed`) drives the loop. A plain `cargo test` runs the fixed
//! [`SMOKE_ITERATIONS`] sweep once; `cargo xtask fuzz` extends it to a
//! wall-clock budget.

use std::path::PathBuf;

use tairix_fontface::{AxisSetting, CellGeometry, Face, ATLAS_EM_PX};

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

/// Read a committed face by its `<family>/<file>` path.
fn asset(rel: &str) -> Vec<u8> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../font/assets")
        .join(rel);
    std::fs::read(&path).unwrap_or_else(|e| panic!("read {rel}: {e}"))
}

/// Rasterise `glyph` from a monospace `face` at a `height`-tall cell,
/// asserting the output is exactly the sized bitmap and 4-bit coverage.
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

/// Exercise the metrics and proportional draw path for `glyph` on any face,
/// asserting a returned bitmap is exactly its reported size and 4-bit.
fn exercise_proportional(face: &Face<'_>, glyph: u16, height: u32) {
    let _ = face.advance(glyph);
    let baseline = height * 3 / 4;
    if let Ok(raster) = face.rasterise_proportional(glyph, f64::from(height), baseline, height) {
        assert_eq!(
            raster.coverage.len(),
            (raster.width as usize) * (height as usize),
            "proportional coverage is not the sized bitmap"
        );
        assert!(
            raster.coverage.iter().all(|&sample| sample <= 15),
            "proportional coverage exceeds 4-bit range"
        );
    }
}

/// A `wght`/`wdth`/`opsz` axis setting from a fuzz word, spanning well past
/// the usual ranges so clamping and normalisation are exercised.
fn fuzz_setting(tag: [u8; 4], word: u64) -> AxisSetting {
    let value = f32::from(u16::try_from(word % 1200).unwrap_or(0));
    AxisSetting { tag, value }
}

#[test]
fn parsing_any_bytes_fails_closed_and_every_draw_stays_in_bounds() {
    let deadline = tairix_fuzzseed::budget_deadline(tairix_fuzzseed::FUZZ_BUDGET_ENV);
    let mut state: u64 = tairix_fuzzseed::start(
        "parsing_any_bytes_fails_closed_and_every_draw_stays_in_bounds",
        tairix_fuzzseed::FUZZ_SEED_ENV,
    );
    let mut next = || {
        state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        state
    };

    let mono = asset("mono/Inconsolata-EX.ttf");
    let variable = asset("inter/Inter-Variable.ttf");

    // Both pristine faces parse once; their glyph ranges drive the rasteriser
    // and instancer fuzzing.
    let good = Face::parse(&mono).expect("committed mono face parses");
    let mono_mapped: Vec<(u32, u16)> = good.mapped().to_vec();
    assert!(
        !mono_mapped.is_empty(),
        "committed face maps no code points"
    );
    let good_var = Face::parse(&variable).expect("committed variable face parses");
    assert!(good_var.is_variable(), "Inter must parse as variable");
    let var_mapped: Vec<(u32, u16)> = good_var.mapped().to_vec();

    let mut iteration: u64 = 0;
    let mut scratch = mono.clone();
    loop {
        // (1) Fuzz the rasteriser over the whole glyph and size space with the
        // known-good monospace face.
        let (_, glyph) = mono_mapped[index(next(), mono_mapped.len())];
        let height = 8 + bounded(next(), MAX_FUZZ_HEIGHT - 8);
        exercise_rasterise(&good, glyph, height);

        // (1b) Instance the known-good variable face at random axis settings
        // and exercise its metrics + proportional draw — a hostile *value* on
        // a valid face must not panic or read out of bounds.
        let settings = [
            fuzz_setting(*b"wght", next()),
            fuzz_setting(*b"wdth", next()),
            fuzz_setting(*b"opsz", next()),
        ];
        let take = 1 + index(next(), settings.len());
        if let Ok(instanced) = Face::parse_instance(&variable, &settings[..take]) {
            let _ = instanced.axes();
            let (_, vglyph) = var_mapped[index(next(), var_mapped.len().max(1))];
            exercise_proportional(&instanced, vglyph, 8 + bounded(next(), MAX_FUZZ_HEIGHT - 8));
        }

        // (2) Fuzz the parser. On odd iterations a handful of bit flips into a
        // copy of a committed face — the static face or, so the variation
        // decoders see adversarial bytes, the variable one — which is the hard,
        // structurally-near-valid case; on even iterations a fully random short
        // buffer.
        let base: &[u8] = if next() & 2 == 0 { &mono } else { &variable };
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
            scratch.extend_from_slice(base);
            let flips = 1 + index(next(), 8);
            for _ in 0..flips {
                let pos = index(next(), scratch.len());
                scratch[pos] ^= 1u8 << (next() % 8);
            }
            &scratch
        };

        if let Ok(face) = Face::parse(candidate) {
            // A face that parses must not panic any accessor. Probe the map and
            // draw a couple of its glyphs, both fixed-cell and proportional, at
            // a random height.
            let _ = face.uniform_advance();
            let _ = face.axes();
            for probe in 0..4u64 {
                let code = u32::try_from(next().wrapping_add(probe) & 0x1F_FFFF).unwrap_or(0);
                if let Some(glyph) = face.glyph_for(code) {
                    let height = 8 + bounded(next(), 24);
                    exercise_rasterise(&face, glyph, height);
                    exercise_proportional(&face, glyph, height);
                }
            }
        }

        iteration += 1;
        if !tairix_fuzzseed::within_budget(deadline) && iteration >= SMOKE_ITERATIONS {
            break;
        }
    }
}
