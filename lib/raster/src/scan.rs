//! The one scan converter every filled shape goes through.
//!
//! A shape reaches here as a set of closed contours in integer coordinates,
//! and leaves as a per-pixel alpha: what the
//! [`Surface`](crate::surface::Surface) fill entry points composite. Solid
//! artwork, grid-fitted device-space chrome, and a multi-contour SVG path are
//! the same problem, so there is one converter rather than one per caller.
//!
//! # Coverage is the area, not a sample count
//!
//! A pixel's alpha is the *exact fraction of its area* the shape covers. Point
//! sampling instead quantises both the answer and the edge's position, which
//! reads as soft, lopsided artwork — a shape symmetric about its centre comes
//! out asymmetric — and downscaled icons, a 256-unit drawing in twenty-odd
//! pixels with strokes a fraction of a pixel wide, show it plainly.
//!
//! The area is accumulated the way FreeType's grey rasteriser does it. Each
//! pixel of a row owns two signed accumulators: `cover`, the vertical extent
//! of the edges crossing it, and `area`, twice the trapezoid area those edges
//! cut off to their left. One left-to-right sweep carrying the running `cover`
//! yields every pixel's signed coverage, which the [`FillRule`] turns into
//! alpha. Nothing is sorted and each edge is visited once per row it touches,
//! so a row costs its edges plus its pixels rather than a sorted pass per
//! sample row.
//!
//! Every coordinate is integer, so a shape rasterises identically on every
//! target. Vertices are clamped to a bound far outside any allocatable surface
//! on the way in, which keeps every product well inside `i64` and makes the
//! whole converter total for adversarial input.

use core::cmp::Ordering;

use alloc::vec::Vec;

use crate::surface::SUBPIXEL;

/// Sub-units per pixel along each axis inside the converter.
///
/// The grid every vertex is snapped to, so it is also the finest edge
/// placement the coverage can distinguish: a 256th of a pixel, far below the
/// 255 alpha levels the result is quoted in.
const UNIT: i64 = 256;

/// What a wholly covered pixel accumulates: twice its area, in sub-units
/// squared.
///
/// Twice, because a trapezoid's area is accumulated without its halving — the
/// factor cancels here rather than being carried through every edge.
const FULL: i64 = 2 * UNIT * UNIT;

/// The furthest from the origin, in sub-units, a vertex may sit.
///
/// A million pixels — no surface that can be allocated comes close to it, so a
/// shape placed inside one is unaffected. Clamping to it bounds every product
/// an intersection computes to roughly `2^58`, so the arithmetic stays exact
/// in `i64` for any `i32` input a caller (or an attacker) supplies rather than
/// overflowing.
const COORD_LIMIT: i64 = 1 << 28;

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
    /// The alpha a pixel that accumulated `signed` coverage takes.
    ///
    /// A pixel covered twice over accumulates twice [`FULL`]; the rule decides
    /// whether that is opaque (non-zero) or cancels (even-odd), which is the
    /// same question the rules answer for a point, asked of an area.
    fn alpha(self, signed: i64) -> u8 {
        let covered = match self {
            Self::NonZero => i64::try_from(signed.unsigned_abs())
                .unwrap_or(FULL)
                .min(FULL),
            Self::EvenOdd => {
                let wrapped = signed.rem_euclid(2 * FULL);
                if wrapped > FULL {
                    2 * FULL - wrapped
                } else {
                    wrapped
                }
            }
        };
        let scaled = (covered * 255 + FULL / 2) / FULL;
        u8::try_from(scaled.clamp(0, 255)).unwrap_or(u8::MAX)
    }
}

/// How a contour's own coordinates reach sub-units, and how a pixel centre
/// gets back to them.
///
/// The two directions are held separately on purpose: the forward map must be
/// exact integer arithmetic, because it decides the coverage, while the
/// backward map only feeds a [`Paint`](crate::paint::Paint) sampler that works
/// in `f64`.
#[derive(Copy, Clone, Debug)]
pub(crate) struct SampleSpace {
    /// Sub-units per `denominator` contour units, horizontally.
    numerator_x: i64,
    /// Sub-units per `denominator` contour units, vertically.
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
            i64::from(width) * UNIT,
            i64::from(height) * UNIT,
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
            UNIT,
            UNIT,
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

