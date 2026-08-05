//! Unit tests for the sandboxed icon-rasterisation service.
//!
//! PNG fixtures are built by hand through the small helpers below,
//! mirroring `lib/image`'s own `png_tests.rs` builder style: `chunk`
//! frames one length/type/payload/CRC-32 chunk, and `zlib_wrap` packs raw
//! (pre-filter) scanline bytes into a genuine zlib stream made of a single
//! STORED deflate block plus a real Adler-32 trailer, so no compressor is
//! needed to produce a stream `tairix_image`'s decoder accepts. This test
//! file cannot reach `tairix_image`'s own (crate-private) chunk/CRC
//! helpers, so both are reimplemented here from the public PNG/zlib
//! specifications rather than duplicating a hand-rolled parser.

use alloc::format;
use alloc::vec;
use alloc::vec::Vec;

use tairix_icon::MAX_ARTWORK_BYTES;
use tairix_log::{Event, Sink};
use tairix_wallpaper::WallpaperFit;

use super::{
    render_wallpaper, render_wallpaper_for_screen, IconRasterFailure, IconRefusal,
    ImageRenderService, WallpaperRefusal, WallpaperRenderFailure, MAX_ICON_SIDE,
};
use crate::host::ParserSandbox;
use crate::loopback::LoopbackLauncher;
use crate::wire::Writer;
use crate::worker::Service;

use super::rasterise_icon;

/// Discards every event (the happy paths log nothing).
struct NullSink;

impl Sink for NullSink {
    fn write_event(&self, _event: &Event<'_>) {}
}

type TestSandbox = ParserSandbox<LoopbackLauncher<fn() -> ImageRenderService>, NullSink>;

fn sandbox() -> TestSandbox {
    ParserSandbox::new(
        LoopbackLauncher::new(ImageRenderService::default as fn() -> ImageRenderService),
        NullSink,
    )
}

/// A minimal SVG icon: one opaque-coloured square covering the whole
/// design grid (the same shape the crate documentation examples use).
fn svg_square(hex: &str) -> Vec<u8> {
    format!(
        r#"<svg viewBox="0 0 10 10"><polygon points="0,0 10,0 10,10 0,10" fill="{hex}"/></svg>"#
    )
    .into_bytes()
}

// ---- a minimal, hand-built PNG fixture (see module doc) -------------------

const PNG_SIGNATURE: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
const IHDR: [u8; 4] = *b"IHDR";
const IDAT: [u8; 4] = *b"IDAT";
const IEND: [u8; 4] = *b"IEND";

/// Standard CRC-32 (the polynomial PNG chunks use), computed over the
/// concatenation of `parts`.
fn crc32_of(parts: &[&[u8]]) -> u32 {
    fn update(mut crc: u32, byte: u8) -> u32 {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            let mask = 0u32.wrapping_sub(crc & 1);
            crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
        }
        crc
    }
    let mut crc = 0xFFFF_FFFFu32;
    for part in parts {
        for &byte in *part {
            crc = update(crc, byte);
        }
    }
    crc ^ 0xFFFF_FFFF
}

fn chunk(chunk_type: [u8; 4], payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let len = u32::try_from(payload.len()).expect("test payload fits a u32 length");
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(&chunk_type);
    out.extend_from_slice(payload);
    let crc = crc32_of(&[&chunk_type, payload]);
    out.extend_from_slice(&crc.to_be_bytes());
    out
}

/// `IHDR` for an 8-bit truecolour-plus-alpha (colour type 6) image, no
/// interlacing.
fn ihdr_payload(width: u32, height: u32) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&width.to_be_bytes());
    out.extend_from_slice(&height.to_be_bytes());
    out.extend_from_slice(&[8, 6, 0, 0, 0]);
    out
}

/// The Adler-32 checksum RFC 1950 requires as the zlib stream trailer.
fn adler32(data: &[u8]) -> u32 {
    let mut a: u32 = 1;
    let mut b: u32 = 0;
    for &byte in data {
        a = (a + u32::from(byte)) % 65521;
        b = (b + a) % 65521;
    }
    (b << 16) | a
}

/// Wrap `data` (raw, pre-filter-reconstruction scanline bytes) in a
/// well-formed zlib stream built entirely from STORED deflate blocks, so
/// no compressor is needed to produce a stream the crate's (transitive)
/// zlib decoder accepts.
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
    out.extend_from_slice(&adler32(data).to_be_bytes());
    out
}

