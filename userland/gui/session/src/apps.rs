//! The session's icon bar: which applications hold a slot, what each one
//! declared, and how a window is reached through the one that owns it.
//!
//! The bar shows *applications*, and an application here is one
//! kernel-attested process. Two facts put a process on the bar, and either
//! alone is enough:
//!
//! * it **declared** an icon-bar presence over the window channel
//!   ([`AppBarService::declare`]), which keeps its slot for the life of the
//!   process whether it owns a window or not — so *Quit* stays meaningful
//!   and "open a fresh window" stays reachable;
//! * it **owns a window**, which gives it a slot even with no declaration,
//!   so no window is ever unreachable. Such a slot has no menu and no
//!   default action: the session invents neither on an application's behalf.
//!
//! Slots keep the order the session first saw each process in, so the strip
//! never reshuffles under the pointer. A process leaves the bar when it has
//! neither a declaration nor a window left — which, for a declaring
//! application, is when the window engine proved the process gone and
//! withdrew its declaration.
//!
//! **Identity is the manifest's, never the process's.** A slot's label,
//! icon, and information panel come from the *signed* `AppInfo` of the
//! bundle the desktop launched the process from, so an application cannot
//! state an identity that is not its own inside system-drawn chrome. A
//! process the desktop did not launch — a shell-spawned program — has no
//! bundle to attest, so its slot states only what the window channel makes
//! knowable and carries no version or author at all.

use alloc::collections::{BTreeMap, BTreeSet};
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use tairix_abi::window_ipc::{AppBar, AppMenu};
use tairix_abi::{AppInfoHeader, Errno, ProcId, APPINFO_WIRE_MAX};
use tairix_geometry::Scale;
use tairix_icon::{
    ArtworkCache, ArtworkRasteriser, ArtworkReader, ArtworkResolver, IconKind, IconPicture,
    IconRequest,
};
use tairix_proglib::{Catalog, EntryId, IconAsset};
use tairix_raster::{Region, Surface};
use tairix_taskbar::{
    AppIdentity, AppSlot, LibraryIconRequest, PickerEntry, TaskId, Taskbar, PICKER_MIN_WINDOWS,
};

use crate::assets::SessionFileReader;

/// The entry-point leaf of a bundle's spawn path: `<bundle>` followed by
/// this is the `Run` binary the desktop launches, and stripping it turns a
/// recorded launch back into the bundle it came from.
pub const BUNDLE_RUN_SUFFIX: &str = "/Run";

/// One application's icon-bar declaration, exactly as the window engine
/// attested and bounded it.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct Declaration {
    /// Whether a primary click on the slot is the application's to handle.
    pub default_action: bool,
    /// The menu a secondary press opens. Empty means the application offers
    /// none, which the bar honours by opening nothing.
    pub menu: AppMenu,
}

/// One application holding a slot, before its identity is resolved: the
/// process, the bundle it was launched from when the desktop launched it,
/// and the windows it owns in the order they opened.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AppGroup {
    /// The kernel-attested process the slot stands for.
    pub owner: ProcId,
    /// The bundle *directory* the desktop launched it from, when it did.
    /// `None` for a process the desktop did not launch: nothing then
    /// attests an identity for it.
    pub bundle: Option<String>,
    /// The application's windows, in the order they opened.
    pub windows: Vec<TaskId>,
}

/// The session's icon-bar service: every application's declaration, the
/// order slots appear in, the identities resolved from their bundles, and a
/// dirty latch the embedder drains to re-push the strip.
#[derive(Debug, Default)]
pub struct AppBarService {
    declared: BTreeMap<ProcId, Declaration>,
    /// The processes that have held a slot, in the order they first did —
    /// the strip's display order, so a slot never moves while it lives.
    order: Vec<ProcId>,
    /// Manifest-attested identity per bundle directory, resolved once.
    identities: BTreeMap<String, AppIdentity>,
    /// Which bundle each process on the strip was launched from, so an
    /// identity already resolved can be found by the process that owns a
    /// window without a second manifest read.
    bundles: BTreeMap<ProcId, String>,
    dirty: bool,
}

