//! The vectorised icon representation and its rasteriser.
//!
//! A [`VectorIcon`] is an ordered stack of filled [`IconLayer`]s over a
//! square design grid. Layers are painted bottom-first, each composited
//! *over* the ones below through `lib/raster`'s single premultiplied-alpha
//! polygon path, so a multi-part glyph (a battery body plus its terminal, a
//! bell plus its clapper) is built by stacking layers rather than by a
//! second multi-contour scan converter.

use alloc::vec::Vec;

use tairix_raster::{Color, Surface};

/// One filled layer of an icon: a fill colour and a single polygon ring in
/// design-grid coordinates.
///
/// Fewer than three vertices covers no area and [`Surface::fill_polygon`]
/// skips it rather than rejecting the whole icon.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IconLayer {
    /// The straight-alpha fill colour of this layer.
    pub fill: Color,
    /// The polygon outline, as `(x, y)` design-grid coordinate pairs.
    pub polygon: Vec<(i32, i32)>,
}

impl IconLayer {
    /// Build a layer from a fill colour and a static slice of design-grid
    /// coordinate pairs, so the built-in glyphs read as self-documenting
    /// coordinate tables.
    #[must_use]
    pub fn from_points(fill: Color, points: &[(i32, i32)]) -> Self {
        Self {
            fill,
            polygon: points.to_vec(),
        }
    }
}

/// A scalable, themeable icon: an ordered stack of filled [`IconLayer`]s over
/// a square design grid.
///
/// The design grid is `design` units on each side. An icon with no layers is
/// legal (it rasterises to a fully transparent image); a degenerate `design`
/// of zero is handled by the rasteriser rather than panicking.
#[derive(Clone, Debug, PartialEq, Eq)]
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
    /// icon rather than crashing. Each layer is filled
    /// through the shared [`Surface::fill_polygon`] path in stack order.
    #[must_use]
    pub fn rasterise(&self, side: u32) -> Option<Surface> {
        if side == 0 {
            return None;
        }
        let mut surface = Surface::new(side, side)?;
        self.draw_onto(&mut surface);
        Some(surface)
    }

    /// Draw the icon's layers onto an existing square `surface`, mapping the
    /// design grid across the whole surface. Used by [`rasterise`] and by a
    /// caller that wants to composite the glyph into a buffer it already
    /// owns.
    ///
    /// [`rasterise`]: Self::rasterise
    pub fn draw_onto(&self, surface: &mut Surface) {
        for layer in &self.layers {
            surface.fill_polygon(&layer.polygon, self.design, layer.fill);
        }
    }
}