/// Assemble a minimal well-formed 8-bit RGBA PNG: signature, `IHDR`, one
/// `IDAT` wrapping `raw_scanlines`, and `IEND`.
fn build_png(width: u32, height: u32, raw_scanlines: &[u8]) -> Vec<u8> {
    let mut out = PNG_SIGNATURE.to_vec();
    out.extend(chunk(IHDR, &ihdr_payload(width, height)));
    out.extend(chunk(IDAT, &zlib_wrap(raw_scanlines)));
    out.extend(chunk(IEND, &[]));
    out
}

/// Build an RGBA PNG of `width`×`height` whose pixel `(x, y)` is
/// `pixel(x, y)`, filter type `None` on every row.
fn png_with(width: u32, height: u32, pixel: impl Fn(u32, u32) -> [u8; 4]) -> Vec<u8> {
    let mut raw = Vec::new();
    for y in 0..height {
        raw.push(0); // filter type None
        for x in 0..width {
            raw.extend_from_slice(&pixel(x, y));
        }
    }
    build_png(width, height, &raw)
}

// ---- a minimal, hand-built JPEG fixture (see module doc) ------------------

/// Frame a JPEG marker segment: `0xFF`, the marker code, and the 2-byte
/// length that counts itself (ITU-T T.81 §B.1.1.4).
fn jpeg_segment(marker: u8, payload: &[u8]) -> Vec<u8> {
    let mut out = vec![0xFF, marker];
    let len = u16::try_from(payload.len() + 2).expect("test payload fits a segment length");
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(payload);
    out
}

/// A greyscale baseline JPEG of `width`×`height` whose every pixel is
/// exactly mid-grey (128).
///
/// Deliberately the cheapest well-formed stream that can be built at a
/// photographic size: one DC and one AC Huffman table, each carrying a
/// single symbol under a single 1-bit code (ITU-T T.81 §B.2.4.2), so every
/// 8×8 block is a zero DC difference followed by end-of-block — two zero
/// bits — and a zero DC coefficient level-shifts to 128 (§A.3.1). The whole
/// entropy-coded segment is therefore a run of zero bytes, needing no
/// encoder and no per-block work to build.
fn flat_grey_jpeg(width: u16, height: u16) -> Vec<u8> {
    const SOI: u8 = 0xD8;
    const DQT: u8 = 0xDB;
    const DHT: u8 = 0xC4;
    const SOF0: u8 = 0xC0;
    const SOS: u8 = 0xDA;
    const EOI: u8 = 0xD9;

    let mut dqt = vec![0x00]; // 8-bit precision, table 0
    dqt.extend_from_slice(&[1u8; 64]);

    // One code of length 1 (`0`) for symbol 0: DC category 0 (a zero
    // difference, no additional bits) and, in the AC table, end-of-block.
    let huffman_counts = {
        let mut counts = [0u8; 16];
        counts[0] = 1;
        counts
    };
    let mut dc_dht = vec![0x00]; // class 0 (DC), table 0
    dc_dht.extend_from_slice(&huffman_counts);
    dc_dht.push(0x00);
    let mut ac_dht = vec![0x10]; // class 1 (AC), table 0
    ac_dht.extend_from_slice(&huffman_counts);
    ac_dht.push(0x00);

    let mut sof = vec![8]; // 8-bit sample precision
    sof.extend_from_slice(&height.to_be_bytes());
    sof.extend_from_slice(&width.to_be_bytes());
    sof.extend_from_slice(&[1, 1, 0x11, 0]); // one component, 1x1 sampled, table 0
    let sos = vec![1, 1, 0x00, 0, 63, 0x00];

    let blocks = u64::from(width.div_ceil(8)) * u64::from(height.div_ceil(8));
    let bits = blocks * 2;
    let mut entropy = vec![0u8; usize::try_from(bits / 8).expect("fixture fits host memory")];
    let spare = u32::try_from(bits % 8).unwrap_or(0);
    if spare != 0 {
        // Pad the final byte with 1 bits, as an encoder must (§F.1.2.3).
        entropy.push(0xFFu8 >> spare);
    }

    let mut out = vec![0xFF, SOI];
    out.extend(jpeg_segment(DQT, &dqt));
    out.extend(jpeg_segment(DHT, &dc_dht));
    out.extend(jpeg_segment(DHT, &ac_dht));
    out.extend(jpeg_segment(SOF0, &sof));
    out.extend(jpeg_segment(SOS, &sos));
    out.extend(entropy);
    out.extend_from_slice(&[0xFF, EOI]);
    out
}

