//! Drainage, lakes, discharge and erosion, over the whole realm at once.
//!
//! # The ordering problem, and how it is solved
//!
//! Flow accumulation is only meaningful if every cell is visited after
//! everything that drains through it, and that ordering does not exist
//! unless the drainage network is acyclic. A raw heightfield has pits and
//! flats, both of which produce cycles, and the usual fixes — nudging
//! every flat by an epsilon, or iterating until a fixed point — are either
//! numerically delicate or unbounded.
//!
//! Priority-Flood gives the ordering for free. Flooding the surface
//! inward from its outlets pops cells in non-decreasing filled height, so
//! **the pop order is itself a downstream-first ordering**: every cell is
//! popped after at least one neighbour it can drain to. Recording that
//! order gives three things at once — the depression-filled surface (so
//! lakes have a surface and an outflow), a guaranteed-acyclic routing
//! (each cell drains to its steepest neighbour *among those popped
//! earlier*), and the traversal order accumulation needs (the reverse).
//! No epsilon, no iteration to convergence, and no flats to special-case.
//!
//! # Erosion
//!
//! Detachment-limited stream-power incision (`E = K·√A·S`, the standard
//! `m = 1/2, n = 1` parameterisation) paired with linear hillslope
//! diffusion. That is deliberately a landscape-evolution model and not a
//! particle simulation: it is what cuts valleys along the drainage the
//! previous paragraph just established, and the diffusion is what lays the
//! flats in their floors. A bounded number of passes, each reading a
//! snapshot and writing a delta, so no pass depends on the order its cells
//! are visited in.

use alloc::collections::BinaryHeap;
use alloc::vec::Vec;
use core::cmp::Reverse;

use tairix_util::mathf;

use crate::error::WorldError;
use crate::geom::Elevation;
use crate::params::RealmParams;
use crate::realm::{try_filled, CoarseSample};

/// Erosion passes. Each is a full solve of the drainage, so the count is
/// what bounds the stage; four is where the incised network stops changing
/// shape and only deepens, which a further pass cannot improve.
const EROSION_PASSES: u32 = 4;

/// Stream-power incision coefficient.
const EROSION_K: f64 = 0.012;

/// Hillslope diffusion coefficient, per pass. Below a quarter, so the
/// explicit update is unconditionally stable on the five-point stencil.
const DIFFUSION: f64 = 0.16;

/// Steps a height is quantised into for the flood's ordering key.
///
/// The key orders the heap and nothing else — the filled surface itself
/// stays in `f64`. Fine enough that two genuinely different heights never
/// collide, coarse enough that the key stays far inside `i64`.
const ORDER_STEPS: f64 = 4096.0;

/// Where a coarse sample drains to.
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq, Ord, PartialOrd, Hash)]
#[repr(u8)]
pub enum FlowDir {
    /// Drains out of the model: the sea, a realm edge, or the one cell a
    /// closed basin's flood started from.
    #[default]
    Sink = 0,
    /// East.
    E = 1,
    /// South-east.
    Se = 2,
    /// South.
    S = 3,
    /// South-west.
    Sw = 4,
    /// West.
    W = 5,
    /// North-west.
    Nw = 6,
    /// North.
    N = 7,
    /// North-east.
    Ne = 8,
}

impl FlowDir {
    /// The grid offset this direction moves by, or `None` for a sink.
    #[must_use]
    pub const fn offset(self) -> Option<(i32, i32)> {
        match self {
            Self::Sink => None,
            Self::E => Some((1, 0)),
            Self::Se => Some((1, 1)),
            Self::S => Some((0, 1)),
            Self::Sw => Some((-1, 1)),
            Self::W => Some((-1, 0)),
            Self::Nw => Some((-1, -1)),
            Self::N => Some((0, -1)),
            Self::Ne => Some((1, -1)),
        }
    }
}

