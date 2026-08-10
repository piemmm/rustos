//! The frosted backdrop of a backdrop-blurred window, and the bounded,
//! pressure-governed cache that retains it between frames.
//!
//! # Why it is retained
//!
//! A frosted window's pixels are blended over a blur of *everything beneath
//! its rectangle*, so the compositor composes the layers below it, blurs that
//! rectangle of the back buffer, and resumes from the window itself. The blur
//! is two separable passes over the whole rectangle whatever the frame
//! actually changed, which made the dominant interaction — the pointer moving
//! inside a frosted terminal — cost a full-window frost per sample: measured
//! at 17.4 ms for a 64×24 repaint, against 0.9 µs for the same repaint over an
//! opaque stack.
//!
//! Nothing about that blur changes when the window's own content changes.
//! Retaining it turns the dominant case into a row copy.
//!
//! # What makes a retained one invalid
//!
//! A frost is a function of exactly four things: the composed pixels beneath
//! it, the window's own rectangle, the physical blur radius, and the window
//! shape the mix is weighted by. The last three are recorded in the entry and
//! compared on every lookup, so a geometry, radius, scale, or corner change
//! fails closed to a recompute even if the compositor forgot to say so.
//!
//! The rectangle recorded is the window's *whole* one, not the part of it that
//! is on screen. A window pushed off an edge is frosted from the row and
//! column the screen begins at while its shape is still read from its own
//! top-left, so two positions that clip to the same on-screen rectangle can
//! weight the same pixels by different parts of the shape.
//!
//! The first cannot be self-checked without reading the pixels it would have
//! saved, so the compositor drops the entry when it marks damage that changes
//! them: damage below the window, or a change to the window's own position,
//! size, stacking, or shape. Damage from the window's own content, or from
//! anything stacked above it, changes nothing it reads.

use tairix_log::Sink;
use tairix_reclaim::{screenful_ui_cache, CachedBytes, PressureGauge, ReclaimCache};

use crate::geometry::Rect;
use crate::surface::{self, Surface};
use crate::window::{WindowId, WindowShape};

/// Worst-case per-entry bookkeeping the cache charges on top of the frosted
/// pixels: the LRU/index tick and charged-size fields (`u64` + `usize`), this
/// cache's small share of its two `BTreeMap`s' node overhead, and the
/// rectangle, radius, shape, and [`Surface`] header a [`FrostedBackdrop`]
/// holds inline. The [`WindowId`] key is one `u64`, already covered here.
const ENTRY_METADATA_BYTES: usize = 128;

/// The epoch a retained [`FrostedBackdrop`] is valid for: this output's scale
/// (in percent) paired with the screen extent frosts were clipped to.
///
/// Both are already caught per entry — the scale through the physical radius,
/// the screen through the clipped rectangle — so the epoch is not what keeps a
/// stale frost off the screen. It is what stops a superseded one *staying
/// charged*: a density or mode change invalidates every window's frost at
/// once, and releasing them together gives the budget back to the frosts that
/// are still live instead of squeezing them until each stale entry is next
/// looked up.
///
/// The theme is deliberately absent, unlike [`ChromeEpoch`](crate::ChromeEpoch).
/// Furniture is *painted* from the palette; a frost only blurs whatever the
/// layers below it composed, and a palette change repaints those layers and
/// marks them damaged, which drops the frosts that read them.
pub type FrostEpoch = (u32, u32, u32);

/// Build the one [`ReclaimCache`] a [`Compositor`](crate::Compositor) retains
/// frosted backdrops in, classified through the shared desktop cache policy
/// (`tairix_reclaim::screenful_ui_cache`).
///
/// `seat` is the seat the output belongs to and `fb_bytes` is the real
/// output's backing byte size, which is also this cache's ceiling: a frost is
/// exactly the visible part of one window, so no more of it than fills the
/// screen can be on screen at once. `pressure` and `sink` are the process's
/// live pressure gauge and audit sink. The embedder — the only party that
/// knows all four — calls this once and hands the result to
/// [`Compositor::new`](crate::Compositor::new).
#[must_use]
pub fn frost_cache(
    seat: u64,
    fb_bytes: usize,
    pressure: &'static (dyn PressureGauge + 'static),
    sink: &'static (dyn Sink + Sync),
) -> ReclaimCache<WindowId, FrostedBackdrop, FrostEpoch> {
    screenful_ui_cache(
        "wm.frost",
        seat,
        fb_bytes,
        ENTRY_METADATA_BYTES,
        pressure,
        sink,
    )
}

