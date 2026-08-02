//! The session's ownership of the user's taskbar pins: the per-user store,
//! the edit operations, and the resolution of each pin into the view the
//! taskbar renders.
//!
//! Pins are per-user configuration (`lib/taskpins`), stored at
//! `~/Settings/Taskbar/pins.conf` under the user's own identity — no new
//! capability, exactly like the program-library overlay. The session is the
//! store's only writer: every edit (pin, unpin, drop-at-index) rewrites the
//! whole document through the one [`SessionFileWriter`] seam, and the
//! in-memory list adopts the change **only after the write succeeded**, so
//! memory and disk can never diverge — a refused write leaves the bar
//! exactly as it was, with a ready-to-print diagnostic for the embedder.
//!
//! Resolution turns each stored [`PinTarget`] into what the bar shows: an
//! `entry` pin resolves through the merged program-library catalog (name,
//! icon asset, bundle); a `bundle` pin resolves through the bundle's own
//! `AppInfo` manifest, read through the one [`SessionFileReader`] seam and
//! bounded by the shared ABI manifest cap. A pin whose target no longer
//! resolves (an uninstalled bundle, an uncatalogued entry) stays visible
//! under its stored identity so the user can still unpin it — it simply
//! cannot launch, and activating it reports why.

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use tairix_abi::{AppInfoHeader, Errno, APPINFO_WIRE_MAX};
use tairix_geometry::Point;
use tairix_icon::IconKind;
use tairix_log::Sink;
use tairix_proglib::{Catalog, EntryId, IconAsset};
use tairix_raster::Surface;
use tairix_reclaim::{disposable_ui_cache, CachedBytes, PressureGauge, ReclaimCache};
use tairix_taskbar::{BarLayout, PinView, TaskId};
use tairix_taskpins::{
    parse as parse_pins, render as render_pins, user_pins_path, BundlePath, PinError, PinList,
    PinTarget,
};
use tairix_window::PinDecision;

use crate::assets::{SessionFileReader, SessionFileWriter};

/// The user's pin store: the parsed list plus the path edits persist to.
#[derive(Clone, Debug, Default)]
pub struct SessionPins {
    list: PinList,
    path: Option<String>,
}

/// Why a pin edit changed nothing. Every variant is a refusal the embedder
/// reports; none is a crash and none leaves memory and disk diverged.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PinEditError {
    /// The session has no home directory, so there is no per-user store to
    /// persist to (pins are per-user state only).
    NoHome,
    /// The target is already pinned.
    AlreadyPinned,
    /// The pin list is at its fixed bound.
    Full,
    /// The index names no pin.
    OutOfRange,
    /// The store write was refused; the in-memory list was left unchanged.
    Write(Errno),
}

impl core::fmt::Display for PinEditError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::NoHome => f.write_str("no home directory; pins cannot be saved"),
            Self::AlreadyPinned => f.write_str("already pinned"),
            Self::Full => f.write_str("the pin strip is full"),
            Self::OutOfRange => f.write_str("no such pin"),
            Self::Write(err) => write!(f, "the pin store could not be written ({err:?})"),
        }
    }
}

impl SessionPins {
    /// Load the user's pin store.
    ///
    /// Mirrors the program-library loader's posture exactly: an absent store
    /// is silently empty (a fresh account); an unreadable, over-long, or
    /// malformed one contributes an empty list plus a ready-to-print warning
    /// line — the desktop always comes up. Without a home there is no store
    /// at all and every edit refuses with [`PinEditError::NoHome`].
    pub fn load<R>(reader: &mut R, home: Option<&str>) -> (Self, Option<String>)
    where
        R: SessionFileReader + ?Sized,
    {
        let Some(path) = home.and_then(user_pins_path) else {
            return (Self::default(), None);
        };
        let (list, warning) = match reader.read(&path) {
            Err(Errno::NotFound) => (PinList::default(), None),
            Err(err) => (
                PinList::default(),
                Some(warning(&path, &format!("read failed ({err:?})"))),
            ),
            Ok(bytes) if bytes.len() > tairix_taskpins::MAX_PINS_LEN => (
                PinList::default(),
                Some(warning(&path, "longer than any valid pin store")),
            ),
            Ok(bytes) => match core::str::from_utf8(&bytes) {
                Err(_) => (PinList::default(), Some(warning(&path, "not valid UTF-8"))),
                Ok(text) => match parse_pins(text) {
                    Ok(list) => (list, None),
                    Err(err) => (PinList::default(), Some(warning(&path, &err.to_string()))),
                },
            },
        };
        (
            Self {
                list,
                path: Some(path),
            },
            warning,
        )
    }