/// The eight neighbours, in the fixed order every tie is broken by.
///
/// The order is the direction enum's, so a tie resolves to the
/// lowest-numbered direction — a rule that does not depend on the grid,
/// the seed, or the traversal.
const NEIGHBOURS: [(FlowDir, i32, i32, f64); 8] = [
    (FlowDir::E, 1, 0, 1.0),
    (FlowDir::Se, 1, 1, core::f64::consts::SQRT_2),
    (FlowDir::S, 0, 1, 1.0),
    (FlowDir::Sw, -1, 1, core::f64::consts::SQRT_2),
    (FlowDir::W, -1, 0, 1.0),
    (FlowDir::Nw, -1, -1, core::f64::consts::SQRT_2),
    (FlowDir::N, 0, -1, 1.0),
    (FlowDir::Ne, 1, -1, core::f64::consts::SQRT_2),
];

/// One solve of the drainage over a height field.
struct Network {
    /// Depression-filled surface, in world units.
    filled: Vec<f64>,
    /// Each cell's downstream direction.
    flow: Vec<FlowDir>,
    /// Coarse cells draining through each cell, itself included.
    discharge: Vec<u32>,
}

/// Erode `samples`, then record the final drainage into them.
///
/// # Errors
///
/// [`WorldError::OutOfMemory`] if the working vectors do not fit.
pub fn solve(params: RealmParams, samples: &mut [CoarseSample]) -> Result<(), WorldError> {
    let side = params.coarse_samples();
    let mut height = try_filled(samples.len(), 0.0_f64)?;
    for (slot, sample) in height.iter_mut().zip(samples.iter()) {
        *slot = sample.elevation.units();
    }

    let step_units = f64::from(params.cells_per_coarse());
    for _ in 0..EROSION_PASSES {
        let network = Network::solve(&height, side)?;
        incise(&mut height, &network, side, step_units)?;
        diffuse(&mut height, side)?;
    }

    let network = Network::solve(&height, side)?;
    for (index, sample) in samples.iter_mut().enumerate() {
        let ground = Elevation::from_units(height[index]);
        sample.elevation = ground;
        // Standing water is the filled surface where the flood had to
        // raise the ground to get out — a lake — and sea level where the
        // ground is below it. Dry ground's water surface is the ground.
        sample.water = Elevation::from_units(network.filled[index]).max(if ground.is_submerged() {
            Elevation::SEA_LEVEL
        } else {
            ground
        });
        sample.flow = network.flow[index];
        sample.discharge = network.discharge[index];
    }
    Ok(())
}

impl Network {
    /// Flood, route, and accumulate one height field.
    fn solve(height: &[f64], side: u32) -> Result<Self, WorldError> {
        let flood = priority_flood(height, side)?;
        let flow = route(&flood.filled, &flood.rank, side)?;
        let discharge = accumulate(&flood.order, &flow, side)?;
        Ok(Self {
            filled: flood.filled,
            flow,
            discharge,
        })
    }
}

/// The heap ordering key of a height: quantised so it is totally ordered,
/// saturating so an extreme height cannot wrap past a modest one.
fn order_key(height: f64) -> i64 {
    let scaled = mathf::clamp(height * ORDER_STEPS, -9.0e15, 9.0e15);
    #[allow(
        clippy::cast_possible_truncation,
        reason = "clamped far inside i64 and rounded to an integer, so the \
                  conversion is exact"
    )]
    {
        mathf::round(scaled) as i64
    }
}

/// What one Priority-Flood pass produces.
struct Flood {
    /// Depression-filled surface, in world units.
    filled: Vec<f64>,
    /// Cell indices in pop order: downstream before upstream.
    order: Vec<u32>,
    /// Each cell's position in [`Self::order`].
    rank: Vec<u32>,
}

