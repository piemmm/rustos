//! The `d` attribute: SVG's path grammar, and flattening its curves.
//!
//! A path is a run of commands, each optionally repeated by simply writing
//! another parameter set after it, with coordinates that may be run together
//! wherever a sign or a second decimal point makes the boundary unambiguous
//! (`M0-4L2-6`, `.5.5`). All of that is read here, and every curve and arc
//! becomes a polyline before it leaves.
//!
//! # Flattening
//!
//! Subdivision is *error bounded*, never a fixed segment count: the number of
//! segments is computed from the curve's own second derivative and the
//! caller's `tolerance`, so a long sweeping curve gets more segments than a
//! tiny one and neither is over-tessellated. The bound used is the standard
//! one — a chord over a parameter step `h` departs from the curve by at most
//! `h²·max|C''|/8` — solved for the step that keeps that below `tolerance`.

use alloc::vec::Vec;

use core::f64::consts::{PI, TAU};

use tairix_util::mathf::{acos, ceil, cos, hypot, sin, sqrt};

use crate::error::SvgError;
use crate::geom::{Point, SubPath};
use crate::number::Numbers;

/// The most segments any single curve or arc may be flattened into.
///
/// A fixed safety bound, not an accuracy knob: the error-bounded formulas
/// below already choose the segment count, and this is only what stops a
/// hostile radius or coordinate from asking for an unbounded one.
const MAX_SEGMENTS: u32 = 256;

/// The smallest flattening tolerance honoured, in user units.
///
/// A tolerance of zero (or a negative or non-finite one) has no finite
/// segment count that satisfies it, so it is floored here rather than
/// allowed to ask for an unbounded subdivision.
const MIN_TOLERANCE: f64 = 1e-4;

/// Parse a `d` attribute into flattened sub-paths in user space.
///
/// `tolerance` is the greatest distance a flattened curve may depart from the
/// true one, in the same user units as the coordinates. `max_points` bounds
/// the whole path: a document that would exceed it is refused rather than
/// allowed to allocate without end.
///
/// A sub-path of a single point (`M10,10Z`) is kept: it draws nothing when
/// filled but is a legitimate round-capped dot when stroked, so the decision
/// belongs to the caller.
///
/// # Errors
/// Returns [`SvgError::UnsupportedPath`] for an unknown command or a path
/// that does not begin with a `moveto`, [`SvgError::InvalidNumber`] for a
/// malformed or missing parameter, and [`SvgError::TooComplex`] once
/// `max_points` would be exceeded. A path that fails anywhere is refused
/// whole; nothing half-parsed is drawn.
pub fn parse_path_data(
    d: &str,
    tolerance: f64,
    max_points: usize,
) -> Result<Vec<SubPath>, SvgError> {
    let mut numbers = Numbers::new(d);
    let mut builder = Builder::new(clamp_tolerance(tolerance), max_points);
    let mut previous: Option<char> = None;

    while !numbers.is_exhausted() {
        let command = match numbers.take_letter() {
            Some(letter) => {
                previous = Some(letter);
                letter
            }
            // A bare parameter set repeats the previous command, except after
            // a moveto, where SVG defines the repeat as a lineto.
            None => match previous {
                Some('M') => 'L',
                Some('m') => 'l',
                Some(letter) => letter,
                None => return Err(SvgError::UnsupportedPath),
            },
        };
        if builder.is_empty() && !matches!(command, 'M' | 'm') {
            return Err(SvgError::UnsupportedPath);
        }
        // A closepath takes no parameters, so letting it repeat implicitly
        // would consume nothing and never terminate. SVG requires a new
        // sub-path to begin with a moveto anyway.
        if matches!(command, 'Z' | 'z') {
            builder.close();
            previous = None;
            continue;
        }
        run(command, &mut numbers, &mut builder)?;
    }
    Ok(builder.finish())
}

/// `tolerance` floored to a value a finite subdivision can satisfy.
fn clamp_tolerance(tolerance: f64) -> f64 {
    if tolerance.is_finite() && tolerance > MIN_TOLERANCE {
        tolerance
    } else {
        MIN_TOLERANCE
    }
}

