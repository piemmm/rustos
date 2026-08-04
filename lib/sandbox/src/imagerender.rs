//! The sandboxed image-rendering service: icon rasterisation and desktop
//! wallpaper placement.
//!
//! An application bundle's icon artwork (SVG or PNG bytes shipped inside
//! the bundle by whoever authored it, not by the system) and a desktop
//! wallpaper (a photograph the user picked, or a shipped master) are both
//! untrusted input, so decoding and drawing them must not run in the
//! calling desktop session's process. This module is the parser-sandbox
//! service for both: the worker side sniffs the format, decodes it, draws
//! it, and replies with validated straight-alpha RGBA8 pixels; the parent
//! side ([`rasterise_icon`], [`render_wallpaper`]) trusts nothing about a
//! reply beyond its length and echoed geometry before handing the bytes to
//! the compositor. A crashed or misbehaving worker is contained and
//! replaced by the [`crate::host::ParserSandbox`] seam, and either failure
//! mode — a typed refusal or a sandbox failure — simply means the caller
//! falls back to its own built-in glyph or the desktop backdrop colour.
//!
//! # Producing icon pixels
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
//! then fitted inside the `side`×`side` square preserving its aspect
//! ratio and centred, with fully transparent padding on the shorter axis,
//! through the crate's one shared resampler (`tairix_raster::resample`) —
//! never a second, private scaling implementation.
//!
//! # Producing wallpaper pixels
//!
//! `OP_WALLPAPER_PREPARE` sniffs and decodes the source image at the
//! smallest scale its format offers that still covers the requested
//! destination (`tairix_image::decode_fitted`, so a 25-megapixel master
//! bound for a 1080p screen never becomes 25 megapixels of held RGBA),
//! computes its placement onto that destination through the one shared
//! placement geometry (`tairix_wallpaper::place`), and holds both until
//! `OP_WALLPAPER_RELEASE`. `OP_WALLPAPER_BAND` then produces destination
//! rows of that placement a band at a time — bounded by
//! [`crate::proto::MAX_FRAME`], never raised to fit a larger reply — either
//! by repeating the decoded source at 1:1
//! ([`tairix_wallpaper::WallpaperFit::Tile`])
//! or by resampling its placed source rectangle through the same shared
//! resampler the icon path uses. Wherever the destination is not fully
//! covered by the placement (a letterboxed fit, a source smaller than the
//! screen), those pixels are fully transparent, so the desktop's own
//! backdrop colour shows through — this service never draws a backdrop.

use alloc::vec;
use alloc::vec::Vec;

use tairix_geometry::Rect;
use tairix_icon::{VectorIcon, MAX_ARTWORK_BYTES, MAX_ARTWORK_SIDE};
use tairix_image::{DecodeLimits, FitBox, ImageFormat, RasterImage};
use tairix_raster::{resample, resample_rows, Region, Rgba8Image, Surface};
use tairix_svg::{SvgError, SvgImage};
use tairix_wallpaper::{Placement, WallpaperFit};

use crate::host::{Launcher, ParserSandbox, SandboxError};
use crate::proto::MAX_FRAME;
use crate::wire::{Reader, Writer};
use crate::worker::Service;

#[cfg(test)]
#[path = "imagerender_tests.rs"]
mod tests;

// ---------------------------------------------------------------------
// Icon rasterisation
// ---------------------------------------------------------------------

/// Largest pixel side a caller may request for the rasterised output.
///
/// `512 * 512 * 4` bytes is a 1 MiB reply, comfortably under
/// [`crate::proto::MAX_FRAME`]; no desktop icon slot is ever asked to
/// render larger than this.
pub const MAX_ICON_SIDE: u32 = 512;

/// Largest total PNG source pixel count
/// ([`tairix_icon::MAX_ARTWORK_SIDE`] squared).
const PNG_DECODE_MAX_PIXELS: u64 = (MAX_ARTWORK_SIDE as u64) * (MAX_ARTWORK_SIDE as u64);

/// Icon-rasterisation request opcode.
const OP_RASTERISE: u8 = 1;

/// Reply tag shared by every refusal this service returns, whatever the
/// request opcode: an error code byte follows.
const REPLY_ERROR: u8 = 0;
/// Icon-rasterisation success reply tag.
const REPLY_PIXELS: u8 = 1;

/// Icon refusal wire codes.
const REFUSAL_MALFORMED_REQUEST: u8 = 1;
const REFUSAL_UNSUPPORTED_FORMAT: u8 = 2;
const REFUSAL_MALFORMED_IMAGE: u8 = 3;
const REFUSAL_UNRENDERABLE: u8 = 4;

/// Why the service refused an icon-rasterisation request, carried typed
/// over the wire.
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

/// The service the sandboxed worker runs: icon rasterisation
/// (`OP_RASTERISE`) and wallpaper placement (`OP_WALLPAPER_*`). Total by
/// construction — every failure is a typed error reply.
///
/// A wallpaper prepare holds one decoded source and its placement between
/// `OP_WALLPAPER_PREPARE` and `OP_WALLPAPER_RELEASE`; icon rasterisation
/// carries no state and is unaffected by whatever wallpaper sequence is
/// interleaved with it, since a worker is reused across many requests.
#[derive(Debug, Default)]
pub struct ImageRenderService {
    wallpaper: Option<PreparedWallpaper>,
}

