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

use tairix_raster::Affine;
use tairix_util::mathf::{hypot, round_i32};

use crate::error::SvgError;

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

    /// This contour with every point mapped through `transform`.
    ///
    /// Closure is a property of the contour rather than of the space it sits
    /// in, so it comes across untouched.
    #[must_use]
    pub fn mapped(&self, transform: Affine) -> Self {
        Self {
            points: self
                .points
                .iter()
                .map(|point| transform.apply(*point))
                .collect(),
            closed: self.closed,
        }
    }
}

/// One marker position on a shape, and which way the path runs through it.
///
/// The directions are unit vectors in the shape's own user space. Either is
/// absent where no segment lies on that side, or where the one that does has
/// no length — a vertex with neither has no direction to orient a marker by.
#[derive(Copy, Clone, Debug, Default, PartialEq)]
pub struct Vertex {
    /// Where the marker sits.
    pub at: Point,
    /// The direction the path arrives in.
    pub incoming: Option<Point>,
    /// The direction the path leaves in.
    pub outgoing: Option<Point>,
}

impl Vertex {
    /// This vertex as another space sees it: the point mapped through
    /// `transform`, each direction through its linear part and re-normalised.
    ///
    /// A direction is carried by the linear part alone — a translation moves
    /// the vertex, never the way the path runs through it — and needs
    /// re-normalising because a scale or shear changes its length. One the
    /// map collapses is dropped, which is the same "no direction here" a
    /// zero-length segment already states.
    #[must_use]
    pub fn mapped(&self, transform: Affine) -> Self {
        let direct = |vector: Option<Point>| {
            let (x, y) = vector?;
            normalise((
                transform.a * x + transform.c * y,
                transform.b * x + transform.d * y,
            ))
        };
        Self {
            at: transform.apply(self.at),
            incoming: direct(self.incoming),
            outgoing: direct(self.outgoing),
        }
    }
}

/// The sub-path being accumulated, so closing it can join its two ends.
struct Open {
    /// Which vertex began it.
    first: usize,
    /// Where it began.
    at: Point,
}

/// A shape's marker vertices, accumulated as its commands are read.
///
/// Markers sit at the vertices the *author* wrote, turned by the path's true
/// direction there, and flattening throws both away — so they are taken while
/// the command structure is still live. A caller that draws markers hands one
/// of these to the parser; one that does not passes nothing and pays a branch
/// per command.
pub struct Vertices {
    list: Vec<Vertex>,
    open: Option<Open>,
    limit: usize,
}

impl Vertices {
    /// A collector holding at most `limit` vertices.
    ///
    /// A security bound rather than a capacity, and deliberately the
    /// conservative one: an instance costs at least one element visit, so
    /// bounding the list by the visits left keeps a hostile path's transient
    /// vertex list to the same order as the geometry it accompanies.
    #[must_use]
    pub const fn new(limit: usize) -> Self {
        Self {
            list: Vec::new(),
            open: None,
            limit,
        }
    }

    /// Begin a sub-path at `at`.
    ///
    /// # Errors
    /// Returns [`SvgError::TooComplex`] once the limit is reached.
    pub fn move_to(&mut self, at: Point) -> Result<(), SvgError> {
        self.open = Some(Open {
            first: self.list.len(),
            at,
        });
        self.push(at, None)
    }

    /// Extend the sub-path to `to` along a straight segment.
    ///
    /// # Errors
    /// Returns [`SvgError::TooComplex`] once the limit is reached.
    pub fn line_to(&mut self, to: Point) -> Result<(), SvgError> {
        let direction = match self.list.last() {
            Some(previous) => delta(previous.at, to),
            None => return self.move_to(to),
        };
        self.curve_to(to, direction, direction)
    }

    /// Extend the sub-path to `to` along a segment that leaves and arrives in
    /// different directions.
    ///
    /// # Errors
    /// Returns [`SvgError::TooComplex`] once the limit is reached.
    pub fn curve_to(&mut self, to: Point, leaving: Point, arriving: Point) -> Result<(), SvgError> {
        let Some(previous) = self.list.last_mut() else {
            // Nothing precedes the segment, so it begins the sub-path.
            return self.move_to(to);
        };
        previous.outgoing = normalise(leaving);
        self.push(to, normalise(arriving))
    }

    /// Close the sub-path with a straight segment back to where it began.
    ///
    /// # Errors
    /// Returns [`SvgError::TooComplex`] once the limit is reached.
    pub fn close(&mut self) -> Result<(), SvgError> {
        let Some(open) = self.open.as_ref() else {
            return Ok(());
        };
        // A sub-path of one point has no two ends to join, and a closepath
        // costs no point of the path budget, so letting a run of them each add
        // a vertex would let a free command allocate without end.
        let (first, start) = (open.first, open.at);
        if self.list.len() <= first + 1 {
            return Ok(());
        }
        let Some(end) = self.list.last_mut() else {
            return Ok(());
        };
        let direction = normalise(delta(end.at, start));
        end.outgoing = direction;
        self.push(start, direction)?;

        // The two ends are one point on one closed curve, so the turn there
        // reads the same from either: in along the closing segment, out along
        // the sub-path's first. A segment that follows without a moveto
        // overwrites the latter, which is where the pen actually goes next.
        let mut leaving = None;
        if let Some(vertex) = self.list.get_mut(first) {
            leaving = vertex.outgoing;
            vertex.incoming = direction;
        }
        if let Some(end) = self.list.last_mut() {
            end.outgoing = leaving;
        }
        self.open = Some(Open {
            first: self.list.len() - 1,
            at: start,
        });
        Ok(())
    }

    /// The vertices, in path order.
    #[must_use]
    pub fn finish(self) -> Vec<Vertex> {
        self.list
    }

    /// Append one vertex, charging it against the limit.
    fn push(&mut self, at: Point, incoming: Option<Point>) -> Result<(), SvgError> {
        if self.list.len() >= self.limit {
            return Err(SvgError::TooComplex);
        }
        self.list.push(Vertex {
            at,
            incoming,
            outgoing: None,
        });
        Ok(())
    }
}

/// The vector from `from` to `to`.
pub(crate) fn delta(from: Point, to: Point) -> Point {
    (to.0 - from.0, to.1 - from.1)
}

/// `vector` scaled to unit length, or `None` when it has no length to scale —
/// which is how a coincident pair or a zero-length segment states that it
/// gives no direction.
pub(crate) fn normalise(vector: Point) -> Option<Point> {
    let length = hypot(vector.0, vector.1);
    (length > 0.0 && length.is_finite()).then(|| (vector.0 / length, vector.1 / length))
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

/// `subpaths` mapped through `to_design` onto an integer design grid, the
/// contour form a filled layer holds, dropping any that enclose no area.
#[must_use]
pub fn place(subpaths: &[SubPath], to_design: Affine) -> Vec<Vec<(i32, i32)>> {
    subpaths
        .iter()
        .filter(|sub| !sub.is_degenerate())
        .map(|sub| {
            sub.points
                .iter()
                .map(|point| {
                    let placed = to_design.apply(*point);
                    (round_i32(placed.0), round_i32(placed.1))
                })
                .collect()
        })
        .collect()
}

#[cfg(test)]
#[path = "geom_tests.rs"]
mod tests;
