//! The contact shadow under a figure.
//!
//! A figure drawn on a ground plane with nothing beneath it floats, and the
//! eye reads a jump as the figure getting bigger rather than as it leaving
//! the ground. A blob under the feet fixes both: it says where the figure is
//! standing, and it *stays there* while the figure rises, which is the whole
//! trick that makes a jump readable.
//!
//! # Why the ellipse is solved rather than guessed
//!
//! The shadow is a circle on the ground stretched away from the light, and
//! the ground itself is foreshortened on the way to the screen. Stretching an
//! already-turned ellipse along a second, different axis does not give an
//! ellipse whose axes are either of those two — so taking the light's stretch
//! as the screen's would put the long axis visibly wrong at every bearing but
//! four. Composing the two maps and recovering the axes of what comes out is
//! a handful of arithmetic and is exactly right at every bearing.

use tairix_util::mathf;

use crate::error::FigureError;
use crate::frame::{Body, Heading, FORESHORTEN};
use tairix_raster::shape::{Placed, Shape};
use tairix_raster::Color;

/// How far a light may rake the shadow out before the stretch is capped.
///
/// A bound rather than a capacity: a contact shadow is the darkening where a
/// figure meets the ground, and a sun on the horizon does not make that
/// darkening a hundred times longer — it makes a *cast* shadow, which is a
/// different thing the scene owns and draws against its own geometry. Four
/// is a low evening sun and the point past which the two stop being the same
/// primitive.
const MAX_RAKE: f64 = 4.0;

/// Where the light comes from.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct Light {
    across: f64,
    into: f64,
    elevation: f64,
}

impl Light {
    /// A light travelling along `(across, into)` over the ground at
    /// `elevation` radians above the horizon.
    ///
    /// The ground direction is the way the light *travels*, so a shadow is
    /// thrown along it. It is normalised here, so a caller may hand over
    /// whatever vector the scene already holds.
    ///
    /// # Errors
    ///
    /// [`FigureError::LightUnreal`] for a direction that is not finite or has
    /// no length, or an elevation that is not above the horizon and at or
    /// below the vertical.
    pub fn new(across: f64, into: f64, elevation: f64) -> Result<Self, FigureError> {
        let length = mathf::hypot(across, into);
        if !across.is_finite() || !into.is_finite() || length <= 0.0 {
            return Err(FigureError::LightUnreal);
        }
        if !elevation.is_finite() || elevation <= 0.0 || elevation > core::f64::consts::FRAC_PI_2 {
            return Err(FigureError::LightUnreal);
        }
        Ok(Self {
            across: across / length,
            into: into / length,
            elevation,
        })
    }

    /// The direction it travels, in the frame of a figure facing along
    /// `heading`.
    ///
    /// The light is the scene's and the figure turns under it, so a surface
    /// normal can only be judged against it once it is expressed in the
    /// frame that normal is stated in.
    #[must_use]
    pub fn toward(self, heading: Heading) -> Body {
        let Heading { east, south } = heading;
        let flat = mathf::cos(self.elevation);
        let (across, into) = (self.across * flat, self.into * flat);
        Body::new(
            across * east + into * south,
            across * south - into * east,
            -mathf::sin(self.elevation),
        )
    }

    /// How much longer than wide the shadow it throws is.
    #[must_use]
    fn rake(self) -> f64 {
        mathf::fmin(1.0 / mathf::sin(self.elevation), MAX_RAKE)
    }
}

/// The darkening under a figure's feet.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct Contact {
    radius: f64,
    tone: Color,
}

impl Contact {
    /// A shadow of `radius` figure-local units in `tone`.
    ///
    /// `tone`'s alpha is what the shadow reaches at full contact; it fades
    /// from there as the figure leaves the ground.
    ///
    /// # Errors
    ///
    /// [`FigureError::GeometryUnreal`] for a radius that is not finite and
    /// positive.
    pub fn new(radius: f64, tone: Color) -> Result<Self, FigureError> {
        if !radius.is_finite() || radius <= 0.0 {
            return Err(FigureError::GeometryUnreal);
        }
        Ok(Self { radius, tone })
    }

