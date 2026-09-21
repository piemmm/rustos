//! A part's surface: rings along a spine, skinned across two joints, and the
//! shaded bands it is drawn as.
//!
//! # Why a mesh and not a flat outline
//!
//! A limb drawn as a flat outline has to be *placed* — an origin, a screen
//! angle and a length — and none of the three survives the projection. The
//! angle is an approximation of a three-axis rotation onto the one axis a
//! billboard can turn about, and the length is the bone's own rather than
//! the shortened length the projection gives it. Measured against the
//! shipped walk, the drawn end of a thigh missed the knee it hangs from by
//! up to a third of the figure's height.
//!
//! A mesh has no placement to get wrong. Every vertex is carried by the
//! joints themselves and then projected, so a limb's far end *is* its child
//! joint, at whatever length and angle the heading leaves it — exactly, at
//! every heading, with no approximation anywhere.
//!
//! # Skinned, so a joint bends rather than creases
//!
//! A ring says how far it is carried by the part's *end* joint rather than
//! its own. A thigh's top rings ride the hip and its bottom rings ride the
//! knee, so the surface bends through the knee instead of two rigid tubes
//! meeting at an angle and opening a wedge.
//!
//! The *last* ring of a spanning part is carried wholly by the far joint,
//! which is what puts the surface's end exactly where that joint is however
//! the chain is posed; the rings before it take the blend, which is what
//! makes the bend smooth instead of a crease.
//!
//! # Drawn as bands, so a limb has a lit side
//!
//! The visible half of each ring is split into a fixed number of arcs, and
//! each arc becomes one closed strip down the part, filled at the tone its
//! own surface normal takes from the light. A cylinder then reads as a
//! cylinder. The tone is quantised to a fixed ladder, so the whole figure is
//! still drawn in a small, exactly-known set of colours — which is what lets
//! the art harness count separable regions and check the palette by equality
//! rather than by eye.

use tairix_inline::ArrayVec;
use tairix_util::mathf;

use crate::error::FigureError;
use crate::frame::{Basis, Body};

/// How many cross-sections one part's mesh holds.
///
/// A bound on authored content: a part is a run of rings along one spine,
/// and a shape needing more of them is two parts.
pub const MAX_RINGS: usize = 6;

/// How many shaded strips the visible side of a part is drawn in.
///
/// Four reads as a curved surface — a highlight, two mid tones and a
/// terminator — without paying a fill per facet.
pub const BANDS: usize = BANDS_REAL as usize;

/// The same count as a real, which is what the arc arithmetic divides by.
const BANDS_REAL: u32 = 4;

/// How many boundaries those strips have.
pub const EDGES: usize = BANDS + 1;

/// How many tones the diffuse term is rounded to.
///
/// The figure is drawn in a small, exactly-known set of colours rather than
/// a continuum: every pixel is then one of the rig's declared tones at one
/// of these levels, which is what makes the art harness's palette check an
/// equality and its region count a count of things a player can tell apart.
pub const LEVELS: u32 = 6;

/// How dark the shaded side of a surface goes, as a fraction of its tone.
///
/// Not to nothing: a limb turned away from the light is still lit by the
/// sky, and a figure whose far side went black would read as a hole.
const AMBIENT: f64 = 0.42;

/// One cross-section of a part.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct Ring {
    /// Its centre, in the part's own frame.
    pub at: Body,
    /// Half-width across the part.
    pub wide: f64,
    /// Half-depth through it.
    pub deep: f64,
    /// How far it is carried by the part's end joint rather than its own,
    /// in `0..=1`.
    pub bind: f64,
}

impl Ring {
    /// A ring of `wide` by `deep` at `at`, carried wholly by the part's own
    /// joint.
    #[must_use]
    pub const fn new(at: Body, wide: f64, deep: f64) -> Self {
        Self {
            at,
            wide,
            deep,
            bind: 0.0,
        }
    }

    /// The same ring, carried `bind` of the way by the part's end joint.
    #[must_use]
    pub const fn bound(mut self, bind: f64) -> Self {
        self.bind = bind;
        self
    }

    /// Whether every dimension is a finite number and the bind is a real
    /// fraction.
    #[must_use]
    pub fn is_real(self) -> bool {
        self.at.is_real()
            && self.wide.is_finite()
            && self.deep.is_finite()
            && self.wide >= 0.0
            && self.deep >= 0.0
            && self.bind.is_finite()
            && (0.0..=1.0).contains(&self.bind)
    }

