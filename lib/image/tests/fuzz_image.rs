//! Deterministic fuzz harness for every image decoder (PNG, JPEG, GIF, BMP,
//! ICO/CUR, RISC OS sprite areas, and TIFF).
//!
//! Invariants, for any bytes an untrusted bundle icon, wallpaper, or opened
//! picture may carry:
//!
//! 1. [`decode`], [`decode_fitted`], and a full walk of [`Sequence`] never
//!    panic for any input, and never report a frame whose width, height, or
//!    pixel count exceeds the [`DecodeLimits`] they were given.
//! 2. Structure-aware mutations of a valid, builder-made file — bit flips,
//!    length/CRC tweaks, reordering of PNG chunks, JPEG marker segments,
//!    GIF blocks, TIFF directory entries, or icon directory entries, and
//!    overwriting a BMP header's or a TIFF directory's declared fields —
//!    never panic: the mutated bytes either decode within the limits or are
//!    refused with a typed error.
//! 3. The generators are not degenerate: every pristine fixture each one
//!    produces actually decodes (a corpus that never round-trips would
//!    leave invariant 2 exercising only the trivial "refused immediately"
//!    path).
//! 4. Garbage carrying a valid signature reaches the format decoder rather
//!    than stopping at the sniffer, so the entropy/scanline paths are fuzzed
//!    and not just the format dispatch.
//! 5. A sprite area has no signature to carry, so every input above is also
//!    driven through the format-naming door, which is both its only way in
//!    and free coverage from every other format's corpus.
//!
//! Every generator, and its chunk/zlib, marker/Huffman, and block/LZW
//! framing helpers, are deliberately self-contained: this harness only calls
//! `tairix_image`'s public API (exactly what a real consumer — the desktop
//! image sandbox — would do), never the crate's own chunk reader, CRC
//! table, Huffman builder, or code-stream writer, so a bug in any of those is
//! still caught here.

use tairix_fuzzseed::Lcg;
use tairix_image::{
    decode, decode_as, decode_fitted, probe_as, sniff, DecodeLimits, FitBox, ImageFormat, Sequence,
    SequenceKind,
};

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

// -----------------------------------------------------------------------
// JPEG fixtures
// -----------------------------------------------------------------------

/// The marker codes this generator emits (ITU-T T.81 §B.1.1.3), restated
/// here for the same reason the PNG signature is.
const SOI: u8 = 0xD8;
const EOI: u8 = 0xD9;
const SOF0: u8 = 0xC0;
const SOF2: u8 = 0xC2;
const DHT: u8 = 0xC4;
const DQT: u8 = 0xDB;
const DRI: u8 = 0xDD;
const SOS: u8 = 0xDA;
const APP0: u8 = 0xE0;
const RST0: u8 = 0xD0;

/// A standalone two-byte marker.
fn bare_marker(code: u8) -> Vec<u8> {
    vec![0xFF, code]
}

/// A marker segment: the marker, then the 2-byte big-endian length that
/// counts itself (ITU-T T.81 §B.1.1.4), then the payload.
fn segment(code: u8, payload: &[u8]) -> Vec<u8> {
    let mut out = bare_marker(code);
    let length = u16::try_from(payload.len() + 2).unwrap_or(u16::MAX);
    out.extend_from_slice(&length.to_be_bytes());
    out.extend_from_slice(payload);
    out
}

/// A `DQT` payload for table `index` with every element 1, so a dequantised
/// coefficient equals the value the entropy decoder produced.
fn dqt_payload(index: u8, precision16: bool) -> Vec<u8> {
    let mut out = vec![(u8::from(precision16) << 4) | index];
    for _ in 0..64 {
        if precision16 {
            out.extend_from_slice(&1u16.to_be_bytes());
        } else {
            out.push(1);
        }
    }
    out
}

/// A `DHT` payload holding the one-symbol canonical table this generator
/// codes with: the single symbol `0x00` at code length 1, i.e. the one-bit
/// code `0` (ITU-T T.81 Annex C). In a DC table that symbol means
/// "difference category 0" — a zero difference carrying no extra bits — and
/// in an AC table it means end-of-block.
fn dht_payload(class: u8, index: u8) -> Vec<u8> {
    let mut out = vec![(class << 4) | index];
    out.push(1); // one code of length 1
    out.extend_from_slice(&[0u8; 15]); // none of lengths 2..=16
    out.push(0x00); // the symbol that code stands for
    out
}

/// A `SOF` payload at 8-bit precision for `components`, each `(id, h, v)`
/// and all sharing quantisation table 0.
fn sof_payload(width: u32, height: u32, components: &[(u8, u32, u32)]) -> Vec<u8> {
    let mut out = vec![8];
    out.extend_from_slice(&u16::try_from(height).unwrap_or(u16::MAX).to_be_bytes());
    out.extend_from_slice(&u16::try_from(width).unwrap_or(u16::MAX).to_be_bytes());
    out.push(u8::try_from(components.len()).unwrap_or(0));
    for &(id, h, v) in components {
        let sampling = (u8::try_from(h).unwrap_or(1) << 4) | u8::try_from(v).unwrap_or(1);
        out.extend_from_slice(&[id, sampling, 0]);
    }
    out
}

/// A `SOS` payload naming `ids` (all on Huffman tables DC 0 / AC 0) over
/// the spectral band `start..=end` at successive approximation
/// `(high, low)`.
fn sos_payload(ids: &[u8], start: u8, end: u8, high: u8, low: u8) -> Vec<u8> {
    let mut out = vec![u8::try_from(ids.len()).unwrap_or(0)];
    for &id in ids {
        out.extend_from_slice(&[id, 0x00]);
    }
    out.extend_from_slice(&[start, end, (high << 4) | low]);
    out
}

/// A JPEG entropy-coded bit writer: MSB first, stuffing a `0x00` after any
/// data byte that comes out `0xFF` (ITU-T T.81 §B.1.1.5), and padding a
/// part-written byte with 1-bits before any marker.
struct Bits {
    out: Vec<u8>,
    acc: u32,
    count: u32,
}

impl Bits {
    fn new() -> Self {
        Self {
            out: Vec::new(),
            acc: 0,
            count: 0,
        }
    }

    fn put(&mut self, value: u32, len: u32) {
        for shift in (0..len).rev() {
            self.acc = (self.acc << 1) | ((value >> shift) & 1);
            self.count += 1;
            if self.count == 8 {
                let byte = u8::try_from(self.acc & 0xFF).unwrap_or(0);
                self.out.push(byte);
                if byte == 0xFF {
                    self.out.push(0x00);
                }
                self.acc = 0;
                self.count = 0;
            }
        }
    }

    fn pad_to_byte(&mut self) {
        while self.count != 0 {
            self.put(1, 1);
        }
    }

    /// Emit restart marker `RSTn`, which is a marker and so never stuffed.
    fn restart(&mut self, index: u8) {
        self.pad_to_byte();
        self.out.push(0xFF);
        self.out.push(RST0 + (index % 8));
    }

    fn finish(mut self) -> Vec<u8> {
        self.pad_to_byte();
        self.out
    }
}

/// One generated scan: its `SOS` payload, how many restart-interval units
/// it codes, and how many bits each of those units contributes.
///
/// Every block of every fixture is all-zero and both Huffman tables give
/// their single symbol the one-bit code `0`, so a coded block is simply a
/// run of `0` bits: two per block in a baseline/extended scan (DC category
/// 0, then end-of-block), and one per block for a progressive DC-first
/// coefficient, DC-refinement correction bit, or AC end-of-band symbol.
struct Scan {
    payload: Vec<u8>,
    units: u64,
    bits_per_unit: u32,
}

/// Append `scan`'s `SOS` segment and entropy-coded data to `out`, with a
/// restart marker after every `restart_interval` units but never after the
/// last one, which no decoder expects (ITU-T T.81 §B.2.5).
fn emit_scan(out: &mut Vec<u8>, scan: &Scan, restart_interval: u32) {
    out.extend(segment(SOS, &scan.payload));
    let mut bits = Bits::new();
    let mut since_restart = 0u32;
    let mut sequence = 0u8;
    for unit in 0..scan.units {
        bits.put(0, scan.bits_per_unit);
        if unit + 1 == scan.units {
            break;
        }
        since_restart += 1;
        if restart_interval > 0 && since_restart == restart_interval {
            bits.restart(sequence);
            sequence = (sequence + 1) % 8;
            since_restart = 0;
        }
    }
    out.extend(bits.finish());
}

/// The number of 8×8 blocks a scan naming exactly one component walks along
/// one axis (ITU-T T.81 §A.2.4): that component's own non-interleaved grid,
/// which is smaller than the padded MCU grid when it subsamples.
fn actual_blocks(natural: u32, factor: u32, factor_max: u32) -> u32 {
    let samples = u64::from(natural).saturating_mul(u64::from(factor));
    let extent = u32::try_from(samples.div_ceil(u64::from(factor_max.max(1)))).unwrap_or(1);
    extent.div_ceil(8)
}

/// Build one structurally valid, randomised JPEG: a flat mid-grey image
/// (every coefficient zero, so no coefficient value has to be encoded),
/// randomised over baseline vs progressive, 1 vs 3 components, dimensions,
/// chroma subsampling, quantisation-element precision, restart interval,
/// the optional JFIF `APP0` segment, and the progressive scan sequence.
fn build_valid_jpeg(rng: &mut Lcg) -> Vec<u8> {
    let progressive = rng.below(2) == 0;
    let width = u32::try_from(rng.below(24) + 1).unwrap_or(1);
    let height = u32::try_from(rng.below(24) + 1).unwrap_or(1);
    let components: Vec<(u8, u32, u32)> = if rng.below(2) == 0 {
        vec![(1, 1, 1)]
    } else {
        let h = u32::try_from(rng.below(2) + 1).unwrap_or(1);
        let v = u32::try_from(rng.below(2) + 1).unwrap_or(1);
        vec![(1, h, v), (2, 1, 1), (3, 1, 1)]
    };
    let restart_interval = u32::try_from(rng.below(3)).unwrap_or(0);

    let h_max = components.iter().map(|&(_, h, _)| h).max().unwrap_or(1);
    let v_max = components.iter().map(|&(_, _, v)| v).max().unwrap_or(1);
    let mcus = u64::from(width.div_ceil(8 * h_max)) * u64::from(height.div_ceil(8 * v_max));
    let blocks_per_mcu: u32 = components.iter().map(|&(_, h, v)| h * v).sum();
    let ids: Vec<u8> = components.iter().map(|&(id, _, _)| id).collect();

    let mut out = bare_marker(SOI);
    if rng.below(2) == 0 {
        // A JFIF APP0 segment, which the decoder must skip whole.
        out.extend(segment(APP0, b"JFIF\0\x01\x02\x00\x00\x01\x00\x01\x00\x00"));
    }
    out.extend(segment(DQT, &dqt_payload(0, rng.below(2) == 0)));
    out.extend(segment(DHT, &dht_payload(0, 0)));
    out.extend(segment(DHT, &dht_payload(1, 0)));
    out.extend(segment(
        if progressive { SOF2 } else { SOF0 },
        &sof_payload(width, height, &components),
    ));
    if restart_interval > 0 {
        let interval = u16::try_from(restart_interval).unwrap_or(1);
        out.extend(segment(DRI, &interval.to_be_bytes()));
    }

    // A scan naming every component is MCU-interleaved; one naming a single
    // component walks that component's own block grid instead.
    let (frame_units, blocks_per_frame_unit) = match components.as_slice() {
        [(_, h, v)] => (
            u64::from(actual_blocks(width, *h, h_max))
                * u64::from(actual_blocks(height, *v, v_max)),
            1,
        ),
        _ => (mcus, blocks_per_mcu),
    };

    if progressive {
        emit_scan(
            &mut out,
            &Scan {
                payload: sos_payload(&ids, 0, 0, 0, 1),
                units: frame_units,
                bits_per_unit: blocks_per_frame_unit,
            },
            restart_interval,
        );
        if rng.below(2) == 0 {
            emit_scan(
                &mut out,
                &Scan {
                    payload: sos_payload(&ids, 0, 0, 1, 0),
                    units: frame_units,
                    bits_per_unit: blocks_per_frame_unit,
                },
                restart_interval,
            );
        }
        for &(id, h, v) in &components {
            let units = u64::from(actual_blocks(width, h, h_max))
                * u64::from(actual_blocks(height, v, v_max));
            emit_scan(
                &mut out,
                &Scan {
                    payload: sos_payload(&[id], 1, 63, 0, 0),
                    units,
                    bits_per_unit: 1,
                },
                restart_interval,
            );
            if rng.below(2) == 0 {
                emit_scan(
                    &mut out,
                    &Scan {
                        payload: sos_payload(&[id], 1, 63, 1, 0),
                        units,
                        bits_per_unit: 1,
                    },
                    restart_interval,
                );
            }
        }
    } else {
        emit_scan(
            &mut out,
            &Scan {
                payload: sos_payload(&ids, 0, 63, 0, 0),
                units: frame_units,
                bits_per_unit: blocks_per_frame_unit * 2,
            },
            restart_interval,
        );
    }
    out.extend(bare_marker(EOI));
    out
}

