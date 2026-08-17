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
//! them: damage below the window, or a change to the window's own stacking.
//! Damage from the window's own content, or from anything stacked above it,
//! changes nothing it reads.
//!
//! # Why a moved window keeps most of it
//!
//! Moving a window does not disturb the layers beneath it, so a frost taken
//! before the move is still exactly right — in *screen* coordinates — wherever
//! neither difference between the two positions can reach: the blur replicates
//! at its rectangle's edges, and the shape weights the mix by a window-local
//! coordinate. Both differences are confined to a border, so the pixels
//! `FrostedBackdrop::reuse` hands back as a core are bit-for-bit what
//! a fresh blur would write and only the border has to be blurred again
//! (`Surface::frost_region_around`). Without that, every sample of a drag paid
//! a full-window blur *and* a full-window composite of the layers under it, for
//! a picture that had moved a few pixels.

use tairix_log::Sink;
use tairix_reclaim::{screenful_ui_cache, CachedBytes, PressureGauge, ReclaimCache};

use crate::geometry::Rect;
use crate::surface::{self, Surface};
use crate::window::{WindowId, WindowShape};

/// What a frame must do about one backdrop-blurred window's frost.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub(crate) enum FrostPlan {
    /// Copy the whole retained frost: the window is exactly where it was, so
    /// nothing is blurred at all.
    Whole,
    /// Copy this screen rectangle of the retained frost and blur the border
    /// around it, because the window has moved, resized, or changed shape and
    /// only that border can differ.
    Core(Rect),
    /// Blur the whole rectangle: nothing retained applies to it.
    Blur,
}

/// `rect` with `by` pixels taken off every side, empty where that leaves
/// nothing.
pub(crate) fn inset(rect: Rect, by: u32) -> Rect {
    let shrink = by.saturating_mul(2);
    let (Some(width), Some(height)) = (
        rect.width.checked_sub(shrink),
        rect.height.checked_sub(shrink),
    ) else {
        return Rect::EMPTY;
    };
    Rect::new(
        rect.left().saturating_add_unsigned(by),
        rect.top().saturating_add_unsigned(by),
        width,
        height,
    )
}

/// How far into its own rectangle a shape's corners weight the mix by less
/// than full coverage: the corner radius the shape actually rounds by, or `0`
/// for a square window, whose every pixel is fully covered.
fn corner_reach(shape: Option<WindowShape>) -> u32 {
    shape.map_or(0, WindowShape::corner_reach)
}

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
    /// The copy is [`Surface::overwrite`], the shared blit walk laying rows
    /// down whole rather than compositing them: a snapshot replaces what it
    /// lands on by definition, and at a screenful a frame that is the
    /// difference between a row copy and reading and blending every pixel.
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
        pixels.overwrite(-rect.left(), -rect.top(), back);
        Some(Self {
            bounds,
            rect,
            radius_px,
            shape,
            pixels,
        })
    }

    /// How much of this frost a window now occupying `bounds` on `screen`,
    /// blurred by `radius_px` and shaped by `shape`, may keep — given the
    /// layers beneath it are unchanged, which is the cache's own contract.
    ///
    /// A blur radius that differs keeps nothing: every pixel is a different
    /// average. Otherwise the geometry decides, and only two things about it
    /// matter, because the backdrop the blur reads has not changed:
    ///
    /// - the blur **replicates** at its rectangle's edges, so a pixel less
    ///   than `radius_px` inside either position's on-screen rectangle averaged
    ///   a different set of samples;
    /// - the shape **weights** the mix at a window-local coordinate, so a pixel
    ///   within a corner's reach of either position's own rectangle was mixed
    ///   at a different coverage.
    ///
    /// Both are confined to a border, so what survives is the shared rectangle
    /// taken in by the larger of the two reaches — and the coverage argument
    /// holds for a resize or a corner change as much as a move, which is why
    /// none of them is a special case here. A pixel that deep inside is
    /// bit-for-bit what a fresh blur would write.
    pub(crate) fn reuse(
        &self,
        bounds: Rect,
        screen: Rect,
        radius_px: u32,
        shape: Option<WindowShape>,
    ) -> FrostPlan {
        if self.radius_px != radius_px {
            return FrostPlan::Blur;
        }
        if self.bounds == bounds && self.shape == shape {
            return FrostPlan::Whole;
        }
        let shared = self.rect.intersection(&bounds.intersection(&screen));
        let reach = radius_px
            .max(corner_reach(self.shape))
            .max(corner_reach(shape));
        let core = inset(shared, reach);
        if core.is_empty() {
            return FrostPlan::Blur;
        }
        FrostPlan::Core(core)
    }

    /// Write the part of this frost that lies inside `area` back into `back`,
    /// replacing whatever is there.
    ///
    /// This is what the blur would have written, so it is a plain copy and not
    /// a blend. Every pixel of `area` intersected with this frost's rectangle
    /// is written, which is what lets the caller skip composing the layers
    /// below there at all: the back buffer covers the whole screen and carries
    /// no clip while a frame is composed, so the row span cannot be refused.
    /// The guard is kept so the copy stays a total function rather than to
    /// leave a row for the layers below to show through.
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
