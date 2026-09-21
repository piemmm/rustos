//! Parametric outline primitives, and how one becomes pixels.
//!
//! A circle states very little. The silhouettes a drawn figure is
//! recognised by need a tall leaning wedge, a shape that tapers and pivots
//! about one end, a scalloped hem that reads as cloth, a hard rim that
//! reads as metal, and a rounded mass that is neither a disc nor a box.
//! Each is generated here as a closed outline and filled through this
//! crate's one anti-aliased scan converter, so there is no second
//! rasteriser and the edge quality is the desktop's own.
//!
//! Outlines are authored in shape-local pixels — `x` to the right, `y`
//! upwards — and every one is **symmetric about its local vertical axis**.
//! That is what keeps a turnaround free: a consumer arranges its parts and
//! turns the arrangement, and no shape needs mirroring, a second asset set,
//! or a branch on which way the subject faces.
//!
//! # Geometry only
//!
//! Every name states the *shape*, never what a caller draws with it. A
//! [`Taper`](Shape::Taper) is a taper; whether it is a limb, a tail or a
//! weapon shaft is the caller's business and is never encoded here.

use tairix_inline::ArrayVec;
use tairix_util::mathf;

use crate::color::Color;
use crate::surface::{Surface, SUBPIXEL};

/// The most vertices any one shape's outline needs.
///
/// The outline buffers are fixed arrays of this length, so a shape cannot
/// ask for more geometry than the buffer holds and be silently truncated
/// into the wrong outline. The assertions below hold each generator to it
/// at build time, so raising a generator's detail without raising this
/// fails the build rather than the picture.
pub const MAX_VERTICES: usize = SUPERELLIPSE_VERTICES;

const _: () = assert!(MAX_VERTICES >= SCALLOPED_PANEL_VERTICES);
const _: () = assert!(MAX_VERTICES >= BEVELLED_PANEL_VERTICES);
const _: () = assert!(MAX_VERTICES >= TAPER_VERTICES);
const _: () = assert!(MAX_VERTICES >= WEDGE_VERTICES);

/// A traced outline in shape-local pixels.
pub type Outline = ArrayVec<(f64, f64), MAX_VERTICES>;

/// One parametric outline.
///
/// A closed set: each member states something the others cannot, and
/// anything it cannot state is a *composition* of these rather than a
/// seventh variant added for one asset.
#[derive(Copy, Clone, Debug, PartialEq)]
pub enum Shape {
    /// A soft radial blob of the given radius, carrying no outline.
    ///
    /// The one member with no traced edge: a crisp rim would read as
    /// moulded plastic where the subject is fur, hair, foliage or smoke,
    /// so a consumer composites it with its own soft falloff and this
    /// carries only the extent.
    Splat {
        /// Radius in pixels before any rim ripple the consumer applies.
        radius: f64,
    },
    /// An ellipse of the given radii pulled `square` of the way out
    /// towards its own bounding box, `0.0` leaving it an ellipse and `1.0`
    /// taking it to the box's corners.
    ///
    /// One parameter covers a disc, a rounded box, and everything between.
    Superellipse {
        /// Half-width in pixels.
        rx: f64,
        /// Half-height in pixels.
        ry: f64,
        /// How far from an ellipse towards a rounded box.
        square: f64,
    },
    /// A taper from the origin to a rounded end `length` below it, `top`
    /// half-wide at the origin and `foot` half-wide at the end.
    ///
    /// Drawn from the origin downwards, so a rotation pivots it about the
    /// end it hangs from rather than sliding it.
    Taper {
        /// How far the shape reaches below its origin.
        length: f64,
        /// Half-width at the origin.
        top: f64,
        /// Half-width at the far end.
        foot: f64,
    },
    /// A wedge `half_width` half-wide at its base rising `height`, its tip
    /// leaned sideways by `lean` pixels, with a tucked base.
    ///
    /// The lean is what separates one silhouette from another, and the
    /// tuck sinks the base below whatever it rises from so no seam shows.
    Wedge {
        /// Half-width at the base.
        half_width: f64,
        /// How far the tip rises above the base.
        height: f64,
        /// How far the tip leans out from the base's centre.
        lean: f64,
    },
    /// A panel `rx` half-wide hanging `ry` below the origin, its hem
    /// scalloped into `folds` lobes.
    ///
    /// A hanging panel drawn as a plain quadrilateral reads as a card
    /// taped on. The hem is what makes it cloth.
    ScallopedPanel {
        /// Half-width at the top.
        rx: f64,
        /// How far the panel hangs below the top.
        ry: f64,
        /// How many lobes the hem is scalloped into.
        folds: u32,
    },
    /// A panel of the given half-extents centred on the origin with its
    /// four corners cut back by `bevel`.
    ///
    /// The hard-edged counterpart to [`Superellipse`](Shape::Superellipse)
    /// and the reason both exist: a rounded rim reads as flesh, and a
    /// bevelled one reads as plate.
    BevelledPanel {
        /// Half-width in pixels.
        rx: f64,
        /// Half-height in pixels.
        ry: f64,
        /// How far each corner is cut back along both edges.
        bevel: f64,
    },
}

