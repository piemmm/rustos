//! The shapes a body part is drawn as, and how one becomes pixels.
//!
//! A circle cannot state a cat. The silhouette a mascot is recognised by needs
//! a tall wedge for an ear, a tapered limb that pivots where it meets the
//! body, a scalloped hem that reads as cloth, and a rounded mass that is
//! neither a disc nor a box. Each is generated as a closed outline and filled
//! through `lib/raster`'s one anti-aliased scan converter, so there is no
//! second rasteriser here and the edge quality is the desktop's own.
//!
//! Outlines are authored in part-local pixels — `x` to the right, `y` upwards
//! — and every one is **symmetric about its local vertical axis**. That is
//! what keeps the turnaround free: the arrangement of parts turns with the
//! heading through the camera, and no shape needs mirroring, a second sprite
//! set, or a branch on which way the creature faces.

use tairix_inline::ArrayVec;
use tairix_raster::{Color, Surface, SUBPIXEL};
use tairix_util::mathf;

/// The most vertices any one shape's outline needs.
///
/// The outline buffers are fixed arrays of this length, so a shape cannot ask
/// for more geometry than the buffer holds and be silently truncated into the
/// wrong outline. The assertions below hold each generator to it at build
/// time, so raising a generator's detail without raising this fails the build
/// rather than the picture.
pub const MAX_VERTICES: usize = MASS_VERTICES;

const _: () = assert!(MAX_VERTICES >= DRAPE_VERTICES);
const _: () = assert!(MAX_VERTICES >= LIMB_VERTICES);
const _: () = assert!(MAX_VERTICES >= EAR_VERTICES);

/// A traced outline in part-local pixels.
type Outline = ArrayVec<(f64, f64), MAX_VERTICES>;

/// What a body part is drawn as.
///
/// A closed set of the five forms the creature is actually built from. Each
/// earns its place by stating something the others cannot: a mass is a
/// rounded volume, a limb tapers and pivots, an ear leans, a drape hangs in
/// folds, and fur is soft-edged where a crisp outline would read as plastic.
#[derive(Copy, Clone, Debug, PartialEq)]
pub enum Shape {
    /// A soft radial splat: the coat's rim and the tail's plume, where a crisp
    /// edge would read as moulded plastic rather than as hair.
    Fur {
        /// Radius in pixels before the rim ripple.
        radius: f64,
    },
    /// A rounded volume: an ellipse of the given radii pulled `square` of the
    /// way out towards its own bounding box, `0.0` leaving it an ellipse and
    /// `1.0` taking it to the box's corners.
    ///
    /// The body, the skull, the haunch, and the eyes. A chest is not a disc
    /// and an anime eye is not a circle; one parameter covers both.
    Mass {
        /// Half-width in pixels.
        rx: f64,
        /// Half-height in pixels.
        ry: f64,
        /// How far from an ellipse towards a rounded box.
        square: f64,
    },
    /// A limb tapering from the joint at the local origin to a foot `length`
    /// below it, `top` half-wide at the joint and `foot` half-wide at the end.
    ///
    /// Drawn from the joint downwards so a swing rotates it about where it
    /// meets the body: a limb that only translated would read as a sliding
    /// peg, which is what made the legs look detached.
    Limb {
        /// How far the limb reaches below its joint.
        length: f64,
        /// Half-width at the joint.
        top: f64,
        /// Half-width at the foot.
        foot: f64,
    },
    /// An ear: a wedge `half_width` half-wide at its base rising `height`,
    /// its tip leaned outwards by `lean` pixels.
    ///
    /// The lean is what separates a cat's ear from a bear's, and the base
    /// tucks below the skull so no seam shows where the two meet.
    Ear {
        /// Half-width at the base.
        half_width: f64,
        /// How far the tip rises above the base.
        height: f64,
        /// How far the tip leans out from the base's centre.
        lean: f64,
    },
    /// A panel of cloth `rx` half-wide hanging `ry`, its hem scalloped into
    /// `folds` lobes.
    ///
    /// A garment drawn as a plain quadrilateral reads as a card taped to the
    /// creature. The hem is what makes it cloth.
    Drape {
        /// Half-width at the shoulder.
        rx: f64,
        /// How far the panel hangs below the shoulder.
        ry: f64,
        /// How many lobes the hem is scalloped into.
        folds: u32,
    },
}

impl Shape {
    /// How far from its anchor this shape reaches.
    ///
    /// Used to size the surface the creature must fit inside, so the bound is
    /// derived from the body rather than asserted about it.
    #[must_use]
    pub fn reach(self) -> f64 {
        match self {
            Self::Fur { radius } => radius,
            // A mass and a drape are both bounded by their own two radii.
            Self::Mass { rx, ry, .. } | Self::Drape { rx, ry, .. } => mathf::hypot(rx, ry),
            Self::Limb { length, top, foot } => length + mathf::fmax(top, foot),
            Self::Ear {
                half_width,
                height,
                lean,
            } => mathf::hypot(half_width + mathf::fabs(lean), height),
        }
    }

