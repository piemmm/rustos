//! Decals: roads, rivers and scars, stamped into the weight field.
//!
//! A road is not geometry laid over the ground and it is not a tile in a
//! grid. It is a **spline that raises its own material's weight** where it
//! passes, so the ground blends it in through exactly the machinery
//! everything else goes through ([`WeightField::cover`]). That is what
//! makes a road *wear into* grass rather than sit on top of it, and it is
//! why two roads meeting merge instead of overlapping — the second stamp
//! at the same coverage is a no-op.
//!
//! The world generator already levels the terrain a road runs over and
//! flags the cells it touches, which is the road's *shape*. What it does
//! not do is decide what the road is made of or how its edge meets the
//! ground, because those are drawing questions. This is where they are
//! answered.
//!
//! # A hard edge is the tell
//!
//! A stamp with a step edge reads as a decal, because nothing in a
//! landscape has one. Two things stop that here: coverage falls off
//! smoothly through a feather band outside the carriageway, and the
//! distance the falloff is measured on is perturbed by a noise field
//! keyed on world position — so the edge frays at the scale of gravel
//! scattering into grass rather than running true.

use tairix_wintersun_net::value::WorldPoint;
use tairix_wintersun_world::biome::Material;

use crate::noise::{self, Field, Tiled};
use crate::weight::{WeightField, TOTAL};

/// A material stamped along a path.
///
/// The path is borrowed: a realm's roads are the world generator's, and
/// copying them per frame would be paying for a decision already made.
#[derive(Copy, Clone, Debug)]
pub struct Decal<'a> {
    /// What the path is made of.
    pub material: Material,
    /// The centreline, in world sub-units. Fewer than two points stamps
    /// nothing.
    pub path: &'a [WorldPoint],
    /// Half-width of the fully covered carriageway, in sub-units.
    pub half_width: u32,
    /// Width of the band outside it over which coverage falls to nothing.
    pub feather: u32,
    /// Coverage at the centreline, out of [`TOTAL`].
    ///
    /// A road is not usually total: letting a little of the ground it
    /// crosses through is what makes a track in heath look different from
    /// the same track in sand.
    pub coverage: u16,
}

/// How far the fray perturbs the measured distance, as a fraction of the
/// feather band's width.
///
/// Bounded to a part of the feather so the break in the edge reads as
/// material scattering off it rather than as a second, noisier edge.
const FRAY_NUMERATOR: u64 = 1;
/// Denominator of [`FRAY_NUMERATOR`].
const FRAY_DENOMINATOR: u64 = 3;

/// How far the fray can move an edge either way, in sub-units.
///
/// Outward as well as inward: the perturbation is signed, so a decal's
/// true extent is its reach plus this. [`Decal::bounds`] adds it, because
/// a caller buckets decals by that extent and a stamp outside the box it
/// reported would simply be missed.
#[must_use]
pub fn fray_offset(feather: u32) -> u32 {
    u32::try_from(u64::from(feather) * FRAY_NUMERATOR / FRAY_DENOMINATOR).unwrap_or(u32::MAX)
}

/// Log2 of the fray lattice's cell, in world sub-units.
///
/// About the scale of loose material scattering off a verge: fine enough
/// to read as an edge breaking up, coarse enough not to look like noise.
pub const FRAY_CELL_LOG2: u32 = 7;

/// An axis-aligned extent in world sub-units.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct Bounds {
    /// West edge.
    pub min_x: i32,
    /// North edge.
    pub min_y: i32,
    /// East edge.
    pub max_x: i32,
    /// South edge.
    pub max_y: i32,
}

impl Bounds {
    /// Whether a point is inside.
    #[must_use]
    pub const fn contains(&self, point: WorldPoint) -> bool {
        point.x >= self.min_x
            && point.x <= self.max_x
            && point.y >= self.min_y
            && point.y <= self.max_y
    }

    /// Whether two extents overlap at all.
    #[must_use]
    pub const fn overlaps(&self, other: &Self) -> bool {
        self.min_x <= other.max_x
            && other.min_x <= self.max_x
            && self.min_y <= other.max_y
            && other.min_y <= self.max_y
    }
}

