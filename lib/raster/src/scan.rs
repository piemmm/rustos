//! The one scan converter every filled shape goes through.
//!
//! A shape reaches here as a set of closed contours in integer coordinates,
//! and leaves as a per-pixel count of covered sub-samples: what the
//! [`Surface`](crate::surface::Surface) fill entry points turn into alpha and
//! composite. Solid artwork, grid-fitted device-space chrome, and a
//! multi-contour SVG path are the same problem, so there is one converter
//! rather than one per caller.
//!
//! Each pixel row is resolved by *scanning*, not by probing. For every one of
//! the [`SUPERSAMPLE`] sample rows in it, the x coordinate where each edge
//! crosses that row is computed once, the crossings are sorted, and walking
//! them under the [`FillRule`] yields the inside spans directly; the spans then
//! turn into per-pixel sample counts. The cost of a sample row is therefore the
//! edges plus the pixels — not the edges *times* the samples, which is what
//! probing every edge for every sub-sample costs and what makes a flattened
//! curve with thousands of edges unusably slow.
//!
//! Every coordinate is integer, so a crossing is exact and a shape rasterises
//! identically on every target. Vertices are clamped to a bound far outside any
//! allocatable surface on the way in, which keeps every product well inside
//! `i64` and makes the whole converter total for adversarial input.

use core::cmp::Ordering;

use alloc::vec::Vec;

use crate::surface::{SUBPIXEL, SUPERSAMPLE};

/// The furthest from the origin, in sample sub-units, a vertex may sit.
///
/// About 134 million sub-units — 16 million pixels — so no surface that can be
/// allocated comes close to it, and a shape placed inside one is unaffected.
/// Clamping to it bounds every product a crossing computes to roughly `2^63`,
/// so the arithmetic stays exact in `i64` for any `i32` input a caller (or an
/// attacker) supplies rather than overflowing.
const COORD_LIMIT: i64 = 1 << 30;

/// Which points enclosed by a set of contours count as inside.
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
pub enum FillRule {
    /// Inside where the signed number of times the contours wind around the
    /// point is not zero, so a hole must be wound against its enclosing
    /// contour. SVG's initial value.
    #[default]
    NonZero,
    /// Inside where the number of contour crossings is odd, so any nested
    /// contour is a hole whichever way it is wound.
    EvenOdd,
}

impl FillRule {
    /// The accumulator after crossing an edge running in `direction`.
    fn step(self, count: i64, direction: i32) -> i64 {
        match self {
            Self::NonZero => count + i64::from(direction),
            Self::EvenOdd => count + 1,
        }
    }

    /// Whether `count` accumulated crossings puts a point inside.
    fn inside(self, count: i64) -> bool {
        match self {
            Self::NonZero => count != 0,
            Self::EvenOdd => count % 2 != 0,
        }
    }
}

/// How a contour's own coordinates reach sample sub-units, and how a pixel
/// centre gets back to them.
///
/// The two directions are held separately on purpose: the forward map must be
/// exact integer arithmetic, because it decides which sub-samples a shape
/// covers, while the backward map only feeds a [`Paint`](crate::paint::Paint)
/// sampler that works in `f64`.
#[derive(Copy, Clone, Debug)]
pub(crate) struct SampleSpace {
    /// Sample sub-units per `denominator` contour units, horizontally.
    numerator_x: i64,
    /// Sample sub-units per `denominator` contour units, vertically.
    numerator_y: i64,
    /// The contour-space span the numerators are quoted over; never zero.
    denominator: i64,
    /// Contour units per pixel, horizontally then vertically.
    contour_per_pixel: (f64, f64),
}

impl SampleSpace {
    /// Artwork authored on a square `design`×`design` grid and stretched over
    /// the whole `width`×`height` surface.
    ///
    /// A `design` of zero would divide by zero, so it is read as `1`.
    pub(crate) fn design(design: u32, width: u32, height: u32) -> Self {
        let design = design.max(1);
        Self::new(
            i64::from(width) * pixel_span(),
            i64::from(height) * pixel_span(),
            i64::from(design),
            (
                f64::from(design) / f64::from(width.max(1)),
                f64::from(design) / f64::from(height.max(1)),
            ),
        )
    }

