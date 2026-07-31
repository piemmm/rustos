//! Unit tests for the PNG decoder.
//!
//! Fixtures are built by hand through the small helpers below: `chunk`
//! frames one chunk (length, type, payload, real CRC-32), and `zlib_wrap`
//! packs raw (pre-filter) scanline bytes into a genuine zlib stream made of
//! STORED deflate blocks plus a real Adler-32 trailer — no compressor is
//! needed to produce a stream our own `tairix_compress::zlib` decoder
//! accepts. One fixture additionally hand-assembles a real fixed-Huffman
//! deflate bit stream, proving the inflate path end to end through PNG.

use alloc::vec;
use alloc::vec::Vec;

use super::{decode, ColourType, IDAT, IEND, IHDR, PLTE, SIGNATURE, TRNS};
use crate::{sniff, DecodeError, DecodeLimits, ImageFormat};

/// Generous limits for every fixture in this file (none exercises the
/// limit-refusal paths, which are tested against a deliberately tight
/// [`DecodeLimits`] of their own).
const ROOMY: DecodeLimits = DecodeLimits::new(1024, 1024, 1_000_000);

fn chunk(chunk_type: [u8; 4], payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let len = u32::try_from(payload.len()).expect("test payload fits a u32 length");
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(&chunk_type);
    out.extend_from_slice(payload);
    let crc = crate::crc32::crc32_of(&[&chunk_type, payload]);
    out.extend_from_slice(&crc.to_be_bytes());
    out
}

fn ihdr_payload(width: u32, height: u32, bit_depth: u8, colour_type: u8, interlace: u8) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&width.to_be_bytes());
    out.extend_from_slice(&height.to_be_bytes());
    out.extend_from_slice(&[bit_depth, colour_type, 0, 0, interlace]);
    out
}

/// Wrap `data` (raw, pre-filter-reconstruction scanline bytes) in a
/// well-formed zlib stream built entirely from STORED deflate blocks, so no
/// compressor is needed to produce a stream the crate's own zlib decoder
/// accepts.
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
            let len = u16::try_from(take).expect("block fits a u16 length");
            out.extend_from_slice(&len.to_le_bytes());
            out.extend_from_slice(&(!len).to_le_bytes());
            out.extend_from_slice(block);
            remaining = rest;
        }
    }
    out.extend_from_slice(&tairix_compress::zlib::adler32(data).to_be_bytes());
    out
}

/// Assemble a minimal, well-formed PNG: signature, `IHDR`, an optional
/// `PLTE`/`tRNS`, one `IDAT` wrapping `raw_scanlines` (STORED-block zlib),
/// and `IEND`.
#[allow(clippy::too_many_arguments)]
fn build_png(
    width: u32,
    height: u32,
    bit_depth: u8,
    colour_type: u8,
    interlace: u8,
    palette: Option<&[u8]>,
    trns: Option<&[u8]>,
    raw_scanlines: &[u8],
) -> Vec<u8> {
    let mut out = SIGNATURE.to_vec();
    out.extend(chunk(
        IHDR,
        &ihdr_payload(width, height, bit_depth, colour_type, interlace),
    ));
    if let Some(plte) = palette {
        out.extend(chunk(PLTE, plte));
    }
    if let Some(trns) = trns {
        out.extend(chunk(TRNS, trns));
    }
    out.extend(chunk(IDAT, &zlib_wrap(raw_scanlines)));
    out.extend(chunk(IEND, &[]));
    out
}

fn rgba(pixels: &[u8], width: u32, x: u32, y: u32) -> [u8; 4] {
    let idx = ((y * width + x) * 4) as usize;
    [
        pixels[idx],
        pixels[idx + 1],
        pixels[idx + 2],
        pixels[idx + 3],
    ]
}

// ---- colour types and bit depths ----------------------------------------

#[test]
fn greyscale_depth8_decodes() {
    let raw = [0, 0x10, 0x20, 0, 0x30, 0x40];
    let png = build_png(2, 2, 8, 0, 0, None, None, &raw);
    let image = decode(&png, &ROOMY).expect("decodes");
    assert_eq!((image.width(), image.height()), (2, 2));
    let p = image.pixels();
    assert_eq!(rgba(p, 2, 0, 0), [0x10, 0x10, 0x10, 255]);
    assert_eq!(rgba(p, 2, 1, 0), [0x20, 0x20, 0x20, 255]);
    assert_eq!(rgba(p, 2, 0, 1), [0x30, 0x30, 0x30, 255]);
    assert_eq!(rgba(p, 2, 1, 1), [0x40, 0x40, 0x40, 255]);
}

