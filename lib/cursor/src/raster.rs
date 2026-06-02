//! Rasterising a [`VectorCursor`] onto a `lib/raster` [`Surface`] at any
//! scale.
//!
//! Scaling is what makes the vector representation worthwhile: a cursor
//! authored once on its design grid is rendered at whatever pixel size a
//! display's DPI calls for. The fill is **anti-aliased** by supersampling —
//! each output pixel is probed on a fixed sub-pixel grid and the fraction of
//! samples inside a shape becomes that shape's coverage — and every covered
//! pixel is composited through `lib/raster`'s single premultiplied-alpha
//! [`Pixel::over`] path, so the cursor's own colour arithmetic is never
//! duplicated (`AGENTS.md` §2.2). Out-of-range scales and degenerate cursors
//! fail closed with `None` rather than panicking (`AGENTS.md` §2.9).
//!
//! [`Pixel::over`]: rustos_raster::Pixel::over

use alloc::vec::Vec;

use rustos_geometry::Point;
use rustos_raster::Surface;

use crate::vector::{Shape, VectorCursor};

/// The number of sub-pixel samples per axis used for anti-aliasing. A 4×4
/// grid gives 17 distinct coverage levels per pixel, enough for smooth
/// cursor edges without the cost of a larger kernel.
const SUPERSAMPLE: u32 = 4;

/// A rasterised cursor: an opaque-where-drawn pixel image plus the hotspot
/// expressed in that image's own pixel coordinates.
///
/// The window manager blits [`surface`](Self::surface) so that
/// [`hotspot`](Self::hotspot) lands on the pointer position; the surface is
/// transparent everywhere the cursor does not draw, so it composites over
/// the desktop correctly.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CursorImage {
    surface: Surface,
    hotspot: Point,
}

impl CursorImage {
    /// The rendered pixels, transparent outside the cursor artwork.
    #[must_use]
    pub fn surface(&self) -> &Surface {
        &self.surface
    }

    /// The hotspot in this image's pixel coordinates.
    #[must_use]
    pub const fn hotspot(&self) -> Point {
        self.hotspot
    }

    /// The image width in pixels.
    #[must_use]
    pub const fn width(&self) -> u32 {
        self.surface.width()
    }

    /// The image height in pixels.
    #[must_use]
    pub const fn height(&self) -> u32 {
        self.surface.height()
    }
}

impl VectorCursor {
    /// The square pixel side this cursor rasterises to at `scale_percent`,
    /// or `None` if the cursor or the scale is degenerate.
    ///
    /// `scale_percent` is relative to the design grid: `100` renders one
    /// pixel per design unit, `200` doubles it, `50` halves it. A side of
    /// zero (an empty design grid or a scale that rounds the artwork away to
    /// nothing) is not renderable and reported as `None`.
    #[must_use]
    pub fn footprint(&self, scale_percent: u32) -> Option<u32> {
        let side = u64::from(self.design_size()).checked_mul(u64::from(scale_percent))? / 100;
        let side = u32::try_from(side).ok()?;
        (side > 0).then_some(side)
    }

    /// Rasterise this cursor at `scale_percent` (see [`footprint`]).
    ///
    /// Returns `None` for a degenerate cursor or scale, or if the resulting
    /// pixel buffer cannot be allocated — the caller falls back to a smaller
    /// scale or a different cursor rather than crashing (`AGENTS.md` §2.9).
    ///
    /// [`footprint`]: Self::footprint
    #[must_use]
    pub fn rasterise(&self, scale_percent: u32) -> Option<CursorImage> {
        let side = self.footprint(scale_percent)?;
        let mut surface = Surface::new(side, side)?;
        let denom = sample_denominator(side)?;

        for shape in self.shapes() {
            self.fill_shape(&mut surface, shape, side, denom);
        }

        let hotspot = self.scaled_hotspot(scale_percent, side);
        Some(CursorImage { surface, hotspot })
    }

    /// Composite one shape's anti-aliased coverage onto `surface`.
    fn fill_shape(&self, surface: &mut Surface, shape: &Shape, side: u32, denom: i64) {
        if shape.polygon.len() < 3 {
            return;
        }
        let scaled = self.scale_polygon(shape, denom);
        let source = shape.fill.premultiply();
        let samples = SUPERSAMPLE * SUPERSAMPLE;

        for py in 0..side {
            for px in 0..side {
                let coverage = coverage_at(&scaled, px, py);
                if coverage == 0 {
                    continue;
                }
                let factor = coverage_to_alpha(coverage, samples);
                let src = source.scale_alpha(factor);
                if let Some(dst) = surface.get(px, py) {
                    surface.set(px, py, src.over(dst));
                }
            }
        }
    }

