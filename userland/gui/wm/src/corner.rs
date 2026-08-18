//! Per-window rounded-corner selection.
//!
//! A window (and the taskbar, through this same path) may request rounded
//! corners of a given radius. Composition multiplies each source pixel's
//! alpha by a *coverage* value in `0..=255`: `255` deep inside the rounded
//! rectangle, `0` outside it, and a smooth ramp across the corner arc.
//!
//! The coverage arithmetic itself is **not** defined here: it is the shared
//! `tairix_raster::round_rect_coverage`, the single rounded-rectangle
//! definition the compositor and the Reactive Alloy control plates both round
//! through, so window corners and control corners can never diverge. This type
//! only carries the per-window Square/Rounded *choice* the theme drives.

use tairix_raster::{round_rect_coverage, round_rect_radius};

/// Per-window corner-rounding selection.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum Corners {
    /// Square corners; coverage is always fully opaque (the opt-out).
    Square,
    /// Rounded corners with the given radius in pixels. A radius is
    /// clamped to half the shorter side at evaluation time.
    Rounded {
        /// Corner radius in pixels.
        radius: u32,
    },
}

impl Corners {
    /// The corner style for a theme corner radius.
    ///
    /// A radius of `0` is the square opt-out; any other radius rounds.
    /// This lets a window or the taskbar take its corner radius straight
    /// from the active theme's [`Metrics`](tairix_theme::Metrics) without
    /// the caller re-deciding what "no rounding" means.
    #[must_use]
    pub const fn from_radius(radius: u32) -> Self {
        if radius == 0 {
            Self::Square
        } else {
            Self::Rounded { radius }
        }
    }

    /// The radius this style rounds a `width`×`height` surface by: the
    /// requested radius clamped to what that surface can carry, `0` where it
    /// rounds nothing.
    #[must_use]
    pub(crate) fn radius(self, width: u32, height: u32) -> u32 {
        match self {
            Self::Square => 0,
            Self::Rounded { radius } => round_rect_radius(width, height, radius),
        }
    }

    /// Whether row `y` of a `width`×`height` surface carries an arc at all:
    /// `false` where the style covers every column of it fully.
    ///
    /// Partial coverage lives only in the corner bands, so a caller that
    /// decides once per row whether the corners can reach it — the compositor
    /// resolving a window row — skips nothing by trusting this.
    #[must_use]
    pub(crate) fn clips_row(self, y: u32, width: u32, height: u32) -> bool {
        let radius = self.radius(width, height);
        radius > 0 && (y < radius || y >= height.saturating_sub(radius))
    }

    /// Coverage in `0..=255` for pixel `(x, y)` of a `width`×`height`
    /// surface, from the shared rounded-rectangle definition.
    ///
    /// A pixel wholly outside the rounded region returns `0`, one wholly
    /// inside returns `255`, and a pixel straddling a corner arc returns the
    /// fraction of its area that is inside. A [`Square`](Self::Square)
    /// selection is fully opaque everywhere.
    #[must_use]
    pub fn coverage(self, x: u32, y: u32, width: u32, height: u32) -> u8 {
        match self {
            Self::Square => 255,
            Self::Rounded { radius } => round_rect_coverage(x, y, width, height, radius),
        }
    }
}
