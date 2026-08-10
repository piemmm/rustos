//! The vectorised icon representation and its rasteriser.
//!
//! A [`VectorIcon`] is an ordered stack of filled [`IconLayer`]s over a
//! square design grid. Layers are painted bottom-first, each composited
//! *over* the ones below through `lib/raster`'s single premultiplied-alpha
//! scan converter, so a multi-part glyph (a battery body plus its terminal,
//! a bell plus its clapper) is built by stacking layers.
//!
//! A layer is several contours under one fill rule rather than a single ring,
//! because artwork decoded from SVG needs shapes with holes and stroke
//! outlines, both of which are many rings filled as one.

use alloc::vec::Vec;

use tairix_raster::{Color, FillRule, Paint, Surface};

/// One filled layer of an icon: what it is painted with, which points it
/// encloses, and the contours that bound them, in design-grid coordinates.
///
/// A contour of fewer than three vertices covers no area and the scan
/// converter skips it rather than rejecting the whole icon.
#[derive(Clone, Debug, PartialEq)]
pub struct IconLayer {
    /// What this layer is painted with.
    pub paint: Paint,
    /// Which points the contours enclose.
    pub rule: FillRule,
    /// The contours, each a ring of `(x, y)` design-grid coordinate pairs.
    pub contours: Vec<Vec<(i32, i32)>>,
}

impl IconLayer {
    /// Build a layer from a fill colour and a static slice of design-grid
    /// coordinate pairs, so the built-in glyphs read as self-documenting
    /// coordinate tables.
    ///
    /// One ring filled even-odd, so a built-in glyph whose outline crosses
    /// itself behaves as its author drew it.
    #[must_use]
    pub fn from_points(fill: Color, points: &[(i32, i32)]) -> Self {
        Self {
            paint: Paint::Solid(fill),
            rule: FillRule::EvenOdd,
            contours: alloc::vec![points.to_vec()],
        }
    }

    /// Build a layer from artwork that is several contours, a fill rule, and
    /// a paint of its own.
    #[must_use]
    pub fn filled(paint: Paint, rule: FillRule, contours: Vec<Vec<(i32, i32)>>) -> Self {
        Self {
            paint,
            rule,
            contours,
        }
    }
}

/// A scalable, themeable icon: an ordered stack of filled [`IconLayer`]s over
/// a square design grid.
///
/// The design grid is `design` units on each side. An icon with no layers is
/// legal (it rasterises to a fully transparent image); a degenerate `design`
/// of zero is handled by the rasteriser rather than panicking.
#[derive(Clone, Debug, PartialEq)]
pub struct VectorIcon {
    design: u32,
    layers: Vec<IconLayer>,
}

impl VectorIcon {
    /// Construct an icon from its design-grid side and layer stack (bottom
    /// layer first).
    #[must_use]
    pub fn new(design: u32, layers: Vec<IconLayer>) -> Self {
        Self { design, layers }
    }

    /// The side length of the square design grid, in design units.
    #[must_use]
    pub const fn design(&self) -> u32 {
        self.design
    }

    /// The layer stack, bottom layer first.
    #[must_use]
    pub fn layers(&self) -> &[IconLayer] {
        &self.layers
    }

    /// Rasterise this icon into a fresh `side`×`side` [`Surface`], transparent
    /// everywhere the glyph does not draw.
    ///
    /// Returns `None` for a zero `side` or if the pixel buffer cannot be
    /// allocated, so the caller falls back to a smaller size or omits the
    /// icon rather than crashing. Each layer is filled through the shared
    /// [`Surface::fill_contours`] path in stack order, and the stack goes
    /// through [`Surface::layered`] so a shape's stroke meets its fill — and
    /// one part of a glyph its neighbour — without the pale seam that
    /// compositing already-anti-aliased layers leaves.
    #[must_use]
    pub fn rasterise(&self, side: u32) -> Option<Surface> {
        if side == 0 {
            return None;
        }
        Surface::layered(side, side, self.layers.len(), |surface| {
            for layer in &self.layers {
                surface.fill_contours(&layer.contours, self.design, layer.rule, &layer.paint);
            }
        })
    }
}