impl Service for ImageRenderService {
    fn handle(&mut self, request: &[u8]) -> Vec<u8> {
        match request.first().copied() {
            Some(OP_RASTERISE) => match dispatch_icon(request) {
                Ok(reply) => reply,
                Err(refusal) => encode_error(refusal.to_wire()),
            },
            Some(OP_WALLPAPER_PREPARE | OP_WALLPAPER_BAND | OP_WALLPAPER_RELEASE) => {
                match self.dispatch_wallpaper(request) {
                    Ok(reply) => reply,
                    Err(refusal) => encode_error(refusal.to_wire()),
                }
            }
            _ => encode_error(REFUSAL_MALFORMED_REQUEST),
        }
    }
}

/// Encode a [`REPLY_ERROR`] reply carrying `code`, whichever refusal
/// enum's wire mapping produced it.
fn encode_error(code: u8) -> Vec<u8> {
    let mut w = Writer::new();
    w.u8(REPLY_ERROR);
    w.u8(code);
    w.finish()
}

/// Decode the icon-rasterisation request, rasterise, and encode the reply.
fn dispatch_icon(request: &[u8]) -> Result<Vec<u8>, IconRefusal> {
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
        .bytes(MAX_ARTWORK_BYTES)
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

/// Decode a PNG icon (bounded by [`tairix_icon::MAX_ARTWORK_SIDE`] /
/// [`PNG_DECODE_MAX_PIXELS`], not by the requested `side`) and scale it to
/// `side`×`side`.
fn rasterise_png(side: u32, icon: &[u8]) -> Result<Vec<u8>, IconRefusal> {
    // Icon artwork is always PNG (`plans/ICONS.md`), never progressive
    // JPEG, so the progressive-coefficient-store bound is never consulted
    // here.
    let limits = DecodeLimits::new(MAX_ARTWORK_SIDE, MAX_ARTWORK_SIDE, PNG_DECODE_MAX_PIXELS, 0);
    let image = tairix_image::decode(icon, &limits).map_err(|_| IconRefusal::MalformedImage)?;
    scale_to_square(image.width(), image.height(), image.pixels(), side)
}

/// Scale straight-alpha RGBA8 `src` (`src_w`×`src_h`) into a `side`×`side`
/// straight-alpha RGBA8 buffer of exactly `side * side * 4` bytes.
///
/// The source is fitted inside the square preserving its aspect ratio
/// ([`fit_within`]) and centred, leaving fully transparent padding on the
/// shorter axis; the fitted rectangle itself is produced by the crate's one
/// shared resampler ([`tairix_raster::resample()`]) rather than a private
/// scaling implementation, so a downscale blends (never nearest-neighbour)
/// exactly as the wallpaper path's resampling does.
fn scale_to_square(src_w: u32, src_h: u32, src: &[u8], side: u32) -> Result<Vec<u8>, IconRefusal> {
    let mut out = vec![0u8; pixel_buffer_len(side, side)];
    let (fit_w, fit_h) = fit_within(src_w, src_h, side);
    let x0 = (side - fit_w) / 2;
    let y0 = (side - fit_h) / 2;
    let image = Rgba8Image::new(src_w, src_h, src).map_err(|_| IconRefusal::Unrenderable)?;
    let fitted =
        resample(&image, image.whole(), fit_w, fit_h).map_err(|_| IconRefusal::Unrenderable)?;
    splice_rows(&fitted, fit_w, x0, side, y0, fit_h, &mut out);
    Ok(out)
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

/// Byte offset of pixel `(x, y)` in a row-major RGBA8 buffer `width`
/// pixels wide, or `None` if the coordinate or the resulting offset would
/// not fit a `usize` — unreachable for any geometry this service bounds,
/// but checked rather than assumed. Shared by the icon and wallpaper paths.
fn pixel_offset(x: u32, y: u32, width: u32) -> Option<usize> {
    let row = u64::from(y).checked_mul(u64::from(width))?;
    let index = row.checked_add(u64::from(x))?;
    let byte_offset = index.checked_mul(4)?;
    usize::try_from(byte_offset).ok()
}

/// `width * height * 4`, saturating rather than overflowing. Shared by the
/// icon and wallpaper paths.
///
/// Every caller bounds `width`/`height` well below the point this could
/// matter (icon sides are capped at [`MAX_ICON_SIDE`], wallpaper
/// destinations at [`MAX_WALLPAPER_WIDTH`]/[`MAX_WALLPAPER_HEIGHT`]), so
/// saturation is unreachable in practice and only keeps the arithmetic
/// total.
fn pixel_buffer_len(width: u32, height: u32) -> usize {
    let count = u64::from(width).saturating_mul(u64::from(height));
    let bytes = count.saturating_mul(4);
    usize::try_from(bytes).unwrap_or(usize::MAX)
}

/// Copy `rows` rows of a `src_width`-wide straight-alpha RGBA8 buffer
/// `src` into `out` (a `dest_width`-wide RGBA8 buffer), placing row `r` of
/// `src` at output row `out_row_offset + r`, columns
/// `[x_offset, x_offset + src_width)`.
///
/// Every offset is bounds-checked before a slice is touched, so a
/// geometry this crate itself computed wrongly would silently skip the
/// out-of-range pixels rather than panic — the same fail-total posture
/// every pixel helper in this module keeps.
fn splice_rows(
    src: &[u8],
    src_width: u32,
    x_offset: u32,
    dest_width: u32,
    out_row_offset: u32,
    rows: u32,
    out: &mut [u8],
) {
    let row_bytes = pixel_buffer_len(src_width, 1);
    for row in 0..rows {
        let Some(src_start) = pixel_offset(0, row, src_width) else {
            continue;
        };
        let Some(src_row) = src.get(src_start..src_start + row_bytes) else {
            continue;
        };
        let Some(dst_start) = pixel_offset(x_offset, out_row_offset + row, dest_width) else {
            continue;
        };
        if let Some(slot) = out.get_mut(dst_start..dst_start + row_bytes) {
            slot.copy_from_slice(src_row);
        }
    }
}

/// Ask the sandboxed worker to decode `icon` (SVG or PNG bytes) and
/// rasterise it to a `side`×`side` straight-alpha RGBA8 image.
///
/// `side` and `icon.len()` are checked locally against
/// [`MAX_ICON_SIDE`]/[`tairix_icon::MAX_ARTWORK_BYTES`] before anything is
/// sent, so an out-of-bounds request never round-trips through the sandbox
/// just to be refused. The reply is never trusted as-is: the tag, the echoed
/// side, and the exact pixel length are all validated before the bytes are
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
    if side == 0 || side > MAX_ICON_SIDE || icon.len() > MAX_ARTWORK_BYTES {
        return Err(IconRasterFailure::Refused(IconRefusal::MalformedRequest));
    }
    let mut w = Writer::new();
    w.u8(OP_RASTERISE);
    w.u32(side);
    w.bytes(icon);
    let reply = sandbox
        .request(&w.finish())
        .map_err(IconRasterFailure::Sandbox)?;
    decode_icon_reply(&reply, side)
}

/// Decode and validate the worker's icon reply fail-closed.
fn decode_icon_reply(reply: &[u8], side: u32) -> Result<Vec<u8>, IconRasterFailure> {
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

// ---------------------------------------------------------------------
// Wallpaper placement
// ---------------------------------------------------------------------

/// Largest destination width, in pixels, a wallpaper render may target.
///
/// A fixed security bound, not a growable capacity, paired with
/// [`MAX_WALLPAPER_HEIGHT`]: 3840×2160 (4K) is comfortably above every
/// display TAIRiX's Tier-1 targets drive today, and the resulting 33 MiB
/// straight-alpha buffer is already a large reservation for the 1 GiB
/// machine the operating-conditions floor demands — a screen larger than
/// this is served letterboxed/cropped by the desktop rather than by
/// raising this bound.
pub const MAX_WALLPAPER_WIDTH: u32 = 3840;

/// Largest destination height, in pixels, a wallpaper render may target.
/// See [`MAX_WALLPAPER_WIDTH`].
pub const MAX_WALLPAPER_HEIGHT: u32 = 2160;

/// Largest destination pixel count
/// (`MAX_WALLPAPER_WIDTH * MAX_WALLPAPER_HEIGHT`).
///
/// A fixed security bound: both axes are already bounded individually so
/// that a single destination row can never itself exceed
/// [`crate::proto::MAX_FRAME`] (see the crate's `rows_per_band` band-sizing
/// helper); this product bound is the one [`render_wallpaper`] checks its
/// caller's `width`/`height` against.
pub const MAX_WALLPAPER_PIXELS: u64 = (MAX_WALLPAPER_WIDTH as u64) * (MAX_WALLPAPER_HEIGHT as u64);

/// Largest total decoded source pixel count a wallpaper prepare may hold.
///
/// A fixed security bound, not a growable capacity: four times
/// [`MAX_WALLPAPER_PIXELS`], because a reduced-scale decode offers only
/// halvings of the source. The scale a wallpaper prepare asks for is the
/// smallest whose output still covers the destination, so on each
/// axis it may overshoot by just under a factor of two — the next scale
/// down would have fallen short of the destination — and a decode that
/// genuinely covers the destination can therefore need close to four times
/// the destination's pixel count. Admitting that is the point of the
/// factor: at the 4K destination this service bounds itself to, the shipped
/// 6688×3764 masters are covered only by their full scale, and a ceiling at
/// [`MAX_WALLPAPER_PIXELS`] would serve every one of them visibly soft.
///
/// A source whose covering scale exceeds this is served from the largest
/// scale that fits (`tairix_image::decode_fitted`), trading a little
/// sharpness for memory; one that exceeds it even at the smallest scale the
/// format offers is refused outright.
pub const MAX_WALLPAPER_DECODE_PIXELS: u64 = MAX_WALLPAPER_PIXELS.saturating_mul(4);

/// Largest size, in bytes, a wallpaper decode's progressive JPEG
/// coefficient store may occupy.
///
/// A fixed security bound, not a growable capacity, and deliberately its
/// own budget rather than a multiple of the pixel ceiling: a progressive
/// scan (ITU-T T.81 Annex G) must buffer every coefficient of every
/// component of the frame **at its natural size** before it can produce a
/// single pixel, and asking for a reduced scale shrinks the output without
/// shrinking that store at all. Three bytes per pixel is what a 4:2:0
/// progressive frame needs (1.5 samples per pixel, 2 bytes per
/// coefficient), so this admits the natural-size store of a progressive
/// source as large as the largest output this service will hold. A frame
/// that spends its pixels less thriftily — 4:4:4 chroma needs six bytes per
/// pixel — is admitted up to half that size and refused beyond it: the
/// bound is a memory budget, which is the honest unit for it, rather than a
/// pixel count dressed up as one. See
/// `tairix_image::DecodeLimits::max_progressive_coefficient_bytes`.
pub const MAX_WALLPAPER_PROGRESSIVE_COEFFICIENT_BYTES: u64 =
    MAX_WALLPAPER_DECODE_PIXELS.saturating_mul(3);

/// Per-axis ceiling `DecodeLimits` is given alongside
/// [`MAX_WALLPAPER_DECODE_PIXELS`], which format decoders require because a
/// declared width or height is weighed before the pixel count is even
/// computed.
///
/// A fixed security bound, deliberately far above any real wallpaper: a
/// JPEG frame dimension cannot exceed `0xFFFF` at all (ITU-T T.81 §B.2.2),
/// and no image whose single axis runs to millions of pixels is a
/// wallpaper. The pixel-count bound is the one that binds in practice.
const MAX_WALLPAPER_DECODE_SIDE: u32 = MAX_WALLPAPER_WIDTH.saturating_mul(MAX_WALLPAPER_HEIGHT);

/// Wallpaper request opcodes.
const OP_WALLPAPER_PREPARE: u8 = 2;
const OP_WALLPAPER_BAND: u8 = 3;
const OP_WALLPAPER_RELEASE: u8 = 4;

/// Wallpaper success reply tags.
const REPLY_WALLPAPER_PREPARED: u8 = 2;
const REPLY_WALLPAPER_BAND: u8 = 3;
const REPLY_WALLPAPER_RELEASED: u8 = 4;

/// Wallpaper refusal wire codes.
const REFUSAL_WALLPAPER_MALFORMED_REQUEST: u8 = 1;
const REFUSAL_WALLPAPER_UNSUPPORTED_FORMAT: u8 = 2;
const REFUSAL_WALLPAPER_MALFORMED_IMAGE: u8 = 3;
const REFUSAL_WALLPAPER_NO_PREPARED_SOURCE: u8 = 4;
const REFUSAL_WALLPAPER_BAND_OUT_OF_RANGE: u8 = 5;
const REFUSAL_WALLPAPER_UNRENDERABLE: u8 = 6;

/// Why the service refused a wallpaper request, carried typed over the
/// wire — the wallpaper counterpart of [`IconRefusal`].
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum WallpaperRefusal {
    /// The request payload violated its grammar: an unknown opcode, a
    /// zero or over-[`MAX_WALLPAPER_WIDTH`]/[`MAX_WALLPAPER_HEIGHT`]
    /// destination, an unrecognised fit byte, an over-
    /// [`tairix_wallpaper::MAX_WALLPAPER_BYTES`] source, or trailing bytes.
    MalformedRequest,
    /// The image bytes are not a format this service's decoder recognises.
    UnsupportedFormat,
    /// The bytes are the recognised format but failed to decode, or could
    /// not be decoded within [`MAX_WALLPAPER_DECODE_PIXELS`] even at the
    /// smallest scale the format offers.
    MalformedImage,
    /// An `OP_WALLPAPER_BAND` request arrived with no source held: no
    /// `OP_WALLPAPER_PREPARE` has succeeded yet, or `OP_WALLPAPER_RELEASE`
    /// dropped the one that had.
    NoPreparedSource,
    /// An `OP_WALLPAPER_BAND` request named an empty range, or one
    /// reaching past the prepared destination's height.
    BandOutOfRange,
    /// The prepared source or placement could not be drawn into the
    /// requested band. Unreachable in practice — `OP_WALLPAPER_PREPARE`
    /// only ever holds geometry it has already validated — but the render
    /// path stays total rather than assuming that holds.
    Unrenderable,
}

impl WallpaperRefusal {
    const fn to_wire(self) -> u8 {
        match self {
            Self::MalformedRequest => REFUSAL_WALLPAPER_MALFORMED_REQUEST,
            Self::UnsupportedFormat => REFUSAL_WALLPAPER_UNSUPPORTED_FORMAT,
            Self::MalformedImage => REFUSAL_WALLPAPER_MALFORMED_IMAGE,
            Self::NoPreparedSource => REFUSAL_WALLPAPER_NO_PREPARED_SOURCE,
            Self::BandOutOfRange => REFUSAL_WALLPAPER_BAND_OUT_OF_RANGE,
            Self::Unrenderable => REFUSAL_WALLPAPER_UNRENDERABLE,
        }
    }

    const fn from_wire(raw: u8) -> Option<Self> {
        match raw {
            REFUSAL_WALLPAPER_MALFORMED_REQUEST => Some(Self::MalformedRequest),
            REFUSAL_WALLPAPER_UNSUPPORTED_FORMAT => Some(Self::UnsupportedFormat),
            REFUSAL_WALLPAPER_MALFORMED_IMAGE => Some(Self::MalformedImage),
            REFUSAL_WALLPAPER_NO_PREPARED_SOURCE => Some(Self::NoPreparedSource),
            REFUSAL_WALLPAPER_BAND_OUT_OF_RANGE => Some(Self::BandOutOfRange),
            REFUSAL_WALLPAPER_UNRENDERABLE => Some(Self::Unrenderable),
            _ => None,
        }
    }
}

impl core::fmt::Display for WallpaperRefusal {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::MalformedRequest => f.write_str("malformed wallpaper request"),
            Self::UnsupportedFormat => f.write_str("wallpaper bytes are not a recognised format"),
            Self::MalformedImage => f.write_str("wallpaper image failed to decode"),
            Self::NoPreparedSource => f.write_str("no wallpaper source is prepared"),
            Self::BandOutOfRange => f.write_str("wallpaper band is out of range"),
            Self::Unrenderable => f.write_str("wallpaper could not be drawn into its band"),
        }
    }
}