    /// Vertices already in device [`SUBPIXEL`] units, placed from the
    /// surface's own origin rather than stretched across it.
    pub(crate) fn device() -> Self {
        Self::new(
            pixel_span(),
            pixel_span(),
            i64::from(SUBPIXEL),
            (f64::from(SUBPIXEL), f64::from(SUBPIXEL)),
        )
    }

    /// The one place the divisor is forced positive, so no mapping below can
    /// divide by zero.
    fn new(
        numerator_x: i64,
        numerator_y: i64,
        denominator: i64,
        contour_per_pixel: (f64, f64),
    ) -> Self {
        Self {
            numerator_x,
            numerator_y,
            denominator: denominator.max(1),
            contour_per_pixel,
        }
    }

    /// `point` in sample sub-units.
    fn to_sample(self, point: (i32, i32)) -> (i64, i64) {
        (
            scale_axis(point.0, self.numerator_x, self.denominator),
            scale_axis(point.1, self.numerator_y, self.denominator),
        )
    }

    /// The contour-space coordinate of pixel `(x, y)`'s centre — where a
    /// gradient is sampled for that pixel.
    fn pixel_centre(self, x: u32, y: u32) -> (f64, f64) {
        (
            (f64::from(x) + 0.5) * self.contour_per_pixel.0,
            (f64::from(y) + 0.5) * self.contour_per_pixel.1,
        )
    }
}

/// One non-horizontal edge of a contour, in sample sub-units.
///
/// A horizontal edge crosses no sample row, so it is never built: it would
/// contribute a spurious crossing at its own y and nothing else.
struct Edge {
    /// The y of the edge's upper endpoint.
    top: i64,
    /// The y of its lower endpoint. `top..bottom` — half open, so a vertex
    /// shared with the next edge is counted once, not twice — is the set of
    /// sample rows this edge crosses.
    bottom: i64,
    /// The x at `top`.
    x: i64,
    /// The x travelled from `top` to `bottom`.
    run: i64,
    /// `1` when the contour ran downward through this edge and `-1` when it
    /// ran upward: the winding the non-zero rule accumulates.
    direction: i32,
}

impl Edge {
    /// The first sample column at or after where this edge crosses `row`.
    ///
    /// A sample sits inside a span that starts at an exact crossing `c` when
    /// its column is `>= c`, and inside one that ends at `c` when its column
    /// is `< c`; rounding the crossing up answers both, so the exact rational
    /// crossing never has to be carried around or compared.
    fn crossing(&self, row: i64) -> i64 {
        let rise = self.bottom - self.top;
        ceil_div(self.x * rise + self.run * (row - self.top), rise)
    }
}

/// A scan-converted shape: its edges, its extent, and the rule that decides
/// which of the regions they enclose is inside.
pub(crate) struct ScanFill {
    edges: Vec<Edge>,
    crossings: Vec<(i64, i32)>,
    rule: FillRule,
    space: SampleSpace,
    /// The sample-space bounding box of every vertex.
    extent: Extent,
}

/// The sample-space extremes of a shape's vertices.
struct Extent {
    min_x: i64,
    max_x: i64,
    min_y: i64,
    max_y: i64,
}

impl ScanFill {
    /// Build the converter for `contours`, or `None` when they enclose no area
    /// at all.
    ///
    /// A contour is implicitly closed, and one with fewer than three points
    /// bounds nothing and is skipped; a list whose every contour is skipped —
    /// or which is empty — yields `None` so the caller draws nothing without
    /// scanning a single row.
    pub(crate) fn new<C: AsRef<[(i32, i32)]>>(
        contours: &[C],
        space: SampleSpace,
        rule: FillRule,
    ) -> Option<Self> {
        let mut edges = Vec::new();
        let mut extent = Extent {
            min_x: i64::MAX,
            max_x: i64::MIN,
            min_y: i64::MAX,
            max_y: i64::MIN,
        };
        for contour in contours {
            let points = contour.as_ref();
            if points.len() < 3 {
                continue;
            }
            let Some(&last) = points.last() else {
                continue;
            };
            let mut previous = space.to_sample(last);
            for &point in points {
                let (x, y) = space.to_sample(point);
                extent.min_x = extent.min_x.min(x);
                extent.max_x = extent.max_x.max(x);
                extent.min_y = extent.min_y.min(y);
                extent.max_y = extent.max_y.max(y);
                if let Some(edge) = edge_between(previous, (x, y)) {
                    edges.push(edge);
                }
                previous = (x, y);
            }
        }
        if edges.is_empty() {
            return None;
        }
        // Sorted by upper endpoint so a row's scan can stop at the first edge
        // that starts below it instead of testing the whole list.
        edges.sort_unstable_by_key(|edge| edge.top);
        Some(Self {
            edges,
            crossings: Vec::new(),
            rule,
            space,
            extent,
        })
    }

