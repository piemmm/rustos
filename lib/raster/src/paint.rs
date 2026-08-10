//! What a fill is painted with: a flat colour or a gradient.
//!
//! A [`Paint`] answers one question — the colour at a point — so the scan
//! converter fills a shape without knowing whether the paint is flat or a
//! ramp, and a new paint kind never touches the fill.
//!
//! A [`Gradient`] is defined in its own *canonical* space and carries the
//! [`Affine`] that maps a shape's coordinates into it
//! ([`Gradient::to_gradient`]). Canonical space is deliberately trivial: a
//! linear gradient runs along the x axis from `0` to `1`, so its parameter is
//! simply the mapped `x`, and a radial gradient is the unit circle at the
//! origin, so its parameter is the distance from the origin. Every ellipse,
//! rotation, and `gradientUnits` convention a document can express is then one
//! matrix rather than a special case in the sampler.
//!
//! Sampling is total: no input produces a `NaN`, a division by zero, or a
//! panic. A gradient with no stops paints nothing, one with a single stop
//! paints that stop everywhere, and a parameter outside `0..=1` is brought
//! back inside it by the [`SpreadMethod`].

use alloc::vec::Vec;

use tairix_util::mathf;

use crate::affine::Affine;
use crate::color::Color;

/// How far from the centre of the unit circle a radial gradient's focal point
/// may sit.
///
/// SVG requires a focal point outside the circle to be moved just inside its
/// edge. Keeping it strictly inside is also what makes the focal-ray formula
/// total: on the edge the ray through a point on that same edge has zero
/// length, and the parameter would divide by zero.
const FOCAL_LIMIT: f64 = 0.99;

/// What a fill is painted with.
#[derive(Clone, Debug, PartialEq)]
pub enum Paint {
    /// One colour everywhere.
    Solid(Color),
    /// A colour that varies with position.
    Gradient(Gradient),
}

impl Paint {
    /// The straight-alpha colour this paint puts at `point`, in the
    /// coordinate space the filled geometry is authored in.
    #[must_use]
    pub fn sample(&self, point: (f64, f64)) -> Color {
        match self {
            Self::Solid(color) => *color,
            Self::Gradient(gradient) => gradient.sample(point),
        }
    }
}

/// A colour ramp: its geometry, its stops, and how it behaves outside them.
#[derive(Clone, Debug, PartialEq)]
pub struct Gradient {
    /// Whether the ramp runs along a line or out from a centre.
    pub kind: GradientKind,
    /// The colour ramp, ordered by ascending `offset` in `0..=1`.
    pub stops: Vec<GradientStop>,
    /// What happens outside `0..=1`.
    pub spread: SpreadMethod,
    /// Maps a point in the filled geometry's own coordinates into canonical
    /// gradient space.
    pub to_gradient: Affine,
}

impl Gradient {
    /// The straight-alpha colour at `point`, in the coordinate space the
    /// filled geometry is authored in.
    ///
    /// With no stops there is no colour to paint, so the answer is fully
    /// transparent; with one stop the ramp is that colour everywhere.
    #[must_use]
    pub fn sample(&self, point: (f64, f64)) -> Color {
        let (Some(first), Some(last)) = (self.stops.first(), self.stops.last()) else {
            return Color::TRANSPARENT;
        };
        let parameter = self
            .spread
            .wrap(self.kind.parameter(self.to_gradient.apply(point)));
        if parameter <= first.offset {
            return first.color;
        }
        if parameter >= last.offset {
            return last.color;
        }
        let index = self.stops.partition_point(|stop| stop.offset <= parameter);
        let (Some(below), Some(above)) = (
            index.checked_sub(1).and_then(|at| self.stops.get(at)),
            self.stops.get(index),
        ) else {
            return last.color;
        };
        let span = above.offset - below.offset;
        if span <= 0.0 {
            // Coincident offsets are a hard stop: the later colour wins.
            return above.color;
        }
        mix(below.color, above.color, (parameter - below.offset) / span)
    }
}

/// The geometry a gradient's parameter is measured along.
#[derive(Copy, Clone, Debug, PartialEq)]
pub enum GradientKind {
    /// The parameter is the x coordinate: the ramp runs from `x = 0` to
    /// `x = 1`.
    Linear,
    /// The parameter is the fraction of the way from `focal` to the unit
    /// circle, along the ray through the sampled point.
    Radial {
        /// Where the ramp starts, inside the unit circle. The origin is the
        /// plain concentric case.
        focal: (f64, f64),
    },
}