#[test]
fn greyscale_depth1_unpacks_msb_first_and_scales() {
    // Four 1-bit samples packed MSB-first into one byte: 1,0,1,1.
    let raw = [0, 0b1011_0000];
    let png = build_png(4, 1, 1, 0, 0, None, None, &raw);
    let image = decode(&png, &ROOMY).expect("decodes");
    let p = image.pixels();
    assert_eq!(rgba(p, 4, 0, 0), [255, 255, 255, 255]);
    assert_eq!(rgba(p, 4, 1, 0), [0, 0, 0, 255]);
    assert_eq!(rgba(p, 4, 2, 0), [255, 255, 255, 255]);
    assert_eq!(rgba(p, 4, 3, 0), [255, 255, 255, 255]);
}

#[test]
fn greyscale_depth2_unpacks_msb_first_and_scales() {
    // Four 2-bit samples: 0,1,2,3 (max 3, scaled by 255/3 = 85 per step).
    let raw = [0, 0b00_01_10_11];
    let png = build_png(4, 1, 2, 0, 0, None, None, &raw);
    let image = decode(&png, &ROOMY).expect("decodes");
    let p = image.pixels();
    assert_eq!(rgba(p, 4, 0, 0), [0, 0, 0, 255]);
    assert_eq!(rgba(p, 4, 1, 0), [85, 85, 85, 255]);
    assert_eq!(rgba(p, 4, 2, 0), [170, 170, 170, 255]);
    assert_eq!(rgba(p, 4, 3, 0), [255, 255, 255, 255]);
}

#[test]
fn greyscale_depth4_unpacks_msb_first_and_scales() {
    // Two 4-bit samples: 0 and 15 (max), one byte.
    let raw = [0, 0b0000_1111];
    let png = build_png(2, 1, 4, 0, 0, None, None, &raw);
    let image = decode(&png, &ROOMY).expect("decodes");
    let p = image.pixels();
    assert_eq!(rgba(p, 2, 0, 0), [0, 0, 0, 255]);
    assert_eq!(rgba(p, 2, 1, 0), [255, 255, 255, 255]);
}

#[test]
fn greyscale_depth16_scales_by_taking_the_high_byte() {
    let raw = [0, 0x12, 0x34];
    let png = build_png(1, 1, 16, 0, 0, None, None, &raw);
    let image = decode(&png, &ROOMY).expect("decodes");
    assert_eq!(rgba(image.pixels(), 1, 0, 0), [0x12, 0x12, 0x12, 255]);
}

#[test]
fn truecolour_depth8_decodes() {
    let raw = [0, 10, 20, 30, 40, 50, 60];
    let png = build_png(2, 1, 8, 2, 0, None, None, &raw);
    let image = decode(&png, &ROOMY).expect("decodes");
    let p = image.pixels();
    assert_eq!(rgba(p, 2, 0, 0), [10, 20, 30, 255]);
    assert_eq!(rgba(p, 2, 1, 0), [40, 50, 60, 255]);
}

#[test]
fn truecolour_depth16_scales_by_taking_the_high_byte() {
    let raw = [0, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66];
    let png = build_png(1, 1, 16, 2, 0, None, None, &raw);
    let image = decode(&png, &ROOMY).expect("decodes");
    assert_eq!(rgba(image.pixels(), 1, 0, 0), [0x11, 0x33, 0x55, 255]);
}

#[test]
fn indexed_depth8_with_partial_trns_defaults_missing_tail_to_opaque() {
    let palette = [255, 0, 0, 0, 255, 0]; // red, green
    let trns = [128]; // alpha for index 0 only; index 1 defaults opaque
    let raw = [0, 0, 1];
    let png = build_png(2, 1, 8, 3, 0, Some(&palette), Some(&trns), &raw);
    let image = decode(&png, &ROOMY).expect("decodes");
    let p = image.pixels();
    assert_eq!(rgba(p, 2, 0, 0), [255, 0, 0, 128]);
    assert_eq!(rgba(p, 2, 1, 0), [0, 255, 0, 255]);
}

#[test]
fn indexed_depth1_unpacks_msb_first() {
    let palette = [10, 20, 30, 40, 50, 60];
    let raw = [0, 0b0101_0000]; // indices 0,1,0,1
    let png = build_png(4, 1, 1, 3, 0, Some(&palette), None, &raw);
    let image = decode(&png, &ROOMY).expect("decodes");
    let p = image.pixels();
    assert_eq!(rgba(p, 4, 0, 0), [10, 20, 30, 255]);
    assert_eq!(rgba(p, 4, 1, 0), [40, 50, 60, 255]);
    assert_eq!(rgba(p, 4, 2, 0), [10, 20, 30, 255]);
    assert_eq!(rgba(p, 4, 3, 0), [40, 50, 60, 255]);
}