    /// The shadow a figure `lift` above its ground point throws, drawn at
    /// `scale` around the surface point `at`.
    ///
    /// Painted before the figure: it is on the ground, and everything the
    /// figure is made of stands on it.
    ///
    /// As the figure rises the blob slides away along the light and spreads
    /// as it fades, which is the penumbra a real shadow grows — approximated
    /// by one ratio rather than simulated, because the cue a player reads is
    /// "the figure has left the ground", not the softness of its edge.
    ///
    /// # Errors
    ///
    /// [`FigureError::GeometryUnreal`] for a lift or an anchor that is not
    /// finite, and [`FigureError::ScaleUnreal`] for a scale that is not
    /// finite and positive.
    pub fn cast(
        self,
        light: Light,
        lift: f64,
        scale: f64,
        at: (f64, f64),
    ) -> Result<Placed, FigureError> {
        if !lift.is_finite() || !at.0.is_finite() || !at.1.is_finite() {
            return Err(FigureError::GeometryUnreal);
        }
        if !scale.is_finite() || scale <= 0.0 {
            return Err(FigureError::ScaleUnreal);
        }
        let height = mathf::fmax(lift, 0.0);
        // One ratio for both cues: the darkening a figure leaves behind
        // spreads as it thins.
        let soften = self.radius / (self.radius + height);

        // The ground ellipse: a circle raked out along the light's bearing.
        let (long, short) = (self.radius * light.rake() / soften, self.radius / soften);
        let bearing = mathf::atan2(light.into, light.across);
        let (sin, cos) = (mathf::sin(bearing), mathf::cos(bearing));
        // Turn the raked circle to the light's bearing, then foreshorten the
        // depth axis on the way to the screen.
        let ellipse = [
            [long * cos, -short * sin],
            [long * sin * FORESHORTEN, short * cos * FORESHORTEN],
        ];
        let (rx, ry, turn) = axes(ellipse);

        // A raised figure's shadow stays on the ground and slides away from
        // the light, which is what reads as height rather than as growth.
        let thrown = height / mathf::tan(light.elevation);
        let (dx, dy) = (light.across * thrown, light.into * thrown * FORESHORTEN);

        Ok(Placed {
            x: at.0 + dx * scale,
            y: at.1 + dy * scale,
            turn,
            shape: Shape::Superellipse {
                rx: rx * scale,
                ry: ry * scale,
                square: 0.0,
            },
            color: Color::rgba(
                self.tone.r,
                self.tone.g,
                self.tone.b,
                fade(self.tone.a, soften),
            ),
            seed: 0,
        })
    }
}

/// How many nested rings a softened shadow is drawn as.
pub const PENUMBRA: usize = 3;

/// How far out each ring of a softened shadow reaches, as a factor on the
/// hard shadow's own axes, outermost first — the order they are painted in.
const PENUMBRA_REACH: [f64; PENUMBRA] = [1.3, 1.15, 1.0];

impl Contact {
    /// The shadow a figure `lift` above its ground point throws, with its edge
    /// softened: [`PENUMBRA`] nested rings, painted outermost first, whose
    /// alphas compose to exactly the hard shadow's at the middle, where all of
    /// them overlap, and fall away in steps across the band between.
    ///
    /// A stepped penumbra rather than a blurred one, because it costs three
    /// fills where a blur costs a pass over every pixel it touches.
    ///
    /// # Errors
    ///
    /// As [`Self::cast`].
    pub fn penumbra(
        self,
        light: Light,
        lift: f64,
        scale: f64,
        at: (f64, f64),
    ) -> Result<[Placed; PENUMBRA], FigureError> {
        let hard = self.cast(light, lift, scale, at)?;
        let Shape::Superellipse { rx, ry, square } = hard.shape else {
            return Err(FigureError::GeometryUnreal);
        };
        let each = ring_alpha(hard.color.a);
        Ok(PENUMBRA_REACH.map(|reach| Placed {
            shape: Shape::Superellipse {
                rx: rx * reach,
                ry: ry * reach,
                square,
            },
            color: Color::rgba(hard.color.r, hard.color.g, hard.color.b, each),
            ..hard
        }))
    }
}

/// The least alpha each of [`PENUMBRA`] overlapping rings needs for the stack
/// of them to reach `alpha`.
///
/// Searched over the bytes a ring can hold rather than solved as a root: the
/// stack's alpha rises with the ring's, so a bisection finds the exact byte
/// in eight steps with no convergence to argue about.
fn ring_alpha(alpha: u8) -> u8 {
    let wanted = f64::from(alpha) / 255.0;
    let stacked = |each: u8| {
        let clear = 1.0 - f64::from(each) / 255.0;
        let mut through = 1.0;
        for _ in 0..PENUMBRA {
            through *= clear;
        }
        1.0 - through
    };
    let (mut low, mut high) = (0u8, alpha);
    while low < high {
        let middle = low + (high - low) / 2;
        if stacked(middle) < wanted {
            low = middle + 1;
        } else {
            high = middle;
        }
    }
    low
}

/// `alpha` thinned by `soften`.
///
/// Only the alpha: the shadow keeps its authored tone and changes how much of
/// it lands, so a warm shadow over sand stays warm as it fades.
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "`soften` is clamped into 0..=1 and the alpha is a u8, so the \
              rounded product is inside 0..=255 before the cast"
)]
fn fade(alpha: u8, soften: f64) -> u8 {
    (f64::from(alpha) * mathf::clamp(soften, 0.0, 1.0) + 0.5) as u8
}

/// The semi-axes and screen turn of the ellipse `map` takes the unit circle
/// to.
///
/// The singular values of a two-by-two are its image's semi-axes and its left
/// singular vectors their directions, both of which come out of four
/// arctangents in closed form — no iteration and no matrix library.
fn axes(map: [[f64; 2]; 2]) -> (f64, f64, f64) {
    let [[a, b], [c, d]] = map;
    let (sum, difference) = (f64::midpoint(a, d), (a - d) * 0.5);
    let (skew, spin) = (f64::midpoint(c, b), (c - b) * 0.5);
    let round = mathf::hypot(sum, spin);
    let stretch = mathf::hypot(difference, skew);
    let turn = f64::midpoint(mathf::atan2(spin, sum), mathf::atan2(skew, difference));
    (round + stretch, mathf::fabs(round - stretch), turn)
}

#[cfg(test)]
#[path = "shadow/tests.rs"]
mod tests;
