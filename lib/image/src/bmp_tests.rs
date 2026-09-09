//! BMP decoder tests: every header version, bit depth, encoding, and row
//! order, and every refusal.
//!
//! Every input is built here from a small set of writers that mirror the
//! format's own layout — the crate ships no fixtures. [`Header`] carries what
//! a DIB header declares and lays it out at whichever version's length it is
//! given, so a test states only the field it is about.

use alloc::vec;
use alloc::vec::Vec;

use super::{Channel, Sampler};
use crate::{decode, probe, DecodeError, DecodeLimits, ImageFormat, Sequence, SequenceKind};

/// The DIB header versions, by their declared length.
const CORE: u32 = 12;
const INFO: u32 = 40;
const V2: u32 = 52;
const V3: u32 = 56;
const V4: u32 = 108;
const V5: u32 = 124;

const BI_RGB: u32 = 0;
const BI_RLE8: u32 = 1;
const BI_RLE4: u32 = 2;
const BI_BITFIELDS: u32 = 3;
const BI_JPEG: u32 = 4;
const BI_ALPHABITFIELDS: u32 = 6;

/// `BITMAPFILEHEADER`'s fixed length.
const FILE_HEADER: usize = 14;

/// Limits generous enough for every fixture here.
fn limits() -> DecodeLimits {
    DecodeLimits::new(256, 256, 256 * 256, 0)
}

/// A four-entry colour table in the format's own blue-first order: red,
/// green, blue, white.
const PALETTE: [[u8; 3]; 4] = [
    [0x00, 0x00, 0xFF],
    [0x00, 0xFF, 0x00],
    [0xFF, 0x00, 0x00],
    [0xFF, 0xFF, 0xFF],
];

/// Opaque RGBA for the `PALETTE` entry `index`.
fn colour(index: usize) -> [u8; 4] {
    let entry = PALETTE[index];
    [entry[2], entry[1], entry[0], 0xFF]
}

/// Fully transparent, which is what a pixel no run covered stays.
const CLEAR: [u8; 4] = [0, 0, 0, 0];

/// `PALETTE` as `count` four-byte `RGBQUAD` entries, the shape every header
/// past `BITMAPCOREHEADER` uses.
fn quads(count: usize) -> Vec<u8> {
    let mut out = Vec::new();
    for index in 0..count {
        let entry = PALETTE[index % PALETTE.len()];
        out.extend_from_slice(&entry);
        out.push(0);
    }
    out
}

/// `PALETTE` as `count` three-byte `RGBTRIPLE` entries, which is what a
/// `BITMAPCOREHEADER` declares.
fn triples(count: usize) -> Vec<u8> {
    let mut out = Vec::new();
    for index in 0..count {
        out.extend_from_slice(&PALETTE[index % PALETTE.len()]);
    }
    out
}

/// What a DIB header declares, defaulted to the ordinary case.
#[derive(Copy, Clone)]
struct Header {
    size: u32,
    width: i32,
    height: i32,
    planes: u16,
    bits: u16,
    compression: u32,
    clr_used: u32,
    masks: [u32; 4],
}

impl Header {
    fn new(size: u32, width: i32, height: i32, bits: u16) -> Self {
        Self {
            size,
            width,
            height,
            planes: 1,
            bits,
            compression: BI_RGB,
            clr_used: 0,
            masks: [0; 4],
        }
    }

    fn compressed(mut self, compression: u32) -> Self {
        self.compression = compression;
        self
    }

    fn bitfields(mut self, compression: u32, masks: [u32; 4]) -> Self {
        self.compression = compression;
        self.masks = masks;
        self
    }

    /// Lay the header out at its declared length. The masks are written
    /// wherever the version or the compression puts them, and everything
    /// past them is the colour-space, gamma, and profile description this
    /// decoder reads past.
    fn bytes(&self) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&self.size.to_le_bytes());
        if self.size == CORE {
            out.extend_from_slice(&self.width.to_le_bytes()[..2]);
            out.extend_from_slice(&self.height.to_le_bytes()[..2]);
            out.extend_from_slice(&self.planes.to_le_bytes());
            out.extend_from_slice(&self.bits.to_le_bytes());
            return out;
        }
        out.extend_from_slice(&self.width.to_le_bytes());
        out.extend_from_slice(&self.height.to_le_bytes());
        out.extend_from_slice(&self.planes.to_le_bytes());
        out.extend_from_slice(&self.bits.to_le_bytes());
        out.extend_from_slice(&self.compression.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes()); // biSizeImage
        out.extend_from_slice(&0u32.to_le_bytes()); // biXPelsPerMeter
        out.extend_from_slice(&0u32.to_le_bytes()); // biYPelsPerMeter
        out.extend_from_slice(&self.clr_used.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes()); // biClrImportant
        let written = if self.compression == BI_ALPHABITFIELDS || self.size >= V3 {
            4
        } else if self.compression == BI_BITFIELDS || self.size >= V2 {
            3
        } else {
            0
        };
        for mask in self.masks.iter().take(written) {
            out.extend_from_slice(&mask.to_le_bytes());
        }
        while out.len() < usize::try_from(self.size).unwrap_or(0) {
            out.push(0);
        }
        out
    }
}

