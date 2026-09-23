//! The authored surfaces a record's features choose between.
//!
//! First-party geometry, one template per form, stated in the frame of the
//! joint that carries it at the reference head and build; the record only
//! picks among them and the builder only scales them. A template that leans
//! to one side is authored once, on the figure's left, and mirrored.
//!
//! # Why hair is two surfaces
//!
//! Surfaces paint far-first by the depth of their centres, and a head seen
//! from the front has hair both behind the face and over it. So a style is
//! a cap over the crown, centred on the skull so it always paints after it,
//! and — where the hair reaches down the back — a mass behind, centred back
//! so it paints first and shows only beyond the skull's own outline.

use crate::frame::Body;
use crate::identity::{EarForm, EyeShape, FaceShape, HairStyle, HornForm, TailForm};
use crate::mesh::Ring;
use crate::socket::Side;
use crate::tint::Tint;

use super::hoop;

/// A ring at `(forward, side, up)`, `wide` across and `deep` through.
const fn ring(forward: f64, side: f64, up: f64, wide: f64, deep: f64) -> Ring {
    Ring::new(Body::new(forward, side, up), wide, deep)
}

/// `rings` reflected across the figure, for the right-hand twin of a
/// template authored on the left.
const fn mirror<const N: usize>(rings: [Ring; N]) -> [Ring; N] {
    let mut out = rings;
    let mut index = 0;
    while index < N {
        out[index].at.side = -rings[index].at.side;
        index += 1;
    }
    out
}

/// `at`, on the side `side` is.
fn sided(at: Body, side: Side) -> Body {
    Body::new(at.forward, at.side * side.across(), at.up)
}

/// The upper skull every face shares, so everything anchored to the crown —
/// hair, horns, an animal's ears — fits every face.
const CRANIUM: [Ring; 4] = [
    hoop(4.5, 5.0, 5.5),
    hoop(8.0, 4.6, 5.2),
    hoop(10.5, 3.2, 3.6),
    hoop(12.0, 0.9, 1.0),
];

/// A skull from its chin and jaw rings over the shared cranium.
const fn skull(chin: Ring, jaw: Ring) -> [Ring; 6] {
    [chin, jaw, CRANIUM[0], CRANIUM[1], CRANIUM[2], CRANIUM[3]]
}

/// The mean of `rings`' centres: where a surface sorts unless told otherwise,
/// and what hair layered on a skull sorts by instead.
pub(super) fn centre(rings: &[Ring]) -> Body {
    let mut sum = Body::ORIGIN;
    for ring in rings {
        sum = sum.plus(ring.at);
    }
    // Bounded by `MAX_RINGS`, far below the mantissa's own range.
    #[allow(clippy::cast_precision_loss, reason = "bounded by MAX_RINGS")]
    let count = rings.len().max(1) as f64;
    sum.scaled(1.0 / count)
}

/// How far behind the skull's centre a mass of hair down the back sorts: far
/// enough that seen from the front it is behind the skull, and seen from
/// behind in front of it.
pub(super) const BEHIND: f64 = 3.0;

/// How far the crown stands above the atlas, whatever the face.
///
/// The top ring's spine is vertical, so its cross-section is level and the
/// crown is its centre.
pub(super) const CROWN: f64 = CRANIUM[3].at.up;

const OVAL: [Ring; 6] = skull(hoop(-1.0, 2.6, 2.8), hoop(1.5, 4.4, 4.8));
const ROUND: [Ring; 6] = skull(hoop(-0.6, 3.3, 3.2), hoop(1.6, 4.9, 5.1));
const LONG: [Ring; 6] = skull(hoop(-2.2, 2.2, 2.6), hoop(0.9, 4.1, 4.6));
const BROAD: [Ring; 6] = skull(hoop(-0.8, 3.8, 3.0), hoop(1.5, 5.0, 4.9));
const HEART: [Ring; 6] = skull(hoop(-1.3, 1.6, 2.2), hoop(1.7, 4.0, 4.6));

