//! GIF decoder tests: every structural variant, the whole disposal model,
//! and the refusals.
//!
//! Every input is built here, byte by byte, from a small set of writers that
//! mirror the `GIF89a` block grammar — the crate ships no fixtures. The LZW
//! streams are produced by a **literal** encoder ([`lzw_literal`]) that emits
//! one root code per pixel with a clear code at the start: valid GIF LZW that
//! never grows the table, so a test's expected pixels are exactly the indices
//! it wrote. The dictionary and code-widening paths get their own encoder
//! ([`lzw_compressed`]), which is the real algorithm and so exercises the
//! not-yet-defined-code case and the width steps.

use alloc::vec;
use alloc::vec::Vec;

use super::{interlaced_row, pass_rows};
use crate::{DecodeError, DecodeLimits, Sequence, SequenceKind};

/// Limits generous enough for every fixture here.
fn limits() -> DecodeLimits {
    DecodeLimits::new(256, 256, 256 * 256, 0)
}

/// A three-entry palette: red, green, blue, padded to the four entries a
/// colour-table size of `1` declares.
const PALETTE_4: [u8; 12] = [
    0xFF, 0x00, 0x00, // 0 red
    0x00, 0xFF, 0x00, // 1 green
    0x00, 0x00, 0xFF, // 2 blue
    0xFF, 0xFF, 0x00, // 3 yellow
];

/// Opaque RGBA for the `PALETTE_4` entry `index`.
fn colour(index: u8) -> [u8; 4] {
    let at = usize::from(index) * 3;
    [PALETTE_4[at], PALETTE_4[at + 1], PALETTE_4[at + 2], 0xFF]
}

/// Fully transparent, which is what an untouched or cleared canvas pixel is.
const CLEAR: [u8; 4] = [0, 0, 0, 0];

/// The header and Logical Screen Descriptor, with `PALETTE_4` as the global
/// colour table when `global` is set.
fn header(width: u16, height: u16, global: bool) -> Vec<u8> {
    let mut out = b"GIF89a".to_vec();
    out.extend_from_slice(&width.to_le_bytes());
    out.extend_from_slice(&height.to_le_bytes());
    // A colour-table size of 1 declares four entries; bit 7 flags the table.
    out.push(if global { 0x81 } else { 0x01 });
    out.push(0); // background colour index
    out.push(0); // pixel aspect ratio
    if global {
        out.extend_from_slice(&PALETTE_4);
    }
    out
}

/// A Graphic Control Extension.
fn graphic_control(disposal: u8, delay_cs: u16, transparent: Option<u8>) -> Vec<u8> {
    let mut out = vec![0x21, 0xF9, 0x04];
    out.push((disposal << 2) | u8::from(transparent.is_some()));
    out.extend_from_slice(&delay_cs.to_le_bytes());
    out.push(transparent.unwrap_or(0));
    out.push(0);
    out
}

/// The `NETSCAPE2.0` animation-loop Application Extension.
fn netscape_loop(loops: u16) -> Vec<u8> {
    let mut out = vec![0x21, 0xFF, 0x0B];
    out.extend_from_slice(b"NETSCAPE2.0");
    out.push(0x03);
    out.push(0x01);
    out.extend_from_slice(&loops.to_le_bytes());
    out.push(0);
    out
}

/// A comment extension, which a decoder walks past.
fn comment(text: &[u8]) -> Vec<u8> {
    let mut out = vec![0x21, 0xFE];
    out.push(u8::try_from(text.len()).expect("short"));
    out.extend_from_slice(text);
    out.push(0);
    out
}

/// The code bytes inside a single-sub-block framing, for a test that reframes
/// them.
fn unframe(framed: &[u8]) -> &[u8] {
    let len = usize::from(framed[0]);
    &framed[1..=len]
}

/// Wrap a byte stream in data sub-blocks, terminator included.
fn sub_blocks(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    for block in data.chunks(255) {
        out.push(u8::try_from(block.len()).expect("at most 255"));
        out.extend_from_slice(block);
    }
    out.push(0);
    out
}

/// A little-endian bit writer, which is how GIF packs LZW codes.
struct Bits {
    bytes: Vec<u8>,
    accumulator: u32,
    held: u32,
}

impl Bits {
    fn new() -> Self {
        Self {
            bytes: Vec::new(),
            accumulator: 0,
            held: 0,
        }
    }

    fn push(&mut self, code: u16, width: u32) {
        self.accumulator |= u32::from(code) << self.held;
        self.held += width;
        while self.held >= 8 {
            self.bytes
                .push(u8::try_from(self.accumulator & 0xFF).expect("masked"));
            self.accumulator >>= 8;
            self.held -= 8;
        }
    }

    fn finish(mut self) -> Vec<u8> {
        if self.held > 0 {
            self.bytes
                .push(u8::try_from(self.accumulator & 0xFF).expect("masked"));
        }
        self.bytes
    }
}

/// A GIF LZW code stream, written at the widths a decoder will read it at.
///
/// A decoder adds one dictionary entry per code after the first since a clear
/// — whether or not the writer used the table — and widens its reads once
/// that fills the current code space. A writer that ignored the schedule
/// would produce a stream no conforming decoder could follow, so it lives
/// here once and every fixture goes through it.
///
/// A compressor's own table runs exactly one entry ahead of the decoder's,
/// which is what makes its highest emittable code the very one the decoder is
/// about to define; the *width* schedule is therefore identical, so a
/// compressing fixture writes through this too.
struct CodeStream {
    bits: Bits,
    root_bits: u32,
    width: u32,
    next: u16,
    since_clear: u32,
}