/// Execute one parameter set of `command`.
fn run(command: char, numbers: &mut Numbers<'_>, builder: &mut Builder) -> Result<(), SvgError> {
    let relative = command.is_ascii_lowercase();
    match command.to_ascii_uppercase() {
        'M' => {
            let point = builder.resolve(numbers.required()?, numbers.required()?, relative);
            builder.move_to(point)?;
        }
        'L' => {
            let point = builder.resolve(numbers.required()?, numbers.required()?, relative);
            builder.line_to(point)?;
        }
        'H' => {
            let x = numbers.required()?;
            let point = (
                if relative { builder.cursor.0 + x } else { x },
                builder.cursor.1,
            );
            builder.line_to(point)?;
        }
        'V' => {
            let y = numbers.required()?;
            let point = (
                builder.cursor.0,
                if relative { builder.cursor.1 + y } else { y },
            );
            builder.line_to(point)?;
        }
        'C' => {
            let c1 = builder.resolve(numbers.required()?, numbers.required()?, relative);
            let c2 = builder.resolve(numbers.required()?, numbers.required()?, relative);
            let to = builder.resolve(numbers.required()?, numbers.required()?, relative);
            builder.cubic_to(c1, c2, to)?;
        }
        'S' => {
            let c1 = builder.reflected_cubic();
            let c2 = builder.resolve(numbers.required()?, numbers.required()?, relative);
            let to = builder.resolve(numbers.required()?, numbers.required()?, relative);
            builder.cubic_to(c1, c2, to)?;
        }
        'Q' => {
            let control = builder.resolve(numbers.required()?, numbers.required()?, relative);
            let to = builder.resolve(numbers.required()?, numbers.required()?, relative);
            builder.quadratic_to(control, to)?;
        }
        'T' => {
            let control = builder.reflected_quadratic();
            let to = builder.resolve(numbers.required()?, numbers.required()?, relative);
            builder.quadratic_to(control, to)?;
        }
        'A' => {
            let rx = numbers.required()?;
            let ry = numbers.required()?;
            let rotation = numbers.required()?;
            let large = numbers.required_flag()?;
            let sweep = numbers.required_flag()?;
            let to = builder.resolve(numbers.required()?, numbers.required()?, relative);
            builder.arc_to((rx, ry), rotation * PI / 180.0, large, sweep, to)?;
        }
        _ => return Err(SvgError::UnsupportedPath),
    }
    Ok(())
}

/// The sub-paths built so far and the pen state the next command starts from.
struct Builder {
    subpaths: Vec<SubPath>,
    current: Vec<Point>,
    start: Point,
    cursor: Point,
    /// The previous command's second cubic control point, which `S` reflects.
    cubic_control: Option<Point>,
    /// The previous command's quadratic control point, which `T` reflects.
    quadratic_control: Option<Point>,
    tolerance: f64,
    points_left: usize,
}

impl Builder {
    fn new(tolerance: f64, max_points: usize) -> Self {
        Self {
            subpaths: Vec::new(),
            current: Vec::new(),
            start: (0.0, 0.0),
            cursor: (0.0, 0.0),
            cubic_control: None,
            quadratic_control: None,
            tolerance,
            points_left: max_points,
        }
    }

    /// Whether no sub-path has been started yet.
    fn is_empty(&self) -> bool {
        self.current.is_empty() && self.subpaths.is_empty()
    }

    /// A parameter pair as an absolute point.
    fn resolve(&self, x: f64, y: f64, relative: bool) -> Point {
        if relative {
            (self.cursor.0 + x, self.cursor.1 + y)
        } else {
            (x, y)
        }
    }

    /// The control point `S` implies: the previous cubic's reflected about
    /// the current point, or the current point when the previous command was
    /// not a cubic.
    fn reflected_cubic(&self) -> Point {
        reflect(self.cursor, self.cubic_control)
    }

