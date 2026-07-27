//! The rasterising font-service dispatcher: the host-testable core that turns
//! one decoded [`FontRequest`](tairix_abi::font_ipc::FontRequest) into a framed
//! `font-v1` reply.
//!
//! [`FontService`] owns the parsed system faces (borrowed byte sources), the
//! native cell geometry derived once from the primary face, and a bounded
//! `(face, glyph, cell height)` cache of already-rasterised 8-bit coverage.
//! [`FontService::handle`] is the whole request pipeline: decode, dispatch,
//! and emit a reply — always producing bytes, an error frame on any failure,
//! so a caller never blocks on a dropped reply (fail closed, `AGENTS.md`
//! §5.4).
//!
//! The service is the *only* process that parses a face or runs the outline
//! rasteriser (`AGENTS.md` §19.5): a client sends a scalar and a cell height
//! and receives the small coverage bitmap it blits, never a font byte.
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
use alloc::collections::{BTreeMap, VecDeque};

use tairix_abi::font_ipc::{
    encode_glyph_error_reply, encode_glyph_reply, encode_metrics_reply, FontMetrics, FontRequest,
    FONT_METRICS_REPLY_LEN,
};
use tairix_abi::Errno;
use tairix_fontface::{CellGeometry, FontFamily, Repertoire, ATLAS_EM_PX};
use tairix_vt::char_width;

/// The largest number of distinct `(face, glyph, cell height)` bitmaps the
/// cache retains before evicting the oldest.
///
/// The desktop draws a small number of sizes over a modest visible glyph
/// repertoire, so this comfortably holds a steady-state working set while
/// capping the entry count: a pathological caller that rasterises at ever more
/// sizes evicts the oldest entries rather than growing without bound (a
/// fail-closed memory bound, not a scalable capacity).
const MAX_ENTRIES: usize = 1024;

/// The four committed faces, in resolution order, scoped exactly as the atlas
/// generator scopes them so the service resolves every scalar to the same face
/// the console atlas would (`AGENTS.md` §2.2): Inconsolata EX (Latin, Greek,
/// Cyrillic, box drawing, …), M PLUS 1 Code (Japanese), `D2Coding` (Korean
/// only), Noto Sans Hebrew (Hebrew).
pub const FACE_REPERTOIRES: [Repertoire; 4] = [
    Repertoire::Full,
    Repertoire::Full,
    Repertoire::Korean,
    Repertoire::Full,
];

/// The cache key: the resolved face index, glyph id, and the cell height the
/// glyph was rasterised at (cell width and baseline are a fixed function of
/// the height, so the height alone keys the geometry).
type Key = (u32, u32, u32);

/// A bounded FIFO map from [`Key`] to a rasterised 8-bit coverage bitmap.
struct GlyphCache {
    entries: BTreeMap<Key, Box<[u8]>>,
    order: VecDeque<Key>,
}

impl GlyphCache {
    const fn new() -> Self {
        Self {
            entries: BTreeMap::new(),
            order: VecDeque::new(),
        }
    }

    fn get(&self, key: &Key) -> Option<&[u8]> {
        self.entries.get(key).map(|b| &b[..])
    }

    /// Insert `coverage` for `key`, evicting the oldest entry first when the
    /// cache is full so its footprint stays bounded.
    fn insert(&mut self, key: Key, coverage: Box<[u8]>) {
        if self.entries.contains_key(&key) {
            return;
        }
        while self.order.len() >= MAX_ENTRIES {
            if let Some(oldest) = self.order.pop_front() {
                self.entries.remove(&oldest);
            } else {
                break;
            }
        }
        self.order.push_back(key);
        self.entries.insert(key, coverage);
    }
}

/// The sandboxed font service's rasterising core.
///
/// It borrows the face byte sources for its lifetime; the `Run` binary reads
/// the `/System/Fonts` faces into owned buffers once at startup and hands them
/// here, and host tests hand it the committed repository faces.
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
    pub fn new(sources: &[(&'a [u8], Repertoire)]) -> Result<Self, Errno> {
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
            cache: GlyphCache::new(),
        })
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
            }) => match self.glyph_reply(scalar, cell_height, reply) {
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
        let key: Key = (
            u32::try_from(face).unwrap_or(u32::MAX),
            u32::from(glyph),
            cell_height,
        );
        let advance = geometry
            .width
            .saturating_mul(u32::from(char_width(scalar)))
            .max(1);

        if let Some(coverage) = self.cache.get(&key) {
            return encode_glyph_reply(reply, bitmap_width, cell_height, advance, coverage);
        }

        let raw = self
            .family
            .rasterise(
                face,
                glyph,
                &geometry,
                self.px_per_em(cell_height),
                bitmap_width,
            )
            .map_err(|_| Errno::NotFound)?;
        // 4-bit engine coverage (`0..=15`) → 8-bit protocol sample; `15 → 255`.
        let coverage: Box<[u8]> = raw
            .iter()
            .map(|&nibble| nibble.saturating_mul(17))
            .collect();
        let len = encode_glyph_reply(reply, bitmap_width, cell_height, advance, &coverage)?;
        self.cache.insert(key, coverage);
        Ok(len)
    }
}

/// Frame a status-word error reply into `reply`, returning its length (`0`
/// only if the buffer cannot hold even the 4-byte status word).
fn error_frame(reply: &mut [u8], err: Errno) -> usize {
    encode_glyph_error_reply(reply, err).unwrap_or(0)
}

#[cfg(test)]
mod tests;