    /// How far it reaches from the part's own origin.
    #[must_use]
    pub fn reach(self) -> f64 {
        self.at.length() + mathf::fmax(self.wide, self.deep)
    }
}

/// One ring after the joints have carried it: where it sits and the two
/// half-axes its cross-section spans.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct Hoop {
    /// Its centre, in the figure's frame.
    pub at: Body,
    /// Its half-axis across the part.
    pub wide: Body,
    /// Its half-axis through the part.
    pub deep: Body,
}

impl Hoop {
    /// The surface point at `angle` round it, and the outward normal there.
    ///
    /// The normal is the ellipse's own, so a flattened cross-section shades
    /// as the flattened surface it is rather than as a circle.
    #[must_use]
    pub fn surface(self, angle: f64) -> (Body, Body) {
        let (sin, cos) = (mathf::sin(angle), mathf::cos(angle));
        let at = self
            .at
            .plus(self.wide.scaled(cos))
            .plus(self.deep.scaled(sin));
        // An ellipse's outward normal is the radius divided by the square of
        // the semi-axis it lies along, which is what makes a flattened
        // cross-section shade as the flat surface it is.
        let normal = self
            .wide
            .scaled(cos / squared(self.wide))
            .plus(self.deep.scaled(sin / squared(self.deep)));
        (at, unit(normal))
    }
}

/// Carry `rings` — stated in the frame `own` offset by `at` — through to the
/// figure's frame, skinning each across `own` and `end`.
///
/// `rest` is where the end joint sits in the part's own frame at rest, which
/// is what expresses a ring's position in the end joint's frame without a
/// second authored set of numbers.
///
/// # Errors
///
/// [`FigureError::TooManyParts`] for more rings than a part holds.
pub fn carry(
    rings: &[Ring],
    at: Body,
    own: (Body, Basis),
    end: Option<(Body, Basis)>,
    rest: Option<(Body, Basis)>,
) -> Result<ArrayVec<Hoop, MAX_RINGS>, FigureError> {
    let mut hoops: ArrayVec<Hoop, MAX_RINGS> = ArrayVec::new();
    let mut centres: ArrayVec<Body, MAX_RINGS> = ArrayVec::new();
    let mut frames: ArrayVec<Basis, MAX_RINGS> = ArrayVec::new();

    // A part with no end joint is carried wholly by its own, whatever its
    // rings claim.
    let bent = end.zip(rest);
    for ring in rings {
        let local = at.plus(ring.at);
        let here = own.0.plus(own.1.apply(local));
        let (centre, basis) = match bent {
            Some(((end_at, end_basis), (rest_at, rest_basis))) if ring.bind > 0.0 => {
                let in_end = rest_basis.unapply(local.plus(rest_at.scaled(-1.0)));
                let there = end_at.plus(end_basis.apply(in_end));
                (
                    here.scaled(1.0 - ring.bind).plus(there.scaled(ring.bind)),
                    blend(own.1, end_basis, ring.bind),
                )
            }
            _ => (here, own.1),
        };
        centres
            .try_push(centre)
            .map_err(|_| FigureError::TooManyParts)?;
        frames
            .try_push(basis)
            .map_err(|_| FigureError::TooManyParts)?;
    }

    for (index, ring) in rings.iter().enumerate() {
        let spine = spine(&centres, index, frames[index]);
        // The wide axis is the body's own side direction taken square to the
        // spine, so a limb's flattening keeps its anatomical sense however
        // the joint above it is turned.
        let side = frames[index].side;
        let wide = unit(side.plus(spine.scaled(-spine.dot(side))));
        let deep = unit(spine.cross(wide));
        hoops
            .try_push(Hoop {
                at: centres[index],
                wide: wide.scaled(ring.wide),
                deep: deep.scaled(ring.deep),
            })
            .map_err(|_| FigureError::TooManyParts)?;
    }
    Ok(hoops)
}

