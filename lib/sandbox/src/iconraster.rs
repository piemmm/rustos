//! The sandboxed icon-rasterisation service.
//!
//! An application bundle's icon artwork — SVG or PNG bytes shipped inside
//! the bundle by whoever authored it, not by the system — is untrusted
//! input, so decoding it and turning it into pixels the compositor can
//! blit must not run in the calling desktop session's process. This
//! module is the parser-sandbox service for it: the worker side sniffs
//! the format, decodes it, rasterises it to the caller's requested square
//! side, and replies with validated straight-alpha RGBA8 pixels; the
//! parent side ([`rasterise_icon`]) trusts nothing about the reply beyond
//! its length and echoed side before handing the bytes to the compositor.
//! A crashed or misbehaving worker is contained and replaced by the
//! [`crate::host::ParserSandbox`] seam, and either failure mode — a typed
//! refusal or a sandbox failure — simply means the caller falls back to
//! its own built-in glyph.
//!
//! # Producing the pixels
//!
//! An **SVG** icon decodes into the desktop's shared vector form
//! (`tairix_svg::decode` then `tairix_icon::VectorIcon::from_svg`) and
//! rasterises directly onto a `side`×`side` surface through the one
//! supersampled polygon-fill path every vector asset shares
//! (`VectorIcon::rasterise`); the premultiplied surface is un-premultiplied
//! back to straight alpha for the wire.
//!
//! A **PNG** icon decodes through the complete, fail-closed
//! `tairix_image` decoder, bounded by its own decode limits — tighter than
//! and distinct from the requested output side, so a small `side` cannot
//! be used to smuggle a huge source image past a small reply — and is
//! then scaled to `side`×`side`: the source is
//! fitted inside the square preserving its aspect ratio and centred, with
//! fully transparent padding on the shorter axis, and every destination
//! pixel is the alpha-weighted average of the source pixels its box
//! covers (a small integer box filter, not nearest-neighbour, so a
//! downscale blends instead of aliasing).

use alloc::vec;
use alloc::vec::Vec;

use tairix_icon::VectorIcon;
use tairix_image::{DecodeLimits, ImageFormat};
use tairix_raster::Surface;
use tairix_svg::{SvgError, SvgImage};

use crate::host::{Launcher, ParserSandbox, SandboxError};
use crate::wire::{Reader, Writer};
use crate::worker::Service;

#[cfg(test)]
#[path = "iconraster_tests.rs"]
mod tests;

/// Largest icon file the service will decode, in bytes.
///
/// A fixed validation bound, not a growable capacity: real desktop icon
/// artwork (SVG source or a modestly sized PNG) is a tiny fraction of
/// this, so the ceiling exists purely to bound how much hostile work a
/// single request can demand before any byte is decoded.
pub const MAX_ICON_INPUT: usize = 256 * 1024;

/// Largest pixel side a caller may request for the rasterised output.
///
/// `512 * 512 * 4` bytes is a 1 MiB reply, comfortably under
/// [`crate::proto::MAX_FRAME`]; no desktop icon slot is ever asked to
/// render larger than this.
pub const MAX_ICON_SIDE: u32 = 512;

/// Largest source width or height a PNG icon may declare, before any
/// scaling to the requested output `side`.
///
/// This is a *decode-time* bound on the source image the worker will ever
/// hold in memory, kept well above any real icon's native resolution but
/// far below a value that could turn a small request into an expensive
/// decode; it is deliberately independent of `side` so a caller asking
/// for a tiny output cannot use that to sneak a huge source image past
/// the reply-size limit.
const PNG_DECODE_MAX_SIDE: u32 = 2048;

/// Largest total PNG source pixel count ([`PNG_DECODE_MAX_SIDE`] squared).
const PNG_DECODE_MAX_PIXELS: u64 = 2048 * 2048;

/// Request opcode.
const OP_RASTERISE: u8 = 1;

/// Reply tags.
const REPLY_ERROR: u8 = 0;
const REPLY_PIXELS: u8 = 1;

/// Refusal wire codes.
const REFUSAL_MALFORMED_REQUEST: u8 = 1;
const REFUSAL_UNSUPPORTED_FORMAT: u8 = 2;
const REFUSAL_MALFORMED_IMAGE: u8 = 3;
const REFUSAL_UNRENDERABLE: u8 = 4;

