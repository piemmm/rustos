//! The software compositor.
//!
//! A [`Compositor`] owns a stack of [`Window`]s (bottom-to-top
//! z-order), a screen-sized back buffer, and the [`DamageRegion`] that
//! records what changed since the last frame. [`Compositor::composite`]
//! recomputes only the damaged pixels — blending each covering window
//! *over* the opaque background through [`Pixel::over`] — and encodes
//! them into a scan-out byte frame laid out for the active
//! [`DisplayMode`]. [`Compositor::present`] hands that frame to a
//! [`Display`] driver.
//!
//! All compositing happens here, in user space; the kernel only ships
//! framebuffer access through a capability. GPU
//! acceleration, when a driver exposes it, replaces this software path
//! behind the same public surface; today the software path is the
//! fallback every platform always has.

use alloc::vec;
use alloc::vec::Vec;

use rustos_abi::driver::display::{
    AccelCaps, AccelLayer, AcceleratedDisplay, DamageRect, Display, DisplayFormat, DisplayMode,
};
use rustos_abi::DriverError;

use rustos_cursor::CursorImage;
use rustos_theme::CursorKind;

use crate::color::{Color, Pixel};
use crate::corner::Corners;
use crate::cursor::CursorLayer;
use crate::damage::DamageRegion;
use crate::geometry::{Point, Rect, Scale};
use crate::surface::Surface;
use crate::window::{Window, WindowId};

/// Scan-out channel order for a supported [`DisplayFormat`].
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
enum ChannelOrder {
    Rgba,
    Bgra,
}

/// A software compositing window manager surface.
///
/// The compositor owns its output's display density as a [`Scale`]: the
/// monitor it scans out to is a single output with one DPI, and the desktop's
/// logical lengths become physical pixels through that one factor. It is the single source of truth for this output's scale — the
/// cursor controller, the taskbar presenter, and apps all *read* it rather
/// than keeping a copy. A multi-monitor desktop is a set of such
/// outputs, each carrying its own scale; a window's effective density is the
/// scale of the output it is on ([`window_scale`](Self::window_scale)).
pub struct Compositor {
    mode: DisplayMode,
    scale: Scale,
    background: Color,
    order: ChannelOrder,
    windows: Vec<Window>,
    cursor: Option<CursorLayer>,
    back: Surface,
    frame: Vec<u8>,
    damage: DamageRegion,
    next_id: u64,
}

impl Compositor {
    /// Create a compositor for the given display `mode`, clearing the
    /// screen to an opaque `background`.
    ///
    /// The background alpha is forced to opaque: the root surface has
    /// nothing behind it, so the composited screen is always fully
    /// opaque and its premultiplied pixels equal their straight-alpha
    /// form on scan-out.
    ///
    /// Returns `None` if `mode` describes a surface too large to
    /// allocate or whose stride cannot hold one scanline, failing
    /// closed rather than panicking.
    #[must_use]
    pub fn new(mode: DisplayMode, background: Color) -> Option<Self> {
        let order = channel_order(mode.format)?;
        let background = Color {
            a: 255,
            ..background
        };
        let back = Surface::filled(mode.width_px, mode.height_px, background.premultiply())?;
        let frame = scanout_frame(&mode)?;
        let mut compositor = Self {
            mode,
            scale: Scale::ONE,
            background,
            order,
            windows: Vec::new(),
            cursor: None,
            back,
            frame,
            damage: DamageRegion::new(),
            next_id: 1,
        };
        compositor.damage.add(compositor.screen_rect());
        Some(compositor)
    }

    /// The active display mode.
    #[must_use]
    pub const fn mode(&self) -> DisplayMode {
        self.mode
    }

    /// The whole-screen rectangle.
    #[must_use]
    pub fn screen_rect(&self) -> Rect {
        Rect::new(0, 0, self.mode.width_px, self.mode.height_px)
    }

    /// This output's display density.
    ///
    /// The compositor owns the scale for the monitor it scans out to; the
    /// cursor controller, the taskbar presenter, and apps read it here rather
    /// than holding their own copy, so the desktop has exactly one place a
    /// monitor's density lives.
    #[must_use]
    pub const fn scale(&self) -> Scale {
        self.scale
    }