    /// The pins, in display order.
    #[must_use]
    pub fn list(&self) -> &PinList {
        &self.list
    }

    /// Append a pin for the program-library entry `entry`, persist the
    /// store, and return the new pin's index.
    ///
    /// # Errors
    ///
    /// [`PinEditError`] when the entry is already pinned, the list is full,
    /// there is no home, or the write is refused (in which case nothing
    /// changes).
    pub fn pin_entry<W>(&mut self, writer: &mut W, entry: EntryId) -> Result<usize, PinEditError>
    where
        W: SessionFileWriter + ?Sized,
    {
        let mut next = self.list.clone();
        let index = next
            .pin(PinTarget::Entry(entry))
            .map_err(PinEditError::from)?;
        self.persist(writer, next)?;
        Ok(index)
    }

    /// Insert a pin for the application bundle at `bundle`, at `index`
    /// (clamped to the end), persist the store, and return the pin's index.
    ///
    /// # Errors
    ///
    /// [`PinEditError`] when the bundle is already pinned, the list is full,
    /// there is no home, or the write is refused (in which case nothing
    /// changes).
    pub fn pin_bundle_at<W>(
        &mut self,
        writer: &mut W,
        index: usize,
        bundle: BundlePath,
    ) -> Result<usize, PinEditError>
    where
        W: SessionFileWriter + ?Sized,
    {
        let mut next = self.list.clone();
        let index = index.min(next.len());
        next.pin_at(index, PinTarget::Bundle(bundle))
            .map_err(PinEditError::from)?;
        self.persist(writer, next)?;
        Ok(index)
    }

    /// Remove the pin at `index` and persist the store.
    ///
    /// # Errors
    ///
    /// [`PinEditError`] when the index names no pin, there is no home, or
    /// the write is refused (in which case nothing changes).
    pub fn unpin<W>(&mut self, writer: &mut W, index: usize) -> Result<PinTarget, PinEditError>
    where
        W: SessionFileWriter + ?Sized,
    {
        let mut next = self.list.clone();
        let removed = next.unpin(index).ok_or(PinEditError::OutOfRange)?;
        self.persist(writer, next)?;
        Ok(removed)
    }

    /// Write `next` to the store and adopt it only on success, so the
    /// in-memory list never diverges from disk.
    fn persist<W>(&mut self, writer: &mut W, next: PinList) -> Result<(), PinEditError>
    where
        W: SessionFileWriter + ?Sized,
    {
        let Some(path) = self.path.as_deref() else {
            return Err(PinEditError::NoHome);
        };
        let rendered = render_pins(&next);
        writer
            .write(path, rendered.as_bytes())
            .map_err(PinEditError::Write)?;
        self.list = next;
        Ok(())
    }
}

/// The warning line for a pin store the desktop could not use, ready for the
/// embedder to print.
fn warning(path: &str, detail: &str) -> String {
    format!("desktop: taskbar pins {path}: {detail}; using no pins\n")
}

/// One stored pin resolved for display: everything the embedder needs to
/// build the taskbar's view, match a running window, load icon artwork, and
/// launch the application.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResolvedPin {
    /// The display label (catalog name, manifest name, or the bundle's leaf
    /// directory name when nothing better resolves).
    pub label: String,
    /// The program-library entry this pin references, when it is an entry
    /// pin — the taskbar uses it to offer *Unpin* on the entry's popup row.
    pub entry: Option<EntryId>,
    /// The `Run` path to spawn, and to match running desktop launches
    /// against. `None` when the target no longer resolves to a bundle (the
    /// pin still shows, so the user can unpin it, but cannot launch).
    pub run_path: Option<String>,
    /// Where the pin's icon artwork comes from, when its bundle declares an
    /// icon asset.
    pub icon: Option<PinIconSource>,
}

/// Where a pin's icon artwork is authored: a named asset inside the pinned
/// bundle's own `Resources/` directory.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PinIconSource {
    /// The bundle the asset lives in.
    pub bundle: String,
    /// The asset's leaf file name inside the bundle's `Resources/`.
    pub asset: String,
}

impl PinIconSource {
    /// The asset's absolute on-disk path.
    #[must_use]
    pub fn path(&self) -> String {
        format!("{}/Resources/{}", self.bundle, self.asset)
    }
}

