//! The sandboxed image-rendering service: icon rasterisation, desktop
//! wallpaper placement, and document viewing.
//!
//! An application bundle's icon artwork (SVG or PNG bytes shipped inside
//! the bundle by whoever authored it, not by the system), a desktop
//! wallpaper (a photograph the user picked, or a shipped master), and a
//! document a user opened in the viewer are all untrusted input, so
//! decoding and drawing them must not run in the calling process. This
//! module is the parser-sandbox service for all three: the worker side
//! sniffs the format, decodes it, draws it, and replies with validated
//! straight-alpha RGBA8 pixels; the parent side ([`rasterise_icon`],
//! [`render_wallpaper`], [`render_page`]) trusts nothing about a reply
//! beyond its length and echoed geometry before handing the bytes to the
//! compositor. A crashed or misbehaving worker is contained and replaced
//! by the [`crate::host::ParserSandbox`] seam, and either failure mode — a
//! typed refusal or a sandbox failure — simply means the caller falls back
//! to its own built-in glyph, the desktop backdrop colour, or a stated
//! reason drawn in the viewer's window.
//!
//! # Handing over an untrusted file
//!
//! A file arrives through one path, whatever it is for: [`begin_document`]
//! declares its length and [`push_document`] carries it in pieces no larger
//! than [`MAX_DOCUMENT_CHUNK`], with [`send_document`] doing both for a
//! caller that already holds the whole of it. There is deliberately no
//! second way — a request carrying a whole file inline is bounded by what
//! one protocol frame holds, and a source ceiling set anywhere else can
//! quietly exceed that and be refused by the transport rather than served.
//! Streaming also means a caller reading a file need never hold it whole.
//!
//! # Producing icon pixels
//!
//! An **SVG** icon decodes into the desktop's shared vector form
//! (`tairix_svg::decode` then `tairix_icon::VectorIcon::from_svg`) and
//! rasterises directly onto a `side`×`side` surface through the one
//! polygon-fill path every vector asset shares (`VectorIcon::rasterise`);
//! the premultiplied surface is un-premultiplied back to straight alpha for
//! the wire.
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
//! A wallpaper render is told two extents: the **screen** it models and the
//! **destination** it actually writes — the same extent for the desktop's
//! own wallpaper, a smaller one for the chooser's true-scale preview.
//! `OP_WALLPAPER_PREPARE` sniffs and decodes the source image at the
//! smallest scale its format offers that still covers the *screen*
//! (`tairix_image::decode_fitted`, so an 8.3-megapixel master bound for a
//! 1080p screen never becomes 8.3 megapixels of held RGBA), then asks the
//! one shared scaling arithmetic (`tairix_wallpaper::nominal_source_size`)
//! what size that decoded image must be treated as having to draw a
//! screen-sized composition onto the destination instead. Exactly when the
//! destination is the screen (the desktop's own case) this nominal size is
//! the decoded size itself, so nothing more happens; otherwise the decoded
//! pixels are resampled once, through the crate's one shared resampler, down
//! to that nominal size before anything is placed — never upscaled, since a
//! destination larger than the screen it claims to model is refused. The
//! placement onto the destination is then computed from the nominal size
//! through the one shared placement geometry (`tairix_wallpaper::place`),
//! so the held pixels and the placement's source rectangle always share one
//! coordinate space, and both are held until `OP_WALLPAPER_RELEASE`.
//! `OP_WALLPAPER_BAND` then produces destination rows of that placement a
//! band at a time — bounded by [`crate::proto::MAX_FRAME`], never raised to
//! fit a larger reply — either by repeating the (possibly resampled) source
//! at 1:1 ([`tairix_wallpaper::WallpaperFit::Tile`]) or by resampling its
//! placed source rectangle through the same shared resampler the icon path
//! uses. Wherever the destination is not fully covered by the placement (a
//! letterboxed fit, a source smaller than the screen), those pixels are
//! fully transparent, so the desktop's own backdrop colour shows through —
//! this service never draws a backdrop.
//!
//! # Viewing a document
//!
//! A viewer holds a file open and moves about inside it, so the view is a
//! *session* rather than a one-shot render. [`open_view`] validates the
//! uploaded document's structure and answers what it declares — its format,
//! how many entries it holds, whether they are frames to play or pages to
//! choose between, and the picture the container as a whole is.
//! [`select_page`] decodes one entry and the worker holds it;
//! [`render_page`] draws a *rectangle* of that held page onto a destination
//! the caller sizes, band by band; [`close_view`] drops both.
//!
//! Two things follow from that shape, and both are the point of it. A
//! zoomed-in viewer sends the crop it is actually showing, so the work and
//! the reply are bounded by the window rather than by the picture — a
//! hundred-megapixel page costs the same to pan around as a small one. And
//! the decoded page stays in the worker between requests, so panning and
//! zooming re-draw it rather than decoding it again; the walk owns the
//! document for the same reason, since an animation's frames composite onto
//! their predecessors and a walk rebuilt per request would re-composite
//! every frame before the one asked for.
//!
//! The viewer's own rotation and flip are deliberately *not* here. They are
//! a permutation of pixels the caller has already been handed and validated,
//! not a decode, so they belong to whatever holds the picture — turning what
//! is displayed costs a window, where turning what was decoded costs the
//! whole page (`tairix_raster::Surface::reoriented`). What this service
//! *does* apply is the orientation a file itself declares, because that is
//! part of reading the file: `tairix_image` rights a turned photograph as it
//! decodes it.

use alloc::vec;
use alloc::vec::Vec;

use tairix_geometry::Rect;
use tairix_icon::{VectorIcon, MAX_ARTWORK_BYTES, MAX_ARTWORK_SIDE};
use tairix_image::{
    DecodeError, DecodeLimits, FitBox, ImageFormat, RasterImage, Sequence, SequenceKind,
};
use tairix_raster::{resample, resample_rows, Region, Rgba8Image, Surface};
use tairix_svg::{SvgError, SvgImage};
use tairix_util::fallible;
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
    /// A buffer the decode or the rasterise needed could not be allocated,
    /// so the picture is sound but this machine cannot produce it right now.
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
/// (`OP_RASTERISE`), wallpaper placement (`OP_WALLPAPER_*`), and document
/// viewing (`OP_VIEW_*`) over a document uploaded with `OP_DOC_*`. Total by
/// construction — every failure is a typed error reply.
///
/// A wallpaper prepare holds one decoded source and its placement between
/// `OP_WALLPAPER_PREPARE` and `OP_WALLPAPER_RELEASE`, and a view holds one
/// open document and its decoded page between `OP_VIEW_OPEN` and
/// `OP_VIEW_RELEASE`; icon rasterisation carries no state and is unaffected
/// by whatever sequence is interleaved with it, since a worker is reused
/// across many requests. Uploading a fresh document drops both, because
/// neither describes the new one.
#[derive(Default)]
pub struct ImageRenderService {
    /// The untrusted file most recently uploaded, which a wallpaper
    /// prepare decodes and a view open takes ownership of.
    document: Option<Document>,
    wallpaper: Option<PreparedWallpaper>,
    view: Option<ViewSession>,
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
            Some(OP_DOC_BEGIN | OP_DOC_PUSH) => match self.dispatch_document(request) {
                Ok(reply) => reply,
                Err(refusal) => encode_error(refusal.to_wire()),
            },
            Some(OP_VIEW_OPEN | OP_VIEW_PAGE | OP_VIEW_RENDER | OP_VIEW_BAND | OP_VIEW_RELEASE) => {
                match self.dispatch_view(request) {
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
    match tairix_svg::decode(icon, tairix_svg::Viewport::Square) {
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
    let image = tairix_image::decode(icon, &limits).map_err(|err| match err {
        DecodeError::OutOfMemory => IconRefusal::Unrenderable,
        _ => IconRefusal::MalformedImage,
    })?;
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
/// destinations at [`MAX_DESTINATION_WIDTH`]/[`MAX_DESTINATION_HEIGHT`]), so
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

/// Largest width, in pixels, any destination this service draws may have.
///
/// One bound for every consumer, because they all ask the same thing of it:
/// the largest surface the compositor can be handed. A wallpaper models a
/// screen and a viewer's picture area sits inside a window on one, so a
/// second copy of this figure under another name would be the duplication
/// the charter forbids.
///
/// A fixed security bound, not a growable capacity, paired with
/// [`MAX_DESTINATION_HEIGHT`]: 3840×2160 (4K) is comfortably above every
/// display TAIRiX's Tier-1 targets drive today, and the resulting 33 MiB
/// straight-alpha buffer is already a large reservation for the 1 GiB
/// machine the operating-conditions floor demands — a screen larger than
/// this is served letterboxed/cropped by the desktop rather than by
/// raising this bound.
pub const MAX_DESTINATION_WIDTH: u32 = 3840;

/// Largest height, in pixels, any destination this service draws may have.
/// See [`MAX_DESTINATION_WIDTH`].
pub const MAX_DESTINATION_HEIGHT: u32 = 2160;

/// Largest destination pixel count
/// (`MAX_DESTINATION_WIDTH * MAX_DESTINATION_HEIGHT`).
///
/// A fixed security bound, and the figure a decode budget is expressed
/// against: the two axes are what a request is checked against, because
/// bounding them individually is what keeps a single destination row from
/// exceeding [`crate::proto::MAX_FRAME`], which is what lets a band reply
/// carry whole rows at all.
pub const MAX_DESTINATION_PIXELS: u64 =
    (MAX_DESTINATION_WIDTH as u64) * (MAX_DESTINATION_HEIGHT as u64);

/// Largest total decoded source pixel count a wallpaper prepare may hold.
///
/// A fixed security bound, not a growable capacity: four times
/// [`MAX_DESTINATION_PIXELS`], because a reduced-scale decode offers only
/// halvings of the source. The scale a wallpaper prepare asks for is the
/// smallest whose output still covers the destination, so on each
/// axis it may overshoot by just under a factor of two — the next scale
/// down would have fallen short of the destination — and a decode that
/// genuinely covers the destination can therefore need close to four times
/// the destination's pixel count. Admitting that is the point of the
/// factor: a user-picked master just short of double the 4K destination on
/// each axis is covered only by its full scale, and a ceiling at
/// [`MAX_DESTINATION_PIXELS`] would serve it visibly soft.
///
/// A source whose covering scale exceeds this is served from the largest
/// scale that fits (`tairix_image::decode_fitted`), trading a little
/// sharpness for memory; one that exceeds it even at the smallest scale the
/// format offers is refused outright.
pub const MAX_WALLPAPER_DECODE_PIXELS: u64 = MAX_DESTINATION_PIXELS.saturating_mul(4);

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
const MAX_WALLPAPER_DECODE_SIDE: u32 = MAX_DESTINATION_WIDTH.saturating_mul(MAX_DESTINATION_HEIGHT);

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
const REFUSAL_WALLPAPER_NO_SOURCE: u8 = 7;

/// Why the service refused a wallpaper request, carried typed over the
/// wire — the wallpaper counterpart of [`IconRefusal`].
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum WallpaperRefusal {
    /// The request payload violated its grammar: an unknown opcode, a
    /// zero or over-[`MAX_DESTINATION_WIDTH`]/[`MAX_DESTINATION_HEIGHT`]
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
    /// An `OP_WALLPAPER_PREPARE` request arrived with no complete document
    /// uploaded to place.
    NoSource,
    /// An `OP_WALLPAPER_BAND` request named an empty range, or one
    /// reaching past the prepared destination's height.
    BandOutOfRange,
    /// A buffer the decode or the render needed could not be allocated, or
    /// the prepared source or placement could not be drawn into the
    /// requested band — the latter unreachable in practice, since
    /// `OP_WALLPAPER_PREPARE` only ever holds geometry it has already
    /// validated, but the render path stays total rather than assuming so.
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
            Self::NoSource => REFUSAL_WALLPAPER_NO_SOURCE,
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
            REFUSAL_WALLPAPER_NO_SOURCE => Some(Self::NoSource),
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
            Self::NoSource => f.write_str("no wallpaper source has been uploaded"),
        }
    }
}

/// Typed failure [`render_wallpaper`] can report.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum WallpaperRenderFailure {
    /// The sandbox itself failed (crash, launch failure, oversize).
    Sandbox(SandboxError),
    /// Uploading the source failed.
    Document(DocumentFailure),
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
            Self::Document(inner) => write!(f, "source upload failed: {inner}"),
            Self::Refused(refusal) => write!(f, "worker refused: {refusal}"),
            Self::ReplyMalformed => f.write_str("worker reply violated the reply grammar"),
        }
    }
}

