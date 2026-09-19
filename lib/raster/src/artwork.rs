//! The shared vector-artwork form every filled drawing reaches the scan
//! converter as.
//!
//! A cursor, an icon glyph, and a decoded SVG document are the same thing: an
//! ordered stack of filled [`Layer`]s over a square design grid, painted
//! bottom first. The form lives here, beside the scan converter and the
//! [`Paint`] and [`FillRule`] it already owns, so the decoder that builds one
//! and the three surfaces that draw one share a single definition and a
//! single walk of it.
//!
//! # Groups are what compositing needs
//!
//! Three of SVG's features — group opacity, clipping, and masking — all ask
//! for a subtree to be drawn *as a unit* and then composited through a
//! per-pixel factor, so [`Group`] is the one answer to all three. A clip is a
//! [`Mask`] whose content is the clip's shapes filled opaque white and read
//! as [`MaskKind::Alpha`]; a `<mask>` is the same group read as
//! [`MaskKind::Luminance`].
//!
//! Isolation is not cosmetic. Weakening each shape and compositing is a
//! different picture from compositing and then weakening — two overlapping
//! opaque shapes at half opacity show the lower one through the upper only in
//! the first — so a group really is rendered into its own buffer. A producer
//! therefore emits one only where it changes the picture; see
//! [`Surface::draw_artwork`](crate::Surface::draw_artwork) for what drawing
//! one costs.

use alloc::vec::Vec;

use crate::color::{Color, Pixel};
use crate::paint::Paint;
use crate::scan::FillRule;

/// The deepest nesting of [`Group`]s a renderer will descend.
///
/// A fixed containment bound, not a capacity: each level in flight holds one
/// full-extent isolation buffer (two under a mask), and the recursion is on
/// the stack. Artwork nests a handful of groups; a tree past this is refused
/// rather than allocated for.
pub const MAX_GROUP_DEPTH: usize = 8;

/// One filled layer: what it is painted with, which points it encloses, and
/// the contours that bound them, in design-grid coordinates.
///
/// A layer holds *several* contours rather than one ring because a single
/// shape often is several: a path with a hole, a multi-part sub-path, and any
/// stroke outline at all (which is the union of a piece per segment, cap, and
/// join). They are filled together, under one rule, so the pieces merge or
/// cancel as the rule says instead of being composited over one another.
///
/// A contour of fewer than three vertices covers no area and the scan
/// converter skips it rather than rejecting the drawing.
#[derive(Clone, Debug, PartialEq)]
pub struct Layer {
    /// What this layer is painted with.
    pub paint: Paint,
    /// Which points the contours enclose.
    pub rule: FillRule,
    /// The contours, each a ring of `(x, y)` design-grid coordinate pairs.
    pub contours: Vec<Vec<(i32, i32)>>,
}

impl Layer {
    /// Build a layer from a paint, a fill rule, and its contours.
    #[must_use]
    pub const fn filled(paint: Paint, rule: FillRule, contours: Vec<Vec<(i32, i32)>>) -> Self {
        Self {
            paint,
            rule,
            contours,
        }
    }

    /// Build a layer from a fill colour and one static slice of design-grid
    /// coordinate pairs, so hand-authored artwork reads as a self-documenting
    /// coordinate table.
    ///
    /// One ring filled even-odd, so an outline that crosses itself behaves as
    /// its author drew it.
    #[must_use]
    pub fn from_points(fill: Color, points: &[(i32, i32)]) -> Self {
        Self {
            paint: Paint::Solid(fill),
            rule: FillRule::EvenOdd,
            contours: alloc::vec![points.to_vec()],
        }
    }

    /// The total number of vertices across every contour.
    #[must_use]
    pub fn vertices(&self) -> usize {
        self.contours.iter().map(Vec::len).sum()
    }
}

/// One step of a drawing: a filled layer, or a subtree composited as a unit.
#[derive(Clone, Debug, PartialEq)]
pub enum Node {
    /// Paint one layer over what is already there.
    Fill(Layer),
    /// Draw a subtree into its own buffer, then composite that buffer.
    Group(Group),
}

/// A subtree composited as a unit, through an opacity and an optional mask.
///
/// A group with full opacity and no mask draws exactly as its children do in
/// place, so a producer splices those children into the enclosing list rather
/// than paying for an isolation buffer that changes nothing.
#[derive(Clone, Debug, PartialEq)]
pub struct Group {
    /// How much of the composited subtree reaches the destination, `0` (none)
    /// to `255` (all).
    pub opacity: u8,
    /// The per-pixel factor the subtree is weakened by, if any.
    pub mask: Option<Mask>,
    /// What the group draws, bottom first.
    pub children: Vec<Node>,
}

/// A per-pixel factor a [`Group`] is composited through: artwork of its own,
/// read as coverage.
#[derive(Clone, Debug, PartialEq)]
pub struct Mask {
    /// Which channel of the rendered content is the factor.
    pub kind: MaskKind,
    /// The artwork whose pixels are the factor, bottom first.
    pub content: Vec<Node>,
}

/// Which part of a rendered [`Mask`] is its per-pixel factor.
///
/// Neither is *the* default: a clip is always an alpha mask and a `<mask>`
/// element's own initial value is a luminance one, so each producer states
/// which it means.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum MaskKind {
    /// The content's alpha alone. A clip is this: opaque where the clip
    /// shapes cover, transparent elsewhere, and a union of shapes needs no
    /// second rule because opaque over opaque is opaque.
    Alpha,
    /// The content's relative luminance, which already carries its alpha
    /// because the pixels are premultiplied.
    Luminance,
}

impl MaskKind {
    /// The factor one rendered mask pixel contributes.
    #[must_use]
    pub(crate) fn factor(self, pixel: Pixel) -> u8 {
        match self {
            Self::Alpha => pixel.a,
            // sRGB relative luminance, as CSS Masking defines a luminance
            // mask — deliberately not the perceptual weights a greyscale
            // conversion uses. Premultiplied channels never exceed alpha, so
            // neither does their weighted mean.
            Self::Luminance => {
                let sum =
                    54 * u32::from(pixel.r) + 183 * u32::from(pixel.g) + 19 * u32::from(pixel.b);
                u8::try_from((sum + 128) >> 8).unwrap_or(u8::MAX)
            }
        }
    }
}

/// Hand every filled layer of `nodes` to `visit`, in the order they draw.
///
/// A group's own mask content is visited after its children, and nesting past
/// [`MAX_GROUP_DEPTH`] is not visited at all, because it is not drawn either.
pub fn for_each_fill(nodes: &[Node], visit: &mut impl FnMut(&Layer)) {
    walk(nodes, 0, visit);
}

fn walk(nodes: &[Node], depth: usize, visit: &mut impl FnMut(&Layer)) {
    if depth >= MAX_GROUP_DEPTH {
        return;
    }
    for node in nodes {
        match node {
            Node::Fill(layer) => visit(layer),
            Node::Group(group) => {
                walk(&group.children, depth + 1, visit);
                if let Some(mask) = &group.mask {
                    walk(&mask.content, depth + 1, visit);
                }
            }
        }
    }
}

/// How many filled layers `nodes` draws in total, counting into groups.
///
/// This is what a caller sizes a seam-resolving supersample from
/// ([`Surface::layered`](crate::Surface::layered)): the question is how many
/// edges can meet, which groups do not change.
#[must_use]
pub fn layer_count(nodes: &[Node]) -> usize {
    let mut count = 0;
    for_each_fill(nodes, &mut |_| count += 1);
    count
}

#[cfg(test)]
#[path = "artwork_tests.rs"]
mod tests;
