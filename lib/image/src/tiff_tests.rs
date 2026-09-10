//! TIFF decoder tests. Every input is synthesised here: the crate ships no
//! fixtures, so a fixture is a builder call and a refusal is a mutation of
//! one.

use alloc::vec;
use alloc::vec::Vec;

use super::{
    decode, pages, probe, COMPRESSION_ADOBE_DEFLATE, COMPRESSION_CCITT_RLE, COMPRESSION_GROUP3,
    COMPRESSION_GROUP4, COMPRESSION_JPEG, COMPRESSION_LZW, COMPRESSION_NONE, COMPRESSION_PACK_BITS,
    PHOTOMETRIC_BLACK_ZERO, PHOTOMETRIC_MASK, PHOTOMETRIC_PALETTE, PHOTOMETRIC_RGB,
    PHOTOMETRIC_SEPARATED, PHOTOMETRIC_WHITE_ZERO, PHOTOMETRIC_YCBCR, TAG_BITS_PER_SAMPLE,
    TAG_COLOUR_MAP, TAG_COMPRESSION, TAG_EXTRA_SAMPLES, TAG_FILL_ORDER, TAG_IMAGE_LENGTH,
    TAG_IMAGE_WIDTH, TAG_INK_SET, TAG_JPEG_TABLES, TAG_NEW_SUBFILE_TYPE, TAG_ORIENTATION,
    TAG_PHOTOMETRIC, TAG_PLANAR_CONFIGURATION, TAG_PREDICTOR, TAG_REFERENCE_BLACK_WHITE,
    TAG_ROWS_PER_STRIP, TAG_SAMPLES_PER_PIXEL, TAG_SAMPLE_FORMAT, TAG_STRIP_BYTE_COUNTS,
    TAG_STRIP_OFFSETS, TAG_T4_OPTIONS, TAG_TILE_BYTE_COUNTS, TAG_TILE_LENGTH, TAG_TILE_OFFSETS,
    TAG_TILE_WIDTH, TAG_YCBCR_SUBSAMPLING,
};
use crate::{DecodeError, DecodeLimits, ImageFormat, RasterImage, Sequence, SequenceKind};

/// Generous enough that every fixture here decodes; the refusal tests state
/// their own tighter limits.
fn limits() -> DecodeLimits {
    DecodeLimits::new(4096, 4096, 4096 * 4096, 1 << 20)
}

/// Field types, as a directory entry spells them.
const BYTE: u16 = 1;
const SHORT: u16 = 3;
const LONG: u16 = 4;
const RATIONAL: u16 = 5;
const UNDEFINED: u16 = 7;

/// Bytes one element of a field type occupies.
fn width(kind: u16) -> usize {
    match kind {
        BYTE | UNDEFINED => 1,
        SHORT => 2,
        RATIONAL => 8,
        _ => 4,
    }
}

/// Numeric field values, laid out little-endian; a big-endian document
/// reverses each element as it is written.
fn raw(kind: u16, values: &[u32]) -> Vec<u8> {
    let mut out = Vec::new();
    for &value in values {
        match kind {
            BYTE | UNDEFINED => out.push(u8::try_from(value).unwrap_or(0)),
            SHORT => out.extend_from_slice(&u16::try_from(value).unwrap_or(0).to_le_bytes()),
            _ => out.extend_from_slice(&value.to_le_bytes()),
        }
    }
    out
}

/// One page of a document under construction.
#[derive(Clone)]
struct PageSpec {
    tags: Vec<(u16, u16, Vec<u8>)>,
    units: Vec<Vec<u8>>,
    tiled: bool,
}

impl PageSpec {
    fn new() -> Self {
        Self {
            tags: Vec::new(),
            units: Vec::new(),
            tiled: false,
        }
    }

    fn tag(mut self, tag: u16, kind: u16, values: &[u32]) -> Self {
        self.tags.retain(|(existing, _, _)| *existing != tag);
        self.tags.push((tag, kind, raw(kind, values)));
        self
    }

    /// A field whose bytes are given verbatim, for content the numeric form
    /// cannot spell.
    fn bytes(mut self, tag: u16, kind: u16, bytes: Vec<u8>) -> Self {
        self.tags.retain(|(existing, _, _)| *existing != tag);
        self.tags.push((tag, kind, bytes));
        self
    }

    fn without(mut self, tag: u16) -> Self {
        self.tags.retain(|(existing, _, _)| *existing != tag);
        self
    }

    fn unit(mut self, bytes: Vec<u8>) -> Self {
        self.units.push(bytes);
        self
    }

    fn units(mut self, units: Vec<Vec<u8>>) -> Self {
        self.units = units;
        self
    }

    fn tiled(mut self) -> Self {
        self.tiled = true;
        self
    }
}

/// Write `value` in the document's byte order.
fn put16(out: &mut Vec<u8>, big: bool, value: u16) {
    out.extend_from_slice(&if big {
        value.to_be_bytes()
    } else {
        value.to_le_bytes()
    });
}

fn put32(out: &mut Vec<u8>, big: bool, value: u32) {
    out.extend_from_slice(&if big {
        value.to_be_bytes()
    } else {
        value.to_le_bytes()
    });
}

/// A field's bytes in the document's byte order.
fn ordered(big: bool, kind: u16, bytes: &[u8]) -> Vec<u8> {
    if !big || width(kind) == 1 {
        return bytes.to_vec();
    }
    // A rational is two 32-bit halves rather than one 64-bit value.
    let step = if kind == RATIONAL { 4 } else { width(kind) };
    bytes
        .chunks(step)
        .flat_map(|chunk| chunk.iter().rev().copied())
        .collect()
}

/// Build a whole document from its pages.
fn build(big: bool, pages: &[PageSpec]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(if big { b"MM" } else { b"II" });
    put16(&mut out, big, 42);
    put32(&mut out, big, 8);
    for (index, page) in pages.iter().enumerate() {
        let (offsets_tag, counts_tag) = if page.tiled {
            (TAG_TILE_OFFSETS, TAG_TILE_BYTE_COUNTS)
        } else {
            (TAG_STRIP_OFFSETS, TAG_STRIP_BYTE_COUNTS)
        };
        let mut tags = page.tags.clone();
        if !page.units.is_empty() {
            let counts: Vec<u32> = page
                .units
                .iter()
                .map(|unit| u32::try_from(unit.len()).unwrap_or(0))
                .collect();
            tags.retain(|(tag, _, _)| *tag != offsets_tag && *tag != counts_tag);
            tags.push((offsets_tag, LONG, vec![0u8; page.units.len() * 4]));
            tags.push((counts_tag, LONG, raw(LONG, &counts)));
        }
        tags.sort_by_key(|(tag, _, _)| *tag);

        // Everything's length is known before anything is written, so the
        // layout settles in one pass and the unit offsets are exact.
        let ifd_at = out.len();
        let ifd_len = 2 + tags.len() * 12 + 4;
        let mut cursor = ifd_at + ifd_len;
        let mut places = Vec::new();
        for (_, kind, values) in &tags {
            let len = ordered(big, *kind, values).len();
            if len <= 4 {
                places.push(None);
            } else {
                places.push(Some(cursor));
                cursor += len;
            }
        }
        let mut unit_offsets = Vec::new();
        for unit in &page.units {
            unit_offsets.push(u32::try_from(cursor).unwrap_or(0));
            cursor += unit.len();
        }
        let next = if index + 1 == pages.len() { 0 } else { cursor };

        put16(&mut out, big, u16::try_from(tags.len()).unwrap_or(0));
        for ((tag, kind, values), place) in tags.iter().zip(&places) {
            let values = if *tag == offsets_tag && !page.units.is_empty() {
                raw(LONG, &unit_offsets)
            } else {
                values.clone()
            };
            let count = u32::try_from(values.len() / width(*kind)).unwrap_or(0);
            let values = ordered(big, *kind, &values);
            put16(&mut out, big, *tag);
            put16(&mut out, big, *kind);
            put32(&mut out, big, count);
            if let Some(at) = place {
                put32(&mut out, big, u32::try_from(*at).unwrap_or(0));
            } else {
                let mut inline = values;
                inline.resize(4, 0);
                out.extend_from_slice(&inline);
            }
        }
        put32(&mut out, big, u32::try_from(next).unwrap_or(0));
        for ((tag, kind, values), place) in tags.iter().zip(&places) {
            if place.is_none() {
                continue;
            }
            let values = if *tag == offsets_tag && !page.units.is_empty() {
                raw(LONG, &unit_offsets)
            } else {
                values.clone()
            };
            out.extend_from_slice(&ordered(big, *kind, &values));
        }
        for unit in &page.units {
            out.extend_from_slice(unit);
        }
    }
    out
}

