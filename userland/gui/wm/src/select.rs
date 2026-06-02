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
//! [`CursorRegistry`] and the desktop [`Scale`], and remembers which
//! [`CursorKind`] is currently on screen. [`CursorController::refresh`] runs
//! the policy and, only when the chosen kind actually changes, rasterises the
//! active set's cursor for that kind at the current scale and installs it
//! through [`Compositor::set_cursor`]. Pointer *motion* is not its job — the
//! caller moves the existing overlay with [`Compositor::move_cursor`]; the
//! controller switches the *shape*.
//!
//! Rasterisation can fail (a degenerate cursor or scale yields no image,
//! `AGENTS.md` §2.9). The controller fails closed: it leaves the current
//! cursor in place and reports that nothing changed rather than blanking the
//! pointer or panicking.

use rustos_cursor::CursorRegistry;
use rustos_geometry::Scale;
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

/// Drives the on-screen pointer shape from interaction state.
///
/// Holds the active [`CursorRegistry`] (the replaceable cursor sets), the
/// desktop [`Scale`] cursors rasterise at, and the [`CursorKind`] currently
/// shown. [`refresh`](Self::refresh) applies the [`desired_cursor`] policy
/// to a compositor.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CursorController {
    registry: CursorRegistry,
    scale: Scale,
    kind: CursorKind,
}

impl CursorController {
    /// A controller for the built-in cursor set at `scale`, before any
    /// cursor is shown. The remembered kind starts at [`CursorKind::Arrow`];
    /// nothing is drawn until the first [`refresh`](Self::refresh) installs
    /// an image.
    #[must_use]
    pub fn new(scale: Scale) -> Self {
        Self::with_registry(CursorRegistry::with_builtin(), scale)
    }

    /// A controller over a caller-provided `registry` at `scale`.
    #[must_use]
    pub fn with_registry(registry: CursorRegistry, scale: Scale) -> Self {
        Self {
            registry,
            scale,
            kind: CursorKind::Arrow,
        }
    }

    /// The cursor sets this controller chooses artwork from.
    #[must_use]
    pub const fn registry(&self) -> &CursorRegistry {
        &self.registry
    }

    /// The scale cursors rasterise at.
    #[must_use]
    pub const fn scale(&self) -> Scale {
        self.scale
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

    /// Change the desktop scale and re-render the current kind at the
    /// router's pointer at the new density. Returns whether a new image was
    /// installed (no-op, returning `false`, when no cursor is currently
    /// shown).
    pub fn set_scale(
        &mut self,
        scale: Scale,
        router: &InputRouter,
        compositor: &mut Compositor,
    ) -> bool {
        self.scale = scale;
        if compositor.cursor_bounds().is_none() {
            return false;
        }
        self.install(self.kind, router.pointer(), compositor)
    }

    /// Apply the [`desired_cursor`] policy: if the chosen kind differs from
    /// the one on screen, rasterise it and install it through
    /// [`Compositor::set_cursor`] at the router's pointer. Returns whether
    /// the displayed cursor changed.
    ///
    /// When the kind is unchanged and a cursor is already shown this does no
    /// work and returns `false`; the pointer's *position* is updated
    /// separately with [`Compositor::move_cursor`]. Fails closed: if the
    /// chosen kind cannot be rasterised, the current cursor is left
    /// untouched.
    pub fn refresh(&mut self, router: &InputRouter, compositor: &mut Compositor) -> bool {
        let kind = desired_cursor(router, compositor);
        if kind == self.kind && compositor.cursor_bounds().is_some() {
            return false;
        }
        self.install(kind, router.pointer(), compositor)
    }

    /// Rasterise `kind` at the current scale and install it so its hotspot
    /// lands on `pointer`. Fails closed (leaving any current cursor
    /// untouched) if the cursor cannot be rasterised (`AGENTS.md` §2.9).
    fn install(&mut self, kind: CursorKind, pointer: Point, compositor: &mut Compositor) -> bool {
        let Some(image) = self
            .registry
            .active_cursor(kind)
            .rasterise(self.scale.percent())
        else {
            return false;
        };
        compositor.set_cursor(image, pointer);
        self.kind = kind;
        true
    }
}