/// The `(start, end)` byte range of every JPEG marker segment in the header
/// region: the walk stops at the first `SOS`, past which entropy-coded data
/// rather than framed segments follows.
fn segment_bounds(bytes: &[u8]) -> Vec<(usize, usize)> {
    let mut bounds = Vec::new();
    let mut pos = 2usize; // past SOI
    while pos + 4 <= bytes.len() {
        if bytes[pos] != 0xFF {
            break;
        }
        let code = bytes[pos + 1];
        if code == SOS || code == EOI {
            break;
        }
        let length = usize::from(u16::from_be_bytes([bytes[pos + 2], bytes[pos + 3]]));
        let Some(end) = pos.checked_add(2).and_then(|p| p.checked_add(length)) else {
            break;
        };
        if length < 2 || end > bytes.len() {
            break;
        }
        bounds.push((pos, end));
        pos = end;
    }
    bounds
}

/// Structurally mutate a pristine JPEG: maybe reorder two header segments,
/// maybe overwrite one segment's own declared length, then flip a handful
/// of random bits.
fn mutate_jpeg(rng: &mut Lcg, pristine: &[u8]) -> Vec<u8> {
    let mut bytes = pristine.to_vec();
    let bounds = segment_bounds(&bytes);
    if rng.below(2) == 0 {
        if let Some(rebuilt) = swap_two_ranges(rng, &bytes, &bounds) {
            bytes = rebuilt;
        }
    }
    if rng.below(2) == 0 {
        // A declared segment length is what every payload bound in the
        // parser is measured against, so it is worth corrupting on purpose
        // rather than only when a bit flip happens to land on it.
        if let Some(&(start, _)) = bounds.get(rng.below(bounds.len().max(1))) {
            if let Some(slot) = bytes.get_mut(start + 3) {
                *slot = u8::try_from(rng.below(256)).unwrap_or(0);
            }
        }
    }
    flip_bits(rng, &mut bytes);
    bytes
}

// -----------------------------------------------------------------------
// GIF fixtures
// -----------------------------------------------------------------------

/// The three magic bytes every GIF opens with, restated here for the same
/// reason the PNG signature is.
const GIF_MAGIC: [u8; 3] = *b"GIF";

/// Wrap a code stream in GIF data sub-blocks, terminator included.
fn gif_sub_blocks(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    for block in data.chunks(255) {
        out.push(u8::try_from(block.len()).unwrap_or(255));
        out.extend_from_slice(block);
    }
    out.push(0);
    out
}

/// A GIF LZW code stream of root codes only, written at the widths a
/// conforming decoder will read it at.
///
/// A decoder adds one dictionary entry per code after the first since a
/// clear and widens its reads once that fills the code space, whether or not
/// the writer used the table — so the schedule is the writer's obligation
/// even for a stream that compresses nothing.
fn gif_lzw(indices: &[u8], min_code_size: u8) -> Vec<u8> {
    let root_bits = u32::from(min_code_size);
    let clear = 1u16 << root_bits;
    let mut bytes = Vec::new();
    let mut accumulator = 0u32;
    let mut held = 0u32;
    let mut width = root_bits + 1;
    let mut next = clear + 2;
    let mut since_clear = 0u32;
    let mut push = |code: u16, width: u32, bytes: &mut Vec<u8>| {
        accumulator |= u32::from(code) << held;
        held += width;
        while held >= 8 {
            bytes.push(u8::try_from(accumulator & 0xFF).unwrap_or(0));
            accumulator >>= 8;
            held -= 8;
        }
    };
    push(clear, width, &mut bytes);
    for &index in indices {
        push(u16::from(index), width, &mut bytes);
        since_clear += 1;
        if since_clear >= 2 && next < 4096 {
            next += 1;
            if u32::from(next) >= (1u32 << width) && width < 12 {
                width += 1;
            }
        }
    }
    push(clear + 1, width, &mut bytes);
    if held > 0 {
        bytes.push(u8::try_from(accumulator & 0xFF).unwrap_or(0));
    }
    gif_sub_blocks(&bytes)
}

/// Build one structurally valid, randomised GIF: randomised screen size,
/// global-table presence, frame count, per-frame sub-rectangle,
/// interlacing, local tables, disposal method, transparency, delay, the
/// animation-loop extension, and interleaved comment blocks.
fn build_valid_gif(rng: &mut Lcg) -> Vec<u8> {
    let width = u16::try_from(rng.below(20) + 1).unwrap_or(1);
    let height = u16::try_from(rng.below(20) + 1).unwrap_or(1);
    // A frame with no table at all is refused, so at least one of the global
    // table and every frame's local table has to be there.
    let global = rng.below(4) != 0;
    let table_bits = u8::try_from(rng.below(3)).unwrap_or(0);
    let entries = 2usize << table_bits;
    let min_code_size = (table_bits + 1).max(2);
    let table: Vec<u8> = (0..entries * 3)
        .map(|at| u8::try_from(at % 251).unwrap_or(0))
        .collect();

    let mut out = GIF_MAGIC.to_vec();
    out.extend_from_slice(if rng.below(4) == 0 { b"87a" } else { b"89a" });
    out.extend_from_slice(&width.to_le_bytes());
    out.extend_from_slice(&height.to_le_bytes());
    out.push((if global { 0x80 } else { 0 }) | table_bits);
    out.push(u8::try_from(rng.below(entries)).unwrap_or(0));
    out.push(0);
    if global {
        out.extend_from_slice(&table);
    }
    if rng.below(2) == 0 {
        out.extend_from_slice(&[0x21, 0xFF, 0x0B]);
        out.extend_from_slice(b"NETSCAPE2.0");
        out.extend_from_slice(&[0x03, 0x01]);
        out.extend_from_slice(&u16::try_from(rng.below(4)).unwrap_or(0).to_le_bytes());
        out.push(0);
    }
    for _ in 0..=rng.below(3) {
        if rng.below(3) == 0 {
            out.extend_from_slice(&[0x21, 0xFE, 0x03, b'h', b'e', b'y', 0x00]);
        }
        let frame_w = u16::try_from(rng.below(usize::from(width)) + 1).unwrap_or(1);
        let frame_h = u16::try_from(rng.below(usize::from(height)) + 1).unwrap_or(1);
        let left = u16::try_from(rng.below(usize::from(width - frame_w) + 1)).unwrap_or(0);
        let top = u16::try_from(rng.below(usize::from(height - frame_h) + 1)).unwrap_or(0);
        let local = !global || rng.below(3) == 0;
        // Disposal 0..=3 only: 4..=7 are reserved and a decoder refuses them,
        // which would make the corpus degenerate.
        let disposal = u8::try_from(rng.below(4)).unwrap_or(0);
        let transparent =
            (rng.below(2) == 0).then(|| u8::try_from(rng.below(entries)).unwrap_or(0));
        out.extend_from_slice(&[0x21, 0xF9, 0x04]);
        out.push((disposal << 2) | u8::from(transparent.is_some()));
        out.extend_from_slice(&u16::try_from(rng.below(8)).unwrap_or(0).to_le_bytes());
        out.push(transparent.unwrap_or(0));
        out.push(0);

        out.push(0x2C);
        out.extend_from_slice(&left.to_le_bytes());
        out.extend_from_slice(&top.to_le_bytes());
        out.extend_from_slice(&frame_w.to_le_bytes());
        out.extend_from_slice(&frame_h.to_le_bytes());
        let interlaced = rng.below(3) == 0;
        out.push((if local { 0x80 } else { 0 }) | (if interlaced { 0x40 } else { 0 }) | table_bits);
        if local {
            out.extend_from_slice(&table);
        }
        out.push(min_code_size);
        let pixels = usize::from(frame_w) * usize::from(frame_h);
        let indices: Vec<u8> = (0..pixels)
            .map(|_| u8::try_from(rng.below(entries)).unwrap_or(0))
            .collect();
        out.extend_from_slice(&gif_lzw(&indices, min_code_size));
    }
    out.push(0x3B);
    out
}

/// The `(start, end)` byte range of every GIF block after the global colour
/// table, found by a best-effort forward walk that stops at the first block
/// running past the end of `bytes`.
fn gif_block_bounds(bytes: &[u8]) -> Vec<(usize, usize)> {
    let mut bounds = Vec::new();
    if bytes.len() < 13 || !bytes.starts_with(&GIF_MAGIC) {
        return bounds;
    }
    let packed = bytes[10];
    let mut pos = 13usize;
    if packed & 0x80 != 0 {
        pos += 3 << ((packed & 0x07) + 1);
    }
    // Walk a data sub-block chain, answering the offset past its terminator.
    let sub_blocks = |mut at: usize| -> Option<usize> {
        loop {
            let len = usize::from(*bytes.get(at)?);
            at = at.checked_add(1)?.checked_add(len)?;
            if at > bytes.len() {
                return None;
            }
            if len == 0 {
                return Some(at);
            }
        }
    };
    while let Some(&introducer) = bytes.get(pos) {
        let start = pos;
        let end = match introducer {
            0x2C => {
                let Some(&fields) = bytes.get(pos + 9) else {
                    break;
                };
                let mut at = pos + 10;
                if fields & 0x80 != 0 {
                    at += 3 << ((fields & 0x07) + 1);
                }
                // Past the minimum code size, then the code stream.
                match at.checked_add(1).and_then(sub_blocks) {
                    Some(end) => end,
                    None => break,
                }
            }
            0x21 => match pos.checked_add(2).and_then(sub_blocks) {
                Some(end) => end,
                None => break,
            },
            // The trailer, and anything a mutation left that this walk cannot
            // frame: either way there is no further block to bound.
            _ => break,
        };
        if end > bytes.len() {
            break;
        }
        bounds.push((start, end));
        pos = end;
    }
    bounds
}