/// A BMP file: the file header, the DIB header, the colour table, then the
/// pixel array, with `bfOffBits` pointing straight past the table.
fn file(header: &Header, palette: &[u8], pixels: &[u8]) -> Vec<u8> {
    let dib = header.bytes();
    let offset = FILE_HEADER + dib.len() + palette.len();
    with_offset(header, palette, pixels, u32::try_from(offset).unwrap_or(0))
}

/// A BMP file whose `bfOffBits` is stated rather than derived.
fn with_offset(header: &Header, palette: &[u8], pixels: &[u8], offset: u32) -> Vec<u8> {
    let dib = header.bytes();
    let total = FILE_HEADER + dib.len() + palette.len() + pixels.len();
    let mut out = b"BM".to_vec();
    out.extend_from_slice(&u32::try_from(total).unwrap_or(0).to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&offset.to_le_bytes());
    out.extend_from_slice(&dib);
    out.extend_from_slice(palette);
    out.extend_from_slice(pixels);
    out
}

/// Pack rows of colour-table indices at `bits` per pixel, most significant
/// first, each row padded to a four-byte boundary. Rows are given top first
/// and emitted in the order `top_down` asks for.
fn indexed_pixels(bits: u32, top_down: bool, rows: &[&[u8]]) -> Vec<u8> {
    let mut ordered: Vec<&&[u8]> = rows.iter().collect();
    if !top_down {
        ordered.reverse();
    }
    let mut out = Vec::new();
    for row in ordered {
        let start = out.len();
        let mut accumulator = 0u32;
        let mut filled = 0u32;
        for &index in *row {
            accumulator = (accumulator << bits) | u32::from(index);
            filled += bits;
            if filled == 8 {
                out.push(u8::try_from(accumulator).unwrap_or(0));
                accumulator = 0;
                filled = 0;
            }
        }
        if filled > 0 {
            out.push(u8::try_from(accumulator << (8 - filled)).unwrap_or(0));
        }
        while (out.len() - start) % 4 != 0 {
            out.push(0);
        }
    }
    out
}

/// Pack rows of `bytes`-wide little-endian pixel words, each row padded to a
/// four-byte boundary.
fn packed_pixels(bytes: usize, top_down: bool, rows: &[&[u32]]) -> Vec<u8> {
    let mut ordered: Vec<&&[u32]> = rows.iter().collect();
    if !top_down {
        ordered.reverse();
    }
    let mut out = Vec::new();
    for row in ordered {
        let start = out.len();
        for &word in *row {
            out.extend_from_slice(&word.to_le_bytes()[..bytes]);
        }
        while (out.len() - start) % 4 != 0 {
            out.push(0);
        }
    }
    out
}

/// The decoded pixels of `bytes`, as one RGBA quad per pixel.
fn pixels_of(bytes: &[u8]) -> Vec<[u8; 4]> {
    let image = decode(bytes, &limits()).expect("fixture failed to decode");
    image.pixels().as_chunks::<4>().0.to_vec()
}

#[test]
fn every_header_version_decodes_the_same_picture() {
    let rows: [&[u32]; 2] = [&[0x00FF_0000, 0x0000_FF00], &[0x0000_00FF, 0x00FF_FFFF]];
    let pixels = packed_pixels(3, false, &rows);
    let expected = vec![
        [0xFF, 0x00, 0x00, 0xFF],
        [0x00, 0xFF, 0x00, 0xFF],
        [0x00, 0x00, 0xFF, 0xFF],
        [0xFF, 0xFF, 0xFF, 0xFF],
    ];
    for size in [CORE, INFO, V2, V3, V4, V5] {
        let bmp = file(&Header::new(size, 2, 2, 24), &[], &pixels);
        assert_eq!(
            pixels_of(&bmp),
            expected,
            "header size {size} decoded wrong"
        );
    }
}