/// The one decoded wallpaper source, and the resolved geometry that draws
/// it, a worker holds between `OP_WALLPAPER_PREPARE` and
/// `OP_WALLPAPER_RELEASE`.
///
/// The pixels are held as a plain buffer rather than a [`RasterImage`]:
/// this crate cannot construct one (its constructor is private to
/// `tairix_image`), and a render never needs anything a `RasterImage`
/// offers beyond the width, height, and straight-alpha bytes already kept
/// here.
///
/// [`Self::source`] is already expressed in the held source's own
/// coordinates rather than in the nominal coordinates the placement was
/// computed in, so a band draws the file's own pixels onto the destination
/// through exactly one resample. Resampling twice — once to a nominal size
/// and again into the destination — costs a whole intermediate image and
/// softens the result for nothing, since the second resample can sample the
/// first's input directly.
#[derive(Debug)]
struct PreparedWallpaper {
    /// The full destination canvas width, as prepared.
    dest_w: u32,
    /// The full destination canvas height, as prepared.
    dest_h: u32,
    /// The held source's width.
    image_width: u32,
    /// The held source's height.
    image_height: u32,
    /// The held source's straight-alpha RGBA8 pixels, row-major.
    image_pixels: Vec<u8>,
    /// Where on the destination canvas the source is drawn.
    destination: Rect,
    /// The rectangle of the held source that is drawn there, in the held
    /// source's own coordinates.
    source: Region,
    /// Whether [`Self::source`] repeats at 1:1 across [`Self::destination`]
    /// rather than being scaled onto it once.
    tiled: bool,
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

    /// `OP_WALLPAPER_PREPARE`: read the uploaded source's header, decode it
    /// at the smallest scale the composition can actually show, resolve
    /// where it is drawn, hold both, and answer with the band size a reply
    /// can carry. Replaces any source (and placement) an earlier prepare
    /// left held.
    ///
    /// The source arrives through `OP_DOC_*` rather than inside this
    /// request: a request carrying a whole file is bounded by what one
    /// protocol frame holds, which a source bound set anywhere else can
    /// quietly exceed.
    fn handle_wallpaper_prepare(
        &mut self,
        r: &mut Reader<'_>,
    ) -> Result<Vec<u8>, WallpaperRefusal> {
        let screen_w = r.u32().map_err(|_| WallpaperRefusal::MalformedRequest)?;
        let screen_h = r.u32().map_err(|_| WallpaperRefusal::MalformedRequest)?;
        let dest_w = r.u32().map_err(|_| WallpaperRefusal::MalformedRequest)?;
        let dest_h = r.u32().map_err(|_| WallpaperRefusal::MalformedRequest)?;
        let fit_byte = r.u8().map_err(|_| WallpaperRefusal::MalformedRequest)?;
        let fit = fit_from_wire(fit_byte).ok_or(WallpaperRefusal::MalformedRequest)?;
        if !r.is_exhausted() {
            return Err(WallpaperRefusal::MalformedRequest);
        }
        if dest_w == 0
            || dest_w > MAX_DESTINATION_WIDTH
            || dest_h == 0
            || dest_h > MAX_DESTINATION_HEIGHT
        {
            return Err(WallpaperRefusal::MalformedRequest);
        }
        // The destination can never be asked to show more of the screen
        // than the screen itself holds: a preview never magnifies past the
        // real display, and requiring this here is what keeps the nominal
        // scaling below from ever upscaling the decoded source. The screen
        // itself carries the same fixed bound as any destination, since
        // the desktop's own wallpaper models a screen no larger than that.
        if screen_w == 0
            || screen_w > MAX_DESTINATION_WIDTH
            || screen_h == 0
            || screen_h > MAX_DESTINATION_HEIGHT
            || dest_w > screen_w
            || dest_h > screen_h
        {
            return Err(WallpaperRefusal::MalformedRequest);
        }
        // Taken, not borrowed: the decode below holds pixels, so the
        // file's own bytes are released rather than kept beside them.
        let document = self
            .document
            .take_if(|document| document.is_complete())
            .ok_or(WallpaperRefusal::NoSource)?;
        let image_bytes = document.bytes.as_slice();
        if image_bytes.len() > tairix_wallpaper::MAX_WALLPAPER_BYTES {
            return Err(WallpaperRefusal::MalformedRequest);
        }
        let native = probe_wallpaper_source(image_bytes)?;
        // What the composition can show decides what is decoded: a
        // thumbnail of a 4K master is served from a one-eighth-scale decode
        // rather than from a screen-sized one it would only throw away.
        let request = tairix_wallpaper::decode_request(
            native,
            (screen_w, screen_h),
            (dest_w, dest_h),
            fit,
        )
        // Unreachable: `native` is non-zero (a probe refuses a zero-sided
        // header) and the screen is already ruled out as zero-sided above.
        .ok_or(WallpaperRefusal::Unrenderable)?;
        let image = decode_wallpaper_source(image_bytes, request.0, request.1)?;
        let nominal = tairix_wallpaper::nominal_source_size(native, (screen_w, screen_h), (dest_w, dest_h))
                // Unreachable for the same reasons as the request above.
                .ok_or(WallpaperRefusal::Unrenderable)?;
        let placement = tairix_wallpaper::place(nominal, (dest_w, dest_h), fit)
            // Unreachable: a nominal size is never zero-sided (clamped up
            // to one) and `(dest_w, dest_h)` is already ruled out above.
            .ok_or(WallpaperRefusal::Unrenderable)?;
        let prepared = hold_wallpaper(image, nominal, dest_w, dest_h, &placement)?;
        self.wallpaper = Some(prepared);
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
        self.document = None;
        let mut w = Writer::new();
        w.u8(REPLY_WALLPAPER_RELEASED);
        Ok(w.finish())
    }
}