/// Why the service refused a request, carried typed over the wire.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum IconRefusal {
    /// The request payload violated the request grammar (bad opcode, an
    /// out-of-range `side`, an oversize icon, or trailing bytes).
    MalformedRequest,
    /// The icon bytes are neither a recognised PNG signature nor a
    /// document that even looks like SVG (not UTF-8, or no `<svg>` root).
    UnsupportedFormat,
    /// The bytes are the recognised format but failed its decoder.
    MalformedImage,
    /// The image decoded successfully but could not be rasterised at the
    /// requested `side` (e.g. the output surface could not be allocated).
    /// The caller falls back to its own built-in glyph either way.
    Unrenderable,
}

impl IconRefusal {
    const fn to_wire(self) -> u8 {
        match self {
            Self::MalformedRequest => REFUSAL_MALFORMED_REQUEST,
            Self::UnsupportedFormat => REFUSAL_UNSUPPORTED_FORMAT,
            Self::MalformedImage => REFUSAL_MALFORMED_IMAGE,
            Self::Unrenderable => REFUSAL_UNRENDERABLE,
        }
    }

    const fn from_wire(raw: u8) -> Option<Self> {
        match raw {
            REFUSAL_MALFORMED_REQUEST => Some(Self::MalformedRequest),
            REFUSAL_UNSUPPORTED_FORMAT => Some(Self::UnsupportedFormat),
            REFUSAL_MALFORMED_IMAGE => Some(Self::MalformedImage),
            REFUSAL_UNRENDERABLE => Some(Self::Unrenderable),
            _ => None,
        }
    }
}

impl core::fmt::Display for IconRefusal {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::MalformedRequest => f.write_str("malformed icon-rasterise request"),
            Self::UnsupportedFormat => f.write_str("icon bytes are neither PNG nor SVG"),
            Self::MalformedImage => f.write_str("icon image failed to decode"),
            Self::Unrenderable => {
                f.write_str("icon decoded but could not be rasterised at the requested size")
            }
        }
    }
}

/// Typed failure [`rasterise_icon`] can report.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum IconRasterFailure {
    /// The sandbox itself failed (crash, launch failure, oversize).
    Sandbox(SandboxError),
    /// The worker refused the request with the carried typed reason.
    Refused(IconRefusal),
    /// The worker's reply violated the reply grammar or lied about its
    /// geometry: it cannot be believed, so the caller gets nothing
    /// (fail closed).
    ReplyMalformed,
}

impl core::fmt::Display for IconRasterFailure {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Sandbox(inner) => write!(f, "parser sandbox failed: {inner:?}"),
            Self::Refused(refusal) => write!(f, "worker refused: {refusal}"),
            Self::ReplyMalformed => f.write_str("worker reply violated the reply grammar"),
        }
    }
}

/// The service the sandboxed worker runs: sniff, decode, and rasterise
/// the icon, a reply payload out. Total by construction — every failure
/// is a typed error reply.
#[derive(Debug, Default)]
pub struct IconRasterService;

impl Service for IconRasterService {
    fn handle(&mut self, request: &[u8]) -> Vec<u8> {
        match dispatch(request) {
            Ok(reply) => reply,
            Err(refusal) => {
                let mut w = Writer::new();
                w.u8(REPLY_ERROR);
                w.u8(refusal.to_wire());
                w.finish()
            }
        }
    }
}

/// Decode the request, rasterise, and encode the reply.
fn dispatch(request: &[u8]) -> Result<Vec<u8>, IconRefusal> {
    let mut r = Reader::new(request);
    let op = r.u8().map_err(|_| IconRefusal::MalformedRequest)?;
    if op != OP_RASTERISE {
        return Err(IconRefusal::MalformedRequest);
    }
    let side = r.u32().map_err(|_| IconRefusal::MalformedRequest)?;
    if side == 0 || side > MAX_ICON_SIDE {
        return Err(IconRefusal::MalformedRequest);
    }
    let icon = r
        .bytes(MAX_ICON_INPUT)
        .map_err(|_| IconRefusal::MalformedRequest)?;
    if !r.is_exhausted() {
        return Err(IconRefusal::MalformedRequest);
    }
    let rgba = rasterise(side, icon)?;
    let mut w = Writer::new();
    w.u8(REPLY_PIXELS);
    w.u32(side);
    w.bytes(&rgba);
    Ok(w.finish())
}