impl CodeStream {
    fn new(min_code_size: u8) -> Self {
        let root_bits = u32::from(min_code_size);
        Self {
            bits: Bits::new(),
            root_bits,
            width: root_bits + 1,
            next: (1u16 << root_bits) + 2,
            since_clear: 0,
        }
    }

    fn clear_code(&self) -> u16 {
        1u16 << self.root_bits
    }

    /// The clear code, read at the width in force and resetting it.
    fn clear(&mut self) {
        let clear = self.clear_code();
        self.bits.push(clear, self.width);
        self.width = self.root_bits + 1;
        self.next = clear + 2;
        self.since_clear = 0;
    }

    fn code(&mut self, code: u16) {
        self.bits.push(code, self.width);
        self.since_clear += 1;
        if self.since_clear >= 2 && usize::from(self.next) < crate::lzw::MAX_CODES {
            self.next += 1;
            if u32::from(self.next) >= (1u32 << self.width)
                && self.width < crate::lzw::MAX_CODE_BITS
            {
                self.width += 1;
            }
        }
    }

    fn end(&mut self) {
        let end = self.clear_code() + 1;
        self.bits.push(end, self.width);
    }

    fn finish(self) -> Vec<u8> {
        sub_blocks(&self.bits.finish())
    }
}

/// Encode `indices` as GIF LZW that uses no dictionary entry of its own: a
/// clear code, one root code per pixel, then the end code.
///
/// The decoder still runs its whole table-growth path over this, but the
/// *output* is the indices verbatim, so a test states its expected pixels
/// directly.
fn lzw_literal(indices: &[u8], min_code_size: u8) -> Vec<u8> {
    let mut stream = CodeStream::new(min_code_size);
    stream.clear();
    for &index in indices {
        stream.code(u16::from(index));
    }
    stream.end();
    stream.finish()
}

/// Encode `indices` with the real LZW algorithm, so the stream exercises the
/// dictionary, the code-width steps, and the not-yet-defined-code case a run
/// of repeats produces.
fn lzw_compressed(indices: &[u8], min_code_size: u8) -> Vec<u8> {
    let clear = 1u16 << min_code_size;
    let mut stream = CodeStream::new(min_code_size);
    stream.clear();
    // (prefix code, suffix byte) -> code, searched linearly: a test encoder
    // is not a hot path.
    let mut table: Vec<((u16, u8), u16)> = Vec::new();
    let mut next = clear + 2;
    let Some((&first, rest)) = indices.split_first() else {
        stream.end();
        return stream.finish();
    };
    let mut current = u16::from(first);
    for &byte in rest {
        if let Some(&(_, code)) = table.iter().find(|&&(key, _)| key == (current, byte)) {
            current = code;
        } else {
            stream.code(current);
            if usize::from(next) < crate::lzw::MAX_CODES {
                table.push(((current, byte), next));
                next += 1;
            }
            current = u16::from(byte);
        }
    }
    stream.code(current);
    stream.end();
    stream.finish()
}

/// One frame's Image Descriptor and LZW data.
struct FrameSpec<'a> {
    left: u16,
    top: u16,
    width: u16,
    height: u16,
    interlaced: bool,
    local_palette: bool,
    indices: &'a [u8],
    compressed: bool,
}

impl FrameSpec<'_> {
    /// A whole-screen, non-interlaced frame over the global palette.
    fn whole(width: u16, height: u16, indices: &[u8]) -> FrameSpec<'_> {
        FrameSpec {
            left: 0,
            top: 0,
            width,
            height,
            interlaced: false,
            local_palette: false,
            indices,
            compressed: false,
        }
    }

    fn bytes(&self) -> Vec<u8> {
        let mut out = vec![0x2C];
        out.extend_from_slice(&self.left.to_le_bytes());
        out.extend_from_slice(&self.top.to_le_bytes());
        out.extend_from_slice(&self.width.to_le_bytes());
        out.extend_from_slice(&self.height.to_le_bytes());
        let mut packed = 0x01u8; // colour-table size 1 (four entries)
        if self.local_palette {
            packed |= 0x80;
        }
        if self.interlaced {
            packed |= 0x40;
        }
        out.push(packed);
        if self.local_palette {
            // The same four colours, rotated one place, so a local table is
            // distinguishable from the global one.
            let mut rotated = Vec::new();
            for entry in 0..4u8 {
                let at = usize::from((entry + 1) % 4) * 3;
                rotated.extend_from_slice(&PALETTE_4[at..at + 3]);
            }
            out.extend_from_slice(&rotated);
        }
        out.push(2); // LZW minimum code size
        out.extend_from_slice(&if self.compressed {
            lzw_compressed(self.indices, 2)
        } else {
            lzw_literal(self.indices, 2)
        });
        out
    }
}

/// The colour a local colour table maps `index` to: the global palette
/// rotated one place, matching [`FrameSpec::bytes`].
fn local_colour(index: u8) -> [u8; 4] {
    colour((index + 1) % 4)
}

