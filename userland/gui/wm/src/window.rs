//! Windows: a placed surface with compositing attributes.

use tairix_controls::{FrameInsets, WindowFrame};
use tairix_theme::{CursorKind, Theme};

use crate::color::{div255, Pixel};
use crate::corner::Corners;
use crate::geometry::{Point, Rect, Scale};
use crate::surface::Surface;
use crate::viewport::RootViewport;

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
    viewport: Option<RootViewport>,
    frame: Option<WindowFrame>,
    band: Option<FrameInsets>,
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
            viewport: None,
            frame: None,
            band: None,
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

    /// This window's window-manager-owned root-viewport scrollbars, if any.
    ///
    /// A window with a root viewport has the window manager compose its
    /// scrollbars as furniture around the client; a window without one is a
    /// plain client surface.
    #[must_use]
    pub const fn viewport(&self) -> Option<&RootViewport> {
        self.viewport.as_ref()
    }

    /// Borrow the window's content surface.
    #[must_use]
    pub const fn surface(&self) -> &Surface {
        &self.surface
    }

    /// Borrow the window's content surface mutably, for an in-place
    /// update of its pixels (the compositor marks the window's bounds
    /// dirty around the edit).
    pub(crate) fn surface_mut(&mut self) -> &mut Surface {
        &mut self.surface
    }

    /// The window's screen rectangle.
    ///
    /// For a plain window this is the origin plus the content surface size. For
    /// a *decorated* window it is the **outer** rectangle: the content surface
    /// grown by the frame band ([`FrameInsets`]) the window manager reserves
    /// for the title bar, borders, and resize edges, so the decoration lives
    /// around the client rather than over it.
    #[must_use]
    pub fn bounds(&self) -> Rect {
        let (w, h) = self.outer_size();
        Rect::new(self.origin.x, self.origin.y, w, h)
    }

    /// The window's outer size in physical pixels: the content surface grown by
    /// the reserved frame band when decorated, else the bare surface size.
    fn outer_size(&self) -> (u32, u32) {
        match self.band {
            Some(insets) => (
                self.surface
                    .width()
                    .saturating_add(insets.left)
                    .saturating_add(insets.right),
                self.surface
                    .height()
                    .saturating_add(insets.top)
                    .saturating_add(insets.bottom),
            ),
            None => (self.surface.width(), self.surface.height()),
        }
    }

    /// The screen rectangle the application content occupies: the inset client
    /// viewport for a decorated window, or the whole bounds for a plain one.
    ///
    /// The content surface is presented here and never overlaps the furniture
    /// band, so a decorated window's client never receives frame input.
    #[must_use]
    pub fn client_rect(&self) -> Rect {
        match self.band {
            Some(insets) => Rect::new(
                self.origin.x.saturating_add_unsigned(insets.left),
                self.origin.y.saturating_add_unsigned(insets.top),
                self.surface.width(),
                self.surface.height(),
            ),
            None => self.bounds(),
        }
    }

    /// This window's window-manager-owned decoration frame, if it is decorated.
    #[must_use]
    pub fn frame(&self) -> Option<&WindowFrame> {
        self.frame.as_ref()
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

    /// The composited contribution of this window at *window-local*
    /// `(lx, ly)` (origin the outer top-left): the source pixel scaled by the
    /// combined opacity and rounded-corner coverage, or `None` outside the
    /// content, in the reserved frame band, or when the window is hidden.
    ///
    /// This is the same per-pixel result as [`Self::sample`] addressed in
    /// the window's own coordinate space; the hardware-layer present path
    /// (`Compositor::present_accelerated`) uses it to bake a window into a
    /// premultiplied layer without re-deriving screen coordinates.
    pub(crate) fn sample_local(&self, lx: u32, ly: u32) -> Option<Pixel> {
        if !self.visible {
            return None;
        }
        // Map outer-window-local coordinates to content-surface coordinates. A
        // point in the reserved frame band yields no client pixel — the window
        // manager paints the furniture there separately — and the content sits
        // inset at the client origin.
        let (sx, sy) = self.to_content(lx, ly)?;
        if self.clipped_by_furniture(sx, sy) {
            return None;
        }
        let pixel = self.surface.get(sx, sy)?;
        // A decorated window's rounded corners belong to the frame rim, not the
        // rectangular client; an undecorated window rounds its own content.
        let coverage = if self.band.is_some() {
            255
        } else {
            self.corners
                .coverage(sx, sy, self.surface.width(), self.surface.height())
        };
        let factor = combine(self.opacity, coverage);
        Some(pixel.scale_alpha(factor))
    }

    /// Translate outer-window-local `(lx, ly)` into content-surface
    /// coordinates, or `None` when the point falls in the reserved frame band
    /// (outside the client). A plain window maps one-to-one.
    fn to_content(&self, lx: u32, ly: u32) -> Option<(u32, u32)> {
        let Some(insets) = self.band else {
            return Some((lx, ly));
        };
        let sx = lx.checked_sub(insets.left)?;
        let sy = ly.checked_sub(insets.top)?;
        if sx >= self.surface.width() || sy >= self.surface.height() {
            return None;
        }
        Some((sx, sy))
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

    pub(crate) fn set_viewport(&mut self, viewport: Option<RootViewport>) {
        self.viewport = viewport;
    }

    pub(crate) fn viewport_mut(&mut self) -> Option<&mut RootViewport> {
        self.viewport.as_mut()
    }

    /// Attach or replace this window's decoration frame, resolving its band for
    /// the given output `scale`/`theme` so [`Self::bounds`] reflects the outer
    /// rectangle immediately. Passing `None` removes the decoration.
    pub(crate) fn set_frame(&mut self, frame: Option<WindowFrame>, scale: Scale, theme: &Theme) {
        self.frame = frame;
        self.refresh_band(scale, theme);
    }

    /// Re-resolve the decoration band for a new output `scale`/`theme`, so a
    /// runtime DPI or theme change re-sizes the reserved furniture band.
    pub(crate) fn refresh_band(&mut self, scale: Scale, theme: &Theme) {
        self.band = self.frame.as_ref().map(|f| f.insets(scale, theme));
    }

    /// `true` when surface-local `(lx, ly)` falls in a reserved scrollbar
    /// gutter, so the client pixel there is clipped away (the furniture, not
    /// the client, owns that strip).
    fn clipped_by_furniture(&self, lx: u32, ly: u32) -> bool {
        let Some(viewport) = self.viewport else {
            return false;
        };
        let local = Rect::new(0, 0, self.surface.width(), self.surface.height());
        let client = viewport.layout(local).client;
        lx >= client.width || ly >= client.height
    }
}

/// Combine two `0..=255` factors as `a * b / 255` (shared `div255`).
fn combine(a: u8, b: u8) -> u8 {
    div255(u32::from(a) * u32::from(b))
}
