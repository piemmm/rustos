//! The vectorised icon representation and its rasteriser.
//!
//! A [`VectorIcon`] is artwork over a square design grid: filled
//! [`IconLayer`]s painted bottom-first, each composited *over* the ones below
//! through `lib/raster`'s single premultiplied-alpha scan converter, so a
//! multi-part glyph (a battery body plus its terminal, a bell plus its
//! clapper) is built by stacking layers.
//!
//! A layer is several contours under one fill rule rather than a single ring,
//! because artwork decoded from SVG needs shapes with holes and stroke
//! outlines, both of which are many rings filled as one. A built-in glyph is
//! a flat stack; a decoded document may also carry groups, where a clip, a
//! mask, or a group opacity composites part of the drawing as a unit.

use alloc::vec::Vec;

use tairix_raster::{layer_count, Node, Surface};

/// One filled layer of an icon: what it is painted with, which points it
/// encloses, and the contours that bound them, in design-grid coordinates.
///
/// The shared artwork layer, so a glyph built here and a document decoded by
/// `lib/svg` are the same thing to the rasteriser.
pub use tairix_raster::Layer as IconLayer;

/// A scalable, themeable icon: artwork over a square design grid.
///
/// The design grid is `design` units on each side. An icon with no artwork is
/// legal (it rasterises to a fully transparent image); a degenerate `design`
/// of zero is handled by the rasteriser rather than panicking.
#[derive(Clone, Debug, PartialEq)]
pub struct VectorIcon {
    design: u32,
    nodes: Vec<Node>,
}

impl VectorIcon {
    /// Construct an icon from its design-grid side and a flat layer stack
    /// (bottom layer first), which is what a built-in glyph is.
    #[must_use]
    pub fn new(design: u32, layers: Vec<IconLayer>) -> Self {
        Self::from_artwork(design, layers.into_iter().map(Node::Fill).collect())
    }

    /// Construct an icon from its design-grid side and artwork that may carry
    /// composited groups, which is what a decoded document is.
    #[must_use]
    pub const fn from_artwork(design: u32, nodes: Vec<Node>) -> Self {
        Self { design, nodes }
    }

    /// The side length of the square design grid, in design units.
    #[must_use]
    pub const fn design(&self) -> u32 {
        self.design
    }

    /// The artwork, bottom first.
    #[must_use]
    pub fn nodes(&self) -> &[Node] {
        &self.nodes
    }

    /// Rasterise this icon into a fresh `side`×`side` [`Surface`], transparent
    /// everywhere the glyph does not draw.
    ///
    /// Returns `None` for a zero `side`, if the pixel buffer cannot be
    /// allocated, or if a group's isolation buffer cannot be — so the caller
    /// falls back to a smaller size or omits the icon rather than showing a
    /// half-composited one. The artwork goes through the shared
    /// [`Surface::draw_artwork`] path, and the stack through
    /// [`Surface::layered`] so a shape's stroke meets its fill — and one part
    /// of a glyph its neighbour — without the pale seam that compositing
    /// already-anti-aliased layers leaves.
    #[must_use]
    pub fn rasterise(&self, side: u32) -> Option<Surface> {
        if side == 0 {
            return None;
        }
        let mut drawn = false;
        let surface = Surface::layered(side, side, layer_count(&self.nodes), |surface| {
            drawn = surface.draw_artwork(&self.nodes, self.design);
        })?;
        drawn.then_some(surface)
    }
}
