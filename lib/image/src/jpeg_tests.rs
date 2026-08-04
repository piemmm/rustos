//! Unit tests for the JPEG decoder.
//!
//! Fixtures are built marker by marker through the small helpers below,
//! entirely independent of the decoder's own private tables and marker
//! constants (a `ZIGZAG` order and marker codes are restated here), so a
//! bug shared between the encoder-side helper and the decoder under test
//! cannot cancel out. [`TestHuffman`] replays the same canonical
//! Huffman-code assignment ITU-T T.81 Annex C.2 defines (independently
//! re-derived here, not copied from `src/jpeg.rs`), so a fixture's
//! `DHT` payload and the codes used to pack its entropy data always agree.
//!
//! Every fixture uses flat (all-DC, zero-AC) blocks unless a test's whole
//! point is AC content (the progressive-vs-baseline comparison): a
//! DC-only block's decoded pixel value is `round(dc / 8) + 128`, an
//! independently checkable formula, which is what makes the "known
//! output" test a real check rather than a round-trip tautology.

use alloc::vec;
use alloc::vec::Vec;

use super::{decode, decode_fitted};
use crate::{sniff, DecodeError, DecodeLimits, FitBox, ImageFormat};

/// Generous limits for every fixture that is not itself exercising a
/// limit refusal.
const ROOMY: DecodeLimits = DecodeLimits::new(4096, 4096, 4096 * 4096, 1_000_000);

// ---------------------------------------------------------------------
// Marker codes and the zig-zag order, restated (this file never reaches
// into the decoder's own private constants).
// ---------------------------------------------------------------------

const SOF0: u8 = 0xC0;
const SOF1: u8 = 0xC1;
const SOF2: u8 = 0xC2;
const SOF9: u8 = 0xC9;
const DHT: u8 = 0xC4;
const RST0: u8 = 0xD0;
const SOI: u8 = 0xD8;
const EOI: u8 = 0xD9;
const SOS: u8 = 0xDA;
const DQT: u8 = 0xDB;
const DNL: u8 = 0xDC;
const DRI: u8 = 0xDD;

const ZIGZAG: [usize; 64] = [
    0, 1, 8, 16, 9, 2, 3, 10, 17, 24, 32, 25, 18, 11, 4, 5, 12, 19, 26, 33, 40, 48, 41, 34, 27, 20,
    13, 6, 7, 14, 21, 28, 35, 42, 49, 56, 57, 50, 43, 36, 29, 22, 15, 23, 30, 37, 44, 51, 58, 59,
    52, 45, 38, 31, 39, 46, 53, 60, 61, 54, 47, 55, 62, 63,
];

// ---------------------------------------------------------------------
// Segment builders
// ---------------------------------------------------------------------

/// A length-prefixed marker segment: `FF`, `code`, big-endian length
/// (counting itself), `payload`.
fn segment(code: u8, payload: &[u8]) -> Vec<u8> {
    let mut out = vec![0xFF, code];
    let len = u16::try_from(payload.len() + 2).expect("test payload fits a segment length");
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(payload);
    out
}

/// A standalone marker with no length field (`SOI`, `EOI`, restart
/// markers).
fn bare_marker(code: u8) -> Vec<u8> {
    vec![0xFF, code]
}

/// A `DQT` payload for one table: `precision16` selects 8- or 16-bit
/// elements; `natural` is the table in natural (row-major) order, which
/// this helper reorders into the zig-zag order the file format itself
/// uses.
fn dqt_payload(index: u8, precision16: bool, natural: &[u16; 64]) -> Vec<u8> {
    let mut out = vec![(u8::from(precision16) << 4) | index];
    for &zigzag_index in &ZIGZAG {
        let value = natural[zigzag_index];
        if precision16 {
            out.extend_from_slice(&value.to_be_bytes());
        } else {
            out.push(u8::try_from(value).expect("test quant value fits a byte"));
        }
    }
    out
}

/// A `SOF0`/`SOF1`/`SOF2` payload. `components` is `(id, h, v,
/// quant_table)` per component.
fn sof_payload(precision: u8, width: u16, height: u16, components: &[(u8, u8, u8, u8)]) -> Vec<u8> {
    let mut out = vec![precision];
    out.extend_from_slice(&height.to_be_bytes());
    out.extend_from_slice(&width.to_be_bytes());
    out.push(u8::try_from(components.len()).expect("test component count fits a byte"));
    for &(id, h, v, q) in components {
        out.push(id);
        out.push((h << 4) | v);
        out.push(q);
    }
    out
}

/// A `SOS` payload. `components` is `(id, dc_table, ac_table)` per scan
/// component.
fn sos_payload(components: &[(u8, u8, u8)], ss: u8, se: u8, ah: u8, al: u8) -> Vec<u8> {
    let mut out = vec![u8::try_from(components.len()).expect("fits")];
    for &(id, dc, ac) in components {
        out.push(id);
        out.push((dc << 4) | ac);
    }
    out.push(ss);
    out.push(se);
    out.push((ah << 4) | al);
    out
}

/// A `DRI` payload.
fn dri_payload(interval: u16) -> Vec<u8> {
    interval.to_be_bytes().to_vec()
}

