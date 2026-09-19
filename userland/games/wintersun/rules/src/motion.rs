//! Movement, and what bodies do when they run into things.
//!
//! Distance is never a number a client supplies. The realm takes the
//! direction a body is holding, multiplies it by the speed that body's own
//! stats and statuses allow, and resolves the result against the ground and
//! against everything else standing on it. A client claiming an impossible
//! step has not made a claim the realm reads.
//!
//! # All integer, on purpose
//!
//! A step is a fixed-point multiply, a floor and a remainder; a separation
//! is an exact integer square root. Nothing here is floating point, so the
//! cross-target claim rests on integer semantics Rust fully specifies rather
//! than on a libm agreeing with itself.
//!
//! # Sliding, and why the order is stated
//!
//! A body blocked on the diagonal tries the eastward part alone, then the
//! southward part alone, and stays put only if neither is clear. That is
//! what makes walking along a wall feel like walking along a wall rather
//! than sticking to it. The order matters — a body wedged in a corner with
//! both parts clear takes the eastward one — so it is stated here rather
//! than left to whichever branch came first.
//!
//! # The body is a box against the ground and a circle against bodies
//!
//! Terrain is tested over the bounding box of the body's circle, which is
//! conservative: a body cannot clip a corner into a wall, and pays for it
//! with a little slack at that corner. Body-against-body is the exact
//! circle test, because two players standing together is a thing players
//! look at and a box would read as wrong.

use tairix_wintersun_net::value::{WorldPoint, WorldVector};
use tairix_wintersun_world::geom::{CellCoord, CELL_SUB_UNITS};

use crate::bounds::MAX_SPEED_SUB_UNITS_PER_TICK;
use crate::entity::{Entity, RESIDUE_SCALE};
use crate::stat::Stats;
use crate::status::StatusSet;
use crate::terrain::{cell_at, occupiable, rise_legal, Terrain};

/// Where one body ends up after a step, and what it carries forward.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct Step {
    /// The resolved position.
    pub at: WorldPoint,
    /// The fractional part carried into the next step.
    pub residue: (i64, i64),
    /// How far it actually moved, for a client to extrapolate from.
    pub moved: WorldVector,
}

/// How fast a body may move this tick, in sub-units per tick.
///
/// Zero while stunned or rooted, because the status set says so rather than
/// because the movement path checks for it separately.
#[must_use]
pub fn effective_speed(stats: Stats, carried: &StatusSet) -> i32 {
    let base = i64::from(stats.speed_sub_units_per_tick());
    let scaled = base.saturating_mul(i64::from(carried.speed_permille())) / 1000;
    let clamped = scaled.clamp(0, i64::from(MAX_SPEED_SUB_UNITS_PER_TICK));
    i32::try_from(clamped).unwrap_or(MAX_SPEED_SUB_UNITS_PER_TICK)
}

/// Integrate one body's held direction into a resolved step.
#[must_use]
pub fn step(entity: &Entity, terrain: &impl Terrain) -> Step {
    let held = entity.held();
    let speed = effective_speed(entity.stats(), entity.status());
    let standing = Step {
        at: entity.at(),
        residue: (0, 0),
        moved: WorldVector::default(),
    };
    if speed == 0 || (held.x() == 0 && held.y() == 0) {
        return standing;
    }

    let (residue_x, residue_y) = entity.residue();
    let numerator = |component: i16, carried: i64| {
        i64::from(component)
            .saturating_mul(i64::from(speed))
            .saturating_add(carried)
    };
    let along_x = numerator(held.x(), residue_x);
    let along_y = numerator(held.y(), residue_y);
    // The remainder is the part below one sub-unit, so it is carried
    // whatever the collision did with the whole part: the fraction was not
    // refused, it simply has not accumulated yet.
    let residue = (
        along_x.rem_euclid(RESIDUE_SCALE),
        along_y.rem_euclid(RESIDUE_SCALE),
    );
    let (dx, dy) = (
        along_x.div_euclid(RESIDUE_SCALE),
        along_y.div_euclid(RESIDUE_SCALE),
    );

    let from = cell_at(entity.at());
    for (try_x, try_y) in [(dx, dy), (dx, 0), (0, dy)] {
        if try_x == 0 && try_y == 0 {
            continue;
        }
        let at = offset(entity.at(), try_x, try_y);
        if footprint_clear(terrain, from, at, entity.radius()) {
            return Step {
                at,
                residue,
                moved: WorldVector {
                    x: narrow(try_x),
                    y: narrow(try_y),
                },
            };
        }
    }
    Step {
        residue,
        ..standing
    }
}

