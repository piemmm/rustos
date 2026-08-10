//! What one composite pass actually did, counted exactly.
//!
//! "The desktop feels slow" is not a defect report. [`FrameStats`] turns it
//! into one: a pointer sample that changes 3 200 pixels but blends 4.2 M of
//! them is a measurement anyone can act on, and a later change that stops
//! blending them is provable rather than claimed.
//!
//! Every field is a **count of work**, never a duration. Counts are exactly
//! reproducible for a given scene, so a test may assert them and stay green
//! under any machine load; a wall-clock figure is neither. The compositor is
//! also `no_std` and holds no clock — the embedder that drives the frame owns
//! time and pairs its own measurement with the counts it reads here.
//!
//! The accumulator is reset by [`Compositor::composite`], so a snapshot
//! describes the frame that composite pass produced and nothing else.
//!
//! [`Compositor::composite`]: crate::Compositor::composite

/// The work one frame cost, in pixels, rectangles, and cache decisions.
///
/// `damaged_px` is the denominator: the pixels the frame was asked to change.
/// `blended_px` counts *layer contributions*, not screen positions — a pixel
/// two windows both draw at is one damaged pixel and two blends — so it may
/// legitimately exceed the damage, and that ratio is exactly what says whether
/// a frame is paying for depth nobody can see.
///
/// Counters saturate rather than wrap: a frame that somehow overflowed a `u64`
/// of pixels would be a diagnostic, and a wrapped one reads as a suspiciously
/// small frame.
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
pub struct FrameStats {
    /// Screen pixels inside the frame's dirty rectangles, after clipping to
    /// the screen and after a blurred window widened the damage it touched.
    /// This is the size of the frame's job.
    pub damaged_px: u64,
    /// Layer contributions blended through the *over* operator. The
    /// per-pixel cost the desktop actually pays.
    pub blended_px: u64,
    /// Screen pixels resolved by copying a fully opaque run of the front
    /// window's own pixels. Each cost no blend at all, and everything beneath
    /// it — the windows below, the desktop layer, the root fill — was skipped,
    /// which is why `blended_px` falls as this rises.
    pub opaque_px: u64,
    /// Pixels rewritten by a backdrop frost. A frame that re-frosts a window
    /// whose backdrop did not change is paying twice for one appearance.
    pub blur_px: u64,
    /// Composed pixels converted to scan-out bytes.
    pub encoded_px: u64,
    /// Dirty rectangles the frame recomposed.
    pub dirty_rects: u32,
    /// Calls the frame made into the display driver to publish itself.
    pub present_calls: u32,
    /// Window-furniture lookups served from the retained cache.
    pub chrome_hits: u32,
    /// Window-furniture lookups that had to be rendered, whether the cache
    /// then retained them or refused.
    pub chrome_misses: u32,
}

impl FrameStats {
    /// A frame that did nothing.
    pub const ZERO: Self = Self {
        damaged_px: 0,
        blended_px: 0,
        opaque_px: 0,
        blur_px: 0,
        encoded_px: 0,
        dirty_rects: 0,
        present_calls: 0,
        chrome_hits: 0,
        chrome_misses: 0,
    };

    /// `true` when the frame recomposed nothing at all.
    #[must_use]
    pub const fn is_idle(&self) -> bool {
        self.damaged_px == 0 && self.dirty_rects == 0
    }
}

/// The live accumulator [`Compositor`] adds to as a frame is composed.
///
/// Separate from [`FrameStats`] so the readable snapshot stays a plain,
/// copyable value with no mutating surface: a consumer cannot accidentally
/// advance the compositor's own counters. Every `add`/`bump` saturates.
///
/// [`Compositor`]: crate::Compositor
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct FrameCounters {
    frame: FrameStats,
}

impl FrameCounters {
    /// A counter set that has seen no frame.
    pub(crate) const fn new() -> Self {
        Self {
            frame: FrameStats::ZERO,
        }
    }

    /// Start accumulating a new frame, discarding the previous one's counts.
    /// Called once per composite pass, at its start, so the counts a reader
    /// snapshots after presenting cover exactly that frame — the composite
    /// work and the presents that published it.
    pub(crate) fn begin_frame(&mut self) {
        self.frame = FrameStats::ZERO;
    }

    /// The frame accumulated so far.
    pub(crate) const fn snapshot(&self) -> FrameStats {
        self.frame
    }

    pub(crate) fn add_damaged(&mut self, px: u64) {
        self.frame.damaged_px = self.frame.damaged_px.saturating_add(px);
        self.frame.dirty_rects = self.frame.dirty_rects.saturating_add(1);
    }

    pub(crate) fn add_blended(&mut self, px: u64) {
        self.frame.blended_px = self.frame.blended_px.saturating_add(px);
    }

    pub(crate) fn add_opaque(&mut self, px: u64) {
        self.frame.opaque_px = self.frame.opaque_px.saturating_add(px);
    }

    pub(crate) fn add_blur(&mut self, px: u64) {
        self.frame.blur_px = self.frame.blur_px.saturating_add(px);
    }

    pub(crate) fn add_encoded(&mut self, px: u64) {
        self.frame.encoded_px = self.frame.encoded_px.saturating_add(px);
    }

    pub(crate) fn bump_present(&mut self) {
        self.frame.present_calls = self.frame.present_calls.saturating_add(1);
    }

    pub(crate) fn add_chrome(&mut self, hits: u32, misses: u32) {
        self.frame.chrome_hits = self.frame.chrome_hits.saturating_add(hits);
        self.frame.chrome_misses = self.frame.chrome_misses.saturating_add(misses);
    }
}

/// The pixel count of a rectangle as a counter increment, saturating.
pub(crate) fn area_px(width: u32, height: u32) -> u64 {
    u64::from(width).saturating_mul(u64::from(height))
}