    /// The control point `T` implies, on the same rule as [`Self::reflected_cubic`].
    fn reflected_quadratic(&self) -> Point {
        reflect(self.cursor, self.quadratic_control)
    }

    /// Begin a new sub-path at `point`.
    fn move_to(&mut self, point: Point) -> Result<(), SvgError> {
        self.flush(false);
        self.start = point;
        self.cursor = point;
        self.charge()?;
        self.current.push(point);
        self.forget_controls();
        Ok(())
    }

    /// Extend the current sub-path to `point`.
    fn line_to(&mut self, point: Point) -> Result<(), SvgError> {
        self.push(point)?;
        self.forget_controls();
        Ok(())
    }

    /// Extend the current sub-path along a cubic curve.
    fn cubic_to(&mut self, c1: Point, c2: Point, to: Point) -> Result<(), SvgError> {
        let from = self.cursor;
        let mut points = Vec::new();
        flatten_cubic(from, c1, c2, to, self.tolerance, &mut points);
        self.extend(points)?;
        self.cubic_control = Some(c2);
        self.quadratic_control = None;
        Ok(())
    }

    /// Extend the current sub-path along a quadratic curve.
    fn quadratic_to(&mut self, control: Point, to: Point) -> Result<(), SvgError> {
        let from = self.cursor;
        let mut points = Vec::new();
        flatten_quadratic(from, control, to, self.tolerance, &mut points);
        self.extend(points)?;
        self.quadratic_control = Some(control);
        self.cubic_control = None;
        Ok(())
    }

    /// Extend the current sub-path along an elliptical arc.
    fn arc_to(
        &mut self,
        radii: Point,
        rotation: f64,
        large: bool,
        sweep: bool,
        to: Point,
    ) -> Result<(), SvgError> {
        let from = self.cursor;
        let mut points = Vec::new();
        match arc_centre(from, to, radii, rotation, large, sweep) {
            // A zero radius, or an arc that ends where it began, degrades to
            // a straight line, exactly as the specification requires.
            None => points.push(to),
            Some(arc) => {
                flatten_ellipse_arc(
                    arc.centre,
                    arc.radii,
                    rotation,
                    arc.start,
                    arc.sweep,
                    self.tolerance,
                    &mut points,
                );
                // The parameterisation reproduces the endpoint only to
                // rounding; landing exactly on it keeps a following segment
                // continuous.
                if let Some(last) = points.last_mut() {
                    *last = to;
                }
            }
        }
        self.extend(points)?;
        self.forget_controls();
        Ok(())
    }

    /// Close the current sub-path; the pen returns to where it began.
    fn close(&mut self) {
        self.flush(true);
        self.cursor = self.start;
        self.forget_controls();
    }

    /// Forget the reflected-control state, which only survives between two
    /// curves of the same kind.
    fn forget_controls(&mut self) {
        self.cubic_control = None;
        self.quadratic_control = None;
    }

    /// Take one point from the budget.
    fn charge(&mut self) -> Result<(), SvgError> {
        if self.points_left == 0 {
            return Err(SvgError::TooComplex);
        }
        self.points_left -= 1;
        Ok(())
    }

    /// Append one point, charging it against the budget.
    ///
    /// A segment that follows a `closepath` without an intervening `moveto`
    /// begins a fresh sub-path at the point the pen returned to, so the
    /// contour does not silently lose its first vertex.
    fn push(&mut self, point: Point) -> Result<(), SvgError> {
        if self.current.is_empty() {
            self.charge()?;
            self.current.push(self.cursor);
        }
        self.charge()?;
        self.current.push(point);
        self.cursor = point;
        Ok(())
    }

    /// Append a flattened run of points.
    fn extend(&mut self, points: Vec<Point>) -> Result<(), SvgError> {
        for point in points {
            self.push(point)?;
        }
        Ok(())
    }

    /// End the current sub-path, if there is one.
    fn flush(&mut self, closed: bool) {
        if self.current.is_empty() {
            return;
        }
        let points = core::mem::take(&mut self.current);
        self.subpaths.push(SubPath { points, closed });
    }