/// Whether a body of `radius` may stand centred at `at`, arriving from the
/// cell `from`.
///
/// Fails closed on any cell the terrain does not have: a body cannot walk
/// off the edge of what the realm has generated.
#[must_use]
pub fn footprint_clear(
    terrain: &impl Terrain,
    from: CellCoord,
    at: WorldPoint,
    radius: u16,
) -> bool {
    let reach = i64::from(radius);
    let origin = terrain.cell(from);
    let west = cell_index(i64::from(at.x) - reach);
    let east = cell_index(i64::from(at.x) + reach);
    let north = cell_index(i64::from(at.y) - reach);
    let south = cell_index(i64::from(at.y) + reach);

    for row in north..=south {
        for column in west..=east {
            let cell = terrain.cell(CellCoord::new(column, row));
            if !occupiable(cell) || !rise_legal(origin, cell) {
                return false;
            }
        }
    }
    true
}

/// How far `near` must move to stop overlapping `far`, or `None` when they
/// do not overlap.
///
/// `far` takes the negation of the answer, so the pair separates
/// symmetrically. Call it with the pair ordered by identity: two bodies at
/// exactly the same point have no axis to separate along, and the ordering
/// is what decides which of them goes west — a stated rule rather than
/// whichever the iteration reached first.
#[must_use]
pub fn separation(
    near: WorldPoint,
    near_radius: u16,
    far: WorldPoint,
    far_radius: u16,
) -> Option<(i64, i64)> {
    let reach = i64::from(near_radius) + i64::from(far_radius);
    let dx = i64::from(far.x) - i64::from(near.x);
    let dy = i64::from(far.y) - i64::from(near.y);
    // The bounding box first, so the squares below are formed only for a
    // pair that could touch: two bodies at opposite ends of the world would
    // otherwise square a coordinate difference straight past `u64`.
    let reach_span = reach.unsigned_abs();
    if dx.unsigned_abs() >= reach_span || dy.unsigned_abs() >= reach_span {
        return None;
    }
    let span = dx.unsigned_abs().pow(2) + dy.unsigned_abs().pow(2);
    if span >= reach_span.pow(2) {
        return None;
    }

    // Each takes half, rounded up, so the pair actually parts rather than
    // resting exactly in contact and pushing again next tick.
    let distance = i64::try_from(span.isqrt()).unwrap_or(i64::MAX);
    let half = (reach - distance + 1) / 2;
    if distance == 0 {
        return Some((-((reach + 1) / 2), 0));
    }
    Some((-(dx * half) / distance, -(dy * half) / distance))
}

/// The cell index a sub-unit coordinate falls in, clamped to the lattice.
fn cell_index(sub_units: i64) -> i32 {
    let index = sub_units.div_euclid(i64::from(CELL_SUB_UNITS));
    i32::try_from(index).unwrap_or(if index.is_negative() {
        i32::MIN
    } else {
        i32::MAX
    })
}

/// Offset a point, clamping at the representable world rather than wrapping.
pub(crate) fn offset(at: WorldPoint, dx: i64, dy: i64) -> WorldPoint {
    let clamp = |value: i64| {
        i32::try_from(value).unwrap_or(if value.is_negative() {
            i32::MIN
        } else {
            i32::MAX
        })
    };
    WorldPoint {
        x: clamp(i64::from(at.x) + dx),
        y: clamp(i64::from(at.y) + dy),
    }
}

/// Narrow a per-tick displacement, which the speed bound keeps inside the
/// wire's own vector.
fn narrow(delta: i64) -> i16 {
    i16::try_from(delta).unwrap_or(if delta.is_negative() {
        i16::MIN
    } else {
        i16::MAX
    })
}

#[cfg(test)]
mod tests;