/// The largest number of destination rows one band reply can carry for a
/// `dest_w`-wide destination, respecting [`crate::proto::MAX_FRAME`] — the
/// reason bands exist at all, never a bound to raise.
///
/// Shared by the wallpaper and view band replies, which is sound because
/// they are the same reply shape: a tag, the echoed row range, and the
/// pixel field's own length prefix.
///
/// `dest_w` is validated to at most [`MAX_DESTINATION_WIDTH`] before this
/// ever runs, so one row (`dest_w * 4` bytes) always fits comfortably
/// below [`MAX_FRAME`]; the floor of one row keeps this total even so.
fn rows_per_band(dest_w: u32) -> u32 {
    // Tag (1) + echoed `first_row` (4) + echoed `rows` (4) + the pixel
    // field's own length prefix (4): the fixed overhead of a band reply
    // besides its pixel payload.
    const BAND_REPLY_HEADER: u64 = 1 + 4 + 4 + 4;
    let row_bytes = u64::from(dest_w) * 4;
    if row_bytes == 0 {
        return 0;
    }
    let budget = (MAX_FRAME as u64).saturating_sub(BAND_REPLY_HEADER);
    u32::try_from((budget / row_bytes).max(1)).unwrap_or(u32::MAX)
}

/// Read `bytes`' header and answer the natural size it declares.
///
/// The declared geometry is the file's own claim and nothing is sized from
/// it here; it is used only to work out what to *ask* a decode for, and the
/// decode then holds that answer to [`MAX_WALLPAPER_DECODE_PIXELS`] as
/// before. A header that is not a supported format, or is malformed, is
/// refused before a pixel buffer exists at all.
fn probe_wallpaper_source(bytes: &[u8]) -> Result<(u32, u32), WallpaperRefusal> {
    match tairix_image::probe(bytes) {
        Ok(info) => Ok((info.width(), info.height())),
        Err(DecodeError::UnknownFormat) => Err(WallpaperRefusal::UnsupportedFormat),
        Err(_) => Err(WallpaperRefusal::MalformedImage),
    }
}

/// Decode `bytes` as a wallpaper source no larger than `request_w`×
/// `request_h`, bounded by [`MAX_WALLPAPER_DECODE_PIXELS`].
///
/// The request is what the composition can actually show (see
/// [`tairix_wallpaper::decode_request`]), so a source with far more detail
/// than that is decoded at a reduced scale rather than in full — for a
/// 3840×2160 master onto a 1920×1080 screen that is a quarter of the
/// pixels, memory the 1 GiB operating-conditions floor keeps rather than
/// spends on detail no one can see, and for a gallery thumbnail it is a
/// sixty-fourth of them. Where even the covering scale exceeds the bounds,
/// the largest scale that fits is decoded instead of refusing the wallpaper.
fn decode_wallpaper_source(
    bytes: &[u8],
    request_w: u32,
    request_h: u32,
) -> Result<RasterImage, WallpaperRefusal> {
    let limits = DecodeLimits::new(
        MAX_WALLPAPER_DECODE_SIDE,
        MAX_WALLPAPER_DECODE_SIDE,
        MAX_WALLPAPER_DECODE_PIXELS,
        MAX_WALLPAPER_PROGRESSIVE_COEFFICIENT_BYTES,
    );
    tairix_image::decode_fitted(bytes, &limits, FitBox::new(request_w, request_h)).map_err(|err| {
        match err {
            DecodeError::OutOfMemory => WallpaperRefusal::Unrenderable,
            _ => WallpaperRefusal::MalformedImage,
        }
    })
}

/// Turn a decoded image and the placement computed for its nominal size
/// into the source a band draws from.
///
/// A decode answers with the whole image at one of the scales its format
/// offers, which is rarely exactly the nominal size the placement speaks in.
/// For every fit but one, that is simply a change of coordinates: the
/// sampled rectangle is scaled into the decoded image's own space and drawn
/// from there, so the file's pixels reach the destination through a single
/// resample.
///
/// [`WallpaperFit::Tile`] is the exception, because it repeats the source at
/// 1:1 rather than scaling it onto the destination: the repeat is only the
/// right size at the nominal scale, so a decode that landed elsewhere is
/// resampled to it once, here, before any band is drawn.
fn hold_wallpaper(
    image: RasterImage,
    nominal: (u32, u32),
    dest_w: u32,
    dest_h: u32,
    placement: &Placement,
) -> Result<PreparedWallpaper, WallpaperRefusal> {
    let decoded = (image.width(), image.height());
    let (image_width, image_height, image_pixels) = if placement.tiled() && decoded != nominal {
        let source = Rgba8Image::new(decoded.0, decoded.1, image.pixels())
            .map_err(|_| WallpaperRefusal::Unrenderable)?;
        let scaled = resample(&source, source.whole(), nominal.0, nominal.1)
            .map_err(|_| WallpaperRefusal::Unrenderable)?;
        (nominal.0, nominal.1, scaled)
    } else {
        (decoded.0, decoded.1, image.into_pixels())
    };
    let source = map_source(placement.source(), nominal, (image_width, image_height));
    Ok(PreparedWallpaper {
        dest_w,
        dest_h,
        image_width,
        image_height,
        image_pixels,
        destination: placement.destination(),
        source,
        tiled: placement.tiled(),
    })
}

/// Express a source rectangle given in `nominal` coordinates in a
/// `held`-sized image's own coordinates.
///
/// The leading edge rounds down and the trailing edge rounds up, so the
/// mapped rectangle covers everything the nominal one did rather than
/// shaving a column off the crop; both are then held inside the image, and
/// the result is never empty. When the two sizes agree — the ordinary case,
/// since the decode was asked for the size the composition wanted — this is
/// the identity.
fn map_source(source: Rect, nominal: (u32, u32), held: (u32, u32)) -> Region {
    let left = u32::try_from(source.left().max(0)).unwrap_or(0);
    let top = u32::try_from(source.top().max(0)).unwrap_or(0);
    let (x, width) = map_span(left, source.width, nominal.0, held.0);
    let (y, height) = map_span(top, source.height, nominal.1, held.1);
    Region {
        x,
        y,
        width,
        height,
    }
}

/// Map one axis of a source span from a `from`-sized image onto a `to`-sized
/// one, as an origin and an extent that lie inside `0..to`.
fn map_span(origin: u32, extent: u32, from: u32, to: u32) -> (u32, u32) {
    let from = u64::from(from).max(1);
    let to64 = u64::from(to);
    let start = u64::from(origin).saturating_mul(to64) / from;
    let end = u64::from(origin)
        .saturating_add(u64::from(extent))
        .saturating_mul(to64)
        .div_ceil(from);
    let start = start.min(to64.saturating_sub(1));
    let end = end.clamp(start.saturating_add(1), to64.max(1));
    (
        u32::try_from(start).unwrap_or(0),
        u32::try_from(end.saturating_sub(start)).unwrap_or(1).max(1),
    )
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
    // Bounded by what a reply frame carries as well as by the destination:
    // a wider band would be drawn in full and only then found to be
    // unsendable.
    if rows == 0 || last > prepared.dest_h || rows > rows_per_band(prepared.dest_w) {
        return Err(WallpaperRefusal::BandOutOfRange);
    }
    let mut out = vec![0u8; pixel_buffer_len(prepared.dest_w, rows)];

    let dest_rect = prepared.destination;
    let dest_top = u32::try_from(dest_rect.top().max(0)).unwrap_or(0);
    let dest_bottom = dest_top.saturating_add(dest_rect.height);
    let band_start = first_row.max(dest_top);
    let band_end = last.min(dest_bottom);
    if band_end <= band_start {
        // No row of this band lands inside the placement: the whole band
        // stays fully transparent.
        return Ok(out);
    }

    if prepared.tiled {
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
    let src_w = prepared.image_width;
    let src_h = prepared.image_height;
    let pixels = prepared.image_pixels.as_slice();
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
        prepared.image_width,
        prepared.image_height,
        prepared.image_pixels.as_slice(),
    )
    .map_err(|_| WallpaperRefusal::Unrenderable)?;
    let region = prepared.source;
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
/// The `screen == (width, height)` case of [`render_wallpaper_for_screen`]:
/// the destination models no screen other than the surface it is itself
/// written onto — the desktop's own wallpaper, never a preview.
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
    render_wallpaper_for_screen(sandbox, (width, height), width, height, fit, image)
}