#[test]
fn indexed_depth2_unpacks_msb_first() {
    let palette = [1, 1, 1, 2, 2, 2, 3, 3, 3, 4, 4, 4];
    let raw = [0, 0b00_01_10_11]; // indices 0,1,2,3
    let png = build_png(4, 1, 2, 3, 0, Some(&palette), None, &raw);
    let image = decode(&png, &ROOMY).expect("decodes");
    let p = image.pixels();
    assert_eq!(rgba(p, 4, 0, 0), [1, 1, 1, 255]);
    assert_eq!(rgba(p, 4, 1, 0), [2, 2, 2, 255]);
    assert_eq!(rgba(p, 4, 2, 0), [3, 3, 3, 255]);
    assert_eq!(rgba(p, 4, 3, 0), [4, 4, 4, 255]);
}

#[test]
fn indexed_depth4_unpacks_msb_first() {
    let palette: Vec<u8> = (0..16u8).flat_map(|i| [i, i, i]).collect();
    let raw = [0, 0b0101_1010]; // indices 5, 10
    let png = build_png(2, 1, 4, 3, 0, Some(&palette), None, &raw);
    let image = decode(&png, &ROOMY).expect("decodes");
    let p = image.pixels();
    assert_eq!(rgba(p, 2, 0, 0), [5, 5, 5, 255]);
    assert_eq!(rgba(p, 2, 1, 0), [10, 10, 10, 255]);
}

#[test]
fn grey_alpha_depth8_decodes() {
    let raw = [0, 100, 50, 200, 150];
    let png = build_png(2, 1, 8, 4, 0, None, None, &raw);
    let image = decode(&png, &ROOMY).expect("decodes");
    let p = image.pixels();
    assert_eq!(rgba(p, 2, 0, 0), [100, 100, 100, 50]);
    assert_eq!(rgba(p, 2, 1, 0), [200, 200, 200, 150]);
}

#[test]
fn rgba_depth8_decodes() {
    let raw = [0, 10, 20, 30, 40];
    let png = build_png(1, 1, 8, 6, 0, None, None, &raw);
    let image = decode(&png, &ROOMY).expect("decodes");
    assert_eq!(rgba(image.pixels(), 1, 0, 0), [10, 20, 30, 40]);
}

// ---- colour-key transparency ---------------------------------------------

#[test]
fn greyscale_colour_key_trns_at_depth8() {
    let trns = [0, 50]; // key = 50, at native (8-bit) depth
    let raw = [0, 50, 60];
    let png = build_png(2, 1, 8, 0, 0, None, Some(&trns), &raw);
    let image = decode(&png, &ROOMY).expect("decodes");
    let p = image.pixels();
    assert_eq!(rgba(p, 2, 0, 0), [50, 50, 50, 0]);
    assert_eq!(rgba(p, 2, 1, 0), [60, 60, 60, 255]);
}

#[test]
fn greyscale_colour_key_trns_at_depth16_compares_at_source_depth() {
    let trns = [0x00, 0xAA]; // key = 0x00AA, compared before 16->8 scaling
    let raw = [0, 0x00, 0xAA, 0x00, 0xBB];
    let png = build_png(2, 1, 16, 0, 0, None, Some(&trns), &raw);
    let image = decode(&png, &ROOMY).expect("decodes");
    let p = image.pixels();
    assert_eq!(rgba(p, 2, 0, 0), [0, 0, 0, 0]);
    assert_eq!(rgba(p, 2, 1, 0), [0, 0, 0, 255]);
}

#[test]
fn truecolour_colour_key_trns_at_depth8() {
    let trns = [0, 10, 0, 20, 0, 30]; // key = (10, 20, 30)
    let raw = [0, 10, 20, 30, 11, 20, 30];
    let png = build_png(2, 1, 8, 2, 0, None, Some(&trns), &raw);
    let image = decode(&png, &ROOMY).expect("decodes");
    let p = image.pixels();
    assert_eq!(rgba(p, 2, 0, 0), [10, 20, 30, 0]);
    assert_eq!(rgba(p, 2, 1, 0), [11, 20, 30, 255]);
}

// ---- filters --------------------------------------------------------------

