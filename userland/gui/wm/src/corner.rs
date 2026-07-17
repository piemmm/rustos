//! Anti-aliased rounded-corner coverage.
//!
//! A window (and the taskbar, through this same path) may request rounded corners of a given radius. Composition
//! multiplies each source pixel's alpha by a *coverage* value in
//! `0..=255` computed here: `255` deep inside the rounded rectangle, `0`
//! outside it, and a smooth ramp across the one-pixel-wide corner arc.
//!
//! Coverage is computed by **supersampling** on a fixed
//! `SUBSAMPLES`×`SUBSAMPLES` grid and counting the sub-pixels that
//! fall inside the rounded rectangle's signed-distance region. This
//! needs no `sqrt` (unavailable in `core`) and is fully deterministic,
//! so the anti-aliasing is exactly reproducible in tests.

/// Sub-samples per axis. `SUBSAMPLES * SUBSAMPLES` coverage levels.
const SUBSAMPLES: u32 = 4;

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

    /// Coverage in `0..=255` for pixel `(x, y)` of a `width`×`height`
    /// surface.
    ///
    /// A pixel wholly outside the rounded region returns `0`, one wholly
    /// inside returns `255`, and a pixel straddling a corner arc returns
    /// the fraction of its area that is inside, quantised to the
    /// supersample grid.
    #[must_use]
    pub fn coverage(self, x: u32, y: u32, width: u32, height: u32) -> u8 {
        let radius = match self {
            Self::Square => return 255,
            Self::Rounded { radius } => radius.min(width / 2).min(height / 2),
        };
        if radius == 0 {
            return 255;
        }
        // Only the four `radius`×`radius` corner squares can be less than
        // fully opaque; every other pixel is deep inside the rounded
        // rectangle. A pixel whose column is clear of both corner columns
        // (`radius <= x < width - radius`), or whose row is clear of both
        // corner rows, has a zero distance on that axis for every
        // sub-sample, so its squared distance never exceeds `radius²` and
        // it is fully covered. Returning 255 for those pixels directly is
        // bit-for-bit identical to supersampling them, and keeps the
        // corner-rounding cost proportional to the corner area rather than
        // the whole surface — a large window rounds as cheaply as a small
        // one. `radius` is clamped to half the shorter side above, so
        // `width - radius >= radius` and the subtraction cannot wrap.
        let in_corner_column = x < radius || x >= width - radius;
        let in_corner_row = y < radius || y >= height - radius;
        if !(in_corner_column && in_corner_row) {
            return 255;
        }
        coverage_supersampled(x, y, width, height, radius)
    }
}

/// Count the in-region sub-samples of pixel `(x, y)` and scale to
/// `0..=255`.
///
/// All distances are kept in fixed-point units of `1 / (2 * SUBSAMPLES)`
/// of a pixel so the sub-sample centres land on integers: the centre of
/// sub-sample `s` within pixel `p` is `p + (2 * s + 1) / (2 *
/// SUBSAMPLES)`, i.e. `p * 2 * SUBSAMPLES + 2 * s + 1` scaled units.
fn coverage_supersampled(x: u32, y: u32, width: u32, height: u32, radius: u32) -> u8 {
    let scale = 2 * SUBSAMPLES;
    let r = u64::from(radius) * u64::from(scale);
    let r_sq = r * r;
    // Inset rectangle edges (the centres of the four corner circles).
    let inset_left = r;
    let inset_top = r;
    let inset_right = u64::from(width) * u64::from(scale) - r;
    let inset_bottom = u64::from(height) * u64::from(scale) - r;

    let mut inside = 0u32;
    for sy in 0..SUBSAMPLES {
        let py = u64::from(y) * u64::from(scale) + u64::from(2 * sy + 1);
        let dy = axis_distance(py, inset_top, inset_bottom);
        for sx in 0..SUBSAMPLES {
            let px = u64::from(x) * u64::from(scale) + u64::from(2 * sx + 1);
            let dx = axis_distance(px, inset_left, inset_right);
            if dx * dx + dy * dy <= r_sq {
                inside += 1;
            }
        }
    }
    let total = SUBSAMPLES * SUBSAMPLES;
    let scaled = (inside * 255 + total / 2) / total;
    u8::try_from(scaled.min(255)).unwrap_or(u8::MAX)
}

/// Distance of `pos` outside the inset interval `[low, high]`, or `0`
/// when inside it. This is the per-axis term of the rounded-rectangle
/// signed-distance test.
fn axis_distance(pos: u64, low: u64, high: u64) -> u64 {
    // The inset interval is non-empty (the radius is clamped to half
    // the side), so at most one term is non-zero.
    low.saturating_sub(pos) + pos.saturating_sub(high)
}

#[cfg(test)]
mod interior_fast_path_tests {
    use super::{coverage_supersampled, Corners};

    /// The interior fast path in [`Corners::coverage`] is a pure
    /// optimisation: it must produce the *exact* byte the full
    /// supersampled scan would for every pixel of a surface, so the only
    /// thing that changed is the cost, never a pixel. This locks that
    /// invariant across sizes (including a large surface where the fast
    /// path skips almost every pixel) and radii (including the
    /// clamped-to-half degenerate case), so a future edit that breaks the
    /// equivalence fails here rather than silently altering rounding.
    #[test]
    fn interior_fast_path_is_identical_to_full_supersampling() {
        for &(width, height) in &[(1, 1), (2, 2), (17, 9), (40, 30), (256, 192)] {
            for radius in [1u32, 2, 5, 8, 16, 64, 200] {
                let corners = Corners::Rounded { radius };
                let clamped = radius.min(width / 2).min(height / 2);
                for y in 0..height {
                    for x in 0..width {
                        let got = corners.coverage(x, y, width, height);
                        let want = if clamped == 0 {
                            255
                        } else {
                            coverage_supersampled(x, y, width, height, clamped)
                        };
                        assert_eq!(
                            got, want,
                            "coverage mismatch at ({x},{y}) on {width}x{height} r={radius}"
                        );
                    }
                }
            }
        }
    }
}
