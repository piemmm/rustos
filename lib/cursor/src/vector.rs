//! The vectorised cursor representation.
//!
//! A cursor is not a fixed-resolution bitmap mask: it is a small stack of
//! filled [`Shape`]s over a square design grid, so the same definition
//! rasterises crisply at any scale ([`VectorCursor::rasterise`]) and carries
//! real colour and alpha rather than a single foreground bit. This is what
//! makes the desktop's cursors "richer than a fill mask" (`PLAN.md` Stage 7):
//! colourful, scalable, and — being pure geometry — replaceable with an
//! entirely different cursor set without touching the window manager.
//!
//! Shapes are painted in order, each composited *over* the ones below it
//! through `lib/raster`'s single premultiplied-alpha path, so a cursor can
//! layer a dark outline beneath a light body and stay legible over any
//! background (no colour arithmetic is duplicated here). A built-in cursor is
//! a flat stack; a decoded SVG one may also carry groups, where a clip, a
//! mask, or a group opacity composites part of the artwork as a unit.

use alloc::vec::Vec;

use tairix_raster::Node;

/// One filled, colourful layer of a cursor: what it is painted with, which
/// points it encloses, and the contours that bound them, in design-grid
/// coordinates.
///
/// The shared artwork layer, so a built-in cursor and a document decoded by
/// `lib/svg` are the same thing to the rasteriser. A layer is several
/// contours under one fill rule rather than a single ring, because artwork
/// decoded from SVG needs shapes with holes and stroke outlines, both of
/// which are many rings filled as one.
pub use tairix_raster::Layer as Shape;

/// A complete cursor: a hotspot and filled artwork over a square design grid.
///
/// The design grid is `design_size` units on each side. The **hotspot** —
/// the single design-grid point that tracks the pointer position — is held
/// in the same units, so it scales with the artwork. A cursor with no
/// artwork is legal (it rasterises to a fully transparent image); a
/// degenerate `design_size` of zero is not renderable and the rasteriser
/// reports that by returning `None` rather than panicking.
#[derive(Clone, Debug, PartialEq)]
pub struct VectorCursor {
    design_size: u32,
    hotspot_x: i32,
    hotspot_y: i32,
    nodes: Vec<Node>,
}

impl VectorCursor {
    /// Construct a cursor from its design-grid side, hotspot, and a flat
    /// shape stack (bottom shape first), which is what a built-in cursor is.
    #[must_use]
    pub fn new(design_size: u32, hotspot_x: i32, hotspot_y: i32, shapes: Vec<Shape>) -> Self {
        Self::from_artwork(
            design_size,
            hotspot_x,
            hotspot_y,
            shapes.into_iter().map(Node::Fill).collect(),
        )
    }

    /// Construct a cursor from artwork that may carry composited groups,
    /// which is what a decoded document is.
    #[must_use]
    pub const fn from_artwork(
        design_size: u32,
        hotspot_x: i32,
        hotspot_y: i32,
        nodes: Vec<Node>,
    ) -> Self {
        Self {
            design_size,
            hotspot_x,
            hotspot_y,
            nodes,
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

    /// The artwork, bottom first.
    #[must_use]
    pub fn nodes(&self) -> &[Node] {
        &self.nodes
    }
}
