//! Deterministic fuzz harness for the PNG decoder.
//!
//! Invariants, for any bytes an untrusted bundle icon may carry:
//!
//! 1. [`decode`] never panics for any input, and never reports a decoded
//!    image whose width, height, or pixel count exceeds the [`DecodeLimits`]
//!    it was given.
//! 2. Structure-aware mutations of a valid, builder-made PNG (bit flips,
//!    length/CRC tweaks, chunk reordering) never panic — the mutated bytes
//!    either decode within the limits or are refused with a typed error.
//! 3. The generator itself is not degenerate: every pristine fixture it
//!    produces actually decodes (a corpus that never round-trips would
//!    leave invariant 2 exercising only the trivial "refused immediately"
//!    path).
//!
//! The generator and its chunk/zlib framing helpers are deliberately
//! self-contained: this harness only calls `tairix_image`'s public API
//! (exactly what a real consumer — the desktop icon pipeline's sandbox —
//! would do), never the crate's own internal chunk reader or CRC table, so
//! a bug in either is still caught here.

use tairix_fuzzseed::Lcg;
use tairix_image::{decode, sniff, DecodeLimits};

/// Fixed-iteration sweep run when no budget is set.
const SMOKE_ITERATIONS: u64 = 2_000;

/// The 8-byte PNG signature (W3C PNG §"PNG file signature"), restated here
/// because this harness only ever calls the crate's public API.
const SIGNATURE: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];

/// PNG's own chunk-framing CRC-32 (ISO-HDLC), restated here for the same
/// reason — this harness builds its own chunks rather than reaching into
/// the crate under test.
fn crc32(data: &[u8]) -> u32 {
    const POLY: u32 = 0xEDB8_8320;
    let mut crc = 0xFFFF_FFFFu32;
    for &byte in data {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ POLY
            } else {
                crc >> 1
            };
        }
    }
    !crc
}

fn chunk(chunk_type: [u8; 4], payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let len = u32::try_from(payload.len()).unwrap_or(u32::MAX);
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(&chunk_type);
    out.extend_from_slice(payload);
    let mut crc_input = chunk_type.to_vec();
    crc_input.extend_from_slice(payload);
    out.extend_from_slice(&crc32(&crc_input).to_be_bytes());
    out
}

/// Wrap `data` in a well-formed zlib stream built from STORED deflate
/// blocks plus a real Adler-32 trailer (`tairix_compress::zlib::adler32`),
/// so no compressor is needed to produce a stream the crate accepts.
fn zlib_wrap(data: &[u8]) -> Vec<u8> {
    let mut out = vec![0x78u8, 0x9C];
    if data.is_empty() {
        out.push(0x01);
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&(!0u16).to_le_bytes());
    } else {
        let mut remaining = data;
        while !remaining.is_empty() {
            let take = remaining.len().min(65_535);
            let (block, rest) = remaining.split_at(take);
            out.push(u8::from(rest.is_empty()));
            let len = u16::try_from(take).unwrap_or(u16::MAX);
            out.extend_from_slice(&len.to_le_bytes());
            out.extend_from_slice(&(!len).to_le_bytes());
            out.extend_from_slice(block);
            remaining = rest;
        }
    }
    out.extend_from_slice(&tairix_compress::zlib::adler32(data).to_be_bytes());
    out
}

/// Adam7's seven passes as `(row_start, col_start, row_step, col_step)`.
const ADAM7: [(u32, u32, u32, u32); 7] = [
    (0, 0, 8, 8),
    (0, 4, 8, 8),
    (4, 0, 8, 4),
    (0, 2, 4, 4),
    (2, 0, 4, 2),
    (0, 1, 2, 2),
    (1, 0, 2, 1),
];

fn pass_extent(total: u32, start: u32, step: u32) -> u32 {
    if start >= total {
        0
    } else {
        (total - start).div_ceil(step)
    }
}

fn channels_for(colour_type: u8) -> u32 {
    match colour_type {
        2 => 3,
        4 => 2,
        6 => 4,
        _ => 1, // 0 (grey) and 3 (indexed) both carry a single channel
    }
}

fn row_sample_bytes(width: u32, colour_type: u8, bit_depth: u8) -> usize {
    let bits = u64::from(width) * u64::from(channels_for(colour_type)) * u64::from(bit_depth);
    usize::try_from(bits.div_ceil(8)).unwrap_or(usize::MAX)
}