    /// `point` in sub-units.
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

/// One non-horizontal edge of a contour, in sub-units, oriented downward.
///
/// A horizontal edge encloses no area and crosses no row, so it is never
/// built.
struct Edge {
    /// The y of the edge's upper endpoint.
    top: i64,
    /// The y of its lower endpoint.
    bottom: i64,
    /// The x at `top`.
    x_top: i64,
    /// The x at `bottom`.
    x_bottom: i64,
    /// `1` when the contour ran downward through this edge and `-1` when it
    /// ran upward: the winding the coverage accumulates.
    direction: i32,
}

impl Edge {
    /// Where this edge sits at `y`, which the caller keeps inside
    /// `top..=bottom`.
    fn x_at(&self, y: i64) -> i64 {
        let rise = self.bottom - self.top;
        let run = self.x_bottom - self.x_top;
        if run == 0 || rise <= 0 {
            return self.x_top;
        }
        self.x_top + rounded_div(run * (y - self.top), rise)
    }
}

/// A straight piece of an edge, clipped to one pixel row and travelling
/// downward.
#[derive(Copy, Clone)]
struct Piece {
    from: (i64, i64),
    to: (i64, i64),
    /// The winding direction of the edge this piece came from.
    sign: i64,
}

impl Piece {
    /// Where the piece sits at `x`, kept inside its own y range so the pieces
    /// of one edge always join up.
    fn y_at(self, x: i64) -> i64 {
        let run = self.to.0 - self.from.0;
        if run == 0 {
            return self.from.1;
        }
        let rise = self.to.1 - self.from.1;
        let y = self.from.1 + rounded_div(rise * (x - self.from.0), run);
        y.clamp(self.from.1, self.to.1)
    }
}

/// One pixel row's accumulators: the signed vertical extent and twice the
/// signed left-hand trapezoid area each pixel's edges contribute.
struct Cells<'a> {
    cover: &'a mut [i64],
    area: &'a mut [i64],
    /// The cover of everything left of the window, which every pixel in it
    /// sees. Those pixels are not drawn, but their winding still decides
    /// whether the first drawn pixel is inside.
    carry: i64,
    /// One past the window's last sub-unit, in window-relative coordinates.
    right: i64,
}

