//! The rasterising font-service dispatcher: the host-testable core that turns
//! one decoded [`FontRequest`] into a framed
//! `font-v1` reply.
//!
//! [`FontService`] owns the parsed system faces (borrowed byte sources), the
//! native cell geometry derived once from the primary face, and a bounded
//! `(face, glyph, cell height, weight)` cache of already-rasterised 8-bit
//! coverage. [`FontService::handle`] is the whole request pipeline: decode,
//! dispatch, and emit a reply — always producing bytes, an error frame on any
//! failure, so a caller never blocks on a dropped reply (fail closed).
//!
//! The service is the *only* process that parses a face or runs the outline
//! rasteriser: a client sends a scalar and a cell height and receives the
//! small coverage bitmap it blits, never a font byte.
//!
//! # The cache a hostile caller cannot grow
//!
//! The cell height is caller-supplied, so the size of what a request makes
//! this service retain is caller-influenced: a client walking the permitted
//! height range would drive an entry-counted cache into hundreds of
//! megabytes. The cache is therefore bounded in **bytes**, by a budget
//! derived from the machine's own RAM, through the shared reclaimable-memory
//! model — the same [`tairix_reclaim::ReclaimCache`] the render-path client
//! on the other side of this endpoint uses, declared once in
//! [`tairix_font::glyph_cache`]. However many distinct sizes a caller asks
//! for, the retained bytes stay under that ceiling and the least recently
//! used rasters are released (and overwritten) to make room.
//!
//! Bounding retention is not input validation and does not replace it: the
//! permitted scalar, cell-height, and weight ranges are checked by the wire
//! decode in [`tairix_abi::font_ipc`] before a request reaches this module,
//! and an out-of-range request is refused rather than rasterised.
//!
//! # Byte-identical rendering
//!
//! The engine produces 4-bit coverage (`0..=15`), exactly as the atlas
//! generator does; each sample is scaled `×17` to the protocol's 8-bit
//! sample (`15 → 255`). The geometry for a requested cell height scales the
//! native cell the same way `lib/font`'s blitter always has (round-to-nearest
//! against the native height), so text drawn through the service is
//! byte-for-byte what the in-process blitter produced before it.

use alloc::boxed::Box;

use tairix_abi::font_ipc::{
    encode_glyph_error_reply, encode_glyph_reply, encode_metrics_reply, FontMetrics, FontRequest,
    FontWeight, FONT_METRICS_REPLY_LEN,
};
use tairix_abi::Errno;
use tairix_font::{glyph_cache_budget, glyph_cache_candidate, CachedGlyph};
use tairix_fontface::{CellGeometry, FontFamily, Repertoire, ATLAS_EM_PX};
use tairix_log::Sink;
use tairix_reclaim::{PressureGauge, ReclaimCache, ReclaimOwner};
use tairix_vt::char_width;

use crate::embolden::{embolden, stroke_subpixels, SUBPIXEL};

/// The four committed faces, in resolution order, scoped exactly as the atlas
/// generator scopes them so the service resolves every scalar to the same face
/// the console atlas would: Inconsolata EX (Latin, Greek,
/// Cyrillic, box drawing, …), M PLUS 1 Code (Japanese), `D2Coding` (Korean
/// only), Noto Sans Hebrew (Hebrew).
pub const FACE_REPERTOIRES: [Repertoire; 4] = [
    Repertoire::Full,
    Repertoire::Full,
    Repertoire::Korean,
    Repertoire::Full,
];

/// The cache key: the resolved face index, glyph id, the cell height the
/// glyph was rasterised at (cell width and baseline are a fixed function of
/// the height, so the height alone keys the geometry), and the weight it was
/// emboldened to — a heavier weight is a different raster of the same outline,
/// so it must not collide with the regular one.
pub type GlyphKey = (u32, u32, u32, u16);

/// The service's rasterised-glyph cache: the shared bounded, classified,
/// pressure-governed cache holding [`CachedGlyph`] coverage under a
/// [`GlyphKey`].
///
/// The generation token is `()` because nothing invalidates a raster while
/// the service lives: the faces are parsed once at startup and never
/// reloaded, so the same glyph at the same height and weight rasterises to
/// the same bytes every time. Entries leave by eviction, by memory pressure,
/// or with the service itself — the owner-teardown invalidation the shared
/// classification declares.
pub type GlyphCache = ReclaimCache<GlyphKey, CachedGlyph, ()>;

/// The audit label the service's cache is named by in reclaim records.
const CACHE_LABEL: &str = "fontd.glyphs";

/// The owner the service's cache charges its bytes to: this service process,
/// named directly, since a userland service has no numeric task id to quote.
const CACHE_OWNER: &str = "fontd";

