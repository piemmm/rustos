//! Chunk geometry, the quantised field units, and the conversions between
//! them.
//!
//! The wire owns the world *coordinate* vocabulary — [`WorldPoint`],
//! [`ChunkCoord`] and `WORLD_SUB_UNITS_PER_UNIT`. It deliberately does not
//! own the chunk's *shape*, because how finely the generator samples the
//! ground is the generator's question. That shape is defined once here and
//! imported everywhere else.
//!
//! # Why the field units are integers
//!
//! Generation computes in `f64` and stores integers. Every stored field —
//! elevation, temperature, moisture, discharge — is a fixed-point integer
//! whose scale is a power of two, so the conversion to and from `f64` is
//! exact and the stored value is the same bit pattern on every target
//! whatever the intermediate arithmetic did. That is what makes a state
//! digest over the stored fields a meaningful cross-target claim rather than
//! a restatement of one host's rounding.

use tairix_util::mathf;
use tairix_wintersun_net::value::{ChunkCoord, WorldPoint};

/// Cells along one edge of a chunk.
///
/// A power of two, so a world cell index splits into chunk and in-chunk
/// parts by shift and mask with no division and no sign surprise. The wire's
/// `WorldEdit` addresses an in-chunk cell as a `u16`, so this is also the
/// value a stored edit is validated against.
pub const CHUNK_CELLS: u32 = 64;

/// `log2(CHUNK_CELLS)`, the shift a world-cell index is split on.
pub const CHUNK_CELLS_LOG2: u32 = CHUNK_CELLS.trailing_zeros();

/// Cells in a whole chunk.
pub const CHUNK_AREA: usize = (CHUNK_CELLS as usize) * (CHUNK_CELLS as usize);

/// World sub-units along one generator cell.
///
/// One cell is one world unit: fine enough that a character crossing a cell
/// is a visible step, coarse enough that a chunk's fields stay small.
pub const CELL_SUB_UNITS: i32 = tairix_wintersun_net::bounds::WORLD_SUB_UNITS_PER_UNIT;

/// Fixed-point divisions of a world unit in a stored [`Elevation`].
///
/// A power of two. Eight gives an eighth-of-a-unit vertical resolution over
/// a `i16`'s ±4096 units, which spans deep ocean floor to high peak without
/// a wider field.
pub const ELEVATION_SUB_UNITS: i32 = 8;

/// Fixed-point divisions of a degree in a stored [`Temperature`].
pub const TEMPERATURE_SUB_UNITS: i32 = 64;

/// Terrain height above the realm's sea level, in [`ELEVATION_SUB_UNITS`].
#[derive(Copy, Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Default, Hash)]
pub struct Elevation(pub i16);

impl Elevation {
    /// Sea level.
    pub const SEA_LEVEL: Self = Self(0);

    /// The lowest representable elevation.
    pub const MIN: Self = Self(i16::MIN);

    /// The highest representable elevation.
    pub const MAX: Self = Self(i16::MAX);

    /// Quantise a height in world units, saturating at the representable
    /// range rather than wrapping.
    #[must_use]
    pub fn from_units(units: f64) -> Self {
        Self(quantise_i16(units * f64::from(ELEVATION_SUB_UNITS)))
    }

    /// This elevation in world units. Exact: the scale is a power of two.
    #[must_use]
    pub fn units(self) -> f64 {
        f64::from(self.0) / f64::from(ELEVATION_SUB_UNITS)
    }

    /// Whether this elevation is at or below sea level.
    #[must_use]
    pub const fn is_submerged(self) -> bool {
        self.0 <= Self::SEA_LEVEL.0
    }
}

/// Air temperature, in [`TEMPERATURE_SUB_UNITS`] of a degree Celsius.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Default, Hash)]
pub struct Temperature(pub i16);

impl Temperature {
    /// Quantise a temperature in degrees Celsius.
    #[must_use]
    pub fn from_celsius(celsius: f64) -> Self {
        Self(quantise_i16(celsius * f64::from(TEMPERATURE_SUB_UNITS)))
    }

    /// This temperature in degrees Celsius. Exact.
    #[must_use]
    pub fn celsius(self) -> f64 {
        f64::from(self.0) / f64::from(TEMPERATURE_SUB_UNITS)
    }
}

/// Relative moisture, `0` bone dry through [`u16::MAX`] saturated.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Default, Hash)]
pub struct Moisture(pub u16);

impl Moisture {
    /// Quantise a moisture fraction, clamping outside `0.0..=1.0`.
    #[must_use]
    pub fn from_fraction(fraction: f64) -> Self {
        Self(quantise_u16(
            mathf::clamp(fraction, 0.0, 1.0) * f64::from(u16::MAX),
        ))
    }

    /// This moisture as a fraction of saturation. Exact division is not
    /// needed: the value is consumed as a weight, never re-quantised.
    #[must_use]
    pub fn fraction(self) -> f64 {
        f64::from(self.0) / f64::from(u16::MAX)
    }
}