fn rgba_at(pixels: &[u8], width: u32, x: u32, y: u32) -> [u8; 4] {
    let idx = ((y * width + x) * 4) as usize;
    [
        pixels[idx],
        pixels[idx + 1],
        pixels[idx + 2],
        pixels[idx + 3],
    ]
}

// ---- happy paths ------------------------------------------------------

#[test]
fn a_full_square_svg_icon_rasterises_to_an_exact_uniform_colour() {
    let mut sandbox = sandbox();
    let svg = svg_square("#3070f0");
    let pixels = rasterise_icon(&mut sandbox, 4, &svg).expect("rasterises");
    assert_eq!(pixels.len(), 4 * 4 * 4);
    // The polygon covers the entire design grid, so every pixel of the
    // fully opaque surface un-premultiplies back to the exact fill colour.
    for y in 0..4 {
        for x in 0..4 {
            assert_eq!(
                rgba_at(&pixels, 4, x, y),
                [0x30, 0x70, 0xf0, 255],
                "pixel ({x}, {y})"
            );
        }
    }
}

#[test]
fn a_png_icon_reply_is_exactly_side_squared_times_four_bytes() {
    let mut sandbox = sandbox();
    let png = png_with(3, 3, |_, _| [10, 20, 30, 255]);
    let pixels = rasterise_icon(&mut sandbox, 5, &png).expect("rasterises");
    assert_eq!(pixels.len(), 5 * 5 * 4);
}

#[test]
fn a_two_by_two_checkerboard_downsamples_to_the_exact_midpoint_average() {
    let mut sandbox = sandbox();
    // Two opaque black pixels and two opaque white pixels average, alpha
    // weighted, to opaque mid-grey — a known, hand-checkable box-filter
    // result.
    let png = png_with(2, 2, |x, y| {
        if (x + y) % 2 == 0 {
            [0, 0, 0, 255]
        } else {
            [255, 255, 255, 255]
        }
    });
    let pixels = rasterise_icon(&mut sandbox, 1, &png).expect("rasterises");
    assert_eq!(pixels, vec![128, 128, 128, 255]);
}

#[test]
fn a_wide_source_is_letterboxed_and_centred_with_transparent_padding() {
    let mut sandbox = sandbox();
    let colour = [200, 50, 50, 255];
    // A 4x2 source fitted into a 4x4 square maps 1:1 onto a 4x2 band
    // centred with one fully transparent padding row above and below.
    let png = png_with(4, 2, |_, _| colour);
    let pixels = rasterise_icon(&mut sandbox, 4, &png).expect("rasterises");
    for x in 0..4 {
        assert_eq!(rgba_at(&pixels, 4, x, 0), [0, 0, 0, 0], "padding row 0");
        assert_eq!(rgba_at(&pixels, 4, x, 3), [0, 0, 0, 0], "padding row 3");
        assert_eq!(rgba_at(&pixels, 4, x, 1), colour, "fitted row 1");
        assert_eq!(rgba_at(&pixels, 4, x, 2), colour, "fitted row 2");
    }
}

// ---- refusals -----------------------------------------------------------

#[test]
fn a_malformed_svg_document_is_a_typed_refusal() {
    let mut sandbox = sandbox();
    // A non-square `viewBox`: a well-formed `<svg>` root that violates the
    // supported subset (design grids must be square), so it is a decode
    // failure rather than "this is not SVG at all".
    assert_eq!(
        rasterise_icon(&mut sandbox, 4, b"<svg viewBox=\"0 0 10 20\"></svg>"),
        Err(IconRasterFailure::Refused(IconRefusal::MalformedImage))
    );
}

#[test]
fn bytes_that_are_neither_png_nor_svg_are_unsupported() {
    let mut sandbox = sandbox();
    assert_eq!(
        rasterise_icon(&mut sandbox, 4, b"plainly not an icon"),
        Err(IconRasterFailure::Refused(IconRefusal::UnsupportedFormat))
    );
    // Non-UTF-8 noise is unsupported the same way (it fails `SvgError::NotUtf8`
    // before any content is even inspected).
    assert_eq!(
        rasterise_icon(&mut sandbox, 4, &[0xFF, 0xFE, 0x00, 0x01]),
        Err(IconRasterFailure::Refused(IconRefusal::UnsupportedFormat))
    );
}

