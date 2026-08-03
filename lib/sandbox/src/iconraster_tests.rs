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

use super::{IconRasterFailure, IconRasterService, IconRefusal, MAX_ICON_SIDE};
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

type TestSandbox = ParserSandbox<LoopbackLauncher<fn() -> IconRasterService>, NullSink>;

fn sandbox() -> TestSandbox {
    ParserSandbox::new(
        LoopbackLauncher::new(IconRasterService::default as fn() -> IconRasterService),
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
    let mut service = IconRasterService;
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