    /// The pixel box `(x0, x1, y0, y1)` — half open on both axes — that can
    /// hold any covered sample, intersected with a `width`×`height` surface.
    /// `None` when the shape misses the surface entirely.
    ///
    /// No sample outside the vertices' own extent can be inside the shape, so
    /// restricting the scan to this box paints exactly what scanning the whole
    /// canvas would: a cursor or an icon glyph costs its own area rather than
    /// the surface's.
    pub(crate) fn bounds(&self, width: u32, height: u32) -> Option<(u32, u32, u32, u32)> {
        let span = pixel_span();
        // A pixel `p` owns sample units `[p * span, (p + 1) * span)`, so the
        // mathematical floor — correct for a vertex left of the origin too —
        // names the pixel each extreme sits in.
        let x0 = self.extent.min_x.div_euclid(span).max(0);
        let x1 = (self.extent.max_x.div_euclid(span) + 1).min(i64::from(width));
        let y0 = self.extent.min_y.div_euclid(span).max(0);
        let y1 = (self.extent.max_y.div_euclid(span) + 1).min(i64::from(height));
        if x0 >= x1 || y0 >= y1 {
            return None;
        }
        Some((
            u32::try_from(x0).ok()?,
            u32::try_from(x1).ok()?,
            u32::try_from(y0).ok()?,
            u32::try_from(y1).ok()?,
        ))
    }

    /// Accumulate the covered sub-sample count of each pixel of row `row` into
    /// `counts`, whose first entry is pixel `first_pixel`.
    ///
    /// The caller owns the buffer — one allocation for a whole fill rather than
    /// one per row — and clears it between rows.
    pub(crate) fn coverage_row(&mut self, row: u32, first_pixel: u32, counts: &mut [u16]) {
        let Self {
            edges,
            crossings,
            rule,
            ..
        } = self;
        let origin = i64::from(first_pixel) * pixel_span();
        for sub in 0..SUPERSAMPLE {
            let sample_row = sample_coordinate(row, sub);
            crossings.clear();
            for edge in edges.iter() {
                if edge.top > sample_row {
                    break;
                }
                if sample_row < edge.bottom {
                    crossings.push((edge.crossing(sample_row), edge.direction));
                }
            }
            crossings.sort_unstable_by_key(|&(at, _)| at);

            let mut accumulated = 0;
            let mut opened = 0;
            for &(at, direction) in crossings.iter() {
                let was_inside = rule.inside(accumulated);
                accumulated = rule.step(accumulated, direction);
                match (was_inside, rule.inside(accumulated)) {
                    (false, true) => opened = at,
                    (true, false) => add_span(counts, origin, opened, at),
                    _ => {}
                }
            }
        }
    }

    /// Where in the contours' own coordinates pixel `(x, y)`'s centre lies —
    /// the point a gradient paint is sampled at for that pixel.
    pub(crate) fn pixel_centre(&self, x: u32, y: u32) -> (f64, f64) {
        self.space.pixel_centre(x, y)
    }
}

/// Map a per-pixel covered-sample count to an alpha factor in `0..=255`.
pub(crate) fn coverage_alpha(count: u16) -> u8 {
    let samples = SUPERSAMPLE * SUPERSAMPLE;
    if samples == 0 {
        return 0;
    }
    let scaled = u32::from(u8::MAX) * u32::from(count) / samples;
    u8::try_from(scaled.min(u32::from(u8::MAX))).unwrap_or(u8::MAX)
}