/// A page of `width` by `height` 8-bit grey pixels in one uncompressed
/// strip.
fn grey(width: u32, height: u32, pixels: Vec<u8>) -> PageSpec {
    PageSpec::new()
        .tag(TAG_IMAGE_WIDTH, LONG, &[width])
        .tag(TAG_IMAGE_LENGTH, LONG, &[height])
        .tag(TAG_BITS_PER_SAMPLE, SHORT, &[8])
        .tag(TAG_COMPRESSION, SHORT, &[u32::from(COMPRESSION_NONE)])
        .tag(TAG_PHOTOMETRIC, SHORT, &[u32::from(PHOTOMETRIC_BLACK_ZERO)])
        .tag(TAG_SAMPLES_PER_PIXEL, SHORT, &[1])
        .tag(TAG_ROWS_PER_STRIP, LONG, &[height])
        .unit(pixels)
}

/// A page of `width` by `height` 8-bit RGB pixels in one uncompressed
/// strip.
fn rgb(width: u32, height: u32, pixels: Vec<u8>) -> PageSpec {
    PageSpec::new()
        .tag(TAG_IMAGE_WIDTH, LONG, &[width])
        .tag(TAG_IMAGE_LENGTH, LONG, &[height])
        .tag(TAG_BITS_PER_SAMPLE, SHORT, &[8, 8, 8])
        .tag(TAG_COMPRESSION, SHORT, &[u32::from(COMPRESSION_NONE)])
        .tag(TAG_PHOTOMETRIC, SHORT, &[u32::from(PHOTOMETRIC_RGB)])
        .tag(TAG_SAMPLES_PER_PIXEL, SHORT, &[3])
        .tag(TAG_ROWS_PER_STRIP, LONG, &[height])
        .unit(pixels)
}

fn pixel(image: &RasterImage, x: u32, y: u32) -> [u8; 4] {
    let at = usize::try_from((y * image.width() + x) * 4).unwrap_or(0);
    let slice = &image.pixels()[at..at + 4];
    [slice[0], slice[1], slice[2], slice[3]]
}

fn decoded(page: PageSpec) -> RasterImage {
    let bytes = build(false, &[page]);
    decode(&bytes, &limits()).expect("a valid fixture decodes")
}

fn refusal(page: PageSpec) -> DecodeError {
    let bytes = build(false, &[page]);
    decode(&bytes, &limits()).expect_err("an invalid fixture is refused")
}

#[test]
fn a_greyscale_strip_decodes_in_either_byte_order() {
    for big in [false, true] {
        let bytes = build(big, &[grey(2, 2, vec![0, 64, 128, 255])]);
        assert_eq!(crate::sniff(&bytes), Some(ImageFormat::Tiff));
        assert_eq!(probe(&bytes), Ok((2, 2)));
        let image = decode(&bytes, &limits()).expect("a greyscale strip decodes");
        assert_eq!((image.width(), image.height()), (2, 2));
        assert_eq!(pixel(&image, 0, 0), [0, 0, 0, 255]);
        assert_eq!(pixel(&image, 1, 0), [64, 64, 64, 255]);
        assert_eq!(pixel(&image, 0, 1), [128, 128, 128, 255]);
        assert_eq!(pixel(&image, 1, 1), [255, 255, 255, 255]);
    }
}

#[test]
fn white_is_zero_inverts_its_samples() {
    let image = decoded(grey(2, 1, vec![0, 255]).tag(
        TAG_PHOTOMETRIC,
        SHORT,
        &[u32::from(PHOTOMETRIC_WHITE_ZERO)],
    ));
    assert_eq!(pixel(&image, 0, 0), [255, 255, 255, 255]);
    assert_eq!(pixel(&image, 1, 0), [0, 0, 0, 255]);
}

#[test]
fn an_rgb_strip_decodes_its_three_samples() {
    let image = decoded(rgb(2, 1, vec![255, 0, 0, 0, 128, 255]));
    assert_eq!(pixel(&image, 0, 0), [255, 0, 0, 255]);
    assert_eq!(pixel(&image, 1, 0), [0, 128, 255, 255]);
}

// --- Codec fixtures -------------------------------------------------------

/// Pack a bit string, most significant bit first, padding the last byte
/// with zeros. Written out as its own bits so a fixture states the codes
/// the specification gives rather than re-deriving them.
fn bits(spec: &str) -> Vec<u8> {
    let mut out = Vec::new();
    let mut byte = 0u8;
    let mut held = 0u32;
    for symbol in spec
        .chars()
        .filter(|symbol| *symbol == '0' || *symbol == '1')
    {
        byte = byte << 1 | u8::from(symbol == '1');
        held += 1;
        if held == 8 {
            out.push(byte);
            byte = 0;
            held = 0;
        }
    }
    if held > 0 {
        out.push(byte << (8 - held));
    }
    out
}

/// `PackBits`-encode `data` as one literal run per 128 bytes, then a
/// repeat run for any tail of equal bytes, so both control forms appear.
fn pack_bits(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut at = 0usize;
    while at < data.len() {
        let byte = data[at];
        let run = data[at..].iter().take_while(|next| **next == byte).count();
        if run >= 3 {
            out.push(u8::try_from(257 - run.min(128)).unwrap_or(0));
            out.push(byte);
            at += run.min(128);
        } else {
            let literal = data[at..].len().min(128);
            out.push(u8::try_from(literal - 1).unwrap_or(0));
            out.extend_from_slice(&data[at..at + literal]);
            at += literal;
        }
    }
    out
}

fn adler32(data: &[u8]) -> u32 {
    let (mut a, mut b) = (1u32, 0u32);
    for &byte in data {
        a = (a + u32::from(byte)) % 65521;
        b = (b + a) % 65521;
    }
    b << 16 | a
}

/// Wrap `data` as a zlib stream of stored DEFLATE blocks, which is what
/// TIFF's Deflate compression carries.
fn zlib(data: &[u8]) -> Vec<u8> {
    let mut out = vec![0x78, 0x01];
    let mut at = 0usize;
    loop {
        let take = (data.len() - at).min(0xFFFF);
        let final_block = at + take == data.len();
        out.push(u8::from(final_block));
        let len = u16::try_from(take).unwrap_or(0);
        out.extend_from_slice(&len.to_le_bytes());
        out.extend_from_slice(&(!len).to_le_bytes());
        out.extend_from_slice(&data[at..at + take]);
        at += take;
        if final_block {
            break;
        }
    }
    out.extend_from_slice(&adler32(data).to_be_bytes());
    out
}

/// An LZW encoder for either dialect.
///
/// The matching table runs one entry ahead of the decoder's, which is what
/// classic LZW is; the *width* schedule follows the decoder's table, since
/// that is the one both sides must agree on.
struct Lzw {
    out: Vec<u8>,
    held: u32,
    accumulator: u32,
    lsb_first: bool,
    early: bool,
    width: u32,
    decoder_next: u16,
    since_clear: u32,
}

impl Lzw {
    fn new(classic: bool) -> Self {
        Self {
            out: Vec::new(),
            held: 0,
            accumulator: 0,
            lsb_first: classic,
            early: !classic,
            width: 9,
            decoder_next: 258,
            since_clear: 0,
        }
    }

    /// The clear code, read at the width in force and resetting it.
    fn clear(&mut self) {
        self.pack(256);
        self.width = 9;
        self.decoder_next = 258;
        self.since_clear = 0;
    }

