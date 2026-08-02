//! Choosing the pointer cursor from live interaction state.
//!
//! The cursor *artwork* (a scalable, colourful, replaceable vector cursor)
//! lives in `lib/cursor`; the compositor ([`crate::cursor`]) only blits the
//! rasterised image. This module is the missing middle: it decides **which**
//! [`CursorKind`] the desktop should show given what the user is doing, then
//! turns that choice into pixels and hands them to the compositor.
//!
//! # The policy
//!
//! [`desired_cursor`] reads the interaction state held by the
//! [`InputRouter`] and the [`Compositor`] and returns a [`CursorKind`]:
//!
//! - an in-flight window move-grab outranks everything and shows
//!   [`CursorKind::Move`];
//! - otherwise the pointer takes the [`cursor_hint`](crate::Window::cursor_hint)
//!   of the top-most window under it (a text editor's
//!   [`Text`](CursorKind::Text), a control's [`Pointer`](CursorKind::Pointer),
//!   a busy view's [`Busy`](CursorKind::Busy), …);
//! - over the desktop background it is the plain [`CursorKind::Arrow`].
//!
//! The policy is a pure function of state, so it is trivially testable and
//! has no hidden authority.
//!
//! # Applying the choice
//!
//! [`CursorController`] ties the policy to the artwork. It owns the active
//! [`CursorRegistry`] and remembers which [`CursorKind`] is on screen and at
//! what density it was rasterised. It does **not** own the scale: the desktop
//! density belongs to the output, so the controller reads it from the
//! [`Compositor`] ([`Compositor::scale`]) every time it installs a cursor. [`CursorController::refresh`] runs the policy and
//! re-rasterises only when something the cursor depends on actually changed —
//! the chosen kind, the active cursor set, or the output scale — installing
//! the result through [`Compositor::set_cursor`]. A runtime DPI change is
//! therefore [`Compositor::set_scale`] followed by one `refresh`. Pointer
//! *motion* is not its job — the caller moves the existing overlay with
//! [`Compositor::move_cursor`]; the controller switches the *shape*.
//!
//! Rasterisation can fail (a degenerate cursor or scale yields no image). The controller fails closed: it leaves the current
//! cursor in place and reports that nothing changed rather than blanking the
//! pointer or panicking.
//!
//! # The cache
//!
//! Each shown [`CursorKind`] is rasterised at most once per scale and cursor
//! set through a [`ReclaimCache`] (`plans/SMARTRAM.md` section 6.4): a
//! bounded, pressure-governed cache shared with every other reclaimable
//! cache in TAIRiX, rather than an unbounded cache of the controller's own.
//! [`CursorController`] never builds its own cache policy — that would be
//! the controller inventing its own memory budget — it is handed a
//! ready-built [`ReclaimCache`] as a required argument to
//! [`new`](CursorController::new) / [`with_registry`](CursorController::with_registry),
//! exactly as the kernel hands a `ClusterCache` to the ARXFS driver.
//! [`cursor_cache`] is the one place this crate assembles that cache from the
//! shared desktop cache policy, so the caller supplies only the real output
//! size, the owning seat, and the process's live pressure gauge and audit
//! sink — never a parameterless fallback. A cache built without a live gauge
//! would classify and serve every lookup correctly while retaining nothing,
//! which is a defect that looks exactly like working software; requiring the
//! real inputs up front is what rules that out.

use tairix_cursor::{CursorImage, CursorRegistry, CursorSetId};
use tairix_log::Sink;
use tairix_reclaim::{disposable_ui_cache, CacheAccounting, PressureGauge, ReclaimCache};
use tairix_theme::CursorKind;

use crate::geometry::Point;
use crate::input::InputRouter;
use crate::window::Window;
use crate::Compositor;

/// Worst-case per-entry bookkeeping the cache charges on top of a cursor
/// image's own pixel bytes: the LRU/index tick and charged-size fields
/// (`u64` + `usize`) plus this cache's small share of its two `BTreeMap`s'
/// node overhead. `CursorKind` itself is a bare enum discriminant, so the
/// key contributes negligible bytes beyond this.
const ENTRY_METADATA_BYTES: usize = 64;