#[test]
fn a_corrupted_png_is_a_typed_refusal() {
    let mut sandbox = sandbox();
    let mut png = png_with(2, 2, |_, _| [1, 2, 3, 255]);
    let last = png.len() - 1;
    png[last] ^= 0xFF; // corrupt the trailing IEND CRC
    assert_eq!(
        rasterise_icon(&mut sandbox, 4, &png),
        Err(IconRasterFailure::Refused(IconRefusal::MalformedImage))
    );
}

#[test]
fn a_zero_or_oversize_side_is_refused_before_any_request() {
    let mut sandbox = sandbox();
    assert_eq!(
        rasterise_icon(&mut sandbox, 0, &svg_square("#000000")),
        Err(IconRasterFailure::Refused(IconRefusal::MalformedRequest))
    );
    assert_eq!(
        rasterise_icon(&mut sandbox, MAX_ICON_SIDE + 1, &svg_square("#000000")),
        Err(IconRasterFailure::Refused(IconRefusal::MalformedRequest))
    );
}

#[test]
fn an_oversize_icon_is_refused_locally_before_any_request() {
    let mut sandbox = sandbox();
    let oversize = vec![0u8; MAX_ARTWORK_BYTES + 1];
    assert_eq!(
        rasterise_icon(&mut sandbox, 4, &oversize),
        Err(IconRasterFailure::Refused(IconRefusal::MalformedRequest))
    );
}

#[test]
fn the_worker_itself_refuses_every_malformed_request_shape() {
    let mut service = ImageRenderService::default();
    // Unknown opcode.
    assert_eq!(service.handle(&[0xFF]), vec![super::REPLY_ERROR, 1]);
    // Truncated request: opcode only, no side, no bytes.
    assert_eq!(
        service.handle(&[super::OP_RASTERISE]),
        vec![super::REPLY_ERROR, 1]
    );
    // Zero side.
    let mut w = Writer::new();
    w.u8(super::OP_RASTERISE);
    w.u32(0);
    w.bytes(b"x");
    assert_eq!(service.handle(&w.finish()), vec![super::REPLY_ERROR, 1]);
    // Over-large side.
    let mut w = Writer::new();
    w.u8(super::OP_RASTERISE);
    w.u32(MAX_ICON_SIDE + 1);
    w.bytes(b"x");
    assert_eq!(service.handle(&w.finish()), vec![super::REPLY_ERROR, 1]);
    // An oversize icon body, refused before any decode is attempted.
    let mut w = Writer::new();
    w.u8(super::OP_RASTERISE);
    w.u32(4);
    w.bytes(&vec![0u8; MAX_ARTWORK_BYTES + 1]);
    assert_eq!(service.handle(&w.finish()), vec![super::REPLY_ERROR, 1]);
    // Trailing bytes after an otherwise well-formed request.
    let mut w = Writer::new();
    w.u8(super::OP_RASTERISE);
    w.u32(4);
    w.bytes(b"x");
    w.u8(0xEE);
    assert_eq!(service.handle(&w.finish()), vec![super::REPLY_ERROR, 1]);
}

// ---- hostile replies ------------------------------------------------------

/// A hostile worker: replies with exactly the given bytes, exactly as a
/// compromised parser process could.
struct EvilWorker(Vec<u8>);

impl Service for EvilWorker {
    fn handle(&mut self, _request: &[u8]) -> Vec<u8> {
        self.0.clone()
    }
}

fn evil_sandbox(
    reply: Vec<u8>,
) -> ParserSandbox<LoopbackLauncher<impl FnMut() -> EvilWorker>, NullSink> {
    ParserSandbox::new(
        LoopbackLauncher::new(move || EvilWorker(reply.clone())),
        NullSink,
    )
}

#[test]
fn a_reply_with_an_unknown_tag_is_refused() {
    let mut sandbox = evil_sandbox(vec![0xEE]);
    assert_eq!(
        rasterise_icon(&mut sandbox, 2, &svg_square("#000000")),
        Err(IconRasterFailure::ReplyMalformed)
    );
}

#[test]
fn a_reply_with_the_wrong_echoed_side_is_refused() {
    let mut w = Writer::new();
    w.u8(super::REPLY_PIXELS);
    w.u32(3); // the request below asks for side 2
    w.bytes(&[0u8; 2 * 2 * 4]);
    let mut sandbox = evil_sandbox(w.finish());
    assert_eq!(
        rasterise_icon(&mut sandbox, 2, &svg_square("#000000")),
        Err(IconRasterFailure::ReplyMalformed)
    );
}