    /// A data code, which grows the decoder's table from the second one on.
    fn code(&mut self, code: u16) {
        self.pack(code);
        self.since_clear += 1;
        if self.since_clear >= 2 {
            self.decoder_next += 1;
            let reached = if self.early {
                u32::from(self.decoder_next) + 1 >= 1 << self.width
            } else {
                u32::from(self.decoder_next) >= 1 << self.width
            };
            if reached && self.width < 12 {
                self.width += 1;
            }
        }
    }

    fn pack(&mut self, code: u16) {
        if self.lsb_first {
            self.accumulator |= u32::from(code) << self.held;
            self.held += self.width;
            while self.held >= 8 {
                self.out
                    .push(u8::try_from(self.accumulator & 0xFF).unwrap_or(0));
                self.accumulator >>= 8;
                self.held -= 8;
            }
        } else {
            self.accumulator = self.accumulator << self.width | u32::from(code);
            self.held += self.width;
            while self.held >= 8 {
                self.held -= 8;
                self.out
                    .push(u8::try_from(self.accumulator >> self.held & 0xFF).unwrap_or(0));
            }
        }
    }

    fn finish(mut self) -> Vec<u8> {
        if self.held > 0 {
            if self.lsb_first {
                self.out
                    .push(u8::try_from(self.accumulator & 0xFF).unwrap_or(0));
            } else {
                self.out
                    .push(u8::try_from(self.accumulator << (8 - self.held) & 0xFF).unwrap_or(0));
            }
        }
        self.out
    }
}

fn lzw(data: &[u8], classic: bool) -> Vec<u8> {
    let mut stream = Lzw::new(classic);
    stream.clear();
    let mut table: Vec<((u16, u8), u16)> = Vec::new();
    let mut next = 258u16;
    let Some((&first, rest)) = data.split_first() else {
        stream.code(257);
        return stream.finish();
    };
    let mut current = u16::from(first);
    for &byte in rest {
        if let Some(&(_, code)) = table.iter().find(|&&(key, _)| key == (current, byte)) {
            current = code;
            continue;
        }
        stream.code(current);
        if next < 4094 {
            table.push(((current, byte), next));
            next += 1;
        }
        current = u16::from(byte);
    }
    stream.code(current);
    stream.pack(257);
    stream.finish()
}

/// A baseline greyscale JPEG whose every coefficient is zero, so it decodes
/// to flat mid-grey and states nothing about entropy coding this crate does
/// not already test elsewhere.
fn tiny_jpeg(width: u32, height: u32) -> Vec<u8> {
    let mut out = vec![0xFF, 0xD8];
    let segment = |code: u8, payload: &[u8], out: &mut Vec<u8>| {
        out.extend_from_slice(&[0xFF, code]);
        out.extend_from_slice(&u16::try_from(payload.len() + 2).unwrap_or(0).to_be_bytes());
        out.extend_from_slice(payload);
    };
    let mut quantisation = vec![0u8];
    quantisation.extend_from_slice(&[0x10; 64]);
    segment(0xDB, &quantisation, &mut out);
    let mut frame = vec![8];
    frame.extend_from_slice(&u16::try_from(height).unwrap_or(0).to_be_bytes());
    frame.extend_from_slice(&u16::try_from(width).unwrap_or(0).to_be_bytes());
    frame.extend_from_slice(&[1, 1, 0x11, 0]);
    segment(0xC0, &frame, &mut out);
    for class in [0x00u8, 0x10] {
        let mut table = vec![class, 1];
        table.extend_from_slice(&[0u8; 15]);
        table.push(0);
        segment(0xC4, &table, &mut out);
    }
    segment(0xDA, &[1, 1, 0x00, 0, 63, 0], &mut out);
    // One `0` bit for the zero DC category and one for end-of-block, per
    // block, then a padding of set bits.
    let blocks = width.div_ceil(8) * height.div_ceil(8);
    let entropy = usize::try_from(u64::from(blocks) * 2).unwrap_or(0);
    let mut spec = "0".repeat(entropy);
    while !spec.len().is_multiple_of(8) {
        spec.push('1');
    }
    out.extend_from_slice(&bits(&spec));
    out.extend_from_slice(&[0xFF, 0xD9]);
    out
}

// --- Colour and sample layout --------------------------------------------

#[test]
fn every_unsigned_bit_depth_widens_to_eight_bits() {
    // One black pixel and one white one, at each depth the format lists.
    for (bits_per_sample, pixels) in [
        (1u32, vec![0b0100_0000]),
        (2, vec![0b0011_0000]),
        (4, vec![0x0F]),
        (8, vec![0, 255]),
        (16, vec![0, 0, 255, 255]),
        (32, vec![0, 0, 0, 0, 255, 255, 255, 255]),
    ] {
        let page = grey(2, 1, pixels).tag(TAG_BITS_PER_SAMPLE, SHORT, &[bits_per_sample]);
        let image = decoded(page);
        assert_eq!(
            pixel(&image, 0, 0),
            [0, 0, 0, 255],
            "depth {bits_per_sample}"
        );
        assert_eq!(
            pixel(&image, 1, 0),
            [255, 255, 255, 255],
            "depth {bits_per_sample}"
        );
    }
}

#[test]
fn a_signed_sample_is_read_as_its_offset_range() {
    let page = grey(3, 1, vec![0x80, 0x00, 0x7F]).tag(TAG_SAMPLE_FORMAT, SHORT, &[2]);
    let image = decoded(page);
    assert_eq!(pixel(&image, 0, 0)[0], 0);
    assert_eq!(pixel(&image, 1, 0)[0], 128);
    assert_eq!(pixel(&image, 2, 0)[0], 255);
}

#[test]
fn a_float_sample_scales_onto_the_unit_interval() {
    let single = grey(3, 1, vec![0, 0, 0, 0, 0, 0, 0, 0x3F, 0, 0, 0x80, 0x3F])
        .tag(TAG_BITS_PER_SAMPLE, SHORT, &[32])
        .tag(TAG_SAMPLE_FORMAT, SHORT, &[3]);
    let image = decoded(single);
    assert_eq!(pixel(&image, 0, 0)[0], 0);
    assert_eq!(pixel(&image, 1, 0)[0], 128);
    assert_eq!(pixel(&image, 2, 0)[0], 255);

    let half = grey(3, 1, vec![0, 0, 0, 0x38, 0, 0x3C])
        .tag(TAG_BITS_PER_SAMPLE, SHORT, &[16])
        .tag(TAG_SAMPLE_FORMAT, SHORT, &[3]);
    let image = decoded(half);
    assert_eq!(pixel(&image, 0, 0)[0], 0);
    assert_eq!(pixel(&image, 1, 0)[0], 128);
    assert_eq!(pixel(&image, 2, 0)[0], 255);
}

#[test]
fn a_float_sample_outside_the_unit_interval_clamps() {
    // Negative, then two, then a quiet not-a-number.
    let page = grey(3, 1, vec![0, 0, 0, 0xBF, 0, 0, 0, 0x40, 0, 0, 0xC0, 0x7F])
        .tag(TAG_BITS_PER_SAMPLE, SHORT, &[32])
        .tag(TAG_SAMPLE_FORMAT, SHORT, &[3]);
    let image = decoded(page);
    assert_eq!(pixel(&image, 0, 0)[0], 0);
    assert_eq!(pixel(&image, 1, 0)[0], 255);
    assert_eq!(pixel(&image, 2, 0)[0], 0);
}