/// The [`CursorKind`] the desktop should display for the current
/// interaction state (see the [module docs](self)).
///
/// A pure function of `router` and `compositor`: it reads the pointer
/// position, the move-grab flag, and the window under the pointer, and
/// never mutates either argument.
#[must_use]
pub fn desired_cursor(router: &InputRouter, compositor: &Compositor) -> CursorKind {
    if router.is_moving() {
        return CursorKind::Move;
    }
    match compositor.window_at(router.pointer()) {
        Some(id) => compositor
            .window(id)
            .map_or(CursorKind::Arrow, Window::cursor_hint),
        None => CursorKind::Arrow,
    }
}

/// The epoch a cached cursor image is valid for: the desktop scale (in
/// percent) paired with the active cursor-set id. A scale change or a
/// cursor-set swap moves the epoch on and invalidates every cached image.
pub type CursorEpoch = (u32, CursorSetId);

/// Build the one [`ReclaimCache`] a [`CursorController`] retains rasterised
/// cursor images in, classified through the shared desktop cache policy
/// (`tairix_reclaim::disposable_ui_cache`).
///
/// `seat` is the seat the controller belongs to and `fb_bytes` is the real
/// output's backing byte size, so the cache's budget scales with the actual
/// display rather than a guessed constant; `pressure` and `sink` are the
/// process's live pressure gauge and audit sink. The embedder — the only
/// party that knows all four — calls this once and hands the result to
/// [`CursorController::new`] or [`CursorController::with_registry`].
#[must_use]
pub fn cursor_cache(
    seat: u64,
    fb_bytes: usize,
    pressure: &'static (dyn PressureGauge + 'static),
    sink: &'static (dyn Sink + Sync),
) -> ReclaimCache<CursorKind, CursorImage, CursorEpoch> {
    disposable_ui_cache(
        "wm.cursor",
        seat,
        fb_bytes,
        ENTRY_METADATA_BYTES,
        pressure,
        sink,
    )
}

/// Drives the on-screen pointer shape from interaction state.
///
/// Holds the active [`CursorRegistry`] (the replaceable cursor sets) and the
/// [`CursorKind`] currently shown, paired with the cache epoch (scale and
/// cursor-set id) it was rasterised for. The density is **not** stored here —
/// it belongs to the output, so [`refresh`](Self::refresh) reads it from the
/// [`Compositor`] and applies the [`desired_cursor`]
/// policy.
///
/// Each shown [`CursorKind`] is rasterised at most once per scale and cursor
/// set: a [`ReclaimCache`] keyed by kind keeps the converted [`CursorImage`]
/// so re-showing a kind reuses the image and only a scale or set change
/// re-rasterises. Cursor *motion* never touches the cache;
/// it moves the existing overlay.
///
/// Neither `Clone` nor `PartialEq`/`Eq` are derived: the cache holds a
/// pressure gauge and a diagnostics sink behind trait objects, which are
/// neither cloneable nor comparable, and cloning a live cache's charged
/// ledger would double-count its bytes.
#[derive(Debug)]
pub struct CursorController {
    registry: CursorRegistry,
    kind: CursorKind,
    shown: Option<CursorEpoch>,
    cache: ReclaimCache<CursorKind, CursorImage, CursorEpoch>,
}

impl CursorController {
    /// A controller for the built-in cursor set, before any cursor is shown,
    /// caching rasterised images in `cache`.
    ///
    /// The remembered kind starts at [`CursorKind::Arrow`]; nothing is drawn
    /// until the first [`refresh`](Self::refresh) installs an image at the
    /// compositor's current scale. `cache` is built by the embedder from the
    /// shared desktop cache policy ([`cursor_cache`]), wired to the real
    /// display backing size, the owning seat, and the process's live
    /// pressure gauge — this controller never invents that policy itself.
    #[must_use]
    pub fn new(cache: ReclaimCache<CursorKind, CursorImage, CursorEpoch>) -> Self {
        Self::with_registry(CursorRegistry::with_builtin(), cache)
    }

    /// A controller over a caller-provided `registry`, caching rasterised
    /// images in `cache` (see [`new`](Self::new)).
    #[must_use]
    pub const fn with_registry(
        registry: CursorRegistry,
        cache: ReclaimCache<CursorKind, CursorImage, CursorEpoch>,
    ) -> Self {
        Self {
            registry,
            kind: CursorKind::Arrow,
            shown: None,
            cache,
        }
    }

