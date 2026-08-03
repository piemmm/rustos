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
use tairix_geometry::{Point, Scale};
use tairix_icon::{ArtworkCache, ArtworkRasteriser, ArtworkReader, IconKind};
use tairix_proglib::{Catalog, EntryId, IconAsset};
use tairix_taskbar::{BarLayout, LibraryIconRequest, PinView, TaskId, Taskbar};
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
        self.pin_at(writer, self.list.len(), PinTarget::Entry(entry))
    }

    /// Insert a pin for `target` at `index` (clamped to the end), persist the
    /// store, and return the pin's index.
    ///
    /// The one insertion point every pin edit goes through, whichever kind of
    /// target it names, so a catalogued entry and a bundle path can never be
    /// admitted to the store on different terms.
    ///
    /// # Errors
    ///
    /// [`PinEditError`] when the target is already pinned, the list is full,
    /// there is no home, or the write is refused (in which case nothing
    /// changes).
    pub fn pin_at<W>(
        &mut self,
        writer: &mut W,
        index: usize,
        target: PinTarget,
    ) -> Result<usize, PinEditError>
    where
        W: SessionFileWriter + ?Sized,
    {
        let mut next = self.list.clone();
        let index = index.min(next.len());
        next.pin_at(index, target).map_err(PinEditError::from)?;
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
        icon: entry_icon_source(catalog, id),
    }
}

/// The icon source a catalogued `id` declares: its own icon asset inside the
/// entry's bundle, or `None` when the entry is uncatalogued or declares no
/// icon (the caller then falls back to the application-bundle artwork or its
/// glyph).
///
/// The single place an `entry` pin *and* a program-library row both derive
/// their bundle icon from, so a pinned application and its library row can
/// never resolve their icon two different ways.
fn entry_icon_source(catalog: &Catalog, id: &EntryId) -> Option<PinIconSource> {
    let entry = catalog.entry(id)?;
    entry.icon().map(|asset| PinIconSource {
        bundle: entry.bundle().as_str().to_string(),
        asset: asset.as_str().to_string(),
    })
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

/// Where an armed pin drag started.
///
/// A drag ends where it began: only the origin that armed an offer can
/// consume or withdraw it, so a release in one place can never claim a drag
/// started in another. The program-library popup is a single surface rather
/// than one of many served windows, so it needs no id of its own.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum DragOrigin {
    /// A served application window, by its window-channel id.
    Window(u64),
    /// The taskbar's own program-library popup.
    Library,
}