/// Structurally mutate a pristine GIF: maybe reorder two blocks, maybe
/// overwrite one sub-block's declared length or one packed-fields byte, then
/// flip a handful of random bits.
fn mutate_gif(rng: &mut Lcg, pristine: &[u8]) -> Vec<u8> {
    let mut bytes = pristine.to_vec();
    let bounds = gif_block_bounds(&bytes);
    if rng.below(2) == 0 {
        if let Some(rebuilt) = swap_two_ranges(rng, &bytes, &bounds) {
            bytes = rebuilt;
        }
    }
    if rng.below(2) == 0 {
        // Every payload bound in the parser is measured against a declared
        // block or sub-block length, so those are worth corrupting on purpose
        // rather than only when a bit flip happens to land on one.
        if let Some(&(start, end)) = bounds.get(rng.below(bounds.len().max(1))) {
            let at = start + rng.below((end - start).max(1));
            if let Some(slot) = bytes.get_mut(at) {
                *slot = u8::try_from(rng.below(256)).unwrap_or(0);
            }
        }
    }
    flip_bits(rng, &mut bytes);
    bytes
}

// -----------------------------------------------------------------------
// BMP fixtures
// -----------------------------------------------------------------------

/// The two magic bytes every BMP file opens with, restated here for the
/// same reason the PNG signature is.
const BMP_MAGIC: [u8; 2] = *b"BM";

/// `BITMAPFILEHEADER`'s fixed length.
const BMP_FILE_HEADER: usize = 14;

/// The DIB header lengths the decoder claims: `BITMAPCOREHEADER`,
/// `BITMAPINFOHEADER`, and the `V2`/`V3`/`V4`/`V5` headers extending it.
const BMP_HEADER_LENS: [u32; 6] = [12, 40, 52, 56, 108, 124];

/// The bit counts the format defines.
const BMP_BIT_COUNTS: [u32; 7] = [1, 2, 4, 8, 16, 24, 32];

/// Bytes one row of `width` pixels at `bits` occupies once padded out to a
/// four-byte boundary.
fn bmp_stride(width: u32, bits: u32) -> usize {
    usize::try_from((u64::from(width) * u64::from(bits)).div_ceil(32) * 4).unwrap_or(0)
}

/// A run-length-encoded pixel array covering `height` rows of `width`
/// pixels exactly, mixing encoded and absolute runs.
fn bmp_rle(rng: &mut Lcg, width: u32, height: u32, four_bit: bool) -> Vec<u8> {
    let width = usize::try_from(width).unwrap_or(1);
    let mut out = Vec::new();
    for _ in 0..height {
        let mut x = 0usize;
        while x < width {
            let run = rng.below(width - x) + 1;
            if run >= 3 && rng.below(4) == 0 {
                out.push(0);
                out.push(u8::try_from(run).unwrap_or(3));
                let bytes = if four_bit { run.div_ceil(2) } else { run };
                for _ in 0..bytes {
                    out.push(u8::try_from(rng.below(256)).unwrap_or(0));
                }
                if bytes % 2 == 1 {
                    out.push(0);
                }
            } else {
                out.push(u8::try_from(run).unwrap_or(1));
                out.push(u8::try_from(rng.below(256)).unwrap_or(0));
            }
            x += run;
        }
        out.extend_from_slice(&[0, 0]);
    }
    out.extend_from_slice(&[0, 1]);
    out
}

/// A DIB header of `size` bytes, with the masks wherever the version or the
/// compression puts them and zeroes over the colour-space description that
/// may follow.
fn bmp_dib(size: u32, width: i32, height: i32, bits: u32, compression: u32) -> Vec<u8> {
    let mut out = size.to_le_bytes().to_vec();
    if size == 12 {
        out.extend_from_slice(&width.to_le_bytes()[..2]);
        out.extend_from_slice(&height.to_le_bytes()[..2]);
        out.extend_from_slice(&1u16.to_le_bytes());
        out.extend_from_slice(&u16::try_from(bits).unwrap_or(1).to_le_bytes());
        return out;
    }
    out.extend_from_slice(&width.to_le_bytes());
    out.extend_from_slice(&height.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&u16::try_from(bits).unwrap_or(1).to_le_bytes());
    out.extend_from_slice(&compression.to_le_bytes());
    for _ in 0..5 {
        out.extend_from_slice(&0u32.to_le_bytes());
    }
    let masks: [u32; 4] = if bits == 16 {
        [0xF800, 0x07E0, 0x001F, 0]
    } else {
        [0x00FF_0000, 0x0000_FF00, 0x0000_00FF, 0xFF00_0000]
    };
    let written = if compression == 6 || size >= 56 {
        4
    } else if compression == 3 || size >= 52 {
        3
    } else {
        0
    };
    for mask in masks.iter().take(written) {
        out.extend_from_slice(&mask.to_le_bytes());
    }
    while out.len() < usize::try_from(size).unwrap_or(0) {
        out.push(0);
    }
    out
}

/// Build one structurally valid, randomised BMP: a randomised header
/// version, geometry, row order, bit count, encoding, and colour table.
fn build_valid_bmp(rng: &mut Lcg) -> Vec<u8> {
    let size = *BMP_HEADER_LENS
        .get(rng.below(BMP_HEADER_LENS.len()))
        .unwrap_or(&40);
    let core = size == 12;
    let bits = *BMP_BIT_COUNTS
        .get(rng.below(BMP_BIT_COUNTS.len()))
        .unwrap_or(&24);
    let width = u32::try_from(rng.below(20) + 1).unwrap_or(1);
    let height = u32::try_from(rng.below(20) + 1).unwrap_or(1);
    // Only a `BITMAPINFOHEADER` or later can spell either of these, and a
    // run-length-encoded array may not be top-down.
    let top_down = !core && rng.below(4) == 0;
    let compression = if core {
        0
    } else if bits == 8 && !top_down && rng.below(3) == 0 {
        1
    } else if bits == 4 && !top_down && rng.below(3) == 0 {
        2
    } else if (bits == 16 || bits == 32) && rng.below(3) == 0 {
        if rng.below(2) == 0 {
            3
        } else {
            6
        }
    } else {
        0
    };

    // Always the bit count's full complement of entries, so any raw index
    // byte at any indexed depth is in range.
    let mut palette = vec![
        0u8;
        if bits <= 8 {
            (if core { 3 } else { 4 }) << bits
        } else {
            0
        }
    ];
    rng.fill(&mut palette);

    let mut pixels = match compression {
        1 => bmp_rle(rng, width, height, false),
        2 => bmp_rle(rng, width, height, true),
        _ => vec![0u8; bmp_stride(width, bits) * usize::try_from(height).unwrap_or(0)],
    };
    if compression == 0 || compression == 3 || compression == 6 {
        rng.fill(&mut pixels);
    }

    let signed_height = i32::try_from(height).unwrap_or(1);
    let dib = bmp_dib(
        size,
        i32::try_from(width).unwrap_or(1),
        if top_down {
            -signed_height
        } else {
            signed_height
        },
        bits,
        compression,
    );
    let offset = BMP_FILE_HEADER + dib.len() + palette.len();
    let mut out = BMP_MAGIC.to_vec();
    out.extend_from_slice(
        &u32::try_from(offset + pixels.len())
            .unwrap_or(0)
            .to_le_bytes(),
    );
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&u32::try_from(offset).unwrap_or(0).to_le_bytes());
    out.extend_from_slice(&dib);
    out.extend_from_slice(&palette);
    out.extend_from_slice(&pixels);
    out
}

/// The `(start, end)` byte range of every declared field a BMP's two
/// headers carry.
///
/// A BMP has no repeated blocks to reorder the way a PNG's chunks or a
/// GIF's blocks can be, so what is worth corrupting on purpose is the
/// fields the decoder sizes and bounds everything else from: the pixel-array
/// offset, the header length, the geometry, the bit count, the compression,
/// and the colour-table count.
fn bmp_fields(bytes: &[u8]) -> Vec<(usize, usize)> {
    if !bytes.starts_with(&BMP_MAGIC) {
        return Vec::new();
    }
    let mut fields = vec![(10, 14)];
    for at in [0usize, 4, 8, 12, 14, 16, 32] {
        let (start, end) = (BMP_FILE_HEADER + at, BMP_FILE_HEADER + at + 4);
        if end <= bytes.len() {
            fields.push((start, end));
        }
    }
    fields
}

/// Structurally mutate a pristine BMP: maybe overwrite one declared header
/// field, then flip a handful of random bits.
fn mutate_bmp(rng: &mut Lcg, pristine: &[u8]) -> Vec<u8> {
    let mut bytes = pristine.to_vec();
    let fields = bmp_fields(&bytes);
    if rng.below(2) == 0 {
        if let Some(&(start, end)) = fields.get(rng.below(fields.len().max(1))) {
            for at in start..end {
                if let Some(slot) = bytes.get_mut(at) {
                    *slot = u8::try_from(rng.below(256)).unwrap_or(0);
                }
            }
        }
    }
    flip_bits(rng, &mut bytes);
    bytes
}

// -----------------------------------------------------------------------
// ICO and CUR fixtures
// -----------------------------------------------------------------------

/// The four leading bytes of an icon and of a cursor container.
const ICO_HEADER: [u8; 4] = [0, 0, 1, 0];
const CUR_HEADER: [u8; 4] = [0, 0, 2, 0];

/// One entry's directory row.
const ICO_ENTRY_LEN: usize = 16;

/// One icon entry's bitmap: a `BITMAPINFOHEADER` declaring twice the
/// picture's height, its colour table, the colour rows, and the 1-bit mask
/// over them.
fn ico_dib_picture(rng: &mut Lcg, width: u32, height: u32) -> Vec<u8> {
    let bits = *BMP_BIT_COUNTS
        .get(rng.below(BMP_BIT_COUNTS.len()))
        .unwrap_or(&32);
    let top_down = rng.below(4) == 0;
    let signed = i32::try_from(height * 2).unwrap_or(2);
    let mut out = bmp_dib(
        40,
        i32::try_from(width).unwrap_or(1),
        if top_down { -signed } else { signed },
        bits,
        0,
    );
    let mut palette = vec![0u8; if bits <= 8 { 4usize << bits } else { 0 }];
    rng.fill(&mut palette);
    out.extend_from_slice(&palette);
    let rows = usize::try_from(height).unwrap_or(1);
    let mut colour = vec![0u8; bmp_stride(width, bits) * rows];
    rng.fill(&mut colour);
    out.extend_from_slice(&colour);
    let mut mask = vec![0u8; bmp_stride(width, 1) * rows];
    rng.fill(&mut mask);
    out.extend_from_slice(&mask);
    out
}

