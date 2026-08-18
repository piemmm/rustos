//! Anti-aliased rounded-rectangle coverage — the single definition.
//!
//! A rounded rectangle appears in two places on the desktop: the compositor
//! rounds a whole window (or the taskbar) surface's corners, and a Reactive
//! Alloy control plate is a small rounded rectangle filled with a colour.
//! Both must round *identically*, so the coverage arithmetic lives here once
//! and both consume it — the window manager's corner mask through
//! [`round_rect_coverage`], and a control plate through
//! [`Surface::fill_round_rect`](crate::Surface::fill_round_rect).
//!
//! Coverage is computed by **supersampling** on a fixed
//! `SUBSAMPLES`×`SUBSAMPLES` grid and counting the sub-pixels inside the
//! rounded rectangle's signed-distance region. This needs no `sqrt`
//! (unavailable in `core`) and is fully deterministic, so the anti-aliasing
//! is exactly reproducible in tests.

/// Sub-samples per axis. `SUBSAMPLES * SUBSAMPLES` coverage levels.
const SUBSAMPLES: u32 = 4;

/// The radius a `width`×`height` rounded rectangle is actually rounded by:
/// `radius` clamped to half its shorter side, so an over-large radius yields
/// a stadium/circle rather than out-of-bounds geometry (fail closed). `0`
/// rounds nothing.
///
/// [`round_rect_coverage`] rounds by exactly this, and it is published so a
/// caller reasoning about *where* a shape's corners are — which rows carry an
/// arc at all, how far one reaches in — reads the same clamp the coverage
/// applies rather than restating it.
#[must_use]
pub fn round_rect_radius(width: u32, height: u32, radius: u32) -> u32 {
    radius.min(width / 2).min(height / 2)
}

/// Coverage in `0..=255` for pixel `(x, y)` of a `width`×`height` rounded
/// rectangle with corner `radius`.
///
/// A pixel wholly outside the rounded region returns `0`, one wholly inside
/// returns `255`, and a pixel straddling a corner arc returns the fraction of
/// its area that is inside, quantised to the supersample grid. A `radius` of
/// `0` (or a degenerate zero-size rectangle) is square, so every in-bounds
/// pixel is fully covered. The `radius` is [clamped](round_rect_radius) to
/// half the shorter side.
#[must_use]
pub fn round_rect_coverage(x: u32, y: u32, width: u32, height: u32, radius: u32) -> u8 {
    let radius = round_rect_radius(width, height, radius);
    if radius == 0 {
        return 255;
    }
    // Only the four `radius`×`radius` corner squares can be less than fully
    // opaque; every other pixel is deep inside the rounded rectangle. A pixel
    // whose column is clear of both corner columns (`radius <= x < width -
    // radius`), or whose row is clear of both corner rows, has a zero
    // distance on that axis for every sub-sample, so its squared distance
    // never exceeds `radius²` and it is fully covered. Returning 255 for those
    // directly is bit-for-bit identical to supersampling them and keeps the
    // cost proportional to the corner area, not the whole surface. `radius` is
    // clamped to half the shorter side above, so `width - radius >= radius`
    // and the subtraction cannot wrap.
    let in_corner_column = x < radius || x >= width - radius;
    let in_corner_row = y < radius || y >= height - radius;
    if !(in_corner_column && in_corner_row) {
        return 255;
    }
    coverage_supersampled(x, y, width, height, radius)
}

/// Count the in-region sub-samples of pixel `(x, y)` and scale to `0..=255`.
///
/// All distances are kept in fixed-point units of `1 / (2 * SUBSAMPLES)` of a
/// pixel so the sub-sample centres land on integers: the centre of sub-sample
/// `s` within pixel `p` is `p + (2 * s + 1) / (2 * SUBSAMPLES)`, i.e.
/// `p * 2 * SUBSAMPLES + 2 * s + 1` scaled units.
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

/// Distance of `pos` outside the inset interval `[low, high]`, or `0` when
/// inside it. The per-axis term of the rounded-rectangle signed-distance test.
fn axis_distance(pos: u64, low: u64, high: u64) -> u64 {
    // The inset interval is non-empty (the radius is clamped to half the
    // side), so at most one term is non-zero.
    low.saturating_sub(pos) + pos.saturating_sub(high)
}