/// Fill `raw` with random (but structurally sized) filtered scanlines for
/// every non-empty pass. Sample bytes are unconstrained — for indexed
/// colour the generator always emits a full 256-entry palette, so any byte
/// value is a valid index regardless of bit depth.
fn build_raw_scanlines(
    rng: &mut Lcg,
    width: u32,
    height: u32,
    colour_type: u8,
    bit_depth: u8,
    interlaced: bool,
) -> Vec<u8> {
    let passes: Vec<(u32, u32)> = if interlaced {
        ADAM7
            .iter()
            .map(|&(row_start, col_start, row_step, col_step)| {
                (
                    pass_extent(width, col_start, col_step),
                    pass_extent(height, row_start, row_step),
                )
            })
            .collect()
    } else {
        vec![(width, height)]
    };

    let mut raw = Vec::new();
    for (pass_width, pass_height) in passes {
        if pass_width == 0 || pass_height == 0 {
            continue;
        }
        let row_bytes = row_sample_bytes(pass_width, colour_type, bit_depth);
        for _ in 0..pass_height {
            raw.push(u8::try_from(rng.below(5)).unwrap_or(0)); // filter type 0..=4
            let mut row = vec![0u8; row_bytes];
            rng.fill(&mut row);
            raw.extend_from_slice(&row);
        }
    }
    raw
}

/// Build one structurally valid, randomised PNG.
fn build_valid_png(rng: &mut Lcg) -> Vec<u8> {
    let colour_type = *[0u8, 2, 3, 4, 6].get(rng.below(5)).unwrap_or(&0);
    let depths: &[u8] = match colour_type {
        0 => &[1, 2, 4, 8, 16],
        3 => &[1, 2, 4, 8],
        _ => &[8, 16],
    };
    let bit_depth = *depths.get(rng.below(depths.len())).unwrap_or(&8);
    let interlaced = rng.below(2) == 0;
    let width = u32::try_from(rng.below(6) + 1).unwrap_or(1);
    let height = u32::try_from(rng.below(6) + 1).unwrap_or(1);

    // A full 256-entry palette so any raw index byte, at any legal indexed
    // bit depth, is always in range.
    let palette = if colour_type == 3 {
        let mut p = vec![0u8; 3 * 256];
        rng.fill(&mut p);
        Some(p)
    } else {
        None
    };

    let trns = if rng.below(3) == 0 {
        None
    } else {
        match colour_type {
            0 => Some({
                let mut t = [0u8; 2];
                rng.fill(&mut t);
                t.to_vec()
            }),
            2 => Some({
                let mut t = [0u8; 6];
                rng.fill(&mut t);
                t.to_vec()
            }),
            3 => {
                let len = rng.below(257);
                let mut t = vec![0u8; len];
                rng.fill(&mut t);
                Some(t)
            }
            _ => None,
        }
    };

    let raw = build_raw_scanlines(rng, width, height, colour_type, bit_depth, interlaced);

    let mut ihdr = Vec::new();
    ihdr.extend_from_slice(&width.to_be_bytes());
    ihdr.extend_from_slice(&height.to_be_bytes());
    ihdr.extend_from_slice(&[bit_depth, colour_type, 0, 0, u8::from(interlaced)]);

    let mut out = SIGNATURE.to_vec();
    out.extend(chunk(*b"IHDR", &ihdr));
    if let Some(p) = &palette {
        out.extend(chunk(*b"PLTE", p));
    }
    if let Some(t) = &trns {
        out.extend(chunk(*b"tRNS", t));
    }
    out.extend(chunk(*b"IDAT", &zlib_wrap(&raw)));
    out.extend(chunk(*b"IEND", &[]));
    out
}

/// The `(start, end)` byte range of every chunk (including its length,
/// type, and CRC) found by a best-effort forward walk, stopping at the
/// first chunk whose declared length runs past the end of `bytes`.
fn chunk_bounds(bytes: &[u8]) -> Vec<(usize, usize)> {
    let mut bounds = Vec::new();
    let mut pos = 8usize;
    while pos + 8 <= bytes.len() {
        let len = u32::from_be_bytes([bytes[pos], bytes[pos + 1], bytes[pos + 2], bytes[pos + 3]]);
        let Some(end) = usize::try_from(len)
            .ok()
            .and_then(|len| (pos + 8 + len).checked_add(4))
        else {
            break;
        };
        if end > bytes.len() {
            break;
        }
        bounds.push((pos, end));
        pos = end;
    }
    bounds
}

