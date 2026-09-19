//! Temperature, and the moisture the wind carries inland.
//!
//! Both are computed over the whole coarse grid at once, for the same
//! reason drainage is: a rain shadow is a record of everything the wind
//! crossed before it arrived, so it cannot be answered from a window.
//!
//! # Temperature
//!
//! A latitudinal gradient between the realm's two declared edge
//! temperatures, an environmental lapse rate with elevation, and a small
//! cooling with distance from open water — which is what makes a
//! continental interior colder than a coast at the same latitude. A little
//! keyed noise on top, so isotherms are not drawn with a ruler.
//!
//! # Moisture
//!
//! One sweep along the prevailing wind. Air leaves water saturated, loses
//! a fraction of what it carries at every step, and loses much more where
//! it is forced to rise. What falls is the precipitation a biome sees;
//! what is left crosses the ridge, which is why the lee side is dry. The
//! sweep is ordered by each cell's projection onto the wind, so every cell
//! is visited after the one upwind of it — the same downstream-first
//! discipline hydrology uses, with the wind in place of gravity.

use alloc::vec::Vec;

use tairix_util::mathf;

use crate::error::WorldError;
use crate::geom::signed;
use crate::geom::{Moisture, Temperature};
use crate::noise;
use crate::params::RealmParams;
use crate::realm::{clamped_index, try_filled, CoarseSample};
use crate::seed::{SeedKey, Stage};

/// Environmental lapse rate, in degrees Celsius per world unit of
/// elevation — 6.5 K/km at a unit to the metre.
const LAPSE_RATE: f64 = 0.0065;

/// Coldest a continental interior runs below its coast, in degrees.
const CONTINENTALITY_COOLING: f64 = 6.0;

/// Distance from water, in coarse samples, at which continentality reaches
/// half its effect.
const CONTINENTALITY_HALF: f64 = 12.0;

/// Amplitude of the temperature jitter, in degrees.
const TEMPERATURE_JITTER: f64 = 1.6;

/// Cycles of that jitter across the realm's edge.
const JITTER_CYCLES: f64 = 6.0;

/// Fraction of its moisture the air keeps crossing one coarse sample of
/// level ground.
const TRANSPORT: f64 = 0.985;

/// Rise, in world units across one coarse step, that wrings out all the
/// air can lose to orographic lift.
const OROGRAPHIC_RISE: f64 = 260.0;

/// Fraction of the carried moisture orographic lift can take at most.
const OROGRAPHIC_STRENGTH: f64 = 0.75;

/// Fraction of the carried moisture that falls on level ground anyway.
const BASELINE_RAIN: f64 = 0.06;

/// Moisture the air picks up crossing one coarse sample of open water.
const EVAPORATION: f64 = 0.5;

/// Humidity of the air arriving over the realm's upwind edge.
///
/// The world does not stop at the rim, and a realm whose windward edge is
/// land would otherwise open with a desert strip that nothing in the
/// terrain explains.
const RIM_HUMIDITY: f64 = 0.55;

/// Fill in temperature and moisture.
///
/// # Errors
///
/// [`WorldError::OutOfMemory`] if the working vectors do not fit.
pub fn solve(
    params: RealmParams,
    key: SeedKey,
    samples: &mut [CoarseSample],
) -> Result<(), WorldError> {
    let side = params.coarse_samples();
    let distance = water_distance(samples, side)?;
    temperature(params, key, samples, &distance);
    moisture(params, samples, side)
}

/// Chebyshev distance from each sample to the nearest open water, in
/// coarse samples.
///
/// A breadth-first flood from every water sample at once, so the result is
/// a pure function of the water mask and not of the order the sources were
/// found in.
fn water_distance(samples: &[CoarseSample], side: u32) -> Result<Vec<u32>, WorldError> {
    let mut distance = try_filled(samples.len(), u32::MAX)?;
    let mut frontier = Vec::new();
    frontier
        .try_reserve(samples.len())
        .map_err(|_| WorldError::OutOfMemory)?;

    for (index, sample) in samples.iter().enumerate() {
        if sample.is_water() {
            distance[index] = 0;
            frontier.push(index);
        }
    }

    let mut read = 0;
    while read < frontier.len() {
        let index = frontier[read];
        read += 1;
        let step = distance[index] + 1;
        let (sx, sy) = grid_of(index, side);
        for (dx, dy) in [(1_i32, 0_i32), (0, 1), (-1, 0), (0, -1)] {
            let (Some(nx), Some(ny)) = (sx.checked_add_signed(dx), sy.checked_add_signed(dy))
            else {
                continue;
            };
            if nx >= side || ny >= side {
                continue;
            }
            let next = (ny as usize) * (side as usize) + (nx as usize);
            if distance[next] <= step {
                continue;
            }
            distance[next] = step;
            frontier.push(next);
        }
    }
    Ok(distance)
}