#[test]
fn an_unsupported_header_size_is_refused() {
    // The OS/2 2.x lengths and anything else are named, not half-read.
    for size in [0, 11, 16, 41, 64, 125] {
        let bmp = file(&Header::new(size, 1, 1, 24), &[], &[0, 0, 0, 0]);
        assert_eq!(
            decode(&bmp, &limits()),
            Err(DecodeError::BmpUnsupportedHeaderSize),
            "header size {size} was not refused"
        );
    }
}

#[test]
fn one_bit_indices_unpack_most_significant_first() {
    let rows: [&[u8]; 1] = [&[0, 1, 1, 0, 1, 0, 0, 1]];
    let bmp = file(
        &Header::new(INFO, 8, 1, 1),
        &quads(2),
        &indexed_pixels(1, false, &rows),
    );
    let expected: Vec<[u8; 4]> = rows[0].iter().map(|&i| colour(usize::from(i))).collect();
    assert_eq!(pixels_of(&bmp), expected);
}

#[test]
fn two_bit_indices_unpack_most_significant_first() {
    let rows: [&[u8]; 1] = [&[0, 1, 2, 3]];
    let bmp = file(
        &Header::new(INFO, 4, 1, 2),
        &quads(4),
        &indexed_pixels(2, false, &rows),
    );
    assert_eq!(
        pixels_of(&bmp),
        vec![colour(0), colour(1), colour(2), colour(3)]
    );
}

#[test]
fn four_bit_indices_unpack_high_nibble_first() {
    let rows: [&[u8]; 1] = [&[3, 0, 1, 2]];
    let bmp = file(
        &Header::new(INFO, 4, 1, 4),
        &quads(16),
        &indexed_pixels(4, false, &rows),
    );
    assert_eq!(
        pixels_of(&bmp),
        vec![colour(3), colour(0), colour(1), colour(2)]
    );
}

#[test]
fn eight_bit_indices_index_the_colour_table() {
    let rows: [&[u8]; 1] = [&[2, 0, 3]];
    let bmp = file(
        &Header::new(INFO, 3, 1, 8),
        &quads(256),
        &indexed_pixels(8, false, &rows),
    );
    assert_eq!(pixels_of(&bmp), vec![colour(2), colour(0), colour(3)]);
}

#[test]
fn sixteen_bit_pixels_default_to_five_five_five() {
    // Red, green, blue, and black at the extremes of each five-bit field.
    let rows: [&[u32]; 1] = [&[0x7C00, 0x03E0, 0x001F, 0x0000]];
    let bmp = file(
        &Header::new(INFO, 4, 1, 16),
        &[],
        &packed_pixels(2, false, &rows),
    );
    assert_eq!(
        pixels_of(&bmp),
        vec![
            [0xFF, 0x00, 0x00, 0xFF],
            [0x00, 0xFF, 0x00, 0xFF],
            [0x00, 0x00, 0xFF, 0xFF],
            [0x00, 0x00, 0x00, 0xFF],
        ]
    );
}

#[test]
fn twenty_four_bit_pixels_are_blue_first() {
    let bmp = file(
        &Header::new(INFO, 1, 1, 24),
        &[],
        &packed_pixels(3, false, &[&[0x0011_2233]]),
    );
    assert_eq!(pixels_of(&bmp), vec![[0x11, 0x22, 0x33, 0xFF]]);
}

#[test]
fn thirty_two_bit_rgb_pixels_are_opaque() {
    // The fourth byte is undefined in a `BI_RGB` file, so a zero there must
    // not make the picture disappear.
    let bmp = file(
        &Header::new(INFO, 2, 1, 32),
        &[],
        &packed_pixels(4, false, &[&[0x0011_2233, 0xFF44_5566]]),
    );
    assert_eq!(
        pixels_of(&bmp),
        vec![[0x11, 0x22, 0x33, 0xFF], [0x44, 0x55, 0x66, 0xFF]]
    );
}

#[test]
fn rows_are_bottom_up_by_default() {
    let rows: [&[u8]; 2] = [&[0], &[3]];
    let bmp = file(
        &Header::new(INFO, 1, 2, 8),
        &quads(256),
        &indexed_pixels(8, false, &rows),
    );
    assert_eq!(pixels_of(&bmp), vec![colour(0), colour(3)]);
}

#[test]
fn a_negative_height_reads_rows_top_down() {
    let rows: [&[u8]; 2] = [&[0], &[3]];
    let bmp = file(
        &Header::new(INFO, 1, -2, 8),
        &quads(256),
        &indexed_pixels(8, true, &rows),
    );
    let image = decode(&bmp, &limits()).expect("top-down fixture failed to decode");
    assert_eq!((image.width(), image.height()), (1, 2));
    assert_eq!(
        image.pixels().as_chunks::<4>().0,
        [colour(0), colour(3)].as_slice()
    );
}

