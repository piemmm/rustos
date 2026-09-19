//! The collision field: what the ground says about where a body may stand.
//!
//! The simulation needs two numbers per cell — how high the ground is and
//! how high the water over it stands — and nothing else. So the seam is a
//! data source rather than a policy: [`Terrain`] answers those two, and the
//! rules that read them ([`occupiable`], [`rise_legal`]) live here, once, and
//! are the same rules whatever the ground came from.
//!
//! That split is deliberate. The *fetching* of generated chunks is a cache
//! with a memory budget and a pressure policy, and that belongs to the
//! process holding it, not to a library. The *interpretation* is a rule the
//! realm and every client must agree on exactly, or a player walks through a
//! hill on one machine and around it on another — so it is here, and it is
//! host-tested.
//!
//! # Unknown ground is not walkable
//!
//! [`Terrain::cell`] answers `None` for ground the caller has not got, and
//! the rules read that as impassable. A body cannot walk off the edge of
//! what the realm has generated, and a window that is missing a chunk fails
//! closed rather than treating absence as open field.

use tairix_wintersun_net::value::WorldPoint;
use tairix_wintersun_world::chunk::Chunk;
use tairix_wintersun_world::geom::{CellCoord, Elevation, CELL_SUB_UNITS, ELEVATION_SUB_UNITS};

use crate::error::RuleError;

/// The deepest standing water a body may wade through, in elevation
/// sub-units.
///
/// Three quarters of a world unit — about waist deep. Deeper water is
/// impassable, which is also what makes open sea impassable without a
/// separate flag for it: the shallows at a shoreline are wadeable and the
/// water beyond them is not, which is the behaviour a shoreline should have.
pub const WADE_DEPTH_SUB_UNITS: i32 = 3 * ELEVATION_SUB_UNITS / 4;

/// The greatest rise a body may step up, in elevation sub-units.
///
/// One world unit. A rise beyond it is a cliff face, and a cell raised
/// further than this above its neighbours is a pillar nothing can climb —
/// which is how the collision field expresses an obstacle without a second
/// vocabulary for one. Descent is unbounded: walking off a ledge is allowed.
pub const MAX_STEP_RISE_SUB_UNITS: i32 = ELEVATION_SUB_UNITS;

/// What the ground is at one cell.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Default)]
pub struct TerrainCell {
    /// Ground height.
    pub ground: Elevation,
    /// Water-surface height, equal to the ground where it is dry.
    pub water: Elevation,
}

impl TerrainCell {
    /// Dry ground at `ground`.
    #[must_use]
    pub const fn dry(ground: Elevation) -> Self {
        Self {
            ground,
            water: ground,
        }
    }

    /// Standing water depth, in elevation sub-units. Never negative.
    #[must_use]
    pub fn depth(self) -> i32 {
        (i32::from(self.water.0) - i32::from(self.ground.0)).max(0)
    }
}

/// Where the ground comes from.
///
/// One method, because two numbers are all the rules read. An implementation
/// over generated chunks is [`ChunkTerrain`]; one over a pattern, for a zone
/// with no world behind it, is [`SyntheticTerrain`].
pub trait Terrain {
    /// The ground at `cell`, or `None` where the caller has not got it.
    fn cell(&self, cell: CellCoord) -> Option<TerrainCell>;
}

/// The terrain cell a point falls in.
///
/// Flooring division, so the lattice has no discontinuity at the origin that
/// truncation toward zero would introduce.
#[must_use]
pub fn cell_at(point: WorldPoint) -> CellCoord {
    CellCoord::new(
        point.x.div_euclid(CELL_SUB_UNITS),
        point.y.div_euclid(CELL_SUB_UNITS),
    )
}

/// Whether a body may stand on this cell at all.
///
/// Fails closed on ground the caller does not have.
#[must_use]
pub fn occupiable(cell: Option<TerrainCell>) -> bool {
    cell.is_some_and(|cell| cell.depth() <= WADE_DEPTH_SUB_UNITS)
}

/// Whether a body standing on `from` may step onto `to`.
///
/// Fails closed on either side being unknown.
#[must_use]
pub fn rise_legal(from: Option<TerrainCell>, to: Option<TerrainCell>) -> bool {
    match (from, to) {
        (Some(from), Some(to)) => {
            let rise = i32::from(to.ground.0) - i32::from(from.ground.0);
            rise <= MAX_STEP_RISE_SUB_UNITS
        }
        _ => false,
    }
}