impl Shape {
    /// How far from its origin this shape reaches.
    ///
    /// Lets a caller size the surface its arrangement must fit inside from
    /// the shapes themselves rather than asserting a bound about them.
    #[must_use]
    pub fn reach(self) -> f64 {
        match self {
            Self::Splat { radius } => radius,
            Self::Superellipse { rx, ry, .. }
            | Self::ScallopedPanel { rx, ry, .. }
            | Self::BevelledPanel { rx, ry, .. } => mathf::hypot(rx, ry),
            Self::Taper { length, top, foot } => length + mathf::fmax(top, foot),
            Self::Wedge {
                half_width,
                height,
                lean,
            } => mathf::hypot(half_width + mathf::fabs(lean), height),
        }
    }

    /// Whether every dimension is a finite number.
    ///
    /// Total over the set, so a caller validating authored geometry does not
    /// have to know which member carries which dimension — and cannot fall
    /// out of step with the set when a member changes.
    #[must_use]
    pub fn is_real(self) -> bool {
        match self {
            Self::Splat { radius } => radius.is_finite(),
            Self::Superellipse { rx, ry, square } => {
                rx.is_finite() && ry.is_finite() && square.is_finite()
            }
            Self::Taper { length, top, foot } => {
                length.is_finite() && top.is_finite() && foot.is_finite()
            }
            Self::Wedge {
                half_width,
                height,
                lean,
            } => half_width.is_finite() && height.is_finite() && lean.is_finite(),
            Self::ScallopedPanel { rx, ry, .. } => rx.is_finite() && ry.is_finite(),
            Self::BevelledPanel { rx, ry, bevel } => {
                rx.is_finite() && ry.is_finite() && bevel.is_finite()
            }
        }
    }

    /// This shape with every length multiplied by `factor`.
    ///
    /// Lengths only: `square` is a fraction of the way to the bounding box
    /// and `folds` a count of lobes, so both are shape-relative and a
    /// uniform scale leaves them alone. This is what lets an arrangement be
    /// authored once at a reference size and drawn at any other.
    #[must_use]
    pub fn scaled(self, factor: f64) -> Self {
        match self {
            Self::Splat { radius } => Self::Splat {
                radius: radius * factor,
            },
            Self::Superellipse { rx, ry, square } => Self::Superellipse {
                rx: rx * factor,
                ry: ry * factor,
                square,
            },
            Self::Taper { length, top, foot } => Self::Taper {
                length: length * factor,
                top: top * factor,
                foot: foot * factor,
            },
            Self::Wedge {
                half_width,
                height,
                lean,
            } => Self::Wedge {
                half_width: half_width * factor,
                height: height * factor,
                lean: lean * factor,
            },
            Self::ScallopedPanel { rx, ry, folds } => Self::ScallopedPanel {
                rx: rx * factor,
                ry: ry * factor,
                folds,
            },
            Self::BevelledPanel { rx, ry, bevel } => Self::BevelledPanel {
                rx: rx * factor,
                ry: ry * factor,
                bevel: bevel * factor,
            },
        }
    }

    /// Trace the shape's closed outline in shape-local pixels into `out`,
    /// which is cleared first.
    ///
    /// [`Splat`](Shape::Splat) has no outline, so it traces nothing and the
    /// caller composites it instead.
    pub fn trace(self, out: &mut Outline) {
        out.clear();
        match self {
            Self::Splat { .. } => {}
            Self::Superellipse { rx, ry, square } => superellipse(rx, ry, square, out),
            Self::Taper { length, top, foot } => taper(length, top, foot, out),
            Self::Wedge {
                half_width,
                height,
                lean,
            } => wedge(half_width, height, lean, out),
            Self::ScallopedPanel { rx, ry, folds } => scalloped_panel(rx, ry, folds, out),
            Self::BevelledPanel { rx, ry, bevel } => bevelled_panel(rx, ry, bevel, out),
        }
    }
}

