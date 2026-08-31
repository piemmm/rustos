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
//! describes the frame that composite pass produced and nothing else. The
//! same accumulator folds each finished frame into a since-epoch
//! [`DesktopFrameTotals`], which is what a reader outside the desktop asks
//! for: one frame's counts are a live gauge, and the worst frame of a
//! gesture is what says whether a hover repainted a control or the screen.
//!
//! [`Compositor::composite`]: crate::Compositor::composite

use tairix_abi::sysinfo::DesktopFrameTotals;

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
    /// the screen and after any blurred window whose frost had to be
    /// recomputed widened the damage it touched. This is the size of the
    /// frame's job.
    pub damaged_px: u64,
    /// Layer contributions blended through the *over* operator. The
    /// per-pixel cost the desktop actually pays.
    pub blended_px: u64,
    /// Screen pixels resolved by copying a fully opaque run of the front
    /// window's own pixels. Each cost no blend at all, and everything beneath
    /// it — the windows below, the desktop layer, the root fill — was skipped,
    /// which is why `blended_px` falls as this rises.
    pub opaque_px: u64,
    /// Pixels rewritten by a *recomputed* backdrop frost. A frost served from
    /// the retained one is copied rather than blurred and counts nothing here,
    /// so this is exactly the blur work the frame could not avoid.
    pub blur_px: u64,
    /// Composed pixels converted to scan-out bytes.
    pub encoded_px: u64,
    /// Dirty rectangles the frame redrew, whether by recomposing them or by
    /// encoding them afresh from the back buffer (a screen fade).
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

    /// `true` when the frame changed nothing at all.
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
    /// Screen pixels the open frame is being composed against, so folding it
    /// attributes it to the screen it was drawn on rather than to whatever
    /// mode is current when the fold happens.
    frame_screen_px: u64,
    /// Whether a frame is open. Distinguishes "no frame has ever been
    /// composed" from "one frame has, and it counted nothing", which
    /// otherwise look identical.
    open: bool,
    /// Frames already folded. The open frame is added on read, never stored,
    /// so reading twice cannot count it twice.
    folded: DesktopFrameTotals,
}

impl FrameCounters {
    /// A counter set that has seen no frame.
    pub(crate) const fn new() -> Self {
        Self {
            frame: FrameStats::ZERO,
            frame_screen_px: 0,
            open: false,
            folded: DesktopFrameTotals::ZERO,
        }
    }

    /// Start accumulating a new frame against a screen of `screen_px`,
    /// folding the previous one into the epoch totals and discarding its
    /// counts. Called once per composite pass, at its start, so the counts a
    /// reader snapshots after presenting cover exactly that frame — the
    /// composite work and the presents that published it.
    pub(crate) fn begin_frame(&mut self, screen_px: u64) {
        if self.open {
            fold_frame(&mut self.folded, &self.frame, self.frame_screen_px);
        }
        self.frame = FrameStats::ZERO;
        self.frame_screen_px = screen_px;
        self.open = true;
    }

    /// The frame accumulated so far.
    pub(crate) const fn snapshot(&self) -> FrameStats {
        self.frame
    }

    /// Every frame this epoch has composed, including the one still open.
    ///
    /// The open frame is folded into a copy rather than into the stored
    /// totals, so this is a pure read: a caller may take it once per frame or
    /// a hundred times and the counts are the same.
    pub(crate) fn totals(&self) -> DesktopFrameTotals {
        let mut totals = self.folded;
        if self.open {
            fold_frame(&mut totals, &self.frame, self.frame_screen_px);
        }
        totals
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

/// Add one finished frame, composed against a screen of `screen_px`, to
/// `totals`.
///
/// A frame composed against a different screen starts a fresh epoch: the
/// counts are read against `screen_px` as their denominator, so mixing two
/// screens would make both the ratio and the per-frame damage bound
/// meaningless. Every addition saturates, and each peak is the maximum of
/// its own field, because the frame that blends most need not be the frame
/// that damages most.
fn fold_frame(totals: &mut DesktopFrameTotals, frame: &FrameStats, screen_px: u64) {
    if totals.screen_px != screen_px {
        *totals = DesktopFrameTotals {
            screen_px,
            ..DesktopFrameTotals::ZERO
        };
    }
    totals.frames = totals.frames.saturating_add(1);
    totals.damaged_px = totals.damaged_px.saturating_add(frame.damaged_px);
    totals.blended_px = totals.blended_px.saturating_add(frame.blended_px);
    totals.opaque_px = totals.opaque_px.saturating_add(frame.opaque_px);
    totals.blur_px = totals.blur_px.saturating_add(frame.blur_px);
    totals.encoded_px = totals.encoded_px.saturating_add(frame.encoded_px);
    totals.dirty_rects = totals
        .dirty_rects
        .saturating_add(u64::from(frame.dirty_rects));
    totals.present_calls = totals
        .present_calls
        .saturating_add(u64::from(frame.present_calls));
    totals.chrome_hits = totals
        .chrome_hits
        .saturating_add(u64::from(frame.chrome_hits));
    totals.chrome_misses = totals
        .chrome_misses
        .saturating_add(u64::from(frame.chrome_misses));
    totals.peak_damaged_px = totals.peak_damaged_px.max(frame.damaged_px);
    totals.peak_blended_px = totals.peak_blended_px.max(frame.blended_px);
}