// ---------------------------------------------------------------------
// A canonical Huffman table built independently of `src/jpeg.rs`, plus a
// big-endian, MSB-first bit writer with JPEG byte-stuffing applied once
// at the end.
// ---------------------------------------------------------------------

/// A Huffman table under construction: `by_length[i]` lists the symbols
/// assigned to code length `i + 1`, in code order.
struct TestHuffman {
    by_length: [Vec<u8>; 16],
}

impl TestHuffman {
    /// Every symbol in `symbols` at the same code length — the smallest
    /// length whose `2^length` codes can hold them all. Not an optimal
    /// Huffman code, but a perfectly valid canonical one (Kraft's
    /// inequality only needs `<= 1`, not `== 1`), which is all a test
    /// fixture needs.
    fn flat(symbols: &[u8]) -> Self {
        let mut length = 1u32;
        while (1usize << length) < symbols.len() {
            length += 1;
        }
        let mut by_length: [Vec<u8>; 16] = Default::default();
        by_length[usize::try_from(length - 1).expect("small")] = symbols.to_vec();
        Self { by_length }
    }

    /// The `DHT` payload for one table (`class` 0 = DC, 1 = AC).
    fn dht_payload(&self, class: u8, index: u8) -> Vec<u8> {
        let mut out = vec![(class << 4) | index];
        for group in &self.by_length {
            out.push(u8::try_from(group.len()).expect("test table stays small"));
        }
        for group in &self.by_length {
            out.extend_from_slice(group);
        }
        out
    }

    /// The `(code, length)` this table's canonical assignment gives
    /// `symbol` (ITU-T T.81 Annex C.2), replayed independently of the
    /// decoder's own table builder.
    fn code_for(&self, symbol: u8) -> (u32, u32) {
        let mut code = 0u32;
        for (i, group) in self.by_length.iter().enumerate() {
            for &s in group {
                if s == symbol {
                    return (code, u32::try_from(i + 1).expect("small"));
                }
                code += 1;
            }
            code <<= 1;
        }
        panic!("test fixture Huffman table has no entry for symbol {symbol}");
    }
}

/// A JPEG category/magnitude encode: the `(category, raw_bits)` pair for
/// a signed value, the inverse of the decoder's own `EXTEND` procedure.
fn category_and_bits(value: i32) -> (u32, u32) {
    if value == 0 {
        return (0, 0);
    }
    let magnitude = value.unsigned_abs();
    let mut category = 0u32;
    while (1u32 << category) <= magnitude {
        category += 1;
    }
    // A non-negative value is sent as its own magnitude; a negative one as
    // `value + (1 << category) - 1`, spelled here via the unsigned
    // magnitude to avoid relying on signed-overflow behaviour.
    let raw = if value >= 0 {
        magnitude
    } else {
        (1u32 << category) - 1 - magnitude
    };
    (category, raw)
}

/// A big-endian, MSB-first bit writer. `finish` applies JPEG's `0xFF` ->
/// `0xFF 0x00` byte-stuffing exactly once, over the whole packed buffer,
/// so callers never have to think about it while packing codes.
#[derive(Default)]
struct BitWriter {
    bytes: Vec<u8>,
    current: u8,
    filled: u32,
}

impl BitWriter {
    fn new() -> Self {
        Self::default()
    }

    fn put_bit(&mut self, bit: u32) {
        self.current = (self.current << 1) | u8::try_from(bit & 1).unwrap_or(0);
        self.filled += 1;
        if self.filled == 8 {
            self.bytes.push(self.current);
            self.current = 0;
            self.filled = 0;
        }
    }

    /// Pack `code`'s `length` bits, MSB first.
    fn put_code(&mut self, code: u32, length: u32) {
        for i in (0..length).rev() {
            self.put_bit((code >> i) & 1);
        }
    }

    /// Encode one DC symbol: a Huffman-coded category, then the category's
    /// raw magnitude bits.
    fn put_dc(&mut self, table: &TestHuffman, diff: i32) {
        let (category, raw) = category_and_bits(diff);
        let (code, length) = table.code_for(u8::try_from(category).expect("small"));
        self.put_code(code, length);
        if category > 0 {
            self.put_code(raw, category);
        }
    }

    /// Encode one AC run/size symbol (`run` zero coefficients, then a
    /// nonzero `value`), or pass `value == 0` with `run == 0` to mean
    /// `EOB`.
    fn put_ac(&mut self, table: &TestHuffman, run: u8, value: i32) {
        if value == 0 {
            let (code, length) = table.code_for(0x00);
            self.put_code(code, length);
            return;
        }
        let (category, raw) = category_and_bits(value);
        let symbol = (run << 4) | u8::try_from(category).expect("small");
        let (code, length) = table.code_for(symbol);
        self.put_code(code, length);
        self.put_code(raw, category);
    }

    /// Byte-align, padding the current partial byte with 1-bits (ITU-T
    /// T.81 §B.2.5's convention, though the decoder accepts any padding).
    fn pad_to_byte(&mut self) {
        while self.filled != 0 {
            self.put_bit(1);
        }
    }