impl Cells<'_> {
    /// Accumulate one downward piece of an edge, already clipped to the row
    /// and expressed relative to the window's first pixel.
    fn segment(&mut self, from: (i64, i64), to: (i64, i64), sign: i64) {
        if from.0 == to.0 {
            self.vertical(from, to, sign);
            return;
        }
        if let Some(piece) = self.clip(Piece { from, to, sign }) {
            self.walk(piece);
        }
    }

    /// A piece that stays in one column: no cell walk, just its own cell.
    fn vertical(&mut self, from: (i64, i64), to: (i64, i64), sign: i64) {
        if from.0 < 0 {
            self.carry += sign * (to.1 - from.1);
            return;
        }
        if from.0 >= self.right {
            return;
        }
        self.add(self.cell_of(from.0), from, to, sign);
    }

    /// Trim `piece` to the window, folding the part left of it into
    /// [`Self::carry`] and discarding the part right of it.
    ///
    /// Both are exact: a pixel's coverage depends on the winding of every cell
    /// to its left but on the geometry of none of them, and on nothing to its
    /// right at all.
    fn clip(&mut self, mut piece: Piece) -> Option<Piece> {
        let ascending = piece.from.0 < piece.to.0;
        let (low, high) = if ascending {
            (piece.from.0, piece.to.0)
        } else {
            (piece.to.0, piece.from.0)
        };
        if high <= 0 {
            self.carry += piece.sign * (piece.to.1 - piece.from.1);
            return None;
        }
        if low >= self.right {
            return None;
        }
        if low < 0 {
            let y = piece.y_at(0);
            if ascending {
                self.carry += piece.sign * (y - piece.from.1);
                piece.from = (0, y);
            } else {
                self.carry += piece.sign * (piece.to.1 - y);
                piece.to = (0, y);
            }
        }
        if high > self.right {
            let y = piece.y_at(self.right);
            if ascending {
                piece.to = (self.right, y);
            } else {
                piece.from = (self.right, y);
            }
        }
        Some(piece)
    }

    /// Split `piece` at each column boundary it crosses and accumulate every
    /// part into the cell that holds it.
    fn walk(&mut self, piece: Piece) {
        let first = self.cell_of(piece.from.0);
        let last = self.cell_of(piece.to.0);
        let mut at = piece.from;
        match last.cmp(&first) {
            Ordering::Equal => {}
            Ordering::Greater => {
                for cell in first..last {
                    at = self.step(cell, at, piece, cell_base(cell + 1));
                }
            }
            Ordering::Less => {
                for cell in ((last + 1)..=first).rev() {
                    at = self.step(cell, at, piece, cell_base(cell));
                }
            }
        }
        self.add(last, at, piece.to, piece.sign);
    }

    /// Accumulate `piece` from `at` to where it crosses `boundary`, and report
    /// that crossing as the next part's start.
    fn step(&mut self, cell: usize, at: (i64, i64), piece: Piece, boundary: i64) -> (i64, i64) {
        let crossing = (boundary, piece.y_at(boundary).clamp(at.1, piece.to.1));
        self.add(cell, at, crossing, piece.sign);
        crossing
    }

    /// Add the part of an edge running `from` → `to` within `cell`.
    fn add(&mut self, cell: usize, from: (i64, i64), to: (i64, i64), sign: i64) {
        let height = to.1 - from.1;
        if height == 0 {
            return;
        }
        let base = cell_base(cell);
        let (Some(cover), Some(area)) = (self.cover.get_mut(cell), self.area.get_mut(cell)) else {
            return;
        };
        *cover += sign * height;
        *area += sign * height * ((from.0 - base) + (to.0 - base));
    }

    /// The cell holding window-relative `x`, which the caller keeps in
    /// `0..=right`.
    fn cell_of(&self, x: i64) -> usize {
        let cell = usize::try_from(x / UNIT).unwrap_or(0);
        cell.min(self.cover.len().saturating_sub(1))
    }
}

/// A scan-converted shape: its edges, its extent, the rule that decides which
/// of the regions they enclose is inside, and the row accumulators it reuses.
pub(crate) struct ScanFill {
    edges: Vec<Edge>,
    cover: Vec<i64>,
    area: Vec<i64>,
    rule: FillRule,
    space: SampleSpace,
    /// The sub-unit bounding box of every vertex.
    extent: Extent,
}