/// Structurally mutate `pristine`: maybe reorder two chunks, then flip a
/// handful of random bits (which also lands on length and CRC fields often
/// enough, since they are ordinary bytes like any other).
fn mutate(rng: &mut Lcg, pristine: &[u8]) -> Vec<u8> {
    let mut bytes = pristine.to_vec();

    let bounds = chunk_bounds(&bytes);
    if bounds.len() >= 2 && rng.below(2) == 0 {
        let i = rng.below(bounds.len());
        let j = rng.below(bounds.len());
        if i != j {
            let mut chunks: Vec<&[u8]> = bounds.iter().map(|&(s, e)| &bytes[s..e]).collect();
            chunks.swap(i, j);
            let mut rebuilt = bytes[..8].to_vec();
            for c in chunks {
                rebuilt.extend_from_slice(c);
            }
            if let Some(&(_, last_end)) = bounds.last() {
                rebuilt.extend_from_slice(&bytes[last_end..]);
            }
            bytes = rebuilt;
        }
    }

    let flips = rng.below(6);
    for _ in 0..flips {
        if bytes.is_empty() {
            break;
        }
        let pos = rng.below(bytes.len());
        let bit = rng.below(8);
        bytes[pos] ^= 1u8 << bit;
    }
    bytes
}

/// Generous enough that most pristine fixtures decode, tight enough that
/// the limit-refusal path is genuinely exercised by mutation.
fn limits() -> DecodeLimits {
    DecodeLimits::new(64, 64, 64 * 64)
}

/// Assert `decode` never panics, and that any successfully decoded image
/// actually respects the limits it was decoded under.
fn decode_never_panics_and_respects_limits(bytes: &[u8]) {
    let limits = limits();
    if let Ok(image) = decode(bytes, &limits) {
        assert!(image.width() <= limits.max_width());
        assert!(image.height() <= limits.max_height());
        assert!(u64::from(image.width()) * u64::from(image.height()) <= limits.max_pixels());
        assert_eq!(image.pixels().len(), image.into_pixels().len());
    }
    let _ = sniff(bytes);
}

#[test]
fn arbitrary_bytes_never_panic_and_respect_limits() {
    let mut rng = Lcg::new(tairix_fuzzseed::start(
        "arbitrary_bytes_never_panic_and_respect_limits",
        tairix_fuzzseed::FUZZ_SEED_ENV,
    ));
    let deadline = tairix_fuzzseed::budget_deadline(tairix_fuzzseed::FUZZ_BUDGET_ENV);
    let mut buf = Vec::new();
    loop {
        for _ in 0..SMOKE_ITERATIONS {
            buf.clear();
            let len = rng.below(300);
            buf.resize(len, 0);
            rng.fill(&mut buf);
            decode_never_panics_and_respects_limits(&buf);
        }
        if !tairix_fuzzseed::within_budget(deadline) {
            break;
        }
    }
}

#[test]
fn mutated_valid_fixtures_never_panic() {
    let mut rng = Lcg::new(tairix_fuzzseed::start(
        "mutated_valid_fixtures_never_panic",
        tairix_fuzzseed::FUZZ_SEED_ENV,
    ));
    let deadline = tairix_fuzzseed::budget_deadline(tairix_fuzzseed::FUZZ_BUDGET_ENV);
    loop {
        for _ in 0..SMOKE_ITERATIONS {
            let pristine = build_valid_png(&mut rng);
            let mutated = mutate(&mut rng, &pristine);
            decode_never_panics_and_respects_limits(&mutated);
        }
        if !tairix_fuzzseed::within_budget(deadline) {
            break;
        }
    }
}

#[test]
fn the_generator_produces_a_valid_corpus() {
    const DRAWS: u64 = 500;
    let mut rng = Lcg::new(tairix_fuzzseed::start(
        "the_generator_produces_a_valid_corpus",
        tairix_fuzzseed::FUZZ_SEED_ENV,
    ));
    let limits = limits();
    for _ in 0..DRAWS {
        let png = build_valid_png(&mut rng);
        assert!(
            decode(&png, &limits).is_ok(),
            "a pristine generated fixture failed to decode"
        );
    }
}