/// Assemble a whole file from pre-built block bytes.
fn file(parts: &[&[u8]]) -> Vec<u8> {
    let mut out = Vec::new();
    for part in parts {
        out.extend_from_slice(part);
    }
    out.push(0x3B);
    out
}

/// Decode every frame of `bytes`, answering each frame's `(delay_ns, pixels)`.
fn frames(bytes: &[u8]) -> Result<Vec<(u64, Vec<u8>)>, DecodeError> {
    let mut sequence = Sequence::open(bytes, &limits())?;
    let mut out = Vec::new();
    while let Some(frame) = sequence.next_frame()? {
        out.push((frame.delay_ns(), frame.pixels().to_vec()));
    }
    Ok(out)
}

/// The expected canvas from a list of per-pixel RGBA quads.
fn canvas(quads: &[[u8; 4]]) -> Vec<u8> {
    quads.iter().flatten().copied().collect()
}

#[test]
fn a_single_frame_decodes_its_indices_through_the_global_palette() {
    let indices = [0u8, 1, 2, 3];
    let gif = file(&[
        &header(2, 2, true),
        &FrameSpec::whole(2, 2, &indices).bytes(),
    ]);
    let decoded = frames(&gif).expect("decodes");
    assert_eq!(decoded.len(), 1);
    assert_eq!(
        decoded[0].1,
        canvas(&[colour(0), colour(1), colour(2), colour(3)])
    );
    assert_eq!(decoded[0].0, 0);
}

#[test]
fn the_still_entry_point_returns_the_first_composited_frame() {
    let gif = file(&[
        &header(2, 1, true),
        &FrameSpec::whole(2, 1, &[2u8, 0]).bytes(),
        &FrameSpec::whole(2, 1, &[1u8, 1]).bytes(),
    ]);
    let image = crate::decode(&gif, &limits()).expect("decodes");
    assert_eq!((image.width(), image.height()), (2, 1));
    assert_eq!(image.pixels(), canvas(&[colour(2), colour(0)]));
    // No reduced-scale decode exists for GIF, so a fitted decode is the same
    // picture whatever box is asked for.
    let fitted = crate::decode_fitted(&gif, &limits(), crate::FitBox::new(1, 1)).expect("decodes");
    assert_eq!(fitted, image);
}

#[test]
fn probe_reports_the_logical_screen_without_decoding() {
    // Deliberately truncated after the header: a probe reads no block chain,
    // so it still answers.
    let gif = header(300, 7, true);
    let info = crate::probe(&gif).expect("probes");
    assert_eq!(info.format(), crate::ImageFormat::Gif);
    assert_eq!((info.width(), info.height()), (300, 7));
}

#[test]
fn a_local_colour_table_overrides_the_global_one_for_its_frame() {
    let mut local = FrameSpec::whole(2, 1, &[0u8, 1]);
    local.local_palette = true;
    let gif = file(&[
        &header(2, 1, true),
        &FrameSpec::whole(2, 1, &[0u8, 1]).bytes(),
        &graphic_control(1, 0, None),
        &local.bytes(),
    ]);
    let decoded = frames(&gif).expect("decodes");
    assert_eq!(decoded[0].1, canvas(&[colour(0), colour(1)]));
    assert_eq!(decoded[1].1, canvas(&[local_colour(0), local_colour(1)]));
}

#[test]
fn a_frame_with_no_table_at_all_is_refused() {
    let gif = file(&[
        &header(2, 1, false),
        &FrameSpec::whole(2, 1, &[0u8, 1]).bytes(),
    ]);
    assert_eq!(frames(&gif), Err(DecodeError::GifMissingColourTable));
}

#[test]
fn an_index_past_the_end_of_the_table_is_refused() {
    // A minimum code size of 2 admits indices 0..=3 as codes, and the table
    // declared here holds four entries, so a fifth index needs a wider code
    // size to even be expressible.
    let mut out = header(2, 1, false);
    out.push(0x2C);
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&2u16.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.push(0x80); // local table, size 0: two entries
    out.extend_from_slice(&PALETTE_4[..6]);
    out.push(2);
    out.extend_from_slice(&lzw_literal(&[0u8, 3], 2));
    let gif = file(&[&out]);
    assert_eq!(frames(&gif), Err(DecodeError::GifPaletteIndexOutOfRange));
}

#[test]
fn a_transparent_index_leaves_the_canvas_showing_through() {
    let gif = file(&[
        &header(3, 1, true),
        &FrameSpec::whole(3, 1, &[1u8, 1, 1]).bytes(),
        &graphic_control(1, 0, Some(1)),
        &FrameSpec::whole(3, 1, &[2u8, 1, 0]).bytes(),
    ]);
    let decoded = frames(&gif).expect("decodes");
    assert_eq!(
        decoded[1].1,
        canvas(&[colour(2), colour(1), colour(0)]),
        "the transparent middle pixel keeps the first frame's green"
    );
}

#[test]
fn an_untouched_canvas_pixel_is_fully_transparent() {
    let mut small = FrameSpec::whole(1, 1, &[2u8]);
    small.left = 1;
    let gif = file(&[&header(2, 1, true), &small.bytes()]);
    let decoded = frames(&gif).expect("decodes");
    assert_eq!(decoded[0].1, canvas(&[CLEAR, colour(2)]));
}