/// Which way the surface runs at ring `index`.
///
/// The difference to the neighbouring ring, so the cross-section stays
/// square to the surface where it bends; a lone ring falls back on the
/// frame's own down axis, which is the direction a part hangs in.
fn spine(centres: &[Body], index: usize, basis: Basis) -> Body {
    let before = index.checked_sub(1).and_then(|i| centres.get(i));
    let after = centres.get(index + 1);
    let here = centres[index];
    let along = match (before, after) {
        (Some(previous), Some(next)) => next.plus(previous.scaled(-1.0)),
        (None, Some(next)) => next.plus(here.scaled(-1.0)),
        (Some(previous), None) => here.plus(previous.scaled(-1.0)),
        (None, None) => Body::ORIGIN,
    };
    if along.length() > 0.0 {
        unit(along)
    } else {
        // A part with one ring, or two rings at one point, hangs down its
        // own joint like every other part does.
        basis.up.scaled(-1.0)
    }
}

/// `a` turned `t` of the way toward `b`, re-squared.
///
/// A straight interpolation of two orthonormal frames is neither
/// orthogonal nor unit, so the result is Gram-Schmidt'd back into a frame; a
/// blend of two frames a joint's own limit separates never approaches the
/// degenerate half-turn where that would fail.
fn blend(a: Basis, b: Basis, t: f64) -> Basis {
    let lerp = |x: Body, y: Body| x.scaled(1.0 - t).plus(y.scaled(t));
    let forward = unit(lerp(a.forward, b.forward));
    let side = unit(lerp(a.up, b.up).cross(forward));
    Basis {
        forward,
        side,
        up: unit(forward.cross(side)),
    }
}

/// A half-axis's squared length, never zero.
fn squared(axis: Body) -> f64 {
    let squared = axis.dot(axis);
    if squared > 0.0 {
        squared
    } else {
        1.0
    }
}

/// `direction` as a unit vector, or the up axis where it has no length.
fn unit(direction: Body) -> Body {
    let length = direction.length();
    if length > 0.0 {
        direction.scaled(1.0 / length)
    } else {
        Body::UP
    }
}

/// Where the near side of `hoop` begins, for a camera looking along `view`.
///
/// The visible half runs a half turn from there. A cross-section is an
/// ellipse, so the two points where its outward normal is square to the view
/// are a closed-form arctangent rather than a search.
#[must_use]
pub fn near(hoop: Hoop, view: Body) -> f64 {
    let along = hoop.wide.dot(view) / squared(hoop.wide);
    let through = hoop.deep.dot(view) / squared(hoop.deep);
    // The normal leans most toward the camera here, so the near half is the
    // quarter turn either side of it.
    mathf::atan2(through, along) - core::f64::consts::FRAC_PI_2
}

/// How far round a ring one shaded strip spans.
pub const STRIP: f64 = core::f64::consts::PI / BANDS_REAL as f64;

/// How lit a surface facing `normal` is, rounded to the tone ladder.
///
/// Rounded rather than continuous because the whole figure has to stay
/// drawable in a small, exactly-known set of colours: a continuum would make
/// every pixel its own tone and there would be nothing for the art harness
/// to count.
#[must_use]
pub fn level(normal: Body, light: Body) -> u32 {
    let facing = mathf::fmax(0.0, -normal.dot(light));
    let lit = AMBIENT + (1.0 - AMBIENT) * facing;
    // Bracketed into the ladder above, so the rounding cannot leave it.
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "`lit` is inside 0..=1 and the product is rounded below the \
                  ladder's own length"
    )]
    let step = (mathf::clamp(lit, 0.0, 1.0) * f64::from(LEVELS - 1) + 0.5) as u32;
    step.min(LEVELS - 1)
}

/// `tone` at ladder step `level`.
///
/// One definition, because the art harness derives the set of colours the
/// figure may paint in from exactly this and a rig's declared tones.
#[must_use]
pub fn shaded(tone: tairix_raster::Color, level: u32) -> tairix_raster::Color {
    let step = level.min(LEVELS - 1);
    let numerator = u32::from(u8::try_from(step).unwrap_or(0)) + LADDER_FLOOR;
    let denominator = LEVELS - 1 + LADDER_FLOOR;
    let scale =
        |channel: u8| u8::try_from(u32::from(channel) * numerator / denominator).unwrap_or(channel);
    tairix_raster::Color::rgba(scale(tone.r), scale(tone.g), scale(tone.b), tone.a)
}

/// How far up the ladder the darkest step sits.
///
/// The shaded side keeps most of its own colour: a figure whose far side
/// went to a fraction of its tone reads as two objects rather than one lit
/// from the side.
const LADDER_FLOOR: u32 = 7;

#[cfg(test)]
#[path = "mesh/tests.rs"]
mod tests;
