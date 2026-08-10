//! Stroking: turning an outline into the area it covers.
//!
//! The input is already a flattened polyline, so the stroked area is the
//! union of simple convex pieces — one quadrilateral per segment, one join
//! per interior vertex, one cap per open end — rather than a pair of offset
//! curves that would have to be trimmed and joined analytically. Every piece
//! is emitted with the same winding direction, so filling the result with the
//! **non-zero** rule unions them; even-odd would punch holes wherever two
//! pieces overlap.
//!
//! Two shapes fall out of that for free: a round join is exactly a disc at
//! the vertex, and a round cap exactly a disc at the end point, because the
//! part of each disc that lies inside the neighbouring quadrilaterals is
//! already covered. Only miter and bevel joins need to know which side of the
//! corner is the outside.

use alloc::vec::Vec;

use core::f64::consts::TAU;

use tairix_util::mathf::{ceil, cos, hypot, sin, sqrt};

use crate::error::SvgError;
use crate::geom::{LineCap, LineJoin, Point, StrokeStyle, SubPath};

/// The most segments any one round join, cap, or dot may be drawn with.
///
/// A fixed safety bound, not an accuracy knob: the tolerance already chooses
/// the segment count, and this is what stops a hostile width from asking for
/// an unbounded one.
const MAX_ARC_SEGMENTS: u32 = 128;

/// The smallest flattening tolerance honoured, in user units.
const MIN_TOLERANCE: f64 = 1e-4;

/// Below this length a segment has no direction to offset along, so it is
/// skipped rather than producing a normal of zeroes.
const MIN_SEGMENT: f64 = 1e-12;

/// The area a stroke covers, as closed contours to be filled with the
/// non-zero winding rule.
///
/// `tolerance` is the greatest distance a round join or cap may depart from a
/// true arc, in the same user units as the coordinates. `max_points` bounds
/// the whole result, so a hostile dash pattern over a long path fails closed
/// instead of allocating without end.
///
/// A width that is zero, negative, or not a number strokes nothing, which is
/// what SVG draws for it.
///
/// # Errors
/// Returns [`SvgError::TooComplex`] once `max_points` would be exceeded.
pub fn stroke_outline(
    subpaths: &[SubPath],
    style: &StrokeStyle,
    tolerance: f64,
    max_points: usize,
) -> Result<Vec<SubPath>, SvgError> {
    if !style.width.is_finite() || style.width <= 0.0 {
        return Ok(Vec::new());
    }
    let mut stroker = Stroker {
        half: style.width / 2.0,
        cap: style.cap,
        join: style.join,
        miter_limit: style.miter_limit.max(1.0),
        tolerance: clamp_tolerance(tolerance),
        pieces: Vec::new(),
        points_left: max_points,
    };
    let pattern = dash_pattern(&style.dashes);
    for subpath in subpaths {
        let points = without_repeats(&subpath.points);
        match &pattern {
            None => stroker.polyline(&points, subpath.closed)?,
            Some(pattern) => {
                let runs = stroker.dashed(&points, subpath.closed, pattern, style.dash_offset)?;
                for run in runs {
                    stroker.polyline(&run, false)?;
                }
            }
        }
    }
    Ok(stroker.pieces)
}

/// `tolerance` floored to a value a finite subdivision can satisfy.
fn clamp_tolerance(tolerance: f64) -> f64 {
    if tolerance.is_finite() && tolerance > MIN_TOLERANCE {
        tolerance
    } else {
        MIN_TOLERANCE
    }
}

/// The dash pattern to walk, or `None` for a solid stroke.
///
/// An odd-length pattern repeats to make it even, as SVG requires. A pattern
/// carrying a negative or non-finite length, or summing to nothing, is
/// invalid and draws solid rather than dashing to a standstill.
fn dash_pattern(dashes: &[f64]) -> Option<Vec<f64>> {
    if dashes.is_empty() {
        return None;
    }
    if dashes.iter().any(|len| !len.is_finite() || *len < 0.0) {
        return None;
    }
    if dashes.iter().sum::<f64>() <= 0.0 {
        return None;
    }
    let mut pattern = Vec::with_capacity(dashes.len() * 2);
    pattern.extend_from_slice(dashes);
    if dashes.len() % 2 == 1 {
        pattern.extend_from_slice(dashes);
    }
    Some(pattern)
}