/// The skull a face shape is drawn as.
pub(super) const fn skull_for(face: FaceShape) -> &'static [Ring] {
    match face {
        FaceShape::Oval => &OVAL,
        FaceShape::Round => &ROUND,
        FaceShape::Long => &LONG,
        FaceShape::Broad => &BROAD,
        FaceShape::Heart => &HEART,
    }
}

/// Where the left eye sits on the face, just proud of the cranium's surface.
pub(super) const EYE_AT: Body = Body::new(4.9, 1.95, 6.2);

/// An eye is a flattened capsule standing on the face: seen from the front
/// it is its full width, from the side a sliver, and from behind it sorts
/// behind the skull and is covered.
const EYE_ROUND: [Ring; 4] = [
    hoop(-0.8, 0.1, 0.05),
    hoop(-0.4, 0.7, 0.26),
    hoop(0.4, 0.7, 0.26),
    hoop(0.8, 0.1, 0.05),
];
const EYE_ALMOND: [Ring; 4] = [
    hoop(-0.55, 0.15, 0.05),
    hoop(-0.25, 0.9, 0.26),
    hoop(0.25, 0.9, 0.26),
    hoop(0.55, 0.15, 0.05),
];
const EYE_NARROW: [Ring; 4] = [
    hoop(-0.32, 0.25, 0.05),
    hoop(-0.12, 1.0, 0.22),
    hoop(0.12, 1.0, 0.22),
    hoop(0.32, 0.25, 0.05),
];

/// The eye an eye shape is drawn as, either side.
pub(super) const fn eye_for(shape: EyeShape) -> &'static [Ring] {
    match shape {
        EyeShape::Round => &EYE_ROUND,
        EyeShape::Almond => &EYE_ALMOND,
        EyeShape::Narrow => &EYE_NARROW,
    }
}

/// One ear: where it sits, its surface and the colour it is drawn in, and
/// the marked face an animal's ear shows.
#[derive(Copy, Clone, Debug)]
pub(super) struct Ear {
    pub(super) at: Body,
    pub(super) outer: &'static [Ring],
    pub(super) tint: Tint,
    pub(super) inner: Option<&'static [Ring]>,
}

const EAR_ROUND: [Ring; 4] = [
    hoop(-1.4, 0.1, 0.2),
    hoop(-0.7, 0.42, 0.95),
    hoop(0.5, 0.42, 1.05),
    hoop(1.4, 0.1, 0.25),
];
const EAR_POINTED: [Ring; 4] = [
    ring(0.0, 0.0, -1.2, 0.12, 0.25),
    ring(0.0, 0.0, 0.0, 0.42, 1.0),
    ring(-0.5, 0.6, 1.6, 0.34, 0.7),
    ring(-1.0, 1.2, 3.0, 0.08, 0.12),
];
const EAR_POINTED_RIGHT: [Ring; 4] = mirror(EAR_POINTED);
const EAR_LONG: [Ring; 5] = [
    ring(0.0, 0.0, -1.2, 0.12, 0.25),
    ring(0.0, 0.0, 0.0, 0.42, 1.05),
    ring(-0.9, 0.9, 1.7, 0.36, 0.75),
    ring(-1.9, 1.9, 3.1, 0.24, 0.4),
    ring(-2.8, 2.7, 4.2, 0.06, 0.1),
];
const EAR_LONG_RIGHT: [Ring; 5] = mirror(EAR_LONG);
const EAR_UPRIGHT: [Ring; 4] = [
    ring(0.0, 0.0, -0.6, 0.8, 0.5),
    ring(0.0, 0.25, 0.8, 1.9, 0.8),
    ring(-0.1, 0.55, 2.6, 1.3, 0.6),
    ring(-0.2, 0.9, 4.6, 0.14, 0.1),
];
const EAR_UPRIGHT_RIGHT: [Ring; 4] = mirror(EAR_UPRIGHT);
/// Forward of the outer ear, so seen from the front it paints over it.
const EAR_UPRIGHT_INNER: [Ring; 3] = [
    ring(0.35, 0.1, 0.2, 0.6, 0.2),
    ring(0.35, 0.35, 1.6, 1.1, 0.3),
    ring(0.2, 0.65, 3.6, 0.12, 0.08),
];
const EAR_UPRIGHT_INNER_RIGHT: [Ring; 3] = mirror(EAR_UPRIGHT_INNER);
const EAR_LOP: [Ring; 5] = [
    ring(0.0, 0.0, 0.3, 0.4, 1.1),
    ring(0.0, 0.9, 0.2, 0.4, 1.5),
    ring(0.1, 1.6, -1.2, 0.35, 1.4),
    ring(0.2, 1.9, -2.8, 0.3, 1.0),
    ring(0.2, 2.0, -3.8, 0.08, 0.2),
];
const EAR_LOP_RIGHT: [Ring; 5] = mirror(EAR_LOP);
/// Outboard of the hanging ear, so its marked face shows from the side.
const EAR_LOP_INNER: [Ring; 4] = [
    ring(0.2, 1.1, 0.1, 0.2, 1.0),
    ring(0.3, 1.8, -1.1, 0.18, 0.95),
    ring(0.35, 2.1, -2.6, 0.15, 0.65),
    ring(0.35, 2.2, -3.4, 0.05, 0.12),
];
const EAR_LOP_INNER_RIGHT: [Ring; 4] = mirror(EAR_LOP_INNER);
/// A fin sweeping back past the skull, thin across the head and tall
/// through it; a membrane, so drawn in the markings rather than the skin.
const EAR_FIN: [Ring; 4] = [
    ring(1.0, 0.0, 0.2, 0.25, 1.4),
    ring(-1.0, 0.5, 0.6, 0.35, 2.2),
    ring(-3.6, 1.3, 1.6, 0.28, 1.8),
    ring(-6.6, 2.1, 2.6, 0.07, 0.3),
];
const EAR_FIN_RIGHT: [Ring; 4] = mirror(EAR_FIN);