impl AppBarService {
    /// A service with nothing on the bar.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Record `owner`'s declaration, replacing any it had made before —
    /// which is how an application changes a row's enablement or its mark.
    ///
    /// # Errors
    ///
    /// [`Errno::NoSpace`] when the bar already holds
    /// [`MAX_BAR_APPS`] applications, so a fork bomb cannot grow the strip
    /// without bound. The refusal is relayed to the application, which
    /// reports it and carries on.
    pub fn declare(&mut self, owner: ProcId, bar: &AppBar) -> Result<(), Errno> {
        if !self.declared.contains_key(&owner) && self.declared.len() >= MAX_BAR_APPS {
            return Err(Errno::NoSpace);
        }
        self.declared.insert(
            owner,
            Declaration {
                default_action: bar.default_action,
                menu: bar.menu,
            },
        );
        self.remember(owner);
        self.dirty = true;
        Ok(())
    }

    /// Drop `owner`'s declaration: the window engine proved the process
    /// gone. Its slot survives only as long as it still owns a window.
    pub fn withdraw(&mut self, owner: ProcId) {
        if self.declared.remove(&owner).is_some() {
            self.dirty = true;
        }
    }

    /// The declaration `owner` made, if it holds one.
    #[must_use]
    pub fn declaration(&self, owner: ProcId) -> Option<&Declaration> {
        self.declared.get(&owner)
    }

    /// Take the dirty latch: `true` when a declaration changed since the
    /// last take, so the embedder re-pushes the strip exactly when needed.
    pub fn take_dirty(&mut self) -> bool {
        core::mem::take(&mut self.dirty)
    }

    /// The strip in display order.
    ///
    /// `windows` is every live served window as `(attested owner, task)`
    /// pairs in the order the windows opened, and `bundle_of` resolves a
    /// process to the bundle directory the desktop launched it from. A
    /// process with a declaration or a window is on the strip; one with
    /// neither is forgotten, so a process that exits without the engine
    /// having withdrawn anything still leaves.
    pub fn strip<F>(&mut self, windows: &[(ProcId, TaskId)], bundle_of: F) -> Vec<AppGroup>
    where
        F: Fn(ProcId) -> Option<String>,
    {
        let mut owned: BTreeMap<ProcId, Vec<TaskId>> = BTreeMap::new();
        for &(owner, task) in windows {
            owned.entry(owner).or_default().push(task);
        }
        for &owner in owned.keys() {
            self.remember(owner);
        }
        let live: BTreeSet<ProcId> = self
            .declared
            .keys()
            .copied()
            .chain(owned.keys().copied())
            .collect();
        self.order.retain(|owner| live.contains(owner));
        let groups: Vec<AppGroup> = self
            .order
            .iter()
            .map(|&owner| AppGroup {
                owner,
                bundle: bundle_of(owner),
                windows: owned.remove(&owner).unwrap_or_default(),
            })
            .collect();
        // An identity is cached per bundle for as long as an application
        // from it is on the bar; a bundle no application still runs from is
        // dropped rather than held for a process that will never return.
        let named: BTreeSet<&str> = groups
            .iter()
            .filter_map(|group| group.bundle.as_deref())
            .collect();
        self.identities
            .retain(|bundle, _| named.contains(bundle.as_str()));
        self.bundles = groups
            .iter()
            .filter_map(|group| group.bundle.clone().map(|bundle| (group.owner, bundle)))
            .collect();
        groups
    }

    /// The identity `owner`'s bundle attests, if one has already been read.
    ///
    /// Cache-only, and deliberately: this answers on the menu-open path,
    /// where reading a manifest would make bringing a chain up wait on the
    /// filesystem. A process whose identity is not yet resolved simply has
    /// none to state, which is honest — a panel may state a name it read but
    /// never one it did not.
    #[must_use]
    pub fn attested_identity(&self, owner: ProcId) -> Option<&AppIdentity> {
        self.identities.get(self.bundles.get(&owner)?)
    }