/// One window's frosted backdrop: the blurred, shape-weighted pixels the
/// window's own content is blended over, and the three facts they are a
/// function of besides the layers beneath them.
///
/// This is an opaque cache payload. Nothing outside the window manager reads
/// its pixels, so it exposes no accessors beyond the [`CachedBytes`] contract
/// the cache holding it requires.
pub struct FrostedBackdrop {
    /// The whole rectangle of the window these pixels frost, on screen or
    /// not — what the blur is a function of.
    bounds: Rect,
    /// The part of `bounds` that is on screen, which is what `pixels` covers.
    rect: Rect,
    /// The physical radius they were blurred by — the logical radius through
    /// the output's scale, so a density change reads as a different frost.
    radius_px: u32,
    /// The shape the blurred copy was mixed back in through, or `None` for a
    /// square window that took the whole rectangle.
    shape: Option<WindowShape>,
    /// The frosted pixels, `rect`-sized, premultiplied, in row-major order.
    pixels: Surface,
}

impl CachedBytes for FrostedBackdrop {
    /// The retained heap size of the frosted pixels — the only heap this
    /// payload owns.
    fn payload_bytes(&self) -> usize {
        self.pixels.payload_bytes()
    }

    /// Overwrite the frosted pixels, so a released entry leaves no readable
    /// image of the user's desktop behind in reusable heap.
    fn wipe(&mut self) {
        self.pixels.wipe();
    }
}

impl FrostedBackdrop {
    /// Take a copy of the frost `back` now holds where the window at `bounds`
    /// reaches `screen`, or `None` when that is nothing or a buffer that size
    /// cannot be allocated — in which case the caller keeps the frame it
    /// already composed and simply retains nothing.
    ///
    /// The clipping is done here rather than by the caller so the recorded
    /// rectangle and the pixels it names are derived together and cannot
    /// disagree.
    ///
    /// [`Surface::blit`] composites through the premultiplied *over* operator,
    /// and a fresh [`Surface::new`] starts fully transparent, so compositing
    /// the back buffer over it reproduces those pixels exactly. That is the
    /// copy path the furniture strips already take, rather than a second
    /// hand-written row loop.
    pub(crate) fn capture(
        back: &Surface,
        bounds: Rect,
        screen: Rect,
        radius_px: u32,
        shape: Option<WindowShape>,
    ) -> Option<Self> {
        let rect = bounds.intersection(&screen);
        if rect.is_empty() {
            return None;
        }
        let mut pixels = Surface::new(rect.width, rect.height)?;
        pixels.blit(-rect.left(), -rect.top(), back);
        Some(Self {
            bounds,
            rect,
            radius_px,
            shape,
            pixels,
        })
    }

    /// Whether this frost is the one a window occupying `bounds` with physical
    /// radius `radius_px` and shape `shape` would produce, given the layers
    /// beneath it are unchanged.
    ///
    /// `bounds` is the window's whole rectangle, not the on-screen part of it:
    /// where the two differ, the offset between them is what the shape is read
    /// through, so two positions clipping alike are still two different
    /// frosts.
    pub(crate) fn matches(&self, bounds: Rect, radius_px: u32, shape: Option<WindowShape>) -> bool {
        self.bounds == bounds && self.radius_px == radius_px && self.shape == shape
    }

    /// Write the part of this frost that lies inside `area` back into `back`,
    /// replacing what the layers beneath just composed there.
    ///
    /// This is what the blur would have written, so it is a plain copy and not
    /// a blend. A row the back buffer will not admit is skipped rather than
    /// written short: the caller composed the layers below first, so a skipped
    /// row shows the unfrosted backdrop rather than stale bytes.
    pub(crate) fn restore(&self, back: &mut Surface, area: Rect) {
        let target = area.intersection(&self.rect);
        let (Ok(left), Ok(top)) = (u32::try_from(target.left()), u32::try_from(target.top()))
        else {
            return;
        };
        let local_x = u32::try_from(target.left().saturating_sub(self.rect.left())).unwrap_or(0);
        let local_y = u32::try_from(target.top().saturating_sub(self.rect.top())).unwrap_or(0);
        for row in 0..target.height {
            let source = surface::row(&self.pixels, local_y.saturating_add(row));
            let Some((start, destination)) =
                back.row_span_mut(top.saturating_add(row), left, target.width)
            else {
                continue;
            };
            // A clip window that cut the span's leading columns advances the
            // source by as much, so the two stay aligned.
            let lead = usize::try_from(local_x.saturating_add(start.saturating_sub(left)))
                .unwrap_or(usize::MAX);
            let Some(source) = source.get(lead..) else {
                continue;
            };
            let len = destination.len().min(source.len());
            if let (Some(destination), Some(source)) =
                (destination.get_mut(..len), source.get(..len))
            {
                destination.copy_from_slice(source);
            }
        }
    }
}