/// An armed drag: where it started and the pin target it carries. Consumed
/// by the drop (or replaced by the next offer).
///
/// The target is the pin store's own union, so a drag from the program
/// library carries the catalogued entry it was dragged from and a drag from
/// an application window carries the bundle path that window offered —
/// neither is re-derived from the other, and the drop admits both through
/// one path.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DragOffer {
    /// Where the drag was started.
    pub origin: DragOrigin,
    /// What a drop would pin.
    pub target: PinTarget,
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
        self.insert(index, PinTarget::Bundle(bundle))
    }

    /// Validate `target` and pin it at `index` (clamped to the end),
    /// persisting the store.
    ///
    /// The admission each kind of target needs, in the one place a drop
    /// resolves: a bundle path must spell a store bundle carrying a
    /// decodable manifest; a program-library entry must still be in
    /// `catalog`, so an entry uninstalled between the drag and the drop is
    /// refused rather than recorded as a pin that can never launch.
    pub fn pin_target_at(
        &mut self,
        index: usize,
        target: PinTarget,
        catalog: &Catalog,
    ) -> PinDecision {
        match target {
            PinTarget::Bundle(bundle) => self.pin_bundle_at(index, bundle.as_str()),
            PinTarget::Entry(entry) => {
                if catalog.entry(&entry).is_none() {
                    return PinDecision::Refused;
                }
                self.insert(index, PinTarget::Entry(entry))
            }
        }
    }

    /// Arm an offer of `target`, dragged from `origin`.
    ///
    /// Armed on shape alone; the drop re-validates fully before pinning.
    /// One offer is armed at a time — a new drag replaces the old, since
    /// only one pointer gesture can be in flight.
    pub fn offer_drag(&mut self, origin: DragOrigin, target: PinTarget) {
        self.drag = Some(DragOffer { origin, target });
    }

    /// Withdraw the armed offer if `origin` is the one that armed it.
    pub fn withdraw_drag(&mut self, origin: DragOrigin) {
        if self.drag.as_ref().is_some_and(|off| off.origin == origin) {
            self.drag = None;
        }
    }

    /// Consume the armed drag offer made from `origin`, if any — how the
    /// embedder resolves a drop.
    pub fn take_drag_for(&mut self, origin: DragOrigin) -> Option<PinTarget> {
        match self.drag.take() {
            Some(offer) if offer.origin == origin => Some(offer.target),
            other => {
                // A release from anywhere else disarms nothing.
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

    /// Insert an admitted `target` at `index` and report the store's verdict.
    /// The one place an admitted pin reaches the store, so every refusal is
    /// spelled the same way.
    fn insert(&mut self, index: usize, target: PinTarget) -> PinDecision {
        match self.pins.pin_at(&mut self.writer, index, target) {
            Ok(_) => {
                self.dirty = true;
                PinDecision::Pinned
            }
            Err(PinEditError::AlreadyPinned) => PinDecision::AlreadyPinned,
            Err(PinEditError::Full) => PinDecision::Full,
            Err(_) => PinDecision::Refused,
        }
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
        // The window channel offers a *path*, so its shape is checked here,
        // where paths arrive; the drop re-validates fully before pinning.
        let Ok(bundle) = BundlePath::new(path) else {
            return false;
        };
        self.offer_drag(DragOrigin::Window(window), PinTarget::Bundle(bundle));
        true
    }

    fn drag_withdrawn(&mut self, window: u64) {
        self.withdraw_drag(DragOrigin::Window(window));
    }
}

/// The icon-decoding seam: turns untrusted icon bytes into `side`×`side`
/// straight-alpha RGBA8 pixels, or `None` when the image is refused.
///
/// In production this is the parser-sandbox icon service — the session
/// never decodes bundle artwork in its own address space; tests supply a
/// fake. It is bridged to the shared [`ArtworkRasteriser`] by
/// [`ArtworkSandbox`] so the one [`ArtworkCache`] verifies and retains every
/// decode; the cache re-checks the returned pixel length before building a
/// surface from it, so the seam is trusted only to the extent the cache
/// verifies.
pub trait IconRasteriser {
    /// Rasterise `icon` to a `side`-pixel square, or refuse.
    fn rasterise(&mut self, side: u32, icon: &[u8]) -> Option<Vec<u8>>;
}

/// Bridges the session's [`SessionFileReader`] to the shared
/// [`ArtworkReader`] the one [`ArtworkCache`] reads asset bytes through.
///
/// A refused or missing read (any [`Errno`]) becomes the glyph-fallback
/// `None`: an unreadable bundle icon is never fatal, the caller degrades to
/// the built-in artwork. Owns its reader so it can be boxed as the shell's
/// artwork seam; it adds no logic of its own beyond the `Result`→`Option`
/// bridge.
pub struct ArtworkFileReader<R>(pub R);

impl<R: SessionFileReader> ArtworkReader for ArtworkFileReader<R> {
    fn read(&mut self, path: &str) -> Option<Vec<u8>> {
        self.0.read(path).ok()
    }
}

/// Bridges the session's [`IconRasteriser`] (the parser sandbox) to the
/// shared [`ArtworkRasteriser`] the one [`ArtworkCache`] rasterises through.
///
/// The pixel signature is identical, so this only forwards — the untrusted
/// decode still runs in the sandbox worker, never here. Owns its rasteriser
/// so it can be boxed as the shell's artwork seam.
pub struct ArtworkSandbox<D>(pub D);

impl<D: IconRasteriser> ArtworkRasteriser for ArtworkSandbox<D> {
    fn rasterise(&mut self, side: u32, bytes: &[u8]) -> Option<Vec<u8>> {
        self.0.rasterise(side, bytes)
    }
}

/// Resolve a primary release as a possible drop of the armed app-reference
/// drag: `origin` is where the release came from (the releasing window's
/// channel id, or the program-library popup) when it is a place a drag can
/// start, `catalog` the merged program library an entry target is checked
/// against, `layout` the bar's current geometry, and `pointer` the screen
/// position of the release.
///
/// The gesture ends here either way — the offer is consumed the moment its
/// origin releases, wherever the release lands (one gesture, one decision),
/// so a release away from the pin band withdraws the drag cleanly rather
/// than leaving it armed for some later click. `None` means nothing was
/// attempted (no armed drag, a release from somewhere a drag cannot start,
/// or a release away from the pin band); `Some(decision)` is the pin
/// admission's verdict for the embedder to report.
pub fn resolve_pin_drop<R, W>(
    service: &mut PinService<R, W>,
    origin: Option<DragOrigin>,
    catalog: &Catalog,
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
    let target = service.take_drag_for(origin?)?;
    let index = layout.pin_drop_index(pointer)?;
    Some(service.pin_target_at(index, target, catalog))
}

/// Assemble the taskbar's pin views from the resolved pins, their live
/// running-window matches, and each pin's artwork (read and rasterised
/// through the shared [`ArtworkCache`]).
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
    cache: &mut ArtworkCache,
    side: u32,
) -> Vec<PinView>
where
    R: ArtworkReader + ?Sized,
    D: ArtworkRasteriser + ?Sized,
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
                .and_then(|source| cache.path_artwork(reader, rasteriser, &source.path(), side))
                .cloned()
            {
                view = view.with_artwork(artwork);
            }
            view
        })
        .collect()
}