    /// The finished sub-paths.
    fn finish(mut self) -> Vec<SubPath> {
        self.flush(false);
        self.subpaths
    }
}

/// `control` mirrored about `about`, or `about` itself when there is no
/// control point to mirror.
fn reflect(about: Point, control: Option<Point>) -> Point {
    match control {
        Some(previous) => (2.0 * about.0 - previous.0, 2.0 * about.1 - previous.1),
        None => about,
    }
}

/// The number of uniform segments that keeps a curve whose greatest second
/// derivative is `curvature` within `tolerance` of its chords.
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "the value is positive and finite here, and a float-to-integer \
              cast saturates rather than wrapping, so the following clamp \
              still lands inside the segment bound"
)]
fn segments_for(curvature: f64, tolerance: f64) -> u32 {
    if !curvature.is_finite() || curvature <= 0.0 {
        return 1;
    }
    let count = ceil(sqrt(curvature / (8.0 * tolerance)));
    if count.is_finite() && count >= 1.0 {
        (count as u32).clamp(1, MAX_SEGMENTS)
    } else {
        1
    }
}

/// Flatten a cubic Bézier, appending the points *after* `from`.
pub fn flatten_cubic(
    from: Point,
    c1: Point,
    c2: Point,
    to: Point,
    tolerance: f64,
    out: &mut Vec<Point>,
) {
    let tolerance = clamp_tolerance(tolerance);
    // A cubic's second derivative is a blend of its two second differences,
    // so the larger of them bounds the curvature over the whole span.
    let first = second_difference(from, c1, c2);
    let second = second_difference(c1, c2, to);
    let n = segments_for(6.0 * first.max(second), tolerance);
    for step in 1..=n {
        let t = f64::from(step) / f64::from(n);
        out.push(cubic_at(from, c1, c2, to, t));
    }
}

/// Flatten a quadratic Bézier, appending the points *after* `from`.
pub fn flatten_quadratic(
    from: Point,
    control: Point,
    to: Point,
    tolerance: f64,
    out: &mut Vec<Point>,
) {
    let tolerance = clamp_tolerance(tolerance);
    let n = segments_for(2.0 * second_difference(from, control, to), tolerance);
    for step in 1..=n {
        let t = f64::from(step) / f64::from(n);
        let u = 1.0 - t;
        out.push((
            u * u * from.0 + 2.0 * u * t * control.0 + t * t * to.0,
            u * u * from.1 + 2.0 * u * t * control.1 + t * t * to.1,
        ));
    }
}

/// Flatten an elliptical arc, appending the points *after* its start.
///
/// The arc runs `sweep_angle` radians from `start_angle`, measured in the
/// ellipse's own frame before `x_rotation_radians` turns it. A full turn is a
/// legitimate sweep, so this also draws whole circles and ellipses.
pub fn flatten_ellipse_arc(
    centre: Point,
    radii: Point,
    x_rotation_radians: f64,
    start_angle: f64,
    sweep_angle: f64,
    tolerance: f64,
    out: &mut Vec<Point>,
) {
    let tolerance = clamp_tolerance(tolerance);
    let bulge = radii.0.abs().max(radii.1.abs());
    let span = sweep_angle.abs();
    // Angle is the parameter here, so the step that meets the tolerance
    // scales with the arc's length as well as its radius.
    let n = segments_for(bulge * span * span, tolerance);
    let (cos_r, sin_r) = (cos(x_rotation_radians), sin(x_rotation_radians));
    for step in 1..=n {
        let angle = start_angle + sweep_angle * f64::from(step) / f64::from(n);
        let (x, y) = (radii.0 * cos(angle), radii.1 * sin(angle));
        out.push((
            centre.0 + cos_r * x - sin_r * y,
            centre.1 + sin_r * x + cos_r * y,
        ));
    }
}