/// The ear an ear form is drawn as, on `side`.
pub(super) fn ear_for(form: EarForm, side: Side) -> Ear {
    let left = side == Side::Left;
    let pick = |on_left: &'static [Ring], on_right: &'static [Ring]| {
        if left {
            on_left
        } else {
            on_right
        }
    };
    let (at, outer, inner) = match form {
        EarForm::Round => (Body::new(-0.4, 4.75, 5.4), &EAR_ROUND[..], None),
        EarForm::Pointed => (
            Body::new(-0.3, 4.6, 5.3),
            pick(&EAR_POINTED, &EAR_POINTED_RIGHT),
            None,
        ),
        EarForm::Long => (
            Body::new(-0.3, 4.6, 5.3),
            pick(&EAR_LONG, &EAR_LONG_RIGHT),
            None,
        ),
        EarForm::Upright => (
            Body::new(0.3, 3.0, 10.4),
            pick(&EAR_UPRIGHT, &EAR_UPRIGHT_RIGHT),
            Some(pick(&EAR_UPRIGHT_INNER, &EAR_UPRIGHT_INNER_RIGHT)),
        ),
        EarForm::Lop => (
            Body::new(-0.3, 3.8, 9.0),
            pick(&EAR_LOP, &EAR_LOP_RIGHT),
            Some(pick(&EAR_LOP_INNER, &EAR_LOP_INNER_RIGHT)),
        ),
        EarForm::Finned => (
            Body::new(-0.8, 4.5, 6.0),
            pick(&EAR_FIN, &EAR_FIN_RIGHT),
            None,
        ),
    };
    let tint = match form {
        EarForm::Finned => Tint::Markings,
        _ => Tint::Skin,
    };
    Ear {
        at: sided(at, side),
        outer,
        tint,
        inner,
    }
}

/// Where the left horn rises from the brow.
const HORN_AT: Body = Body::new(2.0, 2.3, 9.3);