impl Decal<'_> {
    /// Everything this decal can reach, including its frayed edge.
    ///
    /// Returns `None` for a path too short to stamp. A caller buckets
    /// decals per tile with this, so it must be a true bound: nothing
    /// outside it is ever touched.
    #[must_use]
    pub fn bounds(&self) -> Option<Bounds> {
        let first = *self.path.first()?;
        if self.path.len() < 2 {
            return None;
        }
        let reach = i32::try_from(self.reach().saturating_add(fray_offset(self.feather)))
            .unwrap_or(i32::MAX);
        let mut bounds = Bounds {
            min_x: first.x,
            min_y: first.y,
            max_x: first.x,
            max_y: first.y,
        };
        for point in self.path {
            bounds.min_x = bounds.min_x.min(point.x);
            bounds.min_y = bounds.min_y.min(point.y);
            bounds.max_x = bounds.max_x.max(point.x);
            bounds.max_y = bounds.max_y.max(point.y);
        }
        Some(Bounds {
            min_x: bounds.min_x.saturating_sub(reach),
            min_y: bounds.min_y.saturating_sub(reach),
            max_x: bounds.max_x.saturating_add(reach),
            max_y: bounds.max_y.saturating_add(reach),
        })
    }

    /// How far from the centreline the stamp can reach.
    #[must_use]
    pub const fn reach(&self) -> u32 {
        self.half_width.saturating_add(self.feather)
    }

    /// The coverage this decal claims at a world point, out of [`TOTAL`].
    ///
    /// Zero outside the reach, [`coverage`](Self::coverage) inside the
    /// carriageway, and a smooth ramp between the two — with the measured
    /// distance frayed, so the ramp does not run true.
    #[must_use]
    pub fn coverage_at(&self, fray: &Fray, at: WorldPoint) -> u16 {
        if self.path.len() < 2 || self.coverage == 0 {
            return 0;
        }
        let reach = u64::from(self.reach());
        let mut nearest = u64::MAX;
        for pair in self.path.windows(2) {
            nearest = nearest.min(distance_to_segment(pair[0], pair[1], at));
            if nearest <= u64::from(self.half_width) {
                break;
            }
        }
        // The fray is signed, so a point this far out cannot be pulled
        // inside the reach and needs no noise evaluation at all.
        if nearest >= reach.saturating_add(u64::from(fray_offset(self.feather))) {
            return 0;
        }

        let frayed = fray.apply(at, nearest, self.feather);
        if frayed <= u64::from(self.half_width) {
            return self.coverage;
        }
        if frayed >= reach {
            return 0;
        }
        let into = frayed - u64::from(self.half_width);
        let across = u64::from(self.feather).max(1);
        ramp(self.coverage, across - into, across)
    }

    /// Stamp this decal into a cell's weight field at `at`.
    ///
    /// Returns whether the field changed. Nothing is stamped outside the
    /// reach, and a stamp lighter than everything already in a full field
    /// is refused by the field itself.
    pub fn stamp(&self, field: &mut WeightField, fray: &Fray, at: WorldPoint) -> bool {
        let coverage = self.coverage_at(fray, at);
        coverage > 0 && field.cover(self.material, coverage)
    }
}

/// The perturbation that breaks a decal's edge.
///
/// Keyed on the realm, so every client frays a road the same way and the
/// edge does not shimmer as a chunk is regenerated.
#[derive(Copy, Clone, Debug)]
pub struct Fray {
    field: Tiled,
}

impl Fray {
    /// The fray for a realm.
    #[must_use]
    pub const fn new(realm_seed: u64) -> Self {
        Self {
            field: Tiled::unbounded(realm_seed),
        }
    }

    /// `distance` perturbed by up to [`fray_offset`] either way.
    fn apply(&self, at: WorldPoint, distance: u64, feather: u32) -> u64 {
        let half = i32::try_from(fray_offset(feather)).unwrap_or(i32::MAX);
        if half == 0 {
            return distance;
        }
        let sample = self.field.value(Field::Fray, at.x, at.y, FRAY_CELL_LOG2);
        let offset = i64::from(noise::centred(sample, half));
        let signed = i64::try_from(distance).unwrap_or(i64::MAX);
        #[allow(
            clippy::cast_sign_loss,
            reason = "the sum is clamped at zero before the cast"
        )]
        {
            signed.saturating_add(offset).max(0) as u64
        }
    }
}

/// `peak` scaled by `numerator`/`denominator`, run through a smoothstep so
/// the edge of the band meets the ground with zero slope.
fn ramp(peak: u16, numerator: u64, denominator: u64) -> u16 {
    let denominator = denominator.max(1);
    let t = (numerator.min(denominator) * u64::from(u16::MAX)) / denominator;
    let total = u64::from(u16::MAX);
    let square = t * t / total;
    let cube = square * t / total;
    let smooth = (3 * square - 2 * cube).min(total);
    #[allow(
        clippy::cast_possible_truncation,
        reason = "a fraction of `peak`, itself a u16, is a u16"
    )]
    {
        ((u64::from(peak.min(TOTAL)) * smooth) / total) as u16
    }
}

/// Distance from `at` to the segment `a`–`b`, in world sub-units.
///
/// Exact integer arithmetic: the projection is a ratio of dot products
/// held in `i128` so a realm-spanning segment cannot overflow it, and the
/// final length is an exact integer square root. A float here would make
/// a road's edge land a sub-unit differently per target, which is the one
/// thing this crate is built not to do.
fn distance_to_segment(a: WorldPoint, b: WorldPoint, at: WorldPoint) -> u64 {
    let (ax, ay) = (i128::from(a.x), i128::from(a.y));
    let (bx, by) = (i128::from(b.x), i128::from(b.y));
    let (px, py) = (i128::from(at.x), i128::from(at.y));
    let (dx, dy) = (bx - ax, by - ay);
    let length_sq = dx * dx + dy * dy;

    let (nx, ny) = if length_sq == 0 {
        (ax, ay)
    } else {
        let along = ((px - ax) * dx + (py - ay) * dy).clamp(0, length_sq);
        (ax + along * dx / length_sq, ay + along * dy / length_sq)
    };
    let (ox, oy) = (px - nx, py - ny);
    let square = (ox * ox + oy * oy).unsigned_abs();
    u64::try_from(square.isqrt()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests;