    /// Set this output's display density, returning whether it changed.
    ///
    /// A runtime DPI change is one call here: the whole screen is marked dirty
    /// so the next composite re-rasterises every window at the new density,
    /// and the embedder refreshes the scale-dependent overlays it owns (the
    /// cursor, via [`CursorController::refresh`](crate::CursorController::refresh),
    /// and the taskbar, by re-presenting it). Setting the scale already in
    /// effect changes nothing and returns `false`, so the caller can skip the
    /// refresh.
    pub fn set_scale(&mut self, scale: Scale) -> bool {
        if scale == self.scale {
            return false;
        }
        self.scale = scale;
        self.damage.add(self.screen_rect());
        true
    }

    /// The display density of the output the window named by `id` is on, or
    /// `None` for an unknown id.
    ///
    /// This is the read-only query an app uses when it must size something in
    /// physical pixels: picking the desktop density is the compositor's job,
    /// not the app's, so an app observes its window's scale here but never sets
    /// it. With a single output it is this compositor's
    /// [`scale`](Self::scale); a multi-monitor compositor returns the scale of
    /// the output the window currently sits on.
    #[must_use]
    pub fn window_scale(&self, id: WindowId) -> Option<Scale> {
        self.window(id).map(|_| self.scale)
    }

    /// Add `surface` as the top-most window at `origin`, returning its
    /// identifier. The new window's bounds are marked dirty.
    pub fn add_window(&mut self, origin: Point, surface: Surface) -> WindowId {
        let id = WindowId(self.next_id);
        self.next_id += 1;
        let window = Window::new(id, origin, surface);
        self.damage.add(window.bounds());
        self.windows.push(window);
        id
    }

    /// Borrow a window by id.
    #[must_use]
    pub fn window(&self, id: WindowId) -> Option<&Window> {
        self.windows.iter().find(|w| w.id() == id)
    }

    /// The top-most visible window whose bounds contain `point`, or
    /// `None` when the point lands on the desktop background.
    ///
    /// Hit-testing walks the z-order from the top down and uses each
    /// window's rectangular [`bounds`](Window::bounds); rounded corners
    /// are a cosmetic compositing effect and do not
    /// carve holes out of a window's input region.
    #[must_use]
    pub fn window_at(&self, point: Point) -> Option<WindowId> {
        self.windows
            .iter()
            .rev()
            .find(|w| w.is_visible() && w.bounds().contains(point))
            .map(Window::id)
    }

    /// Move a window to a new screen position; both the old and new
    /// covered rectangles are marked dirty.
    pub fn move_window(&mut self, id: WindowId, origin: Point) -> bool {
        self.mutate(id, |w| {
            w.set_origin(origin);
        })
    }

    /// Set a window's opacity (`255` opaque); its bounds are marked
    /// dirty.
    pub fn set_opacity(&mut self, id: WindowId, opacity: u8) -> bool {
        self.mutate(id, |w| w.set_opacity(opacity))
    }

    /// Set a window's corner style; its bounds are marked dirty.
    pub fn set_corners(&mut self, id: WindowId, corners: Corners) -> bool {
        self.mutate(id, |w| w.set_corners(corners))
    }

    /// Show or hide a window; its bounds are marked dirty.
    pub fn set_visible(&mut self, id: WindowId, visible: bool) -> bool {
        self.mutate(id, |w| w.set_visible(visible))
    }

    /// Replace a window's content surface; the union of the old and new
    /// bounds is marked dirty.
    pub fn set_surface(&mut self, id: WindowId, surface: Surface) -> bool {
        self.mutate(id, |w| w.replace_surface(surface))
    }

    /// Raise a window to the top of the z-order; its bounds are marked
    /// dirty.
    pub fn raise(&mut self, id: WindowId) -> bool {
        let Some(index) = self.windows.iter().position(|w| w.id() == id) else {
            return false;
        };
        let window = self.windows.remove(index);
        self.damage.add(window.bounds());
        self.windows.push(window);
        true
    }

    /// Remove a window; its last bounds are marked dirty.
    pub fn remove(&mut self, id: WindowId) -> bool {
        let Some(index) = self.windows.iter().position(|w| w.id() == id) else {
            return false;
        };
        let window = self.windows.remove(index);
        self.damage.add(window.bounds());
        true
    }

    /// Number of windows in the stack.
    #[must_use]
    pub fn window_count(&self) -> usize {
        self.windows.len()
    }

    /// Set the pointer-cursor hint for the window named by `id` — the
    /// [`CursorKind`] shown while the pointer rests over the window
    /// (`crate::select`). Returns `false` for an unknown id.
    ///
    /// The hint is window state, not pixels: changing it does not redraw
    /// the window (the cursor is the separate top overlay), so no damage
    /// is marked. The displayed pointer updates when the selection policy
    /// next runs over this window.
    pub fn set_window_cursor(&mut self, id: WindowId, kind: CursorKind) -> bool {
        let Some(window) = self.windows.iter_mut().find(|w| w.id() == id) else {
            return false;
        };
        window.set_cursor_hint(kind);
        true
    }