    /// The cursor sets this controller chooses artwork from.
    #[must_use]
    pub const fn registry(&self) -> &CursorRegistry {
        &self.registry
    }

    /// The [`CursorKind`] currently shown (the last one
    /// [`refresh`](Self::refresh) installed).
    #[must_use]
    pub const fn kind(&self) -> CursorKind {
        self.kind
    }

    /// Wipe every cached cursor image, so no rasterised pixel data from this
    /// seat's session survives it.
    ///
    /// Called when the seat this controller belongs to is lost or its
    /// session ends. The cache stays usable afterwards — a later refresh
    /// simply rebuilds what it needs — this only discards what was already
    /// rendered.
    pub fn teardown(&mut self) {
        self.cache.teardown();
    }

    /// Apply the current memory-pressure band's forced shrink to the cursor
    /// cache, returning the bytes released.
    ///
    /// The session calls this when the kernel wakes it with a deepened
    /// band, so the desktop gives its rasterised pixels back at the moment
    /// pressure rises rather than at whatever later frame happens to touch
    /// the cache. A band that demands nothing releases nothing.
    pub fn trim(&mut self) -> usize {
        self.cache.enforce_pressure()
    }

    /// Rasterised cursor images currently retained.
    #[must_use]
    pub fn cache_len(&self) -> usize {
        self.cache.len()
    }

    /// Bytes the cursor cache currently has charged: retained pixel data
    /// plus its own per-entry bookkeeping.
    #[must_use]
    pub fn cache_bytes(&self) -> usize {
        self.cache.charged_bytes()
    }

    /// The cursor cache's byte ledger and event counters, for diagnostics.
    #[must_use]
    pub fn cache_stats(&self) -> &CacheAccounting {
        self.cache.accounting()
    }

    /// Replace the cursor sets and immediately re-render the current kind at
    /// the router's pointer so a set swap is visible without waiting for the
    /// next interaction. Returns whether a new image was installed (no-op,
    /// returning `false`, when no cursor is currently shown).
    pub fn set_registry(
        &mut self,
        registry: CursorRegistry,
        router: &InputRouter,
        compositor: &mut Compositor,
    ) -> bool {
        self.registry = registry;
        if compositor.cursor_bounds().is_none() {
            return false;
        }
        self.install(self.kind, router.pointer(), compositor)
    }

    /// Apply the [`desired_cursor`] policy and bring the on-screen cursor up
    /// to date with the compositor's current scale and the active cursor set.
    /// Returns whether the displayed cursor changed.
    ///
    /// It re-rasterises and installs when the chosen kind, the output
    /// [`scale`](Compositor::scale), or the active cursor set differs from
    /// what is on screen; when nothing it depends on changed and a cursor is
    /// already shown it does no work and returns `false`. The pointer's
    /// *position* is updated separately with [`Compositor::move_cursor`].
    /// Fails closed: if the chosen kind cannot be rasterised, the current
    /// cursor is left untouched.
    pub fn refresh(&mut self, router: &InputRouter, compositor: &mut Compositor) -> bool {
        let kind = desired_cursor(router, compositor);
        let epoch = self.epoch(compositor);
        if kind == self.kind && self.shown == Some(epoch) && compositor.cursor_bounds().is_some() {
            return false;
        }
        self.install(kind, router.pointer(), compositor)
    }

    /// The cache epoch for the compositor's current output scale and the
    /// active cursor set.
    fn epoch(&self, compositor: &Compositor) -> CursorEpoch {
        (compositor.scale().percent(), self.registry.active_id())
    }

    /// Rasterise `kind` at the compositor's current scale and install it so
    /// its hotspot lands on `pointer`. Fails closed (leaving any current
    /// cursor untouched) if the cursor cannot be rasterised.
    fn install(&mut self, kind: CursorKind, pointer: Point, compositor: &mut Compositor) -> bool {
        let epoch = self.epoch(compositor);
        let registry = &self.registry;
        let Some(served) = self.cache.get_or_build(&epoch, kind, || {
            registry.active_cursor(kind).rasterise(epoch.0)
        }) else {
            return false;
        };
        let image = (*served).clone();
        compositor.set_cursor(image, pointer);
        self.kind = kind;
        self.shown = Some(epoch);
        true
    }
}