const HORN_NUBS: [Ring; 3] = [
    ring(0.0, 0.0, -0.3, 0.65, 0.65),
    ring(0.2, 0.15, 0.7, 0.5, 0.5),
    ring(0.35, 0.3, 1.5, 0.05, 0.05),
];
const HORN_NUBS_RIGHT: [Ring; 3] = mirror(HORN_NUBS);
const HORN_SWEPT: [Ring; 5] = [
    ring(0.0, 0.0, -0.3, 0.75, 0.75),
    ring(-0.7, 0.35, 1.5, 0.65, 0.65),
    ring(-2.2, 0.75, 2.8, 0.48, 0.48),
    ring(-4.0, 1.05, 3.3, 0.3, 0.3),
    ring(-5.8, 1.25, 3.1, 0.06, 0.06),
];
const HORN_SWEPT_RIGHT: [Ring; 5] = mirror(HORN_SWEPT);
const HORN_CURLED: [Ring; 6] = [
    ring(0.0, 0.0, -0.3, 0.85, 0.85),
    ring(-1.0, 0.7, 1.4, 0.75, 0.75),
    ring(-2.8, 1.5, 1.2, 0.62, 0.62),
    ring(-3.4, 2.1, -0.6, 0.48, 0.48),
    ring(-2.4, 2.4, -2.0, 0.3, 0.3),
    ring(-1.2, 2.3, -2.2, 0.06, 0.06),
];
const HORN_CURLED_RIGHT: [Ring; 6] = mirror(HORN_CURLED);
const HORN_SPIRE: [Ring; 4] = [
    ring(0.0, 0.0, -0.3, 0.7, 0.7),
    ring(0.15, 0.35, 2.2, 0.48, 0.48),
    ring(0.25, 0.65, 4.2, 0.24, 0.24),
    ring(0.3, 0.8, 5.4, 0.05, 0.05),
];
const HORN_SPIRE_RIGHT: [Ring; 4] = mirror(HORN_SPIRE);

/// The horn a horn form is drawn as on `side`, and where it rises.
pub(super) fn horn_for(form: HornForm, side: Side) -> (Body, &'static [Ring]) {
    let left = side == Side::Left;
    let rings: &'static [Ring] = match (form, left) {
        (HornForm::Nubs, true) => &HORN_NUBS,
        (HornForm::Nubs, false) => &HORN_NUBS_RIGHT,
        (HornForm::Swept, true) => &HORN_SWEPT,
        (HornForm::Swept, false) => &HORN_SWEPT_RIGHT,
        (HornForm::Curled, true) => &HORN_CURLED,
        (HornForm::Curled, false) => &HORN_CURLED_RIGHT,
        (HornForm::Spire, true) => &HORN_SPIRE,
        (HornForm::Spire, false) => &HORN_SPIRE_RIGHT,
    };
    (sided(HORN_AT, side), rings)
}

/// A hair style: the mass behind the skull, if it reaches down the back, and
/// the cap over the crown.
#[derive(Copy, Clone, Debug)]
pub(super) struct Hair {
    pub(super) behind: Option<&'static [Ring]>,
    pub(super) over: &'static [Ring],
}

const CROPPED: [Ring; 4] = [
    hoop(7.8, 4.95, 5.45),
    hoop(9.4, 4.5, 5.0),
    hoop(11.0, 3.25, 3.7),
    hoop(12.55, 0.95, 1.05),
];
const SHORT_OVER: [Ring; 4] = [
    hoop(7.6, 5.1, 5.6),
    hoop(9.4, 4.75, 5.25),
    hoop(11.2, 3.45, 3.9),
    hoop(12.9, 1.0, 1.1),
];
const SHORT_BEHIND: [Ring; 5] = [
    ring(-2.1, 0.0, 2.6, 1.2, 1.0),
    ring(-1.9, 0.0, 3.6, 3.2, 2.6),
    ring(-1.3, 0.0, 5.4, 4.6, 4.2),
    ring(-0.8, 0.0, 7.6, 5.0, 4.8),
    ring(-0.5, 0.0, 9.6, 4.2, 4.3),
];
const SHAGGY_OVER: [Ring; 5] = [
    hoop(7.2, 5.5, 6.0),
    hoop(8.8, 5.4, 5.9),
    hoop(10.7, 4.4, 4.9),
    hoop(12.4, 2.6, 2.9),
    hoop(13.6, 0.8, 0.9),
];
const SHAGGY_BEHIND: [Ring; 5] = [
    ring(-2.3, 0.0, 1.2, 1.4, 1.2),
    ring(-1.9, 0.0, 2.6, 4.2, 3.4),
    ring(-1.3, 0.0, 5.0, 5.4, 4.8),
    ring(-0.8, 0.0, 7.6, 5.6, 5.4),
    ring(-0.5, 0.0, 9.8, 4.8, 4.9),
];
const TOPKNOT_BUN: [Ring; 4] = [
    ring(-2.4, 0.0, 11.2, 0.3, 0.3),
    ring(-2.4, 0.0, 11.9, 1.5, 1.5),
    ring(-2.4, 0.0, 13.1, 1.6, 1.6),
    ring(-2.4, 0.0, 14.2, 0.3, 0.3),
];