#[test]
fn a_reply_with_the_wrong_pixel_length_is_refused() {
    let mut w = Writer::new();
    w.u8(super::REPLY_PIXELS);
    w.u32(2);
    w.bytes(&[0u8; 3]); // not 2*2*4
    let mut sandbox = evil_sandbox(w.finish());
    assert_eq!(
        rasterise_icon(&mut sandbox, 2, &svg_square("#000000")),
        Err(IconRasterFailure::ReplyMalformed)
    );
}

#[test]
fn trailing_bytes_after_an_otherwise_well_formed_reply_are_refused() {
    let mut w = Writer::new();
    w.u8(super::REPLY_PIXELS);
    w.u32(2);
    w.bytes(&[0u8; 2 * 2 * 4]);
    let mut reply = w.finish();
    reply.push(0xAB);
    let mut sandbox = evil_sandbox(reply);
    assert_eq!(
        rasterise_icon(&mut sandbox, 2, &svg_square("#000000")),
        Err(IconRasterFailure::ReplyMalformed)
    );
}

#[test]
fn an_unknown_refusal_code_in_an_error_reply_is_refused() {
    let mut w = Writer::new();
    w.u8(super::REPLY_ERROR);
    w.u8(0xFF);
    let mut sandbox = evil_sandbox(w.finish());
    assert_eq!(
        rasterise_icon(&mut sandbox, 2, &svg_square("#000000")),
        Err(IconRasterFailure::ReplyMalformed)
    );
}

// ---- display text ---------------------------------------------------------

#[test]
fn every_refusal_has_non_empty_terse_display_text() {
    for refusal in [
        IconRefusal::MalformedRequest,
        IconRefusal::UnsupportedFormat,
        IconRefusal::MalformedImage,
        IconRefusal::Unrenderable,
    ] {
        assert!(!format!("{refusal}").is_empty());
    }
}

// ---- wallpaper: happy paths -------------------------------------------

/// A uniform, fully opaque colour used as the wallpaper source wherever the
/// test only cares about *where* the source lands, not about resampled
/// blending (the resampler's own arithmetic is covered by `lib/raster`).
const WALLPAPER_COLOUR: [u8; 4] = [10, 20, 30, 255];

fn solid_png(width: u32, height: u32, colour: [u8; 4]) -> Vec<u8> {
    png_with(width, height, |_, _| colour)
}

#[test]
fn wallpaper_round_trips_for_every_fit_with_correct_placement() {
    let source = solid_png(2, 2, WALLPAPER_COLOUR);
    let transparent = [0, 0, 0, 0];

    // Fill and Stretch always cover the whole 4x2 destination: a uniform
    // source therefore fills every pixel, corners and centre alike.
    for fit in [WallpaperFit::Fill, WallpaperFit::Stretch] {
        let mut sandbox = sandbox();
        let pixels = render_wallpaper(&mut sandbox, 4, 2, fit, &source).expect("renders");
        assert_eq!(pixels.len(), 4 * 2 * 4);
        for y in 0..2 {
            for x in 0..4 {
                assert_eq!(
                    rgba_at(&pixels, 4, x, y),
                    WALLPAPER_COLOUR,
                    "{fit:?} pixel ({x}, {y})"
                );
            }
        }
    }

    // Fit and Centre place a 2x2 source onto a 4x2 destination as a
    // centred 2-pixel-wide column: the outer corner columns lie outside
    // the placed rectangle and stay fully transparent, the inner columns
    // are the source colour.
    for fit in [WallpaperFit::Fit, WallpaperFit::Centre] {
        let mut sandbox = sandbox();
        let pixels = render_wallpaper(&mut sandbox, 4, 2, fit, &source).expect("renders");
        for y in 0..2 {
            assert_eq!(
                rgba_at(&pixels, 4, 0, y),
                transparent,
                "{fit:?} left corner column, y={y}"
            );
            assert_eq!(
                rgba_at(&pixels, 4, 3, y),
                transparent,
                "{fit:?} right corner column, y={y}"
            );
            assert_eq!(
                rgba_at(&pixels, 4, 1, y),
                WALLPAPER_COLOUR,
                "{fit:?} centre-left column, y={y}"
            );
            assert_eq!(
                rgba_at(&pixels, 4, 2, y),
                WALLPAPER_COLOUR,
                "{fit:?} centre-right column, y={y}"
            );
        }
    }
}

