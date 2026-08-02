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

use tairix_abi::driver::display::{
    AccelCaps, AccelLayer, AcceleratedDisplay, DamageRect, Display, DisplayFormat, DisplayMode,
};
use tairix_abi::DriverError;

use tairix_controls::{FurniturePart, TitleBarEvent, WindowFrame};
use tairix_cursor::CursorImage;
use tairix_input::{InputEvent, Key};
use tairix_reclaim::{CacheAccounting, PressureBand, PressureGauge, ReclaimCache, Served};
use tairix_theme::{CursorKind, Theme};

use crate::chrome::{ChromeEpoch, WindowChrome};
use crate::color::{Color, Pixel};
use crate::corner::Corners;
use crate::cursor::CursorLayer;
use crate::damage::DamageRegion;
use crate::geometry::{Point, Rect, Scale};
use crate::surface::Surface;
use crate::viewport::{FurnitureHit, RootViewport};
use crate::window::{Window, WindowId, WindowRow};

/// The furniture a composite pass built for itself because the cache would
/// not retain it, kept alive for exactly that pass.
///
/// A reclaimable cache is an accelerator, never a correctness requirement:
/// a window whose entry is refused (the budget is exhausted, pressure
/// forbids growth, the cache is poisoned) must still draw its frame. A
/// short association list rather than a map is deliberate — it is empty on
/// every healthy frame, so it allocates nothing at all, and when it does
/// fill it holds only the windows covering the damage.
type ChromeFallback = Vec<(WindowId, WindowChrome)>;

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
    theme: Theme,
    /// Bumped every time [`set_theme`](Compositor::set_theme) really
    /// changes the theme. It is the theme half of [`ChromeEpoch`], and a
    /// counter rather than the theme's own id because two distinct themes
    /// may share an id (a contrast or motion variant of the built-in dark
    /// theme keeps `ThemeId::DARK`), which would leave furniture painted
    /// from the superseded palette on screen.
    theme_generation: u64,
    /// Every decorated window's rendered furniture, bounded by one
    /// screenful and released under memory pressure (see [`crate::chrome`]).
    chrome: ReclaimCache<WindowId, WindowChrome, ChromeEpoch>,
    /// The machine's memory-pressure band, shared with the furniture
    /// cache so the desktop has one notion of how tight memory is.
    pressure: &'static (dyn PressureGauge + 'static),
    /// Windows whose content the compositor released (or found missing on
    /// becoming visible) and whose owning app must therefore be asked to
    /// present again. The embedder drains this with
    /// [`pending_redraws`](Self::pending_redraws); the compositor never
    /// speaks the window protocol itself.
    pending_redraws: Vec<WindowId>,
    background: Color,
    order: ChannelOrder,
    windows: Vec<Window>,
    cursor: Option<CursorLayer>,
    /// The screen rectangle the cursor covered as of the last
    /// [`composite`](Self::composite), or `None` if it was hidden then.
    /// [`composite`](Self::composite) diffs the *current* cursor state
    /// against this to decide the cursor's damage, so a whole batch of
    /// [`set_cursor`](Self::set_cursor) / [`move_cursor`](Self::move_cursor) /
    /// [`hide_cursor`](Self::hide_cursor) calls pumped between two
    /// composites recomposites only the rectangle the cursor is leaving and
    /// the one it ends up in, never an intermediate position nothing was
    /// ever drawn to.
    cursor_on_screen: Option<Rect>,
    /// Whether [`set_cursor`](Self::set_cursor) installed artwork the last
    /// [`composite`](Self::composite) did not draw. Replacement artwork is
    /// always assumed to differ from what is on screen — exactly as a
    /// replaced window surface is — so a shape change landing on the very
    /// same rectangle (the pointer picking up a text or resize shape
    /// without moving) still repaints.
    cursor_replaced: bool,
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
    /// `chrome` is the bounded, pressure-governed cache this output
    /// retains its decorated windows' rendered furniture in. It is handed
    /// in rather than built here ([`chrome_cache`](crate::chrome_cache) is
    /// the one place it is assembled) because only the embedder knows the
    /// real output size, the owning seat, and the process's live pressure
    /// gauge and audit sink; a cache built without them would serve every
    /// lookup correctly while retaining nothing.
    ///
    /// `pressure` is that same live gauge, which the compositor also
    /// consults directly to decide when to release window *content*
    /// ([`release_content_under_pressure`](Self::release_content_under_pressure)).
    /// Content is not cached furniture and is deliberately not keyed into
    /// `chrome`; both mechanisms answer to the one gauge so the desktop
    /// never holds two disagreeing views of how tight memory is.
    ///
    /// Returns `None` if `mode` describes a surface too large to
    /// allocate or whose stride cannot hold one scanline, failing
    /// closed rather than panicking.
    #[must_use]
    pub fn new(
        mode: DisplayMode,
        background: Color,
        chrome: ReclaimCache<WindowId, WindowChrome, ChromeEpoch>,
        pressure: &'static (dyn PressureGauge + 'static),
    ) -> Option<Self> {
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
            theme: Theme::dark(),
            theme_generation: 0,
            chrome,
            pressure,
            pending_redraws: Vec::new(),
            background,
            order,
            windows: Vec::new(),
            cursor: None,
            cursor_on_screen: None,
            cursor_replaced: false,
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
        self.refresh_frame_bands();
        self.damage.add(self.screen_rect());
        true
    }

    /// The active desktop theme this output decorates windows with.
    ///
    /// The window manager draws decoration frames (title bar, borders, resize
    /// edges) from this theme's palette and metrics; it is the single theme the
    /// output owns, read here rather than copied.
    #[must_use]
    pub const fn theme(&self) -> &Theme {
        &self.theme
    }

    /// Switch the active desktop theme, returning whether it changed.
    ///
    /// A runtime light/dark switch is one call here: every decorated window
    /// re-resolves its reserved furniture band (a theme may change the border,
    /// inset, or title-bar metrics), and the whole screen is marked dirty so
    /// the next composite repaints every window and its decorations under the
    /// new palette. Setting the theme already in effect changes nothing and
    /// returns `false`.
    pub fn set_theme(&mut self, theme: Theme) -> bool {
        if theme == self.theme {
            return false;
        }
        self.theme = theme;
        self.theme_generation = self.theme_generation.saturating_add(1);
        self.refresh_frame_bands();
        self.damage.add(self.screen_rect());
        true
    }

    /// Re-resolve every decorated window's furniture band for the current
    /// output scale and theme (after a DPI or theme change), so each window's
    /// outer bounds reflect the new band thickness.
    ///
    /// No window's retained furniture is released here. Both changes that
    /// reach this point move the chrome epoch on, which drops the whole
    /// cache at the next lookup — the one case where every window's
    /// furniture really is stale at once.
    fn refresh_frame_bands(&mut self) {
        let scale = self.scale;
        for window in &mut self.windows {
            window.refresh_band(scale, &self.theme);
        }
    }

    /// The generation every retained [`WindowChrome`] is valid for: this
    /// output's scale and the theme it was painted under.
    fn chrome_epoch(&self) -> ChromeEpoch {
        (self.scale.percent(), self.theme_generation)
    }

    /// Window furniture currently retained, one entry per decorated window
    /// whose frame has been composited and not since invalidated.
    #[must_use]
    pub fn chrome_cache_len(&self) -> usize {
        self.chrome.len()
    }

    /// Whether the window named by `id` has furniture retained at the
    /// current epoch — which entry survived, not merely how many did.
    #[cfg(test)]
    pub(crate) fn chrome_resident(&self, id: WindowId) -> bool {
        self.chrome.peek(&self.chrome_epoch(), &id).is_some()
    }

    /// Bytes the window-furniture cache currently has charged: retained
    /// strip pixels plus its own per-entry bookkeeping.
    #[must_use]
    pub fn chrome_cache_bytes(&self) -> usize {
        self.chrome.charged_bytes()
    }

    /// The window-furniture cache's byte ledger and event counters, for
    /// diagnostics.
    #[must_use]
    pub fn chrome_cache_stats(&self) -> &CacheAccounting {
        self.chrome.accounting()
    }

    /// Give back whatever the current memory-pressure band says retained
    /// window furniture may keep, returning the bytes released.
    ///
    /// The session calls this when the kernel wakes it with a deepened
    /// band, so the desktop releases its furniture at the moment pressure
    /// rises rather than at whatever later frame happens to compose. Every
    /// window stays correct throughout: a released strip is simply rendered
    /// again on demand, so this costs rendering work and never a wrong
    /// pixel. A band that demands nothing releases nothing.
    pub fn trim_chrome(&mut self) -> usize {
        self.chrome.enforce_pressure()
    }

    /// Release and wipe every retained strip, because the seat this output
    /// belongs to is going away.
    ///
    /// Furniture carries window titles, so the strips are overwritten
    /// rather than merely dropped: an ended session leaves no readable
    /// rendered title behind in reusable heap. The cache stays usable — a
    /// later composite rebuilds what it needs.
    pub fn teardown_chrome(&mut self) {
        self.chrome.teardown();
    }

    /// Release and wipe every window's content pixels, because the seat
    /// this output belongs to is going away.
    ///
    /// A window's content is whatever the user was looking at, so it is
    /// overwritten rather than merely dropped: an ended session leaves no
    /// readable frame behind in reusable heap. No redraw is requested —
    /// the seat is gone, so there is nobody left to present to — which is
    /// what separates this from the pressure ladder
    /// ([`release_content_under_pressure`](Self::release_content_under_pressure)).
    pub fn teardown_content(&mut self) {
        for window in &mut self.windows {
            window.release_content();
        }
        self.pending_redraws.clear();
    }

    /// Release window *content* pixels according to the machine's current
    /// memory-pressure band, returning the bytes given back.
    ///
    /// # Why this is a policy and not a cache
    ///
    /// Window furniture is a keyed LRU cache ([`crate::chrome`]) because
    /// losing a strip costs only a re-render. Content is different: the
    /// compositor holds the *only* copy of a window's pixels, so evicting
    /// a visible window's content is a visible defect, not a slowdown, and
    /// no recency ordering can make it safe. Content is therefore released
    /// by an explicit ladder over the *same* [`PressureGauge`] and the same
    /// [`tairix_reclaim::shrink_target`] ordering the caches obey — one
    /// memory model, two mechanisms suited to two different kinds of
    /// memory. Do not turn this into a cache.
    ///
    /// # What may be released at all
    ///
    /// Only an *app-presented* window ([`Window::is_app_presented`]) is
    /// ever a candidate, whatever the band: a window the embedder paints
    /// itself — the taskbar, a session dialog, the lock screen — has no
    /// client to ask, so releasing it would blank it with no way back.
    ///
    /// # The ladder
    ///
    /// * [`PressureBand::Normal`] — nothing is released. Memory is
    ///   plentiful, and every release costs the owning app a repaint.
    /// * [`PressureBand::Mild`] and deeper — every hidden or minimised
    ///   window's content goes. Nobody is looking at it, so the release is
    ///   invisible until the window is shown again, and a few minimised
    ///   full-screen windows are the largest easily-recovered block the
    ///   desktop holds.
    /// * [`PressureBand::Critical`] — visible but unfocused windows go
    ///   too, `focused` alone excepted. A background window blank for the
    ///   frame it takes its app to answer the redraw is a far better
    ///   outcome than exhausting memory; the focused window is never
    ///   released because there would be nothing to show in its place.
    ///
    /// Every released window is queued for a redraw request
    /// ([`pending_redraws`](Self::pending_redraws)) and its outer bounds
    /// are marked dirty, so the desktop shows through immediately rather
    /// than keeping a stale image the compositor no longer has.
    pub fn release_content_under_pressure(&mut self, focused: Option<WindowId>) -> usize {
        let band = self.pressure.sample();
        if band == PressureBand::Normal {
            return 0;
        }
        let mut released = 0usize;
        let mut freed = Vec::new();
        for window in &mut self.windows {
            let id = window.id();
            if !window.is_app_presented() {
                continue;
            }
            let takeable = if window.is_visible() {
                band >= PressureBand::Critical && Some(id) != focused
            } else {
                true
            };
            if !takeable {
                continue;
            }
            let bytes = window.release_content();
            if bytes > 0 {
                released = released.saturating_add(bytes);
                // A hidden window draws nothing either way, so only a
                // visible one's pixels actually changed on screen.
                let exposed = window.is_visible().then(|| window.bounds());
                freed.push((id, exposed));
            }
        }
        for (id, exposed) in freed {
            self.request_redraw(id);
            if let Some(bounds) = exposed {
                self.damage.add(bounds);
            }
        }
        released
    }

    /// Heap bytes every window's retained content pixels currently
    /// occupy — what [`release_content_under_pressure`] has to give back.
    ///
    /// [`release_content_under_pressure`]: Self::release_content_under_pressure
    #[must_use]
    pub fn content_bytes(&self) -> usize {
        self.windows
            .iter()
            .fold(0, |sum, w| sum.saturating_add(w.content_bytes()))
    }

    /// Take every window awaiting a redraw request, leaving the queue
    /// empty.
    ///
    /// The compositor knows *which* windows lost their pixels but nothing
    /// about the window protocol or which app owns which window — keeping
    /// it that way is what stops the window manager from depending on the
    /// window-server side. The embedder drains this after a release (and
    /// after showing a window again) and delivers the protocol's redraw
    /// event to each owner.
    #[must_use]
    pub fn pending_redraws(&mut self) -> Vec<WindowId> {
        core::mem::take(&mut self.pending_redraws)
    }

    /// Queue `id` for a redraw request, at most once per drain: a window
    /// released twice before the embedder drains still needs exactly one
    /// present back.
    fn request_redraw(&mut self, id: WindowId) {
        if !self.pending_redraws.contains(&id) {
            self.pending_redraws.push(id);
        }
    }

    /// Apply `change` to the window named by `id` under the active scale
    /// and theme, releasing that window's retained furniture, and return
    /// what `change` produced (`None` for an unknown id).
    ///
    /// Every mutation that can alter how a frame is drawn runs through
    /// here, so releasing the entry is part of the mutation rather than
    /// something each caller must remember. It is one key, never the whole
    /// cache: a title edit or a focus flip leaves every *other* window's
    /// furniture perfectly valid.
    fn mutate_frame<R>(
        &mut self,
        id: WindowId,
        change: impl FnOnce(&mut Window, Scale, &Theme) -> R,
    ) -> Option<R> {
        let scale = self.scale;
        let Self {
            theme,
            windows,
            chrome,
            ..
        } = self;
        let window = windows.iter_mut().find(|w| w.id() == id)?;
        let out = change(window, scale, theme);
        chrome.invalidate(&id);
        Some(out)
    }

    /// The desktop background colour behind every window (always opaque).
    #[must_use]
    pub const fn background(&self) -> Color {
        self.background
    }

    /// Set the desktop background colour, returning whether it changed.
    ///
    /// A runtime theme switch is one call here: the whole screen is marked
    /// dirty so the next composite repaints every pixel over the new
    /// background — windows and the cursor are re-blended on top unchanged.
    /// The alpha is forced to opaque exactly as at
    /// [`new`](Self::new): the root surface has nothing behind it. Setting
    /// the colour already in effect changes nothing and returns `false`, so
    /// the caller can skip a redundant present.
    pub fn set_background(&mut self, background: Color) -> bool {
        let background = Color {
            a: 255,
            ..background
        };
        if background == self.background {
            return false;
        }
        self.background = background;
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
    /// covered rectangles are marked dirty. A move to the window's current
    /// origin marks no damage and still returns `true` (only an unknown
    /// `id` returns `false`).
    pub fn move_window(&mut self, id: WindowId, origin: Point) -> bool {
        self.mutate(id, |w| w.set_origin(origin))
    }

    /// Set a window's opacity (`255` opaque); its bounds are marked dirty.
    /// Setting the opacity it already has marks no damage and still
    /// returns `true` (only an unknown `id` returns `false`).
    pub fn set_opacity(&mut self, id: WindowId, opacity: u8) -> bool {
        self.mutate(id, |w| w.set_opacity(opacity))
    }

    /// Set a window's corner style; its bounds are marked dirty. Setting
    /// the corners it already has marks no damage and still returns `true`
    /// (only an unknown `id` returns `false`).
    pub fn set_corners(&mut self, id: WindowId, corners: Corners) -> bool {
        self.mutate(id, |w| w.set_corners(corners))
    }

    /// Show or hide a window; its bounds are marked dirty. Setting the
    /// visibility it already has marks no damage and still returns `true`
    /// (only an unknown `id` returns `false`).
    pub fn set_visible(&mut self, id: WindowId, visible: bool) -> bool {
        let known = self.mutate(id, |w| w.set_visible(visible));
        // A window shown again after its content was released has nothing
        // to draw until its app presents, so ask now rather than leaving
        // it blank until something else happens to it.
        let contentless = self
            .window(id)
            .is_some_and(|w| w.is_app_presented() && !w.has_content());
        if known && visible && contentless {
            self.request_redraw(id);
        }
        known
    }

    /// Declare that a client presents the window named by `id` and can be
    /// asked to present it again, or that the embedder paints it itself.
    /// Returns `false` for an unknown id.
    ///
    /// This is what makes a window's content releasable under memory
    /// pressure ([`release_content_under_pressure`](Self::release_content_under_pressure)).
    /// A window starts un-declared, so an embedder that never calls this
    /// keeps every pixel it hands over: releasing content nobody can
    /// redraw would blank the window permanently, so the default fails
    /// closed rather than guessing there is a client behind it.
    pub fn set_app_presented(&mut self, id: WindowId, app_presented: bool) -> bool {
        self.windows
            .iter_mut()
            .find(|w| w.id() == id)
            .map(|w| w.set_app_presented(app_presented))
            .is_some()
    }

    /// Replace a window's content surface; the union of the old and new
    /// bounds is marked dirty. A replacement surface is always assumed to
    /// change the window's appearance, so this never skips damage the way
    /// [`move_window`](Self::move_window) and its siblings do for a
    /// genuine no-op.
    pub fn set_surface(&mut self, id: WindowId, surface: Surface) -> bool {
        self.mutate(id, |w| {
            w.replace_surface(surface);
            true
        })
    }

    /// Edit a window's content surface **in place**, marking dirty only the
    /// content-local rectangle the edit reports it changed, and return the
    /// edit's own value — or `None` for an unknown `id`.
    ///
    /// Unlike [`set_surface`](Self::set_surface), the caller writes into
    /// the window's existing buffer rather than handing over a fresh one,
    /// so a presenter applying a per-frame damage region into the
    /// window's persistent content needs no second copy of the surface to
    /// convert into and clone from. The edit reports its own damage rather
    /// than the caller declaring one up front, because only the edit
    /// itself — by comparing each converted pixel against the one already
    /// there — learns which pixels genuinely changed; a rectangle handed
    /// down before the edit runs would have to be conservative and repaint
    /// pixels that never moved.
    ///
    /// The reported `Rect` is in content-surface-local pixels (origin at
    /// the content's top-left). It is translated by the window's content
    /// origin and intersected with its client rectangle
    /// ([`Window::client_rect`]), so an empty rectangle marks nothing and
    /// an over-large one is clipped rather than ever reaching into a
    /// neighbouring window. The edit must not resize the surface (a
    /// window's dimensions are fixed for its lifetime).
    pub fn edit_window_surface<T>(
        &mut self,
        id: WindowId,
        edit: impl FnOnce(&mut Surface) -> (T, Rect),
    ) -> Option<T> {
        let window = self.windows.iter_mut().find(|w| w.id() == id)?;
        let (out, local_damage) = edit(window.content_mut()?);
        let client = window.client_rect();
        let screen_damage = Rect::new(
            client.left().saturating_add(local_damage.left()),
            client.top().saturating_add(local_damage.top()),
            local_damage.width,
            local_damage.height,
        )
        .intersection(&client);
        self.damage.add(screen_damage);
        Some(out)
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

    /// Send a window to the bottom of the z-order (put-to-back), keeping it
    /// visible; its bounds are marked dirty so whatever it was covering
    /// recomposites. Returns `false` for an unknown id.
    pub fn lower(&mut self, id: WindowId) -> bool {
        let Some(index) = self.windows.iter().position(|w| w.id() == id) else {
            return false;
        };
        if index == 0 {
            // Already at the back: nothing to restack, nothing to repaint.
            return true;
        }
        let window = self.windows.remove(index);
        self.damage.add(window.bounds());
        self.windows.insert(0, window);
        true
    }

    /// Toggle the decorated, resizable window named by `id` between restored
    /// and maximized, sizing it to `work_area` on maximize and back to its
    /// pre-maximize geometry on restore. Returns the new size state and the
    /// resulting client rectangle (so the session can tell the app its new
    /// content size), or `None` for an unknown, undecorated, or non-resizable
    /// window, or a resize that failed closed. The union of the old and new
    /// outer bounds is marked dirty.
    pub fn toggle_window_size(
        &mut self,
        id: WindowId,
        work_area: Rect,
    ) -> Option<(tairix_controls::WindowSizeState, Rect)> {
        let (result, before, after) = self.mutate_frame(id, |window, scale, theme| {
            let before = window.bounds();
            let result = window.toggle_size(work_area, scale, theme);
            (result, before, window.bounds())
        })?;
        if result.is_some() {
            self.damage.add(before);
            self.damage.add(after);
        }
        result
    }

    /// Remove a window; its last bounds are marked dirty.
    ///
    /// Its retained furniture and its content pixels both go with it,
    /// wiped rather than merely dropped: a closed window's rendered title
    /// and its last frame are user data and must not sit in reusable heap
    /// waiting for something else to overwrite them.
    pub fn remove(&mut self, id: WindowId) -> bool {
        let Some(index) = self.windows.iter().position(|w| w.id() == id) else {
            return false;
        };
        let mut window = self.windows.remove(index);
        self.chrome.invalidate(&id);
        self.pending_redraws.retain(|pending| *pending != id);
        window.release_content();
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

    /// Give the window named by `id` a root viewport, so the window manager
    /// composes its scrollbars as furniture around the client. Returns
    /// `false` for an unknown id. The window's bounds are marked dirty (the
    /// reserved gutter re-clips the client), unless it already had this
    /// exact viewport, in which case nothing is marked.
    pub fn set_root_viewport(&mut self, id: WindowId, viewport: RootViewport) -> bool {
        self.mutate(id, |w| w.set_viewport(Some(viewport)))
    }

    /// Remove the root viewport from the window named by `id`, so the window
    /// manager stops composing scrollbar furniture and the client reclaims
    /// the reserved gutter. Returns `false` for an unknown id. The window's
    /// bounds are marked dirty (the reclaimed gutter recomposites), unless
    /// it had no viewport already, in which case nothing is marked.
    pub fn clear_root_viewport(&mut self, id: WindowId) -> bool {
        self.mutate(id, |w| w.set_viewport(None))
    }

    /// The root viewport of the window named by `id`, or `None` when the id
    /// is unknown or the window has no root viewport.
    #[must_use]
    pub fn root_viewport(&self, id: WindowId) -> Option<&RootViewport> {
        self.window(id).and_then(Window::viewport)
    }

    /// Decorate the window named by `id` with the window-manager-owned frame
    /// `frame` (title bar, borders, resize edges). Returns `false` for an
    /// unknown id.
    ///
    /// The frame band is reserved *around* the client at the active
    /// scale/theme, so the window's outer [`bounds`](Window::bounds) grow to
    /// hold the decoration and its content surface is presented inset at the
    /// [`client_rect`](Window::client_rect); the client never overlaps the
    /// furniture. The union of the old and new outer bounds is marked dirty.
    pub fn set_window_frame(&mut self, id: WindowId, frame: WindowFrame) -> bool {
        let Some((before, after)) = self.mutate_frame(id, |window, scale, theme| {
            let before = window.bounds();
            window.set_frame(Some(frame), scale, theme);
            (before, window.bounds())
        }) else {
            return false;
        };
        self.damage.add(before);
        self.damage.add(after);
        true
    }

    /// Remove the decoration frame from the window named by `id`, so the window
    /// manager stops reserving and drawing furniture and the window's outer
    /// bounds collapse back to the bare content surface. Returns `false` for an
    /// unknown id. The union of the old and new bounds is marked dirty.
    pub fn clear_window_frame(&mut self, id: WindowId) -> bool {
        let Some((before, after)) = self.mutate_frame(id, |window, scale, theme| {
            let before = window.bounds();
            window.set_frame(None, scale, theme);
            (before, window.bounds())
        }) else {
            return false;
        };
        self.damage.add(before);
        self.damage.add(after);
        true
    }

    /// The decoration frame of the window named by `id`, or `None` when the id
    /// is unknown or the window is undecorated.
    #[must_use]
    pub fn window_frame(&self, id: WindowId) -> Option<&WindowFrame> {
        self.window(id).and_then(Window::frame)
    }

    /// The screen rectangle the application content of the window named by `id`
    /// occupies — the inset client viewport for a decorated window, or the full
    /// bounds for a plain one — or `None` for an unknown id.
    #[must_use]
    pub fn window_client_rect(&self, id: WindowId) -> Option<Rect> {
        self.window(id).map(Window::client_rect)
    }

    /// Mark the decorated window named by `id` active or inactive, repainting
    /// its frame rim, title, and controls under the new activation. Returns
    /// `false` for an unknown or undecorated window.
    ///
    /// The window manager keeps this in step with the focused window the
    /// [`InputRouter`](crate::InputRouter) tracks. Only the furniture bands are
    /// marked dirty — the client area does not change on a focus flip — so a
    /// focus change never triggers a full-window recomposite.
    pub fn set_active_frame(&mut self, id: WindowId, active: bool) -> bool {
        let bands = self.mutate_frame(id, |window, _, _| {
            window
                .set_frame_active(active)
                .then(|| window.furniture_bands())
        });
        let Some(Some(bands)) = bands else {
            return false;
        };
        for band in bands {
            self.damage.add(band);
        }
        true
    }

    /// Set the decorated window named by `id`'s title, repainting the title
    /// bar. Returns `false` for an unknown or undecorated window.
    ///
    /// The title is the untrusted string the window channel already carries
    /// (`WindowTitle`); the title bar sanitises and truncates it. Only the top
    /// (title) furniture band is marked dirty — a title edit never touches the
    /// client or the other frame edges.
    pub fn set_window_title(&mut self, id: WindowId, title: &str) -> bool {
        let band = self.mutate_frame(id, |window, _, _| {
            window.set_frame_title(title).then(|| window.title_band())
        });
        let Some(Some(band)) = band else {
            return false;
        };
        self.damage.add(band);
        true
    }

    /// Classify the screen `point` against the furniture of the window named
    /// by `id`, or `None` when the id is unknown or the window has no root
    /// viewport. This is the furniture hit map: a point on a scrollbar or the
    /// corner never reads as client.
    #[must_use]
    pub fn furniture_hit(&self, id: WindowId, point: Point) -> Option<FurnitureHit> {
        let window = self.window(id)?;
        let viewport = window.viewport()?;
        Some(viewport.hit_test(window.bounds(), point))
    }

    /// Classify the screen `point` against the decoration frame of the window
    /// named by `id`, or `None` when the id is unknown or the window is
    /// undecorated. This is the outer-frame furniture hit map: the title bar,
    /// the command controls, the resize edges, and the inert rim each classify
    /// distinctly, and the inset client viewport reads as
    /// [`FurniturePart::Client`] — the client can never receive a point the
    /// frame owns.
    #[must_use]
    pub fn frame_hit(&self, id: WindowId, point: Point) -> Option<FurniturePart> {
        let window = self.window(id)?;
        let frame = window.frame()?;
        Some(frame.hit(window.bounds(), self.scale, &self.theme, point))
    }

    /// Feed a pointer `event` to the decoration furniture of the window named
    /// by `id`, repainting only its furniture bands, and return the typed
    /// [`TitleBarEvent`] it produced (a completed command-control click, or a
    /// title-bar activation/drag gesture). Returns `None` for an unknown or
    /// undecorated window.
    ///
    /// The window manager owns this furniture, so the event is never delivered
    /// to the client; only the furniture bands are marked dirty, so a hover or
    /// press repaint never touches the client area.
    pub fn frame_pointer(&mut self, id: WindowId, event: &InputEvent) -> Option<TitleBarEvent> {
        let (result, bands) = self.mutate_frame(id, |window, scale, theme| {
            let result = window.on_frame_pointer(event, scale, theme);
            (result, window.furniture_bands())
        })?;
        for band in bands {
            self.damage.add(band);
        }
        result
    }

    /// Feed a key `key` to the decoration furniture of the window named by
    /// `id` (the title bar's command controls), repainting the title band, and
    /// return the typed [`TitleBarEvent`] it produced. Returns `None` for an
    /// unknown or undecorated window.
    pub fn frame_key(&mut self, id: WindowId, key: Key) -> Option<TitleBarEvent> {
        let (result, band) = self.mutate_frame(id, |window, _, _| {
            let result = window.on_frame_key(key);
            (result, window.title_band())
        })?;
        self.damage.add(band);
        result
    }

    /// Resize the window named by `id` so its outer rectangle becomes
    /// `new_outer` (its content surface reallocated to the implied client
    /// size, existing pixels preserved, origin and decoration following).
    /// Returns `false` for an unknown window or when the implied client size
    /// is empty. The union of the old and new outer bounds is marked dirty.
    pub fn resize_window(&mut self, id: WindowId, new_outer: Rect) -> bool {
        let Some((changed, before, after)) = self.mutate_frame(id, |window, scale, theme| {
            let before = window.bounds();
            let changed = window.resize_to_outer(new_outer, scale, theme);
            (changed, before, window.bounds())
        }) else {
            return false;
        };
        if changed {
            self.damage.add(before);
            self.damage.add(after);
        }
        changed
    }

    /// Reallocate the content surface of the window named by `id` to the new
    /// client size `client_w` × `client_h`, keeping its origin, preserving the
    /// existing pixels where they still fit, and repainting the decoration at
    /// the new size. Returns `false` for an unknown window or an empty/failed
    /// allocation (fail closed). The union of the old and new outer bounds is
    /// marked dirty.
    ///
    /// This is the window-channel `Resize` path: the app hands the session a
    /// new *client* content size, so the compositor sizes the content directly
    /// (unlike [`resize_window`](Self::resize_window), which sizes from an
    /// outer rectangle and moves the origin for an interactive edge drag).
    pub fn resize_window_client(&mut self, id: WindowId, client_w: u32, client_h: u32) -> bool {
        let Some((changed, before, after)) = self.mutate_frame(id, |window, scale, theme| {
            let before = window.bounds();
            let changed = window.resize_client(client_w, client_h, scale, theme);
            (changed, before, window.bounds())
        }) else {
            return false;
        };
        if changed {
            self.damage.add(before);
            self.damage.add(after);
        }
        changed
    }

    /// Mutate the root viewport of the window named by `id` through `change`,
    /// marking the window's bounds dirty so the bars recompose. Returns
    /// `None` for an unknown id or a window with no root viewport.
    pub fn scroll_root<T>(
        &mut self,
        id: WindowId,
        change: impl FnOnce(&mut RootViewport) -> T,
    ) -> Option<T> {
        let window = self.windows.iter_mut().find(|w| w.id() == id)?;
        let bounds = window.bounds();
        let out = change(window.viewport_mut()?);
        self.damage.add(bounds);
        Some(out)
    }

    /// Show `image` as the pointer cursor with its hotspot at `pointer`,
    /// replacing any current cursor.
    ///
    /// The artwork comes from `lib/cursor` (a scalable, colourful, vector
    /// cursor rasterised at the display scale); the compositor only places
    /// and blends it. This does not mark damage itself:
    /// [`composite`](Self::composite) derives it from the footprint
    /// recorded at the *previous* composite, so any mix of `set_cursor`,
    /// [`move_cursor`](Self::move_cursor), and
    /// [`hide_cursor`](Self::hide_cursor) calls pumped before the next
    /// composite recomposites only the rectangle the cursor is leaving and
    /// the one it ends up in — never an intermediate position nothing was
    /// ever drawn to.
    ///
    /// Replacement artwork always repaints, even when it covers exactly the
    /// rectangle already on screen: the pointer picks up a text or resize
    /// shape without moving, and those pixels differ however identical the
    /// rectangle is.
    pub fn set_cursor(&mut self, image: CursorImage, pointer: Point) {
        self.cursor = Some(CursorLayer::new(image, pointer));
        self.cursor_replaced = true;
    }

    /// Move the pointer cursor so its hotspot sits at `pointer`. Returns
    /// `false` when no cursor is shown.
    ///
    /// See [`set_cursor`](Self::set_cursor) for how the eventual damage is
    /// derived.
    pub fn move_cursor(&mut self, pointer: Point) -> bool {
        let Some(cursor) = &mut self.cursor else {
            return false;
        };
        cursor.set_pointer(pointer);
        true
    }

    /// Hide the pointer cursor so the pixels beneath it are restored on the
    /// next composite. Returns `false` when none was shown.
    ///
    /// See [`set_cursor`](Self::set_cursor) for how the eventual damage is
    /// derived.
    pub fn hide_cursor(&mut self) -> bool {
        self.cursor.take().is_some()
    }

    /// The screen rectangle the cursor currently covers, if one is shown.
    #[must_use]
    pub fn cursor_bounds(&self) -> Option<Rect> {
        self.cursor.as_ref().map(CursorLayer::bounds)
    }

    /// Whether any pixels are pending recomposition — either an explicitly
    /// marked rectangle or a cursor move/show/hide/replacement whose damage
    /// has not yet been derived by a [`composite`](Self::composite).
    ///
    /// This answers exactly the question the next
    /// [`composite`](Self::composite) does: it is `true` if and only if
    /// that composite would recompose at least one pixel. A caller driving
    /// a wake loop can therefore skip the frame entirely when it is
    /// `false`, and never miss one when it is `true`. Damage marked wholly
    /// off screen is no pending work: composite clips every rectangle to
    /// the screen, so this clips them too rather than promising a frame
    /// that would recompose nothing.
    #[must_use]
    pub fn has_damage(&self) -> bool {
        let screen = self.screen_rect();
        let on_screen = |rect: Rect| !rect.intersection(&screen).is_empty();
        if self.damage.rects().iter().any(|&rect| on_screen(rect)) {
            return true;
        }
        if !self.cursor_needs_recompose() {
            return false;
        }
        self.cursor_on_screen.is_some_and(on_screen) || self.cursor_bounds().is_some_and(on_screen)
    }

    /// Whether the cursor overlay's pixels differ from the ones the last
    /// [`composite`](Self::composite) drew: it moved, appeared,
    /// disappeared, or its artwork was replaced
    /// ([`set_cursor`](Self::set_cursor)). The rectangle it occupied then
    /// and the one it occupies now are its whole damage, however many
    /// pointer samples were pumped in between.
    fn cursor_needs_recompose(&self) -> bool {
        self.cursor_replaced || self.cursor_bounds() != self.cursor_on_screen
    }

    /// Whether `point` lies within a currently-dirty rectangle. Test-only: it
    /// lets the decoration tests assert that a furniture-only change (a focus
    /// flip, a title edit) confines its damage to the furniture and never
    /// marks the client area dirty.
    #[cfg(test)]
    pub(crate) fn damage_covers(&self, point: Point) -> bool {
        self.damage.covers(point)
    }

    /// Recompose every damaged pixel into the back buffer and the
    /// scan-out frame, then clear the damage. Pixels outside the damage
    /// region keep their previous value (the point of damage tracking).
    ///
    /// Returns the [`DamageRegion`] actually recomposited — the
    /// screen-clipped rectangles every mutation marked dirty since the
    /// last composite, plus the cursor's own damage (see below) — empty
    /// when nothing was dirty. Presenting each of its rectangles individually
    /// (via [`Display::present_region`]) moves bytes proportional to what
    /// changed rather than their bounding box, which can span far more of
    /// the screen than the union of the pixels that actually moved (a
    /// dirty taskbar strip plus a cursor near the opposite edge, say).
    ///
    /// The pointer cursor is not damaged as it moves; instead this method
    /// diffs the cursor's current footprint (and artwork identity) against
    /// what was recorded at the *previous* composite
    /// ([`set_cursor`](Self::set_cursor)'s docs) and damages just the
    /// rectangle it left and the one it is now in, so a whole batch of
    /// pointer samples pumped between two composites costs exactly two
    /// rectangles, not one per sample.
    pub fn composite(&mut self) -> DamageRegion {
        let screen = self.screen_rect();
        let current_cursor = self.cursor_bounds();
        if self.cursor_needs_recompose() {
            if let Some(old) = self.cursor_on_screen {
                self.damage.add(old);
            }
            if let Some(new) = current_cursor {
                self.damage.add(new);
            }
        }
        self.cursor_on_screen = current_cursor;
        self.cursor_replaced = false;

        let damage = core::mem::take(&mut self.damage);
        let mut composited = DamageRegion::new();
        // The root fill is constant for the whole composite; premultiply
        // it once rather than per pixel.
        let base = self.background.premultiply();
        // Bring every covering window's furniture into the cache before the
        // walk below reads it: the read path holds several windows' strips
        // borrowed at once, which the exclusive borrow a build needs cannot
        // express. Doing it once for the whole composite also means a
        // window covered by two damaged rectangles is rendered once, and
        // that its recency records one composite rather than one per
        // rectangle.
        let fallback = self.ensure_chrome(|window| {
            damage
                .rects()
                .iter()
                .any(|dirty| covers(window, dirty.intersection(&screen)))
        });
        // Reused across rectangles so a multi-rectangle composite makes
        // no per-rectangle allocation on this hot path.
        let mut hits: Vec<usize> = Vec::new();
        for &dirty in damage.rects() {
            let area = dirty.intersection(&screen);
            if area.is_empty() {
                continue;
            }
            composited.add(area);
            // Only a window whose bounds overlap this rectangle can
            // contribute a pixel inside it; every other window's sample
            // is unconditionally `None` here, so skipping it is exact
            // (bit-for-bit identical output) and turns the per-pixel
            // window scan from "all windows" into "the few that overlap".
            hits.clear();
            for (index, window) in self.windows.iter().enumerate() {
                if covers(window, area) {
                    hits.push(index);
                }
            }
            self.recompose_rect(area, base, &hits, &fallback);
        }
        composited
    }

    /// Make the rendered furniture of every window `wanted` selects
    /// available for the immutable pass that follows, returning whatever
    /// the cache would not retain.
    ///
    /// Residency is established here, under the exclusive borrow a build
    /// needs, so the pass itself can read several windows' strips at once
    /// through [`ReclaimCache::peek`]. Touching each key here is also what
    /// keeps eviction honest: the least-recently-*composited* window is the
    /// one that goes first, so a minimised or fully-covered window's
    /// furniture is given back before a visible window's.
    ///
    /// A refusal is not a failure. The cache is an accelerator, so anything
    /// it declines — because the budget is exhausted, pressure forbids
    /// growth, or it is poisoned — is built for this pass alone and
    /// returned to the caller, which draws from it exactly as it would from
    /// a retained entry. An entry admitted and then evicted by a later
    /// window in the same pass is caught the same way.
    fn ensure_chrome(&mut self, wanted: impl Fn(&Window) -> bool) -> ChromeFallback {
        let epoch = self.chrome_epoch();
        let scale = self.scale;
        let Self {
            theme,
            windows,
            chrome,
            ..
        } = self;
        let mut fallback = ChromeFallback::new();
        for window in windows.iter().filter(|w| w.is_decorated() && wanted(w)) {
            let id = window.id();
            if let Some(Served::Uncached(built)) =
                chrome.get_or_build(&epoch, id, || window.render_chrome(scale, theme))
            {
                fallback.push((id, built));
            }
        }
        for window in windows.iter().filter(|w| w.is_decorated() && wanted(w)) {
            let id = window.id();
            if chrome.peek(&epoch, &id).is_some() || fallback.iter().any(|(key, _)| *key == id) {
                continue;
            }
            if let Some(built) = window.render_chrome(scale, theme) {
                fallback.push((id, built));
            }
        }
        fallback
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

    /// Composite any pending damage and present it to `display`.
    ///
    /// No damage means nothing changed since the last present, so this
    /// does nothing at all and does not call `display` — a wake that
    /// changed nothing must not cost a scan-out copy or a driver blit.
    /// A composited region spanning the whole screen takes the full
    /// [`Display::present`] path. Otherwise each composited rectangle is
    /// handed to [`Display::present_region`] individually, so the bytes
    /// moved are proportional to what actually changed rather than the
    /// bounding box of a scattered set of dirty rectangles (a taskbar
    /// strip and a cursor near the opposite edge, say) — unless there are
    /// more than [`MAX_PRESENT_REGIONS`] of them, in which case a single
    /// bounding-box present replaces the whole batch (see its docs for
    /// why). Whatever is finally presented — a single rectangle from the
    /// per-rectangle path or the fallback bounding box — still takes the
    /// full-screen path if it turns out to cover the whole screen.
    ///
    /// # Errors
    ///
    /// Propagates any [`DriverError`] the display driver returns from
    /// [`Display::present`] / [`Display::present_region`].
    pub fn present(&mut self, display: &mut dyn Display) -> Result<(), DriverError> {
        let region = self.composite();
        if region.is_empty() {
            return Ok(());
        }
        let Some(bounding_damage) = sub_screen_damage(&region.bounds(), &self.mode) else {
            return display.present(&self.frame);
        };
        let rects = region.rects();
        if rects.len() > MAX_PRESENT_REGIONS {
            return display.present_region(&self.frame, bounding_damage);
        }
        for &rect in rects {
            match sub_screen_damage(&rect, &self.mode) {
                Some(damage) => display.present_region(&self.frame, damage)?,
                None => display.present(&self.frame)?,
            }
        }
        Ok(())
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
        // Every visible window becomes its own layer here, so every one of
        // them needs its furniture available before the immutable encode.
        let fallback = self.ensure_chrome(|_| true);
        if let Some(buffers) = self.encode_layers(&caps, &fallback) {
            let layers: Vec<AccelLayer<'_>> = buffers.iter().map(LayerBuf::as_layer).collect();
            display.present_layers(&layers)
        } else {
            self.composite();
            display.present(&self.frame)
        }
    }

    /// Encode the current scene as hardware layers, or `None` if the
    /// engine's [`AccelCaps`] cannot serve it (the caller falls back to
    /// software). `fallback` carries the furniture the cache would not
    /// retain for this pass.
    fn encode_layers(&self, caps: &AccelCaps, fallback: &ChromeFallback) -> Option<Vec<LayerBuf>> {
        let epoch = self.chrome_epoch();
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
            let chrome = resolve_chrome(&self.chrome, &epoch, window.id(), fallback);
            layers.push(self.encode_layer(
                bounds.width,
                bounds.height,
                bounds.left(),
                bounds.top(),
                |lx, ly| window.sample_local(lx, ly, chrome),
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

    /// Apply `change` to the window named by `id` and mark the union of
    /// its bounds before and after dirty, but only when `change` reports
    /// it actually changed the window — an unknown `id` still returns
    /// `false`. A no-op update (a move to the same origin, corners set to
    /// what they already were, a visibility flip to the current value)
    /// therefore repaints nothing: the caller learns the true outcome
    /// only by attempting the change, exactly as [`edit_window_surface`]
    /// learns a content edit's true damage only by performing it.
    ///
    /// [`edit_window_surface`]: Self::edit_window_surface
    fn mutate(&mut self, id: WindowId, change: impl FnOnce(&mut Window) -> bool) -> bool {
        let Some(window) = self.windows.iter_mut().find(|w| w.id() == id) else {
            return false;
        };
        let before = window.bounds();
        if !change(window) {
            return true;
        }
        let after = window.bounds();
        self.damage.add(before);
        self.damage.add(after);
        true
    }

    /// Recompute every pixel of screen rectangle `area` (already clipped
    /// to the screen) and write it to the back buffer and the encoded
    /// frame. `hits` is the index, into `self.windows`, of every window
    /// whose bounds overlap `area` — the only windows that can contribute
    /// a pixel here.
    ///
    /// Everything that is constant across a row is resolved before the
    /// column loop: each covering layer's source row ([`Window::row`]),
    /// the destination row of the back buffer, and the destination row of
    /// the encoded frame. A column is then a slice index and a blend,
    /// rather than a coordinate conversion, a layer decision, and a
    /// `y * stride + x * 4` offset recomputed for every pixel — which is
    /// what made the previous per-pixel dispatch cost several times the
    /// arithmetic it actually needed. The one allocation is the row-view
    /// list, made once per damaged rectangle and refilled per row, never
    /// per pixel.
    fn recompose_rect(
        &mut self,
        area: Rect,
        base: Pixel,
        hits: &[usize],
        fallback: &ChromeFallback,
    ) {
        let epoch = self.chrome_epoch();
        let Self {
            mode,
            order,
            windows,
            cursor,
            chrome,
            back,
            frame,
            ..
        } = self;
        let stride = mode.stride_bytes as usize;
        let order = *order;
        let windows: &[Window] = windows;
        let cursor = cursor.as_ref();
        let (Ok(first_col), Ok(cols)) = (usize::try_from(area.left()), usize::try_from(area.width))
        else {
            return;
        };
        let (Some(last_col), Some(first_byte), Some(row_bytes)) = (
            first_col.checked_add(cols),
            first_col.checked_mul(4),
            cols.checked_mul(4),
        ) else {
            return;
        };
        // Which window draws here and which furniture it draws from are
        // both fixed for the whole rectangle, so the cache lookups happen
        // once here rather than once per scanline.
        let mut sources: Vec<(&Window, Option<&WindowChrome>)> = Vec::with_capacity(hits.len());
        sources.extend(hits.iter().filter_map(|&index| {
            let window = windows.get(index)?;
            Some((
                window,
                resolve_chrome(chrome, &epoch, window.id(), fallback),
            ))
        }));
        let mut rows: Vec<WindowRow<'_>> = Vec::with_capacity(hits.len());
        for y in area.top()..area.bottom() {
            let Ok(py) = u32::try_from(y) else { continue };
            rows.clear();
            rows.extend(
                sources
                    .iter()
                    .filter_map(|(window, chrome)| window.row(y, *chrome)),
            );
            let cursor_row = cursor.and_then(|c| c.local_row(y));
            let Some(back_row) = back
                .row_mut(py)
                .and_then(|row| row.get_mut(first_col..last_col))
            else {
                continue;
            };
            let Some(frame_row) = (py as usize)
                .checked_mul(stride)
                .and_then(|row_start| row_start.checked_add(first_byte))
                .and_then(|start| frame.get_mut(start..start.checked_add(row_bytes)?))
            else {
                continue;
            };
            let (frame_pixels, _) = frame_row.as_chunks_mut::<4>();
            for ((dst, bytes), x) in back_row
                .iter_mut()
                .zip(frame_pixels)
                .zip(area.left()..area.right())
            {
                let mut acc = base;
                for row in &rows {
                    if let Some(src) = row.sample(x) {
                        acc = src.over(acc);
                    }
                }
                if let Some(ly) = cursor_row {
                    if let Some(src) = cursor.and_then(|c| c.sample_row(x, ly)) {
                        acc = src.over(acc);
                    }
                }
                *dst = acc;
                *bytes = encode_pixel(order, acc);
            }
        }
    }
}

/// Whether `window` can contribute a pixel inside `area`: it is visible
/// and its outer bounds overlap. Every other window's sample there is
/// unconditionally `None`, so skipping it is exact.
fn covers(window: &Window, area: Rect) -> bool {
    window.is_visible() && !window.bounds().intersection(&area).is_empty()
}

/// The furniture to draw window `id` from: the retained entry, or the one
/// this pass built because the cache would not keep it.
///
/// `None` means the window has none to draw — it is undecorated, or its
/// frame could not be rendered at all — which draws the client alone rather
/// than failing the frame.
fn resolve_chrome<'a>(
    cache: &'a ReclaimCache<WindowId, WindowChrome, ChromeEpoch>,
    epoch: &ChromeEpoch,
    id: WindowId,
    fallback: &'a ChromeFallback,
) -> Option<&'a WindowChrome> {
    cache.peek(epoch, &id).or_else(|| {
        fallback
            .iter()
            .find(|(key, _)| *key == id)
            .map(|(_, chrome)| chrome)
    })
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

/// The most rectangles [`Compositor::present`] will hand to
/// [`Display::present_region`] individually before falling back to a
/// single bounding-box present.
///
/// Each `present_region` call is a synchronous IPC round trip to the
/// display service: its fixed dispatch cost (marshalling the message,
/// the context switch into the driver and back) is paid once per call no
/// matter how few pixels the rectangle covers, while the *marginal* cost
/// of a larger copy is just more bytes memcpy'd — the measured whole
/// 1024×768×4 frame copy this crate optimises away (~108 microseconds
/// for almost 3.2 MiB) puts that marginal cost at a small fraction of a
/// microsecond per extra kilobyte. The fixed per-call cost therefore
/// dominates well before the combined bounding box grows large: past a
/// handful of rectangles, the sum of their round trips costs more than
/// one call that copies their (larger) bounding box in a single trip.
/// Eight keeps the common cases — a moved window plus the cursor, a
/// couple of repainted widgets — on the cheap per-rectangle path while
/// capping a pathological scattered-damage frame at one round trip
/// instead of dozens.
pub const MAX_PRESENT_REGIONS: usize = 8;

/// Convert composited damage `bounds` into the [`DamageRect`] a
/// [`Display::present_region`] call carries, or `None` when the full
/// [`Display::present`] path should run instead: a bounds covering the
/// whole screen, or (defensively; every caller already screen-clips and
/// skips empty rectangles) a degenerate empty one. The bounds are already
/// clipped to the screen, so the coordinate conversions cannot fail; a
/// rectangle that nevertheless does not fit falls back to the full
/// present rather than presenting a wrong region.
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