/// Resolve every stored pin for display.
///
/// An `entry` pin resolves through the merged `catalog`; a `bundle` pin
/// through its own manifest, read via `reader` and decoded fail-closed. A
/// target that does not resolve keeps a best-effort identity (the entry id
/// or the bundle's leaf name) with no run path, so it can still be shown
/// and unpinned but never guessed at.
pub fn resolve_pins<R>(reader: &mut R, list: &PinList, catalog: &Catalog) -> Vec<ResolvedPin>
where
    R: SessionFileReader + ?Sized,
{
    list.iter()
        .map(|target| match target {
            PinTarget::Entry(id) => resolve_entry(id, catalog),
            PinTarget::Bundle(bundle) => resolve_bundle(reader, bundle),
        })
        .collect()
}

/// Resolve an `entry` pin through the merged program-library catalog.
fn resolve_entry(id: &EntryId, catalog: &Catalog) -> ResolvedPin {
    let Some(entry) = catalog.entry(id) else {
        // The entry left the catalog (uninstalled, hidden, or renamed): the
        // pin keeps its stored identity so the user can see and unpin it.
        return ResolvedPin {
            label: id.as_str().to_string(),
            entry: Some(id.clone()),
            run_path: None,
            icon: None,
        };
    };
    ResolvedPin {
        label: entry.name().as_str().to_string(),
        entry: Some(id.clone()),
        run_path: Some(format!("{}/Run", entry.bundle().as_str())),
        icon: entry.icon().map(|asset| PinIconSource {
            bundle: entry.bundle().as_str().to_string(),
            asset: asset.as_str().to_string(),
        }),
    }
}

/// Resolve a `bundle` pin through the bundle's own `AppInfo` manifest.
///
/// The read is bounded by the shared ABI manifest cap and the decode is the
/// shared fail-closed header decoder; a bundle whose manifest is absent,
/// over-long, or undecodable keeps a leaf-name identity with no run path.
fn resolve_bundle<R>(reader: &mut R, bundle: &BundlePath) -> ResolvedPin
where
    R: SessionFileReader + ?Sized,
{
    let fallback_label = bundle_leaf_label(bundle.as_str());
    let manifest = format!("{}/AppInfo", bundle.as_str());
    let header = match reader.read(&manifest) {
        Ok(bytes) if bytes.len() <= APPINFO_WIRE_MAX => AppInfoHeader::from_bytes(&bytes).ok(),
        _ => None,
    };
    let Some(header) = header else {
        return ResolvedPin {
            label: fallback_label,
            entry: None,
            run_path: None,
            icon: None,
        };
    };
    let icon = header
        .library_icon()
        .and_then(|asset| IconAsset::new(asset).ok())
        .map(|asset| PinIconSource {
            bundle: bundle.as_str().to_string(),
            asset: asset.as_str().to_string(),
        });
    ResolvedPin {
        label: header.bundle_name().to_string(),
        entry: None,
        run_path: Some(format!("{}/Run", bundle.as_str())),
        icon,
    }
}

/// The human-facing fallback label for a bundle path: its leaf directory
/// name without the `.app` suffix.
fn bundle_leaf_label(bundle: &str) -> String {
    let leaf = bundle.rsplit('/').next().unwrap_or(bundle);
    leaf.strip_suffix(tairix_abi::BUNDLE_SUFFIX)
        .unwrap_or(leaf)
        .to_string()
}

impl From<PinError> for PinEditError {
    fn from(err: PinError) -> Self {
        match err {
            PinError::AlreadyPinned => Self::AlreadyPinned,
            PinError::Full => Self::Full,
        }
    }
}

/// The window-channel's pin seam, borrowed by the serve pass exactly like
/// the picker slot: the engine has already attested the caller and
/// validated window ownership; the service decides and applies.
pub trait PinBridge {
    /// A user gesture in an app asked to pin the bundle at `path`.
    fn pin_requested(&mut self, path: &str) -> PinDecision;
    /// An app-reference drag started in channel window `window`.
    fn drag_offered(&mut self, window: u64, path: &str) -> bool;
    /// The drag from channel window `window` was cancelled by the app.
    fn drag_withdrawn(&mut self, window: u64);
}

/// An armed drag: the channel window it started in and the validated bundle
/// it carries. Consumed by the drop (or replaced by the next offer).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DragOffer {
    /// The window-channel id of the source window.
    pub window: u64,
    /// The offered application bundle.
    pub bundle: BundlePath,
}