/// Ask the sandboxed worker to decode `image` and place it under `fit` as
/// if composing the whole `screen` extent, but render only a
/// `width`×`height` **destination** that models that screen at its own
/// scale — a true scale model when `(width, height)` is smaller than
/// `screen` (a preview), or the screen itself when they are equal (see
/// [`render_wallpaper`]). Returns the placed straight-alpha RGBA8 pixels:
/// exactly `width * height * 4` bytes, assembled from however many
/// `OP_WALLPAPER_BAND` replies the worker's reply-size answer required.
///
/// `screen`, `width`, `height`, and `image.len()` are checked locally
/// against [`MAX_DESTINATION_WIDTH`]/[`MAX_DESTINATION_HEIGHT`]/
/// [`tairix_wallpaper::MAX_WALLPAPER_BYTES`] before anything is sent, so an
/// out-of-bounds request never round-trips through the sandbox just to be
/// refused; the destination may never exceed the screen it models (a
/// preview never magnifies past the real display). Every reply is
/// validated fail-closed exactly as [`rasterise_icon`]'s is: a compromised
/// worker can lie about a band's geometry, never hand the caller
/// mismatched or wrongly-sized bytes.
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
pub fn render_wallpaper_for_screen<L: Launcher, S: tairix_log::Sink>(
    sandbox: &mut ParserSandbox<L, S>,
    screen: (u32, u32),
    width: u32,
    height: u32,
    fit: WallpaperFit,
    image: &[u8],
) -> Result<Vec<u8>, WallpaperRenderFailure> {
    let (screen_w, screen_h) = screen;
    if width == 0
        || width > MAX_DESTINATION_WIDTH
        || height == 0
        || height > MAX_DESTINATION_HEIGHT
        || screen_w == 0
        || screen_w > MAX_DESTINATION_WIDTH
        || screen_h == 0
        || screen_h > MAX_DESTINATION_HEIGHT
        || width > screen_w
        || height > screen_h
        || image.len() > tairix_wallpaper::MAX_WALLPAPER_BYTES
    {
        return Err(WallpaperRenderFailure::Refused(
            WallpaperRefusal::MalformedRequest,
        ));
    }
    let outcome = prepare_and_assemble(sandbox, screen, width, height, fit, image);
    let _ = release_wallpaper(sandbox);
    outcome
}

/// Drive the prepare/band sequence and assemble the whole destination.
fn prepare_and_assemble<L: Launcher, S: tairix_log::Sink>(
    sandbox: &mut ParserSandbox<L, S>,
    screen: (u32, u32),
    width: u32,
    height: u32,
    fit: WallpaperFit,
    image: &[u8],
) -> Result<Vec<u8>, WallpaperRenderFailure> {
    let rows_per_band = prepare_wallpaper(sandbox, screen, width, height, fit, image)?;
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
    screen: (u32, u32),
    width: u32,
    height: u32,
    fit: WallpaperFit,
    image: &[u8],
) -> Result<u32, WallpaperRenderFailure> {
    send_document(sandbox, image).map_err(WallpaperRenderFailure::Document)?;
    let mut w = Writer::new();
    w.u8(OP_WALLPAPER_PREPARE);
    w.u32(screen.0);
    w.u32(screen.1);
    w.u32(width);
    w.u32(height);
    w.u8(fit_to_wire(fit));
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

// ---------------------------------------------------------------------
// Document upload
// ---------------------------------------------------------------------

/// Largest document, in bytes, this service will hold for a caller.
///
/// A fixed containment bound, not a growable capacity: it bounds what one
/// worker holds *resident*, which is what an untrusted file costs before a
/// single pixel of it is decoded. Sixty-four mebibytes admits an
/// uncompressed 4K RGBA TIFF page (33 MiB) and a large multi-page scan,
/// and is exactly eight [`MAX_FRAME`] chunks, so the number of pushes a
/// document takes is bounded as well as its size.
///
/// Every decoder in `tairix_image` reads a whole file rather than a
/// stream, so the bytes must be resident to be decoded at all; the ceiling
/// is what keeps that from being unbounded.
pub const MAX_DOCUMENT_BYTES: usize = 64 << 20;

/// Fixed bytes an `OP_DOC_PUSH` request spends besides its chunk: the
/// opcode and the chunk field's own length prefix.
const DOC_PUSH_OVERHEAD: usize = 1 + 4;

/// Largest chunk one `OP_DOC_PUSH` may carry.
///
/// Derived from [`MAX_FRAME`] rather than chosen, because the whole point
/// of chunking is that a document need not fit one frame: a bound picked
/// independently could sit just above what a frame can actually carry, and
/// the request would then be refused by the framing rather than served.
pub const MAX_DOCUMENT_CHUNK: usize = MAX_FRAME - DOC_PUSH_OVERHEAD;

/// Document upload opcodes, shared by every consumer that hands this
/// service an untrusted file.
const OP_DOC_BEGIN: u8 = 5;
const OP_DOC_PUSH: u8 = 6;

/// Document upload success reply tags.
const REPLY_DOC_BEGUN: u8 = 5;
const REPLY_DOC_PUSHED: u8 = 6;

/// Document upload refusal wire codes.
const REFUSAL_DOC_MALFORMED_REQUEST: u8 = 1;
const REFUSAL_DOC_TOO_LARGE: u8 = 2;
const REFUSAL_DOC_NOT_BEGUN: u8 = 3;
const REFUSAL_DOC_OVERRUN: u8 = 4;
const REFUSAL_DOC_OUT_OF_MEMORY: u8 = 5;

/// Why the service refused a document-upload request, carried typed over
/// the wire.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum DocumentRefusal {
    /// The request payload violated its grammar, or declared a zero length.
    MalformedRequest,
    /// The declared length exceeds [`MAX_DOCUMENT_BYTES`].
    TooLarge,
    /// A chunk arrived with no `OP_DOC_BEGIN` before it.
    NotBegun,
    /// A chunk would carry the document past the length it declared.
    Overrun,
    /// The declared length could not be reserved.
    OutOfMemory,
}

impl DocumentRefusal {
    const fn to_wire(self) -> u8 {
        match self {
            Self::MalformedRequest => REFUSAL_DOC_MALFORMED_REQUEST,
            Self::TooLarge => REFUSAL_DOC_TOO_LARGE,
            Self::NotBegun => REFUSAL_DOC_NOT_BEGUN,
            Self::Overrun => REFUSAL_DOC_OVERRUN,
            Self::OutOfMemory => REFUSAL_DOC_OUT_OF_MEMORY,
        }
    }

    const fn from_wire(raw: u8) -> Option<Self> {
        match raw {
            REFUSAL_DOC_MALFORMED_REQUEST => Some(Self::MalformedRequest),
            REFUSAL_DOC_TOO_LARGE => Some(Self::TooLarge),
            REFUSAL_DOC_NOT_BEGUN => Some(Self::NotBegun),
            REFUSAL_DOC_OVERRUN => Some(Self::Overrun),
            REFUSAL_DOC_OUT_OF_MEMORY => Some(Self::OutOfMemory),
            _ => None,
        }
    }
}

impl core::fmt::Display for DocumentRefusal {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::MalformedRequest => f.write_str("malformed document request"),
            Self::TooLarge => f.write_str("document is larger than the service will hold"),
            Self::NotBegun => f.write_str("no document upload has begun"),
            Self::Overrun => f.write_str("chunk runs past the declared document length"),
            Self::OutOfMemory => f.write_str("document could not be reserved"),
        }
    }
}

/// Typed failure a document upload can report.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum DocumentFailure {
    /// The sandbox itself failed (crash, launch failure, oversize).
    Sandbox(SandboxError),
    /// The worker refused the request with the carried typed reason.
    Refused(DocumentRefusal),
    /// The worker's reply violated the reply grammar or disagreed about
    /// how much it had received: it cannot be believed, so the caller gets
    /// nothing (fail closed).
    ReplyMalformed,
}

impl core::fmt::Display for DocumentFailure {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Sandbox(inner) => write!(f, "parser sandbox failed: {inner:?}"),
            Self::Refused(refusal) => write!(f, "worker refused: {refusal}"),
            Self::ReplyMalformed => f.write_str("worker reply violated the reply grammar"),
        }
    }
}

/// The untrusted file a worker holds, and how much of it has arrived.
#[derive(Debug)]
struct Document {
    /// The length `OP_DOC_BEGIN` declared, reserved in full up front so a
    /// large document is not repeatedly grown and copied.
    declared: usize,
    bytes: Vec<u8>,
}

impl Document {
    /// Whether every byte the upload declared has arrived.
    fn is_complete(&self) -> bool {
        self.bytes.len() == self.declared
    }
}

impl ImageRenderService {
    /// Route a document-upload request to the op it names.
    fn dispatch_document(&mut self, request: &[u8]) -> Result<Vec<u8>, DocumentRefusal> {
        let mut r = Reader::new(request);
        let op = r.u8().map_err(|_| DocumentRefusal::MalformedRequest)?;
        match op {
            OP_DOC_BEGIN => self.handle_doc_begin(&mut r),
            OP_DOC_PUSH => self.handle_doc_push(&mut r),
            _ => Err(DocumentRefusal::MalformedRequest),
        }
    }

    /// `OP_DOC_BEGIN`: reserve a document of the declared length and drop
    /// whatever the worker held before.
    ///
    /// A new document invalidates every session over the old one, so the
    /// prepared wallpaper and the open view go with it rather than being
    /// left to answer about a file that is no longer here.
    fn handle_doc_begin(&mut self, r: &mut Reader<'_>) -> Result<Vec<u8>, DocumentRefusal> {
        let declared = r.u64().map_err(|_| DocumentRefusal::MalformedRequest)?;
        if !r.is_exhausted() || declared == 0 {
            return Err(DocumentRefusal::MalformedRequest);
        }
        let declared = usize::try_from(declared).map_err(|_| DocumentRefusal::TooLarge)?;
        if declared > MAX_DOCUMENT_BYTES {
            return Err(DocumentRefusal::TooLarge);
        }
        self.wallpaper = None;
        self.view = None;
        self.document = None;
        let mut bytes = Vec::new();
        if !fallible::reserve(&mut bytes, declared) {
            return Err(DocumentRefusal::OutOfMemory);
        }
        self.document = Some(Document { declared, bytes });
        let mut w = Writer::new();
        w.u8(REPLY_DOC_BEGUN);
        Ok(w.finish())
    }

