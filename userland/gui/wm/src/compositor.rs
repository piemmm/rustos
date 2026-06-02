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
//! framebuffer access through a capability (`AGENTS.md` §10). GPU
//! acceleration, when a driver exposes it, replaces this software path
//! behind the same public surface; today the software path is the
//! fallback every platform always has.

use alloc::vec;
use alloc::vec::Vec;

use rustos_abi::driver::display::{Display, DisplayFormat, DisplayMode};
use rustos_abi::DriverError;

use rustos_cursor::CursorImage;
use rustos_theme::CursorKind;

use crate::color::{Color, Pixel};
use crate::corner::Corners;
use crate::cursor::CursorLayer;
use crate::damage::DamageRegion;
use crate::geometry::{Point, Rect};
use crate::surface::Surface;
use crate::window::{Window, WindowId};

/// Scan-out channel order for a supported [`DisplayFormat`].
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
enum ChannelOrder {
    Rgba,
    Bgra,
}

/// A software compositing window manager surface.
pub struct Compositor {
    mode: DisplayMode,
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
    /// closed rather than panicking (§2.9).
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
    /// are a cosmetic compositing effect (`AGENTS.md` §2.2) and do not
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
    /// and blends it (`AGENTS.md` §2.2 / §2.4).
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
    pub fn composite(&mut self) {
        let screen = self.screen_rect();
        let damage = core::mem::take(&mut self.damage);
        for &dirty in damage.rects() {
            let area = dirty.intersection(&screen);
            if area.is_empty() {
                continue;
            }
            for y in area.top()..area.bottom() {
                for x in area.left()..area.right() {
                    self.recompose_pixel(x, y);
                }
            }
        }
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
    /// # Errors
    ///
    /// Propagates any [`DriverError`] the display driver returns from
    /// [`Display::present`].
    pub fn present(&mut self, display: &mut dyn Display) -> Result<(), DriverError> {
        self.composite();
        display.present(&self.frame)
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
/// [`Compositor::new`] rather than guessing a byte order, §2.1).
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