/// The session's pin service: the store, the seams that read and write it,
/// the one armed drag offer, and a repaint-style dirty latch the embedder
/// drains to re-resolve the bar's views after an edit.
pub struct PinService<R, W> {
    reader: R,
    writer: W,
    pins: SessionPins,
    drag: Option<DragOffer>,
    dirty: bool,
}

impl<R, W> PinService<R, W>
where
    R: SessionFileReader,
    W: SessionFileWriter,
{
    /// A service over the loaded store and the session's file seams.
    pub fn new(reader: R, writer: W, pins: SessionPins) -> Self {
        Self {
            reader,
            writer,
            pins,
            drag: None,
            dirty: false,
        }
    }

    /// The pin store.
    #[must_use]
    pub fn pins(&self) -> &SessionPins {
        &self.pins
    }

    /// Take the dirty latch: `true` when an edit changed the store since
    /// the last take, so the embedder re-resolves the bar's views exactly
    /// when needed.
    pub fn take_dirty(&mut self) -> bool {
        core::mem::take(&mut self.dirty)
    }

    /// Resolve every stored pin for display (see [`resolve_pins`]).
    pub fn resolve(&mut self, catalog: &Catalog) -> Vec<ResolvedPin> {
        resolve_pins(&mut self.reader, self.pins.list(), catalog)
    }

    /// Append a pin for the program-library entry `entry` and persist.
    ///
    /// # Errors
    ///
    /// [`PinEditError`] when the edit changed nothing (see
    /// [`SessionPins::pin_entry`]).
    pub fn pin_entry(&mut self, entry: EntryId) -> Result<usize, PinEditError> {
        let index = self.pins.pin_entry(&mut self.writer, entry)?;
        self.dirty = true;
        Ok(index)
    }

    /// Remove the pin at `index` and persist.
    ///
    /// # Errors
    ///
    /// [`PinEditError`] when the edit changed nothing (see
    /// [`SessionPins::unpin`]).
    pub fn unpin(&mut self, index: usize) -> Result<PinTarget, PinEditError> {
        let removed = self.pins.unpin(&mut self.writer, index)?;
        self.dirty = true;
        Ok(removed)
    }

    /// Validate the bundle at `path` and pin it at `index` (clamped to the
    /// end), persisting the store.
    ///
    /// Validation is fail-closed: the path must spell a store bundle and
    /// the bundle must carry a decodable manifest (so it exists and is
    /// launchable in shape — the signed load gate remains the authority at
    /// launch time). Refusals never partially apply.
    pub fn pin_bundle_at(&mut self, index: usize, path: &str) -> PinDecision {
        let Ok(bundle) = BundlePath::new(path) else {
            return PinDecision::Refused;
        };
        if !self.bundle_is_launchable(&bundle) {
            return PinDecision::Refused;
        }
        match self.pins.pin_bundle_at(&mut self.writer, index, bundle) {
            Ok(_) => {
                self.dirty = true;
                PinDecision::Pinned
            }
            Err(PinEditError::AlreadyPinned) => PinDecision::AlreadyPinned,
            Err(PinEditError::Full) => PinDecision::Full,
            Err(_) => PinDecision::Refused,
        }
    }

    /// Consume the armed drag offer from channel window `window`, if any —
    /// how the embedder resolves a drop.
    pub fn take_drag_for(&mut self, window: u64) -> Option<BundlePath> {
        match self.drag.take() {
            Some(offer) if offer.window == window => Some(offer.bundle),
            other => {
                // A release from any other window disarms nothing.
                self.drag = other;
                None
            }
        }
    }

    /// Whether a drag offer is currently armed (so the embedder knows a
    /// release may be a drop).
    #[must_use]
    pub fn drag_armed(&self) -> bool {
        self.drag.is_some()
    }

    /// The bundle's manifest exists and decodes — the fail-closed shape
    /// check behind every pin admission.
    fn bundle_is_launchable(&mut self, bundle: &BundlePath) -> bool {
        let manifest = format!("{}/AppInfo", bundle.as_str());
        match self.reader.read(&manifest) {
            Ok(bytes) if bytes.len() <= APPINFO_WIRE_MAX => {
                AppInfoHeader::from_bytes(&bytes).is_ok()
            }
            _ => false,
        }
    }
}