    /// `OP_DOC_PUSH`: append one chunk, answering how much has arrived.
    fn handle_doc_push(&mut self, r: &mut Reader<'_>) -> Result<Vec<u8>, DocumentRefusal> {
        let chunk = r
            .bytes(MAX_DOCUMENT_CHUNK)
            .map_err(|_| DocumentRefusal::MalformedRequest)?;
        if !r.is_exhausted() {
            return Err(DocumentRefusal::MalformedRequest);
        }
        let document = self.document.as_mut().ok_or(DocumentRefusal::NotBegun)?;
        let total = document
            .bytes
            .len()
            .checked_add(chunk.len())
            .ok_or(DocumentRefusal::Overrun)?;
        if total > document.declared {
            return Err(DocumentRefusal::Overrun);
        }
        // Reserved whole at `OP_DOC_BEGIN`, so this never reallocates; it
        // is spelled fallibly all the same rather than assuming so.
        if !fallible::reserve(&mut document.bytes, chunk.len()) {
            return Err(DocumentRefusal::OutOfMemory);
        }
        document.bytes.extend_from_slice(chunk);
        let mut w = Writer::new();
        w.u8(REPLY_DOC_PUSHED);
        w.u64(document.bytes.len() as u64);
        Ok(w.finish())
    }
}

/// Declare a document of `len` bytes to the worker, dropping whatever it
/// held before.
///
/// # Errors
///
/// [`DocumentFailure`]: the sandbox failed, the worker refused the length,
/// or the reply could not be believed.
pub fn begin_document<L: Launcher, S: tairix_log::Sink>(
    sandbox: &mut ParserSandbox<L, S>,
    len: usize,
) -> Result<(), DocumentFailure> {
    if len == 0 || len > MAX_DOCUMENT_BYTES {
        return Err(DocumentFailure::Refused(if len == 0 {
            DocumentRefusal::MalformedRequest
        } else {
            DocumentRefusal::TooLarge
        }));
    }
    let mut w = Writer::new();
    w.u8(OP_DOC_BEGIN);
    w.u64(len as u64);
    let reply = sandbox
        .request(&w.finish())
        .map_err(DocumentFailure::Sandbox)?;
    let mut r = Reader::new(&reply);
    match r.u8() {
        Ok(REPLY_DOC_BEGUN) if r.is_exhausted() => Ok(()),
        Ok(REPLY_ERROR) => Err(decode_document_error(&mut r)),
        _ => Err(DocumentFailure::ReplyMalformed),
    }
}

/// Push one chunk of the declared document, answering how many bytes the
/// worker now holds.
///
/// `chunk` may be no larger than [`MAX_DOCUMENT_CHUNK`], which is what one
/// protocol frame can carry; a caller reading a file streams it in pieces
/// of at most that size rather than holding the whole of it.
///
/// # Errors
///
/// [`DocumentFailure`]: the sandbox failed, the worker refused the chunk
/// (no upload begun, or it runs past the declared length), or the reply
/// could not be believed.
pub fn push_document<L: Launcher, S: tairix_log::Sink>(
    sandbox: &mut ParserSandbox<L, S>,
    chunk: &[u8],
) -> Result<u64, DocumentFailure> {
    if chunk.len() > MAX_DOCUMENT_CHUNK {
        return Err(DocumentFailure::Refused(DocumentRefusal::MalformedRequest));
    }
    let mut w = Writer::new();
    w.u8(OP_DOC_PUSH);
    w.bytes(chunk);
    let reply = sandbox
        .request(&w.finish())
        .map_err(DocumentFailure::Sandbox)?;
    let mut r = Reader::new(&reply);
    match r.u8() {
        Ok(REPLY_DOC_PUSHED) => {
            let total = r.u64().map_err(|_| DocumentFailure::ReplyMalformed)?;
            if !r.is_exhausted() {
                return Err(DocumentFailure::ReplyMalformed);
            }
            Ok(total)
        }
        Ok(REPLY_ERROR) => Err(decode_document_error(&mut r)),
        _ => Err(DocumentFailure::ReplyMalformed),
    }
}

/// Send a whole document a caller already holds, in as many chunks as the
/// protocol frame requires.
///
/// The worker is left holding exactly `bytes`, and the running total it
/// answers is checked against what was sent, so a worker that quietly drops
/// or duplicates a chunk is caught here rather than by whatever decodes the
/// document next.
///
/// # Errors
///
/// [`DocumentFailure`], as [`begin_document`] and [`push_document`].
pub fn send_document<L: Launcher, S: tairix_log::Sink>(
    sandbox: &mut ParserSandbox<L, S>,
    bytes: &[u8],
) -> Result<(), DocumentFailure> {
    begin_document(sandbox, bytes.len())?;
    let mut sent = 0usize;
    for chunk in bytes.chunks(MAX_DOCUMENT_CHUNK) {
        let held = push_document(sandbox, chunk)?;
        sent += chunk.len();
        if held != sent as u64 {
            return Err(DocumentFailure::ReplyMalformed);
        }
    }
    Ok(())
}

/// Decode a `REPLY_ERROR` reply's refusal code fail-closed, `r` positioned
/// just after the shared tag byte.
fn decode_document_error(r: &mut Reader<'_>) -> DocumentFailure {
    match r.u8().ok().and_then(DocumentRefusal::from_wire) {
        Some(refusal) if r.is_exhausted() => DocumentFailure::Refused(refusal),
        _ => DocumentFailure::ReplyMalformed,
    }
}

// ---------------------------------------------------------------------
// Document viewing
// ---------------------------------------------------------------------

/// Largest page, in pixels, a view may hold decoded.
///
/// A fixed containment bound, not a growable capacity, and deliberately set
/// by what a viewer must be able to *open* rather than by what a particular
/// machine can afford: sixty-four megapixels sits above the top of the
/// current 35 mm camera range, so no photograph a user owns is refused for
/// being a photograph. What a small machine can actually hold is enforced
/// where it belongs — the decode allocates fallibly and answers
/// [`ViewRefusal::Unrenderable`] when the memory is not there — rather than
/// by a ceiling a larger machine would outgrow.
pub const MAX_VIEW_DECODE_PIXELS: u64 = 8192 * 8192;

/// Per-axis ceiling `DecodeLimits` is given alongside
/// [`MAX_VIEW_DECODE_PIXELS`], which format decoders require because a
/// declared width or height is weighed before the pixel count is computed.
///
/// Deliberately far above any single axis a picture has, because a
/// panorama is legitimately long and thin and the pixel-count bound is the
/// one that should decide: capping an axis at a plausible-looking figure
/// would refuse a 100000×600 stitch that costs a fraction of the budget.
const MAX_VIEW_DECODE_SIDE: u32 = 1 << 24;

/// Largest size, in bytes, a view decode's progressive JPEG coefficient
/// store may occupy.
///
/// Three bytes per pixel of the frame at its natural size, for the reason
/// [`MAX_WALLPAPER_PROGRESSIVE_COEFFICIENT_BYTES`] sets out: a progressive
/// scan must buffer every coefficient before it can produce a pixel, and
/// that store does not shrink with the output.
const MAX_VIEW_PROGRESSIVE_COEFFICIENT_BYTES: u64 = MAX_VIEW_DECODE_PIXELS.saturating_mul(3);

/// View opcodes.
const OP_VIEW_OPEN: u8 = 7;
const OP_VIEW_PAGE: u8 = 8;
const OP_VIEW_RENDER: u8 = 9;
const OP_VIEW_BAND: u8 = 10;
const OP_VIEW_RELEASE: u8 = 11;

/// View success reply tags.
const REPLY_VIEW_OPENED: u8 = 7;
const REPLY_VIEW_PAGE: u8 = 8;
const REPLY_VIEW_RENDERED: u8 = 9;
const REPLY_VIEW_BAND: u8 = 10;
const REPLY_VIEW_RELEASED: u8 = 11;

/// View refusal wire codes.
const REFUSAL_VIEW_MALFORMED_REQUEST: u8 = 1;
const REFUSAL_VIEW_NO_DOCUMENT: u8 = 2;
const REFUSAL_VIEW_UNSUPPORTED_FORMAT: u8 = 3;
const REFUSAL_VIEW_MALFORMED_DOCUMENT: u8 = 4;
const REFUSAL_VIEW_NOT_OPEN: u8 = 5;
const REFUSAL_VIEW_NO_SUCH_PAGE: u8 = 6;
const REFUSAL_VIEW_NO_PAGE_DECODED: u8 = 7;
const REFUSAL_VIEW_NO_RENDER: u8 = 8;
const REFUSAL_VIEW_BAND_OUT_OF_RANGE: u8 = 9;
const REFUSAL_VIEW_UNRENDERABLE: u8 = 10;