#[cfg(test)]
mod tests {
    use super::{coverage_supersampled, round_rect_coverage, round_rect_radius};

    #[test]
    fn zero_radius_is_square_everywhere() {
        for &(w, h) in &[(1, 1), (10, 10), (17, 9)] {
            for y in 0..h {
                for x in 0..w {
                    assert_eq!(round_rect_coverage(x, y, w, h, 0), 255);
                }
            }
        }
    }

    #[test]
    fn corner_is_clear_and_centre_is_solid() {
        // The extreme corner pixel of a rounded rect is outside the arc.
        assert_eq!(round_rect_coverage(0, 0, 20, 20, 8), 0);
        // The centre is fully inside.
        assert_eq!(round_rect_coverage(10, 10, 20, 20, 8), 255);
        // Straight edges (clear of both corner bands) are fully covered.
        assert_eq!(round_rect_coverage(10, 0, 20, 20, 8), 255);
        assert_eq!(round_rect_coverage(0, 10, 20, 20, 8), 255);
    }

    #[test]
    fn corner_arc_is_partially_covered() {
        let cov = round_rect_coverage(2, 2, 20, 20, 8);
        assert!(
            cov > 0 && cov < 255,
            "corner arc should be anti-aliased: {cov}"
        );
    }

    #[test]
    fn over_large_radius_clamps_without_panicking() {
        // A radius larger than half the shorter side is clamped, not
        // out-of-bounds: the middle of a long thin rect stays solid.
        assert_eq!(round_rect_coverage(10, 5, 20, 10, 200), 255);
    }

    /// The published radius is the one the coverage actually rounds by, so a
    /// caller reasoning about where the arcs are and the coverage itself can
    /// never disagree.
    #[test]
    fn published_radius_is_the_radius_coverage_rounds_by() {
        for &(width, height) in &[(1, 1), (2, 2), (20, 20), (20, 10), (17, 9)] {
            for radius in [0u32, 1, 5, 8, 200] {
                let effective = round_rect_radius(width, height, radius);
                assert!(
                    effective * 2 <= width.min(height),
                    "{effective} rounds past half of {width}x{height}"
                );
                for y in 0..height {
                    for x in 0..width {
                        assert_eq!(
                            round_rect_coverage(x, y, width, height, radius),
                            round_rect_coverage(x, y, width, height, effective),
                            "({x},{y}) on {width}x{height} r={radius} vs r={effective}"
                        );
                    }
                }
            }
        }
    }

    /// Partial coverage lives only in the corner bands: every column of a row
    /// clear of both of them is fully covered. A caller that decides per row
    /// whether an arc can reach it — the compositor cutting a window's client
    /// to its frame's plate — therefore skips nothing by skipping such a row.
    #[test]
    fn a_row_clear_of_the_corner_bands_is_fully_covered() {
        for &(width, height) in &[(20, 20), (40, 30), (17, 9)] {
            for radius in [1u32, 5, 8, 200] {
                let r = round_rect_radius(width, height, radius);
                for y in r..height - r {
                    for x in 0..width {
                        assert_eq!(
                            round_rect_coverage(x, y, width, height, radius),
                            255,
                            "({x},{y}) on {width}x{height} r={radius} is not solid"
                        );
                    }
                }
            }
        }
    }

    /// The interior fast path in [`round_rect_coverage`] is a pure
    /// optimisation: it must produce the exact byte the full supersampled scan
    /// would for every pixel, so only the cost changes, never a pixel.
    #[test]
    fn interior_fast_path_is_identical_to_full_supersampling() {
        for &(width, height) in &[(1, 1), (2, 2), (17, 9), (40, 30), (256, 192)] {
            for radius in [1u32, 2, 5, 8, 16, 64, 200] {
                let clamped = round_rect_radius(width, height, radius);
                for y in 0..height {
                    for x in 0..width {
                        let got = round_rect_coverage(x, y, width, height, radius);
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