/// Build one structurally valid, randomised icon or cursor: a randomised
/// entry count, with each entry either a bitmap or a whole PNG file.
fn build_valid_ico(rng: &mut Lcg) -> Vec<u8> {
    let pictures: Vec<Vec<u8>> = (0..=rng.below(3))
        .map(|_| {
            if rng.below(4) == 0 {
                build_valid_png(rng)
            } else {
                let width = u32::try_from(rng.below(16) + 1).unwrap_or(1);
                let height = u32::try_from(rng.below(16) + 1).unwrap_or(1);
                ico_dib_picture(rng, width, height)
            }
        })
        .collect();

    let mut out = if rng.below(4) == 0 {
        CUR_HEADER.to_vec()
    } else {
        ICO_HEADER.to_vec()
    };
    out.extend_from_slice(&u16::try_from(pictures.len()).unwrap_or(0).to_le_bytes());
    let mut at = 6 + pictures.len() * ICO_ENTRY_LEN;
    for picture in &pictures {
        // The declared side and bit count are hints the decoder does not act
        // on, so they are randomised rather than made to agree.
        out.push(u8::try_from(rng.below(256)).unwrap_or(0));
        out.push(u8::try_from(rng.below(256)).unwrap_or(0));
        out.extend_from_slice(&[0, 0]);
        out.extend_from_slice(&0u32.to_le_bytes());
        out.extend_from_slice(&u32::try_from(picture.len()).unwrap_or(0).to_le_bytes());
        out.extend_from_slice(&u32::try_from(at).unwrap_or(0).to_le_bytes());
        at += picture.len();
    }
    for picture in &pictures {
        out.extend_from_slice(picture);
    }
    out
}

/// The `(start, end)` byte range of every directory row an icon declares.
fn ico_bounds(bytes: &[u8]) -> Vec<(usize, usize)> {
    if !bytes.starts_with(&ICO_HEADER) && !bytes.starts_with(&CUR_HEADER) {
        return Vec::new();
    }
    let Some(count) = bytes
        .get(4..6)
        .map(|c| usize::from(u16::from_le_bytes([c[0], c[1]])))
    else {
        return Vec::new();
    };
    (0..count)
        .map(|index| (6 + index * ICO_ENTRY_LEN, 6 + (index + 1) * ICO_ENTRY_LEN))
        .take_while(|&(_, end)| end <= bytes.len())
        .collect()
}

/// Structurally mutate a pristine icon: maybe swap two directory rows (so
/// every entry's declared length and offset describe the wrong picture),
/// maybe overwrite one row's fields, then flip a handful of random bits.
fn mutate_ico(rng: &mut Lcg, pristine: &[u8]) -> Vec<u8> {
    let mut bytes = pristine.to_vec();
    let bounds = ico_bounds(&bytes);
    if rng.below(2) == 0 {
        if let Some(rebuilt) = swap_two_ranges(rng, &bytes, &bounds) {
            bytes = rebuilt;
        }
    }
    if rng.below(2) == 0 {
        if let Some(&(start, end)) = bounds.get(rng.below(bounds.len().max(1))) {
            let at = start + rng.below((end - start).max(1));
            if let Some(slot) = bytes.get_mut(at) {
                *slot = u8::try_from(rng.below(256)).unwrap_or(0);
            }
        }
    }
    flip_bits(rng, &mut bytes);
    bytes
}

// -----------------------------------------------------------------------
// RISC OS sprite area fixtures
// -----------------------------------------------------------------------

/// A sprite area's own header, and one sprite's control block.
const SPRITE_AREA_HEADER: usize = 12;
const SPRITE_HEADER: usize = 44;

/// The sprite types this generator draws from, with the bits per pixel each
/// names. Restated here rather than asked of the crate under test.
const SPRITE_TYPES: [(u32, u32); 9] = [
    (1, 1),
    (2, 2),
    (3, 4),
    (4, 8),
    (5, 16),
    (6, 32),
    (8, 24),
    (10, 16),
    (16, 16),
];

/// Numbered screen modes at each depth the format defines.
const SPRITE_MODES: [(u32, u32); 4] = [(0, 1), (1, 2), (12, 4), (15, 8)];

/// One structurally valid sprite: a control block, an optional palette, the
/// image rows, and an optional mask.
fn build_valid_sprite(rng: &mut Lcg) -> Vec<u8> {
    let width = u32::try_from(rng.below(16) + 1).unwrap_or(1);
    let height = u32::try_from(rng.below(8) + 1).unwrap_or(1);
    // A numbered mode allows left-hand wastage; a mode word never does, and
    // type 16 exists only in a RISC OS 5 word.
    let (mode, bits, left) = match rng.below(3) {
        0 => {
            let (mode, bits) = SPRITE_MODES[rng.below(SPRITE_MODES.len())];
            (mode, bits, u32::try_from(rng.below(4)).unwrap_or(0) * bits)
        }
        1 => {
            let (kind, bits) = SPRITE_TYPES[rng.below(SPRITE_TYPES.len() - 1)];
            let wide = u32::from(rng.below(2) == 0) << 31;
            (wide | (kind << 27) | (90 << 14) | (90 << 1) | 1, bits, 0)
        }
        _ => {
            let (kind, bits) = SPRITE_TYPES[rng.below(SPRITE_TYPES.len())];
            // Bits 8-15 carry mode flags; only the RGB family decodes, and
            // alpha needs a fourth field, so the order bit alone is safe.
            let flags = u32::from(rng.below(2) == 0) << 6;
            let wide = u32::from(rng.below(2) == 0) << 31;
            (
                wide | (0xF << 27) | (kind << 20) | (flags << 8) | 1,
                bits,
                0,
            )
        }
    };
    let words = (left + width * bits).div_ceil(32);
    let stride = words as usize * 4;
    let rows = height as usize;

    let entries = if bits <= 8 {
        [0usize, 16, 64, 1usize << bits][rng.below(4)]
    } else {
        0
    };
    let mut palette = vec![0u8; entries * 8];
    rng.fill(&mut palette);
    let mut image = vec![0u8; stride * rows];
    rng.fill(&mut image);
    let mask = match rng.below(3) {
        0 => None,
        // A numbered mode's mask is the image's own depth and layout; a mode
        // word's is one bit per pixel, or eight when its top bit is set.
        _ => Some(if mode < 256 {
            vec![0u8; stride * rows]
        } else {
            let bits = if mode >> 31 & 1 == 1 { 8 } else { 1 };
            vec![0u8; (width * bits).div_ceil(32) as usize * 4 * rows]
        }),
    };

    let image_at = SPRITE_HEADER + palette.len();
    let mask_at = image_at + image.len();
    let total = mask_at + mask.as_ref().map_or(0, Vec::len);
    let mut out = Vec::new();
    let word = |out: &mut Vec<u8>, value: u32| out.extend_from_slice(&value.to_le_bytes());
    word(&mut out, u32::try_from(total).unwrap_or(0));
    out.extend_from_slice(b"fuzz\0\0\0\0\0\0\0\0");
    word(&mut out, words - 1);
    word(&mut out, height - 1);
    word(&mut out, left);
    word(&mut out, (left + width * bits - 1) % 32);
    word(&mut out, u32::try_from(image_at).unwrap_or(0));
    word(
        &mut out,
        u32::try_from(if mask.is_some() { mask_at } else { image_at }).unwrap_or(0),
    );
    word(&mut out, mode);
    out.extend_from_slice(&palette);
    out.extend_from_slice(&image);
    if let Some(mask) = &mask {
        out.extend_from_slice(mask);
    }
    out
}

/// Build one structurally valid sprite area: a randomised count of sprites,
/// each of a randomised depth, mode word form, palette length, and mask.
fn build_valid_sprite_area(rng: &mut Lcg) -> Vec<u8> {
    let sprites: Vec<Vec<u8>> = (0..=rng.below(3))
        .map(|_| build_valid_sprite(rng))
        .collect();
    let total = SPRITE_AREA_HEADER + sprites.iter().map(Vec::len).sum::<usize>();
    let mut out = Vec::new();
    out.extend_from_slice(&u32::try_from(sprites.len()).unwrap_or(0).to_le_bytes());
    // Every offset a file states is four greater than the position it names.
    out.extend_from_slice(&(u32::try_from(SPRITE_AREA_HEADER).unwrap_or(0) + 4).to_le_bytes());
    out.extend_from_slice(&(u32::try_from(total).unwrap_or(0) + 4).to_le_bytes());
    for sprite in &sprites {
        out.extend_from_slice(sprite);
    }
    out
}

/// The `(start, end)` byte range of every control block a best-effort walk
/// of the chain finds.
fn sprite_bounds(bytes: &[u8]) -> Vec<(usize, usize)> {
    let read = |at: usize| {
        bytes
            .get(at..)
            .and_then(<[u8]>::first_chunk::<4>)
            .map(|word| u32::from_le_bytes(*word) as usize)
    };
    let (Some(count), Some(first)) = (read(0), read(4)) else {
        return Vec::new();
    };
    let mut at = first.saturating_sub(4);
    let mut bounds = Vec::new();
    for _ in 0..count.min(bytes.len() / SPRITE_HEADER) {
        if at + SPRITE_HEADER > bytes.len() {
            break;
        }
        bounds.push((at, at + SPRITE_HEADER));
        match read(at) {
            Some(length) if length >= SPRITE_HEADER => at += length,
            _ => break,
        }
    }
    bounds
}

/// Structurally mutate a pristine sprite area: maybe swap two control blocks
/// (so every offset within one describes the wrong payload), maybe overwrite
/// one field of one block, then flip a handful of random bits.
fn mutate_sprite(rng: &mut Lcg, pristine: &[u8]) -> Vec<u8> {
    let mut bytes = pristine.to_vec();
    let bounds = sprite_bounds(&bytes);
    if rng.below(2) == 0 {
        if let Some(rebuilt) = swap_two_ranges(rng, &bytes, &bounds) {
            bytes = rebuilt;
        }
    }
    if rng.below(2) == 0 {
        if let Some(&(start, _)) = bounds.get(rng.below(bounds.len().max(1))) {
            let at = start + rng.below(SPRITE_HEADER / 4) * 4;
            let value = u32::try_from(rng.next_u64() & u64::from(u32::MAX)).unwrap_or(0);
            if let Some(slot) = bytes.get_mut(at..at + 4) {
                slot.copy_from_slice(&value.to_le_bytes());
            }
        }
    }
    // The area header decides where the chain starts and stops, so it is
    // worth mutating on its own.
    if rng.below(4) == 0 {
        let at = rng.below(3) * 4;
        let value = u32::try_from(rng.next_u64() & u64::from(u32::MAX)).unwrap_or(0);
        if let Some(slot) = bytes.get_mut(at..at + 4) {
            slot.copy_from_slice(&value.to_le_bytes());
        }
    }
    flip_bits(rng, &mut bytes);
    bytes
}

// -----------------------------------------------------------------------
// TIFF fixtures
// -----------------------------------------------------------------------

/// The four openings a TIFF can have: the byte-order mark then the version,
/// restated here because this harness builds its own files.
const TIFF_HEADERS: [[u8; 4]; 4] = [
    [b'I', b'I', 42, 0],
    [b'I', b'I', 43, 0],
    [b'M', b'M', 0, 42],
    [b'M', b'M', 0, 43],
];