#[test]
fn keep_disposal_leaves_the_previous_frame_standing() {
    let mut second = FrameSpec::whole(1, 1, &[3u8]);
    second.left = 1;
    let gif = file(&[
        &header(2, 1, true),
        &graphic_control(1, 0, None),
        &FrameSpec::whole(2, 1, &[0u8, 1]).bytes(),
        &graphic_control(1, 0, None),
        &second.bytes(),
    ]);
    let decoded = frames(&gif).expect("decodes");
    assert_eq!(decoded[1].1, canvas(&[colour(0), colour(3)]));
}

#[test]
fn clear_disposal_wipes_only_the_disposing_frames_own_area() {
    let mut first = FrameSpec::whole(1, 1, &[0u8]);
    first.left = 0;
    let mut second = FrameSpec::whole(1, 1, &[1u8]);
    second.left = 1;
    let mut third = FrameSpec::whole(1, 1, &[3u8]);
    third.left = 2;
    let gif = file(&[
        &header(3, 1, true),
        &graphic_control(1, 0, None),
        &first.bytes(),
        // The middle pixel is drawn and then cleared again.
        &graphic_control(2, 0, None),
        &second.bytes(),
        &graphic_control(1, 0, None),
        &third.bytes(),
    ]);
    let decoded = frames(&gif).expect("decodes");
    assert_eq!(decoded[1].1, canvas(&[colour(0), colour(1), CLEAR]));
    assert_eq!(
        decoded[2].1,
        canvas(&[colour(0), CLEAR, colour(3)]),
        "the cleared pixel goes transparent, and the first frame's is untouched"
    );
}

#[test]
fn previous_disposal_restores_what_the_frame_covered() {
    let mut overlay = FrameSpec::whole(1, 1, &[1u8]);
    overlay.left = 1;
    let mut last = FrameSpec::whole(1, 1, &[3u8]);
    last.left = 0;
    let gif = file(&[
        &header(3, 1, true),
        &graphic_control(1, 0, None),
        &FrameSpec::whole(3, 1, &[0u8, 2, 0]).bytes(),
        // Drawn over the middle pixel, then restored to what was under it.
        &graphic_control(3, 0, None),
        &overlay.bytes(),
        &graphic_control(1, 0, None),
        &last.bytes(),
    ]);
    let decoded = frames(&gif).expect("decodes");
    assert_eq!(decoded[1].1, canvas(&[colour(0), colour(1), colour(0)]));
    assert_eq!(
        decoded[2].1,
        canvas(&[colour(3), colour(2), colour(0)]),
        "the middle pixel is the blue that was under the overlay"
    );
}

#[test]
fn a_reserved_disposal_method_is_refused() {
    for reserved in 4u8..=7 {
        let gif = file(&[
            &header(1, 1, true),
            &graphic_control(reserved, 0, None),
            &FrameSpec::whole(1, 1, &[0u8]).bytes(),
        ]);
        assert_eq!(
            frames(&gif),
            Err(DecodeError::GifReservedDisposal),
            "disposal {reserved} is reserved"
        );
    }
}

#[test]
fn a_delay_is_reported_in_nanoseconds_exactly_as_declared() {
    let gif = file(&[
        &header(1, 1, true),
        &graphic_control(1, 7, None),
        &FrameSpec::whole(1, 1, &[0u8]).bytes(),
        // A zero delay is reported as zero rather than clamped: how fast to
        // play is the player's decision.
        &graphic_control(1, 0, None),
        &FrameSpec::whole(1, 1, &[1u8]).bytes(),
    ]);
    let decoded = frames(&gif).expect("decodes");
    assert_eq!(decoded[0].0, 70_000_000);
    assert_eq!(decoded[1].0, 0);
}

#[test]
fn the_loop_count_comes_from_the_animation_extension() {
    let body = FrameSpec::whole(1, 1, &[0u8]).bytes();
    let kind = |bytes: &[u8]| {
        Sequence::open(bytes, &limits())
            .expect("opens")
            .info()
            .kind()
    };
    assert_eq!(
        kind(&file(&[&header(1, 1, true), &body])),
        SequenceKind::Animation {
            loop_count: Some(1)
        },
        "with no extension the format plays a sequence once"
    );
    assert_eq!(
        kind(&file(&[&header(1, 1, true), &netscape_loop(3), &body])),
        SequenceKind::Animation {
            loop_count: Some(3)
        }
    );
    assert_eq!(
        kind(&file(&[&header(1, 1, true), &netscape_loop(0), &body])),
        SequenceKind::Animation { loop_count: None },
        "a declared count of zero means for ever"
    );
}

#[test]
fn comments_and_unknown_extensions_are_walked_past() {
    // 0x2B is not a label any specification defines; its payload still frames
    // as data sub-blocks, so a decoder walks past it.
    let unknown = {
        let mut out = vec![0x21, 0x2B, 0x02, 0xAA, 0xBB, 0x00];
        out.shrink_to_fit();
        out
    };
    let gif = file(&[
        &header(1, 1, true),
        &comment(b"a comment"),
        &unknown,
        &FrameSpec::whole(1, 1, &[2u8]).bytes(),
        &comment(b"another"),
    ]);
    let decoded = frames(&gif).expect("decodes");
    assert_eq!(decoded.len(), 1);
    assert_eq!(decoded[0].1, canvas(&[colour(2)]));
}

