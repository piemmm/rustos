//! The geometry every stage of the decode hands on: flattened contours in
//! user space, and the stroke style that turns an outline into an area.
//!
//! SVG's shapes are curves, arcs, and stroked outlines; the desktop's
//! rasteriser fills polygons. Everything therefore becomes a [`SubPath`] — an
//! ordered run of user-space points, open or closed — as early as possible,
//! and every later stage (stroking, transforming, mapping onto the design
//! grid) works on that one representation rather than on a shape-specific
//! form. There is exactly one flattening step and one place a curve stops
//! being a curve.

use alloc::vec::Vec;

/// A point in the document's user space.
pub type Point = (f64, f64);

/// One flattened contour: the ordered points of a single sub-path, and
/// whether the author closed it.
///
/// Closure is kept rather than baked in because it means different things to
/// the two consumers: a fill always treats a sub-path as closed, while a
/// stroke draws caps on an open one and a join at the seam of a closed one.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SubPath {
    /// The contour's points, in order.
    pub points: Vec<Point>,
    /// Whether the sub-path was closed (`Z`), or is implicitly closed by
    /// being a shape that has no ends (a rect, a circle, a `<polygon>`).
    pub closed: bool,
}

impl SubPath {
    /// A closed contour through `points`.
    #[must_use]
    pub fn closed(points: Vec<Point>) -> Self {
        Self {
            points,
            closed: true,
        }
    }

    /// An open contour through `points`.
    #[must_use]
    pub fn open(points: Vec<Point>) -> Self {
        Self {
            points,
            closed: false,
        }
    }

    /// Whether the contour encloses no area and so contributes no fill.
    #[must_use]
    pub fn is_degenerate(&self) -> bool {
        self.points.len() < 3
    }
}

/// How a stroke ends an open sub-path.
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
pub enum LineCap {
    /// Stop square on the end point (SVG's initial value).
    #[default]
    Butt,
    /// A half-disc centred on the end point.
    Round,
    /// A square extending half the stroke width past the end point.
    Square,
}

/// How a stroke turns a corner.
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
pub enum LineJoin {
    /// Extend both edges to their intersection, falling back to a bevel when
    /// that spike would exceed the miter limit (SVG's initial value).
    #[default]
    Miter,
    /// An arc of the stroke's half-width about the corner.
    Round,
    /// A straight cut across the corner.
    Bevel,
}

/// Everything that decides the area a stroke covers.
///
/// Widths and dash lengths are in the element's own user units, so the style
/// is applied *before* the element's transform, exactly as SVG specifies.
#[derive(Clone, Debug, PartialEq)]
pub struct StrokeStyle {
    /// The stroke width in user units. A non-positive width draws nothing.
    pub width: f64,
    /// How open ends are finished.
    pub cap: LineCap,
    /// How corners are turned.
    pub join: LineJoin,
    /// The ratio of miter length to stroke width past which a miter join
    /// degrades to a bevel.
    pub miter_limit: f64,
    /// The dash pattern in user units: alternating on and off lengths. Empty
    /// means a solid stroke.
    pub dashes: Vec<f64>,
    /// How far into the dash pattern the stroke starts.
    pub dash_offset: f64,
}

impl Default for StrokeStyle {
    /// SVG's initial stroke values: a solid, butt-capped, miter-joined
    /// hairline of one user unit.
    fn default() -> Self {
        Self {
            width: 1.0,
            cap: LineCap::default(),
            join: LineJoin::default(),
            miter_limit: 4.0,
            dashes: Vec::new(),
            dash_offset: 0.0,
        }
    }
}

/// The axis-aligned bounds of `subpaths`, or `None` when they hold no point.
///
/// This is SVG's *object bounding box*: the frame a gradient in
/// `objectBoundingBox` units is resolved against, and it deliberately ignores
/// stroke width, exactly as the specification defines it.
#[must_use]
pub fn bounds(subpaths: &[SubPath]) -> Option<(Point, Point)> {
    let mut min = (f64::MAX, f64::MAX);
    let mut max = (f64::MIN, f64::MIN);
    let mut seen = false;
    for point in subpaths.iter().flat_map(|sub| sub.points.iter()) {
        seen = true;
        min = (min.0.min(point.0), min.1.min(point.1));
        max = (max.0.max(point.0), max.1.max(point.1));
    }
    seen.then_some((min, max))
}