#[test]
fn rows_are_padded_to_a_four_byte_boundary() {
    // Three 24-bit pixels are nine bytes, so each row occupies twelve.
    let rows: [&[u32]; 2] = [&[1, 2, 3], &[4, 5, 6]];
    let pixels = packed_pixels(3, false, &rows);
    assert_eq!(pixels.len(), 24);
    let bmp = file(&Header::new(INFO, 3, 2, 24), &[], &pixels);
    let decoded = pixels_of(&bmp);
    assert_eq!(decoded.len(), 6);
    assert_eq!(decoded[0], [0x00, 0x00, 0x01, 0xFF]);
    assert_eq!(decoded[5], [0x00, 0x00, 0x06, 0xFF]);
}

#[test]
fn bitfield_masks_place_each_channel() {
    // 5-6-5, the other layout sixteen-bit files use.
    let header = Header::new(INFO, 3, 1, 16).bitfields(BI_BITFIELDS, [0xF800, 0x07E0, 0x001F, 0]);
    let bmp = file(
        &header,
        &[],
        &packed_pixels(2, false, &[&[0xF800, 0x07E0, 0x001F]]),
    );
    assert_eq!(
        pixels_of(&bmp),
        vec![
            [0xFF, 0x00, 0x00, 0xFF],
            [0x00, 0xFF, 0x00, 0xFF],
            [0x00, 0x00, 0xFF, 0xFF],
        ]
    );
}

#[test]
fn an_alpha_bitfield_mask_carries_transparency() {
    let header = Header::new(V3, 2, 1, 32).bitfields(
        BI_BITFIELDS,
        [0x00FF_0000, 0x0000_FF00, 0x0000_00FF, 0xFF00_0000],
    );
    let bmp = file(
        &header,
        &[],
        &packed_pixels(4, false, &[&[0x8011_2233, 0x0044_5566]]),
    );
    assert_eq!(
        pixels_of(&bmp),
        vec![[0x11, 0x22, 0x33, 0x80], [0x44, 0x55, 0x66, 0x00]]
    );
}

#[test]
fn bi_alphabitfields_reads_a_fourth_mask_after_the_header() {
    let header = Header::new(INFO, 1, 1, 32).bitfields(
        BI_ALPHABITFIELDS,
        [0x00FF_0000, 0x0000_FF00, 0x0000_00FF, 0xFF00_0000],
    );
    let bmp = file(&header, &[], &packed_pixels(4, false, &[&[0x40AA_BBCC]]));
    assert_eq!(pixels_of(&bmp), vec![[0xAA, 0xBB, 0xCC, 0x40]]);
}

#[test]
fn a_channel_wider_than_eight_bits_keeps_its_top_eight() {
    // 10-10-10 with two bits of alpha, which some capture hardware writes.
    let header = Header::new(V3, 2, 1, 32).bitfields(
        BI_BITFIELDS,
        [0x3FF0_0000, 0x000F_FC00, 0x0000_03FF, 0xC000_0000],
    );
    let bmp = file(
        &header,
        &[],
        &packed_pixels(4, false, &[&[0x3FFF_FFFF, 0x0000_0000]]),
    );
    assert_eq!(
        pixels_of(&bmp),
        vec![[0xFF, 0xFF, 0xFF, 0x00], [0x00, 0x00, 0x00, 0x00]]
    );
}

#[test]
fn a_v4_header_with_bi_rgb_ignores_its_mask_fields() {
    // The masks are only the pixel layout under `BI_BITFIELDS`, so a
    // `BI_RGB` picture keeps the bit count's own default and stays opaque.
    let header = Header::new(V4, 1, 1, 32)
        .bitfields(BI_RGB, [0x0000_00FF, 0x0000_FF00, 0x00FF_0000, 0xFF00_0000]);
    let bmp = file(&header, &[], &packed_pixels(4, false, &[&[0x0011_2233]]));
    assert_eq!(pixels_of(&bmp), vec![[0x11, 0x22, 0x33, 0xFF]]);
}