/// Typed failure [`render_wallpaper`] can report.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum WallpaperRenderFailure {
    /// The sandbox itself failed (crash, launch failure, oversize).
    Sandbox(SandboxError),
    /// The worker refused the request with the carried typed reason.
    Refused(WallpaperRefusal),
    /// The worker's reply violated the reply grammar or lied about its
    /// geometry: it cannot be believed, so the caller gets nothing
    /// (fail closed).
    ReplyMalformed,
}

impl core::fmt::Display for WallpaperRenderFailure {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Sandbox(inner) => write!(f, "parser sandbox failed: {inner:?}"),
            Self::Refused(refusal) => write!(f, "worker refused: {refusal}"),
            Self::ReplyMalformed => f.write_str("worker reply violated the reply grammar"),
        }
    }
}

/// The one decoded wallpaper source and its placement a worker holds
/// between `OP_WALLPAPER_PREPARE` and `OP_WALLPAPER_RELEASE`.
#[derive(Debug)]
struct PreparedWallpaper {
    /// The full destination canvas width, as prepared.
    dest_w: u32,
    /// The full destination canvas height, as prepared.
    dest_h: u32,
    /// The decoded source image, straight alpha.
    image: RasterImage,
    /// Where and how the source is drawn onto the destination.
    placement: Placement,
}