#[test]
fn a_palette_page_reads_its_colour_map() {
    let map = vec![
        0, 0xFFFF, 0, 0, // red
        0, 0, 0, 0xFFFF, // green
        0xFFFF, 0, 0, 0, // blue
    ];
    let page = PageSpec::new()
        .tag(TAG_IMAGE_WIDTH, LONG, &[4])
        .tag(TAG_IMAGE_LENGTH, LONG, &[1])
        .tag(TAG_BITS_PER_SAMPLE, SHORT, &[2])
        .tag(TAG_COMPRESSION, SHORT, &[u32::from(COMPRESSION_NONE)])
        .tag(TAG_PHOTOMETRIC, SHORT, &[u32::from(PHOTOMETRIC_PALETTE)])
        .tag(TAG_SAMPLES_PER_PIXEL, SHORT, &[1])
        .tag(TAG_ROWS_PER_STRIP, LONG, &[1])
        .tag(TAG_COLOUR_MAP, SHORT, &map)
        .unit(vec![0b0001_1011]);
    let image = decoded(page);
    assert_eq!(pixel(&image, 0, 0), [0, 0, 255, 255]);
    assert_eq!(pixel(&image, 1, 0), [255, 0, 0, 255]);
    assert_eq!(pixel(&image, 2, 0), [0, 0, 0, 255]);
    assert_eq!(pixel(&image, 3, 0), [0, 255, 0, 255]);
}

#[test]
fn a_transparency_mask_draws_its_interior_and_nothing_else() {
    let page = PageSpec::new()
        .tag(TAG_IMAGE_WIDTH, LONG, &[2])
        .tag(TAG_IMAGE_LENGTH, LONG, &[1])
        .tag(TAG_BITS_PER_SAMPLE, SHORT, &[1])
        .tag(TAG_COMPRESSION, SHORT, &[u32::from(COMPRESSION_NONE)])
        .tag(TAG_PHOTOMETRIC, SHORT, &[u32::from(PHOTOMETRIC_MASK)])
        .tag(TAG_SAMPLES_PER_PIXEL, SHORT, &[1])
        .tag(TAG_ROWS_PER_STRIP, LONG, &[1])
        .unit(vec![0b0100_0000]);
    let image = decoded(page);
    assert_eq!(pixel(&image, 0, 0), [0, 0, 0, 0]);
    assert_eq!(pixel(&image, 1, 0), [0, 0, 0, 255]);
}

#[test]
fn a_separated_page_converts_its_inks() {
    let page = PageSpec::new()
        .tag(TAG_IMAGE_WIDTH, LONG, &[2])
        .tag(TAG_IMAGE_LENGTH, LONG, &[1])
        .tag(TAG_BITS_PER_SAMPLE, SHORT, &[8, 8, 8, 8])
        .tag(TAG_COMPRESSION, SHORT, &[u32::from(COMPRESSION_NONE)])
        .tag(TAG_PHOTOMETRIC, SHORT, &[u32::from(PHOTOMETRIC_SEPARATED)])
        .tag(TAG_SAMPLES_PER_PIXEL, SHORT, &[4])
        .tag(TAG_ROWS_PER_STRIP, LONG, &[1])
        .unit(vec![0, 0, 0, 0, 255, 0, 0, 0]);
    let image = decoded(page);
    assert_eq!(pixel(&image, 0, 0), [255, 255, 255, 255]);
    assert_eq!(pixel(&image, 1, 0), [0, 255, 255, 255]);
}

/// A `YCbCr` page at the subsampling given, with the chrominance pair each
/// block shares appended to its luminance.
fn ycbcr(horizontal: u32, vertical: u32, unit: Vec<u8>) -> PageSpec {
    PageSpec::new()
        .tag(TAG_IMAGE_WIDTH, LONG, &[2])
        .tag(TAG_IMAGE_LENGTH, LONG, &[2])
        .tag(TAG_BITS_PER_SAMPLE, SHORT, &[8, 8, 8])
        .tag(TAG_COMPRESSION, SHORT, &[u32::from(COMPRESSION_NONE)])
        .tag(TAG_PHOTOMETRIC, SHORT, &[u32::from(PHOTOMETRIC_YCBCR)])
        .tag(TAG_SAMPLES_PER_PIXEL, SHORT, &[3])
        .tag(TAG_ROWS_PER_STRIP, LONG, &[2])
        .tag(TAG_YCBCR_SUBSAMPLING, SHORT, &[horizontal, vertical])
        .unit(unit)
}

#[test]
fn an_unsubsampled_ycbcr_page_converts_to_rgb() {
    let unit = vec![0, 128, 128, 255, 128, 128, 128, 128, 128, 76, 84, 255];
    let image = decoded(ycbcr(1, 1, unit));
    assert_eq!(pixel(&image, 0, 0), [0, 0, 0, 255]);
    assert_eq!(pixel(&image, 1, 0), [255, 255, 255, 255]);
    assert_eq!(pixel(&image, 0, 1), [128, 128, 128, 255]);
    // The luma weights make a pure red exactly this triple.
    let red = pixel(&image, 1, 1);
    assert!(red[0] > 250 && red[1] < 6 && red[2] < 6, "{red:?}");
}

#[test]
fn a_subsampled_ycbcr_page_shares_one_chrominance_pair() {
    let image = decoded(ycbcr(2, 2, vec![0, 255, 255, 0, 128, 128]));
    assert_eq!(pixel(&image, 0, 0), [0, 0, 0, 255]);
    assert_eq!(pixel(&image, 1, 0), [255, 255, 255, 255]);
    assert_eq!(pixel(&image, 0, 1), [255, 255, 255, 255]);
    assert_eq!(pixel(&image, 1, 1), [0, 0, 0, 255]);
}

#[test]
fn a_reference_range_rescales_the_coded_channels() {
    // The studio-swing range, where 16 is black and 235 is white.
    let page = ycbcr(
        1,
        1,
        vec![16, 128, 128, 235, 128, 128, 16, 128, 128, 16, 128, 128],
    )
    .tag(
        TAG_REFERENCE_BLACK_WHITE,
        RATIONAL,
        &[16, 1, 235, 1, 128, 1, 240, 1, 128, 1, 240, 1],
    );
    let image = decoded(page);
    assert_eq!(pixel(&image, 0, 0), [0, 0, 0, 255]);
    assert_eq!(pixel(&image, 1, 0), [255, 255, 255, 255]);
}

#[test]
fn an_extra_sample_carries_alpha_either_premultiplied_or_not() {
    let unassociated = rgb(1, 1, vec![255, 0, 0, 128])
        .tag(TAG_SAMPLES_PER_PIXEL, SHORT, &[4])
        .tag(TAG_BITS_PER_SAMPLE, SHORT, &[8, 8, 8, 8])
        .tag(TAG_EXTRA_SAMPLES, SHORT, &[2]);
    assert_eq!(pixel(&decoded(unassociated), 0, 0), [255, 0, 0, 128]);

    let associated = rgb(1, 1, vec![128, 0, 0, 128])
        .tag(TAG_SAMPLES_PER_PIXEL, SHORT, &[4])
        .tag(TAG_BITS_PER_SAMPLE, SHORT, &[8, 8, 8, 8])
        .tag(TAG_EXTRA_SAMPLES, SHORT, &[1]);
    assert_eq!(pixel(&decoded(associated), 0, 0), [255, 0, 0, 128]);

    let transparent = rgb(1, 1, vec![0, 0, 0, 0])
        .tag(TAG_SAMPLES_PER_PIXEL, SHORT, &[4])
        .tag(TAG_BITS_PER_SAMPLE, SHORT, &[8, 8, 8, 8])
        .tag(TAG_EXTRA_SAMPLES, SHORT, &[1]);
    assert_eq!(pixel(&decoded(transparent), 0, 0), [0, 0, 0, 0]);
}

// --- Storage arrangement --------------------------------------------------

#[test]
fn several_strips_cover_the_picture_with_a_short_last_one() {
    let page = grey(2, 3, Vec::new())
        .tag(TAG_ROWS_PER_STRIP, LONG, &[2])
        .units(vec![vec![1, 2, 3, 4], vec![5, 6]]);
    let image = decoded(page);
    assert_eq!((image.width(), image.height()), (2, 3));
    for (index, expected) in [1u8, 2, 3, 4, 5, 6].into_iter().enumerate() {
        let index = u32::try_from(index).unwrap_or(0);
        assert_eq!(pixel(&image, index % 2, index / 2)[0], expected);
    }
}