/// Resolve the program-library popup's *visible* rows' icon artwork and set
/// it on the popup, so each application row shows its own icon.
///
/// The taskbar renders and the session resolves (exactly as it does for a
/// pinned application): the popup reports which shown rows are launchable
/// entries and the pixel side each draws its icon at
/// ([`tairix_taskbar::LibraryPopup::visible_icon_requests`]), and this
/// resolves each through
/// the same shared [`ArtworkCache`] the pin strip uses, from the same
/// catalog-entry icon source a pin resolves from — one resolution, never two.
///
/// A row whose entry declares an icon gets that icon; a row whose entry
/// declares none, or whose asset will not read or decode, falls back to the
/// shipped application-bundle artwork; and a row for which even that is
/// absent is left with no artwork, so the shared list-row slot draws its
/// built-in glyph and a row can never blank.
///
/// Only the rows the popup actually shows at `scale` are resolved, so
/// opening a large library never decodes an icon nobody sees, and a row
/// already holding artwork of the right pixel side is left alone, so
/// re-resolving before each paint costs a lookup rather than a copy. A
/// closed popup resolves nothing at all.
pub fn resolve_library_icons<R, D>(
    taskbar: &mut Taskbar,
    scale: Scale,
    reader: &mut R,
    rasteriser: &mut D,
    cache: &mut ArtworkCache,
) where
    R: ArtworkReader + ?Sized,
    D: ArtworkRasteriser + ?Sized,
{
    if !taskbar.library().is_open() {
        return;
    }
    let requests = {
        let layout = taskbar.library_layout(scale);
        taskbar
            .library()
            .visible_icon_requests(&layout, scale, taskbar.theme())
    };
    for LibraryIconRequest { row, side, entry } in requests {
        let drawn = taskbar
            .library()
            .row_artwork(row)
            .is_some_and(|art| art.width() == side);
        if drawn {
            continue;
        }
        // The bundle's own icon first; each borrow of the cache is cloned out
        // before the next so the fallback can re-borrow it.
        let source = entry_icon_source(taskbar.library().catalog(), &entry);
        let mut art = None;
        if let Some(source) = source {
            art = cache
                .path_artwork(reader, rasteriser, &source.path(), side)
                .cloned();
        }
        if art.is_none() {
            art = cache
                .kind_artwork(reader, rasteriser, IconKind::AppBundle, side)
                .cloned();
        }
        taskbar.set_library_row_artwork(row, art);
    }
}