    /// The pointer-cursor hint of the window named by `id`, or `None` for
    /// an unknown id.
    #[must_use]
    pub fn window_cursor(&self, id: WindowId) -> Option<CursorKind> {
        self.window(id).map(Window::cursor_hint)
    }

    /// Show `image` as the pointer cursor with its hotspot at `pointer`,
    /// replacing any current cursor. The old and new covered rectangles are
    /// marked dirty so the overlay is recomposited in place.
    ///
    /// The artwork comes from `lib/cursor` (a scalable, colourful, vector
    /// cursor rasterised at the display scale); the compositor only places
    /// and blends it.
    pub fn set_cursor(&mut self, image: CursorImage, pointer: Point) {
        if let Some(cursor) = &self.cursor {
            self.damage.add(cursor.bounds());
        }
        let cursor = CursorLayer::new(image, pointer);
        self.damage.add(cursor.bounds());
        self.cursor = Some(cursor);
    }

    /// Move the pointer cursor so its hotspot sits at `pointer`, marking the
    /// old and new covered rectangles dirty. Returns `false` when no cursor
    /// is shown.
    pub fn move_cursor(&mut self, pointer: Point) -> bool {
        let Some(cursor) = &mut self.cursor else {
            return false;
        };
        let before = cursor.bounds();
        cursor.set_pointer(pointer);
        let after = cursor.bounds();
        self.damage.add(before);
        self.damage.add(after);
        true
    }

    /// Hide the pointer cursor, marking its covered rectangle dirty so the
    /// pixels beneath it are restored. Returns `false` when none was shown.
    pub fn hide_cursor(&mut self) -> bool {
        let Some(cursor) = self.cursor.take() else {
            return false;
        };
        self.damage.add(cursor.bounds());
        true
    }

    /// The screen rectangle the cursor currently covers, if one is shown.
    #[must_use]
    pub fn cursor_bounds(&self) -> Option<Rect> {
        self.cursor.as_ref().map(CursorLayer::bounds)
    }

    /// Whether any pixels are pending recomposition.
    #[must_use]
    pub fn has_damage(&self) -> bool {
        !self.damage.is_empty()
    }

    /// Recompose every damaged pixel into the back buffer and the
    /// scan-out frame, then clear the damage. Pixels outside the damage
    /// region keep their previous value (the point of damage tracking).
    ///
    /// Returns the smallest screen rectangle covering every recomposed
    /// pixel — [`Rect::EMPTY`] when nothing was dirty — so a presenter
    /// can hand the display only the changed region
    /// ([`Display::present_region`]).
    pub fn composite(&mut self) -> Rect {
        let screen = self.screen_rect();
        let damage = core::mem::take(&mut self.damage);
        let mut bounds = Rect::EMPTY;
        for &dirty in damage.rects() {
            let area = dirty.intersection(&screen);
            if area.is_empty() {
                continue;
            }
            bounds = bounds.union(&area);
            for y in area.top()..area.bottom() {
                for x in area.left()..area.right() {
                    self.recompose_pixel(x, y);
                }
            }
        }
        bounds
    }

    /// The current scan-out frame, laid out for [`Compositor::mode`].
    #[must_use]
    pub fn frame(&self) -> &[u8] {
        &self.frame
    }

    /// Borrow the composited back buffer (premultiplied pixels).
    #[must_use]
    pub fn back_buffer(&self) -> &Surface {
        &self.back
    }

    /// Composite any pending damage and present the frame to `display`.
    ///
    /// When the recomposed damage is a strict sub-rectangle of the screen
    /// the frame is handed over through [`Display::present_region`], so a
    /// driver whose scan-out path is a copy (and the remote display
    /// client's shared-memory blit) touches only the changed pixels; a
    /// whole-screen damage — and the no-damage case, whose present is the
    /// caller's explicit request to (re-)show the current frame — takes
    /// the full [`Display::present`] path.
    ///
    /// # Errors
    ///
    /// Propagates any [`DriverError`] the display driver returns from
    /// [`Display::present`] / [`Display::present_region`].
    pub fn present(&mut self, display: &mut dyn Display) -> Result<(), DriverError> {
        let bounds = self.composite();
        if let Some(damage) = sub_screen_damage(&bounds, &self.mode) {
            display.present_region(&self.frame, damage)
        } else {
            display.present(&self.frame)
        }
    }