/// Build the service's glyph cache, budgeted from the machine's total usable
/// physical RAM.
///
/// The `Run` binary reads `total_ram_bytes` from the System Information
/// service and passes the process's own pressure gauge and audit sink, so the
/// cache shrinks on the same bands as every other cache on the machine. A
/// zero reading — no service, a refused or malformed reply — yields a zero
/// budget, which admits nothing: every glyph is then rasterised on demand,
/// correct and merely slower, never a hand-picked ceiling standing in for a
/// figure the machine did not supply.
#[must_use]
pub fn glyph_cache(
    total_ram_bytes: u64,
    pressure: &'static (dyn PressureGauge + 'static),
    sink: &'static (dyn Sink + Sync),
) -> GlyphCache {
    let cache = ReclaimCache::new(
        CACHE_LABEL,
        glyph_cache_candidate(ReclaimOwner::UserlandProcess(CACHE_OWNER)),
        glyph_cache_budget(total_ram_bytes),
        pressure,
        sink,
    );
    // This is the one cache every GUI client's own glyph-atlas cache is
    // ultimately backed by, so it is the most important row the desktop's
    // cache monitor can show; only the freestanding service binary links
    // the reporter (the host build and the dispatcher-only library consumer
    // never do).
    #[cfg(all(freestanding, feature = "program"))]
    if let Some(ledger) = cache.ledger() {
        tairix_rt::cachereport::register(ledger);
    }
    cache
}

/// The sandboxed font service's rasterising core.
///
/// It borrows the face byte sources for its lifetime; the `Run` binary reads
/// the `/System/Fonts` faces into owned buffers once at startup and hands them
/// here, and host tests hand it the committed repository faces.
///
/// The glyph cache is injected rather than built here: sizing it needs the
/// machine's RAM figure, and governing it needs the process's own pressure
/// gauge and audit sink — none of which this host-testable core may reach for
/// itself. [`glyph_cache`] assembles one from those three.
pub struct FontService<'a> {
    family: FontFamily<'a>,
    /// The native cell geometry — the size the atlas is authored at — derived
    /// once from the primary face. Every requested height scales from this.
    native: CellGeometry,
    cache: GlyphCache,
}