/// `points` with consecutive duplicates removed, which is what keeps a
/// zero-length segment from producing a normal of zeroes.
fn without_repeats(points: &[Point]) -> Vec<Point> {
    let mut out: Vec<Point> = Vec::with_capacity(points.len());
    for point in points {
        if out
            .last()
            .is_none_or(|last| distance(*last, *point) > MIN_SEGMENT)
        {
            out.push(*point);
        }
    }
    out
}

/// The pieces stroked so far, and what the next one is drawn with.
struct Stroker {
    half: f64,
    cap: LineCap,
    join: LineJoin,
    miter_limit: f64,
    tolerance: f64,
    pieces: Vec<SubPath>,
    points_left: usize,
}

impl Stroker {
    /// Stroke one polyline: a quadrilateral per segment, a join at every
    /// interior vertex, and either caps or a seam join at its ends.
    fn polyline(&mut self, points: &[Point], closed: bool) -> Result<(), SvgError> {
        // A closed contour that spells its last point as its first again has
        // one vertex, not two, at the seam.
        let ring = match points {
            [first, .., last] if closed && distance(*first, *last) <= MIN_SEGMENT => {
                &points[..points.len() - 1]
            }
            _ => points,
        };
        let count = ring.len();
        match count {
            0 => return Ok(()),
            1 => return self.dot(ring[0]),
            _ => {}
        }

        let segments = if closed { count } else { count - 1 };
        for index in 0..segments {
            let a = ring[index];
            let b = ring[(index + 1) % count];
            if distance(a, b) > MIN_SEGMENT {
                self.segment(a, b)?;
            }
        }
        if closed {
            for index in 0..count {
                let before = ring[(index + count - 1) % count];
                self.corner(before, ring[index], ring[(index + 1) % count])?;
            }
        } else {
            for index in 1..count - 1 {
                self.corner(ring[index - 1], ring[index], ring[index + 1])?;
            }
            self.cap(ring[0], ring[1])?;
            self.cap(ring[count - 1], ring[count - 2])?;
        }
        Ok(())
    }

    /// The rectangle one segment sweeps.
    fn segment(&mut self, a: Point, b: Point) -> Result<(), SvgError> {
        let normal = self.normal(a, b);
        self.emit(alloc::vec![
            (a.0 + normal.0, a.1 + normal.1),
            (b.0 + normal.0, b.1 + normal.1),
            (b.0 - normal.0, b.1 - normal.1),
            (a.0 - normal.0, a.1 - normal.1),
        ])
    }

    /// The piece that fills the outside of the corner at `vertex`.
    fn corner(&mut self, before: Point, vertex: Point, after: Point) -> Result<(), SvgError> {
        let Some(incoming) = direction(before, vertex) else {
            return Ok(());
        };
        let Some(outgoing) = direction(vertex, after) else {
            return Ok(());
        };
        if self.join == LineJoin::Round {
            return self.disc(vertex);
        }
        let cross = incoming.0 * outgoing.1 - incoming.1 * outgoing.0;
        if cross == 0.0 {
            // Collinear: the two rectangles already meet flush.
            return Ok(());
        }
        // The outside of the turn is the side the path turns away from.
        let side = if cross > 0.0 { -self.half } else { self.half };
        let first = (-incoming.1 * side, incoming.0 * side);
        let second = (-outgoing.1 * side, outgoing.0 * side);
        let a = (vertex.0 + first.0, vertex.1 + first.1);
        let b = (vertex.0 + second.0, vertex.1 + second.1);

        if self.join == LineJoin::Miter {
            if let Some(apex) = miter_apex(a, incoming, b, outgoing) {
                // The miter limit is the ratio of the spike's length to the
                // stroke width; beyond it SVG cuts the corner square.
                if distance(apex, vertex) / self.half <= self.miter_limit {
                    return self.emit(alloc::vec![vertex, a, apex, b]);
                }
            }
        }
        self.emit(alloc::vec![vertex, a, b])
    }