/// Sniff `icon`'s format and rasterise it to a `side`×`side` straight-alpha
/// RGBA8 buffer of exactly `side * side * 4` bytes.
fn rasterise(side: u32, icon: &[u8]) -> Result<Vec<u8>, IconRefusal> {
    if tairix_image::sniff(icon) == Some(ImageFormat::Png) {
        return rasterise_png(side, icon);
    }
    match tairix_svg::decode(icon) {
        Ok(image) => rasterise_svg(side, &image),
        // These two reasons mean the bytes do not even look like an SVG
        // document — not UTF-8, or no `<svg>` root at all — the same
        // "this is not a format we recognise" verdict `sniff` gives PNG,
        // just without a byte signature to check first. Every other
        // `SvgError` means the bytes *are* shaped like SVG but violate the
        // supported subset, which is a decode failure, not an
        // unrecognised format.
        Err(SvgError::NotUtf8 | SvgError::MissingRoot) => Err(IconRefusal::UnsupportedFormat),
        Err(_) => Err(IconRefusal::MalformedImage),
    }
}

/// Rasterise a decoded SVG icon directly onto a `side`×`side` surface and
/// un-premultiply it back to the straight-alpha wire form.
fn rasterise_svg(side: u32, image: &SvgImage) -> Result<Vec<u8>, IconRefusal> {
    let icon = VectorIcon::from_svg(image);
    let surface = icon.rasterise(side).ok_or(IconRefusal::Unrenderable)?;
    Ok(straight_alpha_from_surface(&surface))
}

/// Un-premultiply every pixel of a rendered [`Surface`] into a row-major
/// straight-alpha RGBA8 buffer.
fn straight_alpha_from_surface(surface: &Surface) -> Vec<u8> {
    let mut out = Vec::with_capacity(surface.pixels().len().saturating_mul(4));
    for pixel in surface.pixels() {
        let colour = pixel.unpremultiply();
        out.push(colour.r);
        out.push(colour.g);
        out.push(colour.b);
        out.push(colour.a);
    }
    out
}

/// Decode a PNG icon (bounded by [`PNG_DECODE_MAX_SIDE`] /
/// [`PNG_DECODE_MAX_PIXELS`], not by the requested `side`) and scale it to
/// `side`×`side`.
fn rasterise_png(side: u32, icon: &[u8]) -> Result<Vec<u8>, IconRefusal> {
    let limits = DecodeLimits::new(
        PNG_DECODE_MAX_SIDE,
        PNG_DECODE_MAX_SIDE,
        PNG_DECODE_MAX_PIXELS,
    );
    let image = tairix_image::decode(icon, &limits).map_err(|_| IconRefusal::MalformedImage)?;
    Ok(scale_to_square(
        image.width(),
        image.height(),
        image.pixels(),
        side,
    ))
}

/// Scale straight-alpha RGBA8 `src` (`src_w`×`src_h`) into a `side`×`side`
/// straight-alpha RGBA8 buffer of exactly `side * side * 4` bytes.
///
/// The source is fitted inside the square preserving its aspect ratio
/// ([`fit_within`]) and centred, leaving fully transparent padding on the
/// shorter axis. Each destination pixel inside the fitted rectangle is the
/// alpha-weighted average of the source pixels its box covers
/// ([`box_range`], [`average_box`]) — a small integer box filter, so a
/// downscale blends rather than aliases and an upscale degrades to
/// sample-and-hold rather than smearing.
fn scale_to_square(src_w: u32, src_h: u32, src: &[u8], side: u32) -> Vec<u8> {
    let mut out = vec![0u8; pixel_buffer_len(side, side)];
    let (fit_w, fit_h) = fit_within(src_w, src_h, side);
    let x0 = (side - fit_w) / 2;
    let y0 = (side - fit_h) / 2;
    for dy in 0..fit_h {
        let (sy0, sy1) = box_range(dy, fit_h, src_h);
        let out_y = y0 + dy;
        for dx in 0..fit_w {
            let (sx0, sx1) = box_range(dx, fit_w, src_w);
            let out_x = x0 + dx;
            let (r, g, b, a) = average_box(src, src_w, sx0, sx1, sy0, sy1);
            if let Some(offset) = pixel_offset(out_x, out_y, side) {
                if let Some(slot) = out.get_mut(offset..offset + 4) {
                    slot.copy_from_slice(&[r, g, b, a]);
                }
            }
        }
    }
    out
}