    /// Trace the shape's closed outline in part-local pixels into `out`,
    /// which is cleared first.
    ///
    /// `Fur` has no outline — it is composited by the soft splat instead — so
    /// it traces nothing and the caller draws it the other way.
    pub fn trace(self, out: &mut Outline) {
        out.clear();
        match self {
            Self::Fur { .. } => {}
            Self::Mass { rx, ry, square } => mass(rx, ry, square, out),
            Self::Limb { length, top, foot } => limb(length, top, foot, out),
            Self::Ear {
                half_width,
                height,
                lean,
            } => ear(half_width, height, lean, out),
            Self::Drape { rx, ry, folds } => drape(rx, ry, folds, out),
        }
    }
}

/// How many vertices a rounded mass is traced with.
///
/// Enough that the largest mass the creature carries — a haunch some two dozen
/// pixels across — shows no flat on its rim, and few enough that a whole
/// creature's outlines cost a few hundred vertices rather than thousands.
const MASS_VERTICES: usize = 28;

/// Trace an ellipse of `rx` by `ry` pulled `square` of the way out to its own
/// bounding box.
///
/// The pull is along each ray, so the corners round off while the flats stay
/// flat — which is what a shoulder looks like. Scaling the radii towards a
/// rectangle instead would bulge the whole rim.
fn mass(rx: f64, ry: f64, square: f64, out: &mut Outline) {
    let square = mathf::clamp(square, 0.0, 1.0);
    for index in 0..MASS_VERTICES {
        let angle = core::f64::consts::TAU * index_fraction(index, MASS_VERTICES);
        let (sin, cos) = (mathf::sin(angle), mathf::cos(angle));
        // The same ray extended until it meets the bounding box. One of the
        // two magnitudes is always at least `1/sqrt(2)`, so this is total.
        let to_box = 1.0 / mathf::fmax(mathf::fabs(cos), mathf::fabs(sin));
        let pull = 1.0 + (to_box - 1.0) * square;
        push(out, (rx * cos * pull, ry * sin * pull));
    }
}

/// Trace a tapered limb hanging from its joint at the origin.
///
/// The foot is rounded rather than cut square, because a flat-bottomed limb
/// reads as a table leg.
fn limb(length: f64, top: f64, foot: f64, out: &mut Outline) {
    let length = mathf::fmax(length, 0.0);
    let sole = -length + foot;
    push(out, (top, 0.0));
    push(out, (foot, sole));
    for index in 0..=LIMB_FOOT_VERTICES {
        // Sweep the rounded foot from its right side round to its left.
        let turn = core::f64::consts::PI * index_fraction(index, LIMB_FOOT_VERTICES);
        push(
            out,
            (foot * mathf::cos(turn), sole - foot * mathf::sin(turn)),
        );
    }
    push(out, (-foot, sole));
    push(out, (-top, 0.0));
}

/// How many vertices the rounded end of a limb is traced with.
const LIMB_FOOT_VERTICES: usize = 8;

/// How many vertices a limb's outline is traced with: two at the joint, two at
/// the sole's ends, and the rounded sweep between them.
const LIMB_VERTICES: usize = 4 + LIMB_FOOT_VERTICES + 1;

/// Trace an ear: a tucked base, a bowed inner edge, and a leaned tip.
fn ear(half_width: f64, height: f64, lean: f64, out: &mut Outline) {
    let shoulder = height * EAR_SHOULDER;
    push(out, (half_width, 0.0));
    push(
        out,
        (half_width * EAR_OUTER_BOW + lean * EAR_SHOULDER, shoulder),
    );
    push(out, (lean, height));
    push(
        out,
        (-half_width * EAR_INNER_BOW + lean * EAR_SHOULDER, shoulder),
    );
    push(out, (-half_width, 0.0));
    push(out, (-half_width * EAR_BASE_TUCK, -height * EAR_BASE_DEPTH));
    push(out, (half_width * EAR_BASE_TUCK, -height * EAR_BASE_DEPTH));
}

/// How many vertices an ear's outline is traced with.
const EAR_VERTICES: usize = 7;

/// How far up the ear its widest shoulder sits.
const EAR_SHOULDER: f64 = 0.46;

/// How far the ear's outer edge bows in by its shoulder.
const EAR_OUTER_BOW: f64 = 0.86;

/// How far the ear's inner edge bows in by its shoulder.
const EAR_INNER_BOW: f64 = 0.54;

/// How far the ear's base tucks below the skull's surface, as a fraction of
/// its height, so no seam shows where the two meet.
const EAR_BASE_DEPTH: f64 = 0.24;

/// How wide the tucked base is, as a fraction of the ear's half-width.
const EAR_BASE_TUCK: f64 = 0.82;