    /// Build the taskbar's slots from `groups`, resolving each
    /// application's manifest-attested identity and its icon artwork.
    ///
    /// The identity is read from the bundle's *signed* `AppInfo` through
    /// `reader` (once per bundle, then remembered) and the artwork through
    /// the session's one [`ArtworkCache`] at the strip's own `side`, so a
    /// second application from the same bundle costs a lookup rather than a
    /// read and a decode. A bundle whose manifest is absent, over-long, or
    /// undecodable leaves the slot on the window channel's own knowable
    /// label with no version or author — never a guessed identity.
    pub fn slots<R>(
        &mut self,
        groups: &[AppGroup],
        reader: &mut R,
        artwork: (&mut dyn ArtworkResolver, &mut ArtworkCache, u32),
    ) -> Vec<AppSlot>
    where
        R: SessionFileReader + ?Sized,
    {
        let (resolver, cache, side) = artwork;
        groups
            .iter()
            .map(|group| {
                let identity = self.identity(group.bundle.as_deref(), reader);
                let mut slot = AppSlot::new(identity.name.clone(), IconKind::AppBundle)
                    .with_windows(group.windows.clone())
                    .with_identity(identity);
                if let Some(bundle) = group.bundle.as_deref() {
                    let request = IconRequest::bundle(IconKind::AppBundle, bundle);
                    if let Some(art) = cache
                        .artwork(resolver, request, side)
                        .and_then(IconPicture::artwork)
                        .cloned()
                    {
                        slot = slot.with_artwork(art);
                    }
                }
                if let Some(declared) = self.declared.get(&group.owner) {
                    slot = slot.with_declaration(declared.menu, declared.default_action);
                }
                slot
            })
            .collect()
    }

    /// The identity `bundle`'s signed manifest states, resolved once and
    /// remembered.
    ///
    /// A process with no bundle — one the desktop did not launch — has
    /// nothing attesting an identity, so it gets the fallback label and no
    /// version, purpose, or author at all.
    fn identity<R>(&mut self, bundle: Option<&str>, reader: &mut R) -> AppIdentity
    where
        R: SessionFileReader + ?Sized,
    {
        let Some(bundle) = bundle else {
            return AppIdentity {
                name: String::from(UNATTRIBUTED_LABEL),
                ..AppIdentity::default()
            };
        };
        if let Some(identity) = self.identities.get(bundle) {
            return identity.clone();
        }
        let identity = read_identity(reader, bundle);
        self.identities.insert(bundle.to_string(), identity.clone());
        identity
    }

    /// Note that `owner` holds a slot, appending it to the display order the
    /// first time.
    fn remember(&mut self, owner: ProcId) {
        if !self.order.contains(&owner) {
            self.order.push(owner);
        }
    }
}

/// Most applications the icon bar lists at once.
///
/// A **format** bound on a strip a hostile process could otherwise grow by
/// declaring from every fork it makes: the bar is a fixed-width strip of
/// slots that clip away past its region, so a hundred is already far past
/// what any screen can show, and a declaration beyond it is refused rather
/// than accepted into a slot nothing will ever draw.
pub const MAX_BAR_APPS: usize = 100;

/// The label a slot carries when nothing attests an identity for its
/// process — one the desktop did not launch, so no bundle vouches for it.
///
/// Deliberately not a name the process supplied: a window title is the
/// application's own text, and letting it label a system-drawn slot is
/// exactly the identity spoof the manifest attestation exists to stop.
const UNATTRIBUTED_LABEL: &str = "Application";

/// The identity `bundle`'s own signed manifest states.
///
/// Bounded by the shared ABI manifest cap and decoded by the shared
/// fail-closed header decoder; an absent, over-long, or undecodable
/// manifest yields the bundle's leaf name and nothing else, so a panel can
/// state a name it read but never a version it did not.
fn read_identity<R>(reader: &mut R, bundle: &str) -> AppIdentity
where
    R: SessionFileReader + ?Sized,
{
    let header = reader
        .read(&bundle_manifest_path(bundle))
        .ok()
        .as_deref()
        .and_then(decode_bundle_manifest);
    let Some(header) = header else {
        return AppIdentity {
            name: bundle_leaf_label(bundle),
            ..AppIdentity::default()
        };
    };
    AppIdentity {
        name: header.bundle_name().to_string(),
        version: header.bundle_version().to_string(),
        purpose: header.bundle_purpose().map(ToString::to_string),
        author: header.bundle_author().map(ToString::to_string),
    }
}