#[test]
fn a_tiled_page_clips_its_edge_tiles() {
    // Two tiles across and one down, the second covering only four columns.
    let full: Vec<u8> = (0..16 * 16)
        .map(|at| u8::try_from(at % 251).unwrap_or(0))
        .collect();
    let page = PageSpec::new()
        .tag(TAG_IMAGE_WIDTH, LONG, &[20])
        .tag(TAG_IMAGE_LENGTH, LONG, &[16])
        .tag(TAG_BITS_PER_SAMPLE, SHORT, &[8])
        .tag(TAG_COMPRESSION, SHORT, &[u32::from(COMPRESSION_NONE)])
        .tag(TAG_PHOTOMETRIC, SHORT, &[u32::from(PHOTOMETRIC_BLACK_ZERO)])
        .tag(TAG_SAMPLES_PER_PIXEL, SHORT, &[1])
        .tag(TAG_TILE_WIDTH, LONG, &[16])
        .tag(TAG_TILE_LENGTH, LONG, &[16])
        .tiled()
        .units(vec![full.clone(), vec![9u8; 16 * 16]]);
    let image = decoded(page);
    assert_eq!((image.width(), image.height()), (20, 16));
    assert_eq!(pixel(&image, 0, 0)[0], full[0]);
    assert_eq!(pixel(&image, 15, 3)[0], full[3 * 16 + 15]);
    assert_eq!(pixel(&image, 16, 0)[0], 9);
    assert_eq!(pixel(&image, 19, 15)[0], 9);
}

#[test]
fn a_planar_page_gathers_one_sample_from_each_plane() {
    let page = rgb(2, 1, Vec::new())
        .tag(TAG_PLANAR_CONFIGURATION, SHORT, &[2])
        .units(vec![vec![255, 0], vec![0, 128], vec![0, 255]]);
    let image = decoded(page);
    assert_eq!(pixel(&image, 0, 0), [255, 0, 0, 255]);
    assert_eq!(pixel(&image, 1, 0), [0, 128, 255, 255]);
}

#[test]
fn every_orientation_places_the_stored_raster() {
    let stored = vec![10u8, 20, 30, 40];
    let expected = [
        [10u8, 20, 30, 40],
        [20, 10, 40, 30],
        [40, 30, 20, 10],
        [30, 40, 10, 20],
        [10, 30, 20, 40],
        [30, 10, 40, 20],
        [40, 20, 30, 10],
        [20, 40, 10, 30],
    ];
    for (index, want) in expected.iter().enumerate() {
        let orientation = u32::try_from(index).unwrap_or(0) + 1;
        let image = decoded(grey(2, 2, stored.clone()).tag(TAG_ORIENTATION, SHORT, &[orientation]));
        let got: Vec<u8> = (0..4).map(|at| pixel(&image, at % 2, at / 2)[0]).collect();
        assert_eq!(&got[..], &want[..], "orientation {orientation}");
    }
}

#[test]
fn a_transposing_orientation_swaps_the_reported_geometry() {
    let page = grey(4, 2, vec![0u8; 8]).tag(TAG_ORIENTATION, SHORT, &[6]);
    let bytes = build(false, &[page]);
    assert_eq!(probe(&bytes), Ok((2, 4)));
    let image = decode(&bytes, &limits()).expect("a rotated page decodes");
    assert_eq!((image.width(), image.height()), (2, 4));
}

// --- Predictors -----------------------------------------------------------

#[test]
fn the_horizontal_predictor_accumulates_along_each_row() {
    for (bits_per_sample, stored, expected) in [
        (
            8u32,
            vec![10u8, 20, 30, 5, 5, 5],
            [10u8, 20, 30, 15, 25, 35],
        ),
        (
            16,
            vec![0, 10, 0, 20, 0, 30, 0, 5, 0, 5, 0, 5],
            [10, 20, 30, 15, 25, 35],
        ),
    ] {
        let page = rgb(2, 1, stored)
            .tag(TAG_BITS_PER_SAMPLE, SHORT, &[bits_per_sample; 3])
            .tag(TAG_PREDICTOR, SHORT, &[2]);
        let image = decoded(page);
        let first = pixel(&image, 0, 0);
        let second = pixel(&image, 1, 0);
        assert_eq!(
            [first[0], first[1], first[2], second[0], second[1], second[2]],
            expected,
            "depth {bits_per_sample}"
        );
    }
}

#[test]
fn the_floating_point_predictor_unshuffles_its_byte_planes() {
    let page = grey(2, 1, vec![0x3F, 0x00, 0xC1, 0x80, 0x80, 0x00, 0x00, 0x00])
        .tag(TAG_BITS_PER_SAMPLE, SHORT, &[32])
        .tag(TAG_SAMPLE_FORMAT, SHORT, &[3])
        .tag(TAG_PREDICTOR, SHORT, &[3]);
    let image = decoded(page);
    assert_eq!(pixel(&image, 0, 0)[0], 128);
    assert_eq!(pixel(&image, 1, 0)[0], 255);
}

// --- Compressions ---------------------------------------------------------

#[test]
fn pack_bits_expands_its_literal_and_repeat_runs() {
    let mut pixels = vec![1u8, 2, 3];
    pixels.extend_from_slice(&[7u8; 13]);
    let page = grey(16, 1, pack_bits(&pixels)).tag(
        TAG_COMPRESSION,
        SHORT,
        &[u32::from(COMPRESSION_PACK_BITS)],
    );
    let image = decoded(page);
    for (at, expected) in pixels.iter().enumerate() {
        assert_eq!(
            pixel(&image, u32::try_from(at).unwrap_or(0), 0)[0],
            *expected
        );
    }
}

#[test]
fn deflate_expands_its_zlib_stream() {
    let pixels: Vec<u8> = (0..64)
        .map(|at| u8::try_from(at * 3 % 251).unwrap_or(0))
        .collect();
    let page = grey(64, 1, zlib(&pixels)).tag(
        TAG_COMPRESSION,
        SHORT,
        &[u32::from(COMPRESSION_ADOBE_DEFLATE)],
    );
    let image = decoded(page);
    for (at, expected) in pixels.iter().enumerate() {
        assert_eq!(
            pixel(&image, u32::try_from(at).unwrap_or(0), 0)[0],
            *expected
        );
    }
}

#[test]
fn both_lzw_dialects_round_trip_across_a_code_width_step() {
    // Long enough that the table crosses the width the two dialects step at
    // differently, which is the whole difference between them.
    let pixels: Vec<u8> = (0..3000u32)
        .map(|at| u8::try_from(at.wrapping_mul(37) >> 3 & 0xFF).unwrap_or(0))
        .collect();
    for classic in [false, true] {
        let page = grey(3000, 1, lzw(&pixels, classic)).tag(
            TAG_COMPRESSION,
            SHORT,
            &[u32::from(COMPRESSION_LZW)],
        );
        let image = decoded(page);
        let decoded: Vec<u8> = (0..3000).map(|at| pixel(&image, at, 0)[0]).collect();
        assert_eq!(decoded, pixels, "classic dialect: {classic}");
    }
}

#[test]
fn a_jpeg_unit_decodes_through_the_shared_decoder() {
    let page =
        grey(8, 8, tiny_jpeg(8, 8)).tag(TAG_COMPRESSION, SHORT, &[u32::from(COMPRESSION_JPEG)]);
    let image = decoded(page);
    assert_eq!((image.width(), image.height()), (8, 8));
    assert!(image
        .pixels()
        .chunks(4)
        .all(|px| px == [128, 128, 128, 255]));
}