impl ImageRenderService {
    /// Route a wallpaper request to the op it names.
    fn dispatch_wallpaper(&mut self, request: &[u8]) -> Result<Vec<u8>, WallpaperRefusal> {
        let mut r = Reader::new(request);
        let op = r.u8().map_err(|_| WallpaperRefusal::MalformedRequest)?;
        match op {
            OP_WALLPAPER_PREPARE => self.handle_wallpaper_prepare(&mut r),
            OP_WALLPAPER_BAND => self.handle_wallpaper_band(&mut r),
            OP_WALLPAPER_RELEASE => self.handle_wallpaper_release(&mut r),
            _ => Err(WallpaperRefusal::MalformedRequest),
        }
    }

    /// `OP_WALLPAPER_PREPARE`: decode the source, place it, hold both, and
    /// answer with the band size a reply can carry. Replaces any source
    /// (and placement) an earlier prepare left held.
    fn handle_wallpaper_prepare(
        &mut self,
        r: &mut Reader<'_>,
    ) -> Result<Vec<u8>, WallpaperRefusal> {
        let dest_w = r.u32().map_err(|_| WallpaperRefusal::MalformedRequest)?;
        let dest_h = r.u32().map_err(|_| WallpaperRefusal::MalformedRequest)?;
        let fit_byte = r.u8().map_err(|_| WallpaperRefusal::MalformedRequest)?;
        let fit = fit_from_wire(fit_byte).ok_or(WallpaperRefusal::MalformedRequest)?;
        let image_bytes = r
            .bytes(tairix_wallpaper::MAX_WALLPAPER_BYTES)
            .map_err(|_| WallpaperRefusal::MalformedRequest)?;
        if !r.is_exhausted() {
            return Err(WallpaperRefusal::MalformedRequest);
        }
        if dest_w == 0
            || dest_w > MAX_WALLPAPER_WIDTH
            || dest_h == 0
            || dest_h > MAX_WALLPAPER_HEIGHT
        {
            return Err(WallpaperRefusal::MalformedRequest);
        }
        let image = decode_wallpaper_source(image_bytes, dest_w, dest_h)?;
        let placement = tairix_wallpaper::place((image.width(), image.height()), (dest_w, dest_h), fit)
            // Unreachable: `place` only returns `None` for a zero-sided
            // source or screen, and both are already ruled out above.
            .ok_or(WallpaperRefusal::Unrenderable)?;
        self.wallpaper = Some(PreparedWallpaper {
            dest_w,
            dest_h,
            image,
            placement,
        });
        let mut w = Writer::new();
        w.u8(REPLY_WALLPAPER_PREPARED);
        w.u32(rows_per_band(dest_w));
        Ok(w.finish())
    }

