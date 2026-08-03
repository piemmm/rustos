//! Resolving an icon to drawable pixels: the shared artwork layer.
//!
//! [`IconKind`] and [`builtin_icon`](crate::builtin_icon) give the desktop a
//! *total* set of scalable vector glyphs — every kind always draws something.
//! On top of that floor this module adds the preferred tiers: pre-rasterised
//! raster **artwork**, shipped under `/System/Graphics/Icons` as one
//! `<asset-id>.png` per kind, and above it the icon a thing carries of its
//! *own* — an application bundle's `Resources/` master, named by its signed
//! manifest. A draw site states both in one [`IconRequest`] and the answer
//! resolves in one order: the thing's own icon, then its kind's shipped
//! artwork, then the built-in glyph. Resolution is therefore total however
//! much of it is missing, and the order lives here rather than being
//! re-decided by each surface.
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
//! [`ArtworkCache`] retains each decode — success *or* refusal — keyed by what
//! was resolved and the requested side, over the one shared reclaimable-memory
//! cache (`lib/reclaim`) so a crowded or crafted bundle store can never grow a
//! session without bound. [`IconArtworkSource`] binds a cache to its two
//! seams so a renderer can be handed a plain [`IconArtwork`] lookup that
//! borrows the pixels without knowing anything about I/O, and [`NoArtwork`] is
//! the all-glyph lookup a headless build or a test uses.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use tairix_abi::{AppInfoHeader, BundleEntry, APPINFO_WIRE_MAX};
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

/// Largest source side, in pixels, an icon is ever decoded at.
///
/// A fixed validation bound like [`MAX_ARTWORK_BYTES`], and for the same
/// reason: kept well above any real icon master's native resolution but far
/// below a size that would turn one small tile into an expensive decode. It
/// is deliberately independent of the output side a draw site asks for, so a
/// tiny request cannot smuggle a huge source image through. One definition,
/// shared by the sandboxed decoder that enforces it at runtime and the image
/// build that refuses artwork the desktop would later refuse.
pub const MAX_ARTWORK_SIDE: u32 = 2048;

/// Smallest side, in pixels, a shipped icon master may have.
///
/// Icon artwork is authored at a resolution that exceeds the slots the
/// desktop draws it in, so a slot only ever downscales — an upscaled master
/// is visibly soft. This is the floor the image build holds every shipped
/// master and every bundle's own icon to; it is a *quality* contract on
/// first-party artwork, not a validation bound on untrusted input, so the
/// runtime still draws a smaller icon a third party ships rather than
/// refusing it.
pub const MIN_ARTWORK_SIDE: u32 = 256;

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

/// A thing's *own* artwork, preferred over its kind's shipped artwork.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum OwnIcon<'a> {
    /// An asset path the caller has already resolved (the program-library
    /// catalog stores one per listed application).
    Asset(&'a str),
    /// A `<Name>.app` directory whose signed manifest names the asset. The
    /// artwork layer reads and validates the manifest itself, so a draw site
    /// holding only a directory entry needs no manifest knowledge of its own.
    Bundle(&'a str),
}

/// What a draw site is asking for a picture of.
///
/// Every request names the [`IconKind`] that always resolves — the shipped
/// class artwork, and failing that the built-in glyph — and may additionally
/// name the thing's own icon, which takes precedence when it resolves. The
/// resulting order (own icon, then class artwork, then glyph) is the desktop's
/// one icon-resolution rule, stated here so every surface obeys the same one.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct IconRequest<'a> {
    kind: IconKind,
    own: Option<OwnIcon<'a>>,
}

impl<'a> IconRequest<'a> {
    /// A picture for a `kind` alone: shipped class artwork, else the glyph.
    #[must_use]
    pub const fn kind(kind: IconKind) -> Self {
        Self { kind, own: None }
    }

    /// A picture for a thing whose own asset path is already known, falling
    /// back to `kind` when that asset will not serve.
    #[must_use]
    pub const fn asset(kind: IconKind, path: &'a str) -> Self {
        Self {
            kind,
            own: Some(OwnIcon::Asset(path)),
        }
    }

    /// A picture for the application bundle at `dir` (a `<Name>.app`
    /// directory), falling back to `kind` when the bundle declares no icon or
    /// its icon will not serve.
    #[must_use]
    pub const fn bundle(kind: IconKind, dir: &'a str) -> Self {
        Self {
            kind,
            own: Some(OwnIcon::Bundle(dir)),
        }
    }