#[test]
fn wallpaper_tile_repeats_the_source_at_native_scale() {
    let mut sandbox = sandbox();
    let colour_a = [10, 20, 30, 255];
    let colour_b = [200, 210, 220, 255];
    // A 2x2 checkerboard, tiled twice in each direction across a 4x4
    // destination: every destination pixel is the source pixel its
    // position maps to modulo the source's own size.
    let source = png_with(
        2,
        2,
        |x, y| if (x + y) % 2 == 0 { colour_a } else { colour_b },
    );
    let pixels =
        render_wallpaper(&mut sandbox, 4, 4, WallpaperFit::Tile, &source).expect("renders");
    for y in 0..4 {
        for x in 0..4 {
            let expected = if (x + y) % 2 == 0 { colour_a } else { colour_b };
            assert_eq!(rgba_at(&pixels, 4, x, y), expected, "pixel ({x}, {y})");
        }
    }
}

#[test]
fn a_screen_larger_than_the_destination_shrinks_a_centred_source_proportionally() {
    let mut sandbox = sandbox();
    let source = solid_png(2, 2, WALLPAPER_COLOUR);
    // A destination a quarter the screen's own pixel count: the true-scale
    // preview must show the 2x2 source shrunk to a single centred pixel,
    // never the source at its own native size filling the whole
    // destination — the shape a `screen == destination` render (the
    // desktop's own path, and the naive preview this fixes) could never
    // produce for a screen this much larger than what is drawn.
    let pixels =
        render_wallpaper_for_screen(&mut sandbox, (4, 4), 2, 2, WallpaperFit::Centre, &source)
            .expect("renders");
    assert_eq!(pixels.len(), 2 * 2 * 4);
    assert_eq!(
        rgba_at(&pixels, 2, 0, 0),
        WALLPAPER_COLOUR,
        "the one centred pixel"
    );
    assert_eq!(rgba_at(&pixels, 2, 1, 0), [0, 0, 0, 0], "top right");
    assert_eq!(rgba_at(&pixels, 2, 0, 1), [0, 0, 0, 0], "bottom left");
    assert_eq!(rgba_at(&pixels, 2, 1, 1), [0, 0, 0, 0], "bottom right");
}

#[test]
fn a_screen_larger_than_the_destination_shrinks_a_tiled_source_before_repeating() {
    let mut sandbox = sandbox();
    let colour_a = [0, 0, 0, 255];
    let colour_b = [255, 255, 255, 255];
    // The same 2x2 checkerboard the icon path's own downscale test proves
    // averages to exact mid-grey.
    let source = png_with(
        2,
        2,
        |x, y| if (x + y) % 2 == 0 { colour_a } else { colour_b },
    );
    // A destination a quarter the screen's own pixel count: the checkerboard
    // must first shrink to that one averaged mid-grey pixel before it is
    // tiled, so every destination pixel is uniform mid-grey — never the
    // checkerboard repeated at its native size, which is what a
    // `screen == destination` render draws instead (see
    // `wallpaper_tile_repeats_the_source_at_native_scale` above).
    let pixels =
        render_wallpaper_for_screen(&mut sandbox, (8, 8), 4, 4, WallpaperFit::Tile, &source)
            .expect("renders");
    assert_eq!(pixels.len(), 4 * 4 * 4);
    let (chunks, _tail) = pixels.as_chunks::<4>();
    for chunk in chunks {
        assert_eq!(*chunk, [128, 128, 128, 255]);
    }
}

#[test]
fn a_screen_equal_to_the_destination_matches_render_wallpaper_exactly() {
    // The desktop's own path is `render_wallpaper`, a thin wrapper over
    // `render_wallpaper_for_screen` with `screen == (width, height)`; this
    // proves the two are still byte-for-byte identical for every fit,
    // rather than asserting it in prose, so the desktop's own wallpaper
    // rendering is provably untouched by the screen-aware preview path.
    let source = png_with(3, 3, |x, y| {
        if (x + y) % 2 == 0 {
            [10, 20, 30, 255]
        } else {
            [200, 210, 220, 255]
        }
    });
    for fit in [
        WallpaperFit::Fill,
        WallpaperFit::Fit,
        WallpaperFit::Stretch,
        WallpaperFit::Centre,
        WallpaperFit::Tile,
    ] {
        let mut via_render_wallpaper = sandbox();
        let plain = render_wallpaper(&mut via_render_wallpaper, 6, 4, fit, &source)
            .unwrap_or_else(|failure| panic!("{fit:?}: {failure}"));

        let mut via_screen = sandbox();
        let screen_modelled =
            render_wallpaper_for_screen(&mut via_screen, (6, 4), 6, 4, fit, &source)
                .unwrap_or_else(|failure| panic!("{fit:?}: {failure}"));

        assert_eq!(plain, screen_modelled, "{fit:?}");
    }
}