    /// The cap on the end `tip`, whose polyline continues towards `inward`.
    fn cap(&mut self, tip: Point, inward: Point) -> Result<(), SvgError> {
        match self.cap {
            LineCap::Butt => Ok(()),
            LineCap::Round => self.disc(tip),
            LineCap::Square => {
                let Some(direction) = direction(inward, tip) else {
                    return Ok(());
                };
                let normal = (-direction.1 * self.half, direction.0 * self.half);
                let reach = (direction.0 * self.half, direction.1 * self.half);
                self.emit(alloc::vec![
                    (tip.0 + normal.0, tip.1 + normal.1),
                    (tip.0 + normal.0 + reach.0, tip.1 + normal.1 + reach.1),
                    (tip.0 - normal.0 + reach.0, tip.1 - normal.1 + reach.1),
                    (tip.0 - normal.0, tip.1 - normal.1),
                ])
            }
        }
    }

    /// What a sub-path of a single point strokes: a dot under a round or
    /// square cap, and nothing at all under a butt cap.
    fn dot(&mut self, at: Point) -> Result<(), SvgError> {
        match self.cap {
            LineCap::Butt => Ok(()),
            LineCap::Round => self.disc(at),
            LineCap::Square => self.emit(alloc::vec![
                (at.0 - self.half, at.1 - self.half),
                (at.0 + self.half, at.1 - self.half),
                (at.0 + self.half, at.1 + self.half),
                (at.0 - self.half, at.1 + self.half),
            ]),
        }
    }

    /// A disc of the stroke's half width, which is both a round join and a
    /// round cap: the part inside the neighbouring rectangles is already
    /// covered, and the part outside is the arc that was wanted.
    fn disc(&mut self, centre: Point) -> Result<(), SvgError> {
        let steps = arc_steps(self.half, TAU, self.tolerance);
        let mut ring = Vec::with_capacity(steps as usize);
        for step in 0..steps {
            let angle = TAU * f64::from(step) / f64::from(steps);
            ring.push((
                centre.0 + self.half * cos(angle),
                centre.1 + self.half * sin(angle),
            ));
        }
        self.emit(ring)
    }

    /// The offset of a segment's edge from its centre line.
    fn normal(&self, a: Point, b: Point) -> Point {
        match direction(a, b) {
            Some(unit) => (-unit.1 * self.half, unit.0 * self.half),
            None => (0.0, 0.0),
        }
    }

    /// Record one piece, wound the same way as every other so the non-zero
    /// rule unions them rather than cancelling them.
    fn emit(&mut self, mut points: Vec<Point>) -> Result<(), SvgError> {
        if points.len() < 3 {
            return Ok(());
        }
        if points.len() > self.points_left {
            return Err(SvgError::TooComplex);
        }
        self.points_left -= points.len();
        if signed_area(&points) > 0.0 {
            points.reverse();
        }
        self.pieces.push(SubPath::closed(points));
        Ok(())
    }