    /// The kind that resolves when nothing of the thing's own does.
    #[must_use]
    pub const fn icon_kind(&self) -> IconKind {
        self.kind
    }
}

/// A draw-site lookup: the pre-rasterised artwork for `request` at `side`, or
/// `None` to draw the built-in glyph instead.
///
/// The borrow keeps the pixels in the cache the lookup owns, so a grid that
/// draws many icons a frame reads each surface in place rather than copying
/// it.
pub trait IconArtwork {
    /// The artwork for `request` at `side` pixels, or `None` to fall back to
    /// the built-in glyph.
    fn artwork(&mut self, request: IconRequest<'_>, side: u32) -> Option<&Surface>;
}

/// The all-glyph lookup: never any artwork.
///
/// Used by a headless build with no shipped raster assets and by tests: every
/// query returns `None`, so every draw site falls back to its built-in glyph.
#[derive(Copy, Clone, Debug, Default)]
pub struct NoArtwork;

impl IconArtwork for NoArtwork {
    fn artwork(&mut self, _request: IconRequest<'_>, _side: u32) -> Option<&Surface> {
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

/// What one retained decode is keyed by.
///
/// A bundle is keyed by its *directory*, not by the asset its manifest names,
/// so the manifest read is paid once per bundle and a bundle that declares no
/// icon (or names one that will not decode) remembers that refusal too. A
/// directory and an asset file can never spell the same path, but keeping the
/// two apart in the key type says so rather than relying on it.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum ArtworkKey {
    /// An asset file read directly: a shipped `<asset-id>.png`, or an icon
    /// path the caller had already resolved.
    Asset(String),
    /// An application-bundle directory, resolved through its own manifest.
    Bundle(String),
}

impl ArtworkKey {
    /// The path this key names.
    fn as_str(&self) -> &str {
        match self {
            Self::Asset(path) | Self::Bundle(path) => path,
        }
    }
}

/// The outcome of building one cache slot.
enum Slot {
    /// Built and retained, and it has artwork to draw.
    Served,
    /// Retained, but there is no artwork for it — the next tier answers.
    Empty,
    /// The cache retained nothing (pressure forbids growth, or it has
    /// disabled itself), so no borrow can be handed out however much work is
    /// done.
    Closed,
}

/// The reclaim-governed decode cache, keyed by what was resolved and the pixel
/// side.
///
/// One decode outcome per `(key, side)`: a scale change alters the side and so
/// misses, re-rasterising at the new geometry. Nothing invalidates the whole
/// cache at once — shipped artwork changes at install time, not while an icon
/// is on screen — so the generation is the unit value; what bounds this cache
/// is its budget, and what ends it is the owner's teardown.
///
/// The keys are paths, some of them bundle-supplied, so an unbounded map here
/// would let a crafted or merely crowded store grow a session without limit;
/// the budget forecloses that.
pub struct ArtworkCache {
    entries: ReclaimCache<(ArtworkKey, u32), CachedArtwork, ()>,
}

impl ArtworkCache {
    /// An empty cache over the caller's ready-built reclaimable cache.
    ///
    /// The cache is injected rather than defaulted because only the owning
    /// process knows the output's size, the seat, the live pressure gauge, and
    /// the audit sink; a cache built without them would retain nothing while
    /// looking like it worked. [`artwork_cache`] assembles one.
    #[must_use]
    pub const fn new(entries: ReclaimCache<(ArtworkKey, u32), CachedArtwork, ()>) -> Self {
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

    /// The artwork for one already-resolved asset `path` at `side` pixels,
    /// with no fallback tier: served from the cache, or read through `reader`
    /// (bounded by [`MAX_ARTWORK_BYTES`]), rasterised through `rasteriser`,
    /// verified, and cached. A caller that wants the whole resolution order
    /// asks [`artwork`](Self::artwork) instead.
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
        let key = (ArtworkKey::Asset(String::from(path)), side);
        let _ = self.build_slot(&key, || render_icon(reader, rasteriser, path, side));
        self.borrow_slot(&key)
    }