    /// `OP_WALLPAPER_BAND`: draw and answer with exactly the requested
    /// destination rows of the held placement.
    fn handle_wallpaper_band(&self, r: &mut Reader<'_>) -> Result<Vec<u8>, WallpaperRefusal> {
        let first_row = r.u32().map_err(|_| WallpaperRefusal::MalformedRequest)?;
        let rows = r.u32().map_err(|_| WallpaperRefusal::MalformedRequest)?;
        if !r.is_exhausted() {
            return Err(WallpaperRefusal::MalformedRequest);
        }
        let prepared = self
            .wallpaper
            .as_ref()
            .ok_or(WallpaperRefusal::NoPreparedSource)?;
        let pixels = render_wallpaper_band(prepared, first_row, rows)?;
        let mut w = Writer::new();
        w.u8(REPLY_WALLPAPER_BAND);
        w.u32(first_row);
        w.u32(rows);
        w.bytes(&pixels);
        Ok(w.finish())
    }

    /// `OP_WALLPAPER_RELEASE`: drop any held source. Always succeeds,
    /// whether or not anything was held.
    fn handle_wallpaper_release(
        &mut self,
        r: &mut Reader<'_>,
    ) -> Result<Vec<u8>, WallpaperRefusal> {
        if !r.is_exhausted() {
            return Err(WallpaperRefusal::MalformedRequest);
        }
        self.wallpaper = None;
        let mut w = Writer::new();
        w.u8(REPLY_WALLPAPER_RELEASED);
        Ok(w.finish())
    }
}

