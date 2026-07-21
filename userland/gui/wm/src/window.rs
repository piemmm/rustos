//! Windows: a placed surface with compositing attributes.

use tairix_controls::{
    FrameInsets, ResizeGrabber, TitleBarEvent, WindowActivationState, WindowFrame, WindowSizeState,
};
use tairix_font::BitmapFont;
use tairix_input::{InputEvent, Key};
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
    decoration: Option<Surface>,
    /// Whether the window is restored or maximized. Meaningful only for a
    /// decorated, resizable window; a plain window is always `Restored`.
    size_state: WindowSizeState,
    /// The outer rectangle to return to when a maximized window is
    /// restored — captured at the moment it was maximized, so a
    /// maximize/restore round-trip lands back exactly where it started.
    restore_outer: Option<Rect>,
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
            decoration: None,
            size_state: WindowSizeState::Restored,
            restore_outer: None,
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

    /// Whether the window is restored or maximized.
    #[must_use]
    pub const fn size_state(&self) -> WindowSizeState {
        self.size_state
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
        match self.band {
            Some(_) => self.sample_decorated(lx, ly),
            None => self.sample_plain(lx, ly),
        }
    }

    /// The composited contribution of an *undecorated* window at
    /// window-local `(lx, ly)`: the content pixel scaled by opacity and this
    /// window's own rounded-corner coverage (the client is the whole window).
    fn sample_plain(&self, lx: u32, ly: u32) -> Option<Pixel> {
        if self.clipped_by_furniture(lx, ly) {
            return None;
        }
        let pixel = self.surface.get(lx, ly)?;
        let coverage = self
            .corners
            .coverage(lx, ly, self.surface.width(), self.surface.height());
        Some(pixel.scale_alpha(combine(self.opacity, coverage)))
    }

    /// The composited contribution of a *decorated* window at outer-local
    /// `(lx, ly)`: the inset client content where the point maps into the
    /// content surface, otherwise the pre-rendered window-manager furniture
    /// ([`Self::render_decoration`]) in the reserved band. The rounded corners
    /// belong to the frame rim (baked into the decoration as partial alpha),
    /// not the rectangular client, so the client is sampled at full coverage.
    fn sample_decorated(&self, lx: u32, ly: u32) -> Option<Pixel> {
        if let Some((sx, sy)) = self.to_content(lx, ly) {
            if self.clipped_by_furniture(sx, sy) {
                return None;
            }
            let pixel = self.surface.get(sx, sy)?;
            return Some(pixel.scale_alpha(self.opacity));
        }
        let pixel = self.decoration.as_ref()?.get(lx, ly)?;
        Some(pixel.scale_alpha(self.opacity))
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
    /// rectangle immediately and painting the furniture chrome. Passing `None`
    /// removes the decoration.
    pub(crate) fn set_frame(&mut self, frame: Option<WindowFrame>, scale: Scale, theme: &Theme) {
        self.frame = frame;
        self.refresh_band(scale, theme);
    }

    /// Re-resolve the decoration band and repaint the furniture chrome for a
    /// new output `scale`/`theme`, so a runtime DPI or theme change re-sizes
    /// the reserved band and re-rasterises the decoration under the new
    /// density/palette.
    pub(crate) fn refresh_band(&mut self, scale: Scale, theme: &Theme) {
        self.band = self.frame.as_ref().map(|f| f.insets(scale, theme));
        self.render_decoration(scale, theme);
    }

    /// Set the decorated window's activation, repainting the frame rim, title,
    /// and controls so an active/inactive change is reflected immediately.
    /// Returns `false` for an undecorated window (nothing to activate).
    ///
    /// Attention requests are a separate client-driven state, so an active
    /// window becomes [`WindowActivationState::Active`] and an inactive one
    /// [`WindowActivationState::Inactive`]; a window that has raised an
    /// attention request keeps it while inactive rather than being forced
    /// quiet.
    pub(crate) fn set_frame_active(&mut self, active: bool, scale: Scale, theme: &Theme) -> bool {
        let Some(frame) = self.frame.as_mut() else {
            return false;
        };
        let mut furniture = frame.furniture();
        let next = if active {
            WindowActivationState::Active
        } else if furniture.activation == WindowActivationState::AttentionRequested {
            WindowActivationState::AttentionRequested
        } else {
            WindowActivationState::Inactive
        };
        if furniture.activation == next {
            return true;
        }
        furniture.activation = next;
        frame.set_furniture(furniture);
        self.render_decoration(scale, theme);
        true
    }

    /// Set the decorated window's title, repainting the title bar. Returns
    /// `false` for an undecorated window (there is no title bar to label).
    pub(crate) fn set_frame_title(&mut self, title: &str, scale: Scale, theme: &Theme) -> bool {
        let Some(frame) = self.frame.as_mut() else {
            return false;
        };
        frame.title_bar_mut().set_title(title);
        self.render_decoration(scale, theme);
        true
    }

    /// Feed a pointer `event` to this window's decoration furniture (the title
    /// bar and its command controls), repainting the furniture chrome so hover
    /// and press states show, and return the typed [`TitleBarEvent`] it
    /// produced. Returns `None` (and repaints nothing) for an undecorated
    /// window — it has no furniture to receive the event.
    pub(crate) fn on_frame_pointer(
        &mut self,
        event: &InputEvent,
        scale: Scale,
        theme: &Theme,
    ) -> Option<TitleBarEvent> {
        let bounds = self.bounds();
        let event = {
            let frame = self.frame.as_mut()?;
            let title_rect = frame.layout(bounds, scale, theme).title_bar;
            frame
                .title_bar_mut()
                .on_pointer(event, title_rect, scale, theme)
        };
        self.render_decoration(scale, theme);
        event
    }

    /// Feed a key `key` to this window's decoration furniture (the title bar's
    /// command controls: Space/Enter activate the focused control, the arrows
    /// move focus between them), repainting the chrome, and return the typed
    /// [`TitleBarEvent`] it produced. Returns `None` for an undecorated window.
    pub(crate) fn on_frame_key(
        &mut self,
        key: Key,
        scale: Scale,
        theme: &Theme,
    ) -> Option<TitleBarEvent> {
        let event = self.frame.as_mut()?.title_bar_mut().on_key(key);
        self.render_decoration(scale, theme);
        event
    }

    /// Toggle a decorated, resizable window between restored and maximized,
    /// resizing it to `work_area` (the session work rectangle) on maximize
    /// and back to the geometry it had when maximized on restore. Returns
    /// the new size state and the resulting client rectangle, or `None`
    /// (changing nothing) for an undecorated window, a non-resizable one,
    /// or when the resize itself fails closed.
    ///
    /// The frame's furniture size is updated in step, so the size-toggle
    /// control shows the *next* action (Restore while maximized, Maximize
    /// while restored) and the decoration is repainted.
    pub(crate) fn toggle_size(
        &mut self,
        work_area: Rect,
        scale: Scale,
        theme: &Theme,
    ) -> Option<(WindowSizeState, Rect)> {
        let furniture = self.frame.as_ref()?.furniture();
        if !furniture.resizable {
            return None;
        }
        let (target, next_state) = match self.size_state {
            WindowSizeState::Restored => (work_area, WindowSizeState::Maximized),
            WindowSizeState::Maximized => (
                self.restore_outer.unwrap_or_else(|| self.bounds()),
                WindowSizeState::Restored,
            ),
        };
        if self.size_state == WindowSizeState::Restored {
            self.restore_outer = Some(self.bounds());
        }
        if !self.resize_to_outer(target, scale, theme) {
            // Fail closed: nothing moved, so do not record a state change or
            // strand the saved restore geometry.
            if next_state == WindowSizeState::Maximized {
                self.restore_outer = None;
            }
            return None;
        }
        self.size_state = next_state;
        if next_state == WindowSizeState::Restored {
            self.restore_outer = None;
        }
        if let Some(frame) = self.frame.as_mut() {
            let mut furniture = frame.furniture();
            furniture.size = next_state;
            frame.set_furniture(furniture);
            self.render_decoration(scale, theme);
        }
        Some((next_state, self.client_rect()))
    }

    /// Resize this window so its outer rectangle becomes `new_outer`: the
    /// content surface is reallocated to the implied client size (the outer
    /// extent minus the reserved frame band), the existing pixels are
    /// preserved where they still fit (a raw copy, not an alpha blend, so a
    /// live resize keeps the current content until the client re-renders), the
    /// origin follows the new top-left, and the decoration is repainted at the
    /// new size. Returns `false` (changing nothing) when the implied client
    /// size is empty or its buffer cannot be allocated (fail closed).
    pub(crate) fn resize_to_outer(&mut self, new_outer: Rect, scale: Scale, theme: &Theme) -> bool {
        let (band_w, band_h) = match self.band {
            Some(insets) => (
                insets.left.saturating_add(insets.right),
                insets.top.saturating_add(insets.bottom),
            ),
            None => (0, 0),
        };
        let client_w = new_outer.width.saturating_sub(band_w);
        let client_h = new_outer.height.saturating_sub(band_h);
        if !self.resize_surface_preserving(client_w, client_h) {
            return false;
        }
        self.origin = new_outer.origin;
        self.refresh_band(scale, theme);
        true
    }

    /// Reallocate this window's content surface to the given client size in
    /// place — the origin unchanged — preserving existing pixels where they
    /// still fit and repainting the decoration at the new size. This is the
    /// client-driven counterpart to [`resize_to_outer`](Self::resize_to_outer)
    /// (which sizes from an outer rectangle and moves the origin): the window
    /// channel's `Resize` hands the session a new *client* content size, so
    /// the compositor sizes the content directly. Returns `false` (changing
    /// nothing) when the size is empty or its buffer cannot be allocated.
    pub(crate) fn resize_client(
        &mut self,
        client_w: u32,
        client_h: u32,
        scale: Scale,
        theme: &Theme,
    ) -> bool {
        if !self.resize_surface_preserving(client_w, client_h) {
            return false;
        }
        self.refresh_band(scale, theme);
        true
    }

    /// Replace the content surface with one of `client_w` × `client_h`,
    /// copying the existing pixels that still fit (a raw copy, not an alpha
    /// blend, so a live resize keeps the current content until the client
    /// re-renders). Returns `false` (leaving the surface untouched) for an
    /// empty size or an allocation failure — the one definition both
    /// [`resize_to_outer`](Self::resize_to_outer) and
    /// [`resize_client`](Self::resize_client) share, so the resize copy is
    /// never restated.
    fn resize_surface_preserving(&mut self, client_w: u32, client_h: u32) -> bool {
        if client_w == 0 || client_h == 0 {
            return false;
        }
        let Some(mut resized) = Surface::new(client_w, client_h) else {
            return false;
        };
        let keep_w = client_w.min(self.surface.width());
        let keep_h = client_h.min(self.surface.height());
        for y in 0..keep_h {
            for x in 0..keep_w {
                if let Some(pixel) = self.surface.get(x, y) {
                    resized.set(x, y, pixel);
                }
            }
        }
        self.surface = resized;
        true
    }

    /// Repaint the furniture chrome into a fresh outer-sized decoration
    /// surface: the [`WindowFrame`] rim, body, title bar, and command controls,
    /// plus a corner [`ResizeGrabber`] on a resizable window. The client region
    /// of the surface is never sampled (the compositor draws the content
    /// there), and the rounded rim corners stay transparent so the desktop
    /// shows through. An undecorated window — or an allocation that fails —
    /// clears the decoration and falls back to the background band.
    fn render_decoration(&mut self, scale: Scale, theme: &Theme) {
        let Some(frame) = self.frame.as_ref() else {
            self.decoration = None;
            return;
        };
        let (ow, oh) = self.outer_size();
        let Some(mut surface) = Surface::new(ow, oh) else {
            self.decoration = None;
            return;
        };
        let outer = Rect::new(0, 0, ow, oh);
        // The title-bar text renders at the theme's logical font size scaled to
        // physical pixels, like every other desktop length, rather than a size
        // hard-coded here.
        let font =
            BitmapFont::with_pixel_height(scale.scale_length(u32::from(theme.fonts().ui.size_px)));
        frame.render(&mut surface, outer, scale, theme, font);

        let furniture = frame.furniture();
        if furniture.resizable {
            let extent = scale
                .scale_length(theme.metrics().resize_grabber_extent)
                .max(1)
                .min(ow)
                .min(oh);
            let mut grabber = ResizeGrabber::new();
            grabber.set_active_frame(furniture.activation != WindowActivationState::Inactive);
            let gx = i32::try_from(ow.saturating_sub(extent)).unwrap_or(0);
            let gy = i32::try_from(oh.saturating_sub(extent)).unwrap_or(0);
            grabber.render(
                &mut surface,
                Rect::new(gx, gy, extent, extent),
                scale,
                theme,
            );
        }

        self.decoration = Some(surface);
    }

    /// The reserved furniture bands in screen coordinates — the top (title),
    /// bottom, left, and right strips around the client — for a decorated
    /// window, or four empty rectangles for an undecorated one.
    ///
    /// A furniture-only change (focus flip, title edit, control state) marks
    /// just these bands dirty, so the client area is never needlessly
    /// recomposited (damage stays confined to the furniture).
    pub(crate) fn furniture_bands(&self) -> [Rect; 4] {
        let Some(insets) = self.band else {
            return [Rect::EMPTY; 4];
        };
        let outer = self.bounds();
        let client = self.client_rect();
        let top = Rect::new(outer.left(), outer.top(), outer.width, insets.top);
        let bottom = Rect::new(outer.left(), client.bottom(), outer.width, insets.bottom);
        let left = Rect::new(outer.left(), client.top(), insets.left, client.height);
        let right = Rect::new(client.right(), client.top(), insets.right, client.height);
        [top, bottom, left, right]
    }

    /// The top (title-bar) furniture band in screen coordinates for a decorated
    /// window, or [`Rect::EMPTY`] for an undecorated one — the region a title
    /// change repaints.
    pub(crate) fn title_band(&self) -> Rect {
        match self.band {
            Some(insets) => {
                let outer = self.bounds();
                Rect::new(outer.left(), outer.top(), outer.width, insets.top)
            }
            None => Rect::EMPTY,
        }
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