/// Priority-Flood: fill depressions from the outlets inward.
fn priority_flood(height: &[f64], side: u32) -> Result<Flood, WorldError> {
    let area = height.len();
    let mut filled = try_filled(area, 0.0_f64)?;
    let mut rank = try_filled(area, u32::MAX)?;
    let mut queued = try_filled(area, false)?;
    let mut order = Vec::new();
    order
        .try_reserve_exact(area)
        .map_err(|_| WorldError::OutOfMemory)?;

    let mut heap: BinaryHeap<Reverse<(i64, u32)>> = BinaryHeap::new();
    heap.try_reserve(area)
        .map_err(|_| WorldError::OutOfMemory)?;

    // Outlets: the realm's rim, and everything already at or below sea
    // level. Both drain out of the model, so both are where the flood
    // starts and neither can be raised by it.
    for sy in 0..side {
        for sx in 0..side {
            let index = flat(sx, sy, side);
            let rim = sx == 0 || sy == 0 || sx + 1 == side || sy + 1 == side;
            if rim || height[index] <= 0.0 {
                filled[index] = height[index];
                queued[index] = true;
                heap.push(Reverse((order_key(height[index]), index_u32(index))));
            }
        }
    }

    while let Some(Reverse((_, raw))) = heap.pop() {
        let index = raw as usize;
        rank[index] = index_u32(order.len());
        order.push(raw);

        let (sx, sy) = unflat(index, side);
        for (_, dx, dy, _) in NEIGHBOURS {
            let Some(next) = neighbour(sx, sy, dx, dy, side) else {
                continue;
            };
            if queued[next] {
                continue;
            }
            // A neighbour cannot end up below the cell it was reached
            // from: that is the fill, and it is what turns a pit into a
            // lake surface at its outflow height.
            filled[next] = mathf::fmax(height[next], filled[index]);
            queued[next] = true;
            heap.push(Reverse((order_key(filled[next]), index_u32(next))));
        }
    }

    Ok(Flood {
        filled,
        order,
        rank,
    })
}

/// Route every cell to its steepest neighbour among those the flood
/// reached earlier.
///
/// Restricting the choice to earlier-ranked neighbours is what makes the
/// network acyclic: a cycle would need an edge to an equal-or-later rank,
/// and there is none.
fn route(filled: &[f64], rank: &[u32], side: u32) -> Result<Vec<FlowDir>, WorldError> {
    let mut flow = try_filled(filled.len(), FlowDir::Sink)?;

    for sy in 0..side {
        for sx in 0..side {
            let index = flat(sx, sy, side);
            // The rim and the sea drain out of the model rather than to a
            // neighbour, so they stay sinks whatever is next to them.
            if sx == 0 || sy == 0 || sx + 1 == side || sy + 1 == side || filled[index] <= 0.0 {
                continue;
            }
            let mut steepest = 0.0;
            for (dir, dx, dy, distance) in NEIGHBOURS {
                let Some(next) = neighbour(sx, sy, dx, dy, side) else {
                    continue;
                };
                if rank[next] >= rank[index] {
                    continue;
                }
                let slope = (filled[index] - filled[next]) / distance;
                if slope > steepest {
                    steepest = slope;
                    flow[index] = dir;
                }
            }
            if flow[index] == FlowDir::Sink {
                // A flat: every earlier neighbour is level with this cell,
                // so there is no steepest. Take the earliest-reached one,
                // which is the way the flood came and therefore the way
                // out.
                let mut earliest = rank[index];
                for (dir, dx, dy, _) in NEIGHBOURS {
                    let Some(next) = neighbour(sx, sy, dx, dy, side) else {
                        continue;
                    };
                    if rank[next] < earliest {
                        earliest = rank[next];
                        flow[index] = dir;
                    }
                }
            }
        }
    }
    Ok(flow)
}

/// Accumulate one unit per cell down the network.
fn accumulate(order: &[u32], flow: &[FlowDir], side: u32) -> Result<Vec<u32>, WorldError> {
    let mut discharge = try_filled(flow.len(), 1_u32)?;
    // Upstream first: the reverse of the flood order, in which every cell
    // precedes the one it drains to.
    for raw in order.iter().rev().copied() {
        let index = raw as usize;
        let (sx, sy) = unflat(index, side);
        let Some((dx, dy)) = flow[index].offset() else {
            continue;
        };
        let Some(next) = neighbour(sx, sy, dx, dy, side) else {
            continue;
        };
        // Bounded by the cell count, which is at most the square of the
        // coarse side and so far inside `u32`.
        discharge[next] += discharge[index];
    }
    Ok(discharge)
}

