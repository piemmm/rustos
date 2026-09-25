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

use crate::camera::Zoom;
use crate::error::ClientError;
use crate::quality::{RenderScale, CAPS};

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
///
/// The render target covers the same piece of the world as the window at
/// every scale: each of its pixels covers the world the window's would,
/// widened by the inverse of the fraction it is drawn at, so shedding
/// resolution never changes what the player sees, only how finely.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct Viewport {
    window_width: u32,
    window_height: u32,
    render_width: u32,
    render_height: u32,
    scale: RenderScale,
}

impl Viewport {
    /// The render target for a `window_width` × `window_height` window at
    /// `scale`.
    ///
    /// # Errors
    ///
    /// [`ClientError::Viewport`] for a window with no pixels, which is not
    /// a frame to be drawn smaller but a window there is nothing to draw
    /// in, and for a scale no zoom's step stays whole at.
    pub fn new(
        window_width: u32,
        window_height: u32,
        scale: RenderScale,
    ) -> Result<Self, ClientError> {
        if window_width == 0 || window_height == 0 {
            return Err(ClientError::Viewport);
        }
        let scale = cap(window_width, window_height).of(scale);
        // The nearest zoom's step is the smallest and every other is a power
        // of two times it, so a fraction that keeps it whole keeps them all.
        if scale.step(Zoom::NEAREST.sub_units_per_pixel()).is_none() {
            return Err(ClientError::Viewport);
        }
        Ok(Self {
            window_width,
            window_height,
            render_width: scale.apply(window_width),
            render_height: scale.apply(window_height),
            scale,
        })
    }

    /// The world sub-units one render pixel spans at `zoom`.
    ///
    /// Whole by construction: [`Self::new`] refuses a scale that is not.
    #[must_use]
    pub fn step(&self, zoom: Zoom) -> i32 {
        let base = zoom.sub_units_per_pixel();
        self.scale.step(base).unwrap_or(base)
    }

    /// The fraction of the window the render target is drawn at, the
    /// software path's cap and the ladder's own together.
    #[must_use]
    pub const fn scale(&self) -> RenderScale {
        self.scale
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

    /// How many rows each of [`Self::band_count`]'s bands holds, the last
    /// short where they do not divide evenly: the size the target's rows are
    /// cut by.
    #[must_use]
    pub fn band_rows(&self, runner: &dyn JobRunner) -> u32 {
        let count = u32::try_from(self.band_count(runner)).unwrap_or(u32::MAX);
        self.render_height.div_ceil(count.max(1)).max(1)
    }
}

/// The fraction a window must be drawn at to fit the software path's cap:
/// the largest of [`CAPS`] that brings both axes inside it, which keeps the
/// window's proportions.
fn cap(width: u32, height: u32) -> RenderScale {
    CAPS.into_iter()
        .find(|cap| cap.apply(width) <= MAX_RENDER_WIDTH && cap.apply(height) <= MAX_RENDER_HEIGHT)
        .unwrap_or(CAPS[CAPS.len() - 1])
}

#[cfg(test)]
#[path = "view_tests.rs"]
mod tests;