#[test]
fn all_five_filter_types_reconstruct_correctly() {
    // A 3x5 greyscale image where every row after the first exercises a
    // different filter, each hand-computed against the *reconstructed*
    // previous row (never the filtered bytes), per the PNG filter spec.
    let raw = [
        0, 10, 20, 30, // row 0: None -> [10, 20, 30]
        1, 15, 10, 251, // row 1: Sub -> [15, 25, 20]
        2, 2, 5, 5, // row 2: Up -> [17, 30, 25]
        3, 12, 3, 14, // row 3: Average -> [20, 28, 40]
        4, 255, 3, 1, // row 4: Paeth -> [19, 31, 41]
    ];
    let png = build_png(3, 5, 8, 0, 0, None, None, &raw);
    let image = decode(&png, &ROOMY).expect("decodes");
    let p = image.pixels();
    let expect_row = |row: u32, values: [u8; 3]| {
        for (x, &v) in values.iter().enumerate() {
            assert_eq!(
                rgba(p, 3, u32::try_from(x).expect("small index"), row),
                [v, v, v, 255],
                "row {row} col {x}"
            );
        }
    };
    expect_row(0, [10, 20, 30]);
    expect_row(1, [15, 25, 20]);
    expect_row(2, [17, 30, 25]);
    expect_row(3, [20, 28, 40]);
    expect_row(4, [19, 31, 41]);
}

#[test]
fn unknown_filter_byte_is_refused() {
    let raw = [5, 0, 0];
    let png = build_png(2, 1, 8, 0, 0, None, None, &raw);
    assert_eq!(decode(&png, &ROOMY), Err(DecodeError::InvalidFilterType));
}

// ---- interlacing (Adam7) --------------------------------------------------

/// Re-derive Adam7 pass geometry independently of the decoder under test,
/// purely from the public algorithm (W3C PNG §"Interlaced data order"), so
/// this fixture builder cannot share a bug with the code it exercises.
const TEST_ADAM7: [(u32, u32, u32, u32); 7] = [
    (0, 0, 8, 8),
    (0, 4, 8, 8),
    (4, 0, 8, 4),
    (0, 2, 4, 4),
    (2, 0, 4, 2),
    (0, 1, 2, 2),
    (1, 0, 2, 1),
];

fn test_pass_extent(total: u32, start: u32, step: u32) -> u32 {
    if start >= total {
        0
    } else {
        (total - start).div_ceil(step)
    }
}

/// Build an interlaced greyscale-depth8 fixture of `width` x `height` where
/// pixel `(x, y)` carries the value `value(x, y)`, one filter-`None`
/// scanline per pass row.
fn build_interlaced(width: u32, height: u32, value: impl Fn(u32, u32) -> u8) -> Vec<u8> {
    let mut raw = Vec::new();
    for &(row_start, col_start, row_step, col_step) in &TEST_ADAM7 {
        let pass_width = test_pass_extent(width, col_start, col_step);
        let pass_height = test_pass_extent(height, row_start, row_step);
        // A pass with either dimension zero contributes no scanlines at
        // all -- not even a bare filter byte.
        if pass_width == 0 || pass_height == 0 {
            continue;
        }
        for py in 0..pass_height {
            raw.push(0); // filter type None
            for px in 0..pass_width {
                let x = col_start + px * col_step;
                let y = row_start + py * row_step;
                raw.push(value(x, y));
            }
        }
    }
    build_png(width, height, 8, 0, 1, None, None, &raw)
}

fn assert_interlaced_matches(width: u32, height: u32, value: impl Fn(u32, u32) -> u8) {
    let png = build_interlaced(width, height, &value);
    let image = decode(&png, &ROOMY).expect("decodes");
    assert_eq!((image.width(), image.height()), (width, height));
    let p = image.pixels();
    for y in 0..height {
        for x in 0..width {
            let v = value(x, y);
            assert_eq!(rgba(p, width, x, y), [v, v, v, 255], "pixel ({x}, {y})");
        }
    }
}

#[test]
fn adam7_reconstructs_an_eight_by_eight_image() {
    assert_interlaced_matches(8, 8, |x, y| u8::try_from(y * 8 + x).unwrap_or(0));
}

#[test]
fn adam7_reconstructs_a_single_pixel() {
    // Every pass but the first is empty for a 1x1 image.
    assert_interlaced_matches(1, 1, |_, _| 0x42);
}

#[test]
fn adam7_reconstructs_a_five_by_five_image_with_partial_passes() {
    assert_interlaced_matches(5, 5, |x, y| u8::try_from(y * 5 + x).unwrap_or(0));
}

// ---- a real fixed-Huffman deflate stream, end to end ----------------------