impl<R, W> PinBridge for PinService<R, W>
where
    R: SessionFileReader,
    W: SessionFileWriter,
{
    fn pin_requested(&mut self, path: &str) -> PinDecision {
        let end = self.pins.list().len();
        self.pin_bundle_at(end, path)
    }

    fn drag_offered(&mut self, window: u64, path: &str) -> bool {
        // Arm on shape alone; the drop re-validates fully before pinning.
        // One offer is armed at a time: a new drag replaces the old, since
        // only one pointer gesture can be in flight.
        let Ok(bundle) = BundlePath::new(path) else {
            return false;
        };
        self.drag = Some(DragOffer { window, bundle });
        true
    }

    fn drag_withdrawn(&mut self, window: u64) {
        if self
            .drag
            .as_ref()
            .is_some_and(|offer| offer.window == window)
        {
            self.drag = None;
        }
    }
}

/// The icon-decoding seam: turns untrusted icon bytes into `side`×`side`
/// straight-alpha RGBA8 pixels, or `None` when the image is refused.
///
/// In production this is the parser-sandbox icon service — the session
/// never decodes bundle artwork in its own address space; tests supply a
/// fake. The rasteriser is trusted only to the extent the caller verifies:
/// [`IconCache::artwork`] re-checks the returned pixel length before
/// building a surface from it.
pub trait IconRasteriser {
    /// Rasterise `icon` to a `side`-pixel square, or refuse.
    fn rasterise(&mut self, side: u32, icon: &[u8]) -> Option<Vec<u8>>;
}

/// One decode outcome, retained: the rasterised artwork, or the refusal
/// that decoding it produced.
///
/// A refusal is worth remembering — it stops a malformed icon being
/// decoded again on every strip refresh — and it is charged its
/// bookkeeping like any other entry, so a bundle store full of broken
/// artwork cannot grow the cache past its budget.
///
/// Public only because it names the value type of the cache
/// [`artwork_cache`] builds and [`IconCache::new`] takes; the outcome
/// itself is reached through [`IconCache::artwork`].
pub struct CachedArtwork(Option<Surface>);

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

/// The per-seat pinned-app artwork cache: one decode outcome per icon
/// asset path and pixel side.
///
/// A scale change alters the side and so misses the cache, re-rasterising
/// at the new geometry. Nothing invalidates the whole cache at once —
/// bundle artwork changes at install time, not while its icon is on the
/// bar — so the generation is the unit value; what bounds this cache is
/// its budget, and what ends it is the seat's teardown.
///
/// The entries are a user's own pinned-application artwork, so they are
/// owned by the seat, overwritten when released, and given back under
/// memory pressure like every other rasterised desktop asset. The keys
/// are bundle-supplied paths, so an unbounded map here would let a
/// crafted or merely crowded bundle store grow the session without
/// limit; the budget forecloses that.
pub struct IconCache {
    entries: ReclaimCache<(String, u32), CachedArtwork, ()>,
}

/// Bytes of bookkeeping each retained decode costs beyond its pixels: the
/// asset path the entry is keyed by (bounded by the shared path cap), the
/// pixel side, the recency index node, and the map nodes holding them.
const ARTWORK_ENTRY_METADATA_BYTES: usize = 256;

/// Build the cache [`IconCache::new`] wraps, classified and budgeted by
/// the one shared desktop policy.
///
/// `fb_bytes` is the output's frame size, so the artwork a seat may retain
/// scales with the display it is drawn on rather than a fixed ceiling.
#[must_use]
pub fn artwork_cache(
    seat: u64,
    fb_bytes: usize,
    pressure: &'static (dyn PressureGauge + 'static),
    sink: &'static (dyn Sink + Sync),
) -> ReclaimCache<(String, u32), CachedArtwork, ()> {
    disposable_ui_cache(
        "session.pin-artwork",
        seat,
        fb_bytes,
        ARTWORK_ENTRY_METADATA_BYTES,
        pressure,
        sink,
    )
}

impl IconCache {
    /// An empty cache over the caller's ready-built reclaimable cache.
    ///
    /// The cache is injected rather than defaulted for the same reason
    /// the window manager's and taskbar's are: only the session knows the
    /// output's size, the seat, the live pressure gauge, and the audit
    /// sink, and a cache built without them would retain nothing while
    /// looking like it worked.
    #[must_use]
    pub const fn new(entries: ReclaimCache<(String, u32), CachedArtwork, ()>) -> Self {
        Self { entries }
    }

    /// Release every retained decode, overwriting the artwork first.
    ///
    /// Called when the seat is lost or its session ends, so one user's
    /// pinned-application artwork never outlives their session in
    /// reusable heap.
    pub fn teardown(&mut self) {
        self.entries.teardown();
    }

