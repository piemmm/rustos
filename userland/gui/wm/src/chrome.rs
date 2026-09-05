//! Window chrome: the four furniture strips a decorated window is drawn
//! from, and the bounded, pressure-governed cache that retains them.
//!
//! # Strips, not one outer-sized surface
//!
//! A decorated window's whole [`WindowFrame`] does not fit in one
//! outer-window-sized [`Surface`] worth keeping: the client region of such a
//! surface is never sampled — the compositor draws the window's own content
//! there, never the decoration's — so for a large window almost all of it
//! would be allocated, cleared, and rendered for nothing that is ever read.
//!
//! [`WindowChrome`] holds only the four furniture strips the frame actually
//! draws into: the top (title) band, the bottom band, and the left and right
//! side borders. Retained memory scales with the reserved frame band (a fixed
//! size for a given theme/scale), never with the window's client area, so a
//! tall or wide window's decoration does not grow past what its border
//! thickness needs. Nothing outer-sized is allocated on the way there either:
//! each strip is a [`Surface`] the size of its own band that the frame paints
//! *into* as one rectangle of the whole window
//! ([`Surface::with_origin`](crate::surface::Surface::with_origin)), so a
//! window's furniture costs its bands rather than its area even transiently.
//!
//! The top and bottom strips are as deep as the rim's corner radius wherever
//! the reserved inset is thinner than it, because a corner arc must be drawn as
//! furniture over the whole width of its rows or the client's square row would
//! be the only pixels there. The window decides those depths; the strips here
//! are simply cut to the rectangles it gives.
//!
//! # Retained by the compositor, not by the window
//!
//! The strips are *derived* pixels: losing them costs a re-render and never
//! a wrong result. They are therefore held in a [`ReclaimCache`]
//! (`plans/SMARTRAM.md` section 6.4) that the compositor owns and every
//! window shares, rather than in each window, so the desktop's total
//! furniture is bounded, charged to the seat, wiped on release, and given
//! back the moment the machine reports memory pressure. [`chrome_cache`] is
//! the one place this crate assembles that cache; the compositor is handed
//! the result and never builds its own policy, because a cache built
//! without a live gauge would classify and serve every lookup correctly
//! while retaining nothing — a defect that looks exactly like working
//! software.
//!
//! Furniture carries the window's title, which is user data, so the cache's
//! declared sensitivity makes [`CachedBytes::wipe`] a real obligation:
//! every released strip is overwritten before its heap becomes reusable.

use tairix_controls::WindowFrame;
use tairix_hash::BuildFastHash;
use tairix_icon::IconPicture;
use tairix_log::Sink;
use tairix_reclaim::{screenful_ui_cache, CachedBytes, PressureGauge, ReclaimCache};
use tairix_theme::Theme;

use crate::color::Pixel;
use crate::geometry::{Rect, Scale};
use crate::surface::{self, Surface};
use crate::window::WindowId;

/// Worst-case per-entry bookkeeping the cache charges on top of a window's
/// own strip pixels: the LRU/index tick and charged-size fields (`u64` +
/// `usize`), this cache's small share of its two `BTreeMap`s' node
/// overhead, and the four `Option<Surface>` headers a [`WindowChrome`]
/// holds inline. The [`WindowId`] key is one `u64`, already covered here.
const ENTRY_METADATA_BYTES: usize = 192;

/// The epoch a retained [`WindowChrome`] is valid for: this output's scale
/// (in percent) paired with the compositor's theme generation. A DPI change
/// or a theme switch moves the epoch on and drops every window's furniture
/// at once, which is exactly the set a new density or palette re-renders.
///
/// The theme half is a generation counter rather than the theme's own
/// [`ThemeId`](tairix_theme::ThemeId) because two distinct themes may
/// legitimately share an id — a contrast or motion variant of the built-in
/// dark theme keeps `ThemeId::DARK` — and serving furniture painted from
/// the superseded palette would be a visibly wrong pixel, not merely a
/// missed cache hit.
pub type ChromeEpoch = (u32, u64);