/// Why the service refused a view request, carried typed over the wire.
///
/// A viewer draws the reason it was given, so these are the vocabulary a
/// user is shown: each says something different about what to do next.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum ViewRefusal {
    /// The request payload violated its grammar, or named geometry outside
    /// the page or over [`MAX_DESTINATION_WIDTH`]/[`MAX_DESTINATION_HEIGHT`].
    MalformedRequest,
    /// `OP_VIEW_OPEN` arrived with no complete document uploaded.
    NoDocument,
    /// The document is not a format this service's decoders recognise, or
    /// the format named is not one it holds.
    UnsupportedFormat,
    /// The bytes are the recognised format but its structure will not read,
    /// or the page asked for will not decode.
    MalformedDocument,
    /// A request that needs an open document arrived before `OP_VIEW_OPEN`
    /// succeeded, or after `OP_VIEW_RELEASE` dropped the one that had.
    NotOpen,
    /// The page index named is past the last entry the container holds.
    NoSuchPage,
    /// A render arrived before any page had been decoded.
    NoPageDecoded,
    /// A band arrived with no render set up, or after a page change dropped
    /// the one that had.
    NoRender,
    /// A band named an empty range, or one reaching past the render's
    /// destination height.
    BandOutOfRange,
    /// A buffer the decode or the render needed could not be allocated, or
    /// the held page could not be drawn into the requested band.
    Unrenderable,
}

impl ViewRefusal {
    const fn to_wire(self) -> u8 {
        match self {
            Self::MalformedRequest => REFUSAL_VIEW_MALFORMED_REQUEST,
            Self::NoDocument => REFUSAL_VIEW_NO_DOCUMENT,
            Self::UnsupportedFormat => REFUSAL_VIEW_UNSUPPORTED_FORMAT,
            Self::MalformedDocument => REFUSAL_VIEW_MALFORMED_DOCUMENT,
            Self::NotOpen => REFUSAL_VIEW_NOT_OPEN,
            Self::NoSuchPage => REFUSAL_VIEW_NO_SUCH_PAGE,
            Self::NoPageDecoded => REFUSAL_VIEW_NO_PAGE_DECODED,
            Self::NoRender => REFUSAL_VIEW_NO_RENDER,
            Self::BandOutOfRange => REFUSAL_VIEW_BAND_OUT_OF_RANGE,
            Self::Unrenderable => REFUSAL_VIEW_UNRENDERABLE,
        }
    }

    const fn from_wire(raw: u8) -> Option<Self> {
        match raw {
            REFUSAL_VIEW_MALFORMED_REQUEST => Some(Self::MalformedRequest),
            REFUSAL_VIEW_NO_DOCUMENT => Some(Self::NoDocument),
            REFUSAL_VIEW_UNSUPPORTED_FORMAT => Some(Self::UnsupportedFormat),
            REFUSAL_VIEW_MALFORMED_DOCUMENT => Some(Self::MalformedDocument),
            REFUSAL_VIEW_NOT_OPEN => Some(Self::NotOpen),
            REFUSAL_VIEW_NO_SUCH_PAGE => Some(Self::NoSuchPage),
            REFUSAL_VIEW_NO_PAGE_DECODED => Some(Self::NoPageDecoded),
            REFUSAL_VIEW_NO_RENDER => Some(Self::NoRender),
            REFUSAL_VIEW_BAND_OUT_OF_RANGE => Some(Self::BandOutOfRange),
            REFUSAL_VIEW_UNRENDERABLE => Some(Self::Unrenderable),
            _ => None,
        }
    }
}

impl core::fmt::Display for ViewRefusal {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::MalformedRequest => f.write_str("malformed view request"),
            Self::NoDocument => f.write_str("no complete document has been uploaded"),
            Self::UnsupportedFormat => f.write_str("document is not a recognised format"),
            Self::MalformedDocument => f.write_str("document failed to decode"),
            Self::NotOpen => f.write_str("no document is open"),
            Self::NoSuchPage => f.write_str("the document holds no such page"),
            Self::NoPageDecoded => f.write_str("no page has been decoded"),
            Self::NoRender => f.write_str("no render is set up"),
            Self::BandOutOfRange => f.write_str("view band is out of range"),
            Self::Unrenderable => f.write_str("page could not be drawn into its band"),
        }
    }
}

/// Typed failure a view request can report.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum ViewFailure {
    /// The sandbox itself failed (crash, launch failure, oversize).
    Sandbox(SandboxError),
    /// Uploading the document failed.
    Document(DocumentFailure),
    /// The worker refused the request with the carried typed reason.
    Refused(ViewRefusal),
    /// The worker's reply violated the reply grammar or lied about its
    /// geometry: it cannot be believed, so the caller gets nothing
    /// (fail closed).
    ReplyMalformed,
}

impl core::fmt::Display for ViewFailure {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Sandbox(inner) => write!(f, "parser sandbox failed: {inner:?}"),
            Self::Document(inner) => write!(f, "document upload failed: {inner}"),
            Self::Refused(refusal) => write!(f, "worker refused: {refusal}"),
            Self::ReplyMalformed => f.write_str("worker reply violated the reply grammar"),
        }
    }
}

/// What a container declares about its entries as a whole, as
/// [`open_view`] answers it.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct ViewDocument {
    /// The format the document's header identifies.
    pub format: ImageFormat,
    /// Whether the entries are frames to play or pages to choose between.
    pub animated: bool,
    /// How many times an animation asks to be played; `None` for ever, and
    /// always `None` for a page container, which is not played at all.
    pub loop_count: Option<u32>,
    /// How many entries the container holds; `1` for a still picture.
    pub count: u32,
    /// The width of the picture the container is: an animation's canvas, a
    /// still picture's own, or a page container's largest page.
    pub width: u32,
    /// The height of the picture the container is; see [`Self::width`].
    pub height: u32,
}

/// One decoded page of the open document, as [`select_page`] answers it.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct ViewPage {
    /// The entry's zero-based position in the container.
    pub index: u32,
    /// The decoded page's own width, which a page container's pages differ
    /// in and an animation's frames do not.
    pub width: u32,
    /// The decoded page's own height; see [`Self::width`].
    pub height: u32,
    /// How long the container asks for this frame to be shown, in
    /// nanoseconds; `0` where it declares nothing.
    pub delay_ns: u64,
}

/// The wire byte for `format`, and its inverse.
///
/// A named format is how a caller reaches one [`tairix_image::sniff`]
/// cannot recognise — a RISC OS sprite area carries no signature — so the
/// mapping runs both ways rather than only outward.
///
/// `None` for a format the decoding crate has grown and this protocol has
/// not: a document this service could decode but could not *name* to its
/// caller is refused rather than labelled as some other format.
const fn format_to_wire(format: ImageFormat) -> Option<u8> {
    match format {
        ImageFormat::Png => Some(1),
        ImageFormat::Jpeg => Some(2),
        ImageFormat::Gif => Some(3),
        ImageFormat::Bmp => Some(4),
        ImageFormat::Ico => Some(5),
        ImageFormat::Sprite => Some(6),
        ImageFormat::Tiff => Some(7),
        ImageFormat::Webp => Some(8),
        _ => None,
    }
}

/// The format `raw` names, `None` for the zero byte that asks the service
/// to read the document's own signature instead.
const fn format_from_wire(raw: u8) -> Option<ImageFormat> {
    match raw {
        1 => Some(ImageFormat::Png),
        2 => Some(ImageFormat::Jpeg),
        3 => Some(ImageFormat::Gif),
        4 => Some(ImageFormat::Bmp),
        5 => Some(ImageFormat::Ico),
        6 => Some(ImageFormat::Sprite),
        7 => Some(ImageFormat::Tiff),
        8 => Some(ImageFormat::Webp),
        _ => None,
    }
}

/// The open document a worker holds between `OP_VIEW_OPEN` and
/// `OP_VIEW_RELEASE`.
///
/// The walk owns the document rather than borrowing it, which is what lets
/// a worker hold both across requests: an animation's frames composite onto
/// a retained canvas, so a walk that had to be rebuilt per request would
/// re-composite every frame before the one asked for.
///
/// The decoded page is held inside the walk, not copied out beside it, so a
/// band draws the page the walk already has rather than a second copy of it.
struct ViewSession {
    sequence: Sequence<Vec<u8>>,
    /// Which part of the decoded page is drawn onto what, as the most
    /// recent `OP_VIEW_RENDER` set it up. Dropped when the page changes,
    /// because a rectangle of the old page describes nothing of the new one.
    render: Option<ViewRender>,
}

/// Where a render reads and how large it draws.
#[derive(Copy, Clone, Debug)]
struct ViewRender {
    /// The rectangle of the decoded page that is drawn.
    source: Region,
    /// The destination extent it is drawn onto.
    dest_w: u32,
    dest_h: u32,
}

impl ImageRenderService {
    /// Route a view request to the op it names.
    fn dispatch_view(&mut self, request: &[u8]) -> Result<Vec<u8>, ViewRefusal> {
        let mut r = Reader::new(request);
        let op = r.u8().map_err(|_| ViewRefusal::MalformedRequest)?;
        match op {
            OP_VIEW_OPEN => self.handle_view_open(&mut r),
            OP_VIEW_PAGE => self.handle_view_page(&mut r),
            OP_VIEW_RENDER => self.handle_view_render(&mut r),
            OP_VIEW_BAND => self.handle_view_band(&mut r),
            OP_VIEW_RELEASE => self.handle_view_release(&mut r),
            _ => Err(ViewRefusal::MalformedRequest),
        }
    }

