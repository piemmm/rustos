//! The software compositor.
//!
//! A [`Compositor`] owns a stack of [`Window`]s (bottom-to-top
//! z-order), a screen-sized back buffer, and the [`Region`] that
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
    AccelCaps, AccelLayer, AcceleratedDisplay, Display, DisplayMode,
};
use tairix_abi::DriverError;
use tairix_display::{scanout_len, sub_screen_damage, ChannelOrder};

use tairix_controls::{FurniturePart, TitleBarEvent, WindowFrame};
use tairix_cursor::{CursorImage, PlacedCursor};
use tairix_icon::IconKind;
use tairix_input::{InputEvent, Key};
use tairix_raster::BlurScratch;
use tairix_reclaim::{CacheAccounting, PressureBand, PressureGauge, ReclaimCache, Served};
use tairix_theme::{CursorKind, Theme};

use crate::chrome::{ChromeEpoch, WindowChrome};
use crate::color::{div255, Color, Pixel};
use crate::corner::Corners;
use crate::frost::{FrostEpoch, FrostedBackdrop};
use crate::geometry::{Point, Rect, Region, Scale};
use crate::stats::{area_px, FrameCounters, FrameStats};
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
    /// Every backdrop-blurred window's frosted backdrop, bounded and
    /// released on the same terms (see [`crate::frost`]). A window's own
    /// repaint therefore costs a row copy instead of two blur passes over
    /// the whole of it.
    frost: ReclaimCache<WindowId, FrostedBackdrop, FrostEpoch>,
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
    /// The desktop's own layer: the session's wallpaper-and-icons surface,
    /// anchored at the screen origin and composited over the root fill but
    /// *under* every window. It is deliberately not a [`Window`]: it has no
    /// id, so it can never be raised, focused, moved, or restacked, and
    /// nothing in the ordinary z-order can end up beneath it by accident.
    desktop: Option<Surface>,
    windows: Vec<Window>,
    cursor: Option<PlacedCursor>,
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
    /// How much of the composed screen reaches scan-out: [`u8::MAX`] is the
    /// screen as composed, `0` is black (see [`set_reveal`](Compositor::set_reveal)).
    reveal: u8,
    back: Surface,
    frame: Vec<u8>,
    /// Working buffers for a backdrop frost, owned by the compositor and
    /// grown to the largest frosted rectangle a frame has needed, so a
    /// frosted window costs no allocation once it has been drawn once.
    blur_scratch: BlurScratch,
    /// Frosts the composite in flight computed, held until its pass is over
    /// and then handed to [`frost`](Self::frost).
    ///
    /// Admitting one mid-pass could evict an entry the pass had already
    /// decided to reuse; that reuse would become a recompute, and it would
    /// blur a rectangle whose lower layers the frame never composed. The
    /// cache is therefore read-only for the whole pass and written once at
    /// the end of it.
    pending_frost: Vec<(WindowId, FrostedBackdrop)>,
    /// What the frame in flight decided about each window's retained frost,
    /// by z-index: `None` until it has been asked, then whether the frame
    /// may copy the retained one.
    ///
    /// The plan's fixed-point sweep and the composite that follows it both
    /// need the answer, and asking twice would both cost a second lookup and
    /// risk the two reading differently. Reset by each
    /// [`compose_plan`](Self::compose_plan), so an index can never carry a
    /// decision taken about a window that has since moved or gone.
    frost_decision: Vec<Option<bool>>,
    damage: Region,
    /// What the frame in flight has cost so far, reset by each
    /// [`composite`](Compositor::composite) and read back through
    /// [`frame_stats`](Compositor::frame_stats).
    stats: FrameCounters,
    /// Whether the opaque-run copy path may serve a row. Only a test turns it
    /// off, to compose a scene the general way and prove the two agree byte
    /// for byte; production has no reason to and no way to.
    #[cfg(test)]
    opaque_runs: bool,
    /// Whether a retained frost may be reused instead of blurred again. Only
    /// a test turns it off, to compose the same scene both ways and prove
    /// they agree byte for byte; production has no reason to and no way to.
    #[cfg(test)]
    frost_reuse: bool,
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
    /// `chrome` and `frost` are the bounded, pressure-governed caches this
    /// output retains its decorated windows' rendered furniture and its
    /// backdrop-blurred windows' frosted backdrops in. They are handed in
    /// rather than built here ([`chrome_cache`](crate::chrome_cache) and
    /// [`frost_cache`](crate::frost_cache) are the one place each is
    /// assembled) because only the embedder knows the real output size, the
    /// owning seat, and the process's live pressure gauge and audit sink; a
    /// cache built without them would serve every lookup correctly while
    /// retaining nothing.
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
        frost: ReclaimCache<WindowId, FrostedBackdrop, FrostEpoch>,
        pressure: &'static (dyn PressureGauge + 'static),
    ) -> Option<Self> {
        let order = ChannelOrder::for_format(mode.format)?;
        let background = Color {
            a: 255,
            ..background
        };
        let back = Surface::filled(mode.width_px, mode.height_px, background.premultiply())?;
        let frame = vec![0u8; scanout_len(&mode)?];
        let mut compositor = Self {
            mode,
            scale: Scale::ONE,
            theme: Theme::dark(),
            theme_generation: 0,
            chrome,
            frost,
            pressure,
            pending_redraws: Vec::new(),
            background,
            order,
            desktop: None,
            windows: Vec::new(),
            cursor: None,
            cursor_on_screen: None,
            cursor_replaced: false,
            reveal: u8::MAX,
            back,
            frame,
            blur_scratch: BlurScratch::new(),
            pending_frost: Vec::new(),
            frost_decision: Vec::new(),
            damage: Region::new(),
            stats: FrameCounters::new(),
            #[cfg(test)]
            opaque_runs: true,
            #[cfg(test)]
            frost_reuse: true,
            next_id: 1,
        };
        let screen = compositor.screen_rect();
        compositor.mark(screen);
        Some(compositor)
    }

    /// The active display mode.
    #[must_use]
    pub const fn mode(&self) -> DisplayMode {
        self.mode
    }

    /// Adopt a new display mode, rebuilding the back buffer and scan-out
    /// frame for it and marking the whole screen for redraw.
    ///
    /// Served windows survive: they keep their positions and are clipped to
    /// whatever screen the new mode describes, so a session resumed onto a
    /// different monitor keeps its apps rather than losing them.
    ///
    /// The screen reveal ([`set_reveal`](Self::set_reveal)) carries over
    /// untouched: a session fading in that changes mode mid-fade keeps
    /// fading, and the whole-screen redraw below re-encodes every pixel at
    /// the strength in force.
    ///
    /// Returns `false` — leaving the compositor exactly as it was, still
    /// able to draw the old mode — when the new one cannot be adopted: a
    /// pixel format with no software encoding, a stride too small for one
    /// scanline, or buffers that could not be allocated. The caller decides
    /// what to do about a display it cannot draw; a half-adopted mode would
    /// scan out garbage.
    pub fn set_mode(&mut self, mode: DisplayMode) -> bool {
        if mode == self.mode {
            return true;
        }
        let Some(order) = ChannelOrder::for_format(mode.format) else {
            return false;
        };
        let Some(frame_len) = scanout_len(&mode) else {
            return false;
        };
        let Some(back) =
            Surface::filled(mode.width_px, mode.height_px, self.background.premultiply())
        else {
            return false;
        };
        let mut frame = Vec::new();
        if frame.try_reserve_exact(frame_len).is_err() {
            return false;
        }
        frame.resize(frame_len, 0);
        self.mode = mode;
        self.order = order;
        self.back = back;
        self.frame = frame;
        // The frost scratch is sized per use; releasing it now returns the
        // old screen's worth of pixels rather than carrying them until the
        // next frosted frame. Retained frosts belong to the old screen and
        // are dropped by the epoch this mode is part of.
        self.blur_scratch.release();
        self.damage.clear();
        let screen = self.screen_rect();
        self.mark(screen);
        true
    }

    /// The whole-screen rectangle.
    #[must_use]
    pub fn screen_rect(&self) -> Rect {
        Rect::new(0, 0, self.mode.width_px, self.mode.height_px)
    }

    /// Mark `rect` for recomposition, and drop the retained frost of every
    /// backdrop-blurred window whose backdrop the change may have altered.
    ///
    /// This is the conservative mark, for a change that is not confined to
    /// one layer: the root fill, the desktop layer, the density or the theme
    /// every window is drawn with, and any restacking, which changes *which*
    /// layers a frost sees rather than what one of them holds. A frost blurs
    /// whatever the layers beneath it composed, so a change under one
    /// invalidates it; losing a frost costs a re-blur and never a wrong
    /// pixel, so marking too widely is the safe direction.
    fn mark(&mut self, rect: Rect) {
        self.mark_from(rect, 0);
    }

    /// Mark `rect` for recomposition after a change confined to the window
    /// `id` names: its content, its position, its size, its shape, or its
    /// furniture.
    ///
    /// A frosted window's pixels are blended over a blur of the layers
    /// **below** it, so anything one window draws belongs to the frost of
    /// every window stacked above it, and to neither its own nor any below.
    /// That is what leaves the two dominant interactions — the pointer moving
    /// inside a frosted terminal, and a window dragged across one — costing
    /// no re-blur at all. The window's *own* frost is not spared by this: if
    /// it moved or resized, its rectangle no longer matches the retained one,
    /// which a lookup checks for itself and refuses.
    ///
    /// A window that is no longer in the stack cannot be placed in it, so an
    /// unknown `id` marks conservatively.
    fn mark_layer(&mut self, id: WindowId, rect: Rect) {
        match self.index_of(id) {
            Some(index) => self.mark_from(rect, index.saturating_add(1)),
            None => self.mark(rect),
        }
    }

    /// Mark `rect` for recomposition for a change no frost can read: the
    /// cursor overlay, composed after every window, and the screen reveal,
    /// which is applied only as a composed pixel is encoded for scan-out.
    ///
    /// Neither alters the back buffer beneath a frosted window, so a pointer
    /// sample and a fade step keep every retained frost.
    fn mark_overlay(&mut self, rect: Rect) {
        let above_every_window = self.windows.len();
        self.mark_from(rect, above_every_window);
    }

    /// Mark `rect` and invalidate the retained frost of every window from
    /// z-index `from` upwards whose bounds it reaches.
    ///
    /// Windows below `from` are not consulted at all, and a window whose
    /// bounds the rectangle misses cannot have read a changed pixel: a frost
    /// replicates its rectangle's edges rather than sampling past them.
    fn mark_from(&mut self, rect: Rect, from: usize) {
        self.damage.add(rect);
        self.invalidate_frosts_from(rect, from);
    }

    /// Where the window `id` names sits in the stack, back to front.
    fn index_of(&self, id: WindowId) -> Option<usize> {
        self.windows.iter().position(|window| window.id() == id)
    }

    /// Drop the retained frost of every window from z-index `from` upwards
    /// whose bounds `rect` reaches.
    ///
    /// Separate from [`mark_from`](Self::mark_from) because the composite pass
    /// widens its own plan and must invalidate without marking: its damage has
    /// already been taken, and adding to the next frame's would leave the
    /// desktop repainting for ever.
    fn invalidate_frosts_from(&mut self, rect: Rect, from: usize) {
        if rect.is_empty() || self.frost.is_empty() {
            return;
        }
        let Self {
            windows,
            frost,
            frost_decision,
            ..
        } = self;
        for (index, window) in windows.iter().enumerate().skip(from) {
            if !window.bounds().intersection(&rect).is_empty() {
                frost.invalidate(&window.id());
                // A frame that has already been told it may reuse this one is
                // told otherwise here, rather than asking the cache a second
                // time for an answer this call has just determined.
                if let Some(decision) = frost_decision.get_mut(index) {
                    *decision = Some(false);
                }
            }
        }
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
        self.mark(self.screen_rect());
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
        self.mark(self.screen_rect());
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

    /// The generation every retained [`FrostedBackdrop`] is valid for: this
    /// output's scale and the screen extent frosts were clipped to.
    fn frost_epoch(&self) -> FrostEpoch {
        (
            self.scale.percent(),
            self.mode.width_px,
            self.mode.height_px,
        )
    }

    /// The physical radius `window`'s backdrop is blurred by: its logical
    /// radius through this output's scale, resolved here alone so the blur,
    /// the cache lookup, and a retained entry's own check cannot disagree.
    fn blur_radius_px(&self, window: &Window) -> u32 {
        self.scale.scale_length(u32::from(window.blur_radius()))
    }

    /// Whether the frame in flight may copy the retained frost of the window
    /// at z-index `index` instead of blurring its backdrop again.
    ///
    /// Asked of the cache once per frame and remembered
    /// ([`frost_decision`](Self::frost_decision)), because the plan and the
    /// composite that follows it both need the answer and two lookups could
    /// return two different ones — which would leave a window the plan did
    /// not widen for being blurred over a rectangle whose lower layers the
    /// frame never composed, and it would seam.
    ///
    /// This is the one counted lookup: a hit is a frost the frame goes on to
    /// copy, a miss one it has to blur, and the recency the lookup touches is
    /// what keeps a frost every frame reuses ahead of one nothing has looked
    /// at when the band forces an eviction.
    fn frost_reusable(&mut self, index: usize) -> bool {
        if let Some(Some(decided)) = self.frost_decision.get(index).copied() {
            return decided;
        }
        let decided = self.ask_frost(index);
        if let Some(slot) = self.frost_decision.get_mut(index) {
            *slot = Some(decided);
        }
        decided
    }

    /// Ask the cache whether it still holds the frost the window at z-index
    /// `index` needs, counting the lookup.
    ///
    /// A retained entry whose rectangle, radius, or shape no longer match is
    /// released *before* the lookup rather than rejected after it, so the
    /// frame counts one honest miss and the superseded pixels stop being
    /// charged at once instead of waiting to be evicted. Eviction under
    /// pressure is likewise the lookup's own answer: an entry the band takes
    /// as this call enforces it simply reads as absent.
    fn ask_frost(&mut self, index: usize) -> bool {
        #[cfg(test)]
        if !self.frost_reuse {
            return false;
        }
        let Some(window) = self.windows.get(index) else {
            return false;
        };
        let (id, shape) = (window.id(), window.shape());
        let bounds = window.bounds();
        let radius_px = self.blur_radius_px(window);
        let epoch = self.frost_epoch();
        if self
            .frost
            .peek(&epoch, &id)
            .is_some_and(|retained| !retained.matches(bounds, radius_px, shape))
        {
            self.frost.invalidate(&id);
        }
        self.frost.get_or_build(&epoch, id, || None).is_some()
    }

    /// Frosted backdrops currently retained, one entry per backdrop-blurred
    /// window that has been composited and not since invalidated.
    #[must_use]
    pub fn frost_cache_len(&self) -> usize {
        self.frost.len()
    }

    /// Whether the window named by `id` has a frost retained at the current
    /// epoch — which entry survived, not merely how many did.
    #[cfg(test)]
    pub(crate) fn frost_resident(&self, id: WindowId) -> bool {
        self.frost.peek(&self.frost_epoch(), &id).is_some()
    }

    /// Bytes the frost cache currently has charged: retained frosted pixels
    /// plus its own per-entry bookkeeping.
    #[must_use]
    pub fn frost_cache_bytes(&self) -> usize {
        self.frost.charged_bytes()
    }

    /// The frost cache's byte ledger and event counters, for diagnostics.
    #[must_use]
    pub fn frost_cache_stats(&self) -> &CacheAccounting {
        self.frost.accounting()
    }

    /// Give back whatever the current memory-pressure band says retained
    /// frosts may keep, returning the bytes released.
    ///
    /// A released frost is blurred again on demand, so this costs blur work
    /// and never a wrong pixel. A band that demands nothing releases nothing.
    pub fn trim_frost(&mut self) -> usize {
        self.frost.enforce_pressure()
    }

    /// Release and wipe every retained frost, because the seat this output
    /// belongs to is going away.
    ///
    /// A frost is a blurred image of whatever the user had on screen, so the
    /// pixels are overwritten rather than merely dropped. The cache stays
    /// usable — a later composite blurs what it needs again.
    pub fn teardown_frost(&mut self) {
        self.frost.teardown();
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
                self.mark_layer(id, bounds);
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
        self.mark(self.screen_rect());
        true
    }

    /// How much of the composed screen currently reaches scan-out:
    /// [`u8::MAX`] for all of it, `0` for a black screen.
    #[must_use]
    pub const fn reveal(&self) -> u8 {
        self.reveal
    }

    /// Scale every presented pixel towards black by `strength`, returning
    /// whether it changed.
    ///
    /// [`u8::MAX`] presents the composed screen exactly as it is and costs
    /// nothing; `0` presents black; between the two the screen appears
    /// through it. This is how a session reveals its desktop from black
    /// instead of snapping it on, and it is applied once, where a composed
    /// pixel is encoded into the scan-out frame — the back buffer keeps the
    /// true composed colour, so a frosted window's backdrop and a
    /// multi-segment rectangle cannot be dimmed twice.
    ///
    /// A change repaints the whole screen because every pixel's presented
    /// value changed; setting the strength already in force damages nothing.
    pub fn set_reveal(&mut self, strength: u8) -> bool {
        if strength == self.reveal {
            return false;
        }
        self.reveal = strength;
        self.mark_overlay(self.screen_rect());
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

    /// Install `surface` as the desktop layer, replacing any previous one,
    /// and mark both footprints dirty.
    ///
    /// The desktop layer is anchored at the screen origin and composited over
    /// the opaque root fill but beneath every window, so nothing in the
    /// ordinary z-order can cover it by accident: it carries no
    /// [`WindowId`], which is precisely why it cannot be raised, focused,
    /// moved, or restacked. A surface smaller than the screen simply leaves
    /// the root fill showing where it does not reach, and one larger is
    /// clipped — the layer is never a reason to fail a frame.
    pub fn set_desktop(&mut self, surface: Surface) {
        if let Some(previous) = self.desktop_bounds() {
            self.mark(previous);
        }
        self.desktop = Some(surface);
        if let Some(current) = self.desktop_bounds() {
            self.mark(current);
        }
    }

    /// Repaint the desktop layer in place through `paint`, keeping the
    /// screen-sized buffer it is already drawn into, and mark its footprint
    /// dirty.
    ///
    /// The desktop is repainted whenever its owner's model changes — an icon
    /// takes the hover, a selection moves, the folder re-lists — which is
    /// often, and the layer is a whole screen of pixels. Handing the existing
    /// buffer back to the painter means those repaints cost a paint, not a
    /// paint plus a multi-megabyte allocation the heap may refuse. A layer
    /// that is absent, or sized for a screen this output no longer has, is
    /// allocated fresh at the current screen size; `paint` then always sees a
    /// surface of exactly [`screen_rect`](Self::screen_rect)'s extent, and
    /// receives it exactly as the previous frame left it (it is the painter's
    /// job to lay down its own background, which is cheaper than a clear this
    /// method cannot know is redundant).
    ///
    /// Returns `false` — having changed and damaged nothing — when no such
    /// surface could be allocated, so a heap that will not give back a screen
    /// of pixels leaves the desktop exactly as it was rather than blanking it.
    pub fn repaint_desktop(&mut self, paint: impl FnOnce(&mut Surface)) -> bool {
        let screen = self.screen_rect();
        let fits = self
            .desktop
            .as_ref()
            .is_some_and(|s| s.width() == screen.width && s.height() == screen.height);
        if !fits {
            let Some(fresh) = Surface::new(screen.width, screen.height) else {
                return false;
            };
            self.set_desktop(fresh);
        }
        let Some(surface) = self.desktop.as_mut() else {
            return false;
        };
        paint(surface);
        if let Some(covered) = self.desktop_bounds() {
            self.mark(covered);
        }
        true
    }

    /// Take the desktop layer down, marking what it covered dirty. Returns
    /// `false` when none was installed (nothing to do, nothing damaged).
    pub fn clear_desktop(&mut self) -> bool {
        let Some(covered) = self.desktop_bounds() else {
            return false;
        };
        self.desktop = None;
        self.mark(covered);
        true
    }

    /// The screen rectangle the desktop layer covers, or `None` when none is
    /// installed.
    #[must_use]
    pub fn desktop_bounds(&self) -> Option<Rect> {
        self.desktop
            .as_ref()
            .map(|surface| Rect::new(0, 0, surface.width(), surface.height()))
    }

    /// Add `surface` as the top-most window at `origin`, returning its
    /// identifier. The new window's bounds are marked dirty.
    pub fn add_window(&mut self, origin: Point, surface: Surface) -> WindowId {
        let id = WindowId(self.next_id);
        self.next_id += 1;
        let window = Window::new(id, origin, surface);
        let bounds = window.bounds();
        self.windows.push(window);
        self.mark_layer(id, bounds);
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

    /// Set a window's backdrop-blur radius in *logical* pixels (`0`
    /// disables it); its bounds are marked dirty. Setting the radius it
    /// already has marks no damage and still returns `true` (only an
    /// unknown `id` returns `false`).
    ///
    /// A blurred window's pixels blend over a blurred copy of everything
    /// composited behind its rectangle, so a translucent window reads like
    /// frosted glass. The radius is a desktop length: the compositor
    /// resolves it to physical pixels through this output's
    /// [`scale`](Self::scale), so the frosting looks the same at every
    /// display density.
    pub fn set_backdrop_blur(&mut self, id: WindowId, radius_px: u16) -> bool {
        let known = self.mutate(id, |w| w.set_backdrop_blur(radius_px));
        if known && radius_px == 0 {
            // A window that no longer frosts is never asked about again, so
            // nothing would ever notice its retained backdrop had become dead
            // weight. Give the bytes back at the moment it stops needing them.
            self.frost.invalidate(&id);
        }
        known
    }

    /// Whether any visible window asks for a backdrop blur.
    ///
    /// The effect reads the pixels already composited behind a window, so
    /// it exists only in the software composite path: a hardware layer is
    /// composed from its own pixels alone and cannot sample what is behind
    /// it. [`present_accelerated`](Self::present_accelerated) therefore
    /// takes the software path whenever this is `true`.
    #[must_use]
    pub fn has_backdrop_blur(&self) -> bool {
        self.windows
            .iter()
            .any(|w| w.is_visible() && w.blur_radius() > 0)
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

    /// Convert a client's presented frame of `width` × `height` pixels
    /// **into** a window's own content buffer, marking dirty only the
    /// content-local rectangle the conversion reports it changed, and
    /// return the conversion's own value — or `None` for an unknown `id`,
    /// or a buffer of that size that cannot be allocated.
    ///
    /// Unlike [`set_surface`](Self::set_surface), the caller writes into
    /// the window's existing buffer rather than handing over a fresh one,
    /// so a presenter applying a per-frame damage region into the
    /// window's persistent content needs no second copy of the surface to
    /// convert into and clone from. The conversion reports its own damage
    /// rather than the caller declaring one up front, because only the
    /// conversion itself — by comparing each converted pixel against the
    /// one already there — learns which pixels genuinely changed; a
    /// rectangle handed down before it runs would have to be conservative
    /// and repaint pixels that never moved.
    ///
    /// **The presented extent is the buffer's.** The pixels are the
    /// client's, so the client's frame is what its buffer is sized from — a
    /// window-manager resize of the frame *around* the client
    /// ([`resize_window`](Self::resize_window),
    /// [`resize_window_client`](Self::resize_window_client)) never reshapes
    /// them, and a client that has re-rendered at a new size, or whose
    /// pixels were released under memory pressure, gets a buffer to present
    /// into rather than a refusal. Where the buffer was established afresh,
    /// the whole client area is marked dirty, because every pixel of it now
    /// comes from a buffer that carried nothing over.
    ///
    /// The reported `Rect` is in content-surface-local pixels (origin at
    /// the content's top-left). It is translated by the window's content
    /// origin and intersected with its client rectangle
    /// ([`Window::client_rect`]), so an empty rectangle marks nothing and
    /// an over-large one is clipped rather than ever reaching into a
    /// neighbouring window.
    pub fn present_window_content<T>(
        &mut self,
        id: WindowId,
        width: u32,
        height: u32,
        convert: impl FnOnce(&mut Surface) -> (T, Rect),
    ) -> Option<T> {
        let index = self.index_of(id)?;
        let window = self.windows.get_mut(index)?;
        let (content, established) = window.content_for_present(width, height)?;
        let (out, local_damage) = convert(content);
        let client = window.client_rect();
        let screen_damage = if established {
            client
        } else {
            Rect::new(
                client.left().saturating_add(local_damage.left()),
                client.top().saturating_add(local_damage.top()),
                local_damage.width,
                local_damage.height,
            )
            .intersection(&client)
        };
        self.mark_layer(id, screen_damage);
        Some(out)
    }

    /// Raise a window to the top of the z-order; its bounds are marked
    /// dirty. Raising the window already at the front marks no damage and
    /// still returns `true` (only an unknown `id` returns `false`).
    pub fn raise(&mut self, id: WindowId) -> bool {
        let Some(index) = self.index_of(id) else {
            return false;
        };
        if index.saturating_add(1) == self.windows.len() {
            // Already at the front: nothing to restack, nothing to repaint.
            return true;
        }
        let window = self.windows.remove(index);
        let bounds = window.bounds();
        self.windows.push(window);
        // Restacked first: a window's own frosted backdrop is a function of
        // its place in the stack, and marking is what drops it. Only the
        // windows the raise passed changed which layers they see, and after
        // the move they occupy this index upwards; the ones that were already
        // below it see the same stack as before.
        self.mark_from(bounds, index);
        true
    }

    /// Send a window to the bottom of the z-order (put-to-back), keeping it
    /// visible; its bounds are marked dirty so whatever it was covering
    /// recomposites. Returns `false` for an unknown id.
    pub fn lower(&mut self, id: WindowId) -> bool {
        let Some(index) = self.index_of(id) else {
            return false;
        };
        if index == 0 {
            // Already at the back: nothing to restack, nothing to repaint.
            return true;
        }
        let window = self.windows.remove(index);
        let bounds = window.bounds();
        self.windows.insert(0, window);
        self.mark(bounds);
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
            self.mark_layer(id, before);
            self.mark_layer(id, after);
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
        let Some(index) = self.index_of(id) else {
            return false;
        };
        let mut window = self.windows.remove(index);
        self.chrome.invalidate(&id);
        self.frost.invalidate(&id);
        self.pending_redraws.retain(|pending| *pending != id);
        window.release_content();
        // The windows that were above the removed one start at its index now,
        // and they are the only ones whose backdrop lost anything.
        self.mark_from(window.bounds(), index);
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
        self.mark_layer(id, before);
        self.mark_layer(id, after);
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
        self.mark_layer(id, before);
        self.mark_layer(id, after);
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
    /// focus change never triggers a full-window recomposite. Re-asserting the
    /// activation a frame already shows marks no damage and keeps its rendered
    /// furniture.
    pub fn set_active_frame(&mut self, id: WindowId, active: bool) -> bool {
        let Some(changes) = self
            .window(id)
            .and_then(|window| window.frame_activation_changes(active))
        else {
            return false;
        };
        if !changes {
            return true;
        }
        let bands = self.mutate_frame(id, |window, _, _| {
            window
                .set_frame_active(active)
                .then(|| window.furniture_bands())
        });
        let Some(Some(bands)) = bands else {
            return false;
        };
        for band in bands {
            self.mark_layer(id, band);
        }
        true
    }

    /// Set the decorated window named by `id`'s title, repainting the title
    /// bar. Returns `false` for an unknown or undecorated window.
    ///
    /// The title is the untrusted string the window channel already carries
    /// (`WindowTitle`); the title bar sanitises and truncates it. Only the top
    /// (title) furniture band is marked dirty — a title edit never touches the
    /// client or the other frame edges. Setting the label the bar already
    /// reads marks no damage and keeps that window's rendered furniture.
    pub fn set_window_title(&mut self, id: WindowId, title: &str) -> bool {
        let Some(frame) = self.window(id).and_then(Window::frame) else {
            return false;
        };
        if frame.title_bar().title() == title {
            // Sanitising is idempotent, so a title equal to the stored one
            // sanitises to it: there is nothing to relabel or re-render.
            return true;
        }
        let band = self.mutate_frame(id, |window, _, _| {
            window.set_frame_title(title).then(|| window.title_band())
        });
        let Some(Some(band)) = band else {
            return false;
        };
        self.mark_layer(id, band);
        true
    }

    /// Give the decorated window named by `id` the owning application's
    /// identity: `identity` is the icon class its title bar reserves a slot
    /// for, and `artwork` is the picture to draw there. Returns `false` for an
    /// unknown or undecorated window.
    ///
    /// `artwork` must already be rasterised at
    /// [`TitleBar::icon_side`](tairix_controls::TitleBar::icon_side) of this
    /// window's laid-out *title band*, which is what
    /// [`window_title_icon_side`](Self::window_title_icon_side) reports;
    /// `None` draws the built-in glyph for `identity`, so an identified window
    /// is never a blank slot. The identity is the embedder's attestation of
    /// who owns the window, never a string the application supplied.
    ///
    /// Only the top (title) furniture band is marked dirty and only this
    /// window's chrome-cache entry is dropped — an identity is no more
    /// expensive than a retitle.
    pub fn set_window_identity(
        &mut self,
        id: WindowId,
        identity: IconKind,
        artwork: Option<Surface>,
    ) -> bool {
        let band = self.mutate_frame(id, |window, _, _| {
            window
                .set_frame_identity(identity, artwork)
                .then(|| window.title_band())
        });
        let Some(Some(band)) = band else {
            return false;
        };
        self.mark_layer(id, band);
        true
    }

    /// The pixel side the identity icon of the decorated window named by `id`
    /// paints at, or `None` for an unknown or undecorated window.
    ///
    /// This is the size an owner rasterises artwork at before handing it to
    /// [`set_window_identity`](Self::set_window_identity): the compositor
    /// resolves it from the window's own laid-out title band at the active
    /// scale and theme, so a caller never has to reconstruct that geometry.
    #[must_use]
    pub fn window_title_icon_side(&self, id: WindowId) -> Option<u32> {
        let window = self.window(id)?;
        let frame = window.frame()?;
        let band = frame
            .layout(window.bounds(), self.scale, &self.theme)
            .title_bar;
        Some(frame.title_bar().icon_side(band, self.scale, &self.theme))
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
    pub fn frame_pointer(
        &mut self,
        id: WindowId,
        event: &InputEvent,
        damage: &mut Region,
    ) -> Option<TitleBarEvent> {
        let (result, bands) = self.mutate_frame(id, |window, scale, theme| {
            let result = window.on_frame_pointer(event, scale, theme, damage);
            (result, window.furniture_bands())
        })?;
        for band in bands {
            self.mark_layer(id, band);
        }
        result
    }

    /// Feed a key `key` to the decoration furniture of the window named by
    /// `id` (the title bar's command controls), repainting the title band, and
    /// return the typed [`TitleBarEvent`] it produced. Returns `None` for an
    /// unknown or undecorated window.
    pub fn frame_key(
        &mut self,
        id: WindowId,
        key: Key,
        damage: &mut Region,
    ) -> Option<TitleBarEvent> {
        let (result, band) = self.mutate_frame(id, |window, scale, theme| {
            let result = window.on_frame_key(key, scale, theme, damage);
            (result, window.title_band())
        })?;
        self.mark_layer(id, band);
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
            self.mark_layer(id, before);
            self.mark_layer(id, after);
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
            self.mark_layer(id, before);
            self.mark_layer(id, after);
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
        let index = self.index_of(id)?;
        let window = self.windows.get_mut(index)?;
        let bounds = window.bounds();
        let out = change(window.viewport_mut()?);
        self.mark_layer(id, bounds);
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
        self.cursor = Some(PlacedCursor::new(image, pointer));
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
        self.cursor.as_ref().map(PlacedCursor::bounds)
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
        if self.damage.intersects(screen) {
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
        self.damage.contains(point)
    }

    /// Recompose every damaged pixel into the back buffer and the
    /// scan-out frame, then clear the damage. Pixels outside the damage
    /// region keep their previous value (the point of damage tracking).
    ///
    /// Returns the [`Region`] actually recomposited — the
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
    pub fn composite(&mut self) -> Region {
        self.stats.begin_frame();
        self.recompose_damage()
    }

    /// [`composite`](Self::composite) without opening a new frame's counters,
    /// so a present path that already opened one attributes the composite it
    /// drives to that frame rather than starting a second.
    fn recompose_damage(&mut self) -> Region {
        let screen = self.screen_rect();
        let current_cursor = self.cursor_bounds();
        if self.cursor_needs_recompose() {
            if let Some(old) = self.cursor_on_screen {
                self.mark_overlay(old);
            }
            if let Some(new) = current_cursor {
                self.mark_overlay(new);
            }
        }
        self.cursor_on_screen = current_cursor;
        self.cursor_replaced = false;

        let mut damage = core::mem::take(&mut self.damage);
        // Clipping once here is what lets the walk below trust every
        // rectangle it is handed: damage marked wholly off screen composites
        // nothing, which is what `has_damage` promises.
        damage.clip(screen);
        let plan = self.compose_plan(&mut damage, screen);
        let mut composited = Region::new();
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
        let fallback = self.ensure_chrome(|window| plan.iter().any(|&dirty| covers(window, dirty)));
        // Reused across rectangles so a multi-rectangle composite makes
        // no per-rectangle allocation on this hot path.
        let mut hits: Vec<usize> = Vec::new();
        for &area in &plan {
            composited.add(area);
            self.stats.add_damaged(area_px(area.width, area.height));
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
        self.retain_pending_frost();
        composited
    }

    /// Hand the frosts this composite computed to the cache, now that the pass
    /// which might have reused a retained one is over.
    ///
    /// Admitting one mid-pass could evict an entry [`compose_plan`] had
    /// already decided to reuse. That reuse would then find nothing and blur
    /// again — over a rectangle the frame composed the lower layers of only
    /// where the damage happened to fall, because a window planned as reusable
    /// is deliberately *not* widened to its whole bounds. Keeping the cache
    /// read-only for the whole pass is what makes the plan's decision hold.
    ///
    /// [`compose_plan`]: Self::compose_plan
    fn retain_pending_frost(&mut self) {
        if self.pending_frost.is_empty() {
            return;
        }
        let epoch = self.frost_epoch();
        let Self {
            frost,
            pending_frost,
            ..
        } = self;
        for (id, captured) in pending_frost.drain(..) {
            // Offered rather than looked up: this frame has already counted
            // the lookup that found nothing, and a budget or pressure refusal
            // is not a failure — the frame is composed, and the next one
            // blurs again.
            frost.retain(&epoch, id, captured);
        }
    }

    /// The rectangles this composite will recompose: every blurred window whose
    /// frost it must recompute as one whole rectangle, then whatever damage is
    /// left.
    ///
    /// A blurred window's pixels are a function of the *whole* backdrop under
    /// its rectangle, not just the part a caller happened to damage:
    /// recomputing the frost of a strip of it would spread a neighbourhood
    /// clipped to that strip and leave a seam against the pixels around it. So
    /// damage touching such a window promotes the whole of it into a single
    /// rectangle, and that rectangle is *removed* from `damage` — the two sets
    /// stay disjoint, so no pixel is composited twice and the damage outside
    /// the window stays as tight as it was marked. Two blurred windows that
    /// overlap merge into one rectangle, because each reads what the other
    /// wrote.
    ///
    /// **A window whose frost is retained and still valid is not promoted at
    /// all**, because there is no neighbourhood to spread: the pass copies the
    /// retained rectangle back instead of blurring. That is what leaves a
    /// repaint inside a frosted window costing its own few rows rather than
    /// the whole window, and it is the reason the cache is written only after
    /// the pass ([`retain_pending_frost`](Self::retain_pending_frost)).
    ///
    /// Promoting one window can bring the frame into contact with a second, so
    /// the sweep repeats. Recomputing a frost also changes the promoted
    /// window's own pixels across the whole of it — the blur spreads the
    /// change well past the rectangle that caused it — so a frost *above* it
    /// that overlaps is no longer reusable either, and is dropped here for the
    /// next pass to promote. Each pass that grows claims at least one more
    /// window's rectangle for good, so `windows.len()` passes reach the fixed
    /// point, and the common case (no blurred window is touched) settles in
    /// the first.
    ///
    /// `damage` is already screen-clipped and the promoted bounds are clipped
    /// here, so every rectangle returned lies on screen.
    fn compose_plan(&mut self, damage: &mut Region, screen: Rect) -> Vec<Rect> {
        let mut plan: Vec<Rect> = Vec::new();
        self.frost_decision.clear();
        self.frost_decision.resize(self.windows.len(), None);
        for _ in 0..self.windows.len() {
            let mut grown = false;
            for index in 0..self.windows.len() {
                let Some(window) = self.windows.get(index) else {
                    continue;
                };
                if !window.is_visible() || window.blur_radius() == 0 {
                    continue;
                }
                let bounds = window.bounds().intersection(&screen);
                if bounds.is_empty()
                    || plan
                        .iter()
                        .any(|claimed| claimed.intersection(&bounds) == bounds)
                {
                    continue;
                }
                let touched = damage.intersects(bounds)
                    || plan
                        .iter()
                        .any(|claimed| !claimed.intersection(&bounds).is_empty());
                // Whether the cache still holds this window's frost is asked
                // only of a window the frame is going to compose, so an
                // untouched one costs no lookup and counts as neither.
                if !touched || self.frost_reusable(index) {
                    continue;
                }
                let claimed = claim(&mut plan, bounds);
                damage.subtract(claimed);
                self.invalidate_frosts_from(claimed, index.saturating_add(1));
                grown = true;
            }
            if !grown {
                break;
            }
        }
        plan.extend_from_slice(damage.rects());
        plan
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
            stats,
            ..
        } = self;
        // The cache already counts its own hits and misses, so the frame's
        // share of them is the difference across this pass rather than a
        // second tally kept here.
        let before = (chrome.accounting().hits(), chrome.accounting().misses());
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
        let after = (chrome.accounting().hits(), chrome.accounting().misses());
        stats.add_chrome(
            frame_share(before.0, after.0),
            frame_share(before.1, after.1),
        );
        fallback
    }

    /// What the most recent frame cost: pixels damaged, blended, copied,
    /// frosted and encoded, the rectangles it recomposed, the driver calls
    /// that published it, and the furniture cache's hits and misses.
    ///
    /// The counts are reset by each [`composite`](Self::composite) and a
    /// present adds to the frame that composite produced, so a caller that
    /// snapshots after [`present`](Self::present) reads one whole frame. A
    /// wake that composited nothing leaves every count at zero
    /// ([`FrameStats::is_idle`]).
    #[must_use]
    pub const fn frame_stats(&self) -> FrameStats {
        self.stats.snapshot()
    }

    /// Allow or forbid the opaque-run copy path, so a test can compose one
    /// scene both ways and compare the results.
    #[cfg(test)]
    pub(crate) fn set_opaque_runs(&mut self, allowed: bool) {
        self.opaque_runs = allowed;
        self.mark(self.screen_rect());
    }

    /// Allow or forbid reusing a retained frost, so a test can compose one
    /// scene both ways and compare the results.
    ///
    /// Forbidding it also empties the cache, so the scene composed the slow way
    /// cannot be reading a frost the fast way left behind.
    #[cfg(test)]
    pub(crate) fn set_frost_reuse(&mut self, allowed: bool) {
        self.frost_reuse = allowed;
        if !allowed {
            self.frost.teardown();
        }
        self.mark(self.screen_rect());
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
        self.stats.begin_frame();
        let region = self.recompose_damage();
        if region.is_empty() {
            return Ok(());
        }
        let Some(bounding_damage) = sub_screen_damage(&region.bounds(), &self.mode) else {
            self.stats.bump_present();
            return display.present(&self.frame);
        };
        let rects = region.rects();
        if rects.len() > MAX_PRESENT_REGIONS {
            self.stats.bump_present();
            return display.present_region(&self.frame, bounding_damage);
        }
        for &rect in rects {
            self.stats.bump_present();
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
    /// the engine's [`AccelCaps`] cannot hold that many layers, a layer
    /// is larger than the engine can source, or a screen reveal
    /// ([`set_reveal`](Self::set_reveal)) is in flight, the whole frame is
    /// composited in software and presented instead — never a partial
    /// hardware frame.
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
        self.stats.begin_frame();
        // A hardware layer is composed from its own pixels alone and cannot
        // sample what is already behind it, so a backdrop blur has no layer
        // encoding at all and the whole frame goes through software.
        let layers = if self.has_backdrop_blur() {
            None
        } else {
            // Every visible window becomes its own layer here, so every one
            // of them needs its furniture available before the immutable
            // encode.
            let fallback = self.ensure_chrome(|_| true);
            self.encode_layers(&caps, &fallback)
        };
        if let Some(buffers) = layers {
            let layers: Vec<AccelLayer<'_>> = buffers.iter().map(LayerBuf::as_layer).collect();
            self.stats.bump_present();
            display.present_layers(&layers)
        } else {
            self.recompose_damage();
            self.stats.bump_present();
            display.present(&self.frame)
        }
    }

    /// Encode the current scene as hardware layers, or `None` if the
    /// engine's [`AccelCaps`] cannot serve it, or a screen reveal
    /// ([`set_reveal`](Self::set_reveal)) is in flight (the caller falls back
    /// to software either way). `fallback` carries the furniture the cache
    /// would not retain for this pass.
    fn encode_layers(&self, caps: &AccelCaps, fallback: &ChromeFallback) -> Option<Vec<LayerBuf>> {
        // The engine scans a layer out as the driver was handed it, so
        // nothing it composes passes through the reveal and the screen would
        // appear at full strength while the fade ran.
        if self.reveal != u8::MAX {
            return None;
        }
        let epoch = self.chrome_epoch();
        let max_layers = usize::try_from(caps.max_layers).unwrap_or(usize::MAX);
        let mut layers = Vec::new();
        layers.push(
            self.encode_layer(self.mode.width_px, self.mode.height_px, 0, 0, |_, _| {
                Some(self.background.premultiply())
            })?,
        );
        // The desktop layer sits directly on the background, beneath every
        // window, so the hardware result matches the software one.
        if let Some(desktop) = &self.desktop {
            layers.push(
                self.encode_layer(desktop.width(), desktop.height(), 0, 0, |lx, ly| {
                    crate::surface::row(desktop, ly)
                        .get(usize::try_from(lx).ok()?)
                        .copied()
                })?,
            );
        }
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
                    slot.copy_from_slice(&self.order.encode(pixel));
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
    /// only by attempting the change, exactly as
    /// [`present_window_content`] learns a present's true damage only by
    /// converting it.
    ///
    /// [`present_window_content`]: Self::present_window_content
    fn mutate(&mut self, id: WindowId, change: impl FnOnce(&mut Window) -> bool) -> bool {
        let Some(window) = self.windows.iter_mut().find(|w| w.id() == id) else {
            return false;
        };
        let before = window.bounds();
        if !change(window) {
            return true;
        }
        let after = window.bounds();
        self.mark_layer(id, before);
        self.mark_layer(id, after);
        true
    }

    /// Recompute every pixel of screen rectangle `area` (already clipped
    /// to the screen) and write it to the back buffer and the encoded
    /// frame. `hits` is the index, into `self.windows`, of every window
    /// whose bounds overlap `area` — the only windows that can contribute
    /// a pixel here.
    ///
    /// With no backdrop blur in play this is one [`compose_span`] over the
    /// whole layer stack. A blurred window needs the pixels behind it
    /// *before* its own are blended, so the stack is composed in segments
    /// instead: everything below the blurred window is composed first, its
    /// frosted backdrop is put into the back buffer, and the composition
    /// resumes from the blurred window itself over that frosting.
    /// Only the last segment encodes the scan-out frame, so the
    /// intermediate stages cost no wasted encoding.
    ///
    /// [`compose_span`]: Self::compose_span
    fn recompose_rect(
        &mut self,
        area: Rect,
        base: Pixel,
        hits: &[usize],
        fallback: &ChromeFallback,
    ) {
        let mut start = 0;
        let mut under = Some(base);
        for split in 0..hits.len() {
            let Some(index) = hits.get(split).copied() else {
                continue;
            };
            if self.windows.get(index).is_none_or(|w| w.blur_radius() == 0) {
                continue;
            }
            self.compose_span(area, under, hits.get(start..split), fallback, false);
            self.frost_segment(index, area);
            start = split;
            under = None;
        }
        self.compose_span(area, under, hits.get(start..), fallback, true);
    }

    /// Put the frosted backdrop of the window at `index` into the back buffer
    /// where `area` reaches it: the retained one where it is still valid,
    /// otherwise blurred afresh and kept for the next frame.
    ///
    /// The two produce the same bytes — the retained copy *is* the blur's own
    /// output, taken from this buffer when it was last computed — so this is a
    /// specialisation of the blur, never a second frosting. What differs is the
    /// extent: a recompute needs the window's whole rectangle, which the plan
    /// guarantees is inside `area` because it promoted it
    /// ([`compose_plan`](Self::compose_plan)), while a copy needs only the part
    /// of the rectangle this rectangle of damage actually touches.
    fn frost_segment(&mut self, index: usize, area: Rect) {
        let screen = self.screen_rect();
        let reusable = self.frost_reusable(index);
        let Some(window) = self.windows.get(index) else {
            return;
        };
        let (id, shape) = (window.id(), window.shape());
        let bounds = window.bounds();
        let radius_px = self.blur_radius_px(window);
        if reusable {
            let epoch = self.frost_epoch();
            let Self { frost, back, .. } = self;
            if let Some(retained) = frost.peek(&epoch, &id) {
                retained.restore(back, area);
                return;
            }
        }
        self.blur_backdrop(index);
        // Nothing is lost when the copy cannot be taken: the frame is already
        // frosted, and the next one blurs again.
        if let Some(captured) =
            FrostedBackdrop::capture(&self.back, bounds, screen, radius_px, shape)
        {
            self.pending_frost.push((id, captured));
        }
    }

    /// Compose the layers `span` names over screen rectangle `area`,
    /// writing the result to the back buffer and — when `encode` — to the
    /// encoded scan-out frame.
    ///
    /// Encoding is the one point a composed pixel becomes a scan-out byte,
    /// so it is also the one point the screen reveal
    /// ([`set_reveal`](Self::set_reveal)) is applied. The back buffer keeps
    /// the composed colour undimmed, which is what a continuing segment and
    /// a frosted backdrop read.
    ///
    /// `under` is what the layers are composed over: `Some(base)` starts
    /// from the root fill with the desktop layer beneath the windows, while
    /// `None` starts from whatever the back buffer already holds, which is
    /// how a later segment of the same rectangle continues over an earlier
    /// one's (possibly blurred) result. `span` is a sub-slice of the
    /// rectangle's covering-window indices, or `None` for a range that does
    /// not exist, which composes no window at all. The cursor is drawn only
    /// by the encoding segment, so it always lands on top.
    ///
    /// The front-most window of the segment is asked for its **opaque runs**
    /// first ([`WindowRow::opaque_run`]): a run of fully opaque client pixels
    /// replaces whatever is beneath it exactly, so it is copied into the back
    /// buffer and encoded in one pass and every layer below it — the windows
    /// under it, the desktop layer, and the root fill — is skipped for those
    /// columns. That is the whole of the compositor's occlusion handling: it
    /// culls per *run* rather than per window, so a window that covers only
    /// part of a dirty rectangle, or whose own pixels are opaque only in
    /// places, still saves exactly the blending it can. Columns no run covers
    /// take the same [`compose_pixel`] path they always did, over the same
    /// [`Pixel::over`], so this is a loop specialisation and never a second
    /// blend.
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
    fn compose_span(
        &mut self,
        area: Rect,
        under: Option<Pixel>,
        span: Option<&[usize]>,
        fallback: &ChromeFallback,
        encode: bool,
    ) {
        let epoch = self.chrome_epoch();
        #[cfg(test)]
        let opaque_runs = self.opaque_runs;
        #[cfg(not(test))]
        let opaque_runs = true;
        let Self {
            mode,
            order,
            desktop,
            windows,
            cursor,
            chrome,
            reveal,
            back,
            frame,
            stats,
            ..
        } = self;
        let stride = mode.stride_bytes as usize;
        let order = *order;
        let reveal = *reveal;
        let windows: &[Window] = windows;
        // The cursor is the top-most layer, so only the segment that
        // finishes the rectangle draws it.
        let cursor = if encode { cursor.as_ref() } else { None };
        // The desktop sits directly under the windows, so it belongs to
        // the segment that starts from the root fill; a continuing segment
        // finds it already in the back buffer.
        let desktop = under.and(desktop.as_ref());
        let span = span.unwrap_or(&[]);
        let (Ok(first_col), Ok(cols)) = (usize::try_from(area.left()), usize::try_from(area.width))
        else {
            return;
        };
        let (Ok(left), Some(first_byte), Some(row_bytes)) = (
            u32::try_from(area.left()),
            first_col.checked_mul(4),
            cols.checked_mul(4),
        ) else {
            return;
        };
        // Which window draws here and which furniture it draws from are
        // both fixed for the whole rectangle, so the cache lookups happen
        // once here rather than once per scanline.
        let mut sources: Vec<(&Window, Option<&WindowChrome>)> = Vec::with_capacity(span.len());
        sources.extend(span.iter().filter_map(|&index| {
            let window = windows.get(index)?;
            Some((
                window,
                resolve_chrome(chrome, &epoch, window.id(), fallback),
            ))
        }));
        let mut rows: Vec<WindowRow<'_>> = Vec::with_capacity(span.len());
        for y in area.top()..area.bottom() {
            let Ok(py) = u32::try_from(y) else { continue };
            rows.clear();
            rows.extend(
                sources
                    .iter()
                    .filter_map(|(window, chrome)| window.row(y, *chrome)),
            );
            let cursor_row = cursor.and_then(|c| c.local_row(y).map(|ly| (c, ly)));
            let desktop_row = desktop.map(|layer| crate::surface::row(layer, py));
            let Some((_, back_row)) = back.row_span_mut(py, left, area.width) else {
                continue;
            };
            // Resolved even when this segment does not encode, so every
            // segment over one rectangle keeps or skips exactly the same
            // rows and the back buffer can never drift from the frame.
            let Some(frame_row) = (py as usize)
                .checked_mul(stride)
                .and_then(|row_start| row_start.checked_add(first_byte))
                .and_then(|start| frame.get_mut(start..start.checked_add(row_bytes)?))
            else {
                continue;
            };
            // The screen reveal is applied as a pixel is encoded, so a fade
            // in flight has no run a plain copy could serve; the cursor is
            // resolved per row, so only the few rows it draws on lose the
            // fast path.
            let layers = RowLayers {
                under,
                desktop: desktop_row,
                windows: &rows,
                cursor: cursor_row,
                front: rows.last().filter(|_| {
                    opaque_runs && cursor_row.is_none() && (!encode || reveal == u8::MAX)
                }),
            };
            let targets = RowTargets {
                back: back_row,
                frame: encode.then_some(frame_row),
            };
            let work = compose_row(&layers, targets, area.left(), order, reveal);
            if encode {
                stats.add_encoded(u64::from(area.width));
            }
            stats.add_blended(work.blended);
            stats.add_opaque(work.copied);
        }
    }

    /// Frost the back buffer inside the rectangle of the window at `index`,
    /// weighted by that window's own shape coverage, leaving a frosted
    /// backdrop for the window's pixels to be blended over.
    ///
    /// The rectangle is the window's whole on-screen bounds every time —
    /// the compose plan promotes it whenever a frost must be recomputed — so
    /// the frosting a given backdrop produces never depends on which part
    /// of the window a repaint started from. Coverage weights the mix
    /// rather than clipping it, so a rounded corner fades from frosted to
    /// untouched across exactly the arc the window's own pixels fade over
    /// and no square edge shows outside a rounded window.
    ///
    /// The shared frost ([`Surface::frost_region`]) confines the effect to
    /// that rectangle and replicates its edges, so it can never pull a
    /// neighbour's pixels into a window nor write outside its own bounds,
    /// and it works in the scratch this compositor owns and reuses.
    fn blur_backdrop(&mut self, index: usize) {
        let screen = self.screen_rect();
        let radius = self
            .windows
            .get(index)
            .map_or(0, |window| self.blur_radius_px(window));
        let Self {
            windows,
            back,
            blur_scratch,
            stats,
            ..
        } = self;
        let Some(window) = windows.get(index) else {
            return;
        };
        let bounds = window.bounds();
        let region = bounds.intersection(&screen);
        let (Ok(left), Ok(top)) = (u32::try_from(region.left()), u32::try_from(region.top()))
        else {
            return;
        };
        // The frosted rectangle's top-left in the window's own coordinates:
        // a window that starts off screen is frosted from the row and
        // column the screen begins at, and its shape is still read from its
        // own top-left.
        let shape_x = u32::try_from(region.left().saturating_sub(bounds.left())).unwrap_or(0);
        let shape_y = u32::try_from(region.top().saturating_sub(bounds.top())).unwrap_or(0);
        let shape = window.shape();
        stats.add_blur(area_px(region.width, region.height));
        back.frost_region(
            left,
            top,
            region.width,
            region.height,
            radius,
            blur_scratch,
            |lx, ly| {
                shape.map_or(255, |shape| {
                    shape.coverage(shape_x.saturating_add(lx), shape_y.saturating_add(ly))
                })
            },
        );
    }
}

/// One screen row's resolved layers, bottom to top.
struct RowLayers<'a> {
    /// What the layers compose over: the root fill, or `None` to continue
    /// from whatever the back buffer already holds.
    under: Option<Pixel>,
    desktop: Option<&'a [Pixel]>,
    windows: &'a [WindowRow<'a>],
    cursor: Option<(&'a PlacedCursor, u32)>,
    /// The front-most window whose opaque runs may be copied, or `None` where
    /// no run can be: a fade is encoding, or the cursor draws on this row.
    front: Option<&'a WindowRow<'a>>,
}

/// Where a composed row is written: the back buffer's span for these columns
/// and, when this segment finishes the rectangle, the scan-out bytes for the
/// same ones.
struct RowTargets<'a> {
    back: &'a mut [Pixel],
    frame: Option<&'a mut [u8]>,
}

/// What composing one row cost.
struct RowWork {
    /// Layer contributions blended.
    blended: u64,
    /// Pixels resolved by copying an opaque run.
    copied: u64,
}

/// Compose one screen row, writing the back buffer and — when `targets`
/// carries frame bytes — the scan-out frame, from screen column `first_col`.
///
/// Opaque runs of the front-most window are copied whole and everything below
/// them is skipped; every other column takes [`compose_pixel`]. A run whose
/// destination slice cannot be resolved encodes nothing and falls through to
/// the general path, so the frame can never be left holding stale bytes.
fn compose_row(
    layers: &RowLayers<'_>,
    targets: RowTargets<'_>,
    first_col: i32,
    order: ChannelOrder,
    reveal: u8,
) -> RowWork {
    let RowTargets { back, mut frame } = targets;
    let cols = back.len();
    let limit = first_col.saturating_add_unsigned(u32::try_from(cols).unwrap_or(u32::MAX));
    let mut work = RowWork {
        blended: 0,
        copied: 0,
    };
    let mut col = 0usize;
    let mut x = first_col;
    while col < cols {
        if let Some(run) = layers
            .front
            .and_then(|row| row.opaque_run(x, limit))
            .and_then(|run| run.get(..run.len().min(cols - col)))
        {
            // The frame slice is exactly four bytes per pixel of the run, so
            // the encoder takes all of it; reading its count back rather than
            // assuming it keeps the copy and the encode describing the same
            // pixels.
            let len = match frame.as_deref_mut() {
                Some(bytes) => bytes
                    .get_mut(col * 4..(col + run.len()) * 4)
                    .map_or(0, |bytes| order.encode_run(run, bytes)),
                None => run.len(),
            };
            if let (Some(dst), Some(src)) = (back.get_mut(col..col + len), run.get(..len)) {
                dst.copy_from_slice(src);
            }
            if len > 0 {
                work.copied = work
                    .copied
                    .saturating_add(u64::try_from(len).unwrap_or(u64::MAX));
                col += len;
                x = x.saturating_add_unsigned(u32::try_from(len).unwrap_or(u32::MAX));
                continue;
            }
        }
        let Some(dst) = back.get_mut(col) else {
            break;
        };
        let (acc, blends) = compose_pixel(
            layers.under.unwrap_or(*dst),
            x,
            layers.desktop,
            layers.windows,
            layers.cursor,
        );
        work.blended = work.blended.saturating_add(u64::from(blends));
        *dst = acc;
        if let Some(bytes) = frame
            .as_deref_mut()
            .and_then(|bytes| bytes.get_mut(col * 4..col * 4 + 4))
        {
            bytes.copy_from_slice(&order.encode(revealed(acc, reveal)));
        }
        col += 1;
        x = x.saturating_add(1);
    }
    work
}

/// The composited pixel at screen column `x` of an already-resolved
/// scanline: the desktop layer, then each window row back-to-front, then
/// the cursor, each blended *over* `under`. Returns that pixel and how many
/// layers were blended to reach it, which is the per-pixel cost the frame
/// counters report.
///
/// `cursor` carries the overlay and the image-local row this scanline is,
/// already resolved by the caller, or `None` where the cursor draws nothing
/// on this row.
fn compose_pixel(
    under: Pixel,
    x: i32,
    desktop_row: Option<&[Pixel]>,
    rows: &[WindowRow<'_>],
    cursor: Option<(&PlacedCursor, u32)>,
) -> (Pixel, u32) {
    let mut acc = under;
    let mut blends = 0;
    if let Some(src) = desktop_pixel(desktop_row, x) {
        acc = src.over(acc);
        blends += 1;
    }
    for row in rows {
        if let Some(src) = row.sample(x) {
            acc = src.over(acc);
            blends += 1;
        }
    }
    if let Some((cursor, ly)) = cursor {
        if let Some(src) = cursor.sample_row(x, ly) {
            acc = src.over(acc);
            blends += 1;
        }
    }
    (acc, blends)
}

/// `pixel` as the screen reveal presents it: scaled towards black by
/// `strength`, with alpha untouched so an opaque screen stays opaque and the
/// premultiplied invariant (every channel `<= a`) still holds.
///
/// A fully-revealed screen returns the pixel itself, so a desktop that is not
/// fading pays a compare and encodes exactly the bytes it always did.
fn revealed(pixel: Pixel, strength: u8) -> Pixel {
    if strength == u8::MAX {
        return pixel;
    }
    let s = u32::from(strength);
    Pixel {
        r: div255(u32::from(pixel.r) * s),
        g: div255(u32::from(pixel.g) * s),
        b: div255(u32::from(pixel.b) * s),
        a: pixel.a,
    }
}

/// The desktop layer's pixel at screen column `x` on an already-resolved
/// scanline, or `None` where the layer does not reach (no layer at all, a row
/// past its height, or a column past its width) — there the root fill shows
/// through, exactly as it did before a layer was installed.
fn desktop_pixel(row: Option<&[Pixel]>, x: i32) -> Option<Pixel> {
    let column = usize::try_from(x).ok()?;
    row?.get(column).copied()
}

/// Whether `window` can contribute a pixel inside `area`: it is visible
/// and its outer bounds overlap. Every other window's sample there is
/// unconditionally `None`, so skipping it is exact.
fn covers(window: &Window, area: Rect) -> bool {
    window.is_visible() && !window.bounds().intersection(&area).is_empty()
}

/// Claim `rect` in a compose plan, merging it with every rectangle it
/// overlaps, and return what was claimed.
///
/// The plan's rectangles must stay mutually disjoint — a pixel composited by
/// two of them would be blended twice — and each must stay whole, because a
/// blurred window is only seamless when its entire rectangle is recomposed at
/// once. Merging overlaps into their union is the one shape that keeps both.
fn claim(plan: &mut Vec<Rect>, mut rect: Rect) -> Rect {
    let mut index = 0;
    while let Some(claimed) = plan.get(index) {
        if claimed.intersection(&rect).is_empty() {
            index += 1;
            continue;
        }
        rect = rect.union(claimed);
        // Order carries no meaning: each rectangle is composited on its own.
        // Restart, because the grown rectangle may now reach one that was
        // already passed over.
        plan.swap_remove(index);
        index = 0;
    }
    plan.push(rect);
    rect
}

/// A cumulative cache counter's growth across one composite pass, as this
/// frame's own share. A count wider than `u32` saturates rather than wrapping,
/// so a diagnostic never reads as a suspiciously small frame.
fn frame_share(before: u64, after: u64) -> u32 {
    u32::try_from(after.saturating_sub(before)).unwrap_or(u32::MAX)
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