/// The sub-unit extremes of a shape's vertices.
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
            cover: Vec::new(),
            area: Vec::new(),
            rule,
            space,
            extent,
        })
    }

    /// The pixel box `(x0, x1, y0, y1)` — half open on both axes — that can
    /// hold any covered area, intersected with a `width`×`height` surface.
    /// `None` when the shape misses the surface entirely.
    ///
    /// No part of a pixel outside the vertices' own extent can be inside the
    /// shape, so restricting the scan to this box paints exactly what scanning
    /// the whole canvas would: a cursor or an icon glyph costs its own area
    /// rather than the surface's.
    pub(crate) fn bounds(&self, width: u32, height: u32) -> Option<(u32, u32, u32, u32)> {
        // A pixel `p` owns sub-units `[p * UNIT, (p + 1) * UNIT)`, so the
        // mathematical floor — correct for a vertex left of the origin too —
        // names the pixel each extreme sits in.
        let x0 = self.extent.min_x.div_euclid(UNIT).max(0);
        let x1 = (self.extent.max_x.div_euclid(UNIT) + 1).min(i64::from(width));
        let y0 = self.extent.min_y.div_euclid(UNIT).max(0);
        let y1 = (self.extent.max_y.div_euclid(UNIT) + 1).min(i64::from(height));
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

    /// Write the alpha of each pixel of row `row` into `alphas`, whose first
    /// entry is pixel `first_pixel`.
    ///
    /// Every entry is written, so the caller need not clear the buffer between
    /// rows — it owns it only to keep one allocation for a whole fill.
    pub(crate) fn coverage_row(&mut self, row: u32, first_pixel: u32, alphas: &mut [u8]) {
        let Self {
            edges,
            cover,
            area,
            rule,
            ..
        } = self;
        let Ok(count) = i64::try_from(alphas.len()) else {
            return;
        };
        cover.clear();
        cover.resize(alphas.len(), 0);
        area.clear();
        area.resize(alphas.len(), 0);

        let top = i64::from(row) * UNIT;
        let bottom = top + UNIT;
        let origin = i64::from(first_pixel) * UNIT;
        let mut cells = Cells {
            cover,
            area,
            carry: 0,
            right: count * UNIT,
        };
        for edge in edges.iter() {
            if edge.top >= bottom {
                break;
            }
            let from_y = edge.top.max(top);
            let to_y = edge.bottom.min(bottom);
            if from_y >= to_y {
                continue;
            }
            cells.segment(
                (edge.x_at(from_y) - origin, from_y),
                (edge.x_at(to_y) - origin, to_y),
                i64::from(edge.direction),
            );
        }

        let mut running = cells.carry * 2 * UNIT;
        for (index, alpha) in alphas.iter_mut().enumerate() {
            let (Some(&cover), Some(&area)) = (cells.cover.get(index), cells.area.get(index))
            else {
                break;
            };
            running += cover * 2 * UNIT;
            *alpha = rule.alpha(running - area);
        }
    }

    /// Where in the contours' own coordinates pixel `(x, y)`'s centre lies —
    /// the point a gradient paint is sampled at for that pixel.
    pub(crate) fn pixel_centre(&self, x: u32, y: u32) -> (f64, f64) {
        self.space.pixel_centre(x, y)
    }
}

/// The left edge of `cell`, in window-relative sub-units.
fn cell_base(cell: usize) -> i64 {
    i64::try_from(cell).unwrap_or(i64::MAX / UNIT) * UNIT
}

/// `coord * numerator / denominator`, rounded to the nearest sub-unit and
/// clamped into the coordinate range.
///
/// Rounded rather than truncated because truncation pulls every vertex toward
/// the origin, which shifts a shape by up to a sub-unit and — being
/// directional — makes a symmetric shape rasterise asymmetrically.
fn scale_axis(coord: i32, numerator: i64, denominator: i64) -> i64 {
    let coord = i64::from(coord);
    let Some(product) = coord.checked_mul(numerator) else {
        return if coord < 0 { -COORD_LIMIT } else { COORD_LIMIT };
    };
    rounded_div(product, denominator).clamp(-COORD_LIMIT, COORD_LIMIT)
}

/// `numerator / denominator`, rounded half away from zero. A zero denominator
/// has no quotient and answers zero rather than trapping.
fn rounded_div(numerator: i64, denominator: i64) -> i64 {
    let (numerator, denominator) = if denominator < 0 {
        (numerator.saturating_neg(), denominator.saturating_neg())
    } else {
        (numerator, denominator)
    };
    if denominator == 0 {
        return 0;
    }
    let half = denominator / 2;
    let biased = if numerator < 0 {
        numerator.saturating_sub(half)
    } else {
        numerator.saturating_add(half)
    };
    biased / denominator
}

/// The edge from `from` to `to`, oriented downward, or `None` when it is
/// horizontal and encloses no area.
fn edge_between(from: (i64, i64), to: (i64, i64)) -> Option<Edge> {
    let (top, bottom, x_top, x_bottom, direction) = match from.1.cmp(&to.1) {
        Ordering::Less => (from.1, to.1, from.0, to.0, 1),
        Ordering::Greater => (to.1, from.1, to.0, from.0, -1),
        Ordering::Equal => return None,
    };
    Some(Edge {
        top,
        bottom,
        x_top,
        x_bottom,
        direction,
    })
}

#[cfg(test)]
#[path = "scan_tests.rs"]
mod tests;
