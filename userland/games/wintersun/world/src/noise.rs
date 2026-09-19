//! The noise the relief stages are shaped from.
//!
//! Value noise on an integer lattice, smoothstep-interpolated, summed over
//! octaves. Value rather than gradient noise because its lattice value is
//! one keyed hash — random access with no gradient table to keep identical
//! across targets — and because the octave sum, the ridging and the domain
//! warp are what give terrain its character, not the choice of kernel.
//!
//! Everything here is a pure function of `(key, stage, position)`. No state,
//! no caching, no traversal order: the value at a point is the same whether
//! it is the first point asked for or the millionth.

use tairix_util::mathf;

use crate::geom::{lerp, smoothstep};
use crate::seed::{SeedKey, Stage};

/// Octaves the summed forms use.
///
/// Five is where the amplitude of the next octave falls below the quantised
/// output's resolution over the amplitudes relief actually uses, so a sixth
/// would cost a hash per sample and change nothing that is stored.
pub const OCTAVES: u32 = 5;

/// Amplitude ratio between an octave and the next.
pub const PERSISTENCE: f64 = 0.5;

/// Frequency ratio between an octave and the next.
pub const LACUNARITY: f64 = 2.0;

/// Value noise at `(x, y)` in lattice units, in `-1.0..1.0`.
///
/// The lattice cell's four corners are hashed and bilinearly blended under
/// [`smoothstep`], so the field is continuous and has a continuous first
/// derivative across a cell boundary.
#[must_use]
pub fn value(key: SeedKey, stage: Stage, x: f64, y: f64) -> f64 {
    let x0 = mathf::floor(x);
    let y0 = mathf::floor(y);
    let (ix, iy) = (lattice_index(x0), lattice_index(y0));
    let (tx, ty) = (smoothstep(x - x0), smoothstep(y - y0));

    let top = lerp(
        key.signed(stage, ix, iy),
        key.signed(stage, ix.wrapping_add(1), iy),
        tx,
    );
    let bottom = lerp(
        key.signed(stage, ix, iy.wrapping_add(1)),
        key.signed(stage, ix.wrapping_add(1), iy.wrapping_add(1)),
        tx,
    );
    lerp(top, bottom, ty)
}

/// Fractional Brownian motion: [`OCTAVES`] octaves of [`value`], in
/// `-1.0..1.0`.
///
/// Each octave takes its own lattice offset from the octave number, so the
/// octaves are independent rather than the same field rescaled — which
/// would leave a visible grid where their maxima coincide.
#[must_use]
pub fn fbm(key: SeedKey, stage: Stage, x: f64, y: f64) -> f64 {
    summed(key, stage, x, y, |sample| sample)
}

/// Ridged multifractal noise, in `0.0..1.0`, peaking along the zero
/// crossings of the underlying field.
///
/// This is what makes a mountain belt read as a chain of ridges and
/// valleys rather than as lumps.
#[must_use]
pub fn ridged(key: SeedKey, stage: Stage, x: f64, y: f64) -> f64 {
    let signed = summed(key, stage, x, y, |sample| 1.0 - mathf::fabs(sample));
    mathf::clamp(signed, 0.0, 1.0)
}

/// Billowed noise, in `0.0..1.0`: rounded mounds, for dunes.
#[must_use]
pub fn billow(key: SeedKey, stage: Stage, x: f64, y: f64) -> f64 {
    let signed = summed(key, stage, x, y, mathf::fabs);
    mathf::clamp(signed, 0.0, 1.0)
}

/// `(x, y)` displaced by an independent noise field of amplitude
/// `strength` lattice units.
///
/// Warping the *input* of a relief field is what turns the isotropic blur
/// of plain fBm into ground with grain: coastlines gain inlets, ranges gain
/// a direction. The displacement uses two offset samples of one field
/// rather than two fields, which costs one stage tag instead of two and
/// gives an equally uncorrelated pair.
#[must_use]
pub fn warp(key: SeedKey, x: f64, y: f64, frequency: f64, strength: f64) -> (f64, f64) {
    /// Offsets picked to be far apart in lattice space and not a multiple
    /// of the lattice period, so the two samples decorrelate.
    const DX: f64 = 41.5;
    /// The second offset, likewise.
    const DY: f64 = 137.25;

    let u = fbm(key, Stage::Warp, x * frequency, y * frequency);
    let v = fbm(key, Stage::Warp, x * frequency + DX, y * frequency + DY);
    (x + u * strength, y + v * strength)
}

/// Sum [`OCTAVES`] octaves, shaping each sample with `shape` before it is
/// weighted, and normalise by the total weight so the result keeps the
/// shaped sample's range.
fn summed(key: SeedKey, stage: Stage, x: f64, y: f64, shape: impl Fn(f64) -> f64) -> f64 {
    /// Lattice offset between octaves, so no two octaves share a cell
    /// boundary and their grids cannot align into a visible lattice.
    const OCTAVE_OFFSET: f64 = 17.0;

    let mut total = 0.0;
    let mut weight = 0.0;
    let mut amplitude = 1.0;
    let mut frequency = 1.0;

    for octave in 0..OCTAVES {
        let shift = f64::from(octave) * OCTAVE_OFFSET;
        let sample = value(key, stage, x * frequency + shift, y * frequency + shift);
        total += shape(sample) * amplitude;
        weight += amplitude;
        amplitude *= PERSISTENCE;
        frequency *= LACUNARITY;
    }

    total / weight
}

/// The integer lattice index of a floored coordinate.
///
/// Saturates rather than wrapping: a realm's validated extent keeps every
/// sampled coordinate far inside this, and a caller that somehow reached
/// the edge gets the edge's value rather than one from the far side of the
/// lattice.
fn lattice_index(floored: f64) -> i32 {
    mathf::round_i32(mathf::clamp(
        floored,
        f64::from(i32::MIN + 1),
        f64::from(i32::MAX - 1),
    ))
}

#[cfg(test)]
mod tests;