/// Trace a hanging panel of cloth with a scalloped hem.
fn drape(rx: f64, ry: f64, folds: u32, out: &mut Outline) {
    let ry = mathf::fmax(ry, 0.0);
    let lobes = usize::try_from(folds)
        .unwrap_or(DRAPE_MAX_FOLDS)
        .clamp(1, DRAPE_MAX_FOLDS);
    let folds = u32::try_from(lobes).unwrap_or(1);
    push(out, (-rx, 0.0));
    push(out, (rx, 0.0));
    let steps = lobes * DRAPE_STEPS_PER_FOLD;
    for index in 0..=steps {
        // Right to left along the hem, so the ring closes without crossing.
        let across = 1.0 - 2.0 * index_fraction(index, steps);
        let lobe = mathf::sin(core::f64::consts::PI * f64::from(folds) * (1.0 - across) / 2.0);
        // The hem lifts at the panel's corners, which is what stops the
        // scallop reading as a fringe hung off a straight edge.
        let taper = 1.0 - DRAPE_CORNER_LIFT * across * across;
        let sag = ry * taper - mathf::fabs(lobe) * ry * DRAPE_FOLD_DEPTH;
        push(out, (rx * across, -sag));
    }
}

/// How many outline steps each hem lobe is traced with.
const DRAPE_STEPS_PER_FOLD: usize = 6;

/// The most lobes a hem is scalloped into.
///
/// The bound the outline buffer is sized from, so a panel asking for more is
/// gathered more coarsely rather than drawn as a truncated ring.
const DRAPE_MAX_FOLDS: usize = 4;

/// How many vertices a drape's outline is traced with: two at the shoulder and
/// the hem's inclusive sweep across every lobe.
const DRAPE_VERTICES: usize = 2 + DRAPE_MAX_FOLDS * DRAPE_STEPS_PER_FOLD + 1;

/// How deep the hem scallops are, as a fraction of the panel's drop.
const DRAPE_FOLD_DEPTH: f64 = 0.14;

/// How far the hem lifts at the panel's corners.
const DRAPE_CORNER_LIFT: f64 = 0.22;

/// Append `vertex`, dropping it if the outline is somehow already full.
///
/// The build-time bound above is what makes this total: every generator's
/// worst case is asserted to fit, so the drop is unreachable and is written as
/// a discard rather than as a panic on a drawing path.
fn push(out: &mut Outline, vertex: (f64, f64)) {
    let _ = out.try_push(vertex);
}

/// `index / count` as a fraction, total for any `count`.
fn index_fraction(index: usize, count: usize) -> f64 {
    if count == 0 {
        return 0.0;
    }
    let index = u32::try_from(index).unwrap_or(u32::MAX);
    let count = u32::try_from(count).unwrap_or(u32::MAX);
    f64::from(index) / f64::from(count)
}

/// A shape placed on screen: where it goes, how it is turned, and its colour.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct Placed {
    /// Screen column of the shape's local origin.
    pub x: f64,
    /// Screen row of the shape's local origin.
    pub y: f64,
    /// How far the outline is rotated about its own origin, in radians;
    /// positive leans its top to the right.
    pub turn: f64,
    /// What to draw.
    pub shape: Shape,
    /// Straight-alpha colour.
    pub color: Color,
    /// Which fur ripple a `Fur` shape wears, and the tie-break that keeps the
    /// paint order identical from frame to frame.
    pub seed: u16,
}

/// The buffers an outline is traced and transformed through.
///
/// Fixed arrays rather than growable ones: every shape's vertex count is
/// bounded and asserted at build time, so the whole outline path touches no
/// allocator at all.
#[derive(Default)]
pub struct Scratch {
    local: Outline,
    device: ArrayVec<(i32, i32), MAX_VERTICES>,
}

impl Scratch {
    /// Empty buffers.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

/// Fill `placed`'s outline onto `surface`, tracing it through `scratch`.
pub fn fill(surface: &mut Surface, placed: &Placed, scratch: &mut Scratch) {
    if placed.color.a == 0 {
        return;
    }
    placed.shape.trace(&mut scratch.local);
    if scratch.local.len() < 3 {
        return;
    }
    let (sin, cos) = (mathf::sin(placed.turn), mathf::cos(placed.turn));
    scratch.device.clear();
    for index in 0..scratch.local.len() {
        let (lx, ly) = scratch.local[index];
        let rx = lx * cos - ly * sin;
        let ry = lx * sin + ly * cos;
        // Local `y` runs upwards and a surface row runs down, so the one sign
        // flip happens here rather than in every shape.
        let _ = scratch
            .device
            .try_push((to_subpixel(placed.x + rx), to_subpixel(placed.y - ry)));
    }
    surface.fill_polygon_subpixel(&scratch.device, placed.color);
}

/// A screen coordinate in the scan converter's sub-pixel units.
///
/// Saturating rather than wrapping: a shape placed far off the surface must
/// stay off it, and a wrapped coordinate would fold it back across the canvas.
fn to_subpixel(value: f64) -> i32 {
    let scaled = value * f64::from(SUBPIXEL);
    if scaled <= f64::from(i32::MIN) {
        return i32::MIN;
    }
    if scaled >= f64::from(i32::MAX) {
        return i32::MAX;
    }
    mathf::round_i32(scaled)
}

#[cfg(test)]
#[path = "shape_tests.rs"]
mod tests;
