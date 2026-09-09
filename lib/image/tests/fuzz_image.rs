//! Deterministic fuzz harness for every image decoder (PNG, JPEG, and GIF).
//!
//! Invariants, for any bytes an untrusted bundle icon, wallpaper, or opened
//! picture may carry:
//!
//! 1. [`decode`], [`decode_fitted`], and a full walk of [`Sequence`] never
//!    panic for any input, and never report a frame whose width, height, or
//!    pixel count exceeds the [`DecodeLimits`] they were given.
//! 2. Structure-aware mutations of a valid, builder-made file — bit flips,
//!    length/CRC tweaks, and reordering of PNG chunks, JPEG marker segments,
//!    or GIF blocks — never panic: the mutated bytes either decode within
//!    the limits or are refused with a typed error.
//! 3. The generators are not degenerate: every pristine fixture each one
//!    produces actually decodes (a corpus that never round-trips would
//!    leave invariant 2 exercising only the trivial "refused immediately"
//!    path).
//! 4. Garbage carrying a valid signature reaches the format decoder rather
//!    than stopping at the sniffer, so the entropy/scanline paths are fuzzed
//!    and not just the format dispatch.
//!
//! Every generator, and its chunk/zlib, marker/Huffman, and block/LZW
//! framing helpers, are deliberately self-contained: this harness only calls
//! `tairix_image`'s public API (exactly what a real consumer — the desktop
//! image sandbox — would do), never the crate's own chunk reader, CRC
//! table, Huffman builder, or code-stream writer, so a bug in any of those is
//! still caught here.

use tairix_fuzzseed::Lcg;
use tairix_image::{decode, decode_fitted, sniff, DecodeLimits, FitBox, Sequence};

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
/// swap. Shared by both formats' mutators: a PNG chunk and a JPEG marker
/// segment are both "a framed range the decoder must re-walk".
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
    if let Ok(mut sequence) = Sequence::open(bytes, &limits) {
        let info = sequence.info();
        assert!(info.width() <= limits.max_width());
        assert!(info.height() <= limits.max_height());
        for pass in 0..2 {
            let mut steps = 0u32;
            while steps < SEQUENCE_STEPS {
                match sequence.next_frame() {
                    Ok(Some(frame)) => {
                        assert_eq!(frame.width(), info.width());
                        assert_eq!(frame.height(), info.height());
                        assert_eq!(
                            frame.pixels().len(),
                            usize::try_from(u64::from(info.width()) * u64::from(info.height()) * 4)
                                .unwrap_or(usize::MAX)
                        );
                        assert!(frame.index() < info.count());
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
            for prefix in [&SIGNATURE[..], &[0xFF, SOI][..], &GIF_MAGIC[..]] {
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