/// The largest number of destination rows one `OP_WALLPAPER_BAND` reply
/// can carry for a `dest_w`-wide destination, respecting
/// [`crate::proto::MAX_FRAME`] — the reason bands exist at all, never a
/// bound to raise.
///
/// `dest_w` is validated to at most [`MAX_WALLPAPER_WIDTH`] before this
/// ever runs, so one row (`dest_w * 4` bytes) always fits comfortably
/// below [`MAX_FRAME`]; the floor of one row keeps this total even so.
fn rows_per_band(dest_w: u32) -> u32 {
    // Tag (1) + echoed `first_row` (4) + echoed `rows` (4) + the pixel
    // field's own length prefix (4): the fixed overhead of a
    // `REPLY_WALLPAPER_BAND` reply besides its pixel payload.
    const BAND_REPLY_HEADER: u64 = 1 + 4 + 4 + 4;
    let row_bytes = u64::from(dest_w) * 4;
    if row_bytes == 0 {
        return 0;
    }
    let budget = (MAX_FRAME as u64).saturating_sub(BAND_REPLY_HEADER);
    u32::try_from((budget / row_bytes).max(1)).unwrap_or(u32::MAX)
}

/// Sniff and decode `bytes` as a wallpaper source no larger than it has to
/// be to cover a `dest_w`×`dest_h` destination, bounded by
/// [`MAX_WALLPAPER_DECODE_PIXELS`].
///
/// The destination extent is what the decode is asked for, so a source with
/// far more detail than the destination could ever show is decoded at a
/// reduced scale rather than in full — for a 6688×3764 master onto a
/// 1920×1080 screen that is a quarter of the pixels, memory the 1 GiB
/// operating-conditions floor keeps rather than spends on detail no one can
/// see. Where even the covering scale exceeds the bounds, the largest scale
/// that fits is decoded instead of refusing the wallpaper.
fn decode_wallpaper_source(
    bytes: &[u8],
    dest_w: u32,
    dest_h: u32,
) -> Result<RasterImage, WallpaperRefusal> {
    if tairix_image::sniff(bytes).is_none() {
        return Err(WallpaperRefusal::UnsupportedFormat);
    }
    let limits = DecodeLimits::new(
        MAX_WALLPAPER_DECODE_SIDE,
        MAX_WALLPAPER_DECODE_SIDE,
        MAX_WALLPAPER_DECODE_PIXELS,
        MAX_WALLPAPER_PROGRESSIVE_COEFFICIENT_BYTES,
    );
    tairix_image::decode_fitted(bytes, &limits, FitBox::new(dest_w, dest_h))
        .map_err(|_| WallpaperRefusal::MalformedImage)
}

/// The wire byte for `fit`, and its inverse.
const fn fit_to_wire(fit: WallpaperFit) -> u8 {
    match fit {
        WallpaperFit::Fill => 0,
        WallpaperFit::Fit => 1,
        WallpaperFit::Stretch => 2,
        WallpaperFit::Centre => 3,
        WallpaperFit::Tile => 4,
    }
}

/// Decode a wire fit byte; `None` for anything outside the closed set.
const fn fit_from_wire(raw: u8) -> Option<WallpaperFit> {
    match raw {
        0 => Some(WallpaperFit::Fill),
        1 => Some(WallpaperFit::Fit),
        2 => Some(WallpaperFit::Stretch),
        3 => Some(WallpaperFit::Centre),
        4 => Some(WallpaperFit::Tile),
        _ => None,
    }
}

/// Draw destination rows `[first_row, first_row + rows)` of `prepared`'s
/// placement as straight-alpha RGBA8, exactly `rows * dest_w * 4` bytes.
///
/// Every row outside the placement's destination rectangle is left fully
/// transparent (the buffer's zeroed initial state), so a letterboxed or
/// under-sized placement never draws anything the desktop's own backdrop
/// should show through instead.
fn render_wallpaper_band(
    prepared: &PreparedWallpaper,
    first_row: u32,
    rows: u32,
) -> Result<Vec<u8>, WallpaperRefusal> {
    let last = first_row
        .checked_add(rows)
        .ok_or(WallpaperRefusal::BandOutOfRange)?;
    if rows == 0 || last > prepared.dest_h {
        return Err(WallpaperRefusal::BandOutOfRange);
    }
    let mut out = vec![0u8; pixel_buffer_len(prepared.dest_w, rows)];

    let dest_rect = prepared.placement.destination();
    let dest_top = u32::try_from(dest_rect.top().max(0)).unwrap_or(0);
    let dest_bottom = dest_top.saturating_add(dest_rect.height);
    let band_start = first_row.max(dest_top);
    let band_end = last.min(dest_bottom);
    if band_end <= band_start {
        // No row of this band lands inside the placement: the whole band
        // stays fully transparent.
        return Ok(out);
    }

    if prepared.placement.tiled() {
        write_tiled_band(
            prepared, dest_rect, band_start, band_end, first_row, &mut out,
        );
    } else {
        write_resampled_band(
            prepared, dest_rect, band_start, band_end, first_row, &mut out,
        )?;
    }
    Ok(out)
}

/// The source pixel `(sx, sy)` a canvas pixel `(x, y)` samples under 1:1
/// tiling of a `src_w`×`src_h` source whose tile origin is
/// `(dest_left, dest_top)`.
///
/// Carried in `i64` so a canvas coordinate before the tile origin (never
/// produced by [`tairix_wallpaper::place`]'s `Tile` arm today, but not
/// assumed) still wraps correctly via `rem_euclid` rather than through an
/// unsigned wraparound that would pick the wrong source pixel.
fn tiled_pixel(
    x: u32,
    y: u32,
    dest_left: i32,
    dest_top: i32,
    src_w: u32,
    src_h: u32,
) -> (u32, u32) {
    let dx = i64::from(x) - i64::from(dest_left);
    let dy = i64::from(y) - i64::from(dest_top);
    let sx = dx.rem_euclid(i64::from(src_w).max(1));
    let sy = dy.rem_euclid(i64::from(src_h).max(1));
    (
        u32::try_from(sx).unwrap_or(0),
        u32::try_from(sy).unwrap_or(0),
    )
}