/// A directory entry's fixed length.
const TIFF_ENTRY_LEN: usize = 12;

/// The compressions the decoder claims. The generator writes only the three
/// it can encode; a mutation rewrites the tag to any of them, which is what
/// drives the LZW, fax, and JPEG paths with structured-but-wrong payloads.
const TIFF_COMPRESSIONS: [u16; 9] = [1, 2, 3, 4, 5, 7, 8, 32773, 32946];

/// One field of a directory under construction: tag, type, and the values
/// in little-endian element order.
type TiffField = (u16, u16, Vec<u8>);

fn tiff_u16(out: &mut Vec<u8>, big: bool, value: u16) {
    out.extend_from_slice(&if big {
        value.to_be_bytes()
    } else {
        value.to_le_bytes()
    });
}

fn tiff_u32(out: &mut Vec<u8>, big: bool, value: u32) {
    out.extend_from_slice(&if big {
        value.to_be_bytes()
    } else {
        value.to_le_bytes()
    });
}

/// Bytes one element of a field type occupies.
fn tiff_width(kind: u16) -> usize {
    match kind {
        1 | 2 | 6 | 7 => 1,
        3 | 8 => 2,
        5 | 10 | 12 => 8,
        _ => 4,
    }
}

/// A field's bytes in the document's byte order.
fn tiff_ordered(big: bool, kind: u16, bytes: &[u8]) -> Vec<u8> {
    let step = tiff_width(kind);
    if !big || step == 1 {
        return bytes.to_vec();
    }
    let step = if kind == 5 || kind == 10 { 4 } else { step };
    bytes
        .chunks(step)
        .flat_map(|chunk| chunk.iter().rev().copied())
        .collect()
}

fn tiff_shorts(values: &[u16]) -> Vec<u8> {
    values
        .iter()
        .flat_map(|value| value.to_le_bytes())
        .collect()
}

fn tiff_longs(values: &[u32]) -> Vec<u8> {
    values
        .iter()
        .flat_map(|value| value.to_le_bytes())
        .collect()
}

/// Write one page's directory, out-of-line values, and units, chaining on
/// unless it is the `last`.
fn tiff_page(out: &mut Vec<u8>, big: bool, fields: &[TiffField], units: &[Vec<u8>], last: bool) {
    let mut fields = fields.to_vec();
    let counts: Vec<u32> = units
        .iter()
        .map(|unit| u32::try_from(unit.len()).unwrap_or(0))
        .collect();
    // A tiled page names its arrays by the tile tags and a stripped one by
    // the strip tags; the caller states which by seeding a tile width.
    let tiled = fields.iter().any(|(tag, _, _)| *tag == 322);
    let (offsets_tag, counts_tag) = if tiled { (324, 325) } else { (273, 279) };
    fields.push((offsets_tag, 4, vec![0u8; units.len() * 4]));
    fields.push((counts_tag, 4, tiff_longs(&counts)));
    fields.sort_by_key(|(tag, _, _)| *tag);

    let ifd_at = out.len();
    let ifd_len = 2 + fields.len() * TIFF_ENTRY_LEN + 4;
    let mut cursor = ifd_at + ifd_len;
    let mut places = Vec::new();
    for (_, kind, values) in &fields {
        let len = tiff_ordered(big, *kind, values).len();
        if len <= 4 {
            places.push(None);
        } else {
            places.push(Some(cursor));
            cursor += len;
        }
    }
    let mut unit_offsets = Vec::new();
    for unit in units {
        unit_offsets.push(u32::try_from(cursor).unwrap_or(0));
        cursor += unit.len();
    }
    let next = if last { 0 } else { cursor };

    let values: Vec<Vec<u8>> = fields
        .iter()
        .map(|(tag, _, values)| {
            if *tag == offsets_tag {
                tiff_longs(&unit_offsets)
            } else {
                values.clone()
            }
        })
        .collect();
    tiff_u16(out, big, u16::try_from(fields.len()).unwrap_or(0));
    for (((tag, kind, _), place), field) in fields.iter().zip(&places).zip(&values) {
        let count = u32::try_from(field.len() / tiff_width(*kind)).unwrap_or(0);
        tiff_u16(out, big, *tag);
        tiff_u16(out, big, *kind);
        tiff_u32(out, big, count);
        if let Some(at) = place {
            tiff_u32(out, big, u32::try_from(*at).unwrap_or(0));
        } else {
            let mut inline = tiff_ordered(big, *kind, field);
            inline.resize(4, 0);
            out.extend_from_slice(&inline);
        }
    }
    tiff_u32(out, big, u32::try_from(next).unwrap_or(0));
    for ((_, kind, _), (place, field)) in fields.iter().zip(places.iter().zip(&values)) {
        if place.is_some() {
            out.extend_from_slice(&tiff_ordered(big, *kind, field));
        }
    }
    for unit in units {
        out.extend_from_slice(unit);
    }
}

/// `PackBits`-encode `data` as literal runs.
fn tiff_pack_bits(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut at = 0usize;
    while at < data.len() {
        let take = (data.len() - at).min(128);
        out.push(u8::try_from(take - 1).unwrap_or(0));
        out.extend_from_slice(&data[at..at + take]);
        at += take;
    }
    out
}

/// Wrap `data` as a zlib stream of stored DEFLATE blocks, which is what
/// TIFF's Deflate compression carries.
fn tiff_zlib(data: &[u8]) -> Vec<u8> {
    let mut out = vec![0x78, 0x01];
    let mut at = 0usize;
    loop {
        let take = (data.len() - at).min(0xFFFF);
        let last = at + take == data.len();
        out.push(u8::from(last));
        let len = u16::try_from(take).unwrap_or(0);
        out.extend_from_slice(&len.to_le_bytes());
        out.extend_from_slice(&(!len).to_le_bytes());
        out.extend_from_slice(&data[at..at + take]);
        at += take;
        if last {
            break;
        }
    }
    let (mut a, mut b) = (1u32, 0u32);
    for &byte in data {
        a = (a + u32::from(byte)) % 65521;
        b = (b + a) % 65521;
    }
    out.extend_from_slice(&(b << 16 | a).to_be_bytes());
    out
}

/// One page's fields and units, over a random photometric, bit depth,
/// plane arrangement, orientation, predictor, and strip or tile grid.
fn tiff_random_page(rng: &mut Lcg) -> (Vec<TiffField>, Vec<Vec<u8>>) {
    let photometric = [0u16, 1, 2, 3][rng.below(4)];
    let bits = if photometric == 3 {
        [1u16, 2, 4, 8][rng.below(4)]
    } else {
        [1u16, 2, 4, 8, 16][rng.below(5)]
    };
    let samples = if photometric == 2 { 3u16 } else { 1 };
    let planar = photometric == 2 && rng.below(3) == 0;
    let width = 1 + u32::try_from(rng.below(24)).unwrap_or(0);
    let height = 1 + u32::try_from(rng.below(24)).unwrap_or(0);
    let tiled = rng.below(4) == 0;
    let compression = [1u16, 8, 32773][rng.below(3)];

    let mut fields: Vec<TiffField> = vec![
        (256, 4, tiff_longs(&[width])),
        (257, 4, tiff_longs(&[height])),
        (258, 3, tiff_shorts(&vec![bits; usize::from(samples)])),
        (259, 3, tiff_shorts(&[compression])),
        (262, 3, tiff_shorts(&[photometric])),
        (
            274,
            3,
            tiff_shorts(&[1 + u16::try_from(rng.below(8)).unwrap_or(0)]),
        ),
        (277, 3, tiff_shorts(&[samples])),
        (284, 3, tiff_shorts(&[if planar { 2 } else { 1 }])),
    ];
    if photometric == 3 {
        let mut map = vec![0u16; (1usize << bits) * 3];
        for slot in &mut map {
            *slot = u16::try_from(rng.below(0x1_0000)).unwrap_or(0);
        }
        fields.push((320, 3, tiff_shorts(&map)));
    }
    if bits == 8 && rng.below(3) == 0 {
        fields.push((317, 3, tiff_shorts(&[2])));
    }

    let planes = if planar { u32::from(samples) } else { 1 };
    let per_plane = if planar { 1 } else { u32::from(samples) };
    let (columns, rows, across, down) = if tiled {
        let columns = 16 * (1 + u32::try_from(rng.below(2)).unwrap_or(0));
        fields.push((322, 4, tiff_longs(&[columns])));
        fields.push((323, 4, tiff_longs(&[16])));
        (columns, 16, width.div_ceil(columns), height.div_ceil(16))
    } else {
        let rows = (1 + u32::try_from(rng.below(8)).unwrap_or(0)).min(height);
        fields.push((278, 4, tiff_longs(&[rows])));
        (width, rows, 1, height.div_ceil(rows))
    };
    let row_bytes =
        usize::try_from((u64::from(columns) * u64::from(per_plane) * u64::from(bits)).div_ceil(8))
            .unwrap_or(0);
    let mut units = Vec::new();
    for _ in 0..planes {
        for down_index in 0..down {
            let unit_rows = if tiled {
                rows
            } else {
                (height - down_index * rows).min(rows)
            };
            for _ in 0..across {
                let mut raw = vec![0u8; row_bytes * usize::try_from(unit_rows).unwrap_or(0)];
                rng.fill(&mut raw);
                units.push(match compression {
                    8 => tiff_zlib(&raw),
                    32773 => tiff_pack_bits(&raw),
                    _ => raw,
                });
            }
        }
    }
    (fields, units)
}

/// Build one structurally valid, randomised TIFF of one or two pages.
fn build_valid_tiff(rng: &mut Lcg) -> Vec<u8> {
    let big = rng.below(2) == 0;
    let mut out = Vec::new();
    out.extend_from_slice(if big { b"MM" } else { b"II" });
    tiff_u16(&mut out, big, 42);
    tiff_u32(&mut out, big, 8);
    let pages = 1 + rng.below(2);
    for page in 0..pages {
        let (fields, units) = tiff_random_page(rng);
        tiff_page(&mut out, big, &fields, &units, page + 1 == pages);
    }
    out
}

/// The byte range of every entry of a TIFF's *first* directory, for a swap
/// that reorders two of them.
///
/// One directory only, because [`swap_two_ranges`] rebuilds the span its
/// bounds cover and a later page's entries are separated from the first
/// page's by that page's values and units.
fn tiff_bounds(bytes: &[u8]) -> Vec<(usize, usize)> {
    let big = match bytes.first_chunk::<2>() {
        Some(b"MM") => true,
        Some(b"II") => false,
        _ => return Vec::new(),
    };
    let read16 = |at: usize| {
        bytes.get(at..at + 2).map(|pair| {
            let pair = [pair[0], pair[1]];
            if big {
                u16::from_be_bytes(pair)
            } else {
                u16::from_le_bytes(pair)
            }
        })
    };
    let read32 = |at: usize| {
        bytes.get(at..at + 4).map(|quad| {
            let quad = [quad[0], quad[1], quad[2], quad[3]];
            if big {
                u32::from_be_bytes(quad)
            } else {
                u32::from_le_bytes(quad)
            }
        })
    };
    let mut bounds = Vec::new();
    let at = match read32(4).and_then(|at| usize::try_from(at).ok()) {
        Some(at) if at != 0 => at,
        _ => return bounds,
    };
    let Some(count) = read16(at) else {
        return bounds;
    };
    for index in 0..usize::from(count) {
        let entry = at + 2 + index * TIFF_ENTRY_LEN;
        if entry + TIFF_ENTRY_LEN > bytes.len() {
            return bounds;
        }
        bounds.push((entry, entry + TIFF_ENTRY_LEN));
    }
    bounds
}