/// Sample sub-units per pixel along one axis.
///
/// A pixel is twice [`SUPERSAMPLE`] sub-units wide, so every sample centre
/// lands on an odd offset and never exactly on a pixel boundary: a shape
/// grid-fitted to whole pixels is either wholly inside or wholly outside each
/// sample and draws with no fringe.
fn pixel_span() -> i64 {
    i64::from(2 * SUPERSAMPLE)
}

/// The sample-space coordinate of sub-sample `sub` of pixel `pixel`.
fn sample_coordinate(pixel: u32, sub: u32) -> i64 {
    i64::from(pixel) * pixel_span() + i64::from(2 * sub + 1)
}

/// `coord * numerator / denominator`, clamped into the sample-coordinate
/// range.
///
/// A product that would leave `i64` is at least `2^31` sub-units even after the
/// largest divisor a `u32` denominator can be, so saturating it is exactly what
/// the clamp answers anyway — which is why the multiplication needs no wider
/// arithmetic.
fn scale_axis(coord: i32, numerator: i64, denominator: i64) -> i64 {
    if numerator == denominator {
        return i64::from(coord).clamp(-COORD_LIMIT, COORD_LIMIT);
    }
    let scaled = match i64::from(coord).checked_mul(numerator) {
        Some(product) => product / denominator,
        None if coord < 0 => i64::MIN,
        None => i64::MAX,
    };
    scaled.clamp(-COORD_LIMIT, COORD_LIMIT)
}

/// The edge from `from` to `to`, or `None` when it is horizontal and crosses
/// no sample row.
fn edge_between(from: (i64, i64), to: (i64, i64)) -> Option<Edge> {
    let (top, bottom, x, run, direction) = match from.1.cmp(&to.1) {
        Ordering::Less => (from.1, to.1, from.0, to.0 - from.0, 1),
        Ordering::Greater => (to.1, from.1, to.0, from.0 - to.0, -1),
        Ordering::Equal => return None,
    };
    Some(Edge {
        top,
        bottom,
        x,
        run,
        direction,
    })
}

/// Add the sample columns of `from..to` — an inside span in sample units — to
/// the pixels of `counts`, whose first entry starts at sample column `origin`.
fn add_span(counts: &mut [u16], origin: i64, from: i64, to: i64) {
    let span = pixel_span();
    let Ok(pixels) = i64::try_from(counts.len()) else {
        return;
    };
    let start = from.max(origin);
    let end = to.min(origin + span * pixels);
    if start >= end {
        return;
    }
    let first = (start - origin).div_euclid(span);
    let last = (end - 1 - origin).div_euclid(span);
    let (Ok(first_index), Ok(last_index)) = (usize::try_from(first), usize::try_from(last)) else {
        return;
    };
    if first == last {
        add_samples(counts.get_mut(first_index), start, end);
        return;
    }
    add_samples(
        counts.get_mut(first_index),
        start,
        origin + (first + 1) * span,
    );
    add_samples(counts.get_mut(last_index), origin + last * span, end);
    // Every sample column of a pixel the span covers end to end is inside it,
    // so the interior is a whole-row add rather than a per-column count.
    let whole = u16::try_from(SUPERSAMPLE).unwrap_or(u16::MAX);
    if let Some(interior) = counts.get_mut(first_index + 1..last_index) {
        for count in interior.iter_mut() {
            *count += whole;
        }
    }
}

/// Add the number of sample centres in `from..to` to one pixel's count.
///
/// Sample centres sit at the odd sub-unit offsets, so this counts the odd
/// integers the interval holds.
fn add_samples(count: Option<&mut u16>, from: i64, to: i64) {
    let Some(count) = count else {
        return;
    };
    let centres = to.div_euclid(2) - from.div_euclid(2);
    *count += u16::try_from(centres).unwrap_or(0);
}

/// `numerator / divisor` rounded up, with `divisor` strictly positive.
fn ceil_div(numerator: i64, divisor: i64) -> i64 {
    let quotient = numerator.div_euclid(divisor);
    if numerator.rem_euclid(divisor) == 0 {
        quotient
    } else {
        quotient + 1
    }
}

#[cfg(test)]
#[path = "scan_tests.rs"]
mod tests;
