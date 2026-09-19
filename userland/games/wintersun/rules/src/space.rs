//! The broad phase: which bodies are near enough to be worth testing.
//!
//! A crowd is the load case every persistent world meets on its first busy
//! evening, and an all-pairs scan is quadratic in exactly that case. So
//! bodies are bucketed by cell and a query costs the cells it overlaps
//! rather than the zone's population: a hundred players in one market square
//! raise the cost of the cells they occupy and of nothing else.
//!
//! # Why a sorted array rather than a hash of cells
//!
//! The obvious structure is a map from cell to a list of bodies, and it is
//! the wrong one here for two reasons. A hash map's iteration order varies
//! with its keys and its insertion history, and anything an authoritative
//! simulation reads must have one order on every machine. And a map of lists
//! is an allocation per occupied cell, churned every tick.
//!
//! This is one array of `(cell, body)` sorted row-major, rebuilt in place
//! each step. Sorting groups a row contiguously, so a rectangle query is one
//! binary search per row and a walk; the order is total, with the body's own
//! identity as the tiebreak, so two runs visit the same bodies in the same
//! sequence; and the allocation is reused for the process's life.
//!
//! # The query is conservative, so the cell size is only ever a cost
//!
//! Nothing here assumes a body fits in a cell or moves less than one. A
//! query takes the radius it wants and visits every cell the circle's
//! bounding box touches, so correctness does not depend on the cell size —
//! only how much work it takes. That is what lets the size be chosen for
//! efficiency and revisited without auditing the callers.

use alloc::vec::Vec;

use tairix_wintersun_net::value::{EntityId, WorldPoint};

use crate::bounds::MAX_BODY_RADIUS_SUB_UNITS;
use crate::error::RulesError;

/// Sub-units along one edge of a broad-phase cell.
///
/// The widest body's diameter, so a typical query touches a handful of
/// cells rather than one crowded one or a hundred empty ones. Derived rather
/// than picked, and — because the query is conservative — a figure that
/// affects only cost.
pub const GRID_CELL_SUB_UNITS: i32 = 2 * MAX_BODY_RADIUS_SUB_UNITS;

/// One broad-phase cell.
///
/// The row leads the column, so the derived order is row-major: sorting
/// groups a row contiguously and a rectangle query is one search per row.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct GridCell {
    /// Southward cell index.
    pub row: i32,
    /// Eastward cell index.
    pub column: i32,
}

/// The cell a point falls in.
///
/// Flooring division, so the lattice has no discontinuity at the origin that
/// truncation toward zero would introduce.
#[must_use]
pub fn cell_of(point: WorldPoint) -> GridCell {
    GridCell {
        row: point.y.div_euclid(GRID_CELL_SUB_UNITS),
        column: point.x.div_euclid(GRID_CELL_SUB_UNITS),
    }
}

/// Bodies bucketed by cell, sorted row-major.
#[derive(Clone, Debug, Default)]
pub struct BroadPhase {
    slots: Vec<(GridCell, EntityId)>,
}

impl BroadPhase {
    /// An empty broad phase.
    #[must_use]
    pub const fn new() -> Self {
        Self { slots: Vec::new() }
    }

    /// How many bodies are bucketed.
    #[must_use]
    pub fn len(&self) -> usize {
        self.slots.len()
    }

    /// Whether nothing is bucketed.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.slots.is_empty()
    }

    /// Rebuild from every body's position.
    ///
    /// Rebuilt rather than maintained incrementally, and deliberately: an
    /// incremental grid's buckets end up in whatever order bodies happened
    /// to enter them, which an authoritative simulation cannot read. A
    /// rebuild is a sort of one array per step, and it is correct by
    /// construction rather than by an invariant somebody has to keep.
    ///
    /// # Errors
    ///
    /// [`RulesError::OutOfMemory`] when the array cannot be grown to hold
    /// the population.
    pub fn rebuild<I>(&mut self, bodies: I) -> Result<(), RulesError>
    where
        I: ExactSizeIterator<Item = (EntityId, WorldPoint)>,
    {
        self.slots.clear();
        self.slots
            .try_reserve(bodies.len())
            .map_err(|_| RulesError::OutOfMemory)?;
        for (id, at) in bodies {
            self.slots.push((cell_of(at), id));
        }
        self.slots.sort_unstable();
        Ok(())
    }

    /// Visit every body whose cell overlaps the box around `centre` of
    /// `radius` sub-units.
    ///
    /// Conservative: a visited body may be outside the circle, and the
    /// caller does the exact test. Never *misses* one, whatever the radius.
    /// Visit order is row-major then by identity, on every machine.
    pub fn for_each_near(&self, centre: WorldPoint, radius: i64, mut visit: impl FnMut(EntityId)) {
        let cell = i64::from(GRID_CELL_SUB_UNITS);
        let lo_row = clamp_index((i64::from(centre.y) - radius).div_euclid(cell));
        let hi_row = clamp_index((i64::from(centre.y) + radius).div_euclid(cell));
        let lo_column = clamp_index((i64::from(centre.x) - radius).div_euclid(cell));
        let hi_column = clamp_index((i64::from(centre.x) + radius).div_euclid(cell));

        let rows = i64::from(hi_row) - i64::from(lo_row) + 1;
        // A radius wide enough to span more rows than there are bodies makes
        // the per-row search the slower path *and* an unbounded loop. Walking
        // the array costs the population once and visits in the same order.
        if rows >= i64::try_from(self.slots.len()).unwrap_or(i64::MAX) {
            for &(cell, id) in &self.slots {
                if (lo_row..=hi_row).contains(&cell.row)
                    && (lo_column..=hi_column).contains(&cell.column)
                {
                    visit(id);
                }
            }
            return;
        }

        for row in lo_row..=hi_row {
            let first = GridCell {
                row,
                column: lo_column,
            };
            let start = self.slots.partition_point(|(cell, _)| *cell < first);
            let Some(tail) = self.slots.get(start..) else {
                return;
            };
            for &(cell, id) in tail {
                if cell.row != row || cell.column > hi_column {
                    break;
                }
                visit(id);
            }
        }
    }
}

/// Narrow a cell index to the lattice's own range, clamping rather than
/// wrapping.
fn clamp_index(value: i64) -> i32 {
    i32::try_from(value).unwrap_or(if value.is_negative() {
        i32::MIN
    } else {
        i32::MAX
    })
}

#[cfg(test)]
mod tests;