/// A minimal test-only bit writer mirroring `lib/compress`'s inflate
/// bit order (LSB-first for plain fields, MSB-first for Huffman codes),
/// used once here to hand-assemble a genuine fixed-Huffman deflate block —
/// proving the PNG decoder's zlib/inflate path end to end, not only its
/// STORED-block fast path.
struct BitWriter {
    bytes: Vec<u8>,
    cur: u8,
    nbits: u32,
}

impl BitWriter {
    fn new() -> Self {
        Self {
            bytes: Vec::new(),
            cur: 0,
            nbits: 0,
        }
    }

    fn put_bit(&mut self, bit: u32) {
        self.cur |= u8::try_from(bit & 1).unwrap_or(0) << self.nbits;
        self.nbits += 1;
        if self.nbits == 8 {
            self.bytes.push(self.cur);
            self.cur = 0;
            self.nbits = 0;
        }
    }

    fn put_bits(&mut self, value: u32, n: u32) {
        for i in 0..n {
            self.put_bit((value >> i) & 1);
        }
    }

    fn put_code(&mut self, code: u32, n: u32) {
        for i in (0..n).rev() {
            self.put_bit((code >> i) & 1);
        }
    }

    fn finish(mut self) -> Vec<u8> {
        while self.nbits != 0 {
            self.put_bit(0);
        }
        self.bytes
    }
}

fn fixed_litlen_code(symbol: u16) -> (u32, u32) {
    let v = u32::from(symbol);
    if v <= 143 {
        (0x30 + v, 8)
    } else if v <= 255 {
        (0x190 + (v - 144), 9)
    } else if v <= 279 {
        (v - 256, 7)
    } else {
        (0xC0 + (v - 280), 8)
    }
}

fn deflate_fixed_literals(data: &[u8]) -> Vec<u8> {
    let mut bw = BitWriter::new();
    bw.put_bit(1); // BFINAL
    bw.put_bits(1, 2); // BTYPE = 01 (fixed Huffman)
    for &byte in data {
        let (code, len) = fixed_litlen_code(u16::from(byte));
        bw.put_code(code, len);
    }
    let (code, len) = fixed_litlen_code(256); // end of block
    bw.put_code(code, len);
    bw.finish()
}

#[test]
fn a_real_fixed_huffman_idat_stream_decodes() {
    let raw = [0u8, 0xAA, 0xBB, 0, 0xCC, 0xDD];
    let mut idat = vec![0x78u8, 0x9C];
    idat.extend(deflate_fixed_literals(&raw));
    idat.extend_from_slice(&tairix_compress::zlib::adler32(&raw).to_be_bytes());

    let mut png = SIGNATURE.to_vec();
    png.extend(chunk(IHDR, &ihdr_payload(2, 2, 8, 0, 0)));
    png.extend(chunk(IDAT, &idat));
    png.extend(chunk(IEND, &[]));

    let image = decode(&png, &ROOMY).expect("decodes through a real deflate stream");
    let p = image.pixels();
    assert_eq!(rgba(p, 2, 0, 0), [0xAA, 0xAA, 0xAA, 255]);
    assert_eq!(rgba(p, 2, 1, 0), [0xBB, 0xBB, 0xBB, 255]);
    assert_eq!(rgba(p, 2, 0, 1), [0xCC, 0xCC, 0xCC, 255]);
    assert_eq!(rgba(p, 2, 1, 1), [0xDD, 0xDD, 0xDD, 255]);
}

// ---- refusals ---------------------------------------------------------

#[test]
fn bad_signature_is_refused() {
    let mut bytes = SIGNATURE;
    bytes[0] = 0;
    assert_eq!(decode(&bytes, &ROOMY), Err(DecodeError::BadSignature));
    assert_eq!(sniff(&bytes), None);
}