/// The path of `bundle`'s own signed manifest, `bundle` being the bundle
/// *directory* (no trailing separator).
fn bundle_manifest_path(bundle: &str) -> String {
    format!("{bundle}/AppInfo")
}

/// Decode a bundle's `AppInfo` bytes, bounded by the shared ABI manifest cap
/// and decoded by the shared fail-closed header decoder.
///
/// `manifest` is the raw contents of [`bundle_manifest_path`]. An absent,
/// over-long, or malformed manifest is `None`, so a caller degrades to the
/// bundle's leaf identity rather than handling an error.
fn decode_bundle_manifest(manifest: &[u8]) -> Option<AppInfoHeader> {
    if manifest.len() > APPINFO_WIRE_MAX {
        return None;
    }
    AppInfoHeader::from_bytes(manifest).ok()
}

/// The human-facing fallback label for a bundle path: its leaf directory
/// name without the `.app` suffix.
fn bundle_leaf_label(bundle: &str) -> String {
    let leaf = bundle.rsplit('/').next().unwrap_or(bundle);
    leaf.strip_suffix(tairix_abi::BUNDLE_SUFFIX)
        .unwrap_or(leaf)
        .to_string()
}

/// The icon source a catalogued `id` declares: its own icon asset inside the
/// entry's bundle, or `None` when the entry is uncatalogued or declares no
/// icon (the caller then falls back to the application-bundle artwork or its
/// glyph).
///
/// The single place a program-library row's bundle icon is derived from, so
/// a launcher row and the slot the launched application takes can never
/// resolve their icon two different ways.
fn entry_icon_path(catalog: &Catalog, id: &EntryId) -> Option<String> {
    let entry = catalog.entry(id)?;
    let asset = IconAsset::new(entry.icon()?.as_str()).ok()?;
    Some(format!(
        "{}/Resources/{}",
        entry.bundle().as_str(),
        asset.as_str()
    ))
}

/// Scale `frame` down to a `width`×`height` picker thumbnail through the
/// shared rasteriser.
///
/// A window's content surface is the session's own copy of the application's
/// last presented frame — pixels it already holds, so no new authority is
/// involved — and the scaling is `lib/raster`'s one resampler, never a
/// second one here. It stays in the premultiplied space both surfaces are
/// already stored in, so a thumbnail costs one allocation and one filter pass
/// rather than a straight-alpha round trip and two copies of the whole frame.
/// `None` for a frame or a cell that cannot be resampled (either is empty, or
/// the destination cannot be allocated), which leaves the cell drawing its
/// application's glyph rather than a hole.
#[must_use]
pub fn thumbnail(frame: &Surface, width: u32, height: u32) -> Option<Surface> {
    let region = Region {
        x: 0,
        y: 0,
        width: frame.width(),
        height: frame.height(),
    };
    frame.resampled(region, width, height).ok()
}