/// Build the one [`ReclaimCache`] a [`Compositor`](crate::Compositor)
/// retains rendered window furniture in, classified through the shared
/// desktop cache policy (`tairix_reclaim::screenful_ui_cache`).
///
/// `seat` is the seat the output belongs to and `fb_bytes` is the real
/// output's backing byte size, which is also this cache's ceiling: no more
/// furniture than fills the screen can be visible at once, so anything
/// above a screenful belongs to minimised, off-screen, or stacked-under
/// windows and is the first thing eviction should take. `pressure` and
/// `sink` are the process's live pressure gauge and audit sink. The
/// embedder — the only party that knows all four — calls this once and
/// hands the result to [`Compositor::new`](crate::Compositor::new).
#[must_use]
pub fn chrome_cache(
    seat: u64,
    fb_bytes: usize,
    pressure: &'static (dyn PressureGauge + 'static),
    sink: &'static (dyn Sink + Sync),
) -> ReclaimCache<WindowId, WindowChrome, ChromeEpoch, BuildFastHash> {
    screenful_ui_cache(
        "wm.chrome",
        seat,
        fb_bytes,
        ENTRY_METADATA_BYTES,
        pressure,
        sink,
        // Keyed by an identifier this compositor assigned itself, so the
        // fast unkeyed hash is correct and is named here.
        BuildFastHash::new(),
    )
}

/// The rendered furniture strips around a decorated window's client area,
/// in the window's own local coordinates (the outer rectangle's top-left at
/// `(0, 0)`).
///
/// A strip with zero extent — an undecorated edge a theme's frame never
/// reserves — holds no surface at all, rather than an empty one, so an
/// unused side costs nothing beyond the `None`.
///
/// This is an opaque cache payload: the compositor builds one per decorated
/// window and samples rows out of it while composing a frame. Nothing
/// outside the window manager reads its pixels, so it exposes no accessors
/// beyond the [`CachedBytes`] contract the cache holding it requires.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WindowChrome {
    top: Option<Surface>,
    bottom: Option<Surface>,
    left: Option<Surface>,
    right: Option<Surface>,
}

impl CachedBytes for WindowChrome {
    /// The retained heap size of the four strips — the only heap the
    /// chrome owns. A [`Surface`] already measures its own pixel buffer,
    /// so this only sums what is present.
    fn payload_bytes(&self) -> usize {
        self.strips()
            .into_iter()
            .flatten()
            .map(Surface::payload_bytes)
            .sum()
    }

    /// Overwrite every retained strip, so a released window's rendered
    /// title and frame leave nothing readable behind in reusable heap.
    fn wipe(&mut self) {
        for strip in self.strips_mut().into_iter().flatten() {
            strip.wipe();
        }
    }
}