#[test]
fn a_jpeg_unit_splices_the_tables_the_directory_holds_apart() {
    let whole = tiny_jpeg(8, 8);
    // Everything up to the frame header is table data; the rest is the
    // abbreviated stream a real writer would store per strip.
    let split = whole
        .windows(2)
        .position(|pair| pair == [0xFF, 0xC0])
        .expect("the fixture carries a frame header");
    let mut tables = whole[..split].to_vec();
    tables.extend_from_slice(&[0xFF, 0xD9]);
    let mut unit = vec![0xFF, 0xD8];
    unit.extend_from_slice(&whole[split..]);
    let page = grey(8, 8, unit)
        .tag(TAG_COMPRESSION, SHORT, &[u32::from(COMPRESSION_JPEG)])
        .bytes(TAG_JPEG_TABLES, UNDEFINED, tables);
    let image = decoded(page);
    assert!(image
        .pixels()
        .chunks(4)
        .all(|px| px == [128, 128, 128, 255]));
}

// --- Facsimile compressions ----------------------------------------------

/// A one-bit fax page of `width` by `height`, under `compression`.
fn fax(compression: u16, width: u32, height: u32, data: Vec<u8>) -> PageSpec {
    PageSpec::new()
        .tag(TAG_IMAGE_WIDTH, LONG, &[width])
        .tag(TAG_IMAGE_LENGTH, LONG, &[height])
        .tag(TAG_BITS_PER_SAMPLE, SHORT, &[1])
        .tag(TAG_COMPRESSION, SHORT, &[u32::from(compression)])
        .tag(TAG_SAMPLES_PER_PIXEL, SHORT, &[1])
        .tag(TAG_ROWS_PER_STRIP, LONG, &[height])
        .unit(data)
}

/// The eight pixels a fax row of four white then four black draws, under
/// the white-is-zero photometric every fax declares.
const HALF_ROW: [[u8; 4]; 8] = [
    [255, 255, 255, 255],
    [255, 255, 255, 255],
    [255, 255, 255, 255],
    [255, 255, 255, 255],
    [0, 0, 0, 255],
    [0, 0, 0, 255],
    [0, 0, 0, 255],
    [0, 0, 0, 255],
];

#[test]
fn modified_huffman_rows_start_on_a_byte_boundary() {
    // White 4 is `1011` and black 4 is `011`; an all-white row of eight is
    // the white terminating code for 8, `10011`.
    let page = fax(
        COMPRESSION_CCITT_RLE,
        8,
        2,
        [bits("1011 011"), bits("10011")].concat(),
    );
    let image = decoded(page);
    for (at, expected) in HALF_ROW.iter().enumerate() {
        assert_eq!(pixel(&image, u32::try_from(at).unwrap_or(0), 0), *expected);
    }
    for at in 0..8 {
        assert_eq!(pixel(&image, at, 1), [255, 255, 255, 255]);
    }
}

#[test]
fn a_makeup_code_extends_a_run_past_sixty_three() {
    // White makeup 64 is `11011`, then the terminating code for 0.
    let page = fax(COMPRESSION_CCITT_RLE, 64, 1, bits("11011 00110101"));
    let image = decoded(page);
    for at in 0..64 {
        assert_eq!(pixel(&image, at, 0), [255, 255, 255, 255]);
    }
}

#[test]
fn group3_rows_are_found_behind_their_end_of_line_codes() {
    let page = fax(COMPRESSION_GROUP3, 8, 1, bits("000000000001 1011 011"));
    let image = decoded(page);
    for (at, expected) in HALF_ROW.iter().enumerate() {
        assert_eq!(pixel(&image, u32::try_from(at).unwrap_or(0), 0), *expected);
    }
}

#[test]
fn group3_two_dimensional_rows_are_tagged_after_each_end_of_line() {
    // A one-dimensional first row, then a two-dimensional row identical to
    // it: two vertical-zero codes against the reference line.
    let page = fax(
        COMPRESSION_GROUP3,
        8,
        2,
        bits("000000000001 1 1011 011 000000000001 0 1 1"),
    )
    .tag(TAG_T4_OPTIONS, LONG, &[1]);
    let image = decoded(page);
    for row in 0..2 {
        for (at, expected) in HALF_ROW.iter().enumerate() {
            assert_eq!(
                pixel(&image, u32::try_from(at).unwrap_or(0), row),
                *expected,
                "row {row}"
            );
        }
    }
}

#[test]
fn group4_codes_every_row_against_its_predecessor() {
    // The first row has an imaginary all-white reference, so it needs a
    // horizontal code; the second matches it exactly, so two vertical-zero
    // codes suffice.
    let page = fax(COMPRESSION_GROUP4, 8, 2, bits("001 1011 011 1 1"));
    let image = decoded(page);
    for row in 0..2 {
        for (at, expected) in HALF_ROW.iter().enumerate() {
            assert_eq!(
                pixel(&image, u32::try_from(at).unwrap_or(0), row),
                *expected,
                "row {row}"
            );
        }
    }
}

#[test]
fn group4_pass_and_vertical_modes_shift_a_run() {
    // Row one: four white then four black. Row two: a pass over the
    // reference's black run start, then the rest.
    let page = fax(COMPRESSION_GROUP4, 8, 2, bits("001 1011 011 0001 1"));
    let image = decoded(page);
    for at in 0..8 {
        assert_eq!(pixel(&image, at, 1), [255, 255, 255, 255], "column {at}");
    }
}

#[test]
fn a_fax_reads_its_bits_least_significant_first_when_asked() {
    let forward = bits("1011 011");
    let reversed: Vec<u8> = forward.iter().map(|byte| byte.reverse_bits()).collect();
    let page = fax(COMPRESSION_CCITT_RLE, 8, 1, reversed).tag(TAG_FILL_ORDER, SHORT, &[2]);
    let image = decoded(page);
    for (at, expected) in HALF_ROW.iter().enumerate() {
        assert_eq!(pixel(&image, u32::try_from(at).unwrap_or(0), 0), *expected);
    }
}

#[test]
fn a_fax_with_no_photometric_reads_as_white_is_zero() {
    let page = fax(COMPRESSION_GROUP4, 8, 1, bits("001 1011 011"));
    let image = decoded(page);
    assert_eq!(pixel(&image, 0, 0), [255, 255, 255, 255]);
    assert_eq!(pixel(&image, 7, 0), [0, 0, 0, 255]);
}

#[test]
fn uncompressed_mode_is_refused_by_name() {
    let page = fax(COMPRESSION_GROUP4, 8, 1, bits("0000001 111"));
    assert_eq!(refusal(page), DecodeError::TiffFaxUncompressedMode);
}

#[test]
fn a_fax_row_that_overruns_its_width_is_refused() {
    // White 64 twice over an eight-pixel row.
    let page = fax(COMPRESSION_CCITT_RLE, 8, 1, bits("11011 00110101"));
    assert_eq!(refusal(page), DecodeError::TiffFaxRowOverflow);
}

#[test]
fn a_fax_that_ends_before_its_last_row_is_refused() {
    let page = fax(COMPRESSION_GROUP4, 8, 4, bits("001 1011 011 1 1"));
    assert_eq!(refusal(page), DecodeError::TiffFaxTruncated);
}

#[test]
fn a_two_dimensional_row_without_its_end_of_line_is_refused() {
    let page = fax(
        COMPRESSION_GROUP3,
        8,
        2,
        bits("000000000001 1 1011 011 1 1"),
    )
    .tag(TAG_T4_OPTIONS, LONG, &[1]);
    assert_eq!(refusal(page), DecodeError::TiffFaxMissingSync);
}

// --- Pages ----------------------------------------------------------------

#[test]
fn a_plain_decode_skips_a_page_the_file_calls_a_reduced_copy() {
    let thumbnail = grey(1, 1, vec![7]).tag(TAG_NEW_SUBFILE_TYPE, LONG, &[1]);
    let full = grey(2, 1, vec![1, 2]);
    let bytes = build(false, &[thumbnail, full]);
    assert_eq!(probe(&bytes), Ok((2, 1)));
    let image = decode(&bytes, &limits()).expect("the full page decodes");
    assert_eq!(pixel(&image, 0, 0)[0], 1);
}

#[test]
fn a_plain_decode_answers_the_first_page_of_a_document() {
    // The second page is larger, which an icon file would answer with and a
    // document must not.
    let bytes = build(
        false,
        &[grey(2, 1, vec![1, 2]), grey(4, 1, vec![3, 4, 5, 6])],
    );
    assert_eq!(probe(&bytes), Ok((2, 1)));
    let image = decode(&bytes, &limits()).expect("the first page decodes");
    assert_eq!(pixel(&image, 0, 0)[0], 1);
}