/// Structurally mutate a pristine TIFF: maybe reorder two directory
/// entries, maybe rewrite one into a compression tag so a payload reaches a
/// decoder it was not written for, then flip a handful of bits.
fn mutate_tiff(rng: &mut Lcg, pristine: &[u8]) -> Vec<u8> {
    let mut bytes = pristine.to_vec();
    if rng.below(3) == 0 {
        let bounds = tiff_bounds(&bytes);
        if let Some(rebuilt) = swap_two_ranges(rng, &bytes, &bounds) {
            bytes = rebuilt;
        }
    }
    let bounds = tiff_bounds(&bytes);
    if rng.below(2) == 0 && !bounds.is_empty() {
        let big = bytes.first_chunk::<2>() == Some(b"MM");
        let (entry, _) = bounds[rng.below(bounds.len())];
        let compression = TIFF_COMPRESSIONS[rng.below(TIFF_COMPRESSIONS.len())];
        let order16 = |value: u16| {
            if big {
                value.to_be_bytes()
            } else {
                value.to_le_bytes()
            }
        };
        // Tag 259 of SHORT type, count one, so the rewrite lands as a value
        // the page will act on.
        bytes[entry..entry + 2].copy_from_slice(&order16(259));
        bytes[entry + 2..entry + 4].copy_from_slice(&order16(3));
        bytes[entry + 4..entry + 8].copy_from_slice(&if big {
            1u32.to_be_bytes()
        } else {
            1u32.to_le_bytes()
        });
        bytes[entry + 8..entry + 10].copy_from_slice(&order16(compression));
        bytes[entry + 10..entry + 12].fill(0);
    }
    flip_bits(rng, &mut bytes);
    bytes
}

// ---------------------------------------------------------------------------
// WEBP
// ---------------------------------------------------------------------------

/// The two halves of the WEBP form identifier, restated here because this
/// harness only ever calls the crate's public API.
const WEBP_RIFF: [u8; 4] = *b"RIFF";
const WEBP_FORM: [u8; 4] = *b"WEBP";

/// The byte a lossless bitstream opens with.
const WEBP_LOSSLESS_SIGNATURE: u8 = 0x2F;

/// The three bytes a lossy keyframe carries after its frame tag.
const WEBP_START_CODE: [u8; 3] = [0x9D, 0x01, 0x2A];

/// A bit stream written least significant bit first, as a lossless
/// bitstream packs one.
struct WebpBits {
    bytes: Vec<u8>,
    pos: usize,
}

impl WebpBits {
    fn new() -> Self {
        Self {
            bytes: Vec::new(),
            pos: 0,
        }
    }

    fn put(&mut self, value: u32, count: u32) {
        for index in 0..count {
            if self.pos.is_multiple_of(8) {
                self.bytes.push(0);
            }
            let at = self.pos / 8;
            self.bytes[at] |= u8::try_from((value >> index) & 1).unwrap_or(0) << (self.pos % 8);
            self.pos += 1;
        }
    }
}

/// A prefix code naming exactly one symbol, which costs no bits to read.
fn webp_lone(bits: &mut WebpBits, symbol: u32) {
    bits.put(1, 1);
    bits.put(0, 1);
    if symbol < 2 {
        bits.put(0, 1);
        bits.put(symbol, 1);
    } else {
        bits.put(1, 1);
        bits.put(symbol, 8);
    }
}

/// The five prefix codes of one group, each naming a single symbol.
fn webp_flat_group(bits: &mut WebpBits, colour: [u8; 4]) {
    for value in [colour[1], colour[0], colour[2], colour[3]] {
        webp_lone(bits, u32::from(value));
    }
    webp_lone(bits, 0);
}

/// A whole lossless bitstream of one colour.
fn webp_lossless(rng: &mut Lcg, width: u32, height: u32) -> Vec<u8> {
    let mut bits = WebpBits::new();
    bits.put(u32::from(WEBP_LOSSLESS_SIGNATURE), 8);
    bits.put(width - 1, 14);
    bits.put(height - 1, 14);
    bits.put(0, 1);
    bits.put(0, 3);
    bits.put(0, 1);
    bits.put(0, 1);
    bits.put(0, 1);
    let colour = [
        u8::try_from(rng.below(256)).unwrap_or(0),
        u8::try_from(rng.below(256)).unwrap_or(0),
        u8::try_from(rng.below(256)).unwrap_or(0),
        u8::try_from(rng.below(256)).unwrap_or(0),
    ];
    webp_flat_group(&mut bits, colour);
    bits.bytes
}

/// A lossless bitstream over one plane, as a compressed alpha chunk carries
/// one: no signature and no geometry of its own.
fn webp_alpha_stream(value: u8) -> Vec<u8> {
    let mut bits = WebpBits::new();
    bits.put(0, 1);
    bits.put(0, 1);
    bits.put(0, 1);
    webp_flat_group(&mut bits, [0, value, 0, 0]);
    bits.bytes
}

/// A lossy keyframe whose uncompressed header is valid and whose
/// compressed partition is random bytes.
///
/// The compressed part is an *arithmetic* code, so any byte string decodes
/// to some sequence of boolean choices: a random partition therefore walks
/// the whole header — segmentation, the loop-filter and quantiser deltas,
/// every one of the token-probability update flags, the mode trees, and the
/// coefficient tokens — without the harness needing to restate a single one
/// of the format's probability tables. It reaches far more of the decoder
/// than a flat frame would, and the picture it produces is one the decoder
/// is free to refuse.
fn webp_lossy(rng: &mut Lcg, width: u32, height: u32) -> Vec<u8> {
    let mut partition = vec![0u8; 24 + rng.below(200)];
    rng.fill(&mut partition);
    let mut out = Vec::new();
    // The frame tag: a keyframe, profile zero, shown, and the length of the
    // first partition.
    let split = 1 + rng.below(partition.len());
    let tag = u32::try_from(split).unwrap_or(0) << 5 | (1 << 4);
    out.push(u8::try_from(tag & 0xFF).unwrap_or(0));
    out.push(u8::try_from((tag >> 8) & 0xFF).unwrap_or(0));
    out.push(u8::try_from((tag >> 16) & 0xFF).unwrap_or(0));
    out.extend_from_slice(&WEBP_START_CODE);
    out.extend_from_slice(&u16::try_from(width).unwrap_or(0).to_le_bytes());
    out.extend_from_slice(&u16::try_from(height).unwrap_or(0).to_le_bytes());
    out.extend_from_slice(&partition);
    out
}

/// One RIFF chunk, padded to an even length.
fn webp_chunk(id: [u8; 4], payload: &[u8]) -> Vec<u8> {
    let mut out = id.to_vec();
    out.extend_from_slice(&u32::try_from(payload.len()).unwrap_or(0).to_le_bytes());
    out.extend_from_slice(payload);
    if payload.len() % 2 == 1 {
        out.push(0);
    }
    out
}

/// A whole RIFF form over the chunks given.
fn webp_riff(chunks: &[Vec<u8>]) -> Vec<u8> {
    let mut body = WEBP_FORM.to_vec();
    for chunk in chunks {
        body.extend_from_slice(chunk);
    }
    let mut out = WEBP_RIFF.to_vec();
    out.extend_from_slice(&u32::try_from(body.len()).unwrap_or(0).to_le_bytes());
    out.extend_from_slice(&body);
    out
}

/// The extended header's payload.
fn webp_extended(flags: u8, width: u32, height: u32) -> Vec<u8> {
    let mut out = vec![flags, 0, 0, 0];
    out.extend_from_slice(&(width - 1).to_le_bytes()[..3]);
    out.extend_from_slice(&(height - 1).to_le_bytes()[..3]);
    out
}

/// Build one structurally valid, randomised WEBP that genuinely decodes: a
/// simple lossless file, an extended still with an alpha plane, or an
/// animation.
fn build_valid_webp(rng: &mut Lcg) -> Vec<u8> {
    let width = 8 + 8 * u32::try_from(rng.below(3)).unwrap_or(0);
    let height = 8 + 8 * u32::try_from(rng.below(3)).unwrap_or(0);
    match rng.below(4) {
        0 => webp_riff(&[webp_chunk(*b"VP8L", &webp_lossless(rng, width, height))]),
        1 => webp_riff(&[
            webp_chunk(*b"VP8X", &webp_extended(0, width, height)),
            webp_chunk(*b"VP8L", &webp_lossless(rng, width, height)),
        ]),
        2 => webp_riff(&[
            webp_chunk(*b"VP8X", &webp_extended(0x2C, width, height)),
            webp_chunk(*b"ICCP", &[1, 2, 3]),
            webp_chunk(*b"VP8L", &webp_lossless(rng, width, height)),
            webp_chunk(*b"XMP ", &[4]),
        ]),
        _ => {
            let frames = 1 + rng.below(3);
            let mut chunks = vec![
                webp_chunk(*b"VP8X", &webp_extended(0x02, width, height)),
                webp_chunk(
                    *b"ANIM",
                    &[0, 0, 0, 0, u8::try_from(rng.below(4)).unwrap_or(0), 0],
                ),
            ];
            for _ in 0..frames {
                let mut body = Vec::new();
                let duration = u32::try_from(rng.below(100)).unwrap_or(0);
                for value in [0u32, 0, width - 1, height - 1, duration] {
                    body.extend_from_slice(&value.to_le_bytes()[..3]);
                }
                body.push(u8::try_from(rng.below(4)).unwrap_or(0));
                body.extend_from_slice(&webp_chunk(*b"VP8L", &webp_lossless(rng, width, height)));
                chunks.push(webp_chunk(*b"ANMF", &body));
            }
            webp_riff(&chunks)
        }
    }
}

/// Build a WEBP carrying a lossy bitstream, with or without an alpha plane.
///
/// Its picture is whatever the random partition decodes to, or a refusal, so
/// this feeds the mutation sweep rather than the pristine corpus.
fn build_lossy_webp(rng: &mut Lcg) -> Vec<u8> {
    let width = 16 + 16 * u32::try_from(rng.below(2)).unwrap_or(0);
    let height = 16 + 16 * u32::try_from(rng.below(2)).unwrap_or(0);
    let bitstream = webp_chunk(*b"VP8 ", &webp_lossy(rng, width, height));
    if rng.below(2) == 0 {
        return webp_riff(&[bitstream]);
    }
    let value = u8::try_from(rng.below(256)).unwrap_or(0);
    let filter = u8::try_from(rng.below(4)).unwrap_or(0);
    let method = u8::try_from(rng.below(2)).unwrap_or(0);
    let mut alpha = vec![method | (filter << 2)];
    if method == 0 {
        let count = usize::try_from(width * height).unwrap_or(0);
        alpha.extend(core::iter::repeat_n(value, count));
    } else {
        alpha.extend_from_slice(&webp_alpha_stream(value));
    }
    webp_riff(&[
        webp_chunk(*b"VP8X", &webp_extended(0x10, width, height)),
        webp_chunk(*b"ALPH", &alpha),
        bitstream,
    ])
}