impl WindowChrome {
    /// Render the furniture strips described by `bands` (the top, bottom,
    /// left, and right rectangles, in the window's own local space — see
    /// [`Window::local_furniture_bands`]) for `frame` at outer size
    /// `outer_size`, or `None` when a strip cannot be allocated (fail
    /// closed: the caller retains no half-painted frame).
    ///
    /// Each strip is painted by the whole frame, into a surface the size of
    /// that one band standing in for the band's rectangle of the window: the
    /// frame draws its rim, body, and title band across the outer rectangle
    /// in the window's own coordinates, and every write outside the band is
    /// off that surface and dropped. So a strip carries exactly the pixels
    /// the same rectangle of a whole-window render would have, and no buffer
    /// larger than a band is ever allocated.
    ///
    /// `artwork` is the owning application's identity icon, already
    /// rasterised at the title bar's slot side; it is handed straight to
    /// [`WindowFrame::render`], which draws the built-in glyph instead when
    /// it is `None`.
    ///
    /// [`Window::local_furniture_bands`]: crate::window::Window::local_furniture_bands
    pub(crate) fn render(
        frame: &WindowFrame,
        bands: [Rect; 4],
        outer_size: (u32, u32),
        scale: Scale,
        theme: &Theme,
        artwork: Option<&Surface>,
    ) -> Option<Self> {
        let (ow, oh) = outer_size;
        let outer = Rect::new(0, 0, ow, oh);
        // No corner grip: a resizable window's band is the plain frame inset,
        // too thin to hold one without painting into the client. The grab zone
        // is invisible, carried by `WindowFrame::hit`'s client overlap alone.
        let mut strips: [Option<Surface>; 4] = [None, None, None, None];
        for (strip, band) in strips.iter_mut().zip(bands) {
            let Some((x, y)) = strip_origin(band) else {
                continue;
            };
            let mut surface = Surface::new(band.width, band.height)?;
            surface.with_origin(x, y, |target| {
                frame.render(
                    target,
                    outer,
                    scale,
                    theme,
                    artwork.map(IconPicture::Artwork),
                );
            });
            *strip = Some(surface);
        }
        let [top, bottom, left, right] = strips;
        Some(Self {
            top,
            bottom,
            left,
            right,
        })
    }

    /// Row `local_y` of the top (title) strip, spanning the full outer width
    /// from local column `0`, or empty when there is no top band or the row
    /// is out of range.
    pub(crate) fn top_row(&self, local_y: u32) -> &[Pixel] {
        self.top
            .as_ref()
            .map_or(&[][..], |strip| surface::row(strip, local_y))
    }

    /// Row `local_y` of the bottom strip — `local_y` counted from the
    /// strip's own top edge, i.e. from the top of the bottom band — spanning
    /// the full outer width, or empty when there is no bottom band or the
    /// row is out of range.
    pub(crate) fn bottom_row(&self, local_y: u32) -> &[Pixel] {
        self.bottom
            .as_ref()
            .map_or(&[][..], |strip| surface::row(strip, local_y))
    }

    /// Row `local_y` of the left border strip — `local_y` counted from the
    /// strip's own top edge, i.e. from the bottom of the top band — spanning
    /// the left inset width, or empty when there is no left border or the row
    /// is out of range.
    pub(crate) fn left_row(&self, local_y: u32) -> &[Pixel] {
        self.left
            .as_ref()
            .map_or(&[][..], |strip| surface::row(strip, local_y))
    }

    /// Row `local_y` of the right border strip — `local_y` counted from the
    /// strip's own top edge, i.e. from the bottom of the top band — spanning
    /// the right inset width, or empty when there is no right border or the row
    /// is out of range.
    pub(crate) fn right_row(&self, local_y: u32) -> &[Pixel] {
        self.right
            .as_ref()
            .map_or(&[][..], |strip| surface::row(strip, local_y))
    }

    /// The four strips as a fixed array, so measuring and wiping walk one
    /// definition of "every strip this chrome holds" instead of naming the
    /// four fields twice each.
    fn strips(&self) -> [Option<&Surface>; 4] {
        [
            self.top.as_ref(),
            self.bottom.as_ref(),
            self.left.as_ref(),
            self.right.as_ref(),
        ]
    }

    /// The four strips as a fixed array of mutable borrows (see
    /// [`strips`](Self::strips)).
    fn strips_mut(&mut self) -> [Option<&mut Surface>; 4] {
        [
            self.top.as_mut(),
            self.bottom.as_mut(),
            self.left.as_mut(),
            self.right.as_mut(),
        ]
    }
}

/// Where `band` starts in the window's own coordinates, or `None` for a band
/// that holds no furniture: one with no extent, or — since those coordinates
/// begin at the window's outer top-left — one placed outside them.
fn strip_origin(band: Rect) -> Option<(u32, u32)> {
    if band.is_empty() {
        return None;
    }
    Some((
        u32::try_from(band.left()).ok()?,
        u32::try_from(band.top()).ok()?,
    ))
}