#[test]
fn a_sequence_walks_every_page_and_addresses_them() {
    let bytes = build(
        false,
        &[grey(2, 1, vec![1, 2]), grey(4, 1, vec![3, 4, 5, 6])],
    );
    let mut sequence = Sequence::open(&bytes, &limits()).expect("a document opens as a sequence");
    let info = sequence.info();
    assert_eq!(info.format(), ImageFormat::Tiff);
    assert_eq!(info.count(), 2);
    assert_eq!(info.kind(), SequenceKind::Pages);
    // The container's own geometry is its largest page's.
    assert_eq!((info.width(), info.height()), (4, 1));
    let first = sequence
        .next_frame()
        .expect("the first page decodes")
        .expect("a first page");
    assert_eq!((first.width(), first.height()), (2, 1));
    let second = sequence
        .next_frame()
        .expect("the second page decodes")
        .expect("a second page");
    assert_eq!((second.width(), second.height()), (4, 1));
    assert!(sequence.next_frame().expect("the walk ends").is_none());
    let addressed = sequence
        .page(0)
        .expect("a page is addressable")
        .expect("a first page");
    assert_eq!(addressed.width(), 2);
}

#[test]
fn a_page_container_weighs_nothing_against_the_limits_until_a_page_is_asked_for() {
    let bytes = build(false, &[grey(2, 1, vec![1, 2])]);
    let tight = DecodeLimits::new(1, 1, 1, 0);
    let mut sequence = Sequence::open(&bytes, &tight).expect("a page container opens");
    assert_eq!(
        sequence.next_frame(),
        Err(DecodeError::WidthExceedsLimit),
        "the page itself is refused"
    );
}

#[test]
fn the_pages_walk_survives_a_page_that_will_not_decode() {
    let broken = grey(2, 1, vec![1]);
    let bytes = build(false, &[grey(2, 1, vec![1, 2]), broken]);
    let mut walk = pages(&bytes, &limits()).expect("the chain opens");
    assert!(walk.step(&bytes).expect("the first page decodes"));
    assert_eq!(walk.step(&bytes), Err(DecodeError::TiffStripTruncated));
}

// --- Refusals -------------------------------------------------------------

#[test]
fn a_big_tiff_is_refused_by_name() {
    let mut bytes = build(false, &[grey(1, 1, vec![0])]);
    bytes[2] = 43;
    assert_eq!(crate::sniff(&bytes), Some(ImageFormat::Tiff));
    assert_eq!(
        decode(&bytes, &limits()),
        Err(DecodeError::TiffBigTiffUnsupported)
    );
}

#[test]
fn a_file_with_no_byte_order_mark_is_refused() {
    assert_eq!(
        decode(b"XX\x2a\x00\x08\x00\x00\x00", &limits()),
        Err(DecodeError::TiffBadSignature)
    );
    assert_eq!(crate::sniff(b"XX\x2a\x00"), None);
}

#[test]
fn every_prefix_of_a_document_is_refused_rather_than_half_read() {
    let bytes = build(false, &[grey(2, 2, vec![1, 2, 3, 4])]);
    for len in 0..bytes.len() {
        assert!(
            decode(&bytes[..len], &limits()).is_err(),
            "a {len}-byte prefix decoded"
        );
    }
    assert!(decode(&bytes, &limits()).is_ok());
}

#[test]
fn a_chain_that_loops_is_refused_rather_than_followed() {
    let mut bytes = build(false, &[grey(2, 1, vec![1, 2])]);
    let count = usize::from(u16::from_le_bytes([bytes[8], bytes[9]]));
    let next = 8 + 2 + count * 12;
    bytes[next..next + 4].copy_from_slice(&8u32.to_le_bytes());
    assert_eq!(
        decode(&bytes, &limits()),
        Err(DecodeError::TiffTooManyPages)
    );
}

#[test]
fn a_directory_missing_a_tag_its_page_needs_is_refused() {
    for tag in [TAG_IMAGE_WIDTH, TAG_IMAGE_LENGTH, TAG_PHOTOMETRIC] {
        let page = grey(2, 1, vec![1, 2]).without(tag);
        assert_eq!(refusal(page), DecodeError::TiffMissingTag, "tag {tag}");
    }
}

#[test]
fn a_strip_shorter_than_its_geometry_is_refused() {
    assert_eq!(
        refusal(grey(4, 4, vec![1, 2, 3])),
        DecodeError::TiffStripTruncated
    );
}

#[test]
fn a_directory_with_too_few_strip_entries_is_refused() {
    let page = grey(2, 4, vec![1, 2, 3, 4]).tag(TAG_ROWS_PER_STRIP, LONG, &[1]);
    assert_eq!(refusal(page), DecodeError::TiffStripCountMismatch);
}

#[test]
fn unsupported_declarations_are_each_refused_by_their_own_name() {
    let base = grey(2, 1, vec![1, 2]);
    assert_eq!(
        refusal(base.clone().tag(TAG_COMPRESSION, SHORT, &[6])),
        DecodeError::TiffUnsupportedCompression
    );
    assert_eq!(
        refusal(base.clone().tag(TAG_COMPRESSION, SHORT, &[32771])),
        DecodeError::TiffUnsupportedCompression
    );
    assert_eq!(
        refusal(base.clone().tag(TAG_PHOTOMETRIC, SHORT, &[8])),
        DecodeError::TiffUnsupportedPhotometric
    );
    assert_eq!(
        refusal(base.clone().tag(TAG_BITS_PER_SAMPLE, SHORT, &[12])),
        DecodeError::TiffUnsupportedBitDepth
    );
    assert_eq!(
        refusal(base.clone().tag(TAG_BITS_PER_SAMPLE, SHORT, &[8]).tag(
            TAG_SAMPLE_FORMAT,
            SHORT,
            &[3]
        )),
        DecodeError::TiffUnsupportedBitDepth
    );
    assert_eq!(
        refusal(base.clone().tag(TAG_SAMPLE_FORMAT, SHORT, &[6])),
        DecodeError::TiffUnsupportedSampleFormat
    );
    assert_eq!(
        refusal(base.clone().tag(TAG_ORIENTATION, SHORT, &[9])),
        DecodeError::TiffInvalidOrientation
    );
    assert_eq!(
        refusal(base.clone().tag(TAG_FILL_ORDER, SHORT, &[2])),
        DecodeError::TiffUnsupportedFillOrder
    );
    assert_eq!(
        refusal(base.clone().tag(TAG_PLANAR_CONFIGURATION, SHORT, &[3])),
        DecodeError::TiffUnsupportedPlanarConfiguration
    );
    assert_eq!(
        refusal(base.clone().tag(TAG_PREDICTOR, SHORT, &[3])),
        DecodeError::TiffInvalidPredictor
    );
    assert_eq!(
        refusal(
            base.clone()
                .tag(TAG_BITS_PER_SAMPLE, SHORT, &[4])
                .tag(TAG_PREDICTOR, SHORT, &[2])
        ),
        DecodeError::TiffInvalidPredictor
    );
    assert_eq!(
        refusal(base.tag(TAG_SAMPLES_PER_PIXEL, SHORT, &[0])),
        DecodeError::TiffSampleCountMismatch
    );
}

#[test]
fn samples_of_differing_depth_are_refused_rather_than_half_read() {
    let page = rgb(1, 1, vec![0, 0, 0]).tag(TAG_BITS_PER_SAMPLE, SHORT, &[5, 6, 5]);
    assert_eq!(refusal(page), DecodeError::TiffMixedSampleLayout);
}

