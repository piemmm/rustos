//! Resolving an icon to drawable pixels: the shared two-tier artwork layer.
//!
//! [`IconKind`] and [`builtin_icon`](crate::builtin_icon) give the desktop a
//! *total* set of scalable vector glyphs — every kind always draws something.
//! On top of that floor this module adds the preferred tier: pre-rasterised
//! raster **artwork**, shipped under `/System/Graphics/Icons` as one
//! `<asset-id>.png` per kind. A draw site asks for the artwork at a pixel
//! side; when the system ships it (and it reads, decodes, and validates) the
//! richer picture is drawn, and when it does not the caller falls back to the
//! built-in glyph. Resolution is therefore total either way.
//!
//! Both the desktop session and the file manager are separate processes that
//! need this exact behaviour, so it lives here rather than in either of them.
//! Like [`IconAssetSource`](crate::IconAssetSource), the crate stays `no_std`
//! and owns no path to the filesystem or to a decoder: the bytes come through
//! an injected [`ArtworkReader`] (a capability-gated read) and the pixels
//! through an injected [`ArtworkRasteriser`] (the parser sandbox in
//! production), so the untrusted decode never runs in this library or in the
//! renderer that consumes it.
//!
//! [`ArtworkCache`] retains each decode — success *or* refusal — keyed by the
//! asset path and the requested side, over the one shared reclaimable-memory
//! cache (`lib/reclaim`) so a crowded or crafted bundle store can never grow a
//! session without bound. [`IconArtworkSource`] binds a cache to its two
//! seams so a renderer can be handed a plain [`IconArtwork`] lookup that
//! borrows the pixels without knowing anything about I/O, and [`NoArtwork`] is
//! the all-glyph lookup a headless build or a test uses.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use tairix_log::Sink;
use tairix_raster::Surface;
use tairix_reclaim::{disposable_ui_cache, CacheLedger, CachedBytes, PressureGauge, ReclaimCache};

use crate::glyph::IconKind;

/// Where the OS ships its desktop graphics assets.
pub const GRAPHICS_DIR: &str = "/System/Graphics";

/// The icon subdirectory of [`GRAPHICS_DIR`].
pub const ICONS_DIR: &str = "/System/Graphics/Icons";

/// Largest icon artwork file the desktop will ever read, in bytes.
///
/// A fixed validation bound on untrusted input, not a growable capacity: real
/// desktop icon artwork is a tiny fraction of this, so the ceiling exists
/// purely to bound how much hostile work a single asset can demand before any
/// byte is decoded. This is the one definition of that bound — the sandboxed
/// rasteriser refuses over-long input against the same value.
pub const MAX_ARTWORK_BYTES: usize = 256 * 1024;

/// The on-disk path of the vector asset for `kind` (`<ICONS_DIR>/<id>.svg`).
#[must_use]
pub fn icon_vector_path(kind: IconKind) -> String {
    format!("{ICONS_DIR}/{}.svg", kind.asset_id())
}

/// The on-disk path of the raster artwork for `kind` (`<ICONS_DIR>/<id>.png`).
#[must_use]
pub fn icon_artwork_path(kind: IconKind) -> String {
    format!("{ICONS_DIR}/{}.png", kind.asset_id())
}

/// Whether `name` is a legal shipped-artwork file name: a known
/// [`IconKind::asset_id`] followed by `.png`, and nothing else.
///
/// Used by the image build to refuse an asset the desktop could never
/// resolve. The identity check is exact — a name that decodes to a kind whose
/// own `asset_id` does not spell that same stem (an unknown id, a wrong
/// extension, an empty name, or a path with directory separators such as
/// `../../etc/x.png`) is rejected, so this never accepts a name the loader
/// would not later map back to the same kind.
#[must_use]
pub fn artwork_kind_for_file(name: &str) -> Option<IconKind> {
    let stem = name.strip_suffix(".png")?;
    let kind = IconKind::for_asset(stem);
    (kind.asset_id() == stem).then_some(kind)
}

/// Reads an asset's bytes.
///
/// The production reader is a capability-gated filesystem read; tests supply a
/// fake. `None` for any unreadable path — a missing or refused asset is never
/// fatal, the caller falls back to the glyph.
pub trait ArtworkReader {
    /// The bytes at `path`, or `None` when the path is missing or unreadable.
    fn read(&mut self, path: &str) -> Option<Vec<u8>>;
}

/// Turns encoded icon bytes into `side`×`side` straight-alpha RGBA8.
///
/// The desktop backs this with the parser sandbox so the untrusted decode
/// never runs in the calling process; tests supply a fake. The rasteriser is
/// trusted only to the extent the caller verifies: [`ArtworkCache`] re-checks
/// the returned pixel length before building a surface from it.
pub trait ArtworkRasteriser {
    /// Rasterise `bytes` to a `side`-pixel square of straight-alpha RGBA8, or
    /// refuse with `None`.
    fn rasterise(&mut self, side: u32, bytes: &[u8]) -> Option<Vec<u8>>;
}

