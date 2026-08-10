//! The vectorised cursor representation.
//!
//! A cursor is not a fixed-resolution bitmap mask: it is a small ordered
//! stack of filled [`Shape`]s over a square design grid, so the same
//! definition rasterises crisply at any scale ([`VectorCursor::rasterise`])
//! and carries real colour and alpha rather than a single foreground bit.
//! This is what makes the desktop's cursors "richer than a fill mask"
//! (`PLAN.md` Stage 7): colourful, scalable, and — being pure geometry —
//! replaceable with an entirely different cursor set without touching the
//! window manager.
//!
//! Shapes are painted in order, each composited *over* the ones below it
//! through `lib/raster`'s single premultiplied-alpha path, so a cursor can
//! layer a dark outline beneath a light body and stay legible over any
//! background (no colour arithmetic is duplicated here).

use alloc::vec::Vec;

use tairix_raster::{Color, FillRule, Paint};

/// A vertex in a cursor's design grid.
///
/// Coordinates are signed design units measured from the top-left of the
/// square design box (`0..design_size`). They are resolution-independent:
/// the rasteriser maps them to output pixels at the requested scale.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct Vertex {
    /// Horizontal position in design units.
    pub x: i32,
    /// Vertical position in design units.
    pub y: i32,
}

impl Vertex {
    /// Construct a vertex.
    #[must_use]
    pub const fn new(x: i32, y: i32) -> Self {
        Self { x, y }
    }
}

/// One filled, colourful layer of a cursor: what it is painted with, which
/// points it encloses, and the contours that bound them.
///
/// A layer is several contours under one fill rule rather than a single ring,
/// because artwork decoded from SVG needs shapes with holes and stroke
/// outlines, both of which are many rings filled as one. A built-in cursor
/// still writes one ring and gets the even-odd rule, so a ring that crosses
/// itself behaves as its designer drew it. Fewer than three vertices covers
/// no area and is skipped rather than rejected.
#[derive(Clone, Debug, PartialEq)]
pub struct Shape {
    /// What this layer is painted with.
    pub paint: Paint,
    /// Which points the contours enclose.
    pub rule: FillRule,
    /// The contours, each a ring in design-grid [`Vertex`] order.
    pub contours: Vec<Vec<Vertex>>,
}

impl Shape {
    /// Construct a filled shape from a fill colour and a single polygon.
    #[must_use]
    pub fn new(fill: Color, polygon: Vec<Vertex>) -> Self {
        Self {
            paint: Paint::Solid(fill),
            rule: FillRule::EvenOdd,
            contours: alloc::vec![polygon],
        }
    }

    /// Build a shape from a fill colour and a static slice of `(x, y)`
    /// design-grid coordinate pairs. Used by the built-in cursor set so its
    /// geometry reads as self-documenting coordinate tables.
    #[must_use]
    pub fn from_points(fill: Color, points: &[(i32, i32)]) -> Self {
        Self::new(
            fill,
            points.iter().map(|&(x, y)| Vertex::new(x, y)).collect(),
        )
    }

    /// Build a shape from artwork that is several contours, a fill rule, and
    /// a paint of its own.
    #[must_use]
    pub fn filled(paint: Paint, rule: FillRule, contours: Vec<Vec<Vertex>>) -> Self {
        Self {
            paint,
            rule,
            contours,
        }
    }
}

/// A complete cursor: a hotspot and an ordered stack of filled [`Shape`]s
/// over a square design grid.
///
/// The design grid is `design_size` units on each side. The **hotspot** —
/// the single design-grid point that tracks the pointer position — is held
/// in the same units, so it scales with the artwork. A cursor with no
/// shapes is legal (it rasterises to a fully transparent image); a
/// degenerate `design_size` of zero is not renderable and the rasteriser
/// reports that by returning `None` rather than panicking.
#[derive(Clone, Debug, PartialEq)]
pub struct VectorCursor {
    design_size: u32,
    hotspot_x: i32,
    hotspot_y: i32,
    shapes: Vec<Shape>,
}

impl VectorCursor {
    /// Construct a cursor from its design-grid side, hotspot, and shape
    /// stack (bottom shape first).
    #[must_use]
    pub fn new(design_size: u32, hotspot_x: i32, hotspot_y: i32, shapes: Vec<Shape>) -> Self {
        Self {
            design_size,
            hotspot_x,
            hotspot_y,
            shapes,
        }
    }

    /// The side length of the square design grid, in design units.
    #[must_use]
    pub const fn design_size(&self) -> u32 {
        self.design_size
    }

    /// The hotspot x-coordinate in design units.
    #[must_use]
    pub const fn hotspot_x(&self) -> i32 {
        self.hotspot_x
    }

    /// The hotspot y-coordinate in design units.
    #[must_use]
    pub const fn hotspot_y(&self) -> i32 {
        self.hotspot_y
    }

    /// The shape stack, bottom layer first.
    #[must_use]
    pub fn shapes(&self) -> &[Shape] {
        &self.shapes
    }
}