/// A point on a cubic Bézier.
fn cubic_at(from: Point, first: Point, second: Point, to: Point, t: f64) -> Point {
    let rest = 1.0 - t;
    let weights = [
        rest * rest * rest,
        3.0 * rest * rest * t,
        3.0 * rest * t * t,
        t * t * t,
    ];
    (
        weights[0] * from.0 + weights[1] * first.0 + weights[2] * second.0 + weights[3] * to.0,
        weights[0] * from.1 + weights[1] * first.1 + weights[2] * second.1 + weights[3] * to.1,
    )
}

/// The magnitude of the second difference of three control points, which is
/// what bounds a Bézier's curvature.
fn second_difference(a: Point, b: Point, c: Point) -> f64 {
    hypot(a.0 - 2.0 * b.0 + c.0, a.1 - 2.0 * b.1 + c.1)
}

/// An arc in centre parameterisation.
struct Arc {
    centre: Point,
    radii: Point,
    start: f64,
    sweep: f64,
}

/// Convert SVG's endpoint arc parameterisation to the centre one.
///
/// Returns `None` for the cases the specification defines as a straight line:
/// coincident endpoints, or a zero radius. Out-of-range radii are scaled up
/// by the specification's correction factor rather than refused.
fn arc_centre(
    from: Point,
    to: Point,
    radii: Point,
    rotation: f64,
    large: bool,
    sweep: bool,
) -> Option<Arc> {
    if from == to {
        return None;
    }
    let (mut rx, mut ry) = (radii.0.abs(), radii.1.abs());
    if rx == 0.0 || ry == 0.0 {
        return None;
    }
    let (cos_r, sin_r) = (cos(rotation), sin(rotation));
    let (dx, dy) = ((from.0 - to.0) / 2.0, (from.1 - to.1) / 2.0);
    let x1 = cos_r * dx + sin_r * dy;
    let y1 = -sin_r * dx + cos_r * dy;

    // An ellipse too small to reach both endpoints is grown until it just
    // does, which is what the specification prescribes over refusing it.
    let lambda = (x1 * x1) / (rx * rx) + (y1 * y1) / (ry * ry);
    if lambda > 1.0 {
        let grow = sqrt(lambda);
        rx *= grow;
        ry *= grow;
    }

    let numerator = (rx * rx * ry * ry - rx * rx * y1 * y1 - ry * ry * x1 * x1).max(0.0);
    let denominator = rx * rx * y1 * y1 + ry * ry * x1 * x1;
    if denominator <= 0.0 {
        return None;
    }
    let sign = if large == sweep { -1.0 } else { 1.0 };
    let coefficient = sign * sqrt(numerator / denominator);
    let cx1 = coefficient * rx * y1 / ry;
    let cy1 = -coefficient * ry * x1 / rx;
    let centre = (
        cos_r * cx1 - sin_r * cy1 + f64::midpoint(from.0, to.0),
        sin_r * cx1 + cos_r * cy1 + f64::midpoint(from.1, to.1),
    );

    let u = ((x1 - cx1) / rx, (y1 - cy1) / ry);
    let v = ((-x1 - cx1) / rx, (-y1 - cy1) / ry);
    let start = angle_between((1.0, 0.0), u);
    let mut delta = angle_between(u, v) % TAU;
    if !sweep && delta > 0.0 {
        delta -= TAU;
    } else if sweep && delta < 0.0 {
        delta += TAU;
    }
    Some(Arc {
        centre,
        radii: (rx, ry),
        start,
        sweep: delta,
    })
}

/// The signed angle from `u` to `v`, in `-PI..=PI`.
fn angle_between(u: Point, v: Point) -> f64 {
    let lengths = hypot(u.0, u.1) * hypot(v.0, v.1);
    if lengths == 0.0 {
        return 0.0;
    }
    let cosine = (u.0 * v.0 + u.1 * v.1) / lengths;
    let angle = acos(cosine);
    if u.0 * v.1 - u.1 * v.0 < 0.0 {
        -angle
    } else {
        angle
    }
}

#[cfg(test)]
#[path = "pathdata_tests.rs"]
mod tests;