    /// Split a polyline into the runs a dash pattern leaves drawn.
    fn dashed(
        &mut self,
        points: &[Point],
        closed: bool,
        pattern: &[f64],
        offset: f64,
    ) -> Result<Vec<Vec<Point>>, SvgError> {
        if points.len() < 2 {
            return Ok(alloc::vec![points.to_vec()]);
        }
        let total: f64 = pattern.iter().sum();
        let mut phase = if offset.is_finite() {
            offset % total
        } else {
            0.0
        };
        if phase < 0.0 {
            phase += total;
        }
        let mut index = 0;
        while phase >= pattern[index] {
            phase -= pattern[index];
            index = (index + 1) % pattern.len();
        }
        let mut drawing = index % 2 == 0;
        let mut left = pattern[index] - phase;
        let began_drawing = drawing;

        let mut runs: Vec<Vec<Point>> = Vec::new();
        let mut run: Vec<Point> = if drawing {
            alloc::vec![points[0]]
        } else {
            Vec::new()
        };
        let count = points.len();
        let segments = if closed { count } else { count - 1 };
        for step in 0..segments {
            let a = points[step];
            let b = points[(step + 1) % count];
            let length = distance(a, b);
            if length <= MIN_SEGMENT {
                continue;
            }
            let mut walked = 0.0;
            while length - walked > left {
                walked += left;
                let at = lerp(a, b, walked / length);
                // Every turn of this loop records a point, so the dash walk
                // is bounded by the caller's budget and cannot spin.
                self.charge()?;
                run.push(at);
                if drawing {
                    runs.push(core::mem::take(&mut run));
                }
                drawing = !drawing;
                index = (index + 1) % pattern.len();
                left = pattern[index];
            }
            left -= length - walked;
            if drawing {
                self.charge()?;
                run.push(b);
            }
        }
        match runs.first_mut() {
            // A dash that spans a closed contour's seam is one run, not a
            // pair with caps facing each other across the join.
            Some(head) if closed && began_drawing && drawing && !run.is_empty() => {
                run.extend_from_slice(head.get(1..).unwrap_or_default());
                *head = run;
            }
            _ if !run.is_empty() => runs.push(run),
            _ => {}
        }
        Ok(runs)
    }

    /// Take one point from the budget.
    fn charge(&mut self) -> Result<(), SvgError> {
        if self.points_left == 0 {
            return Err(SvgError::TooComplex);
        }
        self.points_left -= 1;
        Ok(())
    }
}

/// The unit vector from `a` to `b`, or `None` when they are the same point.
fn direction(a: Point, b: Point) -> Option<Point> {
    let (dx, dy) = (b.0 - a.0, b.1 - a.1);
    let length = hypot(dx, dy);
    (length > MIN_SEGMENT).then(|| (dx / length, dy / length))
}

/// Where the two offset edges of a miter join meet, or `None` when they are
/// parallel and there is no apex.
fn miter_apex(a: Point, along_a: Point, b: Point, along_b: Point) -> Option<Point> {
    let denominator = along_a.0 * along_b.1 - along_a.1 * along_b.0;
    if denominator == 0.0 {
        return None;
    }
    let (dx, dy) = (b.0 - a.0, b.1 - a.1);
    let t = (dx * along_b.1 - dy * along_b.0) / denominator;
    let apex = (a.0 + along_a.0 * t, a.1 + along_a.1 * t);
    (apex.0.is_finite() && apex.1.is_finite()).then_some(apex)
}

/// The number of segments a round join or cap of radius `radius` needs to
/// stay within `tolerance` of a true arc over `sweep` radians.
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "the value is positive and finite here, and a float-to-integer \
              cast saturates rather than wrapping, so the following clamp \
              still lands inside the segment bound"
)]
fn arc_steps(radius: f64, sweep: f64, tolerance: f64) -> u32 {
    let curvature = radius * sweep * sweep;
    if !curvature.is_finite() || curvature <= 0.0 {
        return 3;
    }
    let count = ceil(sqrt(curvature / (8.0 * tolerance)));
    if count.is_finite() && count >= 3.0 {
        (count as u32).clamp(3, MAX_ARC_SEGMENTS)
    } else {
        3
    }
}

/// The distance between two points.
fn distance(a: Point, b: Point) -> f64 {
    hypot(b.0 - a.0, b.1 - a.1)
}

/// The point a fraction `t` of the way from `a` to `b`.
fn lerp(a: Point, b: Point, t: f64) -> Point {
    (a.0 + (b.0 - a.0) * t, a.1 + (b.1 - a.1) * t)
}

/// Twice the signed area of a closed ring, whose sign is its winding
/// direction.
fn signed_area(points: &[Point]) -> f64 {
    let mut sum = 0.0;
    for index in 0..points.len() {
        let next = points[(index + 1) % points.len()];
        let here = points[index];
        sum += here.0 * next.1 - next.0 * here.1;
    }
    sum
}

#[cfg(test)]
#[path = "stroke_tests.rs"]
mod tests;
