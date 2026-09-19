//! The `WinterSun` palette.
//!
//! Every colour the ground is drawn from is here, and nowhere else. A
//! literal `Color::rgb(…)` anywhere else in the game's art is a second
//! palette to keep in step with this one.
//!
//! # Why a ramp and not a colour
//!
//! `WinterSun` is lit by a low sun, so a surface is never one colour: the
//! slope facing the sun is warm, the slope away from it is cold, and the
//! mid tone is what the material is between them. A palette of flat
//! colours cannot express that without every consumer inventing its own
//! darkening, so the unit here is a [`Ramp`] — shadow, mid, light — and a
//! material's grain, a slope's shading and a particle's tint all sample
//! the one ramp rather than deriving a tint apiece.
//!
//! The ramps are cold-biased, as the realm's name promises: the shadow
//! ends run blue, the light ends run to a pale straw rather than to
//! yellow, and nothing in the ground set is saturated.

use tairix_raster::color::Color;

/// A surface's three tones under a low sun.
///
/// `shadow` is the tone away from the light, `light` the tone into it, and
/// `mid` what the material reads as overall — not the average of the
/// other two, because a physical surface darkens faster than it brightens.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct Ramp {
    /// The tone facing away from the sun.
    pub shadow: Color,
    /// The tone the material reads as.
    pub mid: Color,
    /// The tone facing into the sun.
    pub light: Color,
}

impl Ramp {
    /// A ramp from its three opaque tones.
    #[must_use]
    pub const fn new(shadow: (u8, u8, u8), mid: (u8, u8, u8), light: (u8, u8, u8)) -> Self {
        Self {
            shadow: Color::rgb(shadow.0, shadow.1, shadow.2),
            mid: Color::rgb(mid.0, mid.1, mid.2),
            light: Color::rgb(light.0, light.1, light.2),
        }
    }

    /// The ramp at `t`, where `0` is [`shadow`](Self::shadow), `128` is
    /// [`mid`](Self::mid) and `255` is [`light`](Self::light).
    ///
    /// Two straight lines rather than one, so the mid tone is hit exactly
    /// and the darker half can fall away faster than the lighter half
    /// rises.
    #[must_use]
    pub fn sample(&self, t: u8) -> Color {
        if t < MID_STOP {
            lerp(self.shadow, self.mid, scale_to_byte(t, MID_STOP))
        } else {
            lerp(
                self.mid,
                self.light,
                scale_to_byte(t - MID_STOP, u8::MAX - MID_STOP),
            )
        }
    }
}

/// Where [`Ramp::mid`] sits on the `0..=255` sample axis.
const MID_STOP: u8 = 128;

/// `numerator / denominator` as a `0..=255` fraction, saturating at the top
/// and reading a zero denominator as a full one.
fn scale_to_byte(numerator: u8, denominator: u8) -> u8 {
    if denominator == 0 {
        return u8::MAX;
    }
    let scaled = u32::from(numerator) * u32::from(u8::MAX) / u32::from(denominator);
    u8::try_from(scaled.min(u32::from(u8::MAX))).unwrap_or(u8::MAX)
}

/// `a` toward `b` by `t`/255, per channel, rounded to nearest.
#[must_use]
pub fn lerp(a: Color, b: Color, t: u8) -> Color {
    Color::rgba(
        lerp_channel(a.r, b.r, t),
        lerp_channel(a.g, b.g, t),
        lerp_channel(a.b, b.b, t),
        lerp_channel(a.a, b.a, t),
    )
}

/// One channel of [`lerp`].
fn lerp_channel(a: u8, b: u8, t: u8) -> u8 {
    let (a, b, t) = (u32::from(a), u32::from(b), u32::from(t));
    let total = u32::from(u8::MAX);
    let mixed = a * (total - t) + b * t + total / 2;
    u8::try_from(mixed / total).unwrap_or(u8::MAX)
}

/// Open water, at depth.
pub const WATER: Ramp = Ramp::new((10, 24, 38), (22, 48, 72), (58, 96, 124));
/// Glacier ice: blue in shadow, near-white into the sun.
pub const GLACIER: Ramp = Ramp::new((122, 150, 176), (196, 214, 230), (240, 247, 252));
/// Lying snow, which is the brightest thing in the realm.
pub const SNOWFIELD: Ramp = Ramp::new((150, 164, 186), (218, 226, 238), (250, 252, 255));
/// Frozen ground and low scrub.
pub const TUNDRA: Ramp = Ramp::new((52, 56, 52), (98, 100, 86), (154, 152, 128));
/// Exposed upland heath, purple-brown.
pub const FELL_HEATH: Ramp = Ramp::new((48, 42, 50), (92, 78, 84), (144, 126, 122));
/// Dry cold grassland, pale and bleached.
pub const COLD_STEPPE: Ramp = Ramp::new((66, 62, 46), (124, 116, 84), (180, 172, 132));
/// Spruce and pine: nearly black in shadow, which is what makes a conifer
/// stand read as one.
pub const BOREAL_FOREST: Ramp = Ramp::new((18, 32, 30), (38, 64, 54), (76, 106, 80));
/// Broadleaf woodland, warmer and lighter than the boreal set.
pub const TEMPERATE_FOREST: Ramp = Ramp::new((28, 42, 28), (58, 82, 48), (104, 132, 78));
/// Wet peat.
pub const MOOR: Ramp = Ramp::new((30, 30, 26), (62, 58, 44), (104, 96, 70));
/// Tidal grass over mud.
pub const SALTMARSH: Ramp = Ramp::new((38, 46, 40), (76, 88, 68), (126, 134, 104));
/// Volcanic ash and clinker.
pub const ASHLAND: Ramp = Ramp::new((26, 24, 24), (58, 54, 52), (108, 100, 94));
/// Ground the world was torn through: the one place the palette is allowed
/// a hue that is not in the landscape.
pub const RIFT_WASTE: Ramp = Ramp::new((34, 20, 44), (72, 44, 86), (132, 96, 148));
/// Bare rock.
pub const ROCK: Ramp = Ramp::new((54, 56, 60), (104, 106, 110), (168, 170, 172));
/// River gravel and scree.
pub const GRAVEL: Ramp = Ramp::new((64, 62, 58), (116, 112, 104), (176, 172, 162));
/// Beach and dune sand.
pub const SAND: Ramp = Ramp::new((96, 88, 70), (158, 148, 120), (214, 206, 178));

/// Rain and sleet: near-colourless, and read by their streak rather than
/// their hue.
pub const RAIN: Ramp = Ramp::new((96, 108, 124), (150, 164, 182), (206, 216, 230));
/// Falling snow and hail.
pub const SNOWFALL: Ramp = Ramp::new((176, 186, 202), (226, 232, 242), (255, 255, 255));
/// Embers and sparks.
pub const EMBER: Ramp = Ramp::new((112, 30, 8), (206, 92, 22), (255, 196, 96));
/// Smoke, which is lit from one side like everything else.
pub const SMOKE: Ramp = Ramp::new((26, 26, 30), (72, 72, 78), (138, 140, 146));
/// Kicked-up dust and ash.
pub const DUST: Ramp = Ramp::new((70, 64, 54), (124, 114, 96), (180, 170, 148));
/// Splashed water.
pub const SPLASH: Ramp = Ramp::new((44, 72, 92), (96, 134, 156), (176, 206, 222));
/// Blown leaves and needles.
pub const LEAF: Ramp = Ramp::new((42, 48, 26), (94, 92, 44), (156, 142, 74));

#[cfg(test)]
mod tests;