/// Latitude, lapse rate, continentality, jitter.
fn temperature(params: RealmParams, key: SeedKey, samples: &mut [CoarseSample], distance: &[u32]) {
    let side = params.coarse_samples();
    let north = params.north_temperature().celsius();
    let south = params.south_temperature().celsius();
    let span = f64::from(side.saturating_sub(1)).max(1.0);

    for (index, sample) in samples.iter_mut().enumerate() {
        let sx = index % (side as usize);
        let sy = index / (side as usize);
        #[allow(
            clippy::cast_precision_loss,
            reason = "a grid coordinate is below MAX_COARSE_SAMPLES"
        )]
        let (u, v) = (sx as f64 / span, sy as f64 / span);

        let latitude = crate::geom::lerp(north, south, v);
        let altitude = mathf::fmax(sample.elevation.units(), 0.0) * LAPSE_RATE;

        let inland = if distance[index] == u32::MAX {
            1.0
        } else {
            let d = f64::from(distance[index]);
            d / (d + CONTINENTALITY_HALF)
        };

        let jitter = noise::fbm(key, Stage::Climate, u * JITTER_CYCLES, v * JITTER_CYCLES)
            * TEMPERATURE_JITTER;

        sample.temperature = Temperature::from_celsius(
            latitude - altitude - inland * CONTINENTALITY_COOLING + jitter,
        );
    }
}

/// One advection sweep along the prevailing wind.
fn moisture(
    params: RealmParams,
    samples: &mut [CoarseSample],
    side: u32,
) -> Result<(), WorldError> {
    let (wx, wy) = params.wind().unit_vector();
    let upwind = upwind_offset(wx, wy);
    let order = wind_order(samples.len(), side, wx, wy)?;

    // What the air is carrying, as distinct from what fell out of it: the
    // biome reads the rain, the next cell downwind reads the air.
    let mut carried = try_filled(samples.len(), 0.0_f64)?;

    for raw in order {
        let index = raw as usize;
        let (sx, sy) = grid_of(index, side);
        let source = clamped_index(signed(sx) - upwind.0, signed(sy) - upwind.1, side);

        if samples[index].is_water() {
            // Open water saturates the air crossing it and is itself as wet
            // as ground gets.
            carried[index] = mathf::fmin(carried[source] + EVAPORATION, 1.0);
            samples[index].moisture = Moisture::from_fraction(1.0);
            continue;
        }

        // A cell on the upwind rim has no modelled neighbour to take air
        // from — its source clamps onto itself — so it is given the
        // humidity of air arriving over the realm's edge rather than none.
        let incoming = if source == index {
            RIM_HUMIDITY
        } else {
            carried[source] * TRANSPORT
        };
        let rise = samples[index].elevation.units() - samples[source].elevation.units();
        let lift = mathf::clamp(rise / OROGRAPHIC_RISE, 0.0, 1.0);
        let share = BASELINE_RAIN + lift * OROGRAPHIC_STRENGTH;
        let fell = incoming * mathf::fmin(share, 1.0);

        carried[index] = incoming - fell;
        // Scaled against the share a saturated air mass drops on level
        // ground, so a coastal plain reads as damp rather than as the
        // near-zero fraction the raw depth would give.
        samples[index].moisture = Moisture::from_fraction(fell / BASELINE_RAIN);
    }
    Ok(())
}

/// The grid position of a row-major index.
fn grid_of(index: usize, side: u32) -> (u32, u32) {
    #[allow(
        clippy::cast_possible_truncation,
        reason = "the index is below the grid's area, so each component is \
                  below MAX_COARSE_SAMPLES"
    )]
    {
        (
            (index % (side as usize)) as u32,
            (index / (side as usize)) as u32,
        )
    }
}

/// The grid offset of the neighbour the wind arrives from.
fn upwind_offset(wx: f64, wy: f64) -> (i32, i32) {
    (step_of(wx), step_of(wy))
}

/// One axis of a wind direction, as the grid step it favours.
fn step_of(component: f64) -> i32 {
    /// Below this the wind is treated as having no component on the axis,
    /// so a near-axial wind advects along one row rather than staggering.
    const DEADBAND: f64 = 0.3827;
    if component > DEADBAND {
        1
    } else if component < -DEADBAND {
        -1
    } else {
        0
    }
}

/// Cell indices ordered by their projection onto the wind, upwind first.
///
/// The projection is quantised to an integer before sorting, for the same
/// reason the flood's key is: an `f64` comparator has no total order, and a
/// sort that pretends otherwise depends on the input order. The index is
/// the tiebreak, so equal projections resolve the same way everywhere.
fn wind_order(area: usize, side: u32, wx: f64, wy: f64) -> Result<Vec<u32>, WorldError> {
    /// Steps per coarse sample in the projection key.
    const STEPS: f64 = 4096.0;

    let mut keyed = try_filled(area, (0_i64, 0_u32))?;
    for (index, slot) in keyed.iter_mut().enumerate() {
        #[allow(
            clippy::cast_precision_loss,
            reason = "a grid coordinate is below MAX_COARSE_SAMPLES"
        )]
        let (x, y) = (
            (index % (side as usize)) as f64,
            (index / (side as usize)) as f64,
        );
        #[allow(
            clippy::cast_possible_truncation,
            reason = "the projection is bounded by the grid's diagonal times \
                      STEPS, far inside i64"
        )]
        let key = mathf::round((x * wx + y * wy) * STEPS) as i64;
        #[allow(
            clippy::cast_possible_truncation,
            reason = "the area is at most MAX_COARSE_SAMPLES squared"
        )]
        {
            *slot = (key, index as u32);
        }
    }
    keyed.sort_unstable();

    let mut order = Vec::new();
    order
        .try_reserve_exact(area)
        .map_err(|_| WorldError::OutOfMemory)?;
    order.extend(keyed.iter().map(|&(_, index)| index));
    Ok(order)
}

#[cfg(test)]
mod tests;