#[test]
fn bad_crc_is_refused_for_every_chunk_kind() {
    let flip_last_crc_byte = |mut bytes: Vec<u8>| {
        let len = bytes.len();
        bytes[len - 1] ^= 0xFF;
        bytes
    };

    // IHDR
    let mut png = SIGNATURE.to_vec();
    let bad_ihdr = flip_last_crc_byte(chunk(IHDR, &ihdr_payload(1, 1, 8, 0, 0)));
    png.extend(bad_ihdr);
    assert_eq!(decode(&png, &ROOMY), Err(DecodeError::ChunkCrcMismatch));

    // PLTE
    let mut png = SIGNATURE.to_vec();
    png.extend(chunk(IHDR, &ihdr_payload(1, 1, 8, 3, 0)));
    png.extend(flip_last_crc_byte(chunk(PLTE, &[1, 2, 3])));
    assert_eq!(decode(&png, &ROOMY), Err(DecodeError::ChunkCrcMismatch));

    // IDAT
    let mut png = SIGNATURE.to_vec();
    png.extend(chunk(IHDR, &ihdr_payload(1, 1, 8, 0, 0)));
    png.extend(flip_last_crc_byte(chunk(IDAT, &zlib_wrap(&[0, 1]))));
    assert_eq!(decode(&png, &ROOMY), Err(DecodeError::ChunkCrcMismatch));

    // IEND
    let mut png = SIGNATURE.to_vec();
    png.extend(chunk(IHDR, &ihdr_payload(1, 1, 8, 0, 0)));
    png.extend(chunk(IDAT, &zlib_wrap(&[0, 1])));
    png.extend(flip_last_crc_byte(chunk(IEND, &[])));
    assert_eq!(decode(&png, &ROOMY), Err(DecodeError::ChunkCrcMismatch));
}

#[test]
fn ihdr_rejects_invalid_bit_depth() {
    let png = build_png(1, 1, 3, 0, 0, None, None, &[0, 0]);
    assert_eq!(decode(&png, &ROOMY), Err(DecodeError::InvalidBitDepth));
}

#[test]
fn ihdr_rejects_invalid_colour_type() {
    let png = build_png(1, 1, 8, 1, 0, None, None, &[0, 0]);
    assert_eq!(decode(&png, &ROOMY), Err(DecodeError::InvalidColourType));
}

#[test]
fn ihdr_rejects_unsupported_colour_type_and_depth_combination() {
    // Truecolour (2) never permits a 1-bit depth.
    let png = build_png(1, 1, 1, 2, 0, None, None, &[0, 0]);
    assert_eq!(
        decode(&png, &ROOMY),
        Err(DecodeError::UnsupportedColourTypeAndDepth)
    );
}

#[test]
fn ihdr_rejects_bad_compression_method() {
    let mut png = SIGNATURE.to_vec();
    let mut ihdr = ihdr_payload(1, 1, 8, 0, 0);
    ihdr[10] = 1;
    png.extend(chunk(IHDR, &ihdr));
    png.extend(chunk(IDAT, &zlib_wrap(&[0, 0])));
    png.extend(chunk(IEND, &[]));
    assert_eq!(
        decode(&png, &ROOMY),
        Err(DecodeError::InvalidCompressionMethod)
    );
}

#[test]
fn ihdr_rejects_bad_filter_method() {
    let mut png = SIGNATURE.to_vec();
    let mut ihdr = ihdr_payload(1, 1, 8, 0, 0);
    ihdr[11] = 1;
    png.extend(chunk(IHDR, &ihdr));
    png.extend(chunk(IDAT, &zlib_wrap(&[0, 0])));
    png.extend(chunk(IEND, &[]));
    assert_eq!(decode(&png, &ROOMY), Err(DecodeError::InvalidFilterMethod));
}

#[test]
fn ihdr_rejects_bad_interlace_method() {
    let png = build_png(1, 1, 8, 0, 2, None, None, &[0, 0]);
    assert_eq!(
        decode(&png, &ROOMY),
        Err(DecodeError::InvalidInterlaceMethod)
    );
}

#[test]
fn ihdr_rejects_zero_dimensions() {
    assert_eq!(
        decode(&build_png(0, 1, 8, 0, 0, None, None, &[]), &ROOMY),
        Err(DecodeError::ZeroDimension)
    );
    assert_eq!(
        decode(&build_png(1, 0, 8, 0, 0, None, None, &[]), &ROOMY),
        Err(DecodeError::ZeroDimension)
    );
}

#[test]
fn width_and_pixel_limits_are_enforced_at_the_exact_boundary() {
    let tight = DecodeLimits::new(4, 4, 16);
    let ok = build_png(4, 4, 8, 0, 0, None, None, &[0u8; 4 * 5]);
    assert!(decode(&ok, &tight).is_ok());

    let too_wide = build_png(5, 1, 8, 0, 0, None, None, &[0u8; 6]);
    assert_eq!(
        decode(&too_wide, &tight),
        Err(DecodeError::WidthExceedsLimit)
    );

    let too_tall = build_png(1, 5, 8, 0, 0, None, None, &[0u8; 10]);
    assert_eq!(
        decode(&too_tall, &tight),
        Err(DecodeError::HeightExceedsLimit)
    );

    // 4x5 = 20 pixels > the limit of 16, even though each dimension alone
    // is within its own per-axis limit.
    let too_many_pixels = build_png(4, 4, 8, 0, 0, None, None, &[0u8; 4 * 5]);
    let one_pixel_over = DecodeLimits::new(4, 4, 15);
    assert_eq!(
        decode(&too_many_pixels, &one_pixel_over),
        Err(DecodeError::PixelCountExceedsLimit)
    );
}