/// A cell of the world grid, in absolute cell indices from the realm origin.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct CellCoord {
    /// Eastward cell index.
    pub x: i32,
    /// Southward cell index.
    pub y: i32,
}

impl CellCoord {
    /// The cell at `(x, y)`.
    #[must_use]
    pub const fn new(x: i32, y: i32) -> Self {
        Self { x, y }
    }

    /// The chunk this cell falls in.
    ///
    /// Arithmetic shift, so a negative index floors toward negative infinity
    /// and the chunk grid has no discontinuity at the origin — which integer
    /// division toward zero would introduce.
    #[must_use]
    pub const fn chunk(self) -> ChunkCoord {
        ChunkCoord {
            x: self.x >> CHUNK_CELLS_LOG2,
            y: self.y >> CHUNK_CELLS_LOG2,
        }
    }

    /// This cell's offset within its chunk, both components in
    /// `0..CHUNK_CELLS`.
    ///
    /// The two's-complement low bits *are* the offset for a negative index
    /// exactly as for a positive one, which is why this pairs with
    /// [`Self::chunk`]'s arithmetic shift.
    #[must_use]
    #[allow(
        clippy::cast_sign_loss,
        reason = "only the low bits survive the mask, and they are the \
                  in-chunk offset regardless of the index's sign"
    )]
    pub const fn within_chunk(self) -> (u32, u32) {
        let mask = CHUNK_CELLS - 1;
        ((self.x as u32) & mask, (self.y as u32) & mask)
    }

    /// The centre of this cell, in wire sub-units.
    ///
    /// `None` when the cell lies outside the representable world, which a
    /// validated realm's own extent already excludes; a caller reaching for
    /// a cell beyond it gets no coordinate rather than a wrapped one.
    #[must_use]
    pub fn centre(self) -> Option<WorldPoint> {
        let half = CELL_SUB_UNITS / 2;
        Some(WorldPoint {
            x: self.x.checked_mul(CELL_SUB_UNITS)?.checked_add(half)?,
            y: self.y.checked_mul(CELL_SUB_UNITS)?.checked_add(half)?,
        })
    }
}

/// The cell at a chunk's north-west corner.
#[must_use]
pub const fn chunk_origin(chunk: ChunkCoord) -> CellCoord {
    CellCoord {
        x: chunk.x << CHUNK_CELLS_LOG2,
        y: chunk.y << CHUNK_CELLS_LOG2,
    }
}

/// A grid extent or coordinate as a signed index.
///
/// The one place the crate crosses from an unsigned count to a signed
/// index, so the argument is made once. Every count converted here is
/// bounded far below `i32::MAX` by a validated realm extent, making the
/// conversion exact; saturating rather than wrapping means a value that
/// somehow escaped that bound clamps instead of turning negative.
#[must_use]
pub fn signed(value: u32) -> i32 {
    i32::try_from(value).unwrap_or(i32::MAX)
}

/// Quantise a finite `f64` to `i16`, saturating rather than wrapping.
///
/// The one rounding rule the whole generator uses, so no two stages can
/// round a boundary value differently.
#[must_use]
pub fn quantise_i16(value: f64) -> i16 {
    let rounded = mathf::round(mathf::clamp(
        value,
        f64::from(i16::MIN),
        f64::from(i16::MAX),
    ));
    #[allow(
        clippy::cast_possible_truncation,
        reason = "clamped to i16's range and rounded to an integer above, so \
                  the conversion is exact"
    )]
    {
        rounded as i16
    }
}

/// Quantise a finite `f64` to `u16`, saturating rather than wrapping.
#[must_use]
pub fn quantise_u16(value: f64) -> u16 {
    let rounded = mathf::round(mathf::clamp(value, 0.0, f64::from(u16::MAX)));
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "clamped to u16's range and rounded to an integer above, so \
                  the conversion is exact and non-negative"
    )]
    {
        rounded as u16
    }
}

/// Quantise a finite `f64` to `u8`, saturating rather than wrapping.
#[must_use]
pub fn quantise_u8(value: f64) -> u8 {
    let rounded = mathf::round(mathf::clamp(value, 0.0, f64::from(u8::MAX)));
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "clamped to u8's range and rounded to an integer above, so \
                  the conversion is exact and non-negative"
    )]
    {
        rounded as u8
    }
}

/// Linear interpolation with the endpoints reproduced exactly.
///
/// `a + t * (b - a)` drifts off `b` at `t == 1`; this form does not, which
/// matters because a stage's output at a sample point must equal the sample
/// however it was reached.
#[must_use]
pub fn lerp(a: f64, b: f64, t: f64) -> f64 {
    a * (1.0 - t) + b * t
}

/// The smoothstep easing every stage interpolates with.
#[must_use]
pub fn smoothstep(t: f64) -> f64 {
    let t = mathf::clamp(t, 0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

#[cfg(test)]
mod tests;