/// A draw-site lookup: the pre-rasterised artwork for `kind` at `side`, or
/// `None` to draw the built-in glyph instead.
///
/// The borrow keeps the pixels in the cache the lookup owns, so a grid that
/// draws many icons a frame reads each surface in place rather than copying
/// it.
pub trait IconArtwork {
    /// The artwork for `kind` at `side` pixels, or `None` to fall back to the
    /// built-in glyph.
    fn artwork(&mut self, kind: IconKind, side: u32) -> Option<&Surface>;
}

/// The all-glyph lookup: never any artwork.
///
/// Used by a headless build with no shipped raster assets and by tests: every
/// query returns `None`, so every draw site falls back to its built-in glyph.
#[derive(Copy, Clone, Debug, Default)]
pub struct NoArtwork;

impl IconArtwork for NoArtwork {
    fn artwork(&mut self, _kind: IconKind, _side: u32) -> Option<&Surface> {
        None
    }
}

/// One decode outcome, retained: the rasterised artwork, or the refusal that
/// decoding it produced.
///
/// A refusal is worth remembering — it stops a malformed or absent asset being
/// re-read on every frame — and it is charged its bookkeeping like any other
/// entry, so a store full of broken artwork cannot grow the cache past its
/// budget.
///
/// Public only because it names the value type of the cache
/// [`artwork_cache`] builds and [`ArtworkCache::new`] takes; the outcome
/// itself is reached through [`ArtworkCache::path_artwork`].
pub struct CachedArtwork(Option<Surface>);

impl CachedArtwork {
    /// The retained surface, if this decode produced one.
    fn surface(&self) -> Option<&Surface> {
        self.0.as_ref()
    }
}

impl CachedBytes for CachedArtwork {
    fn payload_bytes(&self) -> usize {
        self.0.as_ref().map_or(0, CachedBytes::payload_bytes)
    }

    fn wipe(&mut self) {
        if let Some(surface) = self.0.as_mut() {
            surface.wipe();
        }
    }
}

/// Bytes of bookkeeping each retained decode costs beyond its pixels: the
/// asset path the entry is keyed by (bounded by the shared path cap), the
/// pixel side, the recency index node, and the map nodes holding them.
///
/// Both consumers build their cache with this one value so a change to the
/// budget's per-entry overhead cannot diverge between them.
pub const ARTWORK_ENTRY_METADATA_BYTES: usize = 256;

/// Build the reclaimable cache an [`ArtworkCache`] wraps, classified and
/// budgeted by the one shared desktop policy.
///
/// `label` names the cache in audit records, `seat` is the owning seat,
/// `fb_bytes` is the output's frame size (so the artwork a consumer may retain
/// scales with the display it draws on rather than a fixed ceiling), and
/// `pressure` / `sink` are the process's live memory-pressure gauge and audit
/// sink. Both the desktop session and the file manager call this so they build
/// the cache identically rather than each inventing budget and metadata
/// numbers.
#[must_use]
pub fn artwork_cache(
    label: &'static str,
    seat: u64,
    fb_bytes: usize,
    pressure: &'static (dyn PressureGauge + 'static),
    sink: &'static (dyn Sink + Sync),
) -> ArtworkCache {
    ArtworkCache::new(disposable_ui_cache(
        label,
        seat,
        fb_bytes,
        ARTWORK_ENTRY_METADATA_BYTES,
        pressure,
        sink,
    ))
}

/// The reclaim-governed decode cache, keyed by asset path and pixel side.
///
/// One decode outcome per `(path, side)`: a scale change alters the side and
/// so misses, re-rasterising at the new geometry. Nothing invalidates the
/// whole cache at once — shipped artwork changes at install time, not while an
/// icon is on screen — so the generation is the unit value; what bounds this
/// cache is its budget, and what ends it is the owner's teardown.
///
/// The keys are asset paths, some of them bundle-supplied, so an unbounded map
/// here would let a crafted or merely crowded store grow a session without
/// limit; the budget forecloses that.
pub struct ArtworkCache {
    entries: ReclaimCache<(String, u32), CachedArtwork, ()>,
}

impl ArtworkCache {
    /// An empty cache over the caller's ready-built reclaimable cache.
    ///
    /// The cache is injected rather than defaulted because only the owning
    /// process knows the output's size, the seat, the live pressure gauge, and
    /// the audit sink; a cache built without them would retain nothing while
    /// looking like it worked. [`artwork_cache`] assembles one.
    #[must_use]
    pub const fn new(entries: ReclaimCache<(String, u32), CachedArtwork, ()>) -> Self {
        Self { entries }
    }

    /// Release every retained decode, overwriting the artwork first.
    ///
    /// Called when the owner is going away, so one user's decoded artwork
    /// never outlives their session in reusable heap.
    pub fn teardown(&mut self) {
        self.entries.teardown();
    }

    /// Apply the current memory-pressure band's forced shrink, returning the
    /// bytes released.
    pub fn trim(&mut self) -> usize {
        self.entries.enforce_pressure()
    }