#[test]
fn missing_palette_for_indexed_colour_is_refused() {
    let png = build_png(1, 1, 8, 3, 0, None, None, &[0, 0]);
    assert_eq!(decode(&png, &ROOMY), Err(DecodeError::PaletteRequired));
}

#[test]
fn palette_forbidden_for_grey_and_grey_alpha() {
    let mut png = SIGNATURE.to_vec();
    png.extend(chunk(IHDR, &ihdr_payload(1, 1, 8, 0, 0)));
    png.extend(chunk(PLTE, &[1, 2, 3]));
    png.extend(chunk(IDAT, &zlib_wrap(&[0, 0])));
    png.extend(chunk(IEND, &[]));
    assert_eq!(decode(&png, &ROOMY), Err(DecodeError::PaletteForbidden));
}

#[test]
fn palette_index_out_of_range_is_refused() {
    let palette = [1, 2, 3]; // one entry only
    let raw = [0, 5]; // index 5: out of range
    let png = build_png(1, 1, 8, 3, 0, Some(&palette), None, &raw);
    assert_eq!(
        decode(&png, &ROOMY),
        Err(DecodeError::PaletteIndexOutOfRange)
    );
}

#[test]
fn truncated_idat_is_refused() {
    let mut png = SIGNATURE.to_vec();
    png.extend(chunk(IHDR, &ihdr_payload(2, 2, 8, 0, 0)));
    let full = zlib_wrap(&[0, 1, 2, 0, 3, 4]);
    png.extend(chunk(IDAT, &full[..full.len() - 3]));
    png.extend(chunk(IEND, &[]));
    assert!(matches!(
        decode(&png, &ROOMY),
        Err(DecodeError::CompressedData(_) | DecodeError::CompressedSizeMismatch)
    ));
}

#[test]
fn undersized_decompressed_stream_is_refused() {
    // Declares a 2x2 image (needs 6 raw bytes) but the IDAT only supplies
    // a single one-byte row.
    let png = build_png(2, 2, 8, 0, 0, None, None, &[0, 1, 2]);
    assert_eq!(
        decode(&png, &ROOMY),
        Err(DecodeError::CompressedSizeMismatch)
    );
}

#[test]
fn oversized_decompressed_stream_is_refused() {
    // Declares a 1x1 image (needs 2 raw bytes) but the IDAT supplies two
    // full rows: the fixed-size output buffer refuses the overflow the
    // moment the extra bytes would be written, one layer down in the
    // wrapped deflate decoder.
    let png = build_png(1, 1, 8, 0, 0, None, None, &[0, 1, 0, 2]);
    assert_eq!(
        decode(&png, &ROOMY),
        Err(DecodeError::CompressedData(
            tairix_compress::zlib::Error::Body(tairix_compress::inflate::Error::OutputOverflow)
        ))
    );
}

#[test]
fn data_after_iend_is_refused() {
    let mut png = build_png(1, 1, 8, 0, 0, None, None, &[0, 0]);
    png.extend(chunk(IHDR, &ihdr_payload(1, 1, 8, 0, 0)));
    assert_eq!(decode(&png, &ROOMY), Err(DecodeError::DataAfterEnd));
}

#[test]
fn unknown_critical_chunk_is_refused() {
    let mut png = SIGNATURE.to_vec();
    png.extend(chunk(IHDR, &ihdr_payload(1, 1, 8, 0, 0)));
    png.extend(chunk(*b"FOOO", &[1, 2, 3])); // uppercase first letter: critical
    png.extend(chunk(IDAT, &zlib_wrap(&[0, 0])));
    png.extend(chunk(IEND, &[]));
    assert_eq!(decode(&png, &ROOMY), Err(DecodeError::UnknownCriticalChunk));
}

#[test]
fn unknown_ancillary_chunk_is_skipped() {
    let mut png = SIGNATURE.to_vec();
    png.extend(chunk(IHDR, &ihdr_payload(1, 1, 8, 0, 0)));
    png.extend(chunk(*b"foOO", &[1, 2, 3])); // lowercase first letter: ancillary
    png.extend(chunk(IDAT, &zlib_wrap(&[0, 0])));
    png.extend(chunk(IEND, &[]));
    assert!(decode(&png, &ROOMY).is_ok());
}