    /// `OP_VIEW_OPEN`: validate the uploaded document's structure and
    /// answer what it declares, decoding no pixels.
    fn handle_view_open(&mut self, r: &mut Reader<'_>) -> Result<Vec<u8>, ViewRefusal> {
        let named = r.u8().map_err(|_| ViewRefusal::MalformedRequest)?;
        if !r.is_exhausted() {
            return Err(ViewRefusal::MalformedRequest);
        }
        let format = match named {
            0 => None,
            raw => Some(format_from_wire(raw).ok_or(ViewRefusal::MalformedRequest)?),
        };
        // Taken rather than borrowed: the walk owns the document from here,
        // so the bytes are never held twice.
        let document = self
            .document
            .take_if(|document| document.is_complete())
            .ok_or(ViewRefusal::NoDocument)?;
        self.view = None;
        let limits = view_limits();
        let sequence = match format {
            Some(format) => Sequence::open_as(format, document.bytes, &limits),
            None => Sequence::open(document.bytes, &limits),
        }
        .map_err(|err| view_decode_refusal(&err))?;
        // Everything the reply needs is settled before the view is
        // installed, so a refusal here cannot leave a document open that
        // the caller has been told did not open.
        let info = sequence.info();
        let (animated, loop_count) = match info.kind() {
            SequenceKind::Animation { loop_count } => (true, loop_count),
            SequenceKind::Pages => (false, None),
            // A container shape this protocol cannot describe is refused
            // rather than reported as one of the two it can.
            _ => return Err(ViewRefusal::UnsupportedFormat),
        };
        let named = format_to_wire(info.format()).ok_or(ViewRefusal::UnsupportedFormat)?;
        self.view = Some(ViewSession {
            sequence,
            render: None,
        });
        let mut w = Writer::new();
        w.u8(REPLY_VIEW_OPENED);
        w.u8(named);
        w.u8(u8::from(animated));
        w.u8(u8::from(loop_count.is_some()));
        w.u32(loop_count.unwrap_or(0));
        w.u32(info.count());
        w.u32(info.width());
        w.u32(info.height());
        Ok(w.finish())
    }

    /// `OP_VIEW_PAGE`: decode the entry at the named index and hold it.
    fn handle_view_page(&mut self, r: &mut Reader<'_>) -> Result<Vec<u8>, ViewRefusal> {
        let index = r.u32().map_err(|_| ViewRefusal::MalformedRequest)?;
        if !r.is_exhausted() {
            return Err(ViewRefusal::MalformedRequest);
        }
        let view = self.view.as_mut().ok_or(ViewRefusal::NotOpen)?;
        // A rectangle of the page being replaced describes nothing of the
        // one replacing it, so the render goes before the decode rather
        // than being left to be validated against the wrong geometry.
        view.render = None;
        let frame = view
            .sequence
            .page(index)
            .map_err(|err| view_decode_refusal(&err))?
            .ok_or(ViewRefusal::NoSuchPage)?;
        let mut w = Writer::new();
        w.u8(REPLY_VIEW_PAGE);
        w.u32(frame.index());
        w.u32(frame.width());
        w.u32(frame.height());
        w.u64(frame.delay_ns());
        Ok(w.finish())
    }

    /// `OP_VIEW_RENDER`: fix which part of the held page is drawn onto what,
    /// and answer the band size a reply can carry. Nothing is drawn yet.
    fn handle_view_render(&mut self, r: &mut Reader<'_>) -> Result<Vec<u8>, ViewRefusal> {
        let source = Region {
            x: r.u32().map_err(|_| ViewRefusal::MalformedRequest)?,
            y: r.u32().map_err(|_| ViewRefusal::MalformedRequest)?,
            width: r.u32().map_err(|_| ViewRefusal::MalformedRequest)?,
            height: r.u32().map_err(|_| ViewRefusal::MalformedRequest)?,
        };
        let dest_w = r.u32().map_err(|_| ViewRefusal::MalformedRequest)?;
        let dest_h = r.u32().map_err(|_| ViewRefusal::MalformedRequest)?;
        if !r.is_exhausted() {
            return Err(ViewRefusal::MalformedRequest);
        }
        if dest_w == 0
            || dest_w > MAX_DESTINATION_WIDTH
            || dest_h == 0
            || dest_h > MAX_DESTINATION_HEIGHT
        {
            return Err(ViewRefusal::MalformedRequest);
        }
        let view = self.view.as_mut().ok_or(ViewRefusal::NotOpen)?;
        let page = view.sequence.current().ok_or(ViewRefusal::NoPageDecoded)?;
        if !covers(source, page.width(), page.height()) {
            return Err(ViewRefusal::MalformedRequest);
        }
        view.render = Some(ViewRender {
            source,
            dest_w,
            dest_h,
        });
        let mut w = Writer::new();
        w.u8(REPLY_VIEW_RENDERED);
        w.u32(rows_per_band(dest_w));
        Ok(w.finish())
    }

    /// `OP_VIEW_BAND`: draw and answer exactly the requested destination
    /// rows of the render most recently set up.
    fn handle_view_band(&self, r: &mut Reader<'_>) -> Result<Vec<u8>, ViewRefusal> {
        let first_row = r.u32().map_err(|_| ViewRefusal::MalformedRequest)?;
        let rows = r.u32().map_err(|_| ViewRefusal::MalformedRequest)?;
        if !r.is_exhausted() {
            return Err(ViewRefusal::MalformedRequest);
        }
        let view = self.view.as_ref().ok_or(ViewRefusal::NotOpen)?;
        let render = view.render.ok_or(ViewRefusal::NoRender)?;
        let page = view.sequence.current().ok_or(ViewRefusal::NoPageDecoded)?;
        let last = first_row
            .checked_add(rows)
            .ok_or(ViewRefusal::BandOutOfRange)?;
        // Bounded by what a reply frame carries as well as by the
        // destination: a wider band would be drawn in full and only then
        // found to be unsendable.
        if rows == 0 || last > render.dest_h || rows > rows_per_band(render.dest_w) {
            return Err(ViewRefusal::BandOutOfRange);
        }
        let source = Rgba8Image::new(page.width(), page.height(), page.pixels())
            .map_err(|_| ViewRefusal::Unrenderable)?;
        let mut pixels = vec![0u8; pixel_buffer_len(render.dest_w, rows)];
        resample_rows(
            &source,
            render.source,
            render.dest_w,
            render.dest_h,
            first_row,
            rows,
            &mut pixels,
        )
        .map_err(|_| ViewRefusal::Unrenderable)?;
        let mut w = Writer::new();
        w.u8(REPLY_VIEW_BAND);
        w.u32(first_row);
        w.u32(rows);
        w.bytes(&pixels);
        Ok(w.finish())
    }

    /// `OP_VIEW_RELEASE`: drop the open document and everything decoded
    /// from it. Always succeeds, whether or not anything was held.
    fn handle_view_release(&mut self, r: &mut Reader<'_>) -> Result<Vec<u8>, ViewRefusal> {
        if !r.is_exhausted() {
            return Err(ViewRefusal::MalformedRequest);
        }
        self.view = None;
        self.document = None;
        let mut w = Writer::new();
        w.u8(REPLY_VIEW_RELEASED);
        Ok(w.finish())
    }
}

/// Whether `source` lies wholly inside a `width`×`height` page and is not
/// empty.
fn covers(source: Region, width: u32, height: u32) -> bool {
    source.width != 0
        && source.height != 0
        && source
            .x
            .checked_add(source.width)
            .is_some_and(|right| right <= width)
        && source
            .y
            .checked_add(source.height)
            .is_some_and(|bottom| bottom <= height)
}

/// The limits a viewed document's pages are decoded under.
fn view_limits() -> DecodeLimits {
    DecodeLimits::new(
        MAX_VIEW_DECODE_SIDE,
        MAX_VIEW_DECODE_SIDE,
        MAX_VIEW_DECODE_PIXELS,
        MAX_VIEW_PROGRESSIVE_COEFFICIENT_BYTES,
    )
}

/// Which refusal a decoder's error is, told apart so a viewer can say
/// whether it could not read the file at all or could not afford it.
fn view_decode_refusal(err: &DecodeError) -> ViewRefusal {
    match err {
        DecodeError::UnknownFormat => ViewRefusal::UnsupportedFormat,
        DecodeError::OutOfMemory => ViewRefusal::Unrenderable,
        _ => ViewRefusal::MalformedDocument,
    }
}