/// The surfaces a hair style is drawn as.
pub(super) const fn hair_for(style: HairStyle) -> Hair {
    match style {
        HairStyle::Cropped => Hair {
            behind: None,
            over: &CROPPED,
        },
        HairStyle::Short => Hair {
            behind: Some(&SHORT_BEHIND),
            over: &SHORT_OVER,
        },
        HairStyle::Shaggy => Hair {
            behind: Some(&SHAGGY_BEHIND),
            over: &SHAGGY_OVER,
        },
        HairStyle::Topknot => Hair {
            behind: Some(&TOPKNOT_BUN),
            over: &CROPPED,
        },
    }
}

/// A tail: its length from the root, and the marked tip where it has one.
///
/// Stated in the tail joint's own frame, which rests pitched so a tail hangs
/// down it the way a limb hangs down its joint; the slight backward offset
/// toward the tip is the curl.
#[derive(Copy, Clone, Debug)]
pub(super) struct Tail {
    pub(super) root: &'static [Ring],
    pub(super) tip: Option<&'static [Ring]>,
}

const BRUSH: [Ring; 4] = [
    ring(0.0, 0.0, 0.0, 1.0, 1.0),
    ring(-0.3, 0.0, -3.2, 2.3, 2.3),
    ring(-0.6, 0.0, -7.6, 3.3, 3.3),
    ring(-0.9, 0.0, -12.0, 3.1, 3.1),
];
const BRUSH_TIP: [Ring; 3] = [
    ring(-0.9, 0.0, -12.0, 3.1, 3.1),
    ring(-1.3, 0.0, -15.8, 1.9, 1.9),
    ring(-1.6, 0.0, -17.8, 0.25, 0.25),
];
const SLENDER: [Ring; 4] = [
    ring(0.0, 0.0, 0.0, 0.55, 0.55),
    ring(-0.1, 0.0, -4.0, 0.5, 0.5),
    ring(-0.3, 0.0, -8.5, 0.45, 0.45),
    ring(-0.9, 0.0, -12.5, 0.4, 0.4),
];
const SLENDER_TIP: [Ring; 3] = [
    ring(-0.9, 0.0, -12.5, 0.4, 0.4),
    ring(-2.0, 0.0, -15.5, 0.32, 0.32),
    ring(-3.2, 0.0, -17.2, 0.05, 0.05),
];
const SCALED: [Ring; 6] = [
    ring(0.0, 0.0, 0.0, 2.4, 2.4),
    ring(0.0, 0.0, -4.0, 2.0, 2.0),
    ring(-0.3, 0.0, -8.5, 1.5, 1.5),
    ring(-0.8, 0.0, -13.0, 1.0, 1.0),
    ring(-1.4, 0.0, -17.5, 0.55, 0.55),
    ring(-2.0, 0.0, -21.0, 0.08, 0.08),
];

/// The surfaces a tail form is drawn as.
pub(super) const fn tail_for(form: TailForm) -> Tail {
    match form {
        TailForm::Brush => Tail {
            root: &BRUSH,
            tip: Some(&BRUSH_TIP),
        },
        TailForm::Slender => Tail {
            root: &SLENDER,
            tip: Some(&SLENDER_TIP),
        },
        TailForm::Scaled => Tail {
            root: &SCALED,
            tip: None,
        },
    }
}