#[test]
fn wallpaper_row_budget_requires_banding_at_4k_but_not_1080p() {
    // A 1920-wide row fits comfortably under one frame's row budget; a
    // 3840-wide (4K) row does not fit the whole 2160-row height in one
    // band, exactly as `plans/PINBOARD.md` describes.
    assert!(u64::from(super::rows_per_band(1920)) >= 1080);
    assert!(u64::from(super::rows_per_band(super::MAX_WALLPAPER_WIDTH)) < 2160);
}

#[test]
fn a_4k_wallpaper_assembles_identically_across_many_bands() {
    let mut sandbox = sandbox();
    let source = solid_png(2, 2, WALLPAPER_COLOUR);
    let pixels = render_wallpaper(
        &mut sandbox,
        super::MAX_WALLPAPER_WIDTH,
        super::MAX_WALLPAPER_HEIGHT,
        WallpaperFit::Stretch,
        &source,
    )
    .expect("renders");
    assert_eq!(
        pixels.len(),
        (super::MAX_WALLPAPER_WIDTH as usize) * (super::MAX_WALLPAPER_HEIGHT as usize) * 4
    );
    // A uniform-colour `Stretch` fills every pixel identically, so the
    // several-band assembly this destination requires must be
    // byte-for-byte the same colour everywhere a hypothetical single-band
    // render would have produced — including at every band boundary, not
    // only the corners.
    let (chunks, _tail) = pixels.as_chunks::<4>();
    for chunk in chunks {
        assert_eq!(*chunk, WALLPAPER_COLOUR);
    }
}

#[test]
fn a_wallpaper_far_larger_than_the_destination_prepares_at_a_reduced_scale() {
    // The geometry of every wallpaper master TAIRiX ships: 6688x3764, over
    // 25 megapixels, three times the largest destination this service will
    // ever draw. Asking the decoder for the destination extent rather than
    // the natural size is what lets the desktop show its own default
    // wallpapers at all: a full decode of one costs 25 megapixels of held
    // RGBA and breaches `MAX_WALLPAPER_DECODE_PIXELS`.
    let source = flat_grey_jpeg(6688, 3764);
    assert!(source.len() < tairix_wallpaper::MAX_WALLPAPER_BYTES);

    // An eighth of the master (836x471) already covers a 320x180 screen, so
    // that is the scale chosen and a sixty-fourth of the pixels is what the
    // worker holds.
    let decoded = super::decode_wallpaper_source(&source, 320, 180).expect("decodes");
    assert_eq!((decoded.width(), decoded.height()), (836, 471));

    // A 1080p screen is not covered by the quarter scale's 1672x941, so the
    // half scale serves it — a quarter of the natural pixel count, and the
    // real case the desktop meets on a 1080p display.
    let decoded = super::decode_wallpaper_source(&source, 1920, 1080).expect("decodes");
    assert_eq!((decoded.width(), decoded.height()), (3344, 1882));

    // End to end: the destination extent is the requested one, and every
    // pixel of it is the master's own flat mid-grey.
    let mut sandbox = sandbox();
    let pixels =
        render_wallpaper(&mut sandbox, 320, 180, WallpaperFit::Fill, &source).expect("renders");
    assert_eq!(pixels.len(), 320 * 180 * 4);
    let (chunks, _tail) = pixels.as_chunks::<4>();
    for chunk in chunks {
        assert_eq!(*chunk, [128, 128, 128, 255]);
    }
}

// ---- wallpaper: refusals ------------------------------------------------

#[test]
fn a_band_before_any_prepare_is_refused() {
    let mut sandbox = sandbox();
    assert_eq!(
        super::band_wallpaper(&mut sandbox, 0, 1, 2),
        Err(WallpaperRenderFailure::Refused(
            WallpaperRefusal::NoPreparedSource
        ))
    );
}

#[test]
fn a_band_out_of_range_or_with_zero_rows_is_refused() {
    let mut sandbox = sandbox();
    let png = solid_png(2, 2, WALLPAPER_COLOUR);
    super::prepare_wallpaper(&mut sandbox, (2, 2), 2, 2, WallpaperFit::Stretch, &png)
        .expect("prepares");
    assert_eq!(
        super::band_wallpaper(&mut sandbox, 1, 5, 2),
        Err(WallpaperRenderFailure::Refused(
            WallpaperRefusal::BandOutOfRange
        ))
    );
    assert_eq!(
        super::band_wallpaper(&mut sandbox, 0, 0, 2),
        Err(WallpaperRenderFailure::Refused(
            WallpaperRefusal::BandOutOfRange
        ))
    );
}

