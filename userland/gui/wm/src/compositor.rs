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
use core::ops::Range;

use tairix_abi::driver::display::{
    AccelCaps, AccelLayer, AcceleratedDisplay, DamageRect, Display, DisplayMode, MAX_DAMAGE_RECTS,
};
use tairix_abi::sysinfo::DesktopFrameTotals;
use tairix_abi::DriverError;
use tairix_display::{damage_list, scanout_len, ChannelOrder};

use tairix_controls::{damage, FurniturePart, ResizeEdge, TitleBarEvent, WindowFrame};
use tairix_cursor::{CursorImage, PlacedCursor};
use tairix_icon::IconKind;
use tairix_input::{InputEvent, Key};
use tairix_parallel::JobRunner;
use tairix_raster::BlurScratch;
use tairix_reclaim::{CacheAccounting, PressureBand, PressureGauge, ReclaimCache, Served};
use tairix_theme::{CursorKind, Theme};

use crate::chrome::{ChromeEpoch, WindowChrome};
use crate::color::{Color, DitherRow, Pixel};
use crate::corner::Corners;
use crate::frost::{inset, FrostEpoch, FrostPlan, FrostedBackdrop};
use crate::geometry::{Point, Rect, Region, Scale};
use crate::stats::{area_px, FrameCounters, FrameStats};
use crate::surface::{blend_run, Surface};
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

/// The fewest pixels a band of a composited rectangle carries before it is worth
/// handing to another core.
///
/// A dispatch costs a wake syscall and the workers' park syscalls; a band of this
/// many pixels costs hundreds of microseconds of blending, which dwarfs them
/// several times over even on a slow core. Below it the rectangle is composed on
/// the calling thread with no atomics at all, which is why a pointer-motion
/// repaint of a few rows pays exactly what it did before a pool existed.
const MIN_PARALLEL_BAND_PX: usize = 16_384;

/// Which end of the z-order a restack moves a window's family to.
#[derive(Copy, Clone, Eq, PartialEq)]
enum StackEnd {
    /// The front, above every other window: a raise.
    Front,
    /// The back, below every other window: a put-to-back.
    Back,
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
    /// Windows whose content was released while nobody could see them, for
    /// the embedder to tell their clients they may let go of their own copies
    /// ([`take_released_notices`](Self::take_released_notices)).
    released_notices: Vec<WindowId>,
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
    /// may copy the retained one, and how much of it.
    ///
    /// The plan's fixed-point sweep and the composite that follows it both
    /// need the answer, and asking twice would both cost a second lookup and
    /// risk the two reading differently. Reset by each
    /// [`compose_plan`](Self::compose_plan), so an index can never carry a
    /// decision taken about a window that has since moved or gone.
    frost_decision: Vec<Option<FrostPlan>>,
    /// Where each composite's per-pixel work is run
    /// ([`set_job_runner`](Self::set_job_runner)). The calling thread alone until
    /// an embedder hands over a worker pool.
    runner: &'static dyn JobRunner,
    damage: Region,
    /// Where the composed pixels are current but the scan-out bytes encoded
    /// from them are stale — the screen reveal's own channel
    /// ([`mark_scanout`](Self::mark_scanout)). Drained by the same
    /// [`recompose_damage`](Self::recompose_damage) that drains `damage`, and
    /// re-encoded rather than recomposed.
    scanout: Region,
    /// Where a segment of a damaged rectangle still has to compose the layers
    /// below a frosted window: the rectangle less whatever the frost is about
    /// to write over. Held here, and cleared per use, so a frame's segments
    /// reuse its buffers instead of allocating a region each.
    plane: Region,
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
            released_notices: Vec::new(),
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
            runner: &tairix_parallel::SERIAL,
            damage: Region::new(),
            scanout: Region::new(),
            plane: Region::new(),
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
        // A rectangle of the old screen cannot name one of the new, and the
        // whole-screen composite below re-encodes every pixel anyway.
        self.scanout.clear();
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
    /// cursor overlay, composed after every window.
    ///
    /// It does not alter the back buffer beneath a frosted window, so a
    /// pointer sample keeps every retained frost. It is still a *composite*:
    /// the cursor is blended into the back buffer like any other layer.
    fn mark_overlay(&mut self, rect: Rect) {
        let above_every_window = self.windows.len();
        self.mark_from(rect, above_every_window);
    }