    /// Present via the display's hardware layer engine when it can serve
    /// the current scene, falling back to the software full-frame path
    /// otherwise (the software path is always the
    /// fallback).
    ///
    /// The scene is encoded back-to-front as one solid background layer,
    /// one layer per visible window (its surface baked with that window's
    /// opacity and rounded-corner coverage), and the cursor on top, so the
    /// hardware result matches the software compositor pixel-for-pixel. If
    /// the engine's [`AccelCaps`] cannot hold that many layers, or a layer
    /// is larger than the engine can source, the whole frame is composited
    /// in software and presented instead — never a partial hardware frame.
    ///
    /// # Errors
    ///
    /// Propagates any [`DriverError`] from the hardware
    /// [`AcceleratedDisplay::present_layers`] or, on fallback, the
    /// software [`Display::present`].
    pub fn present_accelerated(
        &mut self,
        display: &mut dyn AcceleratedDisplay,
    ) -> Result<(), DriverError> {
        let caps = display.accel_caps()?;
        if let Some(buffers) = self.encode_layers(&caps) {
            let layers: Vec<AccelLayer<'_>> = buffers.iter().map(LayerBuf::as_layer).collect();
            display.present_layers(&layers)
        } else {
            self.composite();
            display.present(&self.frame)
        }
    }

    /// Encode the current scene as hardware layers, or `None` if the
    /// engine's [`AccelCaps`] cannot serve it (the caller falls back to
    /// software).
    fn encode_layers(&self, caps: &AccelCaps) -> Option<Vec<LayerBuf>> {
        let max_layers = usize::try_from(caps.max_layers).unwrap_or(usize::MAX);
        let mut layers = Vec::new();
        layers.push(
            self.encode_layer(self.mode.width_px, self.mode.height_px, 0, 0, |_, _| {
                Some(self.background.premultiply())
            })?,
        );
        for window in &self.windows {
            if !window.is_visible() {
                continue;
            }
            let bounds = window.bounds();
            layers.push(self.encode_layer(
                bounds.width,
                bounds.height,
                bounds.left(),
                bounds.top(),
                |lx, ly| window.sample_local(lx, ly),
            )?);
        }
        if let Some(cursor) = &self.cursor {
            let bounds = cursor.bounds();
            layers.push(self.encode_layer(
                bounds.width,
                bounds.height,
                bounds.left(),
                bounds.top(),
                |lx, ly| cursor.sample_local(lx, ly),
            )?);
        }
        if layers.len() > max_layers {
            return None;
        }
        for layer in &layers {
            if layer.width > caps.max_width_px || layer.height > caps.max_height_px {
                return None;
            }
        }
        Some(layers)
    }

    /// Bake a `width`×`height` region into a premultiplied, display-format
    /// layer buffer placed at `(dst_x, dst_y)`. `sample` yields each
    /// surface-local pixel, or `None` for a transparent one. Returns
    /// `None` only if the buffer size overflows `usize`.
    fn encode_layer(
        &self,
        width: u32,
        height: u32,
        dst_x: i32,
        dst_y: i32,
        mut sample: impl FnMut(u32, u32) -> Option<Pixel>,
    ) -> Option<LayerBuf> {
        let w = usize::try_from(width).ok()?;
        let h = usize::try_from(height).ok()?;
        let count = w.checked_mul(h)?.checked_mul(4)?;
        let mut pixels = vec![0u8; count];
        for ly in 0..height {
            let row = usize::try_from(ly).ok()?;
            for lx in 0..width {
                let pixel = sample(lx, ly).unwrap_or(Pixel::TRANSPARENT);
                let col = usize::try_from(lx).ok()?;
                let offset = (row * w + col) * 4;
                if let Some(slot) = pixels.get_mut(offset..offset + 4) {
                    slot.copy_from_slice(&encode_pixel(self.order, pixel));
                }
            }
        }
        Some(LayerBuf {
            pixels,
            width,
            height,
            dst_x,
            dst_y,
        })
    }

    /// Apply `change` to the window named by `id`, marking the union of
    /// its bounds before and after dirty. Returns `false` for an unknown
    /// id.
    fn mutate(&mut self, id: WindowId, change: impl FnOnce(&mut Window)) -> bool {
        let Some(window) = self.windows.iter_mut().find(|w| w.id() == id) else {
            return false;
        };
        let before = window.bounds();
        change(window);
        let after = window.bounds();
        self.damage.add(before);
        self.damage.add(after);
        true
    }

