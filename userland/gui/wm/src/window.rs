//! Windows: a placed surface with compositing attributes.

use rustos_theme::CursorKind;

use crate::color::{div255, Pixel};
use crate::corner::Corners;
use crate::geometry::{Point, Rect};
use crate::surface::Surface;

/// An opaque, compositor-minted window identifier.
///
/// Clients never construct a [`WindowId`]; the compositor returns one
/// from [`Compositor::add_window`] and the client names its window by
/// that token thereafter.
///
/// [`Compositor::add_window`]: crate::Compositor::add_window
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash, Ord, PartialOrd)]
pub struct WindowId(pub(crate) u64);

/// A window: a [`Surface`] placed at a screen [`Point`] with a
/// per-window opacity, corner style, and pointer-cursor hint.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Window {
    id: WindowId,
    origin: Point,
    surface: Surface,
    opacity: u8,
    corners: Corners,
    visible: bool,
    cursor: CursorKind,
}

impl Window {
    /// Build a fully-opaque, square-cornered, visible window from a
    /// surface placed at `origin`. The pointer-cursor hint defaults to the
    /// plain [`CursorKind::Arrow`]; a window whose content wants a
    /// different pointer (an editor's text I-beam, a control's hand) sets
    /// it through [`Compositor::set_window_cursor`].
    ///
    /// [`Compositor::set_window_cursor`]: crate::Compositor::set_window_cursor
    pub(crate) fn new(id: WindowId, origin: Point, surface: Surface) -> Self {
        Self {
            id,
            origin,
            surface,
            opacity: 255,
            corners: Corners::Square,
            visible: true,
            cursor: CursorKind::Arrow,
        }
    }

    /// This window's identifier.
    #[must_use]
    pub const fn id(&self) -> WindowId {
        self.id
    }

    /// Top-left screen position.
    #[must_use]
    pub const fn origin(&self) -> Point {
        self.origin
    }

    /// Per-window opacity (`255` opaque).
    #[must_use]
    pub const fn opacity(&self) -> u8 {
        self.opacity
    }

    /// Corner style.
    #[must_use]
    pub const fn corners(&self) -> Corners {
        self.corners
    }

    /// The pointer-cursor hint for this window's content: the
    /// [`CursorKind`] the compositor shows while the pointer rests over
    /// the window and no higher-priority interaction (such as a window
    /// move-grab) is in progress.
    #[must_use]
    pub const fn cursor_hint(&self) -> CursorKind {
        self.cursor
    }

    /// `true` if the window participates in composition.
    #[must_use]
    pub const fn is_visible(&self) -> bool {
        self.visible
    }

    /// Borrow the window's content surface.
    #[must_use]
    pub const fn surface(&self) -> &Surface {
        &self.surface
    }

    /// The window's screen rectangle (origin plus surface size).
    #[must_use]
    pub fn bounds(&self) -> Rect {
        Rect::new(
            self.origin.x,
            self.origin.y,
            self.surface.width(),
            self.surface.height(),
        )
    }

    /// The composited contribution of this window at screen `(x, y)`:
    /// the source pixel scaled by the combined opacity and rounded-corner
    /// coverage, or `None` when the point falls outside the surface or
    /// the window is hidden.
    pub(crate) fn sample(&self, x: i32, y: i32) -> Option<Pixel> {
        if !self.visible {
            return None;
        }
        let lx = u32::try_from(i64::from(x) - i64::from(self.origin.x)).ok()?;
        let ly = u32::try_from(i64::from(y) - i64::from(self.origin.y)).ok()?;
        self.sample_local(lx, ly)
    }

    /// The composited contribution of this window at *surface-local*
    /// `(lx, ly)`: the source pixel scaled by the combined opacity and
    /// rounded-corner coverage, or `None` outside the surface or when the
    /// window is hidden.
    ///
    /// This is the same per-pixel result as [`Self::sample`] addressed in
    /// the window's own coordinate space; the hardware-layer present path
    /// (`Compositor::present_accelerated`) uses it to bake a window into a
    /// premultiplied layer without re-deriving screen coordinates.
    pub(crate) fn sample_local(&self, lx: u32, ly: u32) -> Option<Pixel> {
        if !self.visible {
            return None;
        }
        let pixel = self.surface.get(lx, ly)?;
        let coverage = self
            .corners
            .coverage(lx, ly, self.surface.width(), self.surface.height());
        let factor = combine(self.opacity, coverage);
        Some(pixel.scale_alpha(factor))
    }

    pub(crate) fn set_origin(&mut self, origin: Point) {
        self.origin = origin;
    }

    pub(crate) fn set_opacity(&mut self, opacity: u8) {
        self.opacity = opacity;
    }

    pub(crate) fn set_corners(&mut self, corners: Corners) {
        self.corners = corners;
    }

    pub(crate) fn set_visible(&mut self, visible: bool) {
        self.visible = visible;
    }

    pub(crate) fn set_cursor_hint(&mut self, cursor: CursorKind) {
        self.cursor = cursor;
    }

    pub(crate) fn replace_surface(&mut self, surface: Surface) {
        self.surface = surface;
    }
}

/// Combine two `0..=255` factors as `a * b / 255` (shared `div255`).
fn combine(a: u8, b: u8) -> u8 {
    div255(u32::from(a) * u32::from(b))
}