#[test]
fn a_plain_text_block_consumes_the_control_it_follows() {
    // The plain-text block is a rendered block this decoder draws nothing
    // for, so the transparency the control declares must not reach the frame
    // after it.
    let plain_text = {
        let mut out = vec![0x21, 0x01, 0x0C];
        out.extend_from_slice(&[0u8; 12]);
        out.extend_from_slice(&[0x03, b'h', b'i', b'!', 0x00]);
        out
    };
    let gif = file(&[
        &header(2, 1, true),
        &FrameSpec::whole(2, 1, &[1u8, 1]).bytes(),
        &graphic_control(1, 0, Some(1)),
        &plain_text,
        &FrameSpec::whole(2, 1, &[1u8, 0]).bytes(),
    ]);
    let decoded = frames(&gif).expect("decodes");
    assert_eq!(
        decoded[1].1,
        canvas(&[colour(1), colour(0)]),
        "index 1 is opaque green here, not the transparency the text block took"
    );
}

#[test]
fn an_interlaced_frame_is_reassembled_in_pass_order() {
    // Eight rows: the passes visit 0, then 4, then 2 and 6, then 1, 3, 5, 7.
    // One index per row, in that visiting order.
    let order = [0u32, 4, 2, 6, 1, 3, 5, 7];
    // A minimum code size of 2 admits indices 0..=3, so the eight rows cycle
    // through the four colours; a non-interlaced decode of the same bytes
    // would put them in a different order, which is what makes this
    // discriminating.
    let indices: Vec<u8> = (0..8u8).map(|row| row % 4).collect();
    let mut spec = FrameSpec::whole(1, 8, &indices);
    spec.interlaced = true;
    let gif = file(&[&header(1, 8, true), &spec.bytes()]);
    let decoded = frames(&gif).expect("decodes");
    let mut expected = vec![CLEAR; 8];
    for (stream_row, &row) in order.iter().enumerate() {
        let index = u8::try_from(stream_row).expect("small") % 4;
        expected[usize::try_from(row).expect("small")] = colour(index);
    }
    assert_eq!(decoded[0].1, canvas(&expected));
    let mut plain = FrameSpec::whole(1, 8, &indices);
    plain.interlaced = false;
    let straight = frames(&file(&[&header(1, 8, true), &plain.bytes()])).expect("decodes");
    assert_ne!(straight[0].1, decoded[0].1);
}

#[test]
fn the_interlace_pass_map_covers_every_row_exactly_once() {
    for height in 1..=40u32 {
        let mut seen = vec![false; usize::try_from(height).expect("small")];
        let total: u32 = super::INTERLACE_PASSES
            .iter()
            .map(|&(start, step)| pass_rows(height, start, step))
            .sum();
        assert_eq!(total, height, "the four passes cover {height} rows");
        for row in 0..height {
            let mapped = interlaced_row(row, height);
            let slot = &mut seen[usize::try_from(mapped).expect("in range")];
            assert!(!*slot, "row {mapped} mapped twice at height {height}");
            *slot = true;
        }
        assert!(seen.into_iter().all(|hit| hit));
    }
}

#[test]
fn a_compressed_stream_decodes_to_the_same_pixels_as_a_literal_one() {
    // A run of repeats is what drives the dictionary and the
    // not-yet-defined-code case; sixteen columns of it also crosses the
    // first code-width step.
    let indices: Vec<u8> = (0..64u8).map(|at| (at / 16) % 4).collect();
    let literal = file(&[
        &header(16, 4, true),
        &FrameSpec::whole(16, 4, &indices).bytes(),
    ]);
    let mut spec = FrameSpec::whole(16, 4, &indices);
    spec.compressed = true;
    let compressed = file(&[&header(16, 4, true), &spec.bytes()]);
    assert_eq!(
        frames(&compressed).expect("decodes"),
        frames(&literal).expect("decodes")
    );
}

#[test]
fn a_long_compressed_stream_crosses_every_code_width_step() {
    // Enough distinct short runs to push the code width from 3 bits to well
    // past 8, so every widening step is taken.
    let mut indices = Vec::new();
    for run in 0..600u32 {
        let index = u8::try_from(run % 4).expect("small");
        for _ in 0..=(run % 7) {
            indices.push(index);
        }
    }
    indices.truncate(64 * 32);
    indices.resize(64 * 32, 1);
    let mut spec = FrameSpec::whole(64, 32, &indices);
    spec.compressed = true;
    let gif = file(&[&header(64, 32, true), &spec.bytes()]);
    let decoded = frames(&gif).expect("decodes");
    let expected: Vec<[u8; 4]> = indices.iter().map(|&index| colour(index)).collect();
    assert_eq!(decoded[0].1, canvas(&expected));
}

#[test]
fn a_stream_that_ends_before_its_last_pixel_is_refused() {
    // The descriptor claims 4x4 while the LZW stream carries eight pixels.
    let short = [0u8; 8];
    let mut out = vec![0x2C];
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&4u16.to_le_bytes());
    out.extend_from_slice(&4u16.to_le_bytes());
    out.push(0x01);
    out.push(2);
    out.extend_from_slice(&lzw_literal(&short, 2));
    let gif = file(&[&header(4, 4, true), &out]);
    assert_eq!(frames(&gif), Err(DecodeError::GifTruncatedImageData));
}