    /// The shape polygon mapped into the fixed-point sample space, where the
    /// whole `design_size` grid spans `denom` sub-units. Mapping each vertex
    /// as `v * denom / design` keeps full precision rather than pre-rounding
    /// a per-unit step.
    fn scale_polygon(&self, shape: &Shape, denom: i64) -> Vec<(i64, i64)> {
        let design = i64::from(self.design_size().max(1));
        let map = |c: i32| i64::from(c) * denom / design;
        shape.polygon.iter().map(|v| (map(v.x), map(v.y))).collect()
    }

    /// The hotspot scaled into output pixels and clamped to the image.
    fn scaled_hotspot(&self, scale_percent: u32, side: u32) -> Point {
        let scale = |design: i32| -> i32 {
            let value = i64::from(design) * i64::from(scale_percent) / 100;
            let max = i64::from(side.saturating_sub(1));
            let clamped = value.clamp(0, max);
            i32::try_from(clamped).unwrap_or(0)
        };
        Point::new(scale(self.hotspot_x()), scale(self.hotspot_y()))
    }
}

/// The number of sub-samples of pixel `(px, py)` that fall inside `polygon`.
fn coverage_at(polygon: &[(i64, i64)], px: u32, py: u32) -> u32 {
    let mut hits = 0;
    for sy in 0..SUPERSAMPLE {
        let sample_y = sample_coordinate(py, sy);
        for sx in 0..SUPERSAMPLE {
            let sample_x = sample_coordinate(px, sx);
            if point_in_polygon(polygon, sample_x, sample_y) {
                hits += 1;
            }
        }
    }
    hits
}

/// The shared denominator mapping the `design_size` grid onto the sample
/// sub-unit space: one output pixel spans `2 * SUPERSAMPLE` sub-units, so
/// the whole `side`-pixel image spans `side * 2 * SUPERSAMPLE` sub-units.
fn sample_denominator(side: u32) -> Option<i64> {
    let denom = u64::from(side)
        .checked_mul(2)?
        .checked_mul(u64::from(SUPERSAMPLE))?;
    i64::try_from(denom).ok()
}

/// The fixed-point coordinate of sub-sample `sub` within output pixel
/// `pixel`, in the same sample sub-units as a scaled polygon.
fn sample_coordinate(pixel: u32, sub: u32) -> i64 {
    // pixel spans [pixel*2*SS, (pixel+1)*2*SS); the `sub`-th sample centre is
    // at pixel*2*SS + 2*sub + 1.
    let base = i64::from(pixel) * 2 * i64::from(SUPERSAMPLE);
    base + 2 * i64::from(sub) + 1
}

/// Even-odd point-in-polygon test in integer sample space.
///
/// A horizontal ray is cast in `+x`; each edge that straddles `py` flips the
/// inside flag when its crossing lies to the right of `px`. The comparison
/// is cross-multiplied (with the edge's vertical direction accounted for) so
/// no division is needed and the result stays exact (`AGENTS.md` §2.9).
fn point_in_polygon(polygon: &[(i64, i64)], px: i64, py: i64) -> bool {
    let mut inside = false;
    let n = polygon.len();
    let mut j = n - 1;
    for i in 0..n {
        let (xi, yi) = polygon[i];
        let (xj, yj) = polygon[j];
        if (yi > py) != (yj > py) {
            let lhs = (px - xi) * (yj - yi);
            let rhs = (xj - xi) * (py - yi);
            if yj - yi > 0 {
                if lhs < rhs {
                    inside = !inside;
                }
            } else if lhs > rhs {
                inside = !inside;
            }
        }
        j = i;
    }
    inside
}

/// Map a sample hit count to an alpha factor in `0..=255`.
fn coverage_to_alpha(hits: u32, samples: u32) -> u8 {
    if samples == 0 {
        return 0;
    }
    let scaled = u32::from(u8::MAX) * hits / samples;
    u8::try_from(scaled.min(u32::from(u8::MAX))).unwrap_or(u8::MAX)
}