#[test]
fn an_invalid_mask_set_is_refused() {
    let cases: [([u32; 4], &str); 4] = [
        ([0x7C00, 0x03E0, 0, 0], "a zero colour mask"),
        ([0x7C00, 0x03E0, 0x7C00, 0], "an overlapping mask"),
        ([0x6C00, 0x03E0, 0x001F, 0], "a discontiguous mask"),
        ([0x0001_0000, 0x03E0, 0x001F, 0], "a mask outside the pixel"),
    ];
    for (masks, what) in cases {
        let header = Header::new(INFO, 1, 1, 16).bitfields(BI_BITFIELDS, masks);
        let bmp = file(&header, &[], &packed_pixels(2, false, &[&[0]]));
        assert_eq!(
            decode(&bmp, &limits()),
            Err(DecodeError::BmpInvalidMask),
            "{what} was not refused"
        );
    }
}

#[test]
fn bitfields_at_an_unsupported_bit_count_are_refused() {
    for bits in [1, 2, 4, 8, 24] {
        let header =
            Header::new(INFO, 1, 1, bits).bitfields(BI_BITFIELDS, [0xFF_0000, 0xFF00, 0xFF, 0]);
        let bmp = file(&header, &quads(256), &[0, 0, 0, 0]);
        assert_eq!(
            decode(&bmp, &limits()),
            Err(DecodeError::BmpCompressionMismatch),
            "bitfields at {bits} bits was not refused"
        );
    }
}

#[test]
fn a_five_bit_channel_scales_across_the_whole_eight_bit_range() {
    let sampler = Sampler::new(Channel::new(0x7C00).expect("a contiguous mask"));
    assert_eq!(sampler.sample(0x0000), 0);
    assert_eq!(sampler.sample(0x7C00), 255);
    assert_eq!(sampler.sample(0x0400), 8);
    assert_eq!(sampler.sample(0x4000), 132);
}

#[test]
fn an_absent_channel_samples_opaque() {
    let sampler = Sampler::new(Channel::ABSENT);
    assert_eq!(sampler.sample(0), 255);
    assert_eq!(sampler.sample(u32::MAX), 255);
}

#[test]
fn a_core_header_reads_three_byte_palette_entries() {
    let rows: [&[u8]; 1] = [&[0, 1, 2, 3]];
    let bmp = file(
        &Header::new(CORE, 4, 1, 2),
        &triples(4),
        &indexed_pixels(2, false, &rows),
    );
    assert_eq!(
        pixels_of(&bmp),
        vec![colour(0), colour(1), colour(2), colour(3)]
    );
}

#[test]
fn clr_used_shortens_the_colour_table() {
    let mut header = Header::new(INFO, 2, 1, 8);
    header.clr_used = 2;
    let rows: [&[u8]; 1] = [&[0, 1]];
    let bmp = file(&header, &quads(2), &indexed_pixels(8, false, &rows));
    assert_eq!(pixels_of(&bmp), vec![colour(0), colour(1)]);
}

#[test]
fn an_index_past_the_colour_table_is_refused() {
    let mut header = Header::new(INFO, 2, 1, 8);
    header.clr_used = 2;
    let rows: [&[u8]; 1] = [&[0, 7]];
    let bmp = file(&header, &quads(2), &indexed_pixels(8, false, &rows));
    assert_eq!(
        decode(&bmp, &limits()),
        Err(DecodeError::BmpPaletteIndexOutOfRange)
    );
}

#[test]
fn clr_used_beyond_the_bit_count_is_refused() {
    let mut header = Header::new(INFO, 1, 1, 4);
    header.clr_used = 17;
    let bmp = file(&header, &quads(17), &[0, 0, 0, 0]);
    assert_eq!(
        decode(&bmp, &limits()),
        Err(DecodeError::BmpInvalidPaletteLength)
    );
}

#[test]
fn a_colour_table_overlapping_the_pixel_array_is_refused() {
    let header = Header::new(INFO, 1, 1, 8);
    let palette = quads(256);
    let pixels = [0u8, 0, 0, 0];
    // `bfOffBits` naming a point inside the declared table leaves the two
    // descriptions of the file contradicting each other.
    let short = u32::try_from(FILE_HEADER + 40 + 16).unwrap_or(0);
    let bmp = with_offset(&header, &palette, &pixels, short);
    assert_eq!(
        decode(&bmp, &limits()),
        Err(DecodeError::BmpInvalidPaletteLength)
    );
}

#[test]
fn a_high_colour_palette_hint_does_not_move_the_pixels() {
    // Above eight bits `biClrUsed` is only a palette-optimisation hint, and
    // real encoders write one while pointing `bfOffBits` straight past the
    // header, so it must not be read as a table in the way.
    let mut header = Header::new(INFO, 1, 1, 24);
    header.clr_used = 256;
    let bmp = file(&header, &[], &packed_pixels(3, false, &[&[0x0011_2233]]));
    assert_eq!(pixels_of(&bmp), vec![[0x11, 0x22, 0x33, 0xFF]]);
}