/// Open the document already uploaded to the worker, answering what it
/// declares about its entries.
///
/// `format` names the format to read the document as, in place of reading
/// its own signature. That is how a caller reaches a format
/// [`tairix_image::sniff`] cannot recognise — a RISC OS sprite area carries
/// no signature at all, so naming it is the only door — and the named
/// format's own parser still validates the bytes, so naming the wrong one
/// is refused rather than misread.
///
/// The document is sent first with [`send_document`], or with
/// [`begin_document`] and [`push_document`] by a caller streaming a file it
/// does not hold whole. Opening takes the bytes from the worker's upload
/// slot, so a further document must be uploaded before another open.
///
/// # Errors
///
/// [`ViewFailure`]: the sandbox failed, the worker refused (no document, an
/// unrecognised format, a structure that will not read), or the reply could
/// not be believed.
pub fn open_view<L: Launcher, S: tairix_log::Sink>(
    sandbox: &mut ParserSandbox<L, S>,
    format: Option<ImageFormat>,
) -> Result<ViewDocument, ViewFailure> {
    let named = match format {
        None => 0,
        Some(format) => {
            format_to_wire(format).ok_or(ViewFailure::Refused(ViewRefusal::UnsupportedFormat))?
        }
    };
    let mut w = Writer::new();
    w.u8(OP_VIEW_OPEN);
    w.u8(named);
    let reply = view_reply(sandbox, w)?;
    let mut r = Reader::new(&reply);
    expect_tag(&mut r, REPLY_VIEW_OPENED)?;
    let named = r.u8().map_err(|_| ViewFailure::ReplyMalformed)?;
    let format = format_from_wire(named).ok_or(ViewFailure::ReplyMalformed)?;
    let animated = read_flag(&mut r)?;
    let counted = read_flag(&mut r)?;
    let declared = r.u32().map_err(|_| ViewFailure::ReplyMalformed)?;
    let count = r.u32().map_err(|_| ViewFailure::ReplyMalformed)?;
    let width = r.u32().map_err(|_| ViewFailure::ReplyMalformed)?;
    let height = r.u32().map_err(|_| ViewFailure::ReplyMalformed)?;
    if !r.is_exhausted() {
        return Err(ViewFailure::ReplyMalformed);
    }
    // A page container is not played, so a loop count beside one is a
    // reply that does not describe any document this service can open.
    if count == 0 || width == 0 || height == 0 || (counted && !animated) {
        return Err(ViewFailure::ReplyMalformed);
    }
    Ok(ViewDocument {
        format,
        animated,
        loop_count: counted.then_some(declared),
        count,
        width,
        height,
    })
}

/// Decode the entry at `index` of the open document and hold it, answering
/// its own geometry and the delay it declares.
///
/// A page container's entries are independent pictures and cost one decode
/// each. An animation's frames composite onto their predecessors, so a
/// later frame costs only the frames between it and the one already held —
/// which is what makes playing one through by index cost each frame once.
///
/// # Errors
///
/// [`ViewFailure`]: the sandbox failed, the worker refused (nothing open,
/// no such page, a page that will not decode), or the reply could not be
/// believed.
pub fn select_page<L: Launcher, S: tairix_log::Sink>(
    sandbox: &mut ParserSandbox<L, S>,
    index: u32,
) -> Result<ViewPage, ViewFailure> {
    let mut w = Writer::new();
    w.u8(OP_VIEW_PAGE);
    w.u32(index);
    let reply = view_reply(sandbox, w)?;
    let mut r = Reader::new(&reply);
    expect_tag(&mut r, REPLY_VIEW_PAGE)?;
    let echoed = r.u32().map_err(|_| ViewFailure::ReplyMalformed)?;
    let width = r.u32().map_err(|_| ViewFailure::ReplyMalformed)?;
    let height = r.u32().map_err(|_| ViewFailure::ReplyMalformed)?;
    let delay_ns = r.u64().map_err(|_| ViewFailure::ReplyMalformed)?;
    if !r.is_exhausted() || echoed != index || width == 0 || height == 0 {
        return Err(ViewFailure::ReplyMalformed);
    }
    Ok(ViewPage {
        index,
        width,
        height,
        delay_ns,
    })
}

/// Draw `source` of the held page onto a `dest_width`×`dest_height`
/// destination, writing the straight-alpha RGBA8 pixels into `out`.
///
/// `out` must be exactly `dest_width * dest_height * 4` bytes: the caller
/// supplies the buffer so a viewer panning or zooming re-renders into the
/// picture it already holds rather than allocating a destination per frame.
///
/// `source` is a rectangle of the page most recently selected, so a viewer
/// zoomed in sends the crop it is showing rather than the whole page scaled
/// down — the destination bounds the work, never the page.
///
/// Every band is validated fail-closed exactly as the wallpaper path's is:
/// a compromised worker can lie about a band's geometry, never hand the
/// caller mismatched or wrongly-sized bytes.
///
/// # Errors
///
/// [`ViewFailure`]: the sandbox failed, the worker refused (nothing open,
/// no page decoded, geometry outside the page or over the destination
/// bounds), or a reply could not be believed.
pub fn render_page<L: Launcher, S: tairix_log::Sink>(
    sandbox: &mut ParserSandbox<L, S>,
    source: Region,
    dest_width: u32,
    dest_height: u32,
    out: &mut [u8],
) -> Result<(), ViewFailure> {
    if dest_width == 0
        || dest_width > MAX_DESTINATION_WIDTH
        || dest_height == 0
        || dest_height > MAX_DESTINATION_HEIGHT
        || source.width == 0
        || source.height == 0
        || out.len() != pixel_buffer_len(dest_width, dest_height)
    {
        return Err(ViewFailure::Refused(ViewRefusal::MalformedRequest));
    }
    let mut w = Writer::new();
    w.u8(OP_VIEW_RENDER);
    w.u32(source.x);
    w.u32(source.y);
    w.u32(source.width);
    w.u32(source.height);
    w.u32(dest_width);
    w.u32(dest_height);
    let reply = view_reply(sandbox, w)?;
    let mut r = Reader::new(&reply);
    expect_tag(&mut r, REPLY_VIEW_RENDERED)?;
    let rows_per_band = r.u32().map_err(|_| ViewFailure::ReplyMalformed)?;
    if !r.is_exhausted() || rows_per_band == 0 {
        return Err(ViewFailure::ReplyMalformed);
    }
    let mut first_row = 0u32;
    while first_row < dest_height {
        let rows = rows_per_band.min(dest_height - first_row);
        let band = view_band(sandbox, first_row, rows, dest_width)?;
        let offset = pixel_buffer_len(dest_width, first_row);
        let expected = pixel_buffer_len(dest_width, rows);
        out.get_mut(offset..offset + expected)
            .ok_or(ViewFailure::ReplyMalformed)?
            .copy_from_slice(&band);
        first_row += rows;
    }
    Ok(())
}

/// Drop the open document and everything decoded from it.
///
/// # Errors
///
/// [`ViewFailure`]: the sandbox failed or the reply could not be believed.
pub fn close_view<L: Launcher, S: tairix_log::Sink>(
    sandbox: &mut ParserSandbox<L, S>,
) -> Result<(), ViewFailure> {
    let mut w = Writer::new();
    w.u8(OP_VIEW_RELEASE);
    let reply = view_reply(sandbox, w)?;
    let mut r = Reader::new(&reply);
    expect_tag(&mut r, REPLY_VIEW_RELEASED)?;
    if !r.is_exhausted() {
        return Err(ViewFailure::ReplyMalformed);
    }
    Ok(())
}

/// Read a reply's boolean field, refusing any byte that is not one.
fn read_flag(r: &mut Reader<'_>) -> Result<bool, ViewFailure> {
    match r.u8() {
        Ok(0) => Ok(false),
        Ok(1) => Ok(true),
        _ => Err(ViewFailure::ReplyMalformed),
    }
}

/// Send one `OP_VIEW_BAND` request and return its validated pixels
/// (exactly `rows * width * 4` bytes).
fn view_band<L: Launcher, S: tairix_log::Sink>(
    sandbox: &mut ParserSandbox<L, S>,
    first_row: u32,
    rows: u32,
    width: u32,
) -> Result<Vec<u8>, ViewFailure> {
    let mut w = Writer::new();
    w.u8(OP_VIEW_BAND);
    w.u32(first_row);
    w.u32(rows);
    let reply = view_reply(sandbox, w)?;
    let mut r = Reader::new(&reply);
    expect_tag(&mut r, REPLY_VIEW_BAND)?;
    let echoed_first = r.u32().map_err(|_| ViewFailure::ReplyMalformed)?;
    let echoed_rows = r.u32().map_err(|_| ViewFailure::ReplyMalformed)?;
    if echoed_first != first_row || echoed_rows != rows {
        return Err(ViewFailure::ReplyMalformed);
    }
    let expected = pixel_buffer_len(width, rows);
    let pixels = r.bytes(expected).map_err(|_| ViewFailure::ReplyMalformed)?;
    if pixels.len() != expected || !r.is_exhausted() {
        return Err(ViewFailure::ReplyMalformed);
    }
    Ok(pixels.to_vec())
}

/// Send `request` and hand back the reply bytes, having turned a
/// `REPLY_ERROR` into its typed refusal so no caller has to.
fn view_reply<L: Launcher, S: tairix_log::Sink>(
    sandbox: &mut ParserSandbox<L, S>,
    request: Writer,
) -> Result<Vec<u8>, ViewFailure> {
    let reply = sandbox
        .request(&request.finish())
        .map_err(ViewFailure::Sandbox)?;
    let mut probe = Reader::new(&reply);
    if probe.u8() == Ok(REPLY_ERROR) {
        return Err(match probe.u8().ok().and_then(ViewRefusal::from_wire) {
            Some(refusal) if probe.is_exhausted() => ViewFailure::Refused(refusal),
            _ => ViewFailure::ReplyMalformed,
        });
    }
    Ok(reply)
}

/// Read the tag byte a reply must open with, refusing any other.
fn expect_tag(r: &mut Reader<'_>, tag: u8) -> Result<(), ViewFailure> {
    match r.u8() {
        Ok(found) if found == tag => Ok(()),
        _ => Err(ViewFailure::ReplyMalformed),
    }
}