#[test]
fn duplicate_ihdr_is_refused() {
    let mut png = SIGNATURE.to_vec();
    png.extend(chunk(IHDR, &ihdr_payload(1, 1, 8, 0, 0)));
    png.extend(chunk(IHDR, &ihdr_payload(1, 1, 8, 0, 0)));
    assert_eq!(decode(&png, &ROOMY), Err(DecodeError::DuplicateHeader));
}

#[test]
fn duplicate_plte_is_refused() {
    let mut png = SIGNATURE.to_vec();
    png.extend(chunk(IHDR, &ihdr_payload(1, 1, 8, 3, 0)));
    png.extend(chunk(PLTE, &[1, 2, 3]));
    png.extend(chunk(PLTE, &[4, 5, 6]));
    assert_eq!(decode(&png, &ROOMY), Err(DecodeError::DuplicatePalette));
}

#[test]
fn duplicate_trns_is_refused() {
    let mut png = SIGNATURE.to_vec();
    png.extend(chunk(IHDR, &ihdr_payload(1, 1, 8, 0, 0)));
    png.extend(chunk(TRNS, &[0, 1]));
    png.extend(chunk(TRNS, &[0, 2]));
    assert_eq!(
        decode(&png, &ROOMY),
        Err(DecodeError::DuplicateTransparency)
    );
}

#[test]
fn trns_forbidden_on_colour_type_six() {
    let mut png = SIGNATURE.to_vec();
    png.extend(chunk(IHDR, &ihdr_payload(1, 1, 8, 6, 0)));
    png.extend(chunk(TRNS, &[0, 1]));
    assert_eq!(
        decode(&png, &ROOMY),
        Err(DecodeError::TransparencyForbidden)
    );
}

#[test]
fn non_contiguous_idat_is_refused() {
    let mut png = SIGNATURE.to_vec();
    png.extend(chunk(IHDR, &ihdr_payload(1, 2, 8, 0, 0)));
    let full = zlib_wrap(&[0, 1, 0, 2]);
    let (first_half, second_half) = full.split_at(full.len() / 2);
    png.extend(chunk(IDAT, first_half));
    png.extend(chunk(*b"tEXt", b"comment"));
    png.extend(chunk(IDAT, second_half));
    png.extend(chunk(IEND, &[]));
    assert_eq!(
        decode(&png, &ROOMY),
        Err(DecodeError::ImageDataNotContiguous)
    );
}

#[test]
fn header_not_first_is_refused() {
    let mut png = SIGNATURE.to_vec();
    png.extend(chunk(*b"tEXt", b"comment"));
    png.extend(chunk(IHDR, &ihdr_payload(1, 1, 8, 0, 0)));
    assert_eq!(decode(&png, &ROOMY), Err(DecodeError::HeaderNotFirst));
}

#[test]
fn missing_idat_is_refused() {
    let mut png = SIGNATURE.to_vec();
    png.extend(chunk(IHDR, &ihdr_payload(1, 1, 8, 0, 0)));
    png.extend(chunk(IEND, &[]));
    assert_eq!(decode(&png, &ROOMY), Err(DecodeError::MissingImageData));
}

#[test]
fn missing_iend_is_refused() {
    let mut png = SIGNATURE.to_vec();
    png.extend(chunk(IHDR, &ihdr_payload(1, 1, 8, 0, 0)));
    png.extend(chunk(IDAT, &zlib_wrap(&[0, 0])));
    assert_eq!(decode(&png, &ROOMY), Err(DecodeError::MissingEnd));
}

#[test]
fn malformed_iend_is_refused() {
    let mut png = SIGNATURE.to_vec();
    png.extend(chunk(IHDR, &ihdr_payload(1, 1, 8, 0, 0)));
    png.extend(chunk(IDAT, &zlib_wrap(&[0, 0])));
    png.extend(chunk(IEND, &[1]));
    assert_eq!(decode(&png, &ROOMY), Err(DecodeError::MalformedEnd));
}

#[test]
fn colour_type_reports_the_right_channel_count() {
    assert_eq!(ColourType::Grey.channels(), 1);
    assert_eq!(ColourType::Truecolour.channels(), 3);
    assert_eq!(ColourType::Indexed.channels(), 1);
    assert_eq!(ColourType::GreyAlpha.channels(), 2);
    assert_eq!(ColourType::Rgba.channels(), 4);
}

#[test]
fn sniff_recognises_a_built_fixture() {
    let png = build_png(1, 1, 8, 0, 0, None, None, &[0, 0]);
    assert_eq!(sniff(&png), Some(ImageFormat::Png));
}