    /// Bytes currently charged for retained artwork, payload plus this cache's
    /// own per-entry bookkeeping.
    #[must_use]
    pub fn charged_bytes(&self) -> usize {
        self.entries.charged_bytes()
    }

    /// A shared handle to this cache's ledger, for the owning process to
    /// register with its process-wide cache reporter.
    ///
    /// This crate stays free of a runtime dependency deliberately: a cache
    /// this library merely wraps is registered by the process that built
    /// it, not by the library. `None` only for a cache declared
    /// unclassifiable, which retains nothing and so has no footprint to
    /// report.
    #[must_use]
    pub fn ledger(&self) -> Option<CacheLedger> {
        self.entries.ledger()
    }

    /// The artwork for an arbitrary asset `path` at `side` pixels (an
    /// application bundle's own icon): served from the cache, or read through
    /// `reader` (bounded by [`MAX_ARTWORK_BYTES`]), rasterised through
    /// `rasteriser`, verified, and cached.
    ///
    /// `None` — also cached, so a bad asset is not re-read every frame — when
    /// the side is zero, the asset is unreadable, over-long, refused by the
    /// decoder, or the returned pixel block is not exactly `side`×`side`. The
    /// surface is borrowed, never cloned, so a grid draws from the cache in
    /// place.
    pub fn path_artwork<R: ArtworkReader + ?Sized, D: ArtworkRasteriser + ?Sized>(
        &mut self,
        reader: &mut R,
        rasteriser: &mut D,
        path: &str,
        side: u32,
    ) -> Option<&Surface> {
        let key = (String::from(path), side);
        // Build and admit the outcome (a refusal is a cached `None`), then
        // borrow it back out of the cache: returning the admitted borrow
        // directly would tie it to the `&mut` build call, so the read-back is
        // how the surface leaves as a shared borrow the caller can hold.
        let _ = self.entries.get_or_build(&(), key.clone(), || {
            Some(CachedArtwork(render_icon(reader, rasteriser, path, side)))
        });
        self.entries
            .peek(&(), &key)
            .and_then(CachedArtwork::surface)
    }

    /// The artwork for a shipped system icon `kind` at `side` pixels.
    ///
    /// Derives the path with [`icon_artwork_path`] and defers to
    /// [`path_artwork`](Self::path_artwork), so there is one decode path.
    pub fn kind_artwork<R: ArtworkReader + ?Sized, D: ArtworkRasteriser + ?Sized>(
        &mut self,
        reader: &mut R,
        rasteriser: &mut D,
        kind: IconKind,
        side: u32,
    ) -> Option<&Surface> {
        let path = icon_artwork_path(kind);
        self.path_artwork(reader, rasteriser, &path, side)
    }
}

/// Read, rasterise, and verify one asset (the cache-miss path).
///
/// A zero side, an unreadable path, an over-long asset (refused *before* the
/// decoder runs), a refused decode, or a reply that is not exactly
/// `side`×`side` straight-alpha RGBA8 all yield `None`; the pixel-length check
/// is checked arithmetic that never panics.
fn render_icon<R: ArtworkReader + ?Sized, D: ArtworkRasteriser + ?Sized>(
    reader: &mut R,
    rasteriser: &mut D,
    path: &str,
    side: u32,
) -> Option<Surface> {
    if side == 0 {
        return None;
    }
    let bytes = reader.read(path)?;
    if bytes.len() > MAX_ARTWORK_BYTES {
        return None;
    }
    let pixels = rasteriser.rasterise(side, &bytes)?;
    // The pixel count is validated here as well as in the transport: a surface
    // is built only from a block of exactly the promised shape, wherever it
    // came from.
    let expected = (side as usize)
        .checked_mul(side as usize)
        .and_then(|area| area.checked_mul(4))?;
    if pixels.len() != expected {
        return None;
    }
    Surface::from_rgba8(side, side, &pixels)
}

/// Binds an [`ArtworkCache`] to its two seams so it can be handed to a
/// renderer as a plain [`IconArtwork`] lookup without the renderer knowing
/// about I/O.
pub struct IconArtworkSource<'a, R: ArtworkReader + ?Sized, D: ArtworkRasteriser + ?Sized> {
    cache: &'a mut ArtworkCache,
    reader: &'a mut R,
    rasteriser: &'a mut D,
}

impl<'a, R: ArtworkReader + ?Sized, D: ArtworkRasteriser + ?Sized> IconArtworkSource<'a, R, D> {
    /// Bind `cache` to the `reader` and `rasteriser` it resolves through.
    pub fn new(cache: &'a mut ArtworkCache, reader: &'a mut R, rasteriser: &'a mut D) -> Self {
        Self {
            cache,
            reader,
            rasteriser,
        }
    }
}

impl<R: ArtworkReader + ?Sized, D: ArtworkRasteriser + ?Sized> IconArtwork
    for IconArtworkSource<'_, R, D>
{
    fn artwork(&mut self, kind: IconKind, side: u32) -> Option<&Surface> {
        self.cache
            .kind_artwork(self.reader, self.rasteriser, kind, side)
    }
}

#[cfg(test)]
#[path = "artwork_tests.rs"]
mod tests;
