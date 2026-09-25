//! Putting a placed figure on a surface, and what that costs.
//!
//! The order is the whole of it, and it is a contract rather than a habit:
//! the contact shadow is on the ground and everything the figure is made of
//! stands on it, and the figure's own strips are already sorted far-first by
//! the placement. Re-sorting them here would undo the depth key that makes
//! one body frame serve every heading.
//!
//! There is one home for that order because there are two consumers — the
//! art harness that renders the contact sheets and the game client that
//! draws the same figure in a scene — and two copies of a paint order drift
//! the moment one of them grows a part.

use tairix_inline::ArrayVec;
use tairix_raster::shape::{fill, Placed, Scratch};
use tairix_raster::surface::{Canvas, SUBPIXEL};
use tairix_raster::{Color, ScanScratch};
use tairix_util::mathf;

use crate::mesh::MAX_RINGS;
use crate::rig::{Placement, Strip};

/// The most outline points one placed figure traces.
///
/// A budget on the scan converter's work, held against every figure of the
/// reference grid by the art harness: a rig that quietly doubled its parts
/// would be paying for it on every frame of every figure on screen.
pub const MAX_FIGURE_POINTS: u32 = 2048;

/// How many points one strip's closed outline has.
const OUTLINE: usize = MAX_RINGS * 2;

/// What drawing one placed figure costs.
///
/// Both halves of the scan converter's work: how many outline points it
/// transforms, and how much area it fills. The area is the sum over strips
/// rather than the union, so a pixel two strips cover counts twice — which
/// is the overdraw, and is exactly what the converter pays.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct Cost {
    /// How many outline points the whole figure traces.
    pub points: u32,
    /// The area, in surface pixels, filled across every strip.
    pub fill_area: f64,
}

/// The buffers a strip's outline is walked and scan-converted through, and a
/// shadow's shapes traced through.
///
/// Held by the caller across figures, so drawing a scene of them costs no
/// allocation at all once the buffers have grown to the largest part.
#[derive(Debug, Default)]
pub struct Brush {
    outline: ArrayVec<(i32, i32), OUTLINE>,
    scan: ScanScratch,
    shape: Scratch,
}

impl Brush {
    /// Empty buffers.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

/// A tone every stroke of a figure is mixed toward, and how far, out of 255:
/// the air between the figure and whoever is looking at it.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct Veil {
    /// The tone mixed toward.
    pub tone: Color,
    /// How far, out of 255.
    pub amount: u8,
}

impl Veil {
    /// Clear air: every stroke drawn as it was placed.
    pub const NONE: Self = Self {
        tone: Color::rgb(0, 0, 0),
        amount: 0,
    };

    /// `color` seen through this veil, its own alpha kept.
    #[must_use]
    pub fn over(self, color: Color) -> Color {
        if self.amount == 0 {
            return color;
        }
        let mix = |from: u8, to: u8| {
            let (from, to) = (i32::from(from), i32::from(to));
            let moved = from + (to - from) * i32::from(self.amount) / 255;
            u8::try_from(moved.clamp(0, 255)).unwrap_or(u8::MAX)
        };
        Color::rgba(
            mix(color.r, self.tone.r),
            mix(color.g, self.tone.g),
            mix(color.b, self.tone.b),
            color.a,
        )
    }
}

/// Draw `placement` onto `canvas` through `veil`, the rings of its contact
/// `shadow` first and outermost first.
///
/// A canvas is a whole surface or one band of one, so a figure drawn band by
/// band on several cores is exactly the figure drawn whole.
pub fn draw<C: Canvas + ?Sized>(
    canvas: &mut C,
    shadow: &[Placed],
    placement: &Placement,
    brush: &mut Brush,
    veil: Veil,
) {
    ground(canvas, shadow, brush, veil);
    figure(canvas, placement, brush, veil);
}

/// The first half of [`draw`]: the contact shadow's rings, outermost first.
///
/// Apart from the figure for a canvas that has to hold the two to different
/// rows — a figure wading is cut at the waterline, and its shadow on the
/// surface is not.
pub fn ground<C: Canvas + ?Sized>(
    canvas: &mut C,
    shadow: &[Placed],
    brush: &mut Brush,
    veil: Veil,
) {
    for ring in shadow {
        let seen = Placed {
            color: veil.over(ring.color),
            ..*ring
        };
        fill(canvas, &seen, &mut brush.shape);
    }
}

/// The second half of [`draw`]: the figure's own strips, far-first.
pub fn figure<C: Canvas + ?Sized>(
    canvas: &mut C,
    placement: &Placement,
    brush: &mut Brush,
    veil: Veil,
) {
    for strip in placement.strips() {
        walk(&strip, &mut brush.outline);
        canvas.fill_polygon_subpixel(&brush.outline, veil.over(strip.color), &mut brush.scan);
    }
}

/// What drawing `placement` would cost.
///
/// Measured off the outlines the painter would fill rather than estimated
/// from the parts' extents, so a part whose mesh grew more rings shows up
/// here.
#[must_use]
pub fn cost(placement: &Placement) -> Cost {
    let mut outline = ArrayVec::<(i32, i32), OUTLINE>::new();
    let mut points = 0u32;
    let mut area = 0.0;
    for strip in placement.strips() {
        walk(&strip, &mut outline);
        points = points.saturating_add(u32::try_from(outline.len()).unwrap_or(u32::MAX));
        area += enclosed(&outline);
    }
    Cost {
        points,
        fill_area: area,
    }
}

/// Walk `strip` into a closed outline in the scan converter's own units:
/// down one boundary and back up the other.
fn walk(strip: &Strip<'_>, out: &mut ArrayVec<(i32, i32), OUTLINE>) {
    out.clear();
    for point in strip.near {
        let _ = out.try_push(*point);
    }
    for point in strip.far.iter().rev() {
        let _ = out.try_push(*point);
    }
}

/// The area an outline in sub-pixel units encloses, in whole pixels.
fn enclosed(outline: &[(i32, i32)]) -> f64 {
    let count = outline.len();
    if count < 3 {
        return 0.0;
    }
    let mut twice = 0.0;
    for index in 0..count {
        let (x0, y0) = outline[index];
        let (x1, y1) = outline[(index + 1) % count];
        twice += f64::from(x0) * f64::from(y1) - f64::from(x1) * f64::from(y0);
    }
    let unit = f64::from(SUBPIXEL) * f64::from(SUBPIXEL);
    mathf::fabs(twice) * 0.5 / unit
}

#[cfg(test)]
#[path = "paint/tests.rs"]
mod tests;