#[test]
fn output_past_the_frames_last_pixel_is_dropped_rather_than_overrunning() {
    let long = [1u8; 16];
    let mut out = vec![0x2C];
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&2u16.to_le_bytes());
    out.extend_from_slice(&2u16.to_le_bytes());
    out.push(0x01);
    out.push(2);
    out.extend_from_slice(&lzw_literal(&long, 2));
    let gif = file(&[&header(2, 2, true), &out]);
    let decoded = frames(&gif).expect("decodes");
    assert_eq!(decoded[0].1, canvas(&[colour(1); 4]));
}

#[test]
fn a_code_the_table_cannot_resolve_is_refused() {
    // Code 6 is the first the table could define, and nothing has defined it
    // yet at the start of a stream.
    let mut stream = CodeStream::new(2);
    stream.clear();
    stream.code(7); // past even the code this step would define
    stream.end();
    let mut out = vec![0x2C];
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&2u16.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.push(0x01);
    out.push(2);
    out.extend_from_slice(&stream.finish());
    let gif = file(&[&header(2, 1, true), &out]);
    assert_eq!(frames(&gif), Err(DecodeError::GifInvalidCode));
}

#[test]
fn a_clear_code_mid_stream_resets_the_table() {
    // Two literal runs separated by a clear: the second run's codes are roots
    // again, so a decoder that failed to reset would resolve them wrongly.
    let mut stream = CodeStream::new(2);
    stream.clear();
    for index in [0u16, 1, 2] {
        stream.code(index);
    }
    stream.clear();
    for index in [3u16, 2, 1] {
        stream.code(index);
    }
    stream.end();
    let mut out = vec![0x2C];
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&6u16.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.push(0x01);
    out.push(2);
    out.extend_from_slice(&stream.finish());
    let gif = file(&[&header(6, 1, true), &out]);
    let decoded = frames(&gif).expect("decodes");
    assert_eq!(
        decoded[0].1,
        canvas(&[
            colour(0),
            colour(1),
            colour(2),
            colour(3),
            colour(2),
            colour(1)
        ])
    );
}

#[test]
fn a_code_stream_split_across_sub_blocks_reads_continuously() {
    // One byte per sub-block, so every code straddles a boundary.
    let indices: Vec<u8> = (0..40u8).map(|at| at % 4).collect();
    let literal = lzw_literal(&indices, 2);
    // The same code stream, reframed one byte per sub-block.
    let mut split = Vec::new();
    for &byte in unframe(&literal) {
        split.push(1u8);
        split.push(byte);
    }
    split.push(0);
    assert_ne!(split, literal, "the framing genuinely differs");

    let descriptor = |data: &[u8]| {
        let mut out = vec![0x2C];
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&8u16.to_le_bytes());
        out.extend_from_slice(&5u16.to_le_bytes());
        out.push(0x01);
        out.push(2);
        out.extend_from_slice(data);
        out
    };
    let one = frames(&file(&[&header(8, 5, true), &descriptor(&literal)])).expect("decodes");
    let many = frames(&file(&[&header(8, 5, true), &descriptor(&split)])).expect("decodes");
    assert_eq!(one, many);
}

#[test]
fn a_rewind_replays_the_sequence_from_a_blank_canvas() {
    let mut second = FrameSpec::whole(1, 1, &[3u8]);
    second.left = 1;
    let gif = file(&[
        &header(2, 1, true),
        &graphic_control(1, 0, None),
        &FrameSpec::whole(1, 1, &[0u8]).bytes(),
        &graphic_control(1, 0, None),
        &second.bytes(),
    ]);
    let mut sequence = Sequence::open(&gif, &limits()).expect("opens");
    assert_eq!(sequence.info().count(), 2);
    let mut first_pass = Vec::new();
    while let Some(frame) = sequence.next_frame().expect("decodes") {
        first_pass.push((frame.index(), frame.pixels().to_vec()));
    }
    sequence.rewind();
    let mut second_pass = Vec::new();
    while let Some(frame) = sequence.next_frame().expect("decodes") {
        second_pass.push((frame.index(), frame.pixels().to_vec()));
    }
    assert_eq!(first_pass, second_pass);
    assert_eq!(first_pass[0].0, 0);
    assert_eq!(first_pass[1].0, 1);
}

#[test]
fn a_still_picture_is_the_one_page_case_of_the_same_shape() {
    let png = crate::tests::minimal_png();
    let mut sequence = Sequence::open(&png, &limits()).expect("opens");
    assert_eq!(sequence.info().count(), 1);
    assert_eq!(sequence.info().kind(), SequenceKind::Pages);
    assert_eq!(sequence.info().format(), crate::ImageFormat::Png);
    let first = sequence.next_frame().expect("decodes").expect("one page");
    assert_eq!((first.index(), first.delay_ns()), (0, 0));
    assert_eq!(first.pixels().len(), 2 * 2 * 4);
    assert!(sequence.next_frame().expect("no more").is_none());
    sequence.rewind();
    assert!(
        sequence.next_frame().expect("decodes").is_some(),
        "a rewind serves the one page again"
    );
}

#[test]
fn a_screen_over_the_limits_is_refused_before_the_canvas_is_allocated() {
    let gif = file(&[
        &header(300, 1, true),
        &FrameSpec::whole(300, 1, &[0u8; 300]).bytes(),
    ]);
    assert_eq!(
        Sequence::open(&gif, &limits()).err(),
        Some(DecodeError::WidthExceedsLimit)
    );
    assert_eq!(
        crate::decode(&gif, &limits()).err(),
        Some(DecodeError::WidthExceedsLimit)
    );
}

