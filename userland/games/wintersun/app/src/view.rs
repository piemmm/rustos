//! The render target: how big it is, and how it is cut up for the other
//! cores.
//!
//! Two things decide the target's size. A window larger than the software
//! path can hold at its refresh is rendered at a cap and upscaled, and the
//! degradation ladder's last rung shrinks it further. Both are fractions
//! of the window rather than absolute sizes, so a change to either cannot
//! produce a target of a shape the window is not.
//!
//! # Why a band and not a square tile
//!
//! Every pass in the frame steps *horizontally*: the terrain splat walks a
//! span of pixels inside one cell row, the light composite walks a row of
//! the buffer, the resample reads a source row. A vertical cut would
//! divide the unit each of those is built around, so the piece handed to
//! another core is a full-width run of rows. That is a tile whose width
//! happens to be the target's — the bucketing a later pass does per piece
//! is unaffected — and it keeps every span whole.

use tairix_parallel::{bands, JobRunner};

use crate::error::ClientError;
use crate::quality::RenderScale;

/// The widest the software path renders before it upscales.
///
/// Not a capacity: a larger window is drawn, at this resolution and
/// upscaled. It is the point past which a first-party software renderer
/// stops being able to fill a frame in time on the reference machine, and
/// it is proportional-preserving, so a wide window is not squashed.
pub const MAX_RENDER_WIDTH: u32 = 2560;

/// The tallest the software path renders before it upscales.
pub const MAX_RENDER_HEIGHT: u32 = 1440;

/// The fewest pixels worth handing to another core.
///
/// A work grain, not a capacity: below roughly this much the dispatch
/// costs more than the pass it is splitting, so the band count collapses
/// to one and the frame runs on the calling thread with no atomics at all.
const MIN_BAND_PIXELS: usize = 1 << 14;

/// Where a frame is drawn, and at what size.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct Viewport {
    window_width: u32,
    window_height: u32,
    render_width: u32,
    render_height: u32,
}

impl Viewport {
    /// The render target for a `window_width` × `window_height` window at
    /// `scale`.
    ///
    /// # Errors
    ///
    /// [`ClientError::Viewport`] for a window with no pixels, which is not
    /// a frame to be drawn smaller but a window there is nothing to draw
    /// in.
    pub fn new(
        window_width: u32,
        window_height: u32,
        scale: RenderScale,
    ) -> Result<Self, ClientError> {
        if window_width == 0 || window_height == 0 {
            return Err(ClientError::Viewport);
        }
        let (capped_w, capped_h) = cap(window_width, window_height);
        Ok(Self {
            window_width,
            window_height,
            render_width: scale.apply(capped_w),
            render_height: scale.apply(capped_h),
        })
    }

    /// The window's own pixel extent.
    #[must_use]
    pub const fn window(&self) -> (u32, u32) {
        (self.window_width, self.window_height)
    }

    /// The render target's pixel extent.
    #[must_use]
    pub const fn render(&self) -> (u32, u32) {
        (self.render_width, self.render_height)
    }

    /// How many pixels a frame writes.
    #[must_use]
    pub const fn render_pixels(&self) -> usize {
        (self.render_width as usize) * (self.render_height as usize)
    }

    /// Whether presenting needs a resample, or the target can be handed to
    /// the compositor as it stands.
    #[must_use]
    pub const fn needs_resample(&self) -> bool {
        self.render_width != self.window_width || self.render_height != self.window_height
    }

    /// How many bands `runner` should cut this target's rows into.
    ///
    /// `1` whenever the target is too small to be worth splitting or the
    /// runner is one thread wide, in which case the frame costs exactly
    /// what it would with no pool at all.
    #[must_use]
    pub fn band_count(&self, runner: &dyn JobRunner) -> usize {
        let rows = self.render_height as usize;
        let grain = (MIN_BAND_PIXELS / (self.render_width as usize).max(1)).max(1);
        bands(runner, rows, grain).max(1)
    }

    /// The rows each of `count` bands covers, longest first.
    ///
    /// The remainder is spread one row at a time over the leading bands
    /// rather than piled onto the last, so no band is a whole extra row
    /// behind the others.
    #[must_use]
    pub fn band_rows(&self, count: usize) -> BandRows {
        BandRows {
            rows: self.render_height as usize,
            count: count.max(1),
        }
    }
}

/// How a target's rows divide between bands.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct BandRows {
    rows: usize,
    count: usize,
}

impl BandRows {
    /// The half-open row range band `index` covers, or `None` past the
    /// last band.
    #[must_use]
    pub fn range(&self, index: usize) -> Option<(usize, usize)> {
        if index >= self.count {
            return None;
        }
        let base = self.rows / self.count;
        let extra = self.rows % self.count;
        let start = base * index + index.min(extra);
        let len = base + usize::from(index < extra);
        Some((start, start + len))
    }

    /// How many bands there are.
    #[must_use]
    pub const fn count(&self) -> usize {
        self.count
    }
}

/// A window extent brought within the software path's cap, keeping its
/// proportions.
fn cap(width: u32, height: u32) -> (u32, u32) {
    if width <= MAX_RENDER_WIDTH && height <= MAX_RENDER_HEIGHT {
        return (width, height);
    }
    let (w, h) = (u64::from(width), u64::from(height));
    // Whichever axis is further over its cap decides the factor, so the
    // other lands inside its own.
    if w * u64::from(MAX_RENDER_HEIGHT) >= h * u64::from(MAX_RENDER_WIDTH) {
        let scaled = h * u64::from(MAX_RENDER_WIDTH) / w;
        (MAX_RENDER_WIDTH, u32::try_from(scaled).unwrap_or(1).max(1))
    } else {
        let scaled = w * u64::from(MAX_RENDER_HEIGHT) / h;
        (u32::try_from(scaled).unwrap_or(1).max(1), MAX_RENDER_HEIGHT)
    }
}

#[cfg(test)]
#[path = "view_tests.rs"]
mod tests;