/// An RLE pixel array from `(count, value)` pairs, terminated as given.
fn rle(pairs: &[(u8, u8)], extra: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    for &(count, value) in pairs {
        out.push(count);
        out.push(value);
    }
    out.extend_from_slice(extra);
    out
}

#[test]
fn rle8_encoded_runs_repeat_their_index() {
    let header = Header::new(INFO, 4, 2, 8).compressed(BI_RLE8);
    // Bottom row first: four of index 1, then four of index 2.
    let stream = rle(&[(4, 1), (0, 0), (4, 2), (0, 1)], &[]);
    let bmp = file(&header, &quads(256), &stream);
    let mut expected = vec![colour(2); 4];
    expected.extend(vec![colour(1); 4]);
    assert_eq!(pixels_of(&bmp), expected);
}

#[test]
fn rle8_absolute_runs_are_padded_to_a_word() {
    let header = Header::new(INFO, 3, 1, 8).compressed(BI_RLE8);
    // An absolute run of three bytes is followed by one pad byte.
    let stream = rle(&[(0, 3)], &[2, 1, 0, 0xFF, 0, 1]);
    let bmp = file(&header, &quads(256), &stream);
    assert_eq!(pixels_of(&bmp), vec![colour(2), colour(1), colour(0)]);
}

#[test]
fn rle8_delta_leaves_the_skipped_pixels_transparent() {
    let header = Header::new(INFO, 4, 2, 8).compressed(BI_RLE8);
    // One pixel, then a delta of one right and one up, then one pixel.
    let stream = rle(&[(1, 3)], &[0, 2, 1, 1, 1, 3, 0, 1]);
    let bmp = file(&header, &quads(256), &stream);
    assert_eq!(
        pixels_of(&bmp),
        vec![
            CLEAR,
            CLEAR,
            colour(3),
            CLEAR,
            colour(3),
            CLEAR,
            CLEAR,
            CLEAR,
        ]
    );
}

#[test]
fn rle8_omitting_its_end_of_bitmap_is_accepted_once_every_row_is_covered() {
    let header = Header::new(INFO, 2, 2, 8).compressed(BI_RLE8);
    let stream = rle(&[(2, 1), (0, 0), (2, 3), (0, 0)], &[]);
    let bmp = file(&header, &quads(256), &stream);
    let mut expected = vec![colour(3); 2];
    expected.extend(vec![colour(1); 2]);
    assert_eq!(pixels_of(&bmp), expected);
}

#[test]
fn rle4_encoded_runs_alternate_two_nibbles() {
    let header = Header::new(INFO, 4, 1, 4).compressed(BI_RLE4);
    let stream = rle(&[(4, 0x13), (0, 1)], &[]);
    let bmp = file(&header, &quads(16), &stream);
    assert_eq!(
        pixels_of(&bmp),
        vec![colour(1), colour(3), colour(1), colour(3)]
    );
}

#[test]
fn rle4_absolute_runs_pack_two_nibbles_per_byte() {
    let header = Header::new(INFO, 3, 1, 4).compressed(BI_RLE4);
    // Three nibbles occupy two bytes, padded out to four.
    let stream = rle(&[(0, 3)], &[0x21, 0x00, 0, 0, 0, 1]);
    let bmp = file(&header, &quads(16), &stream);
    assert_eq!(pixels_of(&bmp), vec![colour(2), colour(1), colour(0)]);
}

#[test]
fn an_rle_run_past_the_row_is_refused() {
    let header = Header::new(INFO, 2, 1, 8).compressed(BI_RLE8);
    let stream = rle(&[(5, 1), (0, 1)], &[]);
    let bmp = file(&header, &quads(256), &stream);
    assert_eq!(decode(&bmp, &limits()), Err(DecodeError::BmpRleOutOfBounds));
}

#[test]
fn an_rle_delta_past_the_last_row_is_refused() {
    let header = Header::new(INFO, 2, 2, 8).compressed(BI_RLE8);
    let stream = rle(&[], &[0, 2, 0, 9, 1, 1, 0, 1]);
    let bmp = file(&header, &quads(256), &stream);
    assert_eq!(decode(&bmp, &limits()), Err(DecodeError::BmpRleOutOfBounds));
}