impl GradientKind {
    /// The ramp parameter at `point`, already in canonical gradient space and
    /// before the spread method brings it into `0..=1`.
    fn parameter(self, point: (f64, f64)) -> f64 {
        match self {
            Self::Linear => point.0,
            Self::Radial { focal } => radial_parameter(point, focal),
        }
    }
}

/// One colour in a ramp.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct GradientStop {
    /// Where along the ramp this colour sits, in `0..=1`.
    pub offset: f64,
    /// The colour itself, in straight alpha.
    pub color: Color,
}

/// What a gradient paints outside `0..=1`.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum SpreadMethod {
    /// The end colours continue outwards.
    Pad,
    /// The ramp repeats, mirrored each time, so the seams match.
    Reflect,
    /// The ramp repeats from the start, with a visible seam.
    Repeat,
}

impl SpreadMethod {
    /// `parameter` brought into `0..=1`.
    ///
    /// The final clamp is what makes an extreme or non-finite parameter — a
    /// point mapped through a near-degenerate transform, or one far outside a
    /// tiny radial gradient — resolve to an end colour rather than escaping
    /// as a `NaN`.
    fn wrap(self, parameter: f64) -> f64 {
        let wrapped = match self {
            Self::Pad => parameter,
            Self::Repeat => parameter - mathf::floor(parameter),
            Self::Reflect => {
                let doubled = parameter - 2.0 * mathf::floor(0.5 * parameter);
                if doubled > 1.0 {
                    2.0 - doubled
                } else {
                    doubled
                }
            }
        };
        mathf::clamp(wrapped, 0.0, 1.0)
    }
}

/// The focal-ray parameter of `point`: how far it lies from `focal` as a
/// fraction of the distance from `focal` to the unit circle along the same
/// ray.
///
/// Solving `|focal + s * (point - focal)| = 1` for the positive root gives the
/// `s` that reaches the circle, and the answer is `1/s`. The root exists and
/// is strictly positive for every focal point inside the circle, which is what
/// [`clamp_focal`] guarantees.
fn radial_parameter(point: (f64, f64), focal: (f64, f64)) -> f64 {
    let (focal_x, focal_y) = clamp_focal(focal);
    let (dx, dy) = (point.0 - focal_x, point.1 - focal_y);
    let squared = dx * dx + dy * dy;
    if !squared.is_finite() || squared <= 0.0 {
        // The focal point itself is where the ramp starts.
        return 0.0;
    }
    let along = focal_x * dx + focal_y * dy;
    let gap = 1.0 - (focal_x * focal_x + focal_y * focal_y);
    let reach = (mathf::sqrt(along * along + squared * gap) - along) / squared;
    if reach > 0.0 {
        1.0 / reach
    } else {
        // Only reachable if the subtraction above cancels to zero, which puts
        // the point at the circle's edge.
        1.0
    }
}

/// `focal` moved just inside the unit circle if it is not already there.
fn clamp_focal(focal: (f64, f64)) -> (f64, f64) {
    let (focal_x, focal_y) = focal;
    if !focal_x.is_finite() || !focal_y.is_finite() {
        // An unusable focal point degrades to the concentric case rather than
        // poisoning the ray arithmetic.
        return (0.0, 0.0);
    }
    let distance = mathf::hypot(focal_x, focal_y);
    if distance <= FOCAL_LIMIT {
        return (focal_x, focal_y);
    }
    let scale = FOCAL_LIMIT / distance;
    (focal_x * scale, focal_y * scale)
}

/// `from` at `fraction` zero and `to` at one, interpolated per channel in
/// straight alpha so a ramp that fades out keeps its hue instead of darkening
/// toward black.
fn mix(from: Color, to: Color, fraction: f64) -> Color {
    Color::rgba(
        mix_channel(from.r, to.r, fraction),
        mix_channel(from.g, to.g, fraction),
        mix_channel(from.b, to.b, fraction),
        mix_channel(from.a, to.a, fraction),
    )
}

/// One channel of [`mix`], rounded to the nearest level and kept in range for
/// any `fraction`.
fn mix_channel(from: u8, to: u8, fraction: f64) -> u8 {
    let start = f64::from(from);
    let value = start + (f64::from(to) - start) * fraction;
    u8::try_from(mathf::round_i32(mathf::clamp(value, 0.0, 255.0))).unwrap_or(u8::MAX)
}

#[cfg(test)]
#[path = "paint_tests.rs"]
mod tests;