    /// Apply the current memory-pressure band's forced shrink, returning
    /// the bytes released.
    pub fn trim(&mut self) -> usize {
        self.entries.enforce_pressure()
    }

    /// Bytes currently charged for retained artwork, payload plus this
    /// cache's own per-entry bookkeeping.
    #[must_use]
    pub fn charged_bytes(&self) -> usize {
        self.entries.charged_bytes()
    }

    /// The artwork for `source` at `side` pixels: served from the cache,
    /// or read through `reader` (bounded), rasterised through
    /// `rasteriser`, verified, and cached. `None` — also cached — when
    /// the asset is unreadable, over-long, refused by the decoder, or the
    /// returned pixel block is not exactly `side`×`side`.
    pub fn artwork<R, D>(
        &mut self,
        reader: &mut R,
        rasteriser: &mut D,
        source: &PinIconSource,
        side: u32,
    ) -> Option<Surface>
    where
        R: SessionFileReader + ?Sized,
        D: IconRasteriser + ?Sized,
    {
        let path = source.path();
        let served = self.entries.get_or_build(&(), (path.clone(), side), || {
            Some(CachedArtwork(render_icon(reader, rasteriser, &path, side)))
        })?;
        served.0.clone()
    }
}

/// Read, rasterise, and verify one asset (the cache-miss path).
fn render_icon<R, D>(reader: &mut R, rasteriser: &mut D, path: &str, side: u32) -> Option<Surface>
where
    R: SessionFileReader + ?Sized,
    D: IconRasteriser + ?Sized,
{
    if side == 0 {
        return None;
    }
    let bytes = reader.read(path).ok()?;
    let pixels = rasteriser.rasterise(side, &bytes)?;
    // The pixel count is validated here as well as in the transport: a
    // surface is built only from a block of exactly the promised shape,
    // wherever it came from.
    let expected = (side as usize)
        .checked_mul(side as usize)
        .and_then(|area| area.checked_mul(4))?;
    if pixels.len() != expected {
        return None;
    }
    Surface::from_rgba8(side, side, &pixels)
}

/// Resolve a primary release as a possible drop of the armed app-reference
/// drag: `channel` is the releasing window's channel id (when it is a
/// served window), `layout` the bar's current geometry, and `pointer` the
/// screen position of the release.
///
/// The gesture ends here either way — the offer is consumed the moment its
/// window releases, wherever the release lands (one gesture, one decision).
/// `None` means nothing was attempted (no armed drag, an unserved window,
/// or a release away from the pin band); `Some(decision)` is the pin
/// admission's verdict for the embedder to report.
pub fn resolve_pin_drop<R, W>(
    service: &mut PinService<R, W>,
    channel: Option<u64>,
    layout: &BarLayout,
    pointer: Point,
) -> Option<PinDecision>
where
    R: SessionFileReader,
    W: SessionFileWriter,
{
    if !service.drag_armed() {
        return None;
    }
    let bundle = service.take_drag_for(channel?)?;
    let index = layout.pin_drop_index(pointer)?;
    Some(service.pin_bundle_at(index, bundle.as_str()))
}

/// Assemble the taskbar's pin views from the resolved pins, their live
/// running-window matches, and each pin's artwork (rasterised through the
/// sandbox seam and cached).
///
/// The three inputs are positional: `matches[i]` is the running desktop
/// task behind `resolved[i]`, or `None`. A missing artwork leaves the view
/// on the shared application-bundle glyph — the strip never blocks on, or
/// fails over, a bad icon.
pub fn build_pin_views<R, D>(
    resolved: &[ResolvedPin],
    matches: &[Option<TaskId>],
    reader: &mut R,
    rasteriser: &mut D,
    icons: &mut IconCache,
    side: u32,
) -> Vec<PinView>
where
    R: SessionFileReader + ?Sized,
    D: IconRasteriser + ?Sized,
{
    resolved
        .iter()
        .enumerate()
        .map(|(index, pin)| {
            let mut view = PinView::new(pin.label.as_str(), IconKind::AppBundle);
            if let Some(entry) = &pin.entry {
                view = view.with_entry(entry.clone());
            }
            if let Some(task) = matches.get(index).copied().flatten() {
                view = view.with_window(task);
            }
            if let Some(artwork) = pin
                .icon
                .as_ref()
                .and_then(|source| icons.artwork(reader, rasteriser, source, side))
            {
                view = view.with_artwork(artwork);
            }
            view
        })
        .collect()
}
