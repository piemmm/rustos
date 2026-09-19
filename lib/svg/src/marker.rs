//! `<marker>`: where one marker instance sits on a shape, and which way it
//! faces.
//!
//! A marker is a drawing placed at a shape's command vertices and turned to
//! follow the path through each. Everything the element says about that —
//! `refX`/`refY`, `markerWidth`/`markerHeight`, `markerUnits`, `orient`, and
//! its own `viewBox`/`preserveAspectRatio` — collapses into one [`Affine`] per
//! instance, exactly as a pattern's units and transform collapse into one map,
//! so the walk that draws an instance has no cases to take.
//!
//! The element is read once per shape; only the matrix is per vertex.

use core::f64::consts::TAU;

use tairix_raster::Affine;
use tairix_util::mathf::{cos, sin};

use crate::error::SvgError;
use crate::geom::{normalise, Point, Vertex};
use crate::number::{parse_length, parse_number};
use crate::transform::{parse_aspect_ratio, parse_view_box, viewport_transform, AspectRatio};
use crate::xml::Element;

/// The marker viewport extent SVG assumes when the element states none.
const DEFAULT_EXTENT: f64 = 3.0;

/// Which of the three marker properties placed an instance.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum Position {
    /// The path's first vertex.
    Start,
    /// A vertex that is neither the first nor the last.
    Mid,
    /// The path's last vertex.
    End,
}

/// Where one instance sits, in the shape's own user space.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct Placement {
    /// The marker viewport, which is the rectangle its `overflow` clips to.
    pub viewport: Affine,
    /// The content's own coordinates.
    pub content: Affine,
}

/// How an instance is turned to face the path.
#[derive(Copy, Clone, Debug, PartialEq)]
enum Orient {
    /// A direction the element states outright, as a unit vector.
    Fixed(Point),
    /// The path's own direction at the vertex.
    Auto,
    /// [`Auto`](Self::Auto), reversed at a `marker-start` — which is what
    /// lets one arrowhead point out of both ends of a path.
    AutoStartReverse,
}

/// A `<marker>` element read once, ready to place at any number of vertices.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct Marker {
    /// The marker viewport, in marker units.
    pub viewport: (f64, f64),
    /// The viewport the content's own percentages resolve against.
    pub content_viewport: (f64, f64),
    /// Content coordinates onto the marker viewport.
    fit: Affine,
    /// The point aligned with the vertex, in viewport coordinates.
    reference: Point,
    orient: Orient,
    /// Whether the viewport is measured in the referencing element's stroke
    /// widths rather than its user units.
    stroke_units: bool,
}

impl Marker {
    /// Read a `<marker>` element, or `None` when it draws nothing.
    ///
    /// `viewport` is the referencing element's, which a percentage extent
    /// resolves against. A zero extent disables the marker, exactly as a zero
    /// tile disables a pattern; a negative one is an error.
    ///
    /// # Errors
    /// Returns the parse error of a malformed attribute.
    pub fn read(node: &Element<'_>, viewport: (f64, f64)) -> Result<Option<Self>, SvgError> {
        let width = extent(node, "markerWidth", viewport.0)?;
        let height = extent(node, "markerHeight", viewport.1)?;
        if width < 0.0 || height < 0.0 {
            return Err(SvgError::InvalidNumber);
        }
        if width == 0.0 || height == 0.0 {
            return Ok(None);
        }
        let (fit, content_viewport) = match node.attr("viewBox") {
            Some(text) => {
                let view_box = parse_view_box(text)?;
                let ratio = match node.attr("preserveAspectRatio") {
                    Some(spec) => parse_aspect_ratio(spec)?,
                    None => AspectRatio::default(),
                };
                (
                    viewport_transform(view_box, (width, height), ratio),
                    view_box.size,
                )
            }
            None => (Affine::IDENTITY, (width, height)),
        };
        // The reference point is stated in the content's own coordinates, and
        // what lands on the vertex is where the viewBox fit puts it.
        let reference = fit.apply((
            coordinate(node, "refX", content_viewport.0)?,
            coordinate(node, "refY", content_viewport.1)?,
        ));
        Ok(Some(Self {
            viewport: (width, height),
            content_viewport,
            fit,
            reference,
            orient: orient(node)?,
            stroke_units: !matches!(node.attr("markerUnits"), Some("userSpaceOnUse")),
        }))
    }