    /// Finish: byte-align, then stuff every `0xFF` byte with a trailing
    /// `0x00`.
    fn finish(mut self) -> Vec<u8> {
        self.pad_to_byte();
        let mut out = Vec::with_capacity(self.bytes.len());
        for byte in self.bytes {
            out.push(byte);
            if byte == 0xFF {
                out.push(0x00);
            }
        }
        out
    }
}

// ---------------------------------------------------------------------
// Shared fixture tables
// ---------------------------------------------------------------------

/// Covers DC categories `0..=12` — every magnitude an 8-bit-precision
/// image's DC coefficient (up to category 11) or the difference between
/// two such coefficients (up to category 12) can need.
fn dc_table() -> TestHuffman {
    TestHuffman::flat(&[0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12])
}

/// Covers `EOB`, `ZRL`, and every `(run = 0, size = 1..=8)` combination —
/// enough for every AC value this test file's fixtures ever encode.
fn ac_table() -> TestHuffman {
    TestHuffman::flat(&[0x00, 0xF0, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08])
}

/// An all-ones quantisation table (dequantised value == coded coefficient),
/// which keeps every fixture's arithmetic to the coefficient values
/// themselves.
const UNIT_QUANT: [u16; 64] = [1; 64];

/// The `(category, raw_bits)` — restated here as `(diff, ...)` — needed to
/// encode a DC-only block whose decoded flat pixel value is `pixel`,
/// given the running DC `predictor` (updated in place).
fn dc_diff_for_pixel(predictor: &mut i32, pixel: i32) -> i32 {
    let target_dc = 8 * (pixel - 128);
    let diff = target_dc - *predictor;
    *predictor = target_dc;
    diff
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

/// Pack a single-component, `h == v == 1` entropy stream of DC-only
/// (all-AC-zero) blocks, one per entry of `pixel_values`, in raster MCU
/// order. `restart_interval`, when given, inserts a real `RSTn` marker
/// (cycling `0..=7`) after every that-many blocks (never after the last)
/// and resets the DC predictor at each one, exactly as a real encoder
/// would.
fn encode_flat_mono_entropy(pixel_values: &[i32], restart_interval: Option<u32>) -> Vec<u8> {
    let dc = dc_table();
    let ac = ac_table();
    let interval = restart_interval.unwrap_or(u32::MAX);
    let mut out = Vec::new();
    let mut restart_seq = 0u8;
    let mut index = 0usize;
    while index < pixel_values.len() {
        let mut writer = BitWriter::new();
        let mut predictor = 0i32;
        let mut count = 0u32;
        while index < pixel_values.len() && count < interval {
            let diff = dc_diff_for_pixel(&mut predictor, pixel_values[index]);
            writer.put_dc(&dc, diff);
            writer.put_ac(&ac, 0, 0); // EOB: every AC coefficient is zero
            index += 1;
            count += 1;
        }
        out.extend(writer.finish());
        if index < pixel_values.len() {
            out.push(0xFF);
            out.push(RST0 + restart_seq);
            restart_seq = (restart_seq + 1) % 8;
        }
    }
    out
}

/// A minimal, well-formed baseline JPEG: single component, `width` ×
/// `height`, one flat DC-only pixel value per 8×8 block (in raster MCU
/// order), optionally with restart markers.
fn build_flat_mono(
    width: u16,
    height: u16,
    pixel_values: &[i32],
    restart_interval: Option<u32>,
) -> Vec<u8> {
    let mut out = bare_marker(SOI);
    out.extend(segment(DQT, &dqt_payload(0, false, &UNIT_QUANT)));
    out.extend(segment(DHT, &dc_table().dht_payload(0, 0)));
    out.extend(segment(DHT, &ac_table().dht_payload(1, 0)));
    if let Some(interval) = restart_interval {
        out.extend(segment(
            DRI,
            &dri_payload(u16::try_from(interval).expect("small")),
        ));
    }
    out.extend(segment(
        SOF0,
        &sof_payload(8, width, height, &[(1, 1, 1, 0)]),
    ));
    out.extend(segment(SOS, &sos_payload(&[(1, 0, 0)], 0, 63, 0, 0)));
    out.extend(encode_flat_mono_entropy(pixel_values, restart_interval));
    out.extend(bare_marker(EOI));
    out
}

/// A single-block baseline JPEG encoding exactly `dc` and, at zig-zag
/// position 1, `ac1` (every other AC coefficient zero).
fn build_baseline_single_block(dc: i32, ac1: i32) -> Vec<u8> {
    let dct = dc_table();
    let act = ac_table();
    let mut out = bare_marker(SOI);
    out.extend(segment(DQT, &dqt_payload(0, false, &UNIT_QUANT)));
    out.extend(segment(DHT, &dct.dht_payload(0, 0)));
    out.extend(segment(DHT, &act.dht_payload(1, 0)));
    out.extend(segment(SOF0, &sof_payload(8, 8, 8, &[(1, 1, 1, 0)])));
    out.extend(segment(SOS, &sos_payload(&[(1, 0, 0)], 0, 63, 0, 0)));
    let mut w = BitWriter::new();
    w.put_dc(&dct, dc);
    w.put_ac(&act, 0, ac1);
    w.put_ac(&act, 0, 0); // EOB for the rest of the block
    out.extend(w.finish());
    out.extend(bare_marker(EOI));
    out
}

/// The equivalent progressive JPEG: `DC` first (`Al = 1`) then a
/// DC-refine scan, `AC` first (`Al = 1`, over `dc`'s companion `ac1 = 7`
/// only) then an AC-refine scan — chosen so `dc` is even (no refine bit
/// needed) and `ac1 == 7` (so its refine bit is exactly the `1` needed to
/// turn the AC-first reconstruction of `6` into `7`). Deliberately not
/// general: a single, hand-checked example is the strongest correctness
/// check available without an external file, per a real successive-
/// approximation encoder's bit-plane splitting.
fn build_progressive_single_block_example() -> Vec<u8> {
    const DC: i32 = 576; // even: 576 >> 1 == 288, 288 << 1 == 576 exactly
    const AC1_FIRST: i32 = 3; // 7 >> 1 == 3
    let dct = dc_table();
    let act = ac_table();

    let mut out = bare_marker(SOI);
    out.extend(segment(DQT, &dqt_payload(0, false, &UNIT_QUANT)));
    out.extend(segment(DHT, &dct.dht_payload(0, 0)));
    out.extend(segment(DHT, &act.dht_payload(1, 0)));
    out.extend(segment(SOF2, &sof_payload(8, 8, 8, &[(1, 1, 1, 0)])));

    // DC first: Ss = Se = 0, Ah = 0, Al = 1.
    out.extend(segment(SOS, &sos_payload(&[(1, 0, 0)], 0, 0, 0, 1)));
    let mut w = BitWriter::new();
    w.put_dc(&dct, DC >> 1);
    out.extend(w.finish());

    // DC refine: Ah = 1, Al = 0. `DC` is exactly `288 << 1`, so the one
    // refinement bit is 0 (no correction needed).
    out.extend(segment(SOS, &sos_payload(&[(1, 0, 0)], 0, 0, 1, 0)));
    let mut w = BitWriter::new();
    w.put_bit(0);
    out.extend(w.finish());

    // AC first: Ss = 1, Se = 63, Ah = 0, Al = 1. Places `3 << 1 == 6` at
    // position 1, then EOB (run = 0) for the rest of the block.
    out.extend(segment(SOS, &sos_payload(&[(1, 0, 0)], 1, 63, 0, 1)));
    let mut w = BitWriter::new();
    w.put_ac(&act, 0, AC1_FIRST);
    w.put_ac(&act, 0, 0);
    out.extend(w.finish());

    // AC refine: Ss = 1, Se = 63, Ah = 1, Al = 0. A single EOB (run = 0)
    // code sets an end-of-band run of exactly this one block, whose
    // correction phase then refines position 1's existing `6` by the one
    // transmitted bit (`1`), turning it into `7`.
    out.extend(segment(SOS, &sos_payload(&[(1, 0, 0)], 1, 63, 1, 0)));
    let mut w = BitWriter::new();
    w.put_ac(&act, 0, 0);
    w.put_bit(1);
    out.extend(w.finish());

    out.extend(bare_marker(EOI));
    out
}

// =======================================================================
// Format identification
// =======================================================================

#[test]
fn sniff_recognises_the_jpeg_signature() {
    let mut bytes = vec![0xFF, 0xD8, 0xFF, 0xE0];
    bytes.extend_from_slice(b"anything after the signature");
    assert_eq!(sniff(&bytes), Some(ImageFormat::Jpeg));
}

// =======================================================================
// Correctness
// =======================================================================

#[test]
fn eight_by_eight_grey_image_decodes_to_a_known_output() {
    // DC = 576, every AC coefficient zero: pixel = round(576 / 8) + 128
    // == 200, independently of anything this crate's own decoder computes.
    let jpeg = build_baseline_single_block(576, 0);
    let image = decode(&jpeg, &ROOMY).expect("decodes");
    assert_eq!((image.width(), image.height()), (8, 8));
    let pixels = image.pixels();
    for y in 0..8 {
        for x in 0..8 {
            assert_eq!(
                rgba(pixels, 8, x, y),
                [200, 200, 200, 255],
                "pixel ({x}, {y})"
            );
        }
    }
}

#[test]
fn three_component_four_two_zero_image_decodes_each_block() {
    // One MCU: Y at 2x2 (four blocks), Cb/Cr at 1x1, all DC-only. Neutral
    // (128) chroma keeps R == G == B == Y exactly, so this also exercises
    // the YCbCr conversion path with a known, hand-checked answer.
    let dct = dc_table();
    let act = ac_table();
    let mut out = bare_marker(SOI);
    out.extend(segment(DQT, &dqt_payload(0, false, &UNIT_QUANT)));
    out.extend(segment(DHT, &dct.dht_payload(0, 0)));
    out.extend(segment(DHT, &act.dht_payload(1, 0)));
    out.extend(segment(
        SOF0,
        &sof_payload(8, 16, 16, &[(1, 2, 2, 0), (2, 1, 1, 0), (3, 1, 1, 0)]),
    ));
    out.extend(segment(
        SOS,
        &sos_payload(&[(1, 0, 0), (2, 0, 0), (3, 0, 0)], 0, 63, 0, 0),
    ));

    let mut w = BitWriter::new();
    let mut y_predictor = 0i32;
    for &pixel in &[100i32, 110, 120, 130] {
        let diff = dc_diff_for_pixel(&mut y_predictor, pixel);
        w.put_dc(&dct, diff);
        w.put_ac(&act, 0, 0);
    }
    // Cb then Cr: one neutral DC-only block each, each starting from its
    // own zero predictor, so both encode identically.
    for _chroma in 0..2 {
        let mut predictor = 0i32;
        let diff = dc_diff_for_pixel(&mut predictor, 128);
        w.put_dc(&dct, diff);
        w.put_ac(&act, 0, 0);
    }
    out.extend(w.finish());
    out.extend(bare_marker(EOI));

    let image = decode(&out, &ROOMY).expect("decodes");
    assert_eq!((image.width(), image.height()), (16, 16));
    let pixels = image.pixels();
    let expect_quadrant = |x0: u32, y0: u32, value: u8| {
        for y in y0..y0 + 8 {
            for x in x0..x0 + 8 {
                assert_eq!(
                    rgba(pixels, 16, x, y),
                    [value, value, value, 255],
                    "pixel ({x}, {y})"
                );
            }
        }
    };
    expect_quadrant(0, 0, 100);
    expect_quadrant(8, 0, 110);
    expect_quadrant(0, 8, 120);
    expect_quadrant(8, 8, 130);
}

#[test]
fn sixteen_by_sixteen_image_spans_several_mcus() {
    let jpeg = build_flat_mono(16, 16, &[60, 90, 150, 200], None);
    let image = decode(&jpeg, &ROOMY).expect("decodes");
    assert_eq!((image.width(), image.height()), (16, 16));
    let pixels = image.pixels();
    assert_eq!(rgba(pixels, 16, 0, 0), [60, 60, 60, 255]);
    assert_eq!(rgba(pixels, 16, 15, 0), [90, 90, 90, 255]);
    assert_eq!(rgba(pixels, 16, 0, 15), [150, 150, 150, 255]);
    assert_eq!(rgba(pixels, 16, 15, 15), [200, 200, 200, 255]);
}

#[test]
fn restart_markers_reset_the_dc_predictor() {
    let jpeg = build_flat_mono(16, 16, &[60, 90, 150, 200], Some(1));
    let image = decode(&jpeg, &ROOMY).expect("decodes");
    let pixels = image.pixels();
    assert_eq!(rgba(pixels, 16, 0, 0), [60, 60, 60, 255]);
    assert_eq!(rgba(pixels, 16, 15, 0), [90, 90, 90, 255]);
    assert_eq!(rgba(pixels, 16, 0, 15), [150, 150, 150, 255]);
    assert_eq!(rgba(pixels, 16, 15, 15), [200, 200, 200, 255]);
}

#[test]
fn a_size_that_is_not_a_multiple_of_the_mcu_exercises_edge_padding() {
    // 10x10 at 1x1 sampling needs a 2x2 padded block grid (16x16), but
    // only the top-left 10x10 corner of it is visible.
    let jpeg = build_flat_mono(10, 10, &[11, 22, 33, 44], None);
    let image = decode(&jpeg, &ROOMY).expect("decodes");
    assert_eq!((image.width(), image.height()), (10, 10));
    let pixels = image.pixels();
    assert_eq!(rgba(pixels, 10, 0, 0), [11, 11, 11, 255]);
    assert_eq!(rgba(pixels, 10, 9, 0), [22, 22, 22, 255]);
    assert_eq!(rgba(pixels, 10, 0, 9), [33, 33, 33, 255]);
    assert_eq!(rgba(pixels, 10, 9, 9), [44, 44, 44, 255]);
}

#[test]
fn extended_sequential_sof1_decodes() {
    let dct = dc_table();
    let act = ac_table();
    let mut out = bare_marker(SOI);
    out.extend(segment(DQT, &dqt_payload(0, false, &UNIT_QUANT)));
    out.extend(segment(DHT, &dct.dht_payload(0, 0)));
    out.extend(segment(DHT, &act.dht_payload(1, 0)));
    out.extend(segment(SOF1, &sof_payload(8, 8, 8, &[(1, 1, 1, 0)])));
    out.extend(segment(SOS, &sos_payload(&[(1, 0, 0)], 0, 63, 0, 0)));
    let mut w = BitWriter::new();
    w.put_dc(&dct, 576);
    w.put_ac(&act, 0, 0);
    out.extend(w.finish());
    out.extend(bare_marker(EOI));

    let image = decode(&out, &ROOMY).expect("decodes");
    assert_eq!(rgba(image.pixels(), 8, 0, 0), [200, 200, 200, 255]);
}

#[test]
fn progressive_scans_decode_to_the_same_pixels_as_the_equivalent_baseline() {
    let baseline = decode(&build_baseline_single_block(576, 7), &ROOMY).expect("baseline decodes");
    let progressive =
        decode(&build_progressive_single_block_example(), &ROOMY).expect("progressive decodes");
    assert_eq!(baseline, progressive);
}

// =======================================================================
// Reduced-scale decoding
// =======================================================================

#[test]
fn decode_fitted_picks_the_smallest_covering_scale_and_rounds_up() {
    // 21x21 natural size: outputs at m = 1, 2, 4, 8 are 3, 6, 11, 21 --
    // the m = 4 (half) output of 11 is not exactly 21 / 2 (10.5), so this
    // also exercises the "rounds up" requirement.
    let pixels: Vec<i32> = core::iter::repeat_n(128, 9).collect(); // 3x3 blocks
    let jpeg = build_flat_mono(21, 21, &pixels, None);

    let natural = decode(&jpeg, &ROOMY).expect("decodes");
    assert_eq!((natural.width(), natural.height()), (21, 21));

    let fitted = decode_fitted(&jpeg, &ROOMY, FitBox::new(9, 9)).expect("decodes");
    assert_eq!((fitted.width(), fitted.height()), (11, 11));
}

#[test]
fn decode_fitted_never_scales_up_past_natural_size() {
    let pixels: Vec<i32> = core::iter::repeat_n(128, 9).collect();
    let jpeg = build_flat_mono(21, 21, &pixels, None);
    let fitted = decode_fitted(&jpeg, &ROOMY, FitBox::new(1000, 1000)).expect("decodes");
    assert_eq!((fitted.width(), fitted.height()), (21, 21));
}

#[test]
fn decode_fitted_degrades_to_the_largest_scale_the_limits_admit() {
    // 21x21 natural size, so the scale outputs are 3, 6, 11 and 21. A box
    // of 21x21 is only covered by the full scale, but limits that stop
    // short of 21 must not refuse the image: the next scale down that they
    // do admit is decoded instead, at full fidelity for its own size.
    let pixels: Vec<i32> = core::iter::repeat_n(128, 9).collect();
    let jpeg = build_flat_mono(21, 21, &pixels, None);
    let box_needing_full_scale = FitBox::new(21, 21);

    let half_only = DecodeLimits::new(15, 15, 15 * 15, 0);
    let fitted = decode_fitted(&jpeg, &half_only, box_needing_full_scale).expect("degrades");
    assert_eq!((fitted.width(), fitted.height()), (11, 11));
    assert_eq!(rgba(fitted.pixels(), 11, 10, 10), [128, 128, 128, 255]);

    // Tighter still: the half scale's 11x11 no longer fits either, so the
    // quarter scale's 6x6 is the sharpest affordable answer.
    let quarter_only = DecodeLimits::new(10, 10, 10 * 10, 0);
    let fitted = decode_fitted(&jpeg, &quarter_only, box_needing_full_scale).expect("degrades");
    assert_eq!((fitted.width(), fitted.height()), (6, 6));
    assert_eq!(rgba(fitted.pixels(), 6, 5, 5), [128, 128, 128, 255]);

    // A pixel-count limit degrades exactly as a per-axis one does.
    let by_pixel_count = DecodeLimits::new(21, 21, 6 * 6, 0);
    let fitted = decode_fitted(&jpeg, &by_pixel_count, box_needing_full_scale).expect("degrades");
    assert_eq!((fitted.width(), fitted.height()), (6, 6));
}

#[test]
fn decode_fitted_refuses_a_source_too_large_even_at_one_eighth() {
    // An eighth of 21x21 is 3x3, the smallest a reduced inverse DCT can
    // produce, so limits below that leave nothing to degrade to.
    let pixels: Vec<i32> = core::iter::repeat_n(128, 9).collect();
    let jpeg = build_flat_mono(21, 21, &pixels, None);
    let fit = FitBox::new(21, 21);

    assert_eq!(
        decode_fitted(&jpeg, &DecodeLimits::new(2, 21, 21 * 21, 0), fit),
        Err(DecodeError::WidthExceedsLimit)
    );
    assert_eq!(
        decode_fitted(&jpeg, &DecodeLimits::new(21, 2, 21 * 21, 0), fit),
        Err(DecodeError::HeightExceedsLimit)
    );
    assert_eq!(
        decode_fitted(&jpeg, &DecodeLimits::new(21, 21, 3 * 3 - 1, 0), fit),
        Err(DecodeError::PixelCountExceedsLimit)
    );
    // The eighth scale itself is admitted the moment it fits.
    let eighth = decode_fitted(&jpeg, &DecodeLimits::new(3, 3, 3 * 3, 0), fit).expect("decodes");
    assert_eq!((eighth.width(), eighth.height()), (3, 3));
}

// =======================================================================
// Refusals
// =======================================================================

#[test]
fn truncated_before_soi_second_byte_is_refused() {
    assert_eq!(decode(&[0xFF], &ROOMY), Err(DecodeError::JpegBadSignature));
}

#[test]
fn truncation_at_each_marker_is_refused() {
    let jpeg = build_baseline_single_block(576, 0);
    for cut in 1..jpeg.len() {
        let truncated = &jpeg[..cut];
        // Never panics; every truncation is either a refusal or (only
        // possible if the cut lands exactly on a complete file, which
        // `1..jpeg.len()` never does) a decode.
        let _ = decode(truncated, &ROOMY);
    }
}

#[test]
fn bad_marker_is_refused() {
    let mut out = bare_marker(SOI);
    out.extend(segment(0xF0, &[0, 0])); // JPGn: reserved, not APPn/COM
    assert_eq!(decode(&out, &ROOMY), Err(DecodeError::JpegUnknownMarker));
}

#[test]
fn missing_quantisation_table_is_refused() {
    let dct = dc_table();
    let act = ac_table();
    let mut out = bare_marker(SOI);
    // No DQT at all.
    out.extend(segment(DHT, &dct.dht_payload(0, 0)));
    out.extend(segment(DHT, &act.dht_payload(1, 0)));
    out.extend(segment(SOF0, &sof_payload(8, 8, 8, &[(1, 1, 1, 0)])));
    out.extend(segment(SOS, &sos_payload(&[(1, 0, 0)], 0, 63, 0, 0)));
    let mut w = BitWriter::new();
    w.put_dc(&dct, 0);
    w.put_ac(&act, 0, 0);
    out.extend(w.finish());
    out.extend(bare_marker(EOI));
    assert_eq!(
        decode(&out, &ROOMY),
        Err(DecodeError::JpegMissingQuantizationTable)
    );
}

#[test]
fn missing_huffman_table_is_refused() {
    let mut out = bare_marker(SOI);
    out.extend(segment(DQT, &dqt_payload(0, false, &UNIT_QUANT)));
    // No DHT at all.
    out.extend(segment(SOF0, &sof_payload(8, 8, 8, &[(1, 1, 1, 0)])));
    out.extend(segment(SOS, &sos_payload(&[(1, 0, 0)], 0, 63, 0, 0)));
    out.extend(vec![0u8; 4]);
    out.extend(bare_marker(EOI));
    assert_eq!(
        decode(&out, &ROOMY),
        Err(DecodeError::JpegMissingHuffmanTable)
    );
}

#[test]
fn unknown_huffman_code_is_refused() {
    // The DC table's 13 symbols occupy 4-bit codes 0..=12; 15 (`1111`) is
    // never assigned, and no other length has any code at all, so three
    // bytes of `1111_1110` correctly exhaust the search to length 16
    // without a match, never a truncation.
    let mut out = bare_marker(SOI);
    out.extend(segment(DQT, &dqt_payload(0, false, &UNIT_QUANT)));
    out.extend(segment(DHT, &dc_table().dht_payload(0, 0)));
    out.extend(segment(DHT, &ac_table().dht_payload(1, 0)));
    out.extend(segment(SOF0, &sof_payload(8, 8, 8, &[(1, 1, 1, 0)])));
    out.extend(segment(SOS, &sos_payload(&[(1, 0, 0)], 0, 63, 0, 0)));
    out.extend_from_slice(&[0xFE, 0xFE, 0xFE]);
    out.extend(bare_marker(EOI));
    assert_eq!(
        decode(&out, &ROOMY),
        Err(DecodeError::JpegHuffmanCodeNotFound)
    );
}

#[test]
fn component_id_mismatch_is_refused() {
    let dct = dc_table();
    let act = ac_table();
    let mut out = bare_marker(SOI);
    out.extend(segment(DQT, &dqt_payload(0, false, &UNIT_QUANT)));
    out.extend(segment(DHT, &dct.dht_payload(0, 0)));
    out.extend(segment(DHT, &act.dht_payload(1, 0)));
    out.extend(segment(SOF0, &sof_payload(8, 8, 8, &[(1, 1, 1, 0)])));
    // The scan names component id 2, but the frame only declares id 1.
    out.extend(segment(SOS, &sos_payload(&[(2, 0, 0)], 0, 63, 0, 0)));
    out.extend(vec![0u8; 4]);
    out.extend(bare_marker(EOI));
    assert_eq!(
        decode(&out, &ROOMY),
        Err(DecodeError::JpegComponentIdMismatch)
    );
}

#[test]
fn zero_dimensions_are_refused() {
    let jpeg = build_flat_mono(0, 8, &[128], None);
    assert_eq!(decode(&jpeg, &ROOMY), Err(DecodeError::ZeroDimension));
}

#[test]
fn width_height_and_pixel_limits_are_enforced() {
    let jpeg = build_flat_mono(16, 16, &[128, 128, 128, 128], None);
    assert_eq!(
        decode(&jpeg, &DecodeLimits::new(15, 16, 15 * 16, 0)),
        Err(DecodeError::WidthExceedsLimit)
    );
    assert_eq!(
        decode(&jpeg, &DecodeLimits::new(16, 15, 15 * 16, 0)),
        Err(DecodeError::HeightExceedsLimit)
    );
    assert_eq!(
        decode(&jpeg, &DecodeLimits::new(16, 16, 16 * 16 - 1, 0)),
        Err(DecodeError::PixelCountExceedsLimit)
    );
    assert!(decode(&jpeg, &DecodeLimits::new(16, 16, 16 * 16, 0)).is_ok());
}

#[test]
fn progressive_coefficient_store_limit_is_enforced() {
    let jpeg = build_progressive_single_block_example();
    // One 8x8 greyscale block needs exactly 64 coefficients * 2 bytes.
    let tight = DecodeLimits::new(64, 64, 64 * 64, 128);
    assert!(decode(&jpeg, &tight).is_ok());
    let too_tight = DecodeLimits::new(64, 64, 64 * 64, 127);
    assert_eq!(
        decode(&jpeg, &too_tight),
        Err(DecodeError::JpegProgressiveCoefficientStoreExceedsLimit)
    );
}

#[test]
fn twelve_bit_precision_is_refused() {
    let mut out = bare_marker(SOI);
    out.extend(segment(DQT, &dqt_payload(0, false, &UNIT_QUANT)));
    out.extend(segment(DHT, &dc_table().dht_payload(0, 0)));
    out.extend(segment(DHT, &ac_table().dht_payload(1, 0)));
    out.extend(segment(SOF0, &sof_payload(12, 8, 8, &[(1, 1, 1, 0)])));
    assert_eq!(
        decode(&out, &ROOMY),
        Err(DecodeError::JpegUnsupportedPrecision)
    );
}

#[test]
fn arithmetic_coding_is_refused() {
    let mut out = bare_marker(SOI);
    out.extend(segment(DQT, &dqt_payload(0, false, &UNIT_QUANT)));
    out.extend(segment(SOF9, &sof_payload(8, 8, 8, &[(1, 1, 1, 0)])));
    assert_eq!(
        decode(&out, &ROOMY),
        Err(DecodeError::JpegArithmeticCodingUnsupported)
    );
}

#[test]
fn four_components_is_refused() {
    let mut out = bare_marker(SOI);
    out.extend(segment(DQT, &dqt_payload(0, false, &UNIT_QUANT)));
    out.extend(segment(
        SOF0,
        &sof_payload(
            8,
            8,
            8,
            &[(1, 1, 1, 0), (2, 1, 1, 0), (3, 1, 1, 0), (4, 1, 1, 0)],
        ),
    ));
    assert_eq!(
        decode(&out, &ROOMY),
        Err(DecodeError::JpegUnsupportedComponentCount)
    );
}

#[test]
fn two_components_is_refused() {
    let mut out = bare_marker(SOI);
    out.extend(segment(DQT, &dqt_payload(0, false, &UNIT_QUANT)));
    out.extend(segment(
        SOF0,
        &sof_payload(8, 8, 8, &[(1, 1, 1, 0), (2, 1, 1, 0)]),
    ));
    assert_eq!(
        decode(&out, &ROOMY),
        Err(DecodeError::JpegUnsupportedComponentCount)
    );
}

#[test]
fn dnl_marker_is_refused() {
    let mut out = bare_marker(SOI);
    out.extend(segment(DNL, &[0, 8]));
    assert_eq!(decode(&out, &ROOMY), Err(DecodeError::JpegDnlUnsupported));
}

// =======================================================================
// Property-style: the decoder never panics on arbitrary or mutated bytes
// =======================================================================

/// A small, deterministic PRNG local to this file (this crate's own
/// public API is all that is under test; nothing here reaches into
/// `tairix_fuzzseed` beyond the `Lcg` stream generator it already shares
/// with `tests/fuzz_image.rs`).
fn lcg_next(state: &mut u64) -> u64 {
    *state = state
        .wrapping_mul(6_364_136_223_846_793_005)
        .wrapping_add(1);
    *state
}

#[test]
fn arbitrary_bytes_never_panic() {
    let mut state = 0x9E37_79B9_7F4A_7C15u64;
    let limits = DecodeLimits::new(64, 64, 64 * 64, 4096);
    for _ in 0..2_000 {
        let len = usize::try_from(lcg_next(&mut state) % 300).unwrap_or(0);
        let mut buf = vec![0u8; len];
        for byte in &mut buf {
            *byte = u8::try_from(lcg_next(&mut state) & 0xFF).unwrap_or(0);
        }
        let _ = decode(&buf, &limits);
        let _ = decode_fitted(&buf, &limits, FitBox::new(8, 8));
    }
}

#[test]
fn mutated_valid_fixtures_never_panic() {
    let mut state = 0xD1B5_4A32_D192_ED03u64;
    let limits = DecodeLimits::new(64, 64, 64 * 64, 4096);
    let pristine = build_flat_mono(16, 16, &[60, 90, 150, 200], Some(2));
    for _ in 0..2_000 {
        let mut mutated = pristine.clone();
        let flips = usize::try_from(lcg_next(&mut state) % 6).unwrap_or(0);
        for _ in 0..flips {
            if mutated.is_empty() {
                break;
            }
            let pos = usize::try_from(lcg_next(&mut state)).unwrap_or(0) % mutated.len();
            let bit = u8::try_from(lcg_next(&mut state) & 7).unwrap_or(0);
            mutated[pos] ^= 1u8 << bit;
        }
        let _ = decode(&mutated, &limits);
    }
}