/// How many vertices a superellipse is traced with.
///
/// Enough that a rim a couple of dozen pixels across shows no flat, and few
/// enough that a whole arrangement's outlines cost a few hundred vertices
/// rather than thousands.
const SUPERELLIPSE_VERTICES: usize = 28;

/// Trace an ellipse of `rx` by `ry` pulled `square` of the way out to its
/// own bounding box.
///
/// The pull is along each ray, so the corners round off while the flats
/// stay flat. Scaling the radii towards a rectangle instead would bulge the
/// whole rim.
fn superellipse(rx: f64, ry: f64, square: f64, out: &mut Outline) {
    let square = mathf::clamp(square, 0.0, 1.0);
    for index in 0..SUPERELLIPSE_VERTICES {
        let angle = core::f64::consts::TAU * index_fraction(index, SUPERELLIPSE_VERTICES);
        let (sin, cos) = (mathf::sin(angle), mathf::cos(angle));
        // The same ray extended until it meets the bounding box. One of the
        // two magnitudes is always at least `1/sqrt(2)`, so this is total.
        let to_box = 1.0 / mathf::fmax(mathf::fabs(cos), mathf::fabs(sin));
        let pull = 1.0 + (to_box - 1.0) * square;
        push(out, (rx * cos * pull, ry * sin * pull));
    }
}

/// Trace a taper hanging from its origin, its far end rounded.
///
/// The end is rounded rather than cut square, because a flat-bottomed taper
/// reads as a table leg.
fn taper(length: f64, top: f64, foot: f64, out: &mut Outline) {
    let length = mathf::fmax(length, 0.0);
    let sole = -length + foot;
    push(out, (top, 0.0));
    push(out, (foot, sole));
    for index in 0..=TAPER_FOOT_VERTICES {
        // Sweep the rounded end from its right side round to its left.
        let turn = core::f64::consts::PI * index_fraction(index, TAPER_FOOT_VERTICES);
        push(
            out,
            (foot * mathf::cos(turn), sole - foot * mathf::sin(turn)),
        );
    }
    push(out, (-foot, sole));
    push(out, (-top, 0.0));
}

/// How many vertices the rounded end of a taper is traced with.
const TAPER_FOOT_VERTICES: usize = 8;

/// How many vertices a taper's outline is traced with: two at the origin,
/// two at the end's sides, and the rounded sweep between them.
const TAPER_VERTICES: usize = 4 + TAPER_FOOT_VERTICES + 1;

/// Trace a wedge: a tucked base, a bowed inner edge, and a leaned tip.
fn wedge(half_width: f64, height: f64, lean: f64, out: &mut Outline) {
    let shoulder = height * WEDGE_SHOULDER;
    push(out, (half_width, 0.0));
    push(
        out,
        (
            half_width * WEDGE_OUTER_BOW + lean * WEDGE_SHOULDER,
            shoulder,
        ),
    );
    push(out, (lean, height));
    push(
        out,
        (
            -half_width * WEDGE_INNER_BOW + lean * WEDGE_SHOULDER,
            shoulder,
        ),
    );
    push(out, (-half_width, 0.0));
    push(
        out,
        (-half_width * WEDGE_BASE_TUCK, -height * WEDGE_BASE_DEPTH),
    );
    push(
        out,
        (half_width * WEDGE_BASE_TUCK, -height * WEDGE_BASE_DEPTH),
    );
}

/// How many vertices a wedge's outline is traced with.
const WEDGE_VERTICES: usize = 7;

/// How far up the wedge its widest shoulder sits.
const WEDGE_SHOULDER: f64 = 0.46;

/// How far the wedge's outer edge bows in by its shoulder.
const WEDGE_OUTER_BOW: f64 = 0.86;

/// How far the wedge's inner edge bows in by its shoulder.
const WEDGE_INNER_BOW: f64 = 0.54;

/// How far the wedge's base tucks below whatever it rises from, as a
/// fraction of its height, so no seam shows where the two meet.
const WEDGE_BASE_DEPTH: f64 = 0.24;

/// How wide the tucked base is, as a fraction of the wedge's half-width.
const WEDGE_BASE_TUCK: f64 = 0.82;