    /// Where this marker sits at `vertex`, or `None` when the placement
    /// collapses and there is nothing to draw.
    ///
    /// `stroke_width` is the referencing element's, which the default
    /// `markerUnits` measures the marker in — so a marker scales with the line
    /// it decorates whether or not that line's stroke is painted.
    #[must_use]
    pub fn place(
        &self,
        vertex: &Vertex,
        position: Position,
        stroke_width: f64,
    ) -> Option<Placement> {
        let scale = if self.stroke_units { stroke_width } else { 1.0 };
        if scale <= 0.0 || !scale.is_finite() {
            return None;
        }
        let viewport = Affine::translate(-self.reference.0, -self.reference.1)
            .then(Affine::scale(scale, scale))
            .then(facing(self.direction(vertex, position)))
            .then(Affine::translate(vertex.at.0, vertex.at.1));
        Some(Placement {
            viewport,
            content: self.fit.then(viewport),
        })
    }

    /// The unit direction this instance's positive x axis takes.
    fn direction(&self, vertex: &Vertex, position: Position) -> Point {
        match self.orient {
            Orient::Fixed(direction) => direction,
            Orient::AutoStartReverse if position == Position::Start => {
                let (x, y) = bisector(vertex);
                (-x, -y)
            }
            Orient::Auto | Orient::AutoStartReverse => bisector(vertex),
        }
    }
}

/// The unit direction the path runs in at `vertex`.
///
/// With a segment on each side it is the bisector of the two, taken as the
/// normalised sum of the unit vectors. An exact reversal has no bisector —
/// approached from either side the answer tends to one of the two
/// perpendiculars — so a perpendicular is taken rather than whatever finite
/// value the arithmetic would otherwise fall out with. One segment gives its
/// own direction, which is an open sub-path's end; none gives the positive x
/// axis, where an unoriented marker already points.
fn bisector(vertex: &Vertex) -> Point {
    match (vertex.incoming, vertex.outgoing) {
        (Some(arriving), Some(leaving)) => {
            normalise((arriving.0 + leaving.0, arriving.1 + leaving.1))
                .unwrap_or((-arriving.1, arriving.0))
        }
        (Some(only), None) | (None, Some(only)) => only,
        (None, None) => (1.0, 0.0),
    }
}

/// The rotation that takes the positive x axis onto the unit vector
/// `direction`.
///
/// Built from the vector rather than from an angle: every direction here is
/// already a unit vector, so this is exact and costs no trigonometry.
fn facing(direction: Point) -> Affine {
    Affine {
        a: direction.0,
        b: direction.1,
        c: -direction.1,
        d: direction.0,
        e: 0.0,
        f: 0.0,
    }
}

/// Parse an `orient` attribute.
fn orient(node: &Element<'_>) -> Result<Orient, SvgError> {
    match node.attr("orient").map(str::trim) {
        Some("auto") => Ok(Orient::Auto),
        Some("auto-start-reverse") => Ok(Orient::AutoStartReverse),
        Some(text) => {
            let radians = angle(text)?;
            // Normalised so a huge angle, whose sine and cosine no longer
            // square to one, still turns the marker rather than resizing it.
            Ok(Orient::Fixed(
                normalise((cos(radians), sin(radians))).unwrap_or((1.0, 0.0)),
            ))
        }
        None => Ok(Orient::Fixed((1.0, 0.0))),
    }
}

/// An `<angle>` in radians: a bare number is degrees, as SVG defines it.
fn angle(text: &str) -> Result<f64, SvgError> {
    // `grad` is tried before `rad`, which is a suffix of it.
    for (unit, whole_turn) in [("grad", 400.0), ("deg", 360.0), ("rad", TAU), ("turn", 1.0)] {
        if let Some(number) = text.strip_suffix(unit) {
            return Ok(parse_number(number)? / whole_turn * TAU);
        }
    }
    Ok(parse_number(text)? / 360.0 * TAU)
}

/// A marker viewport extent, defaulting to the three SVG assumes.
fn extent(node: &Element<'_>, name: &str, basis: f64) -> Result<f64, SvgError> {
    match node.attr(name) {
        Some(text) => parse_length(text, basis),
        None => Ok(DEFAULT_EXTENT),
    }
}

/// One reference-point coordinate, defaulting to the content's origin.
fn coordinate(node: &Element<'_>, name: &str, basis: f64) -> Result<f64, SvgError> {
    match node.attr(name) {
        Some(text) => parse_length(text, basis),
        None => Ok(0.0),
    }
}

#[cfg(test)]
#[path = "marker_tests.rs"]
mod tests;
