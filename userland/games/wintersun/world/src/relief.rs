//! Coarse relief: what the plates and the noise make of the ground, and
//! where the sea comes to.
//!
//! The heightfield is a weighted sum of three fields with different jobs.
//! Tectonic uplift decides *where* high ground is and gives a range its
//! direction. Domain-warped continental noise decides the shape of land
//! against sea. Ridged noise, admitted only in proportion to how strongly
//! a point sits in a mountain belt, gives the high ground its texture —
//! so ridges appear along belts and nowhere else.
//!
//! # Sea level is cut, not chosen
//!
//! A realm asks for a submerged fraction, not for a height. The stage
//! sorts the raw relief and cuts at the requested quantile, then shifts
//! every sample so the cut is zero. That way a realm's coastline is the
//! one the operator asked for whatever the noise happened to produce,
//! rather than an emergent property nobody controls.

use tairix_util::mathf;

use crate::error::WorldError;
use crate::geom::{quantise_u8, Elevation};
use crate::noise;
use crate::params::RealmParams;
use crate::realm::{try_filled, CoarseSample};
use crate::seed::{SeedKey, Stage};
use crate::uplift::Plates;

/// Cycles of continental noise across the realm's edge.
///
/// Realm-relative, not absolute: a continent is structure on the same
/// scale as a plate, so a bigger realm gets bigger continents rather than
/// more of them. Fine texture is the chunk stage's, and that *is*
/// absolute.
const CONTINENT_CYCLES: f64 = 2.5;

/// Cycles of ridge noise across the realm's edge.
const RIDGE_CYCLES: f64 = 9.0;

/// Cycles of the warp field across the realm's edge.
const WARP_CYCLES: f64 = 1.75;

/// Warp displacement, as a fraction of the realm's edge.
const WARP_STRENGTH: f64 = 0.22;

/// Weight of the continental field in raw relief.
const CONTINENT_WEIGHT: f64 = 0.55;

/// Weight of tectonic uplift in raw relief.
const UPLIFT_WEIGHT: f64 = 0.85;

/// Weight of the ridge field, before the belt strength scales it.
const RIDGE_WEIGHT: f64 = 0.45;

/// Fraction of the peak relief the deepest ocean floor reaches below sea
/// level. Ocean basins are shallower than mountains are tall, which is
/// what gives a coast a shelf rather than a cliff.
const OCEAN_DEPTH_FRACTION: f64 = 0.55;

/// Fill `samples` with raw relief, then cut sea level into it.
///
/// # Errors
///
/// [`WorldError::OutOfMemory`] if the quantile scratch does not fit.
pub fn solve(
    params: RealmParams,
    key: SeedKey,
    plates: Plates,
    samples: &mut [CoarseSample],
) -> Result<(), WorldError> {
    let side = params.coarse_samples();
    let mut raw = try_filled(samples.len(), 0.0_f64)?;

    for sy in 0..side {
        for sx in 0..side {
            let index = (sy as usize) * (side as usize) + (sx as usize);
            let (u, v) = realm_position(sx, sy, side);
            let (height, belt) = raw_relief(key, plates, u, v);
            raw[index] = height;
            samples[index].belt = quantise_u8(belt * f64::from(u8::MAX));
        }
    }

    let sea = sea_level(&raw, params.ocean_permille())?;
    let relief = f64::from(params.relief_units());
    let above = 1.0 - sea;
    let below = sea + 1.0;

    for (sample, height) in samples.iter_mut().zip(raw.iter().copied()) {
        // Two scales either side of the cut, so the requested submerged
        // fraction sets the coastline while the ocean stays shallower than
        // the mountains are tall.
        let units = if height >= sea {
            let t = if above > f64::EPSILON {
                (height - sea) / above
            } else {
                0.0
            };
            t * relief
        } else {
            let t = if below > f64::EPSILON {
                (sea - height) / below
            } else {
                0.0
            };
            -t * relief * OCEAN_DEPTH_FRACTION
        };
        sample.elevation = Elevation::from_units(units);
        sample.water = sample.elevation.max(Elevation::SEA_LEVEL);
    }

    Ok(())
}

/// A grid position as a fraction of the realm's edge.
fn realm_position(sx: u32, sy: u32, side: u32) -> (f64, f64) {
    (
        f64::from(sx) / f64::from(side),
        f64::from(sy) / f64::from(side),
    )
}

/// Raw relief in roughly `-1.0..1.0`, and the belt strength that shaped it.
fn raw_relief(key: SeedKey, plates: Plates, u: f64, v: f64) -> (f64, f64) {
    let (wu, wv) = noise::warp(key, u, v, WARP_CYCLES, WARP_STRENGTH);

    let grid = f64::from(plates.grid());
    let tectonics = plates.tectonics(wu * grid, wv * grid);

    let continent = noise::fbm(
        key,
        Stage::Continent,
        wu * CONTINENT_CYCLES,
        wv * CONTINENT_CYCLES,
    );
    let ridge = noise::ridged(key, Stage::Ridge, wu * RIDGE_CYCLES, wv * RIDGE_CYCLES);

    // Buoyancy is the plate's own standing height: a cratonic plate's
    // interior is land even where the noise is neutral, an oceanic plate's
    // is not. Without it the coastline would follow only the noise and
    // ignore the partition that is supposed to explain it.
    let standing = (tectonics.buoyancy - 0.5) * 0.6;

    let height = continent * CONTINENT_WEIGHT
        + tectonics.uplift * UPLIFT_WEIGHT
        + ridge * tectonics.belt * RIDGE_WEIGHT
        + standing;

    (mathf::clamp(height, -2.0, 2.0), tectonics.belt)
}

/// The raw-relief height below which `permille` parts per thousand of the
/// realm lies.
///
/// Sorts a quantised copy rather than the heights themselves: `f64` has no
/// total order, and a comparator that pretends otherwise is a sort whose
/// result depends on the input order. Quantising first gives integers,
/// which sort totally and identically everywhere.
fn sea_level(raw: &[f64], permille: u16) -> Result<f64, WorldError> {
    /// Steps a raw height is quantised into for the quantile search. Fine
    /// enough that the cut lands within a thousandth of the intended
    /// fraction on any realm the grid can hold.
    const STEPS: f64 = 1_048_576.0;

    if raw.is_empty() || permille == 0 {
        return Ok(f64::MIN);
    }
    let mut keys = try_filled(raw.len(), 0_i32)?;
    for (slot, height) in keys.iter_mut().zip(raw.iter().copied()) {
        *slot = mathf::round_i32(mathf::clamp(height, -2.0, 2.0) * STEPS);
    }
    keys.sort_unstable();

    let rank = (keys.len() * usize::from(permille)) / 1000;
    let index = rank.min(keys.len() - 1);
    Ok(f64::from(keys[index]) / STEPS)
}

#[cfg(test)]
mod tests;