/// The byte range of every chunk of a WEBP form, for a swap that reorders
/// two of them.
///
/// The chunks are contiguous, so the whole list satisfies
/// [`swap_two_ranges`]' covering requirement.
fn webp_bounds(bytes: &[u8]) -> Vec<(usize, usize)> {
    let mut bounds = Vec::new();
    let mut pos = WEBP_RIFF.len() + 4 + WEBP_FORM.len();
    while pos + 8 <= bytes.len() {
        let size = u32::from_le_bytes([
            bytes[pos + 4],
            bytes[pos + 5],
            bytes[pos + 6],
            bytes[pos + 7],
        ]);
        let Some(end) = usize::try_from(size)
            .ok()
            .and_then(|size| pos.checked_add(8 + size + size % 2))
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

/// Structurally mutate a pristine WEBP: maybe reorder two chunks, then flip
/// a handful of random bits.
fn mutate_webp(rng: &mut Lcg, pristine: &[u8]) -> Vec<u8> {
    let mut bytes = pristine.to_vec();
    let bounds = webp_bounds(&bytes);
    if rng.below(2) == 0 {
        if let Some(rebuilt) = swap_two_ranges(rng, &bytes, &bounds) {
            bytes = rebuilt;
        }
    }
    flip_bits(rng, &mut bytes);
    bytes
}

// -----------------------------------------------------------------------
// Mutation and invariants
// -----------------------------------------------------------------------

/// The `(start, end)` byte range of every PNG chunk (including its length,
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

/// Swap two of the framed `bounds` ranges of `bytes`, leaving everything
/// outside them (the signature ahead of the first range, and whatever
/// follows the last) exactly where it was. `None` when there is nothing to
/// swap. Shared by every format's mutator: a PNG chunk, a JPEG marker
/// segment, and a TIFF directory entry are all "a framed range the decoder
/// must re-walk".
///
/// The ranges must **cover** the span from the first to the last, because
/// the rebuild concatenates them: bounds with a gap between two of them
/// would drop whatever sat in it.
fn swap_two_ranges(rng: &mut Lcg, bytes: &[u8], bounds: &[(usize, usize)]) -> Option<Vec<u8>> {
    if bounds.len() < 2 {
        return None;
    }
    let i = rng.below(bounds.len());
    let j = rng.below(bounds.len());
    if i == j {
        return None;
    }
    let &(first_start, _) = bounds.first()?;
    let &(_, last_end) = bounds.last()?;
    let mut pieces: Vec<&[u8]> = bounds.iter().map(|&(s, e)| &bytes[s..e]).collect();
    pieces.swap(i, j);
    let mut rebuilt = bytes[..first_start].to_vec();
    for piece in pieces {
        rebuilt.extend_from_slice(piece);
    }
    rebuilt.extend_from_slice(&bytes[last_end..]);
    Some(rebuilt)
}

/// Flip up to five random bits of `bytes`, which lands on length, CRC,
/// table, and header fields often enough since they are ordinary bytes like
/// any other.
fn flip_bits(rng: &mut Lcg, bytes: &mut [u8]) {
    let flips = rng.below(6);
    for _ in 0..flips {
        if bytes.is_empty() {
            break;
        }
        let pos = rng.below(bytes.len());
        let bit = rng.below(8);
        bytes[pos] ^= 1u8 << bit;
    }
}

/// Structurally mutate a pristine PNG: maybe reorder two chunks, then flip
/// a handful of random bits.
fn mutate_png(rng: &mut Lcg, pristine: &[u8]) -> Vec<u8> {
    let mut bytes = pristine.to_vec();
    let bounds = chunk_bounds(&bytes);
    if rng.below(2) == 0 {
        if let Some(rebuilt) = swap_two_ranges(rng, &bytes, &bounds) {
            bytes = rebuilt;
        }
    }
    flip_bits(rng, &mut bytes);
    bytes
}

/// Generous enough that most pristine fixtures decode, tight enough that
/// the limit-refusal paths (dimensions, pixel count, and the progressive
/// coefficient store) are genuinely exercised by mutation: the JPEG
/// generator's largest pristine store is under 4 KiB, while an inflated
/// mutant well inside the 64x64 dimension limit can ask for six times the
/// byte budget below.
fn limits() -> DecodeLimits {
    DecodeLimits::new(64, 64, 64 * 64, 8 * 1024)
}

/// A sequence walk this harness will not run past, so a fixture declaring a
/// great many frames cannot turn one fuzz iteration into a long one. The
/// decoder has its own, far larger containment bound on the count itself.
const SEQUENCE_STEPS: u32 = 64;

/// Assert no decode entry point panics, and that any image or frame they
/// return actually respects the limits it was decoded under.
fn decode_never_panics_and_respects_limits(bytes: &[u8]) {
    let limits = limits();
    let decoded = [
        decode(bytes, &limits),
        // A box smaller than any fixture, so JPEG's reduced-scale (1/2,
        // 1/4, 1/8) inverse-DCT paths are chosen rather than full scale.
        decode_fitted(bytes, &limits, FitBox::new(3, 3)),
        decode_fitted(bytes, &limits, FitBox::new(u32::MAX, u32::MAX)),
    ];
    for image in decoded.into_iter().flatten() {
        assert!(image.width() <= limits.max_width());
        assert!(image.height() <= limits.max_height());
        assert!(u64::from(image.width()) * u64::from(image.height()) <= limits.max_pixels());
        assert_eq!(image.pixels().len(), image.into_pixels().len());
    }
    // The sequence walk is what reaches a multi-frame container's
    // composition and disposal paths at all: `decode` stops at the first
    // frame. A rewind and a second walk cover the restart too.
    if let Ok(sequence) = Sequence::open(bytes, &limits) {
        walk(sequence, &limits);
    }
    // A RISC OS sprite area carries no signature, so the sniffing doors
    // above can never reach its decoder — and, for the same reason, any
    // bytes at all are a candidate sprite area. Naming the format is both
    // the only way in and free extra coverage for every fixture here.
    let _ = probe_as(ImageFormat::Sprite, bytes);
    if let Ok(image) = decode_as(ImageFormat::Sprite, bytes, &limits) {
        assert!(image.width() <= limits.max_width());
        assert!(image.height() <= limits.max_height());
        assert!(u64::from(image.width()) * u64::from(image.height()) <= limits.max_pixels());
    }
    if let Ok(sequence) = Sequence::open_as(ImageFormat::Sprite, bytes, &limits) {
        walk(sequence, &limits);
    }
    let _ = sniff(bytes);
}

/// Walk a sequence twice, checking every frame against the limits it was
/// opened under; the second pass covers the restart.
fn walk(mut sequence: Sequence<'_>, limits: &DecodeLimits) {
    let info = sequence.info();
    for pass in 0..2 {
        let mut steps = 0u32;
        while steps < SEQUENCE_STEPS {
            match sequence.next_frame() {
                Ok(Some(frame)) => {
                    assert!(frame.width() <= limits.max_width());
                    assert!(frame.height() <= limits.max_height());
                    let pixels = u64::from(frame.width()) * u64::from(frame.height());
                    assert!(pixels <= limits.max_pixels());
                    assert_eq!(
                        frame.pixels().len(),
                        usize::try_from(pixels * 4).unwrap_or(usize::MAX)
                    );
                    assert!(frame.index() < info.count());
                    if matches!(info.kind(), SequenceKind::Animation { .. }) {
                        // An animation's frames are one canvas, so each is
                        // the container's own size; a page container's pages
                        // are pictures in their own right and carry their
                        // own, so its geometry is only the largest.
                        assert_eq!(frame.width(), info.width());
                        assert_eq!(frame.height(), info.height());
                    }
                }
                Ok(None) | Err(_) => break,
            }
            steps += 1;
        }
        if pass == 0 {
            sequence.rewind();
        }
    }
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
fn arbitrary_bytes_behind_each_signature_never_panic() {
    // Random bytes essentially never open with a valid signature, so
    // without this the format decoders themselves — the scanline, Huffman,
    // and scan paths — would hardly ever be entered at all.
    let mut rng = Lcg::new(tairix_fuzzseed::start(
        "arbitrary_bytes_behind_each_signature_never_panic",
        tairix_fuzzseed::FUZZ_SEED_ENV,
    ));
    let deadline = tairix_fuzzseed::budget_deadline(tairix_fuzzseed::FUZZ_BUDGET_ENV);
    let mut body = Vec::new();
    let mut buf = Vec::new();
    loop {
        for _ in 0..SMOKE_ITERATIONS {
            body.clear();
            body.resize(rng.below(300), 0);
            rng.fill(&mut body);
            let mut prefixes: Vec<&[u8]> = vec![
                &SIGNATURE[..],
                &[0xFF, SOI][..],
                &GIF_MAGIC[..],
                &BMP_MAGIC[..],
                &ICO_HEADER[..],
                &CUR_HEADER[..],
            ];
            prefixes.extend(TIFF_HEADERS.iter().map(|header| &header[..]));
            prefixes.push(&WEBP_SIGNATURE_PREFIX[..]);
            for prefix in prefixes {
                buf.clear();
                buf.extend_from_slice(prefix);
                buf.extend_from_slice(&body);
                decode_never_panics_and_respects_limits(&buf);
            }
        }
        if !tairix_fuzzseed::within_budget(deadline) {
            break;
        }
    }
}

#[test]
fn mutated_valid_png_fixtures_never_panic() {
    let mut rng = Lcg::new(tairix_fuzzseed::start(
        "mutated_valid_png_fixtures_never_panic",
        tairix_fuzzseed::FUZZ_SEED_ENV,
    ));
    let deadline = tairix_fuzzseed::budget_deadline(tairix_fuzzseed::FUZZ_BUDGET_ENV);
    loop {
        for _ in 0..SMOKE_ITERATIONS {
            let pristine = build_valid_png(&mut rng);
            let mutated = mutate_png(&mut rng, &pristine);
            decode_never_panics_and_respects_limits(&mutated);
        }
        if !tairix_fuzzseed::within_budget(deadline) {
            break;
        }
    }
}

#[test]
fn mutated_valid_jpeg_fixtures_never_panic() {
    let mut rng = Lcg::new(tairix_fuzzseed::start(
        "mutated_valid_jpeg_fixtures_never_panic",
        tairix_fuzzseed::FUZZ_SEED_ENV,
    ));
    let deadline = tairix_fuzzseed::budget_deadline(tairix_fuzzseed::FUZZ_BUDGET_ENV);
    loop {
        for _ in 0..SMOKE_ITERATIONS {
            let pristine = build_valid_jpeg(&mut rng);
            let mutated = mutate_jpeg(&mut rng, &pristine);
            decode_never_panics_and_respects_limits(&mutated);
        }
        if !tairix_fuzzseed::within_budget(deadline) {
            break;
        }
    }
}

#[test]
fn mutated_valid_gif_fixtures_never_panic() {
    let mut rng = Lcg::new(tairix_fuzzseed::start(
        "mutated_valid_gif_fixtures_never_panic",
        tairix_fuzzseed::FUZZ_SEED_ENV,
    ));
    let deadline = tairix_fuzzseed::budget_deadline(tairix_fuzzseed::FUZZ_BUDGET_ENV);
    loop {
        for _ in 0..SMOKE_ITERATIONS {
            let pristine = build_valid_gif(&mut rng);
            let mutated = mutate_gif(&mut rng, &pristine);
            decode_never_panics_and_respects_limits(&mutated);
        }
        if !tairix_fuzzseed::within_budget(deadline) {
            break;
        }
    }
}

#[test]
fn the_gif_generator_produces_a_valid_corpus() {
    const DRAWS: u64 = 500;
    let mut rng = Lcg::new(tairix_fuzzseed::start(
        "the_gif_generator_produces_a_valid_corpus",
        tairix_fuzzseed::FUZZ_SEED_ENV,
    ));
    let limits = limits();
    for _ in 0..DRAWS {
        let gif = build_valid_gif(&mut rng);
        let mut sequence =
            Sequence::open(&gif, &limits).expect("a pristine generated fixture failed to open");
        let count = sequence.info().count();
        let mut seen = 0u32;
        while sequence
            .next_frame()
            .expect("a pristine generated fixture failed to decode a frame")
            .is_some()
        {
            seen += 1;
        }
        assert_eq!(seen, count, "a fixture decoded a different frame count");
    }
}

#[test]
fn the_png_generator_produces_a_valid_corpus() {
    const DRAWS: u64 = 500;
    let mut rng = Lcg::new(tairix_fuzzseed::start(
        "the_png_generator_produces_a_valid_corpus",
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

#[test]
fn the_jpeg_generator_produces_a_valid_corpus() {
    const DRAWS: u64 = 500;
    let mut rng = Lcg::new(tairix_fuzzseed::start(
        "the_jpeg_generator_produces_a_valid_corpus",
        tairix_fuzzseed::FUZZ_SEED_ENV,
    ));
    let limits = limits();
    for _ in 0..DRAWS {
        let jpeg = build_valid_jpeg(&mut rng);
        let image = decode(&jpeg, &limits).expect("a pristine generated fixture failed to decode");
        // Every coefficient is zero, so every pixel is exactly the level
        // shift the inverse DCT adds (ITU-T T.81 §A.3.1) — opaque mid-grey.
        // A fixture that decoded to anything else would be silently
        // exercising the wrong bytes.
        let (pixels, tail) = image.pixels().as_chunks::<4>();
        assert!(tail.is_empty(), "an image's pixels are whole RGBA quads");
        assert!(
            pixels.iter().all(|&px| px == [128, 128, 128, 255]),
            "a pristine fixture decoded to something other than flat mid-grey"
        );
    }
}

#[test]
fn mutated_valid_bmp_fixtures_never_panic() {
    let mut rng = Lcg::new(tairix_fuzzseed::start(
        "mutated_valid_bmp_fixtures_never_panic",
        tairix_fuzzseed::FUZZ_SEED_ENV,
    ));
    let deadline = tairix_fuzzseed::budget_deadline(tairix_fuzzseed::FUZZ_BUDGET_ENV);
    loop {
        for _ in 0..SMOKE_ITERATIONS {
            let pristine = build_valid_bmp(&mut rng);
            let mutated = mutate_bmp(&mut rng, &pristine);
            decode_never_panics_and_respects_limits(&mutated);
        }
        if !tairix_fuzzseed::within_budget(deadline) {
            break;
        }
    }
}

#[test]
fn mutated_valid_ico_fixtures_never_panic() {
    let mut rng = Lcg::new(tairix_fuzzseed::start(
        "mutated_valid_ico_fixtures_never_panic",
        tairix_fuzzseed::FUZZ_SEED_ENV,
    ));
    let deadline = tairix_fuzzseed::budget_deadline(tairix_fuzzseed::FUZZ_BUDGET_ENV);
    loop {
        for _ in 0..SMOKE_ITERATIONS {
            let pristine = build_valid_ico(&mut rng);
            let mutated = mutate_ico(&mut rng, &pristine);
            decode_never_panics_and_respects_limits(&mutated);
        }
        if !tairix_fuzzseed::within_budget(deadline) {
            break;
        }
    }
}

#[test]
fn the_bmp_generator_produces_a_valid_corpus() {
    const DRAWS: u64 = 500;
    let mut rng = Lcg::new(tairix_fuzzseed::start(
        "the_bmp_generator_produces_a_valid_corpus",
        tairix_fuzzseed::FUZZ_SEED_ENV,
    ));
    let limits = limits();
    for _ in 0..DRAWS {
        let bmp = build_valid_bmp(&mut rng);
        assert!(
            decode(&bmp, &limits).is_ok(),
            "a pristine generated fixture failed to decode"
        );
    }
}

#[test]
fn the_ico_generator_produces_a_valid_corpus() {
    const DRAWS: u64 = 500;
    let mut rng = Lcg::new(tairix_fuzzseed::start(
        "the_ico_generator_produces_a_valid_corpus",
        tairix_fuzzseed::FUZZ_SEED_ENV,
    ));
    let limits = limits();
    for _ in 0..DRAWS {
        let ico = build_valid_ico(&mut rng);
        let mut sequence =
            Sequence::open(&ico, &limits).expect("a pristine generated fixture failed to open");
        let count = sequence.info().count();
        let mut seen = 0u32;
        while sequence
            .next_frame()
            .expect("a pristine generated fixture failed to decode a page")
            .is_some()
        {
            seen += 1;
        }
        assert_eq!(seen, count, "a fixture decoded a different page count");
    }
}

#[test]
fn mutated_valid_sprite_fixtures_never_panic() {
    let mut rng = Lcg::new(tairix_fuzzseed::start(
        "mutated_valid_sprite_fixtures_never_panic",
        tairix_fuzzseed::FUZZ_SEED_ENV,
    ));
    let deadline = tairix_fuzzseed::budget_deadline(tairix_fuzzseed::FUZZ_BUDGET_ENV);
    loop {
        for _ in 0..SMOKE_ITERATIONS {
            let pristine = build_valid_sprite_area(&mut rng);
            let mutated = mutate_sprite(&mut rng, &pristine);
            decode_never_panics_and_respects_limits(&mutated);
        }
        if !tairix_fuzzseed::within_budget(deadline) {
            break;
        }
    }
}

#[test]
fn the_sprite_generator_produces_a_valid_corpus() {
    const DRAWS: u64 = 500;
    let mut rng = Lcg::new(tairix_fuzzseed::start(
        "the_sprite_generator_produces_a_valid_corpus",
        tairix_fuzzseed::FUZZ_SEED_ENV,
    ));
    let limits = limits();
    for _ in 0..DRAWS {
        let bytes = build_valid_sprite_area(&mut rng);
        let mut sequence = Sequence::open_as(ImageFormat::Sprite, &bytes, &limits)
            .expect("a pristine generated fixture failed to open");
        let count = sequence.info().count();
        let mut seen = 0u32;
        while sequence
            .next_frame()
            .expect("a pristine generated fixture failed to decode a sprite")
            .is_some()
        {
            seen += 1;
        }
        assert_eq!(seen, count, "a fixture decoded a different sprite count");
    }
}

#[test]
fn mutated_valid_tiff_fixtures_never_panic() {
    let mut rng = Lcg::new(tairix_fuzzseed::start(
        "mutated_valid_tiff_fixtures_never_panic",
        tairix_fuzzseed::FUZZ_SEED_ENV,
    ));
    let deadline = tairix_fuzzseed::budget_deadline(tairix_fuzzseed::FUZZ_BUDGET_ENV);
    loop {
        for _ in 0..SMOKE_ITERATIONS {
            let pristine = build_valid_tiff(&mut rng);
            let mutated = mutate_tiff(&mut rng, &pristine);
            decode_never_panics_and_respects_limits(&mutated);
        }
        if !tairix_fuzzseed::within_budget(deadline) {
            break;
        }
    }
}

#[test]
fn the_tiff_generator_produces_a_valid_corpus() {
    const DRAWS: u64 = 500;
    let mut rng = Lcg::new(tairix_fuzzseed::start(
        "the_tiff_generator_produces_a_valid_corpus",
        tairix_fuzzseed::FUZZ_SEED_ENV,
    ));
    let limits = DecodeLimits::new(256, 256, 256 * 256, 1 << 16);
    for _ in 0..DRAWS {
        let bytes = build_valid_tiff(&mut rng);
        let mut sequence =
            Sequence::open(&bytes, &limits).expect("a pristine generated fixture failed to open");
        let count = sequence.info().count();
        let mut seen = 0u32;
        while sequence
            .next_frame()
            .expect("a pristine generated fixture failed to decode a page")
            .is_some()
        {
            seen += 1;
        }
        assert_eq!(seen, count, "a fixture decoded a different page count");
    }
}

/// A `RIFF` header whose declared region covers the rest of a 300-byte body,
/// so random bytes behind it reach the container's chunk walk rather than
/// being refused for a region that is not there.
const WEBP_SIGNATURE_PREFIX: [u8; 12] = [
    b'R', b'I', b'F', b'F', 0x2C, 0x01, 0, 0, b'W', b'E', b'B', b'P',
];

#[test]
fn mutated_valid_webp_fixtures_never_panic() {
    let mut rng = Lcg::new(tairix_fuzzseed::start(
        "mutated_valid_webp_fixtures_never_panic",
        tairix_fuzzseed::FUZZ_SEED_ENV,
    ));
    let deadline = tairix_fuzzseed::budget_deadline(tairix_fuzzseed::FUZZ_BUDGET_ENV);
    loop {
        for _ in 0..SMOKE_ITERATIONS {
            let pristine = if rng.below(2) == 0 {
                build_valid_webp(&mut rng)
            } else {
                build_lossy_webp(&mut rng)
            };
            let mutated = mutate_webp(&mut rng, &pristine);
            decode_never_panics_and_respects_limits(&mutated);
        }
        if !tairix_fuzzseed::within_budget(deadline) {
            break;
        }
    }
}

#[test]
fn the_webp_generator_produces_a_valid_corpus() {
    const DRAWS: u64 = 500;
    let mut rng = Lcg::new(tairix_fuzzseed::start(
        "the_webp_generator_produces_a_valid_corpus",
        tairix_fuzzseed::FUZZ_SEED_ENV,
    ));
    let limits = DecodeLimits::new(256, 256, 256 * 256, 1 << 16);
    for _ in 0..DRAWS {
        let bytes = build_valid_webp(&mut rng);
        assert_eq!(sniff(&bytes), Some(ImageFormat::Webp));
        let mut sequence =
            Sequence::open(&bytes, &limits).expect("a pristine generated fixture failed to open");
        let count = sequence.info().count();
        let mut seen = 0u32;
        while sequence
            .next_frame()
            .expect("a pristine generated fixture failed to decode a frame")
            .is_some()
        {
            seen += 1;
        }
        assert_eq!(seen, count, "a fixture decoded a different frame count");
    }
}