impl<'a> FontService<'a> {
    /// Parse `sources` (each `(face bytes, repertoire)`) into the merged
    /// family and derive the native cell geometry.
    ///
    /// # Errors
    ///
    /// [`Errno::BadMagic`] if a face fails to parse or the primary face does
    /// not yield a uniform monospace advance — the service cannot serve
    /// coverage it cannot rasterise, so it fails closed at startup rather than
    /// binding a broken endpoint.
    pub fn new(sources: &[(&'a [u8], Repertoire)], cache: GlyphCache) -> Result<Self, Errno> {
        let family = FontFamily::parse(sources).map_err(|_| Errno::BadMagic)?;
        let advance = family
            .primary()
            .uniform_advance()
            .map_err(|_| Errno::BadMagic)?;
        let native = CellGeometry::derive(family.primary(), advance, ATLAS_EM_PX)
            .map_err(|_| Errno::BadMagic)?;
        Ok(Self {
            family,
            native,
            cache,
        })
    }

    /// Release whatever the live memory-pressure band no longer permits the
    /// glyph cache to hold, returning the bytes given back.
    ///
    /// The serve loop calls this when the kernel wakes it to say the band
    /// moved, so the service gives rasters back as the machine tightens
    /// instead of holding them until something else is starved. A band that
    /// permits what is already held releases nothing.
    pub fn trim_cache(&mut self) -> usize {
        self.cache.enforce_pressure()
    }

    /// The cell geometry for a `cell_height`-pixel cell: the native cell
    /// scaled by `cell_height / native_height`, rounded to the nearest whole
    /// pixel exactly as `lib/font`'s blitter rounds it, never below one pixel
    /// wide.
    fn geometry_for(&self, cell_height: u32) -> CellGeometry {
        let nh = self.native.height;
        let scale = |value: u32| (value.saturating_mul(cell_height) + nh / 2) / nh;
        let width = scale(self.native.width).max(1);
        CellGeometry {
            width,
            height: cell_height,
            baseline: scale(self.native.baseline),
        }
    }

    /// The pixels-per-em to rasterise at so a `cell_height`-tall cell is
    /// proportional to the native atlas cell (the reference size scaled
    /// linearly).
    fn px_per_em(&self, cell_height: u32) -> f64 {
        f64::from(ATLAS_EM_PX) * f64::from(cell_height) / f64::from(self.native.height)
    }

    /// The same rasterised em size in 1/256 px, which is what the weight
    /// stroke is derived from.
    ///
    /// The em is an exact rational of the cell height, so this is computed in
    /// integers: the stroke a weight adds is a fixed-point pixel count and has
    /// no use for a rounded float.
    fn em_subpixels(&self, cell_height: u32) -> u32 {
        let numerator = u64::from(ATLAS_EM_PX) * u64::from(cell_height) * u64::from(SUBPIXEL);
        let em = numerator / u64::from(self.native.height.max(1));
        u32::try_from(em).unwrap_or(u32::MAX)
    }

    /// The monospace cell metrics for a `cell_height`-tall cell.
    #[must_use]
    pub fn metrics(&self, cell_height: u32) -> FontMetrics {
        let geometry = self.geometry_for(cell_height);
        FontMetrics {
            cell_width: geometry.width,
            cell_height: geometry.height,
            baseline: geometry.baseline,
        }
    }

    /// Handle one request frame, writing the reply into `reply` and returning
    /// its length.
    ///
    /// Always produces a reply: a malformed request or a rasterisation failure
    /// becomes a status-word error frame (which both a glyph-expecting and a
    /// metrics-expecting client decode as the carried [`Errno`]), never a
    /// dropped reply. `reply` must be at least
    /// [`tairix_abi::font_ipc::FONT_MAX_GLYPH_REPLY`] bytes; a `0` return means
    /// even the error frame did not fit (structurally impossible for a
    /// correctly sized buffer) and the caller drops the reply, so the client
    /// fails closed on decode.
    pub fn handle(&mut self, request: &[u8], reply: &mut [u8]) -> usize {
        match FontRequest::from_bytes(request) {
            Ok(FontRequest::Glyph {
                scalar,
                cell_height,
                weight,
            }) => match self.glyph_reply(scalar, cell_height, weight, reply) {
                Ok(len) => len,
                Err(err) => error_frame(reply, err),
            },
            Ok(FontRequest::Metrics { cell_height }) => {
                let bytes = encode_metrics_reply(Ok(self.metrics(cell_height)));
                if reply.len() < FONT_METRICS_REPLY_LEN {
                    return 0;
                }
                reply[..FONT_METRICS_REPLY_LEN].copy_from_slice(&bytes);
                FONT_METRICS_REPLY_LEN
            }
            Err(err) => error_frame(reply, err),
        }
    }

    /// Rasterise (or fetch cached) coverage for `scalar` at `cell_height` and
    /// frame it as a successful glyph reply in `reply`.
    fn glyph_reply(
        &mut self,
        scalar: char,
        cell_height: u32,
        weight: FontWeight,
        reply: &mut [u8],
    ) -> Result<usize, Errno> {
        let geometry = self.geometry_for(cell_height);
        // A glyph may cover two cells, so the bitmap is always two cells wide;
        // a one-cell glyph leaves the continuation cell transparent. The
        // client clips a narrow glyph to `advance`.
        let bitmap_width = geometry.width.saturating_mul(2);
        let code = u32::from(scalar);
        let (face, glyph) = self
            .family
            .resolve(code)
            .or_else(|| self.family.resolve(u32::from(char::REPLACEMENT_CHARACTER)))
            .ok_or(Errno::NotFound)?;
        // The family holds a handful of faces, so the index always fits a
        // `u32`; a structurally impossible overflow keys a distinct slot.
        let key: GlyphKey = (
            u32::try_from(face).unwrap_or(u32::MAX),
            u32::from(glyph),
            cell_height,
            weight.to_wire(),
        );
        let advance = geometry
            .width
            .saturating_mul(u32::from(char_width(scalar)))
            .max(1);
        let px_per_em = self.px_per_em(cell_height);
        let stroke = stroke_subpixels(self.em_subpixels(cell_height), weight);

        // The rasteriser reads the family while the cache is borrowed to
        // admit what it produces, so the two fields are split apart here
        // rather than reached through `self` inside the build closure.
        let Self { family, cache, .. } = self;
        let served = cache
            .get_or_build(&(), key, || {
                rasterise(
                    family,
                    face,
                    glyph,
                    &geometry,
                    px_per_em,
                    bitmap_width,
                    stroke,
                )
            })
            .ok_or(Errno::NotFound)?;
        encode_glyph_reply(reply, served.width, served.height, advance, &served.data)
    }
}

/// Rasterise one glyph at `geometry` and thicken it by `stroke`, yielding the
/// 8-bit coverage bitmap a reply carries.
///
/// `None` when the engine cannot rasterise the outline; the caller turns that
/// into a refused request rather than an empty bitmap.
fn rasterise(
    family: &FontFamily<'_>,
    face: usize,
    glyph: u16,
    geometry: &CellGeometry,
    px_per_em: f64,
    bitmap_width: u32,
    stroke: u32,
) -> Option<CachedGlyph> {
    let raw = family
        .rasterise(face, glyph, geometry, px_per_em, bitmap_width)
        .ok()?;
    // 4-bit engine coverage (`0..=15`) → 8-bit protocol sample; `15 → 255`.
    let mut coverage: Box<[u8]> = raw
        .iter()
        .map(|&nibble| nibble.saturating_mul(17))
        .collect();
    embolden(&mut coverage, bitmap_width as usize, stroke);
    Some(CachedGlyph::new(bitmap_width, geometry.height, coverage))
}

/// Frame a status-word error reply into `reply`, returning its length (`0`
/// only if the buffer cannot hold even the 4-byte status word).
fn error_frame(reply: &mut [u8], err: Errno) -> usize {
    encode_glyph_error_reply(reply, err).unwrap_or(0)
}

#[cfg(test)]
mod tests;