/// The cells a hover picker shows for the application at strip index `app`:
/// one per window, captioned with its title and carrying the window's last
/// presented frame scaled to the cell.
///
/// `thumbnail_of` hands back the window's frame already scaled to the cell —
/// the embedder prepared it while the pointer rested out its dwell, one
/// window per turn of the serve loop, so no picker is built by scaling a
/// screenful of frames in one go — and `None` leaves that cell on its
/// application's glyph until the embedder fills it in. Refused, as an empty
/// list, for an application with fewer than [`PICKER_MIN_WINDOWS`] windows:
/// with one window there is nothing to choose, so the bar is asked to open
/// nothing.
pub fn picker_cells<F>(taskbar: &Taskbar, app: usize, mut thumbnail_of: F) -> Vec<PickerEntry>
where
    F: FnMut(TaskId) -> Option<Surface>,
{
    let Some(slot) = taskbar.apps().get(app) else {
        return Vec::new();
    };
    if slot.windows().len() < PICKER_MIN_WINDOWS {
        return Vec::new();
    }
    slot.windows()
        .iter()
        .map(|&window| {
            let title = taskbar
                .tasks()
                .entries()
                .iter()
                .find(|entry| entry.id == window)
                .map_or("", |entry| entry.title.as_str());
            let entry = PickerEntry::new(window, title);
            match thumbnail_of(window) {
                Some(scaled) => entry.with_thumbnail(scaled),
                None => entry,
            }
        })
        .collect()
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

/// Ask `resolver` to start decoding every catalogued application's icon the
/// bar's surfaces will draw, at the sides they draw them.
///
/// A decode is a read plus a sandbox round trip, so a surface that first asks
/// for its icons as it paints shows a screenful of built-in glyphs and only
/// replaces them a round trip per icon later. The bar names its whole set
/// ([`Taskbar::catalog_icon_wants`]) the moment the catalog naming it
/// changes, which is long before the launcher is opened, so the wait happens
/// then rather than in front of the user.
///
/// Nothing is drawn and nothing is waited for: a resolver that produces on the
/// calling thread prefetches nothing at all, so this costs a lookup per icon
/// where there is no worker to hand the decode to.
pub fn prefetch_bar_icons(
    taskbar: &Taskbar,
    scale: Scale,
    resolver: &mut dyn ArtworkResolver,
    cache: &mut ArtworkCache,
) {
    let catalog = taskbar.library().catalog();
    for want in taskbar.catalog_icon_wants(scale) {
        // The application's own icon where the catalog names one, else its class
        // picture — the same order, and the same request, the paint resolves.
        let asset = entry_icon_path(catalog, &want.entry);
        let request = asset.as_deref().map_or_else(
            || IconRequest::kind(IconKind::AppBundle),
            |path| IconRequest::asset(IconKind::AppBundle, path),
        );
        cache.prefetch(resolver, request, want.side);
    }
}

/// Resolve the program-library popup's *visible* rows' icon artwork and set
/// it on the popup, so each application row shows its own icon.
///
/// The taskbar renders and the session resolves: the popup reports which
/// shown rows are launchable entries and the pixel side each draws its icon
/// at ([`tairix_taskbar::LibraryPopup::visible_icon_requests`]), and this
/// resolves each through the same shared [`ArtworkCache`] every other slot
/// uses, from the same catalog-entry icon source — one resolution, never
/// two.
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
pub fn resolve_library_icons(
    taskbar: &mut Taskbar,
    scale: Scale,
    resolver: &mut dyn ArtworkResolver,
    cache: &mut ArtworkCache,
) {
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
        // The application's own icon, else the shipped bundle artwork, else
        // the glyph the row draws when this leaves it empty: the artwork
        // layer owns that order, so the launcher does not restate it.
        let asset = entry_icon_path(taskbar.library().catalog(), &entry);
        let request = asset.as_deref().map_or_else(
            || IconRequest::kind(IconKind::AppBundle),
            |path| IconRequest::asset(IconKind::AppBundle, path),
        );
        let art = cache
            .artwork(resolver, request, side)
            .and_then(IconPicture::artwork)
            .cloned();
        taskbar.set_library_row_artwork(row, art);
    }
}

/// The window-channel's icon-bar seam, borrowed by the serve pass exactly
/// like the picker slot: the engine has already attested the caller and
/// bounded the declaration; the service records it and the strip is
/// re-resolved before the next present.
pub trait AppBarBridge {
    /// The attested `owner` declared (or re-declared) its icon-bar presence.
    ///
    /// # Errors
    ///
    /// Any [`Errno`] the session cannot list the application under; the
    /// refusal is relayed to it, and no slot is recorded.
    fn app_bar_declared(&mut self, owner: ProcId, bar: &AppBar) -> Result<(), Errno>;

    /// The attested `owner` is gone; drop the presence it declared.
    fn app_bar_withdrawn(&mut self, owner: ProcId);

    /// The identity `owner`'s bundle attests, if the session has already read
    /// it. Never reads one here: bringing a menu chain up may not wait on the
    /// filesystem.
    fn attested_identity(&self, owner: ProcId) -> Option<AppIdentity>;
}

impl AppBarBridge for AppBarService {
    fn app_bar_declared(&mut self, owner: ProcId, bar: &AppBar) -> Result<(), Errno> {
        self.declare(owner, bar)
    }

    fn app_bar_withdrawn(&mut self, owner: ProcId) {
        self.withdraw(owner);
    }

    fn attested_identity(&self, owner: ProcId) -> Option<AppIdentity> {
        Self::attested_identity(self, owner).cloned()
    }
}