/// The largest `(width, height)` no bigger than `side` on either axis that
/// preserves `src_w`/`src_h`'s aspect ratio, so the source is never
/// distorted — [`scale_to_square`] pads the shorter mapped axis
/// transparent rather than stretching it.
///
/// `side` is already validated non-zero by every caller (the request
/// dispatch bound of `1..=MAX_ICON_SIDE`); this private helper relies on
/// that rather than re-checking it, exactly as `RasterImage::from_parts`
/// relies on its own already-validated geometry.
fn fit_within(src_w: u32, src_h: u32, side: u32) -> (u32, u32) {
    if src_w >= src_h {
        (side, scale_dimension(src_h, src_w, side))
    } else {
        (scale_dimension(src_w, src_h, side), side)
    }
}

/// `round(value * side / reference)`, clamped to `1..=side`.
///
/// `reference` is `src_w` or `src_h` from a decoded PNG, which the format
/// decoder already refuses to be zero; the zero guard below only keeps
/// this function total rather than leaning on that invariant.
fn scale_dimension(value: u32, reference: u32, side: u32) -> u32 {
    if reference == 0 {
        return 1;
    }
    let numerator = u64::from(value)
        .saturating_mul(u64::from(side))
        .saturating_add(u64::from(reference) / 2);
    let scaled = numerator / u64::from(reference);
    u32::try_from(scaled).unwrap_or(side).clamp(1, side)
}

/// The half-open source-pixel range `[start, end)` along one axis that
/// destination pixel `dst` (of `dst_len` total) should average, given
/// `src_len` source pixels on that axis.
///
/// The axis is split into `dst_len` equal fixed-point spans (`u64`
/// throughout, so no bounded icon geometry can overflow the products); a
/// span that would come out empty — destination longer than source, i.e.
/// upscaling — is widened to exactly one source pixel, so upscaling
/// degrades to sample-and-hold rather than averaging an empty box.
fn box_range(dst: u32, dst_len: u32, src_len: u32) -> (u32, u32) {
    let start = (u64::from(dst) * u64::from(src_len)) / u64::from(dst_len);
    let mut end = ((u64::from(dst) + 1) * u64::from(src_len)) / u64::from(dst_len);
    if end <= start {
        end = start + 1;
    }
    // `dst < dst_len` guarantees `start < src_len`, and the arithmetic
    // above guarantees `end <= src_len`; both casts are therefore exact.
    (
        u32::try_from(start).unwrap_or(0),
        u32::try_from(end).unwrap_or(src_len),
    )
}

/// Average the pixels of straight-alpha RGBA8 `src` (`src_w` wide) inside
/// `[sx0, sx1) x [sy0, sy1)`, weighting each pixel's colour by its own
/// alpha before averaging and averaging alpha unweighted — so a fully
/// transparent source pixel contributes no colour (never darkening a
/// half-transparent edge) while still pulling the average alpha down.
///
/// Every accumulator is `u64`: the largest permitted box holds at most
/// [`PNG_DECODE_MAX_PIXELS`] samples, each channel product at most
/// `255 * 255`, nowhere near overflowing 64 bits.
fn average_box(src: &[u8], src_w: u32, sx0: u32, sx1: u32, sy0: u32, sy1: u32) -> (u8, u8, u8, u8) {
    let mut sum_r: u64 = 0;
    let mut sum_g: u64 = 0;
    let mut sum_b: u64 = 0;
    let mut sum_a: u64 = 0;
    let mut count: u64 = 0;
    for y in sy0..sy1 {
        for x in sx0..sx1 {
            let Some(offset) = pixel_offset(x, y, src_w) else {
                continue;
            };
            let Some(pixel) = src.get(offset..offset + 4) else {
                continue;
            };
            let alpha = u64::from(pixel[3]);
            sum_r += u64::from(pixel[0]) * alpha;
            sum_g += u64::from(pixel[1]) * alpha;
            sum_b += u64::from(pixel[2]) * alpha;
            sum_a += alpha;
            count += 1;
        }
    }
    if count == 0 {
        return (0, 0, 0, 0);
    }
    let avg_a = round_div(sum_a, count);
    let (avg_r, avg_g, avg_b) = if sum_a == 0 {
        (0, 0, 0)
    } else {
        (
            round_div(sum_r, sum_a),
            round_div(sum_g, sum_a),
            round_div(sum_b, sum_a),
        )
    };
    (avg_r, avg_g, avg_b, avg_a)
}