/// Stream-power incision along the drainage.
fn incise(
    height: &mut [f64],
    network: &Network,
    side: u32,
    step_units: f64,
) -> Result<(), WorldError> {
    let mut cut = try_filled(height.len(), 0.0_f64)?;

    for sy in 0..side {
        for sx in 0..side {
            let index = flat(sx, sy, side);
            if height[index] <= 0.0 {
                continue;
            }
            let Some((dx, dy)) = network.flow[index].offset() else {
                continue;
            };
            let Some(next) = neighbour(sx, sy, dx, dy, side) else {
                continue;
            };
            let distance = if dx == 0 || dy == 0 {
                step_units
            } else {
                step_units * core::f64::consts::SQRT_2
            };
            let drop = network.filled[index] - network.filled[next];
            if drop <= 0.0 {
                continue;
            }
            let area = f64::from(network.discharge[index]);
            let incision = EROSION_K * mathf::sqrt(area) * (drop / distance);
            // Never below the cell downstream: incision deepens a valley,
            // it does not invert the gradient that drives it.
            cut[index] = mathf::fmin(incision, mathf::fmax(height[index] - height[next], 0.0));
        }
    }

    for (slot, amount) in height.iter_mut().zip(cut.iter().copied()) {
        if amount > 0.0 {
            // Floored at sea level: erosion cuts valleys, it does not
            // enlarge the ocean the operator asked for. Only cells the
            // pass actually cut are touched, so a sea floor — which was
            // never a candidate — is not raised to the floor.
            *slot = mathf::fmax(*slot - amount, 0.0);
        }
    }
    Ok(())
}

/// Linear hillslope diffusion: the sediment half of the model.
fn diffuse(height: &mut [f64], side: u32) -> Result<(), WorldError> {
    let before = {
        let mut copy = try_filled(height.len(), 0.0_f64)?;
        copy.copy_from_slice(height);
        copy
    };

    for sy in 0..side {
        for sx in 0..side {
            let index = flat(sx, sy, side);
            if before[index] <= 0.0 {
                continue;
            }
            let mut total = 0.0;
            let mut count = 0.0;
            for (_, dx, dy, distance) in NEIGHBOURS {
                if distance > 1.0 {
                    continue;
                }
                let Some(next) = neighbour(sx, sy, dx, dy, side) else {
                    continue;
                };
                total += before[next];
                count += 1.0;
            }
            if count == 0.0 {
                continue;
            }
            let mean = total / count;
            height[index] = mathf::fmax(before[index] + DIFFUSION * (mean - before[index]), 0.0);
        }
    }
    Ok(())
}

/// The row-major index of an in-range grid position.
fn flat(sx: u32, sy: u32, side: u32) -> usize {
    (sy as usize) * (side as usize) + (sx as usize)
}

/// The grid position of a row-major index.
fn unflat(index: usize, side: u32) -> (u32, u32) {
    #[allow(
        clippy::cast_possible_truncation,
        reason = "the index is below the grid's area, so both components \
                  are below the side, which is itself a u32"
    )]
    {
        (
            (index % (side as usize)) as u32,
            (index / (side as usize)) as u32,
        )
    }
}

/// The index of the neighbour at `(dx, dy)`, or `None` off the grid.
fn neighbour(sx: u32, sy: u32, dx: i32, dy: i32, side: u32) -> Option<usize> {
    let nx = i64::from(sx) + i64::from(dx);
    let ny = i64::from(sy) + i64::from(dy);
    if nx < 0 || ny < 0 || nx >= i64::from(side) || ny >= i64::from(side) {
        return None;
    }
    #[allow(
        clippy::cast_sign_loss,
        clippy::cast_possible_truncation,
        reason = "both components were just bounds-checked into 0..side"
    )]
    {
        Some(flat(nx as u32, ny as u32, side))
    }
}

/// Narrow a grid index to the `u32` the order and rank vectors hold.
fn index_u32(index: usize) -> u32 {
    #[allow(
        clippy::cast_possible_truncation,
        reason = "the grid's area is at most MAX_COARSE_SAMPLES squared, \
                  which is 2^18"
    )]
    {
        index as u32
    }
}

#[cfg(test)]
mod tests;