/// Draw rows `[band_start, band_end)` of a tiled placement into `out` (a
/// band starting at canvas row `canvas_first_row`).
fn write_tiled_band(
    prepared: &PreparedWallpaper,
    dest_rect: Rect,
    band_start: u32,
    band_end: u32,
    canvas_first_row: u32,
    out: &mut [u8],
) {
    let src_w = prepared.image.width();
    let src_h = prepared.image.height();
    let pixels = prepared.image.pixels();
    let x_start = u32::try_from(dest_rect.left().max(0)).unwrap_or(0);
    let x_end = u32::try_from(dest_rect.right().max(0))
        .unwrap_or(0)
        .min(prepared.dest_w);
    for y in band_start..band_end {
        let out_row = y - canvas_first_row;
        for x in x_start..x_end {
            let (sx, sy) = tiled_pixel(x, y, dest_rect.left(), dest_rect.top(), src_w, src_h);
            let Some(src_off) = pixel_offset(sx, sy, src_w) else {
                continue;
            };
            let Some(pixel) = pixels.get(src_off..src_off + 4) else {
                continue;
            };
            let Some(dst_off) = pixel_offset(x, out_row, prepared.dest_w) else {
                continue;
            };
            if let Some(slot) = out.get_mut(dst_off..dst_off + 4) {
                slot.copy_from_slice(pixel);
            }
        }
    }
}

/// Draw rows `[band_start, band_end)` of a resampled (non-tiled) placement
/// into `out` (a band starting at canvas row `canvas_first_row`), through
/// the crate's one shared resampler.
fn write_resampled_band(
    prepared: &PreparedWallpaper,
    dest_rect: Rect,
    band_start: u32,
    band_end: u32,
    canvas_first_row: u32,
    out: &mut [u8],
) -> Result<(), WallpaperRefusal> {
    let image = Rgba8Image::new(
        prepared.image.width(),
        prepared.image.height(),
        prepared.image.pixels(),
    )
    .map_err(|_| WallpaperRefusal::Unrenderable)?;
    let source = prepared.placement.source();
    let region = Region {
        x: u32::try_from(source.left().max(0)).unwrap_or(0),
        y: u32::try_from(source.top().max(0)).unwrap_or(0),
        width: source.width,
        height: source.height,
    };
    let dest_top = u32::try_from(dest_rect.top().max(0)).unwrap_or(0);
    let dest_left = u32::try_from(dest_rect.left().max(0)).unwrap_or(0);
    let local_first = band_start - dest_top;
    let local_rows = band_end - band_start;

    let mut band_buf = vec![0u8; pixel_buffer_len(dest_rect.width, local_rows)];
    resample_rows(
        &image,
        region,
        dest_rect.width,
        dest_rect.height,
        local_first,
        local_rows,
        &mut band_buf,
    )
    .map_err(|_| WallpaperRefusal::Unrenderable)?;

    let out_row_offset = band_start - canvas_first_row;
    splice_rows(
        &band_buf,
        dest_rect.width,
        dest_left,
        prepared.dest_w,
        out_row_offset,
        local_rows,
        out,
    );
    Ok(())
}

/// Ask the sandboxed worker to decode `image`, place it under `fit` onto a
/// `width`×`height` destination, and return the placed straight-alpha
/// RGBA8 pixels: exactly `width * height * 4` bytes, assembled from
/// however many `OP_WALLPAPER_BAND` replies the worker's reply-size answer
/// required.
///
/// `width`, `height`, and `image.len()` are checked locally against
/// [`MAX_WALLPAPER_WIDTH`]/[`MAX_WALLPAPER_HEIGHT`]/
/// [`tairix_wallpaper::MAX_WALLPAPER_BYTES`] before anything is sent, so an
/// out-of-bounds request never round-trips through the sandbox just to be
/// refused. Every reply is validated fail-closed exactly as
/// [`rasterise_icon`]'s is: a compromised worker can lie about a band's
/// geometry, never hand the caller mismatched or wrongly-sized bytes.
///
/// The held source is always released before this returns — on the
/// success path and on every error path alike — so a worker never holds a
/// decoded wallpaper past one call; a failure while releasing is discarded
/// rather than overriding this call's own more specific outcome.
///
/// # Errors
///
/// [`WallpaperRenderFailure`]: the sandbox failed, the worker refused the
/// request (bad shape, an unrecognised format, a decode failure, or an
/// out-of-range band), or a reply could not be believed.
pub fn render_wallpaper<L: Launcher, S: tairix_log::Sink>(
    sandbox: &mut ParserSandbox<L, S>,
    width: u32,
    height: u32,
    fit: WallpaperFit,
    image: &[u8],
) -> Result<Vec<u8>, WallpaperRenderFailure> {
    if width == 0
        || width > MAX_WALLPAPER_WIDTH
        || height == 0
        || height > MAX_WALLPAPER_HEIGHT
        || image.len() > tairix_wallpaper::MAX_WALLPAPER_BYTES
    {
        return Err(WallpaperRenderFailure::Refused(
            WallpaperRefusal::MalformedRequest,
        ));
    }
    let outcome = prepare_and_assemble(sandbox, width, height, fit, image);
    let _ = release_wallpaper(sandbox);
    outcome
}