/// Trace a hanging panel with a scalloped hem.
fn scalloped_panel(rx: f64, ry: f64, folds: u32, out: &mut Outline) {
    let ry = mathf::fmax(ry, 0.0);
    let lobes = usize::try_from(folds)
        .unwrap_or(SCALLOPED_MAX_FOLDS)
        .clamp(1, SCALLOPED_MAX_FOLDS);
    let folds = u32::try_from(lobes).unwrap_or(1);
    push(out, (-rx, 0.0));
    push(out, (rx, 0.0));
    let steps = lobes * SCALLOPED_STEPS_PER_FOLD;
    for index in 0..=steps {
        // Right to left along the hem, so the ring closes without crossing.
        let across = 1.0 - 2.0 * index_fraction(index, steps);
        let lobe = mathf::sin(core::f64::consts::PI * f64::from(folds) * (1.0 - across) / 2.0);
        // The hem lifts at the panel's corners, which is what stops the
        // scallop reading as a fringe hung off a straight edge.
        let taper = 1.0 - SCALLOPED_CORNER_LIFT * across * across;
        let sag = ry * taper - mathf::fabs(lobe) * ry * SCALLOPED_FOLD_DEPTH;
        push(out, (rx * across, -sag));
    }
}

/// How many outline steps each hem lobe is traced with.
const SCALLOPED_STEPS_PER_FOLD: usize = 6;

/// The most lobes a hem is scalloped into.
///
/// The bound the outline buffer is sized from, so a panel asking for more
/// is gathered more coarsely rather than drawn as a truncated ring.
const SCALLOPED_MAX_FOLDS: usize = 4;

/// How many vertices a scalloped panel is traced with: two at the top and
/// the hem's inclusive sweep across every lobe.
const SCALLOPED_PANEL_VERTICES: usize = 2 + SCALLOPED_MAX_FOLDS * SCALLOPED_STEPS_PER_FOLD + 1;

/// How deep the hem scallops are, as a fraction of the panel's drop.
const SCALLOPED_FOLD_DEPTH: f64 = 0.14;

/// How far the hem lifts at the panel's corners.
const SCALLOPED_CORNER_LIFT: f64 = 0.22;

/// Trace a panel centred on the origin with its corners cut back.
///
/// The cut is clamped to half the shorter side, so the widest bevel a
/// caller can ask for is the lozenge where the cuts meet — never an
/// inside-out ring.
fn bevelled_panel(rx: f64, ry: f64, bevel: f64, out: &mut Outline) {
    let rx = mathf::fmax(rx, 0.0);
    let ry = mathf::fmax(ry, 0.0);
    let bevel = mathf::clamp(bevel, 0.0, mathf::fmin(rx, ry));
    let (inset_x, inset_y) = (rx - bevel, ry - bevel);
    push(out, (inset_x, ry));
    push(out, (rx, inset_y));
    push(out, (rx, -inset_y));
    push(out, (inset_x, -ry));
    push(out, (-inset_x, -ry));
    push(out, (-rx, -inset_y));
    push(out, (-rx, inset_y));
    push(out, (-inset_x, ry));
}

/// How many vertices a bevelled panel is traced with: two per cut corner.
const BEVELLED_PANEL_VERTICES: usize = 8;

/// Append `vertex`, dropping it if the outline is somehow already full.
///
/// The build-time bound above is what makes this total: every generator's
/// worst case is asserted to fit, so the drop is unreachable and is written
/// as a discard rather than as a panic on a drawing path.
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

/// A shape placed on a surface: where it goes, how it is turned, and its
/// colour.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct Placed {
    /// Surface column of the shape's local origin.
    pub x: f64,
    /// Surface row of the shape's local origin.
    pub y: f64,
    /// How far the outline is rotated about its own origin, in radians;
    /// positive leans its top to the right.
    pub turn: f64,
    /// What to draw.
    pub shape: Shape,
    /// Straight-alpha colour.
    pub color: Color,
    /// Which ripple a [`Splat`](Shape::Splat) wears, and the tie-break that
    /// keeps a caller's paint order identical from frame to frame.
    pub seed: u16,
}

/// The buffers an outline is traced and transformed through.
///
/// Fixed arrays rather than growable ones: every shape's vertex count is
/// bounded and asserted at build time, so the whole outline path touches no
/// allocator at all.
#[derive(Debug, Default)]
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
        // Local `y` runs upwards and a surface row runs down, so the one
        // sign flip happens here rather than in every shape.
        let _ = scratch
            .device
            .try_push((to_subpixel(placed.x + rx), to_subpixel(placed.y - ry)));
    }
    surface.fill_polygon_subpixel(&scratch.device, placed.color);
}

/// A surface coordinate in the scan converter's sub-pixel units.
///
/// Saturating rather than wrapping: a shape placed far off the surface must
/// stay off it, and a wrapped coordinate would fold it back across the
/// canvas.
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