#[test]
fn a_pixel_wider_than_the_containment_bound_is_refused() {
    let wide = rgb(1, 1, vec![0, 0, 0])
        .tag(TAG_SAMPLES_PER_PIXEL, SHORT, &[64])
        .tag(TAG_BITS_PER_SAMPLE, SHORT, &[8; 64]);
    assert_eq!(refusal(wide), DecodeError::TiffPixelTooWide);
    // A sample count past the bound is settled before the per-sample tags
    // are scanned at all, since one bit is the narrowest a sample can be.
    let many = rgb(1, 1, vec![0, 0, 0]).tag(TAG_SAMPLES_PER_PIXEL, SHORT, &[300]);
    assert_eq!(refusal(many), DecodeError::TiffPixelTooWide);
}

#[test]
fn a_separated_page_with_a_named_ink_set_is_refused() {
    let page = grey(1, 1, vec![0])
        .tag(TAG_PHOTOMETRIC, SHORT, &[u32::from(PHOTOMETRIC_SEPARATED)])
        .tag(TAG_SAMPLES_PER_PIXEL, SHORT, &[4])
        .tag(TAG_BITS_PER_SAMPLE, SHORT, &[8, 8, 8, 8])
        .tag(TAG_INK_SET, SHORT, &[2]);
    assert_eq!(refusal(page), DecodeError::TiffUnsupportedInkSet);
}

#[test]
fn a_palette_page_without_its_colour_map_is_refused() {
    let page = grey(1, 1, vec![0]).tag(TAG_PHOTOMETRIC, SHORT, &[u32::from(PHOTOMETRIC_PALETTE)]);
    assert_eq!(refusal(page), DecodeError::TiffMissingTag);
}

#[test]
fn a_palette_page_with_the_wrong_map_length_is_refused() {
    let page = grey(1, 1, vec![0])
        .tag(TAG_PHOTOMETRIC, SHORT, &[u32::from(PHOTOMETRIC_PALETTE)])
        .tag(TAG_BITS_PER_SAMPLE, SHORT, &[4])
        .tag(TAG_COLOUR_MAP, SHORT, &[0; 12]);
    assert_eq!(refusal(page), DecodeError::TiffInvalidColourMap);
}

#[test]
fn a_tile_off_a_sixteen_pixel_boundary_is_refused() {
    let page = grey(20, 16, Vec::new())
        .tag(TAG_TILE_WIDTH, LONG, &[20])
        .tag(TAG_TILE_LENGTH, LONG, &[16])
        .tiled()
        .units(vec![vec![0u8; 320]]);
    assert_eq!(refusal(page), DecodeError::TiffInvalidTileGeometry);
}

#[test]
fn separate_planes_are_refused_where_the_unit_cannot_carry_them() {
    let jpeg = grey(8, 8, tiny_jpeg(8, 8))
        .tag(TAG_COMPRESSION, SHORT, &[u32::from(COMPRESSION_JPEG)])
        .tag(TAG_SAMPLES_PER_PIXEL, SHORT, &[3])
        .tag(TAG_BITS_PER_SAMPLE, SHORT, &[8, 8, 8])
        .tag(TAG_PHOTOMETRIC, SHORT, &[u32::from(PHOTOMETRIC_RGB)])
        .tag(TAG_PLANAR_CONFIGURATION, SHORT, &[2]);
    assert_eq!(
        refusal(jpeg),
        DecodeError::TiffUnsupportedPlanarConfiguration
    );

    let subsampled = ycbcr(2, 2, vec![0; 6]).tag(TAG_PLANAR_CONFIGURATION, SHORT, &[2]);
    assert_eq!(
        refusal(subsampled),
        DecodeError::TiffUnsupportedPlanarConfiguration
    );
}

#[test]
fn a_predictor_over_subsampled_blocks_is_refused() {
    let page = ycbcr(2, 2, vec![0; 6]).tag(TAG_PREDICTOR, SHORT, &[2]);
    assert_eq!(refusal(page), DecodeError::TiffInvalidPredictor);
}

#[test]
fn an_invalid_chrominance_subsampling_is_refused() {
    assert_eq!(
        refusal(ycbcr(3, 1, vec![0; 12])),
        DecodeError::TiffInvalidSubsampling
    );
}

#[test]
fn a_jpeg_unit_of_the_wrong_size_is_refused() {
    let page =
        grey(8, 8, tiny_jpeg(16, 8)).tag(TAG_COMPRESSION, SHORT, &[u32::from(COMPRESSION_JPEG)]);
    assert_eq!(refusal(page), DecodeError::TiffJpegGeometryMismatch);
}

#[test]
fn a_geometry_past_the_callers_limits_is_refused_before_anything_is_reserved() {
    let bytes = build(false, &[grey(64, 64, vec![0u8; 64 * 64])]);
    assert_eq!(
        decode(&bytes, &DecodeLimits::new(32, 64, 1 << 20, 0)),
        Err(DecodeError::WidthExceedsLimit)
    );
    assert_eq!(
        decode(&bytes, &DecodeLimits::new(64, 32, 1 << 20, 0)),
        Err(DecodeError::HeightExceedsLimit)
    );
    assert_eq!(
        decode(&bytes, &DecodeLimits::new(64, 64, 128, 0)),
        Err(DecodeError::PixelCountExceedsLimit)
    );
}

#[test]
fn a_zero_side_is_refused_as_malformed_rather_than_large() {
    assert_eq!(refusal(grey(0, 1, Vec::new())), DecodeError::ZeroDimension);
}

#[test]
fn a_corrupt_deflate_stream_carries_its_inner_reason() {
    let mut stream = zlib(&[0u8; 8]);
    let last = stream.len() - 1;
    stream[last] ^= 0xFF;
    let page = grey(8, 1, stream).tag(
        TAG_COMPRESSION,
        SHORT,
        &[u32::from(COMPRESSION_ADOBE_DEFLATE)],
    );
    assert!(matches!(refusal(page), DecodeError::TiffCompressedData(_)));
}

#[test]
fn a_tile_larger_than_the_callers_limits_is_refused_before_it_is_reserved() {
    // A legal grid — one tile covering the picture — whose tile is far
    // larger than the picture, which is what a writer using a fixed tile
    // size produces and what leaves nothing but the caller's own ceiling
    // bounding the buffer behind it.
    let page = grey(16, 16, Vec::new())
        .tag(TAG_TILE_WIDTH, LONG, &[256])
        .tag(TAG_TILE_LENGTH, LONG, &[256])
        .tiled()
        .units(vec![vec![0u8; 16]]);
    let bytes = build(false, &[page]);
    assert_eq!(
        decode(&bytes, &DecodeLimits::new(64, 64, 64 * 64, 0)),
        Err(DecodeError::WidthExceedsLimit)
    );
    // Under limits that allow the tile, the refusal is the unit's own
    // shortness instead, so what the check above states is the caller's
    // ceiling and not a ceiling of this decoder's own.
    assert_eq!(
        decode(&bytes, &limits()),
        Err(DecodeError::TiffStripTruncated)
    );
}

#[test]
fn a_facsimile_page_that_is_not_bilevel_is_refused() {
    let deep =
        fax(COMPRESSION_GROUP4, 8, 1, bits("001 1011 011")).tag(TAG_BITS_PER_SAMPLE, SHORT, &[8]);
    assert_eq!(refusal(deep), DecodeError::TiffUnsupportedBitDepth);
    let many = fax(COMPRESSION_GROUP4, 8, 1, bits("001 1011 011"))
        .tag(TAG_SAMPLES_PER_PIXEL, SHORT, &[3])
        .tag(TAG_BITS_PER_SAMPLE, SHORT, &[1, 1, 1]);
    assert_eq!(refusal(many), DecodeError::TiffUnsupportedBitDepth);
}

#[test]
fn a_subsampled_page_with_a_fourth_sample_is_refused() {
    let page = ycbcr(2, 2, vec![0; 6])
        .tag(TAG_SAMPLES_PER_PIXEL, SHORT, &[4])
        .tag(TAG_BITS_PER_SAMPLE, SHORT, &[8, 8, 8, 8])
        .tag(TAG_EXTRA_SAMPLES, SHORT, &[2]);
    assert_eq!(refusal(page), DecodeError::TiffSampleCountMismatch);
}