#[test]
fn a_truncated_rle_stream_is_refused() {
    let header = Header::new(INFO, 4, 2, 8).compressed(BI_RLE8);
    for stream in [
        rle(&[(4, 1)], &[]),          // no terminator, one row short
        rle(&[(4, 1), (0, 0)], &[0]), // a lone byte where a pair belongs
        rle(&[], &[0, 2, 1]),         // a delta missing its second axis
        rle(&[], &[0, 4, 1, 2]),      // an absolute run missing bytes
    ] {
        let bmp = file(&header, &quads(256), &stream);
        assert_eq!(
            decode(&bmp, &limits()),
            Err(DecodeError::BmpRleTruncated),
            "a truncated stream was not refused: {stream:?}"
        );
    }
}

#[test]
fn a_run_length_encoding_at_the_wrong_bit_count_is_refused() {
    for (compression, bits) in [(BI_RLE8, 4), (BI_RLE8, 24), (BI_RLE4, 8), (BI_RLE4, 1)] {
        let header = Header::new(INFO, 1, 1, bits).compressed(compression);
        let bmp = file(&header, &quads(256), &[0, 1]);
        assert_eq!(
            decode(&bmp, &limits()),
            Err(DecodeError::BmpCompressionMismatch),
            "compression {compression} at {bits} bits was not refused"
        );
    }
}

#[test]
fn a_top_down_run_length_encoded_array_is_refused() {
    let header = Header::new(INFO, 1, -1, 8).compressed(BI_RLE8);
    let bmp = file(&header, &quads(256), &[0, 1]);
    assert_eq!(
        decode(&bmp, &limits()),
        Err(DecodeError::BmpCompressionMismatch)
    );
}

#[test]
fn an_unclaimed_compression_is_refused() {
    // An embedded JPEG or PNG pixel array, a CMYK encoding, and a code the
    // format does not define at all.
    for code in [BI_JPEG, 5, 11, 12, 13, 7, 0xFFFF] {
        let header = Header::new(INFO, 1, 1, 24).compressed(code);
        let bmp = file(&header, &[], &[0, 0, 0, 0]);
        assert_eq!(
            decode(&bmp, &limits()),
            Err(DecodeError::BmpUnsupportedCompression),
            "compression {code} was not refused"
        );
    }
}

#[test]
fn a_plane_count_other_than_one_is_refused() {
    for size in [CORE, INFO] {
        let mut header = Header::new(size, 1, 1, 24);
        header.planes = 2;
        let bmp = file(&header, &[], &[0, 0, 0, 0]);
        assert_eq!(
            decode(&bmp, &limits()),
            Err(DecodeError::BmpInvalidPlanes),
            "header size {size} accepted two colour planes"
        );
    }
}

#[test]
fn an_unsupported_bit_count_is_refused() {
    for bits in [0, 3, 5, 6, 7, 9, 15, 17, 48, 64] {
        let bmp = file(&Header::new(INFO, 1, 1, bits), &[], &[0, 0, 0, 0]);
        assert_eq!(
            decode(&bmp, &limits()),
            Err(DecodeError::BmpUnsupportedBitCount),
            "a bit count of {bits} was not refused"
        );
    }
}

#[test]
fn a_negative_width_is_refused() {
    let bmp = file(&Header::new(INFO, -4, 1, 24), &[], &[0; 16]);
    assert_eq!(
        decode(&bmp, &limits()),
        Err(DecodeError::BmpInvalidDimensions)
    );
}

#[test]
fn a_zero_dimension_is_refused() {
    for (width, height) in [(0, 1), (1, 0), (0, 0)] {
        let bmp = file(&Header::new(INFO, width, height, 24), &[], &[0; 4]);
        assert_eq!(
            decode(&bmp, &limits()),
            Err(DecodeError::ZeroDimension),
            "{width}x{height} was not refused"
        );
    }
}

#[test]
fn a_bad_signature_is_refused() {
    let mut bmp = file(&Header::new(INFO, 1, 1, 24), &[], &[0; 4]);
    bmp[1] = b'A';
    assert_eq!(crate::sniff(&bmp), None);
    assert_eq!(
        super::decode(&bmp, &limits()),
        Err(DecodeError::BmpBadSignature)
    );
    assert_eq!(super::probe(&bmp), Err(DecodeError::BmpBadSignature));
}

#[test]
fn a_cut_inside_the_headers_is_refused_as_truncated() {
    let bmp = file(&Header::new(INFO, 1, 1, 24), &[], &[0; 4]);
    // `biClrUsed` at header offset 32 is the last field read, so a cut
    // before the end of it stops a header read rather than anything later.
    for cut in 2..FILE_HEADER + 36 {
        assert_eq!(
            decode(&bmp[..cut], &limits()),
            Err(DecodeError::BmpTruncated),
            "a header cut at {cut} was not refused as truncated"
        );
    }
}

