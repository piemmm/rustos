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
//! has no hidden authority (`AGENTS.md` §2.11).
//!
//! # Applying the choice
//!
//! [`CursorController`] ties the policy to the artwork. It owns the active
//! [`CursorRegistry`] and remembers which [`CursorKind`] is on screen and at
//! what density it was rasterised. It does **not** own the scale: the desktop
//! density belongs to the output, so the controller reads it from the
//! [`Compositor`] ([`Compositor::scale`]) every time it installs a cursor
//! (`AGENTS.md` §10 / §2.2). [`CursorController::refresh`] runs the policy and
//! re-rasterises only when something the cursor depends on actually changed —
//! the chosen kind, the active cursor set, or the output scale — installing
//! the result through [`Compositor::set_cursor`]. A runtime DPI change is
//! therefore [`Compositor::set_scale`] followed by one `refresh`. Pointer
//! *motion* is not its job — the caller moves the existing overlay with
//! [`Compositor::move_cursor`]; the controller switches the *shape*.
//!
//! Rasterisation can fail (a degenerate cursor or scale yields no image,
//! `AGENTS.md` §2.9). The controller fails closed: it leaves the current
//! cursor in place and reports that nothing changed rather than blanking the
//! pointer or panicking.

use rustos_cursor::{CursorImage, CursorRegistry, CursorSetId};
use rustos_raster::RasterCache;
use rustos_theme::CursorKind;

use crate::geometry::Point;
use crate::input::InputRouter;
use crate::window::Window;
use crate::Compositor;

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
/// cursor-set swap moves the epoch on and invalidates every cached image
/// (`AGENTS.md` §10).
type CursorEpoch = (u32, CursorSetId);

/// Drives the on-screen pointer shape from interaction state.
///
/// Holds the active [`CursorRegistry`] (the replaceable cursor sets) and the
/// [`CursorKind`] currently shown, paired with the cache epoch (scale and
/// cursor-set id) it was rasterised for. The density is **not** stored here —
/// it belongs to the output, so [`refresh`](Self::refresh) reads it from the
/// [`Compositor`] (`AGENTS.md` §10 / §2.2) and applies the [`desired_cursor`]
/// policy.
///
/// Each shown [`CursorKind`] is rasterised at most once per scale and cursor
/// set: a [`RasterCache`] keyed by kind keeps the converted [`CursorImage`]
/// so re-showing a kind reuses the image and only a scale or set change
/// re-rasterises (`AGENTS.md` §10). Cursor *motion* never touches the cache;
/// it moves the existing overlay.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CursorController {
    registry: CursorRegistry,
    kind: CursorKind,
    shown: Option<CursorEpoch>,
    cache: RasterCache<CursorKind, CursorImage, CursorEpoch>,
}

impl CursorController {
    /// A controller for the built-in cursor set, before any cursor is shown.
    /// The remembered kind starts at [`CursorKind::Arrow`]; nothing is drawn
    /// until the first [`refresh`](Self::refresh) installs an image at the
    /// compositor's current scale.
    #[must_use]
    pub fn new() -> Self {
        Self::with_registry(CursorRegistry::with_builtin())
    }

    /// A controller over a caller-provided `registry`.
    #[must_use]
    pub fn with_registry(registry: CursorRegistry) -> Self {
        Self {
            registry,
            kind: CursorKind::Arrow,
            shown: None,
            cache: RasterCache::new(),
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
    /// cursor is left untouched (`AGENTS.md` §2.9).
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
    /// cursor untouched) if the cursor cannot be rasterised (`AGENTS.md`
    /// §2.9).
    fn install(&mut self, kind: CursorKind, pointer: Point, compositor: &mut Compositor) -> bool {
        let epoch = self.epoch(compositor);
        let registry = &self.registry;
        let Some(image) = self
            .cache
            .get_or_render(&epoch, kind, || {
                registry.active_cursor(kind).rasterise(epoch.0)
            })
            .cloned()
        else {
            return false;
        };
        compositor.set_cursor(image, pointer);
        self.kind = kind;
        self.shown = Some(epoch);
        true
    }
}

impl Default for CursorController {
    /// A controller over the built-in cursor set ([`CursorController::new`]).
    fn default() -> Self {
        Self::new()
    }
}