    /// The picture for `request` at `side` pixels: the thing's own icon when
    /// it resolves, else the shipped artwork for its kind, else `None` for the
    /// caller to draw the built-in glyph.
    ///
    /// This is the one place the desktop's icon-resolution order is decided,
    /// so every surface — a taskbar button, a launcher row, a file-manager
    /// tile — resolves identically. A bundle's own icon is read and decoded
    /// exactly like a shipped asset: bounded, sandboxed by the injected
    /// rasteriser, and accepted only as exactly the pixels asked for.
    pub fn artwork<R: ArtworkReader + ?Sized, D: ArtworkRasteriser + ?Sized>(
        &mut self,
        reader: &mut R,
        rasteriser: &mut D,
        request: IconRequest<'_>,
        side: u32,
    ) -> Option<&Surface> {
        if let Some(own) = request.own {
            let key = (own.cache_key(), side);
            match self.build_slot(&key, || render_own(reader, rasteriser, own, side)) {
                Slot::Served => return self.borrow_slot(&key),
                // Nothing is being retained, so the kind tier could only
                // repeat the same refusal at the cost of another decode.
                Slot::Closed => return None,
                Slot::Empty => {}
            }
        }
        let key = (ArtworkKey::Asset(icon_artwork_path(request.kind)), side);
        let _ = self.build_slot(&key, || {
            render_icon(reader, rasteriser, key.0.as_str(), side)
        });
        self.borrow_slot(&key)
    }

    /// Build `key`'s slot once (a refusal is retained as an empty slot), and
    /// report whether it can serve.
    fn build_slot<F: FnOnce() -> Option<Surface>>(
        &mut self,
        key: &(ArtworkKey, u32),
        render: F,
    ) -> Slot {
        match self
            .entries
            .get_or_build(&(), key.clone(), || Some(CachedArtwork(render())))
        {
            Some(served) if !served.is_cached() => Slot::Closed,
            Some(served) if served.surface().is_some() => Slot::Served,
            _ => Slot::Empty,
        }
    }

    /// Borrow a built slot's artwork back out of the cache.
    ///
    /// Returning the admitted value directly would tie the borrow to the
    /// `&mut` build call, so the read-back is how a surface leaves as a shared
    /// borrow the caller can hold while drawing.
    fn borrow_slot(&self, key: &(ArtworkKey, u32)) -> Option<&Surface> {
        self.entries.peek(&(), key).and_then(CachedArtwork::surface)
    }
}

impl OwnIcon<'_> {
    /// The cache slot this icon source occupies.
    fn cache_key(self) -> ArtworkKey {
        match self {
            Self::Asset(path) => ArtworkKey::Asset(String::from(path)),
            Self::Bundle(dir) => ArtworkKey::Bundle(String::from(dir)),
        }
    }
}

/// Read, rasterise, and verify a thing's own icon (the cache-miss path).
fn render_own<R: ArtworkReader + ?Sized, D: ArtworkRasteriser + ?Sized>(
    reader: &mut R,
    rasteriser: &mut D,
    own: OwnIcon<'_>,
    side: u32,
) -> Option<Surface> {
    match own {
        OwnIcon::Asset(path) => render_icon(reader, rasteriser, path, side),
        OwnIcon::Bundle(dir) => {
            let path = bundle_icon_path(reader, dir)?;
            render_icon(reader, rasteriser, &path, side)
        }
    }
}

/// The path of the icon an application bundle names in its own manifest, or
/// `None` when it names none.
///
/// A bundle's manifest is authored by whoever built the bundle, so it is
/// treated as untrusted input at this boundary as much as the artwork is: it
/// is read under the ABI's own wire bound, decoded by the shared fail-closed
/// header decoder, and the asset is accepted only as a plain file name. A
/// bundle therefore cannot aim the desktop at a file outside its own
/// `Resources/` — the name is resolved *inside* the directory it came from,
/// never joined as a caller-supplied path.
fn bundle_icon_path<R: ArtworkReader + ?Sized>(reader: &mut R, dir: &str) -> Option<String> {
    let manifest = reader.read(&format!("{dir}/{}", BundleEntry::AppInfo.as_str()))?;
    if manifest.len() > APPINFO_WIRE_MAX {
        return None;
    }
    let header = AppInfoHeader::from_bytes(&manifest).ok()?;
    let asset = header.library_icon()?;
    tairix_path::validate_file_name(asset).ok()?;
    Some(format!("{dir}/{}/{asset}", BundleEntry::Resources.as_str()))
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
    fn artwork(&mut self, request: IconRequest<'_>, side: u32) -> Option<&Surface> {
        self.cache
            .artwork(self.reader, self.rasteriser, request, side)
    }
}

#[cfg(test)]
#[path = "artwork_tests.rs"]
mod tests;