    /// Recompute the composited value of screen pixel `(x, y)` and write
    /// it to the back buffer and the encoded frame.
    fn recompose_pixel(&mut self, x: i32, y: i32) {
        let mut acc = self.background.premultiply();
        for window in &self.windows {
            if let Some(src) = window.sample(x, y) {
                acc = src.over(acc);
            }
        }
        if let Some(cursor) = &self.cursor {
            if let Some(src) = cursor.sample(x, y) {
                acc = src.over(acc);
            }
        }
        let Ok(px) = u32::try_from(x) else { return };
        let Ok(py) = u32::try_from(y) else { return };
        self.back.set(px, py, acc);
        self.encode_pixel(px, py, acc);
    }

    /// Write the four scan-out bytes for `(px, py)` in the active format.
    fn encode_pixel(&mut self, px: u32, py: u32, pixel: Pixel) {
        let stride = self.mode.stride_bytes as usize;
        let offset = py as usize * stride + px as usize * 4;
        let bytes = encode_pixel(self.order, pixel);
        if let Some(slot) = self.frame.get_mut(offset..offset + 4) {
            slot.copy_from_slice(&bytes);
        }
    }
}

/// A baked, display-format layer buffer and its on-screen placement,
/// held alive while the borrowing [`AccelLayer`]s are handed to the
/// hardware engine.
struct LayerBuf {
    pixels: Vec<u8>,
    width: u32,
    height: u32,
    dst_x: i32,
    dst_y: i32,
}

impl LayerBuf {
    /// Borrow this buffer as an [`AccelLayer`]. The buffer is dense, so
    /// the stride is exactly four bytes per pixel.
    fn as_layer(&self) -> AccelLayer<'_> {
        AccelLayer {
            pixels: &self.pixels,
            width_px: self.width,
            height_px: self.height,
            stride_bytes: self.width.saturating_mul(4),
            dst_x: self.dst_x,
            dst_y: self.dst_y,
            opacity: 255,
        }
    }
}

/// Convert composited damage `bounds` into the [`DamageRect`] a
/// [`Display::present_region`] call carries, or `None` when the full
/// [`Display::present`] path should run instead: an empty bounds (the
/// present is then an explicit re-show of the current frame) or a bounds
/// covering the whole screen. The bounds are already clipped to the
/// screen, so the coordinate conversions cannot fail; a rectangle that
/// nevertheless does not fit falls back to the full present rather than
/// presenting a wrong region.
fn sub_screen_damage(bounds: &Rect, mode: &DisplayMode) -> Option<DamageRect> {
    if bounds.is_empty() {
        return None;
    }
    let damage = DamageRect {
        x: u32::try_from(bounds.left()).ok()?,
        y: u32::try_from(bounds.top()).ok()?,
        width_px: bounds.width,
        height_px: bounds.height,
    };
    if damage.covers(mode) {
        return None;
    }
    Some(damage)
}

/// Allocate a zeroed scan-out frame for `mode`, or `None` if its size
/// overflows `usize` or its stride is too small for one scanline.
fn scanout_frame(mode: &DisplayMode) -> Option<Vec<u8>> {
    let min_stride = mode.width_px.checked_mul(mode.format.bytes_per_pixel())?;
    if mode.stride_bytes < min_stride || mode.width_px == 0 || mode.height_px == 0 {
        return None;
    }
    let len = u64::from(mode.stride_bytes).checked_mul(u64::from(mode.height_px))?;
    let len = usize::try_from(len).ok()?;
    Some(vec![0u8; len])
}

/// Map a [`DisplayFormat`] to its [`ChannelOrder`], or `None` for a
/// format this software compositor does not encode (it fails closed at
/// [`Compositor::new`] rather than guessing a byte order).
fn channel_order(format: DisplayFormat) -> Option<ChannelOrder> {
    match format {
        DisplayFormat::Rgba8888 => Some(ChannelOrder::Rgba),
        DisplayFormat::Bgra8888 => Some(ChannelOrder::Bgra),
        _ => None,
    }
}

/// Encode one premultiplied pixel into the four scan-out bytes for
/// `order`. The screen is opaque, so the premultiplied channels equal
/// their straight-alpha form.
fn encode_pixel(order: ChannelOrder, pixel: Pixel) -> [u8; 4] {
    match order {
        ChannelOrder::Rgba => [pixel.r, pixel.g, pixel.b, pixel.a],
        ChannelOrder::Bgra => [pixel.b, pixel.g, pixel.r, pixel.a],
    }
}