    /// Mark `rect` for re-encoding: the composed pixels there are current, and
    /// only the scan-out bytes derived from them are stale.
    ///
    /// The screen reveal is the whole of this channel. It is applied as a
    /// composed pixel is encoded for scan-out and the back buffer keeps the
    /// true composed colour, so a fade step changes what every pixel *presents*
    /// without changing any pixel — recompositing the scene to arrive at the
    /// back buffer it already holds is work the fade cannot use, and it grows
    /// with the scene the fade did not touch.
    ///
    /// Nothing composes, so no frost is consulted or dropped, and no layer is
    /// read: [`recompose_damage`](Self::recompose_damage) re-encodes these
    /// rectangles from the back buffer as it stands. What that is worth is
    /// measured in `docs/src/desktop/wm.md`.
    fn mark_scanout(&mut self, rect: Rect) {
        self.scanout.add(rect);
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
                // time for an answer this call has just determined. Rewriting
                // it is also what keeps the plan and the cache in step: a frame
                // may never read a decision to copy a frost this dropped.
                if let Some(decision) = frost_decision.get_mut(index) {
                    *decision = Some(FrostPlan::Blur);
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

    /// The mode everything drawn on this output is valid for: its scale and
    /// the generation of the theme it was painted under.
    ///
    /// Retained furniture keys on it, and so does anything else the desktop
    /// placed against the mode rather than re-derived per frame — a menu
    /// chain dismisses when it moves on, because re-placing a plate the user
    /// has dragged is not defined.
    #[must_use]
    pub fn chrome_epoch(&self) -> ChromeEpoch {
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

    /// What the frame in flight will do about the frost of the window at
    /// z-index `index`: copy the retained one whole, copy its still-valid core
    /// and blur the border around it, or blur the whole rectangle.
    ///
    /// Asked of the cache once per frame and remembered
    /// ([`frost_decision`](Self::frost_decision)), because the plan and the
    /// composite that follows it both need the answer and two lookups could
    /// return two different ones — which would leave a window the plan did
    /// not widen for being blurred over a rectangle whose lower layers the
    /// frame never composed, and it would seam.
    ///
    /// This is the one counted lookup: a hit is a frost the frame goes on to
    /// copy from, whole or in part, a miss one it has to blur outright, and the
    /// recency the lookup touches is what keeps a frost every frame reuses
    /// ahead of one nothing has looked at when the band forces an eviction.
    fn frost_plan(&mut self, index: usize) -> FrostPlan {
        if let Some(Some(decided)) = self.frost_decision.get(index).copied() {
            return decided;
        }
        let decided = self.ask_frost(index);
        if let Some(slot) = self.frost_decision.get_mut(index) {
            *slot = Some(decided);
        }
        decided
    }

    /// Ask the cache how much of the frost the window at z-index `index` needs
    /// it still holds, counting the lookup.
    ///
    /// A retained entry nothing can be kept from is released *before* the
    /// lookup rather than rejected after it, so the frame counts one honest
    /// miss and the superseded pixels stop being charged at once instead of
    /// waiting to be evicted. Eviction under pressure is likewise the lookup's
    /// own answer: an entry the band takes as this call enforces it simply
    /// reads as absent.
    fn ask_frost(&mut self, index: usize) -> FrostPlan {
        #[cfg(test)]
        if !self.frost_reuse {
            return FrostPlan::Blur;
        }
        let screen = self.screen_rect();
        let Some(window) = self.windows.get(index) else {
            return FrostPlan::Blur;
        };
        let (id, shape) = (window.id(), window.shape());
        let bounds = window.bounds();
        let radius_px = self.blur_radius_px(window);
        let epoch = self.frost_epoch();
        let kept = self
            .frost
            .peek(&epoch, &id)
            .map(|retained| retained.reuse(bounds, screen, radius_px, shape));
        if kept == Some(FrostPlan::Blur) {
            self.frost.invalidate(&id);
        }
        match self.frost.get_or_build(&epoch, id, || None) {
            Some(_) => kept.unwrap_or(FrostPlan::Blur),
            None => FrostPlan::Blur,
        }
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
        self.frost_retained(id)
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
        self.released_notices.clear();
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
    /// A released **visible** window is queued for a redraw request
    /// ([`pending_redraws`](Self::pending_redraws)) and its outer bounds are
    /// marked dirty, so the desktop shows through immediately rather than
    /// keeping a stale image the compositor no longer has.
    ///
    /// A released **hidden** window is not asked at all. Asking one would
    /// spend the memory the release just recovered — the client presents, and
    /// the buffer is established again — for pixels nobody can see; the
    /// release would free nothing and cost a repaint per window, under
    /// pressure. It is asked by [`set_visible`](Self::set_visible) when it is
    /// next shown, and meanwhile it is queued in
    /// [`take_released_notices`](Self::take_released_notices) so the embedder
    /// can tell its client to let go of its own copies.
    ///
    /// # Two triggers, one decision
    ///
    /// The ladder reads two things — the band and each window's visibility —
    /// so it runs when *either* moves: here on the band's wake, and in
    /// [`set_visible`](Self::set_visible) on the hide. Running only on the
    /// band's wake would miss the ordinary case, since that wake is
    /// edge-triggered and a user minimising a window on an already-tight
    /// machine produces no edge.
    pub fn release_content_under_pressure(&mut self, focused: Option<WindowId>) -> usize {
        let band = self.pressure.sample();
        if band == PressureBand::Normal {
            return 0;
        }
        let mut released = 0usize;
        for index in 0..self.windows.len() {
            released =
                released.saturating_add(self.take_content_under_pressure(index, band, focused));
        }
        released
    }

    /// Apply the ladder to the single window at `index` and record what the
    /// release owes: the bytes given back, or zero if the band, the window's
    /// visibility, or its lack of a client spares it.
    ///
    /// The one place the ladder's decision and its consequences live, so the
    /// band's own wake and the visibility edge ([`set_visible`](Self::set_visible))
    /// cannot come to differ about what a release means.
    fn take_content_under_pressure(
        &mut self,
        index: usize,
        band: PressureBand,
        focused: Option<WindowId>,
    ) -> usize {
        if band == PressureBand::Normal {
            return 0;
        }
        let Some(window) = self.windows.get_mut(index) else {
            return 0;
        };
        let id = window.id();
        if !window.is_app_presented() {
            return 0;
        }
        let visible = window.is_visible();
        if visible && !(band >= PressureBand::Critical && Some(id) != focused) {
            return 0;
        }
        let bytes = window.release_content();
        if bytes == 0 {
            return 0;
        }
        // A hidden window draws nothing either way, so only a visible one's
        // pixels actually changed on screen.
        let exposed = visible.then(|| window.bounds());
        match exposed {
            // Visible: it must not be left blank, so its pixels are asked
            // for now and the rectangle it vacated is repainted.
            Some(bounds) => {
                self.request_redraw(id);
                self.mark_layer(id, bounds);
            }
            // Hidden: asking now would spend the very memory the release
            // recovered, and nobody would see the result. `set_visible` asks
            // when the window is next shown, so what this owes its client is
            // only the news that it may let go of its own copies too.
            None => {
                if !self.released_notices.contains(&id) {
                    self.released_notices.push(id);
                }
            }
        }
        bytes
    }

    /// Take every window whose content was released while hidden, leaving the
    /// queue empty.
    ///
    /// The embedder tells each one's client that the session let go of its
    /// frames ([`tairix_abi::window_ipc::WindowEvent::ContentReleased`]) and
    /// unmaps its side, which is what turns a release into pages the machine
    /// gets back: the compositor's own copy is one of three, and the other two
    /// are the client's.
    pub fn take_released_notices(&mut self) -> Vec<WindowId> {
        core::mem::take(&mut self.released_notices)
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
    /// and theme, marking exactly the rectangles it reported repainting, and
    /// return what `change` produced (`None` for an unknown id).
    ///
    /// Every mutation that can alter how a frame is drawn runs through here,
    /// so what it reported decides both halves of the repaint rather than each
    /// caller remembering them: the reported rectangles are marked over this
    /// window's layer, and the retained furniture is released only when
    /// something was reported at all. A mutation that changed no drawn pixel —
    /// a title set to the label already there, a pointer sample crossing a
    /// title bar without entering a control — keeps that window's rendered
    /// furniture and marks nothing.
    ///
    /// The release is one key, never the whole cache: a title edit or a focus
    /// flip leaves every *other* window's furniture perfectly valid.
    fn mutate_frame<R>(
        &mut self,
        id: WindowId,
        change: impl FnOnce(&mut Window, Scale, &Theme, &mut Region) -> R,
    ) -> Option<R> {
        let scale = self.scale;
        let mut damage = damage::sink();
        let out = {
            let Self { theme, windows, .. } = self;
            let window = windows.iter_mut().find(|w| w.id() == id)?;
            change(window, scale, theme, &mut damage)
        };
        if damage.is_empty() {
            return Some(out);
        }
        self.chrome.invalidate(&id);
        for rect in damage.rects() {
            self.mark_layer(id, *rect);
        }
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
    /// A change re-encodes the whole screen because every pixel's presented
    /// value changed — and *only* re-encodes it, because no composed pixel
    /// moved: the next [`composite`](Self::composite) writes the scan-out bytes
    /// afresh from the back buffer it already holds, laying no layer. Setting
    /// the strength already in force damages nothing.
    pub fn set_reveal(&mut self, strength: u8) -> bool {
        if strength == self.reveal {
            return false;
        }
        self.reveal = strength;
        self.mark_scanout(self.screen_rect());
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

    /// Repaint the parts of the desktop layer covered by `area` through
    /// `paint`, keeping the screen-sized buffer it is already drawn into, and
    /// mark exactly those parts dirty.
    ///
    /// The desktop is repainted whenever its owner's model changes — an icon
    /// takes the hover, a selection moves, the folder re-lists — which is
    /// often, and the layer is a whole screen of pixels. Two things follow,
    /// and this method is where both are paid:
    ///
    /// * Handing the existing buffer back to the painter means a repaint
    ///   costs a paint, not a paint plus a multi-megabyte allocation the heap
    ///   may refuse.
    /// * Painting and marking only `area` means an icon taking the hover
    ///   costs that icon. Marking the whole layer would not merely repaint a
    ///   screen: the desktop is the bottom layer, so every window above it
    ///   would recomposite and every frosted backdrop over the marked pixels
    ///   would be thrown away and blurred again — a screenful of work to
    ///   redraw one label.
    ///
    /// `paint` receives the surface and the rectangles to paint, already
    /// clipped to the layer and disjoint; it must write inside them and
    /// nowhere else, since nothing outside them is marked. It sees the
    /// surface exactly as the previous frame left it, so it lays down its own
    /// background over each rectangle rather than relying on a clear this
    /// method cannot know is redundant.
    ///
    /// A layer that is absent, or sized for a screen this output no longer
    /// has, is allocated fresh at the current screen size and painted whole
    /// however little `area` asked for — a buffer with no pixels worth keeping
    /// has nothing for a partial paint to preserve.
    ///
    /// Returns `false` — having changed and damaged nothing — when no such
    /// surface could be allocated, so a heap that will not give back a screen
    /// of pixels leaves the desktop exactly as it was rather than blanking it.
    pub fn repaint_desktop(
        &mut self,
        area: &Region,
        paint: impl FnOnce(&mut Surface, &[Rect]),
    ) -> bool {
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
        let Some(layer) = self.desktop_bounds() else {
            return false;
        };
        let painted = if fits {
            let mut region = area.clone();
            region.clip(layer);
            region
        } else {
            Region::from(layer)
        };
        if painted.is_empty() {
            return true;
        }
        let Some(surface) = self.desktop.as_mut() else {
            return false;
        };
        paint(surface, painted.rects());
        for rect in painted.rects() {
            self.mark(*rect);
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

    /// Add `surface` as a window `parent` owns — a *transient*: the menu or
    /// sheet that window opened — at `origin`, stacked directly above its
    /// owner and any transient already there. Returns its identifier, or
    /// `None` for an unknown `parent`.
    ///
    /// A transient is not a window in its own right. It is composed above its
    /// owner and restacked with it ([`raise`](Self::raise),
    /// [`lower`](Self::lower)), so nothing can be raised between the two and
    /// no caller has to re-assert the arrangement afterwards. Refusing an
    /// unknown owner is the fail-closed half of that: a transient whose owner
    /// does not exist would have no place in the stack to hold.
    ///
    /// Only the new window's own bounds are marked dirty when its owner
    /// already holds the front — which is the ordinary case, a menu opening on
    /// the focused window — so opening one costs its own rectangle rather than
    /// its owner's whole frame.
    pub fn add_transient_window(
        &mut self,
        parent: WindowId,
        origin: Point,
        surface: Surface,
    ) -> Option<WindowId> {
        let above = self.family_top(parent)?;
        let id = WindowId(self.next_id);
        self.next_id += 1;
        let mut window = Window::new(id, origin, surface);
        window.set_parent(Some(parent));
        let bounds = window.bounds();
        self.windows.insert(above, window);
        self.mark_layer(id, bounds);
        self.restack_family(parent, StackEnd::Front);
        Some(id)
    }

    /// The stack index directly above `parent` and every transient it already
    /// owns — where the next one belongs — or `None` when `parent` is not a
    /// window here.
    fn family_top(&self, parent: WindowId) -> Option<usize> {
        Some(self.family_front_index(parent)?.saturating_add(1))
    }

    /// The stack index of the front-most member of `parent`'s family: the
    /// top-most transient it owns, or `parent` itself when it owns none.
    fn family_front_index(&self, parent: WindowId) -> Option<usize> {
        let mut front = self.index_of(parent)?;
        for (index, window) in self.windows.iter().enumerate() {
            if window.parent() == Some(parent) {
                front = front.max(index);
            }
        }
        Some(front)
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
    ///
    /// Hiding is the other input to the content-release ladder
    /// ([`release_content_under_pressure`](Self::release_content_under_pressure)),
    /// so the decision is remade here for this window. The band's own wake is
    /// edge-triggered — it fires when the band *moves* — and a user minimising
    /// a window on a machine whose pressure has already settled produces no
    /// such edge, so without this the largest easily-recovered block the
    /// desktop holds would be released only if the band happened to move again.
    ///
    /// Showing is the reverse: a window shown again after its content was
    /// released has nothing to draw until its app presents, so the redraw is
    /// asked for now rather than leaving it blank until something else
    /// happens to it.
    pub fn set_visible(&mut self, id: WindowId, visible: bool) -> bool {
        let known = self.mutate(id, |w| w.set_visible(visible));
        if !known {
            return false;
        }
        if visible {
            // A notice not yet drained is one the embedder has not acted on,
            // so both sides still hold the region: withdrawing it spares a
            // quick minimise-then-restore an unmap and a re-attach that would
            // change nothing.
            self.released_notices.retain(|&queued| queued != id);
            if self
                .window(id)
                .is_some_and(|w| w.is_app_presented() && !w.has_content())
            {
                self.request_redraw(id);
            }
        } else if let Some(index) = self.windows.iter().position(|w| w.id() == id) {
            // A window that has just been hidden is takeable whatever holds
            // focus, so the ladder's focus exception cannot apply to it.
            let _ = self.take_content_under_pressure(index, self.pressure.sample(), None);
        }
        true
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

    /// Repaint part of a window the **embedder** paints itself — a menu
    /// plate, a session panel — keeping the pixels already there and marking
    /// only what `paint` was given to write.
    ///
    /// The window-content mirror of
    /// [`repaint_desktop`](Self::repaint_desktop), and it exists for the same
    /// two reasons: handing the retained buffer back means a repaint costs a
    /// paint rather than a paint plus an allocation the heap may refuse, and
    /// marking only `area` means a menu row taking the highlight costs that
    /// row instead of the plate — which on a frosted surface is the
    /// difference between re-blending two rows and re-blending, then
    /// re-blurring behind, all of it.
    ///
    /// The embedder declares the damage here where a client
    /// ([`present_window_content`](Self::present_window_content)) has it
    /// discovered by the conversion: an embedder painting its own model knows
    /// what changed before it paints, and a client does not.
    ///
    /// `paint` receives the content surface and the rectangles to paint,
    /// already clipped to it and disjoint, in the surface's own local
    /// coordinates; it must write inside them and nowhere else, since nothing
    /// outside them is marked. It sees the surface exactly as the last paint
    /// left it, so it lays its own background over each rectangle rather than
    /// relying on a clear this method cannot know is redundant.
    ///
    /// A window whose content is absent or is not `size` is repainted whole
    /// into a fresh buffer of that size, however little `area` asked for: a
    /// buffer with no pixels worth keeping has nothing for a partial paint to
    /// preserve, and its size is what the window's geometry then follows.
    ///
    /// Returns `false` — having changed and damaged nothing — for an unknown
    /// `id` or a buffer the heap will not give back, so an exhausted machine
    /// leaves the window exactly as it was rather than blanking it.
    pub fn repaint_window(
        &mut self,
        id: WindowId,
        size: (u32, u32),
        area: &Region,
        paint: impl FnOnce(&mut Surface, &[Rect]),
    ) -> bool {
        let (width, height) = size;
        let Some(index) = self.index_of(id) else {
            return false;
        };
        let local = Rect::new(0, 0, width, height);
        let fits = self.windows.get(index).is_some_and(|window| {
            window
                .content()
                .is_some_and(|held| held.width() == width && held.height() == height)
        });
        if !fits {
            let Some(mut fresh) = Surface::new(width, height) else {
                return false;
            };
            paint(&mut fresh, &[local]);
            return self.set_surface(id, fresh);
        }
        let mut painted = area.clone();
        painted.clip(local);
        if painted.is_empty() {
            return true;
        }
        let Some(window) = self.windows.get_mut(index) else {
            return false;
        };
        let Some(content) = window.content_mut() else {
            return false;
        };
        paint(content, painted.rects());
        let client = window.client_rect();
        painted.translate(client.left(), client.top());
        painted.clip(client);
        for rect in painted.rects() {
            self.mark_layer(id, *rect);
        }
        true
    }

    /// Raise a window to the top of the z-order; its bounds are marked
    /// dirty. Raising a window whose family already holds the front marks no
    /// damage and still returns `true` (only an unknown `id` returns
    /// `false`).
    ///
    /// A window and its transients ([`add_transient_window`]) rise together
    /// and keep their order, so raising either the owner or its menu brings
    /// the pair, and neither can be left stranded under a window that was
    /// raised between them.
    ///
    /// [`add_transient_window`]: Self::add_transient_window
    pub fn raise(&mut self, id: WindowId) -> bool {
        if self.index_of(id).is_none() {
            return false;
        }
        self.restack_family(self.family_root(id), StackEnd::Front);
        true
    }

    /// Send a window to the bottom of the z-order (put-to-back), keeping it
    /// visible; its bounds are marked dirty so whatever it was covering
    /// recomposites. Returns `false` for an unknown id.
    ///
    /// The family goes as a unit, exactly as [`raise`](Self::raise) brings
    /// it: a window sent to the back takes the menu it owns with it, rather
    /// than leaving it floating over windows it does not belong to.
    pub fn lower(&mut self, id: WindowId) -> bool {
        if self.index_of(id).is_none() {
            return false;
        }
        self.restack_family(self.family_root(id), StackEnd::Back);
        true
    }

    /// The window every restack moves as one with `id`: the window it is a
    /// transient of, or `id` itself when it owns its place in the stack.
    fn family_root(&self, id: WindowId) -> WindowId {
        self.window(id).and_then(Window::parent).unwrap_or(id)
    }

    /// The front-most window of `id`'s family — the top-most transient its
    /// owner has open, or `id`'s own family root when it has none. `None` for
    /// a window this compositor does not know.
    ///
    /// A family rises as a unit ([`raise`](Self::raise)), so after a raise this
    /// is the window the pointer and the keyboard actually meet. Anything
    /// choosing a window to *focus* must ask for it rather than focus the
    /// owner: a sheet or menu its owner opened sits above that owner, so
    /// focusing the owner would put the keyboard behind a surface the user can
    /// see and the owner cannot dismiss.
    #[must_use]
    pub fn family_front(&self, id: WindowId) -> Option<WindowId> {
        let front = self.family_front_index(self.family_root(id))?;
        self.windows.get(front).map(Window::id)
    }

    /// How many windows `id` owns as transients.
    ///
    /// A transient's own transients are not counted, because none exist: a
    /// popup is opened by a window, never by another popup, and a chain would
    /// need a stacking rule of its own rather than this one applied twice.
    fn transients(&self, id: WindowId) -> usize {
        self.windows
            .iter()
            .filter(|window| window.parent() == Some(id))
            .count()
    }

    /// Move `root`'s family to one end of the z-order, owner first, and mark
    /// what the move changed. Reports whether anything actually moved.
    ///
    /// A family already sitting at that end is left completely alone — no
    /// restack, no damage, no dropped backdrop, and not so much as an
    /// allocation. That is the common case, not a rare one: an owner and its
    /// menu spend their whole life at the front, and re-deriving an
    /// arrangement that already holds must not cost a window's worth of blur
    /// to be told nothing changed.
    ///
    /// **A move marks where the family and the windows it crossed overlap,
    /// and nothing else.** Reordering two windows that do not overlap changes
    /// no pixel: nothing is drawn differently, and no frost sees a different
    /// backdrop. Marking the whole family instead would drop the owner's own
    /// retained frost every time — a menu opening on a window with anything at
    /// all above it would re-blur the entire window, which is the expensive
    /// case, not the rare one, because the taskbar sits above app windows.
    fn restack_family(&mut self, root: WindowId, end: StackEnd) -> bool {
        let Some(root_at) = self.index_of(root) else {
            return false;
        };
        let owned = self.transients(root);
        let first = match end {
            StackEnd::Front => self.windows.len().saturating_sub(owned.saturating_add(1)),
            StackEnd::Back => 0,
        };
        if root_at == first && self.family_is_placed(root, first, owned) {
            return false;
        }
        // The lowest window the move disturbs: everything below it sees the
        // same stack it always did, so its retained backdrop still holds.
        let low = root_at.min(first);
        let mut order = Vec::with_capacity(owned.saturating_add(1));
        order.push(root);
        order.extend(
            self.windows
                .iter()
                .filter(|window| window.parent() == Some(root))
                .map(Window::id),
        );
        let crossed = self.crossed_bounds(&order, end);
        let mut taken = Vec::with_capacity(order.len());
        // Top-down, so each removal leaves the indices below it untouched.
        for id in order.iter().rev() {
            if let Some(index) = self.index_of(*id) {
                taken.push(self.windows.remove(index));
            }
        }
        let mut moved = Vec::with_capacity(taken.len());
        for (offset, window) in taken.into_iter().rev().enumerate() {
            moved.push(window.bounds());
            let at = first.saturating_add(offset).min(self.windows.len());
            self.windows.insert(at, window);
        }
        // Restacked first: a window's own frosted backdrop is a function of
        // its place in the stack, and marking is what drops it.
        for bounds in moved {
            for over in &crossed {
                self.mark_from(bounds.intersection(over), low);
            }
        }
        true
    }

    /// The bounds of every window `family` swaps places with when it moves to
    /// `end` — the only windows the move can put on the other side of it.
    ///
    /// The family lands contiguously at one end, so moving to the front puts it
    /// above everything that was above its *lowest* member, and moving to the
    /// back puts it below everything that was below its *highest*. Windows
    /// beyond that keep their relative order with the family and so see exactly
    /// the stack they always did. An invisible window contributes no pixel to
    /// any composite, so crossing one changes nothing.
    fn crossed_bounds(&self, family: &[WindowId], end: StackEnd) -> Vec<Rect> {
        let indices = family.iter().filter_map(|id| self.index_of(*id));
        let (lowest, highest) = indices.fold((usize::MAX, 0), |(low, high), at| {
            (low.min(at), high.max(at))
        });
        self.windows
            .iter()
            .enumerate()
            .filter(|(at, window)| {
                window.is_visible()
                    && !family.contains(&window.id())
                    && match end {
                        StackEnd::Front => *at > lowest,
                        StackEnd::Back => *at < highest,
                    }
            })
            .map(|(_, window)| window.bounds())
            .collect()
    }

    /// Whether `root`'s family already occupies the stack from `first`
    /// upwards: the owner there, and each of the `owned` windows above it one
    /// of its transients.
    ///
    /// Their order among themselves is not examined, because it cannot be
    /// wrong: a restack preserves it, so any arrangement of a family's own
    /// transients above their owner is the one a restack would produce.
    fn family_is_placed(&self, root: WindowId, first: usize, owned: usize) -> bool {
        self.windows.get(first).map(Window::id) == Some(root)
            && self
                .windows
                .get(first.saturating_add(1)..first.saturating_add(1).saturating_add(owned))
                .is_some_and(|above| above.iter().all(|w| w.parent() == Some(root)))
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
        self.mutate_frame(id, |window, scale, theme, damage| {
            let before = window.bounds();
            let result = window.toggle_size(work_area, scale, theme);
            if result.is_some() {
                damage.add(before);
                damage.add(window.bounds());
            }
            result
        })
        .flatten()
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
        // A transient outliving its owner stands on its own: the window engine
        // tears a window's popups down with it, and until each arrives the
        // link must not name a window that has gone.
        for other in &mut self.windows {
            if other.parent() == Some(id) {
                other.set_parent(None);
            }
        }
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
        self.mutate_frame(id, |window, scale, theme, damage| {
            damage.add(window.bounds());
            window.set_frame(Some(frame), scale, theme);
            damage.add(window.bounds());
        })
        .is_some()
    }

    /// Remove the decoration frame from the window named by `id`, so the window
    /// manager stops reserving and drawing furniture and the window's outer
    /// bounds collapse back to the bare content surface. Returns `false` for an
    /// unknown id. The union of the old and new bounds is marked dirty.
    pub fn clear_window_frame(&mut self, id: WindowId) -> bool {
        self.mutate_frame(id, |window, scale, theme, damage| {
            damage.add(window.bounds());
            window.set_frame(None, scale, theme);
            damage.add(window.bounds());
        })
        .is_some()
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
        self.mutate_frame(id, |window, _, _, damage| {
            if !window.set_frame_active(active) {
                return false;
            }
            for band in window.furniture_bands() {
                damage.add(band);
            }
            true
        })
        .unwrap_or(false)
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
        self.mutate_frame(id, |window, _, _, damage| {
            if !window.set_frame_title(title) {
                return false;
            }
            damage.add(window.title_band());
            true
        })
        .unwrap_or(false)
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
        self.mutate_frame(id, |window, _, _, damage| {
            if !window.set_frame_identity(identity, artwork) {
                return false;
            }
            damage.add(window.title_band());
            true
        })
        .unwrap_or(false)
    }

    /// The pixel side the identity icon of the decorated window named by `id`
    /// paints at, or `None` for an unknown or undecorated window.
    ///
    /// This is the size an owner rasterises artwork at before handing it to
    /// [`set_window_identity`](Self::set_window_identity): the frame states it
    /// from the active scale and theme, so a caller never has to reconstruct
    /// that geometry. Every decorated window shares the side — the title band's
    /// height is the theme's, not any one window's — so `id` decides only
    /// *whether* there is an identity slot, never how big it is.
    #[must_use]
    pub fn window_title_icon_side(&self, id: WindowId) -> Option<u32> {
        self.window(id)?.frame()?;
        Some(self.title_identity_icon_side())
    }

    /// The pixel side every decorated window's title-bar identity icon draws
    /// at, at this compositor's scale and theme.
    ///
    /// The side is the theme's, not any one window's, so an embedder can have an
    /// application's artwork decoded before the window that will wear it exists
    /// — which is the difference between a window appearing with its own icon
    /// and appearing with a glyph it replaces a decode later.
    #[must_use]
    pub fn title_identity_icon_side(&self) -> u32 {
        WindowFrame::identity_icon_side(self.scale, &self.theme)
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

    /// The window whose resize band claims the screen `point`, and which
    /// edge, when nothing else is drawn there.
    ///
    /// A resizable frame's grab band straddles its outer edge
    /// ([`GrabReach`](tairix_controls::GrabReach)), so its outer half falls
    /// outside the window: this is how that half is reached. It is
    /// deliberately consulted **only** where [`window_at`](Self::window_at)
    /// finds nothing — over the desktop, in the gap between windows — so a
    /// band can never take a press from a window whose own pixels are under
    /// the pointer. The topmost claimant wins, as it would for any other
    /// pointer question.
    #[must_use]
    pub fn resize_target(&self, point: Point) -> Option<(WindowId, ResizeEdge)> {
        self.windows.iter().rev().find_map(|window| {
            if !window.is_visible() {
                return None;
            }
            let FurniturePart::ResizeEdge(edge) =
                window
                    .frame()?
                    .hit(window.bounds(), self.scale, &self.theme, point)
            else {
                return None;
            };
            Some((window.id(), edge))
        })
    }

    /// Feed a pointer `event` to the decoration furniture of the window named
    /// by `id`, repainting only what the furniture reported changing, and
    /// return the typed [`TitleBarEvent`] it produced (a completed
    /// command-control click, or a title-bar activation/drag gesture). Returns
    /// `None` for an unknown or undecorated window.
    ///
    /// The window manager owns this furniture, so the event is never delivered
    /// to the client. The furniture reports its own repainted rectangles, so a
    /// hover that enters one command control costs that control — not the band
    /// it sits in, and never the client area — and a sample that merely crosses
    /// the drag region costs nothing at all and keeps the window's rendered
    /// chrome.
    pub fn frame_pointer(&mut self, id: WindowId, event: &InputEvent) -> Option<TitleBarEvent> {
        self.mutate_frame(id, |window, scale, theme, damage| {
            window.on_frame_pointer(event, scale, theme, damage)
        })
        .flatten()
    }

    /// Tell the decoration furniture of the window named by `id` that the
    /// pointer has left it, repainting only the control that was lit. Returns
    /// `false` for an unknown window (an undecorated one has no furniture to
    /// tell and reports `true`, having nothing to do).
    ///
    /// The counterpart of [`frame_pointer`](Self::frame_pointer) for the end of
    /// a hover that no pointer sample marks: the pointer is still inside the
    /// frame's own rectangle when a window is raised over it, so the hover has
    /// to be ended by the party that can see the stack rather than by
    /// re-testing a position that has not changed.
    pub fn frame_pointer_left(&mut self, id: WindowId) -> bool {
        self.mutate_frame(id, |window, scale, theme, damage| {
            window.on_frame_pointer_left(scale, theme, damage);
        })
        .is_some()
    }

    /// Feed a key `key` to the decoration furniture of the window named by
    /// `id` (the title bar's command controls), repainting only what the
    /// furniture reported changing, and return the typed [`TitleBarEvent`] it
    /// produced. Returns `None` for an unknown or undecorated window.
    pub fn frame_key(&mut self, id: WindowId, key: Key) -> Option<TitleBarEvent> {
        self.mutate_frame(id, |window, scale, theme, damage| {
            window.on_frame_key(key, scale, theme, damage)
        })
        .flatten()
    }

    /// Adopt the minimum client extent the application owning `id` declared
    /// when it created its window, in physical pixels; `(0, 0)` declares
    /// none. Returns `false` for an unknown id (fail closed).
    ///
    /// It bounds what a *user* may drag the window down to
    /// ([`window_min_outer_size`](Self::window_min_outer_size)), not what the
    /// application may ask for itself: an application sizing its own window
    /// is choosing that size, and a window already smaller than the minimum
    /// is left where it is rather than grown under its owner.
    pub fn set_window_min_client_size(&mut self, id: WindowId, min_w: u32, min_h: u32) -> bool {
        let Some(window) = self.windows.iter_mut().find(|w| w.id() == id) else {
            return false;
        };
        window.set_min_client_size(min_w, min_h);
        true
    }

    /// The screen rectangle of the move surface of the window named by `id`:
    /// the span of its title band between the two command clusters. `None`
    /// for an unknown or undecorated window.
    ///
    /// This is what a move-grab keeps reachable on screen, so it is the span
    /// the title bar itself lays out rather than a second idea of where a
    /// window may be dragged from.
    #[must_use]
    pub fn window_drag_surface(&self, id: WindowId) -> Option<Rect> {
        self.window(id)?.drag_surface(self.scale, &self.theme)
    }

    /// The smallest outer size an interactive resize may take the window
    /// named by `id` down to, at the active scale and theme — `None` for an
    /// unknown id.
    ///
    /// The greater of the window furniture's own floor (the title bar's
    /// commands and a drag surface between them) and the owning
    /// application's declared minimum client extent grown by the band.
    #[must_use]
    pub fn window_min_outer_size(&self, id: WindowId) -> Option<(u32, u32)> {
        let window = self.window(id)?;
        Some(window.min_outer_size(self.scale, &self.theme))
    }

    /// Resize the window named by `id` so its outer rectangle becomes
    /// `new_outer` (its content surface reallocated to the implied client
    /// size, existing pixels preserved, origin and decoration following).
    /// Returns `false` for an unknown window or when the implied client size
    /// is empty. The union of the old and new outer bounds is marked dirty.
    pub fn resize_window(&mut self, id: WindowId, new_outer: Rect) -> bool {
        self.mutate_frame(id, |window, scale, theme, damage| {
            let before = window.bounds();
            let changed = window.resize_to_outer(new_outer, scale, theme);
            if changed {
                damage.add(before);
                damage.add(window.bounds());
            }
            changed
        })
        .unwrap_or(false)
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
        self.mutate_frame(id, |window, scale, theme, damage| {
            let before = window.bounds();
            let changed = window.resize_client(client_w, client_h, scale, theme);
            if changed {
                damage.add(before);
                damage.add(window.bounds());
            }
            changed
        })
        .unwrap_or(false)
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

    /// Whether the next frame would change a pixel the display shows — an
    /// explicitly marked rectangle, a pending re-encode
    /// ([`set_reveal`](Self::set_reveal)), or a cursor
    /// move/show/hide/replacement whose damage has not yet been derived by a
    /// [`composite`](Self::composite).
    ///
    /// This answers exactly the question the next
    /// [`composite`](Self::composite) does: it is `true` if and only if that
    /// composite would change at least one presented pixel — whether by
    /// recomposing it or by encoding it afresh. A caller driving a wake loop
    /// can therefore skip the frame entirely when it is `false`, and never
    /// miss one when it is `true`. Damage marked wholly off screen is no
    /// pending work: composite clips every rectangle to the screen, so this
    /// clips them too rather than promising a frame that would change nothing.
    #[must_use]
    pub fn has_damage(&self) -> bool {
        let screen = self.screen_rect();
        let on_screen = |rect: Rect| !rect.intersection(&screen).is_empty();
        if self.damage.intersects(screen) || self.scanout.intersects(screen) {
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
    /// when nothing was dirty. Naming each of its rectangles in the present
    /// (via [`Display::present_rects`]) moves bytes proportional to what
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
        self.stats.begin_frame(self.screen_px());
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
        self.rescan_scanout(&mut composited, screen);
        composited
    }

    /// Re-encode the scan-out bytes of everything
    /// [`mark_scanout`](Self::mark_scanout) marked and the composite above did
    /// not already cover, adding those rectangles to `changed`.
    ///
    /// Everything the composite touched it also encoded, at the current reveal,
    /// so those rectangles are subtracted rather than encoded twice — a fade
    /// step that lands in the same frame as a real repaint pays for the repaint
    /// and the rest of the screen, never for the overlap twice.
    fn rescan_scanout(&mut self, changed: &mut Region, screen: Rect) {
        let mut scanout = core::mem::take(&mut self.scanout);
        scanout.clip(screen);
        for &done in changed.rects() {
            scanout.subtract(done);
        }
        for &area in scanout.rects() {
            self.stats.add_damaged(area_px(area.width, area.height));
            self.encode_rect(area);
            changed.add(area);
        }
        // Handed back emptied so the next frame reuses the buffer it grew.
        scanout.clear();
        self.scanout = scanout;
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
                if !window.is_visible() || !window.reads_backdrop() {
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
                // untouched one costs no lookup and counts as neither. A frost
                // that survives only in part is promoted like one that must be
                // blurred outright: its border is blurred, and a border blurred
                // over a strip of damage would spread a neighbourhood clipped to
                // that strip.
                if !touched || self.frost_plan(index) == FrostPlan::Whole {
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

    /// What every frame composed against the current screen has cost, the
    /// frame in progress included.
    ///
    /// The pull-side reading, for a monitor or a regression gate outside this
    /// process: cumulative work plus the worst single frame, so a hover that
    /// repaints one control is distinguishable from one that repaints the
    /// screen — an average hides exactly that. A display-mode change starts a
    /// fresh epoch, because the counts are read against the screen as their
    /// denominator.
    #[must_use]
    pub fn frame_totals(&self) -> DesktopFrameTotals {
        self.stats.totals()
    }

    /// The current screen's pixel count.
    fn screen_px(&self) -> u64 {
        area_px(self.mode.width_px, self.mode.height_px)
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
    /// No damage means nothing changed since the last present, so this does
    /// nothing at all: no scan-out copy, no driver blit, and no frame in the
    /// accounting ([`frame_totals`](Self::frame_totals)). A run loop may
    /// therefore call this on every wake, and a reader watching those totals
    /// for change still settles while the screen is idle.
    ///
    /// **A frame is presented once, naming everything it changed.** Damage
    /// that covers the screen takes the full [`Display::present`] path;
    /// anything else takes [`Display::present_rects`], whose list is the
    /// frame's disjoint dirty rectangles (`damage_list` chooses between the
    /// two, and degrades to the bounding box past [`MAX_DAMAGE_RECTS`]). The
    /// bytes moved are therefore proportional to what actually changed
    /// rather than to the box around it — a taskbar strip and a cursor near
    /// the opposite edge cost those two rectangles even though the box
    /// between them is the screen — and a scattered frame costs one dispatch
    /// rather than one per rectangle.
    ///
    /// # Errors
    ///
    /// Propagates any [`DriverError`] the display driver returns from
    /// [`Display::present`] / [`Display::present_rects`].
    pub fn present(&mut self, display: &mut dyn Display) -> Result<(), DriverError> {
        // A wake with nothing pending is not a frame, and must not open one:
        // the counters are what a reader watches for change, so counting the
        // wakes themselves would make the totals move for ever on an idle
        // desktop and never let such a reader settle.
        if !self.has_damage() {
            return Ok(());
        }
        self.stats.begin_frame(self.screen_px());
        let region = self.recompose_damage();
        if region.is_empty() {
            return Ok(());
        }
        self.stats.bump_present();
        let mut list = [DamageRect::full(&self.mode); MAX_DAMAGE_RECTS];
        match damage_list(&region, &self.mode, &mut list) {
            Some(rects) => display.present_rects(&self.frame, rects),
            None => display.present(&self.frame),
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
        self.stats.begin_frame(self.screen_px());
        // A hardware layer is composed from its own pixels alone and cannot
        // sample what is already behind it, so a backdrop blur has no layer
        // encoding at all and the whole frame goes through software.
        let layers = if self.has_backdrop_blur() || self.has_translucent_window() {
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

    /// Whether any visible window is translucent as a whole.
    ///
    /// Such a window is a large field the *engine* would have to blend over
    /// what is beneath it, and an engine blends in the scan-out's own 8 bits
    /// with a fixed rounding: the wallpaper under it would arrive in the
    /// `256 - opacity` levels that leaves and step into bands. The composite
    /// this crate performs spends that missing resolution across the area
    /// instead ([`DitherRow`]), which no layer stack can express, so the
    /// frame goes through software exactly as a backdrop blur does. A
    /// window's own antialiased corner is not this case — its partial
    /// coverage is an edge a few pixels wide, with no gradient to band.
    fn has_translucent_window(&self) -> bool {
        self.windows
            .iter()
            .any(|window| window.is_visible() && window.opacity() != u8::MAX)
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
    /// The layers below a frost are composed only where the frost will not
    /// write over them ([`frost_spared`](Self::frost_spared)). A retained frost
    /// is copied on top of whatever is there, so composing the stack underneath
    /// it first is work the copy throws away — for a frosted terminal being
    /// dragged, a whole window's worth of blending per pointer sample.
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
            if self.windows.get(index).is_none_or(|w| !w.reads_backdrop()) {
                continue;
            }
            let plan = self.frost_plan(index);
            self.compose_plane(
                area,
                self.frost_spared(index, plan),
                under,
                hits.get(start..split),
                fallback,
            );
            self.frost_segment(index, plan, area);
            start = split;
            under = None;
        }
        self.compose_span(area, under, hits.get(start..), fallback, Pass::Finish);
    }

    /// Encode `area`'s scan-out bytes from the back buffer as it stands, at the
    /// current reveal strength.
    ///
    /// The same row and band plumbing every composite runs, with no layer to
    /// lay: no root fill, no desktop layer, no window, no cursor, and so no
    /// furniture — which is why the fallback it is handed is empty rather than
    /// built. What a fade step costs is then one encode of what is already
    /// composed.
    fn encode_rect(&mut self, area: Rect) {
        let unread = ChromeFallback::new();
        self.compose_span(area, None, None, &unread, Pass::Rescan);
    }

    /// Compose the layers `span` names over `area` except `spared`, which the
    /// frost above them is about to write over.
    ///
    /// The remainder is composed as the disjoint rectangles it is, never as the
    /// box around them: for a window whose frost survives in full that
    /// remainder is usually nothing at all, and for one whose border has to be
    /// blurred it is the ring the border reads from. Both are strictly less than
    /// the rectangle, and neither is a different composite — the same
    /// [`compose_span`](Self::compose_span) writes the same pixels, over fewer
    /// columns.
    fn compose_plane(
        &mut self,
        area: Rect,
        spared: Rect,
        under: Option<Pixel>,
        span: Option<&[usize]>,
        fallback: &ChromeFallback,
    ) {
        if spared.is_empty() {
            self.compose_span(area, under, span, fallback, Pass::Compose);
            return;
        }
        // Taken out and put back so the region keeps the buffers it grew;
        // composing borrows the compositor mutably.
        let mut plane = core::mem::take(&mut self.plane);
        plane.clear();
        plane.add(area);
        plane.subtract(spared);
        for rect in plane.rects() {
            self.compose_span(*rect, under, span, fallback, Pass::Compose);
        }
        self.plane = plane;
    }

    /// The part of the window at z-index `index` its frost will write over
    /// without reading, so the layers below need not be composed there.
    ///
    /// A whole retained frost is copied over its entire rectangle, so all of it
    /// is spared. A frost kept only in its core has its border blurred, and that
    /// blur *reads* the backdrop up to `radius` inside the core, so only the
    /// core taken in by the radius is spared. A frost being blurred outright
    /// reads the whole rectangle and spares nothing.
    ///
    /// Nothing is spared for a frost the cache no longer holds, which the plan
    /// says cannot happen — every path that drops an entry rewrites the plan —
    /// but asking here and copying in [`frost_segment`](Self::frost_segment)
    /// with only a composite in between is what makes the pair agree by
    /// construction rather than by that argument.
    fn frost_spared(&self, index: usize, plan: FrostPlan) -> Rect {
        let Some(window) = self.windows.get(index) else {
            return Rect::EMPTY;
        };
        match plan {
            FrostPlan::Whole if self.frost_retained(window.id()) => {
                window.bounds().intersection(&self.screen_rect())
            }
            FrostPlan::Core(core) => inset(core, self.blur_radius_px(window)),
            FrostPlan::Whole | FrostPlan::Blur => Rect::EMPTY,
        }
    }

    /// Whether the cache holds a frost for the window `id` names at the current
    /// epoch, without counting a lookup or touching its recency.
    fn frost_retained(&self, id: WindowId) -> bool {
        self.frost.peek(&self.frost_epoch(), &id).is_some()
    }

    /// Copy `keep` of the retained frost of the window `id` names into the back
    /// buffer, reporting whether there was one to copy.
    fn restore_frost(&mut self, id: WindowId, keep: Rect) -> bool {
        let epoch = self.frost_epoch();
        let Self { frost, back, .. } = self;
        let Some(retained) = frost.peek(&epoch, &id) else {
            return false;
        };
        retained.restore(back, keep);
        true
    }

    /// Put the frosted backdrop of the window at `index` into the back buffer
    /// where `area` reaches it, doing as little of the blur as `plan` allows.
    ///
    /// Every arm produces the same bytes — a retained frost *is* the blur's own
    /// output, taken from this buffer when it was last computed — so this is a
    /// specialisation of the blur, never a second frosting. What differs is how
    /// much of the rectangle has to be blurred again:
    ///
    /// - [`Whole`](FrostPlan::Whole): none of it. A copy needs only the part of
    ///   the rectangle this rectangle of damage actually touches.
    /// - [`Core`](FrostPlan::Core): the border around the core, which the plan
    ///   guarantees is inside `area` because it promoted the whole window. The
    ///   border is blurred *before* the core is copied back, because the border's
    ///   own neighbourhood reaches into the core and what it must read there is
    ///   the backdrop, not the frost of it.
    /// - [`Blur`](FrostPlan::Blur): all of it.
    ///
    /// A frost the frame recomputed any part of is captured whole, so the next
    /// frame compares against where the window is *now* rather than eroding the
    /// same core until nothing is left of it.
    fn frost_segment(&mut self, index: usize, plan: FrostPlan, area: Rect) {
        let screen = self.screen_rect();
        let Some(window) = self.windows.get(index) else {
            return;
        };
        let (id, shape) = (window.id(), window.shape());
        let bounds = window.bounds();
        let radius_px = self.blur_radius_px(window);
        match plan {
            FrostPlan::Whole => {
                if self.restore_frost(id, area) {
                    return;
                }
                // Unreachable while the plan and the cache agree, and correct
                // anyway: nothing was spared for a frost that is not there, so
                // the layers below this rectangle were composed after all.
                self.blur_backdrop(index, Rect::EMPTY);
            }
            FrostPlan::Core(core) => {
                self.blur_backdrop(index, core);
                self.restore_frost(id, core);
            }
            FrostPlan::Blur => self.blur_backdrop(index, Rect::EMPTY),
        }
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
    /// take the same [`compose_segment`] path they always did, over the same
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
        pass: Pass,
    ) {
        let epoch = self.chrome_epoch();
        #[cfg(test)]
        let opaque_runs = self.opaque_runs;
        #[cfg(not(test))]
        let opaque_runs = true;
        let runner = self.runner;
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
        // finishes the rectangle draws it. A rescan lays no layer at all.
        let cursor = cursor.as_ref().filter(|_| pass == Pass::Finish);
        // The desktop sits directly under the windows, so it belongs to
        // the segment that starts from the root fill; a continuing segment
        // finds it already in the back buffer.
        let desktop = under.and(desktop.as_ref());
        let span = span.unwrap_or(&[]);
        let (Ok(first_col), Ok(cols)) = (usize::try_from(area.left()), usize::try_from(area.width))
        else {
            return;
        };
        let (Some(first_byte), Some(row_bytes)) = (first_col.checked_mul(4), cols.checked_mul(4))
        else {
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
        let shared = SpanShared {
            area,
            under,
            desktop,
            sources: &sources,
            cursor,
            order,
            reveal,
            opaque_runs,
            pass,
            stride,
            first_byte,
            row_bytes,
        };
        // The rectangle is screen-clipped before it gets here, so its rows are
        // exactly the back buffer's and the conversion cannot lose one.
        let (Ok(top), Ok(bottom)) = (u32::try_from(area.top()), u32::try_from(area.bottom()))
        else {
            return;
        };
        let rows = usize::try_from(bottom.saturating_sub(top)).unwrap_or(0);
        if rows == 0 {
            return;
        }
        // A band is worth handing off only once it carries enough pixels to
        // dwarf the dispatch, so the grain is a pixel budget expressed in this
        // rectangle's own rows: a narrow rectangle needs many rows to reach it,
        // a full-width one needs few.
        let count =
            tairix_parallel::bands(runner, rows, MIN_PARALLEL_BAND_PX.div_ceil(cols.max(1)));
        let per_band = u32::try_from(rows.div_ceil(count.max(1)))
            .unwrap_or(u32::MAX)
            .max(1);
        // Resolved once for the whole rectangle rather than per row, and the
        // band split below divides exactly this region: a segment that cannot
        // reach its scan-out bytes composes nothing, so the back buffer can
        // never drift from the frame.
        let (Some(band_bytes), Some(region)) = (
            usize::try_from(per_band)
                .ok()
                .and_then(|n| n.checked_mul(stride)),
            frame_region(frame, top, bottom, stride),
        ) else {
            return;
        };
        // Both splits step the same number of rows over the same row span, so
        // band *i* owns the scan-out bytes of exactly the rows it composes.
        let mut bands = back
            .row_bands_mut(top..bottom, per_band)
            .zip(region.chunks_mut(band_bytes))
            .map(|(back, frame)| SpanBand {
                back,
                frame,
                work: BandWork::default(),
            });
        if count <= 1 {
            // One band is the whole rectangle: no vector, no dispatch, and the
            // same per-row body a wide composite runs.
            if let Some(mut only) = bands.next() {
                compose_band(&shared, &mut only);
                only.work.record(stats);
            }
            return;
        }
        let mut split: Vec<SpanBand<'_>> = bands.collect();
        tairix_parallel::for_each(runner, &mut split, &|band| compose_band(&shared, band));
        for band in &split {
            band.work.record(stats);
        }
    }

    /// Spread each composite's per-pixel work across `runner`'s participants.
    ///
    /// A composite is a stack of passes over rectangles whose rows are
    /// independent by construction — each writes one row of the back buffer and
    /// the scan-out bytes of that row, and reads only immutable window content —
    /// so handing bands of them to other cores changes the wall-clock cost and
    /// nothing else: the composed pixels are bit-for-bit what one thread
    /// produces, whatever order the bands run in.
    ///
    /// The default is [`tairix_parallel::SERIAL`], which composes on the calling
    /// thread. A single-CPU machine, a headless build, and a process the kernel
    /// would grant no thread all keep exactly that, and pay nothing for the
    /// machinery they do not use.
    pub fn set_job_runner(&mut self, runner: &'static dyn JobRunner) {
        self.runner = runner;
    }

    /// The runner installed by [`set_job_runner`](Self::set_job_runner).
    ///
    /// A session converts an application's presented frame into that window's
    /// own content surface — a whole-window pass it cannot bound, since the app
    /// declares the damage — and spreads it across the very same participants
    /// the composite uses. Reading the installed runner back is what keeps one
    /// answer to "how wide is this machine" rather than two installations that
    /// could drift.
    #[must_use]
    pub fn job_runner(&self) -> &'static dyn JobRunner {
        self.runner
    }

    /// Frost the back buffer inside the rectangle of the window at `index`,
    /// weighted by that window's own shape coverage, leaving a frosted
    /// backdrop for the window's pixels to be blended over — everywhere except
    /// `keep`, whose pixels the caller is about to copy from the frost it
    /// retained.
    ///
    /// The rectangle is the window's whole on-screen bounds every time —
    /// the compose plan promotes it whenever a frost must be recomputed — so
    /// the frosting a given backdrop produces never depends on which part
    /// of the window a repaint started from. Coverage weights the mix
    /// rather than clipping it, so a rounded corner fades from frosted to
    /// untouched across exactly the arc the window's own pixels fade over
    /// and no square edge shows outside a rounded window.
    ///
    /// `keep` is the whole rectangle's own answer being reused, not a smaller
    /// frost: the shared frost still replicates at the rectangle's edges and
    /// reads coverage at the rectangle's coordinates, so the border it writes
    /// around `keep` is bit-for-bit what a full blur would have written there
    /// (`Surface::frost_region_around`). An empty `keep` frosts all of it.
    ///
    /// The shared frost confines the effect to that rectangle and replicates
    /// its edges, so it can never pull a neighbour's pixels into a window nor
    /// write outside its own bounds, and it works in the scratch this
    /// compositor owns and reuses.
    fn blur_backdrop(&mut self, index: usize, keep: Rect) {
        let screen = self.screen_rect();
        let radius = self
            .windows
            .get(index)
            .map_or(0, |window| self.blur_radius_px(window));
        let runner = self.runner;
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
        let kept = keep.intersection(&region);
        let (cols, rows) = (
            local_span(kept.left(), kept.right(), region.left()),
            local_span(kept.top(), kept.bottom(), region.top()),
        );
        // Only a real blur is charged. A window that retains its backdrop
        // without one — translucent but unblurred — comes through here with
        // radius zero, and the shared frost leaves the composed layers exactly
        // as it found them, so counting those pixels would report blur work
        // that never happened.
        if radius > 0 {
            stats.add_blur(
                area_px(region.width, region.height)
                    .saturating_sub(area_px(kept.width, kept.height)),
            );
        }
        back.frost_region_around(
            left,
            top,
            region.width,
            region.height,
            cols,
            rows,
            radius,
            blur_scratch,
            runner,
            |lx, ly| {
                shape.map_or(255, |shape| {
                    shape.coverage(shape_x.saturating_add(lx), shape_y.saturating_add(ly))
                })
            },
        );
    }
}

/// `start..end`, screen coordinates, as the offsets from `origin` a surface
/// rectangle is spelled in — empty where the rectangle is.
fn local_span(start: i32, end: i32, origin: i32) -> Range<u32> {
    let offset = |at: i32| u32::try_from(at.saturating_sub(origin)).unwrap_or(0);
    let (from, until) = (offset(start), offset(end));
    from..until.max(from)
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

/// How a composed row rounds into its 8-bit targets: the screen reveal the
/// scan-out byte is dimmed by, and the ordered dither every blend on the row
/// varies its rounding with.
#[derive(Copy, Clone)]
struct RowRound {
    reveal: u8,
    dither: DitherRow,
}

/// Everything a band of one segment reads. Identical for every band of it, so it
/// is resolved once per rectangle and shared.
struct SpanShared<'a> {
    area: Rect,
    /// What the layers compose over, or `None` to continue from the back buffer.
    under: Option<Pixel>,
    desktop: Option<&'a Surface>,
    /// Each covering window with the furniture it draws from.
    sources: &'a [(&'a Window, Option<&'a WindowChrome>)],
    cursor: Option<&'a PlacedCursor>,
    order: ChannelOrder,
    reveal: u8,
    opaque_runs: bool,
    pass: Pass,
    /// Scan-out bytes per screen row.
    stride: usize,
    /// Byte offset of the rectangle's first column within a scan-out row.
    first_byte: usize,
    /// Bytes one row of the rectangle occupies.
    row_bytes: usize,
}

/// What one pass over a rectangle's rows is for.
///
/// A rectangle whose layers must be composed is walked in *segments* — one per
/// frosted window that reads the backdrop — and only the last of them draws the
/// cursor and encodes, so the scan-out sees each pixel once, finished.
#[derive(Copy, Clone, Eq, PartialEq)]
enum Pass {
    /// Compose the layers into the back buffer and leave the scan-out to the
    /// segment that finishes the rectangle.
    Compose,
    /// Compose the layers, the cursor over them, and encode the result.
    Finish,
    /// Encode the back buffer as it already stands: no layer is read and no
    /// composed pixel is written. What a screen fade needs, and all it needs.
    Rescan,
}

/// One band of a segment: the back-buffer rows it owns, the scan-out bytes of
/// exactly those rows, and what composing them cost.
struct SpanBand<'a> {
    back: tairix_raster::RowBand<'a>,
    frame: &'a mut [u8],
    work: BandWork,
}

/// What composing one band cost, tallied in the band and folded into the frame's
/// counters once it is done — so a band never touches shared state while it runs.
#[derive(Default)]
struct BandWork {
    blended: u64,
    copied: u64,
    encoded: u64,
}

impl BandWork {
    /// Fold this band's cost into the frame's counters.
    fn record(&self, stats: &mut FrameCounters) {
        stats.add_blended(self.blended);
        stats.add_opaque(self.copied);
        stats.add_encoded(self.encoded);
    }
}

/// The scan-out bytes of rows `[top, bottom)`, or `None` when the frame does not
/// hold them.
fn frame_region(frame: &mut [u8], top: u32, bottom: u32, stride: usize) -> Option<&mut [u8]> {
    let start = usize::try_from(top).ok()?.checked_mul(stride)?;
    let end = usize::try_from(bottom).ok()?.checked_mul(stride)?;
    frame.get_mut(start..end)
}

/// Compose one band's rows of a segment.
///
/// This is the whole of a segment's per-row work, whichever way the rectangle was
/// split: a rectangle composed on the calling thread is one band through here, and
/// a rectangle spread across cores is several. There is no second row loop.
fn compose_band(shared: &SpanShared<'_>, band: &mut SpanBand<'_>) {
    let SpanShared {
        area,
        under,
        desktop,
        sources,
        cursor,
        order,
        reveal,
        opaque_runs,
        pass,
        stride,
        first_byte,
        row_bytes,
    } = *shared;
    let encode = pass != Pass::Compose;
    let Ok(left) = u32::try_from(area.left()) else {
        return;
    };
    let rows = band.back.rows();
    let base = rows.start;
    let mut window_rows: Vec<WindowRow<'_>> = Vec::with_capacity(sources.len());
    for py in rows {
        let Ok(y) = i32::try_from(py) else { continue };
        let Some((_, back_row)) = band.back.row_span_mut(py, left, area.width) else {
            continue;
        };
        // Resolved even when this segment does not encode, so every segment over
        // one rectangle keeps or skips exactly the same rows and the back buffer
        // can never drift from the frame.
        let Some(frame_row) = usize::try_from(py.saturating_sub(base))
            .ok()
            .and_then(|local| local.checked_mul(stride))
            .and_then(|row_start| row_start.checked_add(first_byte))
            .and_then(|start| band.frame.get_mut(start..start.checked_add(row_bytes)?))
        else {
            continue;
        };
        if pass == Pass::Rescan {
            // The composed row is already what it should be; only the strength
            // it reaches the display at moved.
            encode_segment(frame_row, back_row, order, reveal);
            band.work.encoded = band.work.encoded.saturating_add(u64::from(area.width));
            continue;
        }
        window_rows.clear();
        window_rows.extend(
            sources
                .iter()
                .filter_map(|(window, chrome)| window.row(y, *chrome)),
        );
        let dither = DitherRow::at(py);
        let cursor_row = cursor.and_then(|c| c.local_row(y).map(|ly| (c, ly)));
        let desktop_row = desktop.map(|layer| crate::surface::row(layer, py));
        // The screen reveal is applied as a pixel is encoded, so a fade in flight
        // has no run a plain copy could serve; the cursor is resolved per row, so
        // only the few rows it draws on lose the fast path.
        let layers = RowLayers {
            under,
            desktop: desktop_row,
            windows: &window_rows,
            cursor: cursor_row,
            front: window_rows
                .last()
                .filter(|_| opaque_runs && cursor_row.is_none() && (!encode || reveal == u8::MAX)),
        };
        let targets = RowTargets {
            back: back_row,
            frame: encode.then_some(frame_row),
        };
        let work = compose_row(
            &layers,
            targets,
            area.left(),
            order,
            RowRound { reveal, dither },
        );
        if encode {
            band.work.encoded = band.work.encoded.saturating_add(u64::from(area.width));
        }
        band.work.blended = band.work.blended.saturating_add(work.blended);
        band.work.copied = band.work.copied.saturating_add(work.copied);
    }
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
/// them is skipped; the columns between two such runs are composed as one
/// **segment** ([`compose_segment`]), and a row with no copyable run at all is
/// one segment from end to end. A run whose destination slice cannot be
/// resolved encodes nothing and falls through to the general path, so the
/// frame can never be left holding stale bytes.
fn compose_row(
    layers: &RowLayers<'_>,
    targets: RowTargets<'_>,
    first_col: i32,
    order: ChannelOrder,
    round: RowRound,
) -> RowWork {
    let RowTargets { back, mut frame } = targets;
    let RowRound { reveal, dither } = round;
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
        // As far as the front-most window's next copyable run, so the columns
        // between two runs are composed in one pass rather than one at a time.
        let segment = layers
            .front
            .map_or(cols - col, |row| row.blend_len(x, limit))
            .clamp(1, cols - col);
        let Some(dst) = back.get_mut(col..col + segment) else {
            break;
        };
        work.blended = work
            .blended
            .saturating_add(compose_segment(dst, layers, x, dither));
        if let Some(bytes) = frame
            .as_deref_mut()
            .and_then(|bytes| bytes.get_mut(col * 4..(col + segment) * 4))
        {
            encode_segment(bytes, dst, order, reveal);
        }
        col += segment;
        x = x.saturating_add_unsigned(u32::try_from(segment).unwrap_or(u32::MAX));
    }
    work
}

/// Compose the columns of one screen row that `dst` covers, from screen column
/// `first_x`: the root fill or whatever the back buffer already held, then the
/// desktop layer, then each window row back to front, then the cursor. Returns
/// how many layer contributions were blended, which is the cost the frame
/// counters report.
///
/// The layers are laid **one at a time over the whole segment**, not one column
/// at a time through the whole stack. Each is a straight run of source pixels
/// at a screen column and a constant opacity ([`blend_run`]), so the arithmetic
/// per pixel is the same *over* at the same rounding while the layer decision,
/// the coordinate conversion, and the bounds checks around it are paid once per
/// run instead of once per pixel — which measurement showed was where a
/// translucent composite's time actually went. A pixel still sees the layers in
/// exactly the order it saw them column by column, so the result is unchanged.
///
/// A window row the shape cuts, and the cursor, keep the column-by-column walk
/// inside their own contribution ([`WindowRow::blend_into`]); they are a few
/// rows of a frame, and their coverage genuinely varies per column.
fn compose_segment(
    dst: &mut [Pixel],
    layers: &RowLayers<'_>,
    first_x: i32,
    dither: DitherRow,
) -> u64 {
    if let Some(base) = layers.under {
        dst.fill(base);
    }
    // The desktop layer is a whole screen row, so its first pixel is screen
    // column zero.
    let mut blended = layers.desktop.map_or(0, |desktop| {
        blend_run(dst, first_x, desktop, 0, u8::MAX, dither)
    });
    for row in layers.windows {
        blended = blended.saturating_add(row.blend_into(dst, first_x, dither));
    }
    if let Some((cursor, ly)) = layers.cursor {
        for (dst, x) in dst.iter_mut().zip(first_x..) {
            let bias = dither.bias(x.cast_unsigned());
            if let Some(src) = cursor.sample_row(x, ly) {
                *dst = src.over_biased(*dst, bias);
                blended = blended.saturating_add(1);
            }
        }
    }
    blended
}

/// Encode `pixels` into `bytes` as scan-out, dimmed by the screen reveal.
///
/// A fully-revealed screen — every frame but the few of a fade — is the shared
/// run encoder over the whole segment.
fn encode_segment(bytes: &mut [u8], pixels: &[Pixel], order: ChannelOrder, reveal: u8) {
    if reveal == u8::MAX {
        // Four bytes per pixel of the segment by construction, so the shared
        // encoder takes all of them.
        let _encoded = order.encode_run(pixels, bytes);
        return;
    }
    for (slot, pixel) in bytes.as_chunks_mut::<4>().0.iter_mut().zip(pixels) {
        *slot = order.encode(pixel.dimmed(reveal));
    }
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