#[test]
fn a_frame_outside_the_logical_screen_is_refused() {
    let mut spec = FrameSpec::whole(2, 1, &[0u8, 1]);
    spec.left = 1;
    let gif = file(&[&header(2, 1, true), &spec.bytes()]);
    assert_eq!(frames(&gif), Err(DecodeError::GifFrameOutsideScreen));
}

#[test]
fn a_zero_sided_frame_or_screen_is_refused() {
    let mut spec = FrameSpec::whole(2, 1, &[0u8, 1]);
    spec.width = 0;
    let gif = file(&[&header(2, 1, true), &spec.bytes()]);
    assert_eq!(frames(&gif), Err(DecodeError::GifZeroFrame));

    let zero_screen = file(&[&header(0, 1, true), &FrameSpec::whole(1, 1, &[0u8]).bytes()]);
    assert_eq!(frames(&zero_screen), Err(DecodeError::ZeroDimension));
}

#[test]
fn an_out_of_range_code_size_is_refused() {
    for size in [0u8, 1, 9, 255] {
        let mut out = vec![0x2C];
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&1u16.to_le_bytes());
        out.extend_from_slice(&1u16.to_le_bytes());
        out.push(0x01);
        out.push(size);
        out.extend_from_slice(&lzw_literal(&[0u8], 2));
        let gif = file(&[&header(1, 1, true), &out]);
        assert_eq!(
            frames(&gif),
            Err(DecodeError::GifInvalidCodeSize),
            "code size {size}"
        );
    }
}

#[test]
fn a_stream_with_no_image_block_is_refused() {
    let gif = file(&[&header(1, 1, true), &comment(b"nothing to draw")]);
    assert_eq!(frames(&gif), Err(DecodeError::GifNoFrames));
}

#[test]
fn a_bad_signature_or_version_is_refused_with_the_reason() {
    assert_eq!(
        Sequence::open(b"GIF99a", &limits()).err(),
        Some(DecodeError::GifUnknownVersion)
    );
    // Not sniffed as a GIF at all, so the crate's own dispatch refuses first.
    assert_eq!(
        Sequence::open(b"GIx89a\0\0\0\0\0\0\0", &limits()).err(),
        Some(DecodeError::UnknownFormat)
    );
    // Sniffed, but the magic check inside the decoder is what a caller
    // reaching the module directly meets.
    assert_eq!(super::probe(b"GIx89a"), Err(DecodeError::GifBadSignature));
}

#[test]
fn a_truncated_stream_is_refused_at_every_stage() {
    let whole = file(&[
        &header(2, 2, true),
        &graphic_control(1, 5, None),
        &FrameSpec::whole(2, 2, &[0u8, 1, 2, 3]).bytes(),
    ]);
    for cut in 1..whole.len() {
        let result = frames(&whole[..cut]);
        assert!(
            result.is_err(),
            "a stream cut at {cut} of {} bytes decoded",
            whole.len()
        );
    }
    assert!(frames(&whole).is_ok());
}

#[test]
fn an_unrecognised_block_introducer_is_refused() {
    let mut gif = header(1, 1, true);
    gif.push(0x99);
    assert_eq!(frames(&gif), Err(DecodeError::GifUnknownBlock));
}

#[test]
fn a_malformed_extension_block_size_is_refused() {
    // A graphic control extension whose declared block size is not four.
    let mut bad = vec![0x21u8, 0xF9, 0x03, 0x00, 0x00, 0x00, 0x00];
    bad.push(0);
    let gif = file(&[
        &header(1, 1, true),
        &bad,
        &FrameSpec::whole(1, 1, &[0u8]).bytes(),
    ]);
    assert_eq!(frames(&gif), Err(DecodeError::GifMalformedExtension));

    // And one whose terminator is missing.
    let mut unterminated = vec![0x21u8, 0xF9, 0x04, 0x00, 0x00, 0x00, 0x00];
    unterminated.push(0x2C);
    let gif = file(&[&header(1, 1, true), &unterminated]);
    assert_eq!(frames(&gif), Err(DecodeError::GifMalformedExtension));
}

#[test]
fn more_frames_than_the_containment_bound_are_refused() {
    let body = FrameSpec::whole(1, 1, &[0u8]).bytes();
    let mut gif = header(1, 1, true);
    for _ in 0..=crate::MAX_ANIMATION_FRAMES {
        gif.extend_from_slice(&body);
    }
    gif.push(0x3B);
    assert_eq!(frames(&gif), Err(DecodeError::GifTooManyFrames));
}

#[test]
fn bytes_after_the_trailer_are_ignored() {
    let mut gif = file(&[&header(1, 1, true), &FrameSpec::whole(1, 1, &[2u8]).bytes()]);
    gif.extend_from_slice(b"padding some writers append");
    let decoded = frames(&gif).expect("decodes");
    assert_eq!(decoded[0].1, canvas(&[colour(2)]));
}

/// A 256-entry colour table whose entry `index` is `(index, 255 - index, 0)`,
/// so a wrongly-resolved index is visible.
fn palette_256() -> Vec<u8> {
    (0..256u16)
        .flat_map(|index| {
            let index = u8::try_from(index).expect("in range");
            [index, u8::MAX - index, 0]
        })
        .collect()
}