/// Terrain read out of generated chunks.
///
/// Holds a borrowed window and nothing else: no cache, no budget, no
/// generation. The caller decides which chunks are resident and when they
/// are released, and hands a slice of them here for the duration of a step.
#[derive(Copy, Clone, Debug)]
pub struct ChunkTerrain<'a> {
    window: &'a [&'a Chunk],
}

impl<'a> ChunkTerrain<'a> {
    /// Wrap a window of chunks, sorted by coordinate.
    ///
    /// Sorted because the lookup binary-searches it: a zone's window is as
    /// large as the region it simulates, and a linear scan of it per cell
    /// test would put the zone's area on the movement path.
    ///
    /// # Errors
    ///
    /// [`RuleError::TerrainWindow`] when the slice is not sorted, since an
    /// unsorted window would silently answer `None` for chunks it holds.
    pub fn new(window: &'a [&'a Chunk]) -> Result<Self, RuleError> {
        // Strictly increasing, so a duplicate coordinate is refused too: two
        // chunks claiming one coordinate would make the lookup's answer
        // depend on which the search landed on.
        if !window.is_sorted_by(|a, b| a.coord() < b.coord()) {
            return Err(RuleError::TerrainWindow);
        }
        Ok(Self { window })
    }
}

impl Terrain for ChunkTerrain<'_> {
    fn cell(&self, cell: CellCoord) -> Option<TerrainCell> {
        let coord = cell.chunk();
        let index = self
            .window
            .binary_search_by_key(&coord, |chunk| chunk.coord())
            .ok()?;
        let chunk = self.window.get(index)?;
        let (cx, cy) = cell.within_chunk();
        Some(TerrainCell {
            ground: chunk.elevation(cx, cy),
            water: chunk.water(cx, cy),
        })
    }
}

/// Ground that is a pure function of the cell rather than of a realm.
///
/// Two consumers and no third: the determinism reference run needs ground
/// identical on every target without solving a realm first, and a zone with
/// no world behind it — an arena — needs ground at all. Obstacles are raised
/// cells and pools are deep ones, so the pattern exercises both passability
/// rules rather than inventing a third way to be impassable.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct SyntheticTerrain {
    pillar_period: i32,
    pool_period: i32,
}

impl SyntheticTerrain {
    /// How far a pillar stands above the surrounding ground, in elevation
    /// sub-units — past the step rise, so nothing can climb one.
    pub const PILLAR_RISE_SUB_UNITS: i32 = MAX_STEP_RISE_SUB_UNITS + 1;

    /// How deep a pool stands, in elevation sub-units — past the wade
    /// depth, so nothing can cross one.
    pub const POOL_DEPTH_SUB_UNITS: i32 = WADE_DEPTH_SUB_UNITS + 1;

    /// Open ground, everywhere.
    #[must_use]
    pub const fn open() -> Self {
        Self {
            pillar_period: 0,
            pool_period: 0,
        }
    }

    /// Ground with a pillar and a pool on each lattice.
    ///
    /// A period of zero or less omits that feature, and the pool lattice
    /// needs a period above one to appear at all. The two lattices sit on
    /// different offsets, so neither hides the other.
    #[must_use]
    pub const fn lattice(pillar_period: i32, pool_period: i32) -> Self {
        Self {
            pillar_period,
            pool_period,
        }
    }
}

impl Terrain for SyntheticTerrain {
    fn cell(&self, cell: CellCoord) -> Option<TerrainCell> {
        let on = |period: i32, offset: i32| {
            period > 0 && cell.x.rem_euclid(period) == offset && cell.y.rem_euclid(period) == offset
        };
        let ground = if on(self.pillar_period, 0) {
            elevation(Self::PILLAR_RISE_SUB_UNITS)
        } else {
            Elevation(0)
        };
        let water = if on(self.pool_period, 1) {
            elevation(i32::from(ground.0) + Self::POOL_DEPTH_SUB_UNITS)
        } else {
            ground
        };
        Some(TerrainCell { ground, water })
    }
}

/// Quantise a height in elevation sub-units, saturating at the field's own
/// range rather than wrapping.
fn elevation(sub_units: i32) -> Elevation {
    Elevation(i16::try_from(sub_units).unwrap_or(i16::MAX))
}

#[cfg(test)]
mod tests;