/// Round `numerator / denominator` to the nearest integer and clamp it
/// into `u8`.
///
/// Every caller's `denominator` is a sum of `u8` values (so `numerator`
/// can never exceed it once divided) and is checked non-zero before this
/// runs; the guard below only keeps the function total rather than
/// leaning on that invariant.
fn round_div(numerator: u64, denominator: u64) -> u8 {
    if denominator == 0 {
        return 0;
    }
    let rounded = (numerator + denominator / 2) / denominator;
    u8::try_from(rounded.min(u64::from(u8::MAX))).unwrap_or(u8::MAX)
}

/// Byte offset of pixel `(x, y)` in a row-major RGBA8 buffer `width`
/// pixels wide, or `None` if the coordinate or the resulting offset would
/// not fit a `usize` — unreachable for any icon geometry this service
/// bounds, but checked rather than assumed.
fn pixel_offset(x: u32, y: u32, width: u32) -> Option<usize> {
    let row = u64::from(y).checked_mul(u64::from(width))?;
    let index = row.checked_add(u64::from(x))?;
    let byte_offset = index.checked_mul(4)?;
    usize::try_from(byte_offset).ok()
}

/// `width * height * 4`, saturating rather than overflowing.
///
/// Every caller bounds `width`/`height` well below the point this could
/// matter (icon sides are capped at [`MAX_ICON_SIDE`]), so saturation is
/// unreachable in practice and only keeps the arithmetic total.
fn pixel_buffer_len(width: u32, height: u32) -> usize {
    let count = u64::from(width).saturating_mul(u64::from(height));
    let bytes = count.saturating_mul(4);
    usize::try_from(bytes).unwrap_or(usize::MAX)
}

/// Ask the sandboxed worker to decode `icon` (SVG or PNG bytes) and
/// rasterise it to a `side`×`side` straight-alpha RGBA8 image.
///
/// `side` and `icon.len()` are checked locally against
/// [`MAX_ICON_SIDE`]/[`MAX_ICON_INPUT`] before anything is sent, so an
/// out-of-bounds request never round-trips through the sandbox just to be
/// refused. The reply is never trusted as-is: the tag, the echoed side,
/// and the exact pixel length are all validated before the bytes are
/// returned, so a compromised worker can lie about its geometry, never
/// hand the caller a buffer of the wrong size.
///
/// # Errors
///
/// [`IconRasterFailure`]: the sandbox failed, the worker refused the
/// request (bad shape, an unrecognised format, a decode failure, or a
/// decode that could not be rasterised at `side`), or the reply could not
/// be believed.
pub fn rasterise_icon<L: Launcher, S: tairix_log::Sink>(
    sandbox: &mut ParserSandbox<L, S>,
    side: u32,
    icon: &[u8],
) -> Result<Vec<u8>, IconRasterFailure> {
    if side == 0 || side > MAX_ICON_SIDE || icon.len() > MAX_ICON_INPUT {
        return Err(IconRasterFailure::Refused(IconRefusal::MalformedRequest));
    }
    let mut w = Writer::new();
    w.u8(OP_RASTERISE);
    w.u32(side);
    w.bytes(icon);
    let reply = sandbox
        .request(&w.finish())
        .map_err(IconRasterFailure::Sandbox)?;
    decode_reply(&reply, side)
}

/// Decode and validate the worker's reply fail-closed.
fn decode_reply(reply: &[u8], side: u32) -> Result<Vec<u8>, IconRasterFailure> {
    let mut r = Reader::new(reply);
    let tag = r.u8().map_err(|_| IconRasterFailure::ReplyMalformed)?;
    match tag {
        REPLY_PIXELS => {
            let echoed_side = r.u32().map_err(|_| IconRasterFailure::ReplyMalformed)?;
            if echoed_side != side {
                return Err(IconRasterFailure::ReplyMalformed);
            }
            let expected_len = pixel_buffer_len(side, side);
            let pixels = r
                .bytes(expected_len)
                .map_err(|_| IconRasterFailure::ReplyMalformed)?;
            if pixels.len() != expected_len || !r.is_exhausted() {
                return Err(IconRasterFailure::ReplyMalformed);
            }
            Ok(pixels.to_vec())
        }
        REPLY_ERROR => {
            let code = r.u8().map_err(|_| IconRasterFailure::ReplyMalformed)?;
            if !r.is_exhausted() {
                return Err(IconRasterFailure::ReplyMalformed);
            }
            let refusal = IconRefusal::from_wire(code).ok_or(IconRasterFailure::ReplyMalformed)?;
            Err(IconRasterFailure::Refused(refusal))
        }
        _ => Err(IconRasterFailure::ReplyMalformed),
    }
}