/// Drive the prepare/band sequence and assemble the whole destination.
fn prepare_and_assemble<L: Launcher, S: tairix_log::Sink>(
    sandbox: &mut ParserSandbox<L, S>,
    width: u32,
    height: u32,
    fit: WallpaperFit,
    image: &[u8],
) -> Result<Vec<u8>, WallpaperRenderFailure> {
    let rows_per_band = prepare_wallpaper(sandbox, width, height, fit, image)?;
    if rows_per_band == 0 {
        return Err(WallpaperRenderFailure::ReplyMalformed);
    }
    let mut out = vec![0u8; pixel_buffer_len(width, height)];
    let mut first_row = 0u32;
    while first_row < height {
        let rows = rows_per_band.min(height - first_row);
        let band = band_wallpaper(sandbox, first_row, rows, width)?;
        let offset = pixel_buffer_len(width, first_row);
        let expected = pixel_buffer_len(width, rows);
        out.get_mut(offset..offset + expected)
            .ok_or(WallpaperRenderFailure::ReplyMalformed)?
            .copy_from_slice(&band);
        first_row += rows;
    }
    Ok(out)
}

/// Send `OP_WALLPAPER_PREPARE` and return the worker's answered band size.
fn prepare_wallpaper<L: Launcher, S: tairix_log::Sink>(
    sandbox: &mut ParserSandbox<L, S>,
    width: u32,
    height: u32,
    fit: WallpaperFit,
    image: &[u8],
) -> Result<u32, WallpaperRenderFailure> {
    let mut w = Writer::new();
    w.u8(OP_WALLPAPER_PREPARE);
    w.u32(width);
    w.u32(height);
    w.u8(fit_to_wire(fit));
    w.bytes(image);
    let reply = sandbox
        .request(&w.finish())
        .map_err(WallpaperRenderFailure::Sandbox)?;
    let mut r = Reader::new(&reply);
    let tag = r.u8().map_err(|_| WallpaperRenderFailure::ReplyMalformed)?;
    match tag {
        REPLY_WALLPAPER_PREPARED => {
            let rows = r
                .u32()
                .map_err(|_| WallpaperRenderFailure::ReplyMalformed)?;
            if !r.is_exhausted() {
                return Err(WallpaperRenderFailure::ReplyMalformed);
            }
            Ok(rows)
        }
        REPLY_ERROR => Err(decode_wallpaper_error(&mut r)),
        _ => Err(WallpaperRenderFailure::ReplyMalformed),
    }
}

/// Send one `OP_WALLPAPER_BAND` request and return its validated pixels
/// (exactly `rows * width * 4` bytes).
fn band_wallpaper<L: Launcher, S: tairix_log::Sink>(
    sandbox: &mut ParserSandbox<L, S>,
    first_row: u32,
    rows: u32,
    width: u32,
) -> Result<Vec<u8>, WallpaperRenderFailure> {
    let mut w = Writer::new();
    w.u8(OP_WALLPAPER_BAND);
    w.u32(first_row);
    w.u32(rows);
    let reply = sandbox
        .request(&w.finish())
        .map_err(WallpaperRenderFailure::Sandbox)?;
    let mut r = Reader::new(&reply);
    let tag = r.u8().map_err(|_| WallpaperRenderFailure::ReplyMalformed)?;
    match tag {
        REPLY_WALLPAPER_BAND => {
            let echoed_first = r
                .u32()
                .map_err(|_| WallpaperRenderFailure::ReplyMalformed)?;
            let echoed_rows = r
                .u32()
                .map_err(|_| WallpaperRenderFailure::ReplyMalformed)?;
            if echoed_first != first_row || echoed_rows != rows {
                return Err(WallpaperRenderFailure::ReplyMalformed);
            }
            let expected_len = pixel_buffer_len(width, rows);
            let pixels = r
                .bytes(expected_len)
                .map_err(|_| WallpaperRenderFailure::ReplyMalformed)?;
            if pixels.len() != expected_len || !r.is_exhausted() {
                return Err(WallpaperRenderFailure::ReplyMalformed);
            }
            Ok(pixels.to_vec())
        }
        REPLY_ERROR => Err(decode_wallpaper_error(&mut r)),
        _ => Err(WallpaperRenderFailure::ReplyMalformed),
    }
}

/// Send `OP_WALLPAPER_RELEASE` and validate its reply fail-closed.
fn release_wallpaper<L: Launcher, S: tairix_log::Sink>(
    sandbox: &mut ParserSandbox<L, S>,
) -> Result<(), WallpaperRenderFailure> {
    let mut w = Writer::new();
    w.u8(OP_WALLPAPER_RELEASE);
    let reply = sandbox
        .request(&w.finish())
        .map_err(WallpaperRenderFailure::Sandbox)?;
    let mut r = Reader::new(&reply);
    let tag = r.u8().map_err(|_| WallpaperRenderFailure::ReplyMalformed)?;
    match tag {
        REPLY_WALLPAPER_RELEASED if r.is_exhausted() => Ok(()),
        REPLY_ERROR => Err(decode_wallpaper_error(&mut r)),
        _ => Err(WallpaperRenderFailure::ReplyMalformed),
    }
}

/// Decode a `REPLY_ERROR` reply's refusal code fail-closed, `r` positioned
/// just after the shared tag byte.
fn decode_wallpaper_error(r: &mut Reader<'_>) -> WallpaperRenderFailure {
    match r.u8().ok().and_then(WallpaperRefusal::from_wire) {
        Some(refusal) if r.is_exhausted() => WallpaperRenderFailure::Refused(refusal),
        _ => WallpaperRenderFailure::ReplyMalformed,
    }
}