#[test]
fn a_pixel_offset_outside_the_file_is_refused() {
    let header = Header::new(INFO, 1, 1, 24);
    let pixels = [0u8; 4];
    let past = u32::try_from(FILE_HEADER + 40 + 8).unwrap_or(0);
    assert_eq!(
        decode(&with_offset(&header, &[], &pixels, past), &limits()),
        Err(DecodeError::BmpInvalidPixelOffset)
    );
    assert_eq!(
        decode(&with_offset(&header, &[], &pixels, 20), &limits()),
        Err(DecodeError::BmpInvalidPixelOffset)
    );
}

#[test]
fn a_short_pixel_array_is_refused() {
    let bmp = file(&Header::new(INFO, 4, 4, 24), &[], &[0; 24]);
    assert_eq!(
        decode(&bmp, &limits()),
        Err(DecodeError::BmpPixelDataTruncated)
    );
}

#[test]
fn limits_are_weighed_before_the_buffer_is_allocated() {
    // The pixel array is one byte long, so a decode that reached it would
    // refuse for that reason instead.
    let bmp = file(&Header::new(INFO, 4000, 4000, 24), &[], &[0]);
    let tight = DecodeLimits::new(64, 64, 64 * 64, 0);
    assert_eq!(decode(&bmp, &tight), Err(DecodeError::WidthExceedsLimit));
    let tall = DecodeLimits::new(8000, 64, 64 * 64, 0);
    assert_eq!(decode(&bmp, &tall), Err(DecodeError::HeightExceedsLimit));
    let roomy = DecodeLimits::new(8000, 8000, 1024, 0);
    assert_eq!(
        decode(&bmp, &roomy),
        Err(DecodeError::PixelCountExceedsLimit)
    );
}

#[test]
fn probe_answers_the_declared_geometry_without_decoding() {
    // The pixel array is absent, so only a probe can answer at all.
    let bmp = file(&Header::new(INFO, 640, 480, 24), &[], &[]);
    let info = probe(&bmp).expect("a valid header failed to probe");
    assert_eq!(info.format(), ImageFormat::Bmp);
    assert_eq!((info.width(), info.height()), (640, 480));
    let top_down = file(&Header::new(INFO, 7, -9, 32), &[], &[]);
    let info = probe(&top_down).expect("a top-down header failed to probe");
    assert_eq!((info.width(), info.height()), (7, 9));
}

#[test]
fn probe_refuses_a_malformed_header() {
    let bmp = file(&Header::new(INFO, 1, 1, 13), &[], &[]);
    assert_eq!(probe(&bmp), Err(DecodeError::BmpUnsupportedBitCount));
}

#[test]
fn a_bmp_is_a_one_entry_sequence() {
    let bmp = file(
        &Header::new(INFO, 1, 1, 24),
        &[],
        &packed_pixels(3, false, &[&[0x0011_2233]]),
    );
    let mut sequence = Sequence::open(&bmp, &limits()).expect("a valid BMP failed to open");
    assert_eq!(sequence.info().format(), ImageFormat::Bmp);
    assert_eq!(sequence.info().count(), 1);
    assert_eq!(sequence.info().kind(), SequenceKind::Pages);
    let frame = sequence
        .next_frame()
        .expect("a valid BMP failed to decode")
        .expect("a valid BMP produced no entry");
    assert_eq!((frame.width(), frame.height()), (1, 1));
    assert_eq!(frame.delay_ns(), 0);
    assert!(sequence
        .next_frame()
        .expect("a second step failed")
        .is_none());
    sequence.rewind();
    assert!(sequence
        .next_frame()
        .expect("a rewound step failed")
        .is_some());
    assert!(sequence.page(0).expect("page zero failed").is_some());
    assert!(sequence.page(1).expect("page one failed").is_none());
}

#[test]
fn every_prefix_of_a_valid_file_is_refused_rather_than_half_decoded() {
    let bmp = file(
        &Header::new(V5, 3, 2, 8),
        &quads(256),
        &indexed_pixels(8, false, &[&[0, 1, 2], &[3, 2, 1]]),
    );
    assert!(decode(&bmp, &limits()).is_ok());
    for cut in 0..bmp.len() {
        assert!(
            decode(&bmp[..cut], &limits()).is_err(),
            "a file cut at {cut} decoded"
        );
    }
}