/// Opaque RGBA for a [`palette_256`] entry.
fn colour_256(index: u8) -> [u8; 4] {
    [index, u8::MAX - index, 0, u8::MAX]
}

/// A whole-screen frame over a 256-entry local table at code size 8.
fn frame_256(width: u16, height: u16, indices: &[u8], compressed: bool) -> Vec<u8> {
    let mut out = vec![0x2C];
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&width.to_le_bytes());
    out.extend_from_slice(&height.to_le_bytes());
    out.push(0x87); // local table, size 7: 256 entries
    out.extend_from_slice(&palette_256());
    out.push(8);
    out.extend_from_slice(&if compressed {
        lzw_compressed(indices, 8)
    } else {
        lzw_literal(indices, 8)
    });
    out
}

#[test]
fn the_widest_code_size_and_a_full_256_entry_table_decode() {
    let indices: Vec<u8> = (0..256u16)
        .map(|index| u8::try_from(index).expect("in range"))
        .collect();
    let gif = file(&[&header(16, 16, false), &frame_256(16, 16, &indices, false)]);
    let decoded = frames(&gif).expect("decodes");
    let expected: Vec<[u8; 4]> = indices.iter().map(|&index| colour_256(index)).collect();
    assert_eq!(decoded[0].1, canvas(&expected));
}

#[test]
fn a_stream_that_fills_the_table_keeps_decoding_at_twelve_bits() {
    // Enough non-repeating data at code size 8 to define far more than the
    // 4096 entries the format's widest code can address. Neither the writer
    // nor the decoder emits a clear code, so both must carry on against the
    // table as it stands — the deferred clear real encoders rely on.
    let mut state = 0x2545_F491_4F6C_DD1Du64;
    let indices: Vec<u8> = (0..128 * 96)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            u8::try_from(state & 0xFF).expect("masked")
        })
        .collect();
    let compressed = file(&[&header(128, 96, false), &frame_256(128, 96, &indices, true)]);
    let literal = file(&[
        &header(128, 96, false),
        &frame_256(128, 96, &indices, false),
    ]);
    let expected: Vec<[u8; 4]> = indices.iter().map(|&index| colour_256(index)).collect();
    assert_eq!(frames(&literal).expect("decodes")[0].1, canvas(&expected));
    assert_eq!(
        frames(&compressed).expect("decodes")[0].1,
        canvas(&expected)
    );
}

#[test]
fn a_frame_may_change_the_code_size_the_one_before_it_used() {
    // The dictionary's root set is the code size's, so a second frame with a
    // narrower one must not resolve a code the first frame's table defined.
    let wide: Vec<u8> = (0..64u16)
        .map(|index| u8::try_from(index * 4).expect("in range"))
        .collect();
    let narrow: Vec<u8> = (0..64u8).map(|at| at % 4).collect();
    let gif = file(&[
        &header(8, 8, true),
        &frame_256(8, 8, &wide, true),
        &graphic_control(1, 0, None),
        &FrameSpec::whole(8, 8, &narrow).bytes(),
    ]);
    let decoded = frames(&gif).expect("decodes");
    let first: Vec<[u8; 4]> = wide.iter().map(|&index| colour_256(index)).collect();
    let second: Vec<[u8; 4]> = narrow.iter().map(|&index| colour(index)).collect();
    assert_eq!(decoded[0].1, canvas(&first));
    assert_eq!(decoded[1].1, canvas(&second));
}

#[test]
fn a_refused_step_is_remembered_until_a_rewind() {
    // The second frame's index is outside its own two-entry local table, so
    // it refuses part-way through compositing — which leaves the canvas
    // describing no whole frame.
    let mut bad = vec![0x2C];
    bad.extend_from_slice(&0u16.to_le_bytes());
    bad.extend_from_slice(&0u16.to_le_bytes());
    bad.extend_from_slice(&2u16.to_le_bytes());
    bad.extend_from_slice(&1u16.to_le_bytes());
    bad.push(0x80); // local table, size 0: two entries
    bad.extend_from_slice(&PALETTE_4[..6]);
    bad.push(2);
    bad.extend_from_slice(&lzw_literal(&[0u8, 3], 2));
    let gif = file(&[
        &header(2, 1, true),
        &FrameSpec::whole(2, 1, &[1u8, 1]).bytes(),
        &graphic_control(1, 0, None),
        &bad,
    ]);
    let mut sequence = Sequence::open(&gif, &limits()).expect("opens");
    let first = sequence
        .next_frame()
        .expect("decodes")
        .expect("a first frame")
        .pixels()
        .to_vec();
    assert_eq!(first, canvas(&[colour(1), colour(1)]));
    assert_eq!(
        sequence.next_frame().err(),
        Some(DecodeError::GifPaletteIndexOutOfRange)
    );
    // Stepping on would composite the third frame onto whatever the refused
    // one left behind, so it answers the same refusal instead.
    assert_eq!(
        sequence.next_frame().err(),
        Some(DecodeError::GifPaletteIndexOutOfRange)
    );
    sequence.rewind();
    assert_eq!(
        sequence
            .next_frame()
            .expect("decodes")
            .expect("a first frame")
            .pixels(),
        first,
        "a rewind puts the sequence back at a blank canvas"
    );
}