#[test]
fn release_makes_a_subsequent_band_fail_closed() {
    let mut sandbox = sandbox();
    let png = solid_png(2, 2, WALLPAPER_COLOUR);
    super::prepare_wallpaper(&mut sandbox, (2, 2), 2, 2, WallpaperFit::Stretch, &png)
        .expect("prepares");
    assert_eq!(super::release_wallpaper(&mut sandbox), Ok(()));
    assert_eq!(
        super::band_wallpaper(&mut sandbox, 0, 1, 2),
        Err(WallpaperRenderFailure::Refused(
            WallpaperRefusal::NoPreparedSource
        ))
    );
}

#[test]
fn an_oversize_destination_is_refused_before_any_request() {
    let mut sandbox = sandbox();
    let png = solid_png(2, 2, WALLPAPER_COLOUR);
    assert_eq!(
        render_wallpaper(
            &mut sandbox,
            super::MAX_WALLPAPER_WIDTH + 1,
            100,
            WallpaperFit::Fill,
            &png
        ),
        Err(WallpaperRenderFailure::Refused(
            WallpaperRefusal::MalformedRequest
        ))
    );
    assert_eq!(
        render_wallpaper(
            &mut sandbox,
            100,
            super::MAX_WALLPAPER_HEIGHT + 1,
            WallpaperFit::Fill,
            &png
        ),
        Err(WallpaperRenderFailure::Refused(
            WallpaperRefusal::MalformedRequest
        ))
    );
    assert_eq!(
        render_wallpaper(&mut sandbox, 0, 100, WallpaperFit::Fill, &png),
        Err(WallpaperRenderFailure::Refused(
            WallpaperRefusal::MalformedRequest
        ))
    );
}

#[test]
fn an_oversize_source_is_refused_locally_before_any_request() {
    let mut sandbox = sandbox();
    let oversize = vec![0u8; tairix_wallpaper::MAX_WALLPAPER_BYTES + 1];
    assert_eq!(
        render_wallpaper(&mut sandbox, 4, 4, WallpaperFit::Fill, &oversize),
        Err(WallpaperRenderFailure::Refused(
            WallpaperRefusal::MalformedRequest
        ))
    );
}

#[test]
fn a_malformed_wallpaper_image_is_a_typed_refusal() {
    let mut sandbox = sandbox();
    let mut png = solid_png(2, 2, WALLPAPER_COLOUR);
    let last = png.len() - 1;
    png[last] ^= 0xFF; // corrupt the trailing IEND CRC
    assert_eq!(
        render_wallpaper(&mut sandbox, 4, 4, WallpaperFit::Fill, &png),
        Err(WallpaperRenderFailure::Refused(
            WallpaperRefusal::MalformedImage
        ))
    );
}

#[test]
fn an_unrecognised_wallpaper_format_is_a_typed_refusal() {
    let mut sandbox = sandbox();
    assert_eq!(
        render_wallpaper(
            &mut sandbox,
            4,
            4,
            WallpaperFit::Fill,
            b"plainly not an image"
        ),
        Err(WallpaperRenderFailure::Refused(
            WallpaperRefusal::UnsupportedFormat
        ))
    );
}

#[test]
fn the_icon_op_still_round_trips_after_a_wallpaper_sequence() {
    let mut sandbox = sandbox();
    let png = solid_png(2, 2, WALLPAPER_COLOUR);
    render_wallpaper(&mut sandbox, 4, 4, WallpaperFit::Fill, &png).expect("wallpaper renders");
    let svg = svg_square("#3070f0");
    let pixels = rasterise_icon(&mut sandbox, 4, &svg).expect("icon still rasterises");
    assert_eq!(pixels.len(), 4 * 4 * 4);
}

#[test]
fn every_wallpaper_refusal_has_non_empty_terse_display_text() {
    for refusal in [
        WallpaperRefusal::MalformedRequest,
        WallpaperRefusal::UnsupportedFormat,
        WallpaperRefusal::MalformedImage,
        WallpaperRefusal::NoPreparedSource,
        WallpaperRefusal::BandOutOfRange,
        WallpaperRefusal::Unrenderable,
    ] {
        assert!(!format!("{refusal}").is_empty());
    }
}
