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
use tairix_raster::Region;
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
    // A well-formed `<svg>` root whose contents the decoder refuses — here a
    // colour outside the accepted syntax — so it is a decode failure rather
    // than "this is not SVG at all".
    assert_eq!(
        rasterise_icon(
            &mut sandbox,
            4,
            b"<svg viewBox=\"0 0 10 10\"><rect width=\"10\" height=\"10\" fill=\"chartreuseish\"/></svg>"
        ),
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
fn a_wallpaper_drawn_larger_than_its_source_is_interpolated_not_blocked() {
    // The desktop's own complaint, end to end: a source smaller than the
    // screen it is stretched onto must be reconstructed smoothly, not held
    // one source pixel at a time across the destination pixels it covers.
    // A four-step horizontal ramp stretched over sixteen columns would show
    // four flat blocks of four under a sample-and-hold; interpolated, the
    // ramp rises pixel by pixel.
    let source = png_with(4, 1, |x, _y| {
        let level = u8::try_from(x * 60).unwrap_or(u8::MAX);
        [level, level, level, 255]
    });
    let mut sandbox = sandbox();
    let pixels =
        render_wallpaper(&mut sandbox, 16, 1, WallpaperFit::Stretch, &source).expect("renders");
    let reds: Vec<u8> = (0..16).map(|x| rgba_at(&pixels, 16, x, 0)[0]).collect();
    assert!(
        reds.windows(2).all(|pair| pair[0] <= pair[1]),
        "the ramp rises: {reds:?}"
    );
    let held = reds.windows(2).filter(|pair| pair[0] == pair[1]).count();
    assert!(
        held < 4,
        "source samples are not held across the destination: {reds:?}"
    );
}

#[test]
fn a_thumbnail_of_a_large_master_is_placed_exactly_as_the_screen_would_be() {
    // A gallery tile models a screen exactly as large as itself, so what it
    // shows must be the same composition the desktop shows, only smaller.
    // This is the case the render now serves from a far smaller decode: the
    // request is what the tile can show, not what the screen could.
    let source = png_with(64, 32, |x, y| {
        let level = u8::try_from((x * 4 + y) % 256).unwrap_or(0);
        [level, 255 - level, 128, 255]
    });
    let mut sandbox = sandbox();
    let tile = render_wallpaper(&mut sandbox, 8, 8, WallpaperFit::Fill, &source).expect("renders");
    assert_eq!(tile.len(), 8 * 8 * 4);
    // Fill covers the whole square: no pixel is left transparent.
    for y in 0..8 {
        for x in 0..8 {
            assert_eq!(rgba_at(&tile, 8, x, y)[3], 255, "tile pixel ({x}, {y})");
        }
    }
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
    assert!(u64::from(super::rows_per_band(super::MAX_DESTINATION_WIDTH)) < 2160);
}

#[test]
fn a_4k_wallpaper_assembles_identically_across_many_bands() {
    let mut sandbox = sandbox();
    let source = solid_png(2, 2, WALLPAPER_COLOUR);
    let pixels = render_wallpaper(
        &mut sandbox,
        super::MAX_DESTINATION_WIDTH,
        super::MAX_DESTINATION_HEIGHT,
        WallpaperFit::Stretch,
        &source,
    )
    .expect("renders");
    assert_eq!(
        pixels.len(),
        (super::MAX_DESTINATION_WIDTH as usize) * (super::MAX_DESTINATION_HEIGHT as usize) * 4
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
    // A synthetic master well beyond our new 8.3-megapixel shipped masters
    // (this one is over three times the pixels of a 4K destination)
    // so the reduced-scale path is exercised even though every
    // shipped master now fits within `MAX_DESTINATION_WIDTH`/
    // `MAX_DESTINATION_HEIGHT`. Asking the decoder for the destination extent
    // rather than the natural size is what would let the desktop show a
    // user-picked wallpaper this large at all: a full decode of one costs
    // its large pixel count in held RGBA and breaches `MAX_WALLPAPER_DECODE_PIXELS`.
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
            super::MAX_DESTINATION_WIDTH + 1,
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
            super::MAX_DESTINATION_HEIGHT + 1,
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

// ---- document upload -----------------------------------------------------

/// A tiny PNG grown to exactly `total` bytes by a private ancillary
/// chunk, so the file is large without the picture in it being.
///
/// Padding after `IEND` would not do: the decoder refuses trailing bytes,
/// which is what a well-formed PNG has none of. An ancillary chunk a
/// decoder is required to skip is the format's own way to carry bytes it
/// does not read.
fn png_padded_to(total: usize) -> Vec<u8> {
    const PADDING: [u8; 4] = *b"paDd";
    let base = png_with(2, 2, |_, _| [9, 8, 7, 255]);
    let overhead = base.len() + chunk(PADDING, &[]).len();
    let mut out = PNG_SIGNATURE.to_vec();
    out.extend(chunk(IHDR, &ihdr_payload(2, 2)));
    out.extend(chunk(PADDING, &vec![0u8; total - overhead]));
    let mut raw = Vec::new();
    for _ in 0..2 {
        raw.extend_from_slice(&[0, 9, 8, 7, 255, 9, 8, 7, 255]);
    }
    out.extend(chunk(IDAT, &zlib_wrap(&raw)));
    out.extend(chunk(IEND, &[]));
    out
}

/// A small test coordinate as the pixel byte it stands for.
fn byte(value: u32) -> u8 {
    u8::try_from(value).expect("test coordinates stay inside a byte")
}

/// Frame one request from its opcode and payload bytes.
fn request(op: u8, payload: &[u8]) -> Vec<u8> {
    let mut out = vec![op];
    out.extend_from_slice(payload);
    out
}

/// An `OP_DOC_BEGIN` request declaring `len` bytes.
fn begin(len: u64) -> Vec<u8> {
    request(super::OP_DOC_BEGIN, &len.to_le_bytes())
}

/// An `OP_DOC_PUSH` request carrying `chunk`.
fn push(chunk: &[u8]) -> Vec<u8> {
    let mut w = Writer::new();
    w.u8(super::OP_DOC_PUSH);
    w.bytes(chunk);
    w.finish()
}

/// Load `service` with `bytes` as a complete document.
fn load(service: &mut ImageRenderService, bytes: &[u8]) {
    assert_eq!(
        service.handle(&begin(bytes.len() as u64)),
        vec![super::REPLY_DOC_BEGUN]
    );
    for chunk in bytes.chunks(super::MAX_DOCUMENT_CHUNK) {
        let reply = service.handle(&push(chunk));
        assert_eq!(reply.first().copied(), Some(super::REPLY_DOC_PUSHED));
    }
}

#[test]
fn a_document_pushed_in_pieces_reassembles_byte_for_byte() {
    let mut service = ImageRenderService::default();
    assert_eq!(service.handle(&begin(6)), vec![super::REPLY_DOC_BEGUN]);
    service.handle(&push(b"abc"));
    service.handle(&push(b"def"));
    let held = service.document.as_ref().expect("the document is held");
    assert_eq!(held.bytes, b"abcdef".to_vec());
    assert!(held.is_complete());
}

#[test]
fn a_push_answers_the_running_total_it_now_holds() {
    let mut service = ImageRenderService::default();
    service.handle(&begin(5));
    let mut w = Writer::new();
    w.u8(super::REPLY_DOC_PUSHED);
    w.u64(3);
    assert_eq!(service.handle(&push(b"abc")), w.finish());
}

#[test]
fn a_chunk_past_the_declared_length_is_refused() {
    let mut service = ImageRenderService::default();
    service.handle(&begin(4));
    assert_eq!(
        service.handle(&push(b"abcde")),
        vec![super::REPLY_ERROR, super::REFUSAL_DOC_OVERRUN]
    );
}

#[test]
fn a_chunk_before_any_begin_is_refused() {
    let mut service = ImageRenderService::default();
    assert_eq!(
        service.handle(&push(b"abc")),
        vec![super::REPLY_ERROR, super::REFUSAL_DOC_NOT_BEGUN]
    );
}

#[test]
fn a_document_larger_than_the_containment_bound_is_refused_before_it_is_reserved() {
    let mut service = ImageRenderService::default();
    assert_eq!(
        service.handle(&begin(super::MAX_DOCUMENT_BYTES as u64 + 1)),
        vec![super::REPLY_ERROR, super::REFUSAL_DOC_TOO_LARGE]
    );
    assert!(service.document.is_none());
}

#[test]
fn a_zero_length_document_is_refused() {
    let mut service = ImageRenderService::default();
    assert_eq!(
        service.handle(&begin(0)),
        vec![super::REPLY_ERROR, super::REFUSAL_DOC_MALFORMED_REQUEST]
    );
}

#[test]
fn trailing_bytes_on_a_begin_are_refused() {
    let mut service = ImageRenderService::default();
    let mut payload = 4u64.to_le_bytes().to_vec();
    payload.push(0);
    assert_eq!(
        service.handle(&request(super::OP_DOC_BEGIN, &payload)),
        vec![super::REPLY_ERROR, super::REFUSAL_DOC_MALFORMED_REQUEST]
    );
}

#[test]
fn beginning_a_fresh_document_drops_whatever_was_open_over_the_old_one() {
    let mut service = ImageRenderService::default();
    load(&mut service, &png_with(2, 2, |_, _| [1, 2, 3, 255]));
    assert_eq!(
        service.handle(&request(super::OP_VIEW_OPEN, &[0]))[0],
        super::REPLY_VIEW_OPENED
    );
    assert!(service.view.is_some());
    service.handle(&begin(3));
    assert!(
        service.view.is_none(),
        "a view over the previous document cannot answer about the new one"
    );
}

#[test]
fn a_maximal_chunk_is_exactly_what_one_protocol_frame_carries() {
    // The bound the framing imposes, derived rather than chosen: a source
    // ceiling picked independently of it can sit just above what a frame
    // holds, and every request at that size is then refused by the
    // transport instead of being served.
    assert_eq!(
        super::MAX_DOCUMENT_CHUNK + super::DOC_PUSH_OVERHEAD,
        crate::proto::MAX_FRAME
    );
    let mut w = Writer::new();
    w.u8(super::OP_DOC_PUSH);
    w.bytes(&vec![0u8; super::MAX_DOCUMENT_CHUNK]);
    assert_eq!(w.finish().len(), crate::proto::MAX_FRAME);
}

#[test]
fn a_source_filling_the_whole_wallpaper_bound_is_placed_rather_than_refused_by_the_framing() {
    // A wallpaper of exactly `MAX_WALLPAPER_BYTES` used to be admitted by
    // the caller's own bound and then refused by the transport, because the
    // request carried the whole file and overflowed one frame. It is
    // uploaded in chunks now, so the two bounds no longer collide.
    let png = png_padded_to(tairix_wallpaper::MAX_WALLPAPER_BYTES);
    assert_eq!(png.len(), tairix_wallpaper::MAX_WALLPAPER_BYTES);
    let mut sandbox = sandbox();
    let pixels = render_wallpaper(&mut sandbox, 2, 2, WallpaperFit::Stretch, &png)
        .expect("a source at the bound is placed");
    assert_eq!(pixels.len(), 2 * 2 * 4);
}

#[test]
fn a_wallpaper_prepare_with_nothing_uploaded_is_refused() {
    let mut service = ImageRenderService::default();
    let mut w = Writer::new();
    w.u8(super::OP_WALLPAPER_PREPARE);
    for field in [4u32, 4, 4, 4] {
        w.u32(field);
    }
    w.u8(0);
    assert_eq!(
        service.handle(&w.finish()),
        vec![super::REPLY_ERROR, super::REFUSAL_WALLPAPER_NO_SOURCE]
    );
}

// ---- fixtures for the two container shapes -------------------------------

/// Build an ICO holding one PNG entry per `(side, pixel)`, which is the
/// page-container shape: independent pictures of differing sizes.
fn ico_of(entries: &[(u32, [u8; 4])]) -> Vec<u8> {
    const DIRECTORY_ENTRY: usize = 16;
    let payloads: Vec<Vec<u8>> = entries
        .iter()
        .map(|(side, pixel)| png_with(*side, *side, |_, _| *pixel))
        .collect();
    let mut out = vec![0, 0, 1, 0];
    out.extend_from_slice(
        &u16::try_from(entries.len())
            .expect("test entry count")
            .to_le_bytes(),
    );
    let mut offset = 6 + DIRECTORY_ENTRY * entries.len();
    for ((side, _), payload) in entries.iter().zip(&payloads) {
        let dimension = u8::try_from(*side).expect("test icon side fits a byte");
        out.extend_from_slice(&[dimension, dimension, 0, 0]);
        out.extend_from_slice(&1u16.to_le_bytes());
        out.extend_from_slice(&32u16.to_le_bytes());
        out.extend_from_slice(
            &u32::try_from(payload.len())
                .expect("test payload")
                .to_le_bytes(),
        );
        out.extend_from_slice(&u32::try_from(offset).expect("test offset").to_le_bytes());
        offset += payload.len();
    }
    for payload in &payloads {
        out.extend_from_slice(payload);
    }
    out
}

/// One 1×1 GIF frame's LZW data sub-block: clear, the pixel's palette
/// index, then end-of-information, at the three-bit width a minimum code
/// size of two starts at, packed least-significant bit first.
fn gif_frame_data(index: u8) -> [u8; 4] {
    const CLEAR: u8 = 4;
    const END: u8 = 5;
    let first = CLEAR | (index << 3) | ((END & 0x03) << 6);
    [2, first, END >> 2, 0]
}

/// Build a 1×1 animated GIF: one frame per `(palette index, delay in
/// hundredths)`, over a two-colour global table, looping `loop_count`
/// times (`0` for ever).
fn gif_of(frames: &[(u8, u16)], loop_count: u16) -> Vec<u8> {
    let mut out = b"GIF89a".to_vec();
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    // Global colour table present, two entries.
    out.extend_from_slice(&[0x80, 0, 0]);
    out.extend_from_slice(&[0x10, 0x20, 0x30, 0x40, 0x50, 0x60]);
    out.extend_from_slice(&[0x21, 0xFF, 0x0B]);
    out.extend_from_slice(b"NETSCAPE2.0");
    out.extend_from_slice(&[0x03, 0x01]);
    out.extend_from_slice(&loop_count.to_le_bytes());
    out.push(0x00);
    for (index, delay) in frames {
        out.extend_from_slice(&[0x21, 0xF9, 0x04, 0x00]);
        out.extend_from_slice(&delay.to_le_bytes());
        out.extend_from_slice(&[0x00, 0x00]);
        out.push(0x2C);
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&1u16.to_le_bytes());
        out.extend_from_slice(&1u16.to_le_bytes());
        out.push(0x00);
        out.push(0x02);
        out.extend_from_slice(&gif_frame_data(*index));
    }
    out.push(0x3B);
    out
}

// ---- viewing a document, end to end --------------------------------------

/// The window covering the whole of a `width`×`height` scaling.
fn whole(width: u32, height: u32) -> Region {
    Region {
        x: 0,
        y: 0,
        width,
        height,
    }
}

/// Upload `bytes` and open them, answering what the container declares.
fn open(
    sandbox: &mut TestSandbox,
    bytes: &[u8],
) -> Result<super::ViewDocument, super::ViewFailure> {
    super::send_document(sandbox, bytes).map_err(super::ViewFailure::Document)?;
    super::open_view(sandbox, None)
}

#[test]
fn a_still_picture_opens_as_the_one_page_case() {
    let mut sandbox = sandbox();
    let png = png_with(4, 3, |x, y| [byte(x), byte(y), 0, 255]);
    let document = open(&mut sandbox, &png).expect("the picture opens");
    assert_eq!(document.format, super::ViewFormat::Png);
    assert!(!document.animated);
    assert_eq!(document.loop_count, None);
    assert_eq!((document.count, document.width, document.height), (1, 4, 3));
}

#[test]
fn a_page_at_its_own_scale_comes_back_pixel_for_pixel() {
    let mut sandbox = sandbox();
    let png = png_with(4, 2, |x, y| [byte(x * 10), byte(y * 20), 7, 255]);
    open(&mut sandbox, &png).expect("the picture opens");
    let page = super::select_page(&mut sandbox, 0).expect("page 0 decodes");
    assert_eq!((page.index, page.width, page.height), (0, 4, 2));
    let mut out = vec![0u8; 4 * 2 * 4];
    super::render_page(&mut sandbox, (4, 2), whole(4, 2), &mut out)
        .expect("the page renders at its own size");
    for y in 0..2u32 {
        for x in 0..4u32 {
            let at = ((y * 4 + x) * 4) as usize;
            assert_eq!(
                &out[at..at + 4],
                &[byte(x * 10), byte(y * 20), 7, 255],
                "pixel ({x}, {y}) survives a one-to-one render"
            );
        }
    }
}

#[test]
fn a_window_renders_only_the_part_of_the_page_it_names() {
    let mut sandbox = sandbox();
    // Four quadrants of a 2×2, so a 1×1 window can only be one of them.
    let png = png_with(2, 2, |x, y| [byte(x * 100), byte(y * 100), 0, 255]);
    open(&mut sandbox, &png).expect("the picture opens");
    super::select_page(&mut sandbox, 0).expect("page 0 decodes");
    let mut out = vec![0u8; 4];
    super::render_page(
        &mut sandbox,
        (2, 2),
        Region {
            x: 1,
            y: 1,
            width: 1,
            height: 1,
        },
        &mut out,
    )
    .expect("the window renders");
    assert_eq!(out, vec![100, 100, 0, 255]);
}

#[test]
fn a_window_of_a_magnification_is_drawn_because_a_viewer_zooms_in() {
    let mut sandbox = sandbox();
    let png = png_with(2, 2, |_, _| [3, 4, 5, 255]);
    open(&mut sandbox, &png).expect("the picture opens");
    super::select_page(&mut sandbox, 0).expect("page 0 decodes");
    let mut out = vec![0u8; 8 * 8 * 4];
    super::render_page(
        &mut sandbox,
        (16, 16),
        Region {
            x: 0,
            y: 0,
            width: 8,
            height: 8,
        },
        &mut out,
    )
    .expect("the top-left quarter of an eightfold magnification draws");
    assert!(
        out.as_chunks::<4>()
            .0
            .iter()
            .all(|pixel| *pixel == [3, 4, 5, 255]),
        "magnifying a flat picture fills the window with it"
    );
}

#[test]
fn panning_a_zoom_moves_by_one_screen_pixel_rather_than_by_the_zoom_factor() {
    // The reason a render names a rectangle of the *scaling* rather than
    // of the page: at eight times, an integer page rectangle could only
    // move the picture eight screen pixels at a time.
    let mut sandbox = sandbox();
    let png = png_with(4, 1, |x, _| [byte(x * 60), 0, 0, 255]);
    open(&mut sandbox, &png).expect("the picture opens");
    super::select_page(&mut sandbox, 0).expect("page 0 decodes");
    let mut read = |x: u32| {
        let mut out = vec![0u8; 8 * 4];
        super::render_page(
            &mut sandbox,
            (32, 8),
            Region {
                x,
                y: 4,
                width: 8,
                height: 1,
            },
            &mut out,
        )
        .expect("the window renders");
        out
    };
    let left = read(8);
    let right = read(9);
    assert_ne!(left, right, "one screen pixel of pan changes the picture");
    assert_eq!(
        left[4..],
        right[..left.len() - 4],
        "and changes it by exactly one screen pixel"
    );
}

#[test]
fn a_page_container_addresses_its_pages_independently() {
    let mut sandbox = sandbox();
    let ico = ico_of(&[(2, [11, 0, 0, 255]), (4, [0, 22, 0, 255])]);
    let document = open(&mut sandbox, &ico).expect("the icon file opens");
    assert!(!document.animated);
    assert_eq!(document.count, 2);
    // The container's own geometry is its largest page.
    assert_eq!((document.width, document.height), (4, 4));
    let small = super::select_page(&mut sandbox, 0).expect("page 0 decodes");
    assert_eq!((small.width, small.height), (2, 2));
    let large = super::select_page(&mut sandbox, 1).expect("page 1 decodes");
    assert_eq!((large.width, large.height), (4, 4));
    // Backwards, because pages are independent and order is the caller's.
    let again = super::select_page(&mut sandbox, 0).expect("page 0 decodes again");
    assert_eq!((again.index, again.width), (0, 2));
}

#[test]
fn an_animation_reports_its_loop_count_and_each_frames_own_delay() {
    let mut sandbox = sandbox();
    let gif = gif_of(&[(0, 10), (1, 25), (0, 5)], 3);
    let document = open(&mut sandbox, &gif).expect("the animation opens");
    assert_eq!(document.format, super::ViewFormat::Gif);
    assert!(document.animated);
    assert_eq!(document.loop_count, Some(3));
    assert_eq!((document.count, document.width, document.height), (3, 1, 1));
    for (index, hundredths) in [(0u32, 10u64), (1, 25), (2, 5)] {
        let frame = super::select_page(&mut sandbox, index).expect("the frame composites");
        assert_eq!(frame.index, index);
        assert_eq!(frame.delay_ns, hundredths * 10_000_000);
    }
}

#[test]
fn an_animation_looping_for_ever_says_so_rather_than_naming_a_count() {
    let mut sandbox = sandbox();
    let document = open(&mut sandbox, &gif_of(&[(0, 4)], 0)).expect("the animation opens");
    assert!(document.animated);
    assert_eq!(document.loop_count, None);
}

#[test]
fn an_animations_frames_composite_and_can_be_replayed_from_the_start() {
    let mut sandbox = sandbox();
    // Two frames of opposite palette entries, so the canvas differs.
    let gif = gif_of(&[(0, 4), (1, 4)], 0);
    open(&mut sandbox, &gif).expect("the animation opens");
    let mut first = vec![0u8; 4];
    let mut second = vec![0u8; 4];
    super::select_page(&mut sandbox, 0).expect("frame 0 composites");
    super::render_page(&mut sandbox, (1, 1), whole(1, 1), &mut first).expect("frame 0 renders");
    super::select_page(&mut sandbox, 1).expect("frame 1 composites");
    super::render_page(&mut sandbox, (1, 1), whole(1, 1), &mut second).expect("frame 1 renders");
    assert_eq!(first, vec![0x10, 0x20, 0x30, 255]);
    assert_eq!(second, vec![0x40, 0x50, 0x60, 255]);
    // Going back restarts the composition rather than answering the canvas
    // as it stands.
    let mut replayed = vec![0u8; 4];
    super::select_page(&mut sandbox, 0).expect("frame 0 composites again");
    super::render_page(&mut sandbox, (1, 1), whole(1, 1), &mut replayed)
        .expect("frame 0 renders again");
    assert_eq!(replayed, first);
}

#[test]
fn a_format_with_no_signature_is_reached_by_being_named() {
    let mut sandbox = sandbox();
    // A one-sprite area: count, first, end, then the control block. Not
    // sniffable by construction, which is the point.
    let png = png_with(2, 2, |_, _| [1, 1, 1, 255]);
    super::send_document(&mut sandbox, &png).expect("the document uploads");
    assert_eq!(
        super::open_view(&mut sandbox, Some(super::ViewFormat::Sprite)),
        Err(super::ViewFailure::Refused(
            super::ViewRefusal::MalformedDocument
        )),
        "naming the wrong format is refused by that format's own parser, \
         never read as the one the bytes really are"
    );
}

#[test]
fn bytes_of_no_recognised_format_are_refused_as_such() {
    let mut sandbox = sandbox();
    assert_eq!(
        open(&mut sandbox, b"not a picture at all"),
        Err(super::ViewFailure::Refused(
            super::ViewRefusal::UnsupportedFormat
        ))
    );
}

#[test]
fn releasing_a_view_drops_the_document_with_it() {
    let mut sandbox = sandbox();
    let png = png_with(2, 2, |_, _| [1, 2, 3, 255]);
    open(&mut sandbox, &png).expect("the picture opens");
    super::close_view(&mut sandbox).expect("the view releases");
    assert_eq!(
        super::select_page(&mut sandbox, 0),
        Err(super::ViewFailure::Refused(super::ViewRefusal::NotOpen))
    );
    assert_eq!(
        super::open_view(&mut sandbox, None),
        Err(super::ViewFailure::Refused(super::ViewRefusal::NoDocument)),
        "the released document is gone, not left to be reopened"
    );
}

// ---- viewing a vector document -------------------------------------------

/// A drawing whose left half is opaque red, in a `view_w`×`view_h`
/// coordinate box.
///
/// The one edge is at exactly half the width, so at any even extent it
/// falls on a pixel boundary: a rasterisation at that extent is fully
/// covered on one side and untouched on the other, where a resampling of
/// some other extent would leave a soft column.
fn svg_half(view_w: u32, view_h: u32) -> Vec<u8> {
    let half = f64::from(view_w) / 2.0;
    format!(
        r##"<svg viewBox="0 0 {view_w} {view_h}"><polygon points="0,0 {half},0 {half},{view_h} 0,{view_h}" fill="#ff0000"/></svg>"##
    )
    .into_bytes()
}

/// Open `svg`, select its one page, and answer the sandbox.
fn opened_vector(svg: &[u8]) -> TestSandbox {
    let mut sandbox = sandbox();
    open(&mut sandbox, svg).expect("the drawing opens");
    super::select_page(&mut sandbox, 0).expect("the one page selects");
    sandbox
}

/// Render `window` of `svg` scaled to `extent`.
fn vector_pixels(sandbox: &mut TestSandbox, extent: (u32, u32), window: Region) -> Vec<u8> {
    let mut out = vec![0u8; (window.width as usize) * (window.height as usize) * 4];
    super::render_page(sandbox, extent, window, &mut out).expect("the window renders");
    out
}

#[test]
fn a_drawing_opens_as_the_one_page_case_at_the_size_it_declares() {
    let mut sandbox = sandbox();
    let document = open(&mut sandbox, &svg_half(40, 25)).expect("the drawing opens");
    assert_eq!(document.format, super::ViewFormat::Svg);
    assert!(!document.animated);
    assert_eq!(document.loop_count, None);
    // One user unit is one pixel, so the coordinate box is the size the
    // picture is shown at unzoomed.
    assert_eq!(
        (document.count, document.width, document.height),
        (1, 40, 25)
    );
    let page = super::select_page(&mut sandbox, 0).expect("the one page selects");
    assert_eq!(
        (page.index, page.width, page.height, page.delay_ns),
        (0, 40, 25, 0)
    );
}

#[test]
fn a_drawing_holds_exactly_one_page() {
    let mut sandbox = sandbox();
    open(&mut sandbox, &svg_half(4, 4)).expect("the drawing opens");
    assert_eq!(
        super::select_page(&mut sandbox, 1),
        Err(super::ViewFailure::Refused(super::ViewRefusal::NoSuchPage))
    );
}

#[test]
fn a_render_before_the_drawings_page_is_selected_is_refused() {
    // The vector backing keeps the same state machine a raster container
    // has, so an app drives one flow rather than two.
    let mut sandbox = sandbox();
    open(&mut sandbox, &svg_half(4, 4)).expect("the drawing opens");
    let mut out = vec![0u8; 4];
    assert_eq!(
        super::render_page(&mut sandbox, (1, 1), whole(1, 1), &mut out),
        Err(super::ViewFailure::Refused(
            super::ViewRefusal::NoPageDecoded
        ))
    );
}

#[test]
fn a_drawing_is_rasterised_afresh_at_whatever_extent_a_render_asks_for() {
    // The whole point of a vector backing: every zoom level is drawn at
    // full precision rather than resampled from one decode. A resample
    // would leave the edge soft at some extents; a rasterisation puts it
    // exactly on the boundary at all of them.
    let mut sandbox = opened_vector(&svg_half(2, 1));
    for scale in [4u32, 8, 32, 100] {
        let (width, height) = (scale * 2, scale);
        let pixels = vector_pixels(&mut sandbox, (width, height), whole(width, height));
        for y in 0..height {
            for x in 0..width {
                let at = ((y * width + x) * 4) as usize;
                let expected = if x < scale {
                    [255, 0, 0, 255]
                } else {
                    [0, 0, 0, 0]
                };
                assert_eq!(
                    pixels[at..at + 4],
                    expected,
                    "at {width}x{height}, pixel ({x}, {y}) is exact"
                );
            }
        }
    }
}

#[test]
fn a_window_of_a_drawings_zoom_is_that_rectangle_of_the_whole() {
    let mut sandbox = opened_vector(&svg_half(3, 2));
    let extent = (33u32, 22u32);
    let full = vector_pixels(&mut sandbox, extent, whole(extent.0, extent.1));
    for window in [
        Region {
            x: 0,
            y: 0,
            width: 7,
            height: 5,
        },
        Region {
            x: 14,
            y: 9,
            width: 11,
            height: 6,
        },
        Region {
            x: extent.0 - 1,
            y: extent.1 - 1,
            width: 1,
            height: 1,
        },
    ] {
        let cut = vector_pixels(&mut sandbox, extent, window);
        for y in 0..window.height {
            let cut_row = ((y * window.width) * 4) as usize;
            let full_row = (((window.y + y) * extent.0 + window.x) * 4) as usize;
            let bytes = (window.width * 4) as usize;
            assert_eq!(
                cut[cut_row..cut_row + bytes],
                full[full_row..full_row + bytes],
                "{window:?} row {y}"
            );
        }
    }
}

#[test]
fn windows_stacked_up_a_drawing_reassemble_into_the_one_they_partition() {
    // The band offset arithmetic: a viewer collects a tall window in
    // pieces, and a piece that read the wrong rows would tear the picture
    // at every boundary.
    let mut sandbox = opened_vector(&svg_half(2, 3));
    let extent = (16u32, 12u32);
    let whole_window = Region {
        x: 2,
        y: 1,
        width: 9,
        height: 8,
    };
    let full = vector_pixels(&mut sandbox, extent, whole_window);
    let mut assembled = Vec::new();
    for (first, rows) in [(0u32, 3u32), (3, 1), (4, 4)] {
        assembled.extend_from_slice(&vector_pixels(
            &mut sandbox,
            extent,
            Region {
                x: whole_window.x,
                y: whole_window.y + first,
                width: whole_window.width,
                height: rows,
            },
        ));
    }
    assert_eq!(assembled, full);
}

#[test]
fn a_magnification_no_buffer_could_hold_still_renders_its_window() {
    // Twenty billion pixels: the window can only be answered by never
    // sizing anything from the magnification.
    let mut sandbox = opened_vector(&svg_half(2, 1));
    let extent = (200_000u32, 100_000u32);
    let inside = vector_pixels(
        &mut sandbox,
        extent,
        Region {
            x: 1_000,
            y: 50_000,
            width: 16,
            height: 8,
        },
    );
    assert!(
        inside
            .as_chunks::<4>()
            .0
            .iter()
            .all(|p| *p == [255, 0, 0, 255]),
        "a window inside the drawn half is drawn"
    );
    let outside = vector_pixels(
        &mut sandbox,
        extent,
        Region {
            x: 150_000,
            y: 50_000,
            width: 16,
            height: 8,
        },
    );
    assert!(
        outside
            .as_chunks::<4>()
            .0
            .iter()
            .all(|p| *p == [0, 0, 0, 0]),
        "and one outside it is not"
    );
}

#[test]
fn a_drawing_is_reached_by_being_named_as_well_as_by_having_no_signature() {
    let mut sandbox = sandbox();
    super::send_document(&mut sandbox, &svg_half(6, 4)).expect("the document uploads");
    let document = super::open_view(&mut sandbox, Some(super::ViewFormat::Svg))
        .expect("naming the vector format opens the drawing");
    assert_eq!(
        (document.format, document.width, document.height),
        (super::ViewFormat::Svg, 6, 4)
    );
}

#[test]
fn naming_the_vector_format_for_a_raster_file_is_unsupported_not_malformed() {
    let mut sandbox = sandbox();
    super::send_document(&mut sandbox, &png_with(2, 2, |_, _| [1, 2, 3, 255]))
        .expect("the document uploads");
    assert_eq!(
        super::open_view(&mut sandbox, Some(super::ViewFormat::Svg)),
        Err(super::ViewFailure::Refused(
            super::ViewRefusal::UnsupportedFormat
        )),
        "bytes that are not a drawing at all are not a drawing this \
         decoder refused"
    );
}

#[test]
fn a_drawing_outside_the_supported_subset_is_a_malformed_document() {
    let mut sandbox = sandbox();
    // Shaped like SVG — it has an `<svg>` root — but with no coordinate
    // system to draw in, which is a decode failure rather than a file of
    // some other kind.
    assert_eq!(
        open(&mut sandbox, br"<svg><circle cx='1' cy='1' r='1'/></svg>"),
        Err(super::ViewFailure::Refused(
            super::ViewRefusal::MalformedDocument
        ))
    );
}

#[test]
fn a_picture_larger_than_a_view_opens_says_so_rather_than_calling_it_broken() {
    // A user can act on "too large" and it says nothing is wrong with
    // their file; "failed to decode" would tell them their photograph is
    // broken when it is only big.
    let mut vector = sandbox();
    let at_bound = open(&mut vector, &svg_half(super::MAX_DRAWING_EXTENT, 4))
        .expect("a drawing exactly at the bound opens");
    assert_eq!(at_bound.width, super::MAX_DRAWING_EXTENT);

    let mut vector = sandbox();
    let huge = svg_half(super::MAX_DRAWING_EXTENT + 8, 4);
    assert_eq!(
        open(&mut vector, &huge),
        Err(super::ViewFailure::Refused(super::ViewRefusal::TooLarge)),
        "a drawing declaring a box no render could ask for"
    );

    let mut raster = sandbox();
    // A well-formed header declaring far more pixels than a view decodes,
    // weighed before a scanline is allocated.
    let over = build_png(20_000, 20_000, &[]);
    let refusal = match open(&mut raster, &over) {
        Ok(_) => super::select_page(&mut raster, 0).map(|_| ()).unwrap_err(),
        Err(err) => err,
    };
    assert_eq!(
        refusal,
        super::ViewFailure::Refused(super::ViewRefusal::TooLarge),
        "a raster page over the decode bound"
    );
}

// ---- view refusals, at the worker ----------------------------------------

/// A service with `png` open and page 0 decoded, which is the state every
/// render and band request is judged against.
fn opened(png: &[u8]) -> ImageRenderService {
    let mut service = ImageRenderService::default();
    load(&mut service, png);
    assert_eq!(
        service.handle(&request(super::OP_VIEW_OPEN, &[0]))[0],
        super::REPLY_VIEW_OPENED
    );
    service
}

/// An `OP_VIEW_RENDER` request for `window` of a picture scaled to
/// `extent`.
fn render_request(extent: (u32, u32), window: (u32, u32, u32, u32)) -> Vec<u8> {
    let mut w = Writer::new();
    w.u8(super::OP_VIEW_RENDER);
    for field in [extent.0, extent.1, window.0, window.1, window.2, window.3] {
        w.u32(field);
    }
    w.finish()
}

/// An `OP_VIEW_BAND` request over `first_row..first_row + rows`.
fn band_request(first_row: u32, rows: u32) -> Vec<u8> {
    let mut w = Writer::new();
    w.u8(super::OP_VIEW_BAND);
    w.u32(first_row);
    w.u32(rows);
    w.finish()
}

/// An `OP_VIEW_PAGE` request for `index`.
fn page_request(index: u32) -> Vec<u8> {
    let mut w = Writer::new();
    w.u8(super::OP_VIEW_PAGE);
    w.u32(index);
    w.finish()
}

fn refused(reply: &[u8]) -> Option<super::ViewRefusal> {
    match reply {
        [super::REPLY_ERROR, code] => super::ViewRefusal::from_wire(*code),
        _ => None,
    }
}

#[test]
fn opening_an_incomplete_document_is_refused_rather_than_decoded_short() {
    let mut service = ImageRenderService::default();
    let png = png_with(2, 2, |_, _| [1, 2, 3, 255]);
    service.handle(&begin(png.len() as u64));
    service.handle(&push(&png[..png.len() - 1]));
    assert_eq!(
        refused(&service.handle(&request(super::OP_VIEW_OPEN, &[0]))),
        Some(super::ViewRefusal::NoDocument)
    );
}

#[test]
fn naming_a_format_byte_no_format_uses_is_refused() {
    let mut service = ImageRenderService::default();
    load(&mut service, &png_with(2, 2, |_, _| [1, 2, 3, 255]));
    assert_eq!(
        refused(&service.handle(&request(super::OP_VIEW_OPEN, &[0xEE]))),
        Some(super::ViewRefusal::MalformedRequest)
    );
}

#[test]
fn every_view_request_before_an_open_is_refused_as_not_open() {
    let mut service = ImageRenderService::default();
    for probe in [
        page_request(0),
        render_request((1, 1), (0, 0, 1, 1)),
        band_request(0, 1),
    ] {
        assert_eq!(
            refused(&service.handle(&probe)),
            Some(super::ViewRefusal::NotOpen)
        );
    }
}

#[test]
fn a_page_past_the_last_entry_is_refused() {
    let mut service = opened(&png_with(2, 2, |_, _| [1, 2, 3, 255]));
    assert_eq!(
        refused(&service.handle(&page_request(1))),
        Some(super::ViewRefusal::NoSuchPage)
    );
}

#[test]
fn a_render_before_any_page_is_decoded_is_refused() {
    let mut service = opened(&png_with(2, 2, |_, _| [1, 2, 3, 255]));
    assert_eq!(
        refused(&service.handle(&render_request((1, 1), (0, 0, 1, 1)))),
        Some(super::ViewRefusal::NoPageDecoded)
    );
}

#[test]
fn a_window_outside_the_extent_it_names_is_refused() {
    let mut service = opened(&png_with(2, 2, |_, _| [1, 2, 3, 255]));
    service.handle(&page_request(0));
    for window in [
        (0, 0, 3, 1),
        (0, 0, 1, 3),
        (2, 0, 1, 1),
        (0, 2, 1, 1),
        (0, 0, 0, 1),
        (0, 0, 1, 0),
        (u32::MAX, 0, 1, 1),
        (0, u32::MAX, 1, 1),
    ] {
        assert_eq!(
            refused(&service.handle(&render_request((2, 2), window))),
            Some(super::ViewRefusal::MalformedRequest),
            "window {window:?} does not lie inside a 2x2 scaling"
        );
    }
}

#[test]
fn a_window_outside_the_service_bounds_is_refused() {
    let mut service = opened(&png_with(2, 2, |_, _| [1, 2, 3, 255]));
    service.handle(&page_request(0));
    let over_wide = super::MAX_DESTINATION_WIDTH + 1;
    let over_tall = super::MAX_DESTINATION_HEIGHT + 1;
    for (extent, window) in [
        ((over_wide, 1), (0, 0, over_wide, 1)),
        ((1, over_tall), (0, 0, 1, over_tall)),
    ] {
        assert_eq!(
            refused(&service.handle(&render_request(extent, window))),
            Some(super::ViewRefusal::MalformedRequest),
            "window {window:?} is larger than this service draws"
        );
    }
}

#[test]
fn an_extent_past_what_the_rasteriser_places_is_refused() {
    // Past this a vector document's contours would be clamped, so the
    // answer would be a distorted picture rather than a refused request —
    // and one render shape takes one bound whichever backing answers it.
    let mut service = opened(&png_with(2, 2, |_, _| [1, 2, 3, 255]));
    service.handle(&page_request(0));
    let limit = tairix_raster::MAX_DRAWING_EXTENT;
    assert_eq!(
        service.handle(&render_request((limit, limit), (0, 0, 8, 8)))[0],
        super::REPLY_VIEW_RENDERED,
        "the bound itself renders"
    );
    for extent in [(limit + 1, limit), (limit, limit + 1)] {
        assert_eq!(
            refused(&service.handle(&render_request(extent, (0, 0, 8, 8)))),
            Some(super::ViewRefusal::MalformedRequest),
            "extent {extent:?} is past what the rasteriser places exactly"
        );
    }
}

#[test]
fn a_band_with_no_render_set_up_is_refused() {
    let mut service = opened(&png_with(2, 2, |_, _| [1, 2, 3, 255]));
    service.handle(&page_request(0));
    assert_eq!(
        refused(&service.handle(&band_request(0, 1))),
        Some(super::ViewRefusal::NoRender)
    );
}

#[test]
fn changing_page_drops_the_render_that_described_the_old_one() {
    let mut service = opened(&ico_of(&[(4, [1, 0, 0, 255]), (2, [0, 1, 0, 255])]));
    service.handle(&page_request(0));
    assert_eq!(
        service.handle(&render_request((4, 4), (0, 0, 4, 4)))[0],
        super::REPLY_VIEW_RENDERED
    );
    // A scaling of page 0 shows page 0, so it describes nothing of the
    // page that replaces it.
    service.handle(&page_request(1));
    assert_eq!(
        refused(&service.handle(&band_request(0, 1))),
        Some(super::ViewRefusal::NoRender),
        "a rectangle of the page just replaced is never drawn against its \
         replacement"
    );
}

#[test]
fn a_band_outside_the_renders_destination_is_refused() {
    let mut service = opened(&png_with(2, 2, |_, _| [1, 2, 3, 255]));
    service.handle(&page_request(0));
    service.handle(&render_request((2, 2), (0, 0, 2, 2)));
    for (first_row, rows) in [(0, 0), (0, 3), (2, 1), (1, 2), (u32::MAX, 1)] {
        assert_eq!(
            refused(&service.handle(&band_request(first_row, rows))),
            Some(super::ViewRefusal::BandOutOfRange),
            "rows {first_row}..+{rows} do not lie inside a 2-row destination"
        );
    }
}

#[test]
fn trailing_bytes_on_any_view_request_are_refused() {
    let mut service = opened(&png_with(2, 2, |_, _| [1, 2, 3, 255]));
    service.handle(&page_request(0));
    service.handle(&render_request((2, 2), (0, 0, 2, 2)));
    for base in [
        request(super::OP_VIEW_OPEN, &[0]),
        page_request(0),
        render_request((2, 2), (0, 0, 2, 2)),
        band_request(0, 1),
        request(super::OP_VIEW_RELEASE, &[]),
    ] {
        let mut probe = base.clone();
        probe.push(0);
        assert_eq!(
            refused(&service.handle(&probe)),
            Some(super::ViewRefusal::MalformedRequest),
            "a request with a byte after its fields is not that request"
        );
    }
}

#[test]
fn every_view_refusal_states_a_reason() {
    for refusal in [
        super::ViewRefusal::MalformedRequest,
        super::ViewRefusal::NoDocument,
        super::ViewRefusal::UnsupportedFormat,
        super::ViewRefusal::MalformedDocument,
        super::ViewRefusal::NotOpen,
        super::ViewRefusal::NoSuchPage,
        super::ViewRefusal::NoPageDecoded,
        super::ViewRefusal::NoRender,
        super::ViewRefusal::BandOutOfRange,
        super::ViewRefusal::Unrenderable,
        super::ViewRefusal::TooLarge,
    ] {
        assert!(!format!("{refusal}").is_empty());
        assert_eq!(
            super::ViewRefusal::from_wire(refusal.to_wire()),
            Some(refusal),
            "every refusal survives the wire it is carried on"
        );
    }
    for refusal in [
        super::DocumentRefusal::MalformedRequest,
        super::DocumentRefusal::TooLarge,
        super::DocumentRefusal::NotBegun,
        super::DocumentRefusal::Overrun,
        super::DocumentRefusal::OutOfMemory,
    ] {
        assert!(!format!("{refusal}").is_empty());
        assert_eq!(
            super::DocumentRefusal::from_wire(refusal.to_wire()),
            Some(refusal)
        );
    }
}

// ---- a worker that turns hostile part-way through a session --------------

/// Serves honestly until the request naming `op`, whose reply it corrupts.
///
/// The view is a session, so a reply cannot be judged in isolation the way
/// the icon path's can: the parent has to be walked all the way to the
/// band before there is a band to lie about.
struct TamperingWorker {
    inner: ImageRenderService,
    op: u8,
    tamper: fn(Vec<u8>) -> Vec<u8>,
}

impl Service for TamperingWorker {
    fn handle(&mut self, request: &[u8]) -> Vec<u8> {
        let reply = self.inner.handle(request);
        if request.first().copied() == Some(self.op) {
            (self.tamper)(reply)
        } else {
            reply
        }
    }
}

fn tampering(
    op: u8,
    tamper: fn(Vec<u8>) -> Vec<u8>,
) -> ParserSandbox<LoopbackLauncher<impl FnMut() -> TamperingWorker>, NullSink> {
    ParserSandbox::new(
        LoopbackLauncher::new(move || TamperingWorker {
            inner: ImageRenderService::default(),
            op,
            tamper,
        }),
        NullSink,
    )
}

/// Walk a tampering sandbox through a whole render of a 2×2 picture,
/// answering whatever the first step to fail reports.
fn drive_tampered(
    sandbox: &mut ParserSandbox<LoopbackLauncher<impl FnMut() -> TamperingWorker>, NullSink>,
) -> Result<(), super::ViewFailure> {
    let png = png_with(2, 2, |_, _| [1, 2, 3, 255]);
    super::send_document(sandbox, &png).map_err(super::ViewFailure::Document)?;
    super::open_view(sandbox, None)?;
    super::select_page(sandbox, 0)?;
    let mut out = vec![0u8; 2 * 2 * 4];
    super::render_page(sandbox, (2, 2), whole(2, 2), &mut out)
}

#[test]
fn a_band_echoing_a_row_range_that_was_not_asked_for_is_refused() {
    let mut sandbox = tampering(super::OP_VIEW_BAND, |mut reply| {
        // Bytes 1..5 are the echoed first row.
        reply[1] = reply[1].wrapping_add(1);
        reply
    });
    assert_eq!(
        drive_tampered(&mut sandbox),
        Err(super::ViewFailure::ReplyMalformed)
    );
}

#[test]
fn a_band_carrying_the_wrong_number_of_pixels_is_refused() {
    let mut sandbox = tampering(super::OP_VIEW_BAND, |mut reply| {
        reply.push(0);
        reply
    });
    assert_eq!(
        drive_tampered(&mut sandbox),
        Err(super::ViewFailure::ReplyMalformed)
    );
}

#[test]
fn a_band_reply_cut_short_of_its_pixels_is_refused() {
    let mut sandbox = tampering(super::OP_VIEW_BAND, |mut reply| {
        reply.truncate(reply.len() - 1);
        reply
    });
    assert_eq!(
        drive_tampered(&mut sandbox),
        Err(super::ViewFailure::ReplyMalformed)
    );
}

#[test]
fn a_render_claiming_it_can_carry_no_rows_is_refused_rather_than_looped_on() {
    let mut sandbox = tampering(super::OP_VIEW_RENDER, |mut reply| {
        for byte in reply.iter_mut().skip(1) {
            *byte = 0;
        }
        reply
    });
    assert_eq!(
        drive_tampered(&mut sandbox),
        Err(super::ViewFailure::ReplyMalformed),
        "a band size of zero would never reach the last row"
    );
}

#[test]
fn a_page_reply_about_some_other_page_is_refused() {
    let mut sandbox = tampering(super::OP_VIEW_PAGE, |mut reply| {
        reply[1] = reply[1].wrapping_add(1);
        reply
    });
    assert_eq!(
        drive_tampered(&mut sandbox),
        Err(super::ViewFailure::ReplyMalformed)
    );
}

#[test]
fn a_page_reply_claiming_no_pixels_is_refused() {
    let mut sandbox = tampering(super::OP_VIEW_PAGE, |mut reply| {
        // Bytes 5..9 are the page's width.
        for byte in reply.iter_mut().skip(5).take(4) {
            *byte = 0;
        }
        reply
    });
    assert_eq!(
        drive_tampered(&mut sandbox),
        Err(super::ViewFailure::ReplyMalformed)
    );
}

#[test]
fn an_open_reply_naming_a_format_the_protocol_does_not_carry_is_refused() {
    let mut sandbox = tampering(super::OP_VIEW_OPEN, |mut reply| {
        reply[1] = 0xEE;
        reply
    });
    assert_eq!(
        drive_tampered(&mut sandbox),
        Err(super::ViewFailure::ReplyMalformed)
    );
}

#[test]
fn an_open_reply_giving_a_page_container_a_loop_count_is_refused() {
    let mut sandbox = tampering(super::OP_VIEW_OPEN, |mut reply| {
        // Byte 2 is `animated`, byte 3 whether a loop count follows.
        reply[3] = 1;
        reply
    });
    assert_eq!(
        drive_tampered(&mut sandbox),
        Err(super::ViewFailure::ReplyMalformed),
        "only something that is played can declare how often to play it"
    );
}

#[test]
fn an_open_reply_with_a_flag_byte_that_is_not_a_flag_is_refused() {
    let mut sandbox = tampering(super::OP_VIEW_OPEN, |mut reply| {
        reply[2] = 2;
        reply
    });
    assert_eq!(
        drive_tampered(&mut sandbox),
        Err(super::ViewFailure::ReplyMalformed)
    );
}

#[test]
fn an_open_reply_declaring_an_empty_document_is_refused() {
    let mut sandbox = tampering(super::OP_VIEW_OPEN, |mut reply| {
        // Bytes 8..12 are the entry count.
        for byte in reply.iter_mut().skip(8).take(4) {
            *byte = 0;
        }
        reply
    });
    assert_eq!(
        drive_tampered(&mut sandbox),
        Err(super::ViewFailure::ReplyMalformed)
    );
}

#[test]
fn a_push_reply_disagreeing_about_how_much_arrived_is_refused() {
    let mut sandbox = tampering(super::OP_DOC_PUSH, |mut reply| {
        reply[1] = reply[1].wrapping_add(1);
        reply
    });
    assert_eq!(
        drive_tampered(&mut sandbox),
        Err(super::ViewFailure::Document(
            super::DocumentFailure::ReplyMalformed
        ))
    );
}

#[test]
fn a_view_reply_with_an_unknown_tag_is_refused() {
    let mut sandbox = tampering(super::OP_VIEW_OPEN, |_| vec![0xEE]);
    assert_eq!(
        drive_tampered(&mut sandbox),
        Err(super::ViewFailure::ReplyMalformed)
    );
}

#[test]
fn a_refusal_code_no_refusal_uses_is_not_read_as_one() {
    let mut sandbox = tampering(super::OP_VIEW_OPEN, |_| vec![super::REPLY_ERROR, 0xEE]);
    assert_eq!(
        drive_tampered(&mut sandbox),
        Err(super::ViewFailure::ReplyMalformed)
    );
}

#[test]
fn an_untampered_session_completes_so_the_tampering_is_what_is_being_tested() {
    let mut sandbox = tampering(0xEE, |reply| reply);
    assert_eq!(drive_tampered(&mut sandbox), Ok(()));
}

#[test]
fn opening_early_leaves_an_unfinished_upload_where_it_was() {
    let mut service = ImageRenderService::default();
    let png = png_with(2, 2, |_, _| [1, 2, 3, 255]);
    service.handle(&begin(png.len() as u64));
    service.handle(&push(&png[..2]));
    assert_eq!(
        refused(&service.handle(&request(super::OP_VIEW_OPEN, &[0]))),
        Some(super::ViewRefusal::NoDocument)
    );
    // Refusing an upload that has simply not finished must not throw away
    // what has arrived: the rest can still be pushed.
    service.handle(&push(&png[2..]));
    assert_eq!(
        service.handle(&request(super::OP_VIEW_OPEN, &[0]))[0],
        super::REPLY_VIEW_OPENED
    );
}

#[test]
fn bands_of_one_vector_render_are_the_rows_of_it_they_claim_to_be() {
    // A tall window arrives in pieces, and each piece rasterises its own
    // rows of the magnification: a band that read from the window's top
    // instead of from its own offset would repeat the same strip.
    let mut service = ImageRenderService::default();
    // Varying down the page, so two bands cannot agree by coincidence.
    load(
        &mut service,
        br#"<svg viewBox="0 0 4 4"><polygon points="0,0 4,4 0,4"/></svg>"#,
    );
    assert_eq!(
        service.handle(&request(super::OP_VIEW_OPEN, &[0]))[0],
        super::REPLY_VIEW_OPENED
    );
    service.handle(&page_request(0));
    assert_eq!(
        service.handle(&render_request((12, 12), (2, 3, 6, 4)))[0],
        super::REPLY_VIEW_RENDERED
    );
    let pixels = |reply: &[u8]| {
        assert_eq!(reply.first().copied(), Some(super::REPLY_VIEW_BAND));
        // Tag, echoed first row, echoed rows, and the pixel field's own
        // length prefix.
        reply[13..].to_vec()
    };
    let whole = pixels(&service.handle(&band_request(0, 4)));
    let mut assembled = pixels(&service.handle(&band_request(0, 1)));
    assembled.extend_from_slice(&pixels(&service.handle(&band_request(1, 3))));
    assert_eq!(assembled, whole);
    assert_ne!(
        whole[..6 * 4],
        whole[6 * 4..2 * 6 * 4],
        "the drawing genuinely varies down the window"
    );
}

#[test]
fn a_band_wider_than_a_reply_frame_carries_is_refused_before_it_is_drawn() {
    // The largest destination this service draws needs several bands, so
    // asking for all its rows at once names a reply no frame could hold.
    let side = super::MAX_DESTINATION_WIDTH;
    let rows = super::MAX_DESTINATION_HEIGHT;
    let per_band = super::rows_per_band(side);
    assert!(
        per_band < rows,
        "the largest destination must genuinely need more than one band \
         for this to be testing anything"
    );
    let mut service = opened(&png_with(2, 2, |_, _| [1, 2, 3, 255]));
    service.handle(&page_request(0));
    assert_eq!(
        service.handle(&render_request((side, rows), (0, 0, side, rows)))[0],
        super::REPLY_VIEW_RENDERED
    );
    assert_eq!(
        refused(&service.handle(&band_request(0, per_band + 1))),
        Some(super::ViewRefusal::BandOutOfRange)
    );
    // One band's worth is served, so the bound is the frame and not the
    // destination.
    assert_eq!(
        service.handle(&band_request(0, per_band))[0],
        super::REPLY_VIEW_BAND
    );
}

#[test]
fn a_wallpaper_band_wider_than_a_reply_frame_carries_is_refused_too() {
    let width = super::MAX_DESTINATION_WIDTH;
    let height = super::MAX_DESTINATION_HEIGHT;
    let per_band = super::rows_per_band(width);
    let mut service = ImageRenderService::default();
    load(&mut service, &png_with(2, 2, |_, _| [4, 5, 6, 255]));
    let mut w = Writer::new();
    w.u8(super::OP_WALLPAPER_PREPARE);
    for field in [width, height, width, height] {
        w.u32(field);
    }
    w.u8(0);
    assert_eq!(
        service.handle(&w.finish())[0],
        super::REPLY_WALLPAPER_PREPARED
    );
    let mut band = Writer::new();
    band.u8(super::OP_WALLPAPER_BAND);
    band.u32(0);
    band.u32(per_band + 1);
    assert_eq!(
        service.handle(&band.finish()),
        vec![
            super::REPLY_ERROR,
            super::REFUSAL_WALLPAPER_BAND_OUT_OF_RANGE
        ]
    );
}
