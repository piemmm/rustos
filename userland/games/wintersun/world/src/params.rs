//! The realm parameter document.
//!
//! A realm is a `u64` seed and this: eight numbers that decide how big the
//! world is, how finely it is solved, and what kind of place it is. Nothing
//! here is content — spells, items and biome tuning are declarative
//! documents elsewhere — and nothing here is a capacity the machine should
//! be choosing.
//!
//! # Every field is bounded, because this document is untrusted
//!
//! A client is handed its realm's parameters by the realm, and a realm is
//! no more trusted by a client than a client is by a realm. So this type
//! has one validating constructor and no public fields: an out-of-range
//! extent, a resolution that would not tile, or a temperature range that
//! would not quantise is refused with a reason rather than clamped into
//! something the two ends might disagree about. The bounds are fixed
//! security bounds on untrusted input and do not scale with the machine.
//!
//! This crate deliberately decodes no bytes. The wire encoding of these
//! fields belongs with the rest of the protocol in `wintersun/net`, which
//! already owns bounded decode and its fuzz harnesses; a second decoder
//! here would be a second place to get it wrong.

use tairix_wintersun_net::value::Facing;

use crate::geom::{Temperature, CHUNK_CELLS};

/// Smallest realm, in chunks along one edge.
pub const MIN_EXTENT_CHUNKS: u32 = 4;

/// Largest realm, in chunks along one edge.
///
/// At 64 cells to a chunk and one world unit to a cell this is a realm
/// 262 144 units on a side. Its half-extent in wire sub-units is an eighth
/// of `i32`'s range, so no position inside a legal realm can overflow a
/// coordinate.
pub const MAX_EXTENT_CHUNKS: u32 = 4096;

/// Fewest coarse samples along one edge of the realm field.
pub const MIN_COARSE_SAMPLES: u32 = 32;

/// Most coarse samples along one edge of the realm field.
///
/// This bounds the one structure whose size does not follow the working
/// set: the realm field is solved globally and held for the realm's life,
/// so its cost must not follow the realm's extent. It does not — a realm
/// four thousand chunks across and one four chunks across get the same
/// grid, the former simply at a coarser step. At the ceiling the field is
/// 512 × 512 samples, a few mebibytes, which is the same on every machine.
pub const MAX_COARSE_SAMPLES: u32 = 512;

/// Fewest continental plates.
pub const MIN_PLATES: u32 = 4;

/// Most continental plates.
pub const MAX_PLATES: u32 = 64;

/// Highest permitted peak relief, in world units above sea level.
///
/// Below the elevation field's own ±4096-unit range, so erosion, uplift and
/// the channel carve all have headroom above the tallest legal peak and
/// cannot saturate the field.
pub const MAX_RELIEF_UNITS: u16 = 3000;

/// Lowest permitted sea-level temperature, in whole degrees Celsius.
pub const MIN_EDGE_CELSIUS: i16 = -90;

/// Highest permitted sea-level temperature, in whole degrees Celsius.
pub const MAX_EDGE_CELSIUS: i16 = 60;

/// Why a parameter document was refused.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum ParamsError {
    /// The realm extent is outside [`MIN_EXTENT_CHUNKS`]..=[`MAX_EXTENT_CHUNKS`]
    /// or is not a power of two.
    Extent,
    /// The coarse resolution is outside
    /// [`MIN_COARSE_SAMPLES`]..=[`MAX_COARSE_SAMPLES`], is not a power of
    /// two, or is finer than the cell grid it samples.
    CoarseResolution,
    /// The plate count is outside [`MIN_PLATES`]..=[`MAX_PLATES`].
    PlateCount,
    /// The submerged fraction exceeds one thousand parts per thousand.
    OceanFraction,
    /// The peak relief is zero or above [`MAX_RELIEF_UNITS`].
    Relief,
    /// A sea-level temperature is outside
    /// [`MIN_EDGE_CELSIUS`]..=[`MAX_EDGE_CELSIUS`].
    Temperature,
}

/// The fields a [`RealmParams`] is built from.
///
/// A plain record so a caller names what it is setting, validated into the
/// opaque [`RealmParams`] by [`RealmParams::new`].
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct RealmSpec {
    /// The realm seed. Every value is legal; the world is a pure function
    /// of it and this document.
    pub seed: u64,
    /// Chunks along one edge of the realm, a power of two. The realm is
    /// centred on the origin.
    pub extent_chunks: u32,
    /// Coarse samples along one edge of the realm field, a power of two.
    pub coarse_samples: u32,
    /// Continental plates.
    pub plates: u32,
    /// Target fraction of the realm below sea level, in parts per thousand.
    pub ocean_permille: u16,
    /// Peak relief above sea level, in world units.
    pub relief_units: u16,
    /// Sea-level temperature at the realm's northern edge, in degrees
    /// Celsius.
    pub north_celsius: i16,
    /// Sea-level temperature at the realm's southern edge, in degrees
    /// Celsius.
    pub south_celsius: i16,
    /// The prevailing wind moisture is advected along.
    pub wind: Facing,
}

/// A validated realm parameter document.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct RealmParams {
    spec: RealmSpec,
}

impl RealmParams {
    /// Validate a specification.
    ///
    /// # Errors
    ///
    /// The first field that fails its bound, so a refusal names what to
    /// correct rather than that something was wrong.
    pub fn new(spec: RealmSpec) -> Result<Self, ParamsError> {
        if !(MIN_EXTENT_CHUNKS..=MAX_EXTENT_CHUNKS).contains(&spec.extent_chunks)
            || !spec.extent_chunks.is_power_of_two()
        {
            return Err(ParamsError::Extent);
        }
        let cells_per_edge = spec.extent_chunks * CHUNK_CELLS;
        if !(MIN_COARSE_SAMPLES..=MAX_COARSE_SAMPLES).contains(&spec.coarse_samples)
            || !spec.coarse_samples.is_power_of_two()
            || spec.coarse_samples > cells_per_edge
        {
            return Err(ParamsError::CoarseResolution);
        }
        if !(MIN_PLATES..=MAX_PLATES).contains(&spec.plates) {
            return Err(ParamsError::PlateCount);
        }
        if spec.ocean_permille > 1000 {
            return Err(ParamsError::OceanFraction);
        }
        if spec.relief_units == 0 || spec.relief_units > MAX_RELIEF_UNITS {
            return Err(ParamsError::Relief);
        }
        let band = MIN_EDGE_CELSIUS..=MAX_EDGE_CELSIUS;
        if !band.contains(&spec.north_celsius) || !band.contains(&spec.south_celsius) {
            return Err(ParamsError::Temperature);
        }
        Ok(Self { spec })
    }

    /// The `WinterSun` default realm for `seed`: cold-biased, mostly land,
    /// with a westerly prevailing wind — blowing toward the east, a
    /// thirty-second of a turn south of it, so the moisture sweep is not
    /// axis-aligned and a coast does not advect along a single row.
    ///
    /// Infallible by construction — the constants below are inside every
    /// bound above, and a test holds them there.
    #[must_use]
    pub fn winter_default(seed: u64) -> Self {
        Self {
            spec: RealmSpec {
                seed,
                extent_chunks: 256,
                coarse_samples: 256,
                plates: 12,
                ocean_permille: 380,
                relief_units: 1800,
                north_celsius: -22,
                south_celsius: 14,
                wind: Facing(0x0800),
            },
        }
    }

    /// The specification these parameters validated.
    #[must_use]
    pub const fn spec(self) -> RealmSpec {
        self.spec
    }

    /// The realm seed.
    #[must_use]
    pub const fn seed(self) -> u64 {
        self.spec.seed
    }

    /// Chunks along one edge of the realm.
    #[must_use]
    pub const fn extent_chunks(self) -> u32 {
        self.spec.extent_chunks
    }

    /// Cells along one edge of the realm.
    #[must_use]
    pub const fn extent_cells(self) -> u32 {
        self.spec.extent_chunks * CHUNK_CELLS
    }

    /// Coarse samples along one edge of the realm field.
    #[must_use]
    pub const fn coarse_samples(self) -> u32 {
        self.spec.coarse_samples
    }

    /// Cells between adjacent coarse samples. Never zero, and exact: both
    /// extents are powers of two and the resolution is the finer.
    #[must_use]
    pub const fn cells_per_coarse(self) -> u32 {
        self.extent_cells() / self.spec.coarse_samples
    }

    /// Continental plates.
    #[must_use]
    pub const fn plates(self) -> u32 {
        self.spec.plates
    }

    /// Target fraction of the realm below sea level, in parts per thousand.
    #[must_use]
    pub const fn ocean_permille(self) -> u16 {
        self.spec.ocean_permille
    }

    /// Peak relief above sea level, in world units.
    #[must_use]
    pub const fn relief_units(self) -> u16 {
        self.spec.relief_units
    }

    /// Sea-level temperature at the northern edge.
    #[must_use]
    pub fn north_temperature(self) -> Temperature {
        Temperature::from_celsius(f64::from(self.spec.north_celsius))
    }

    /// Sea-level temperature at the southern edge.
    #[must_use]
    pub fn south_temperature(self) -> Temperature {
        Temperature::from_celsius(f64::from(self.spec.south_celsius))
    }

    /// The prevailing wind.
    #[must_use]
    pub const fn wind(self) -> Facing {
        self.spec.wind
    }

    /// Half the realm's edge, in chunks.
    ///
    /// The one place the extent crosses into signed indices, so the one
    /// place the conversion has to be argued.
    #[must_use]
    #[allow(
        clippy::cast_possible_wrap,
        reason = "validation caps the extent at MAX_EXTENT_CHUNKS, so half \
                  of it is at most 2048"
    )]
    pub const fn half_extent_chunks(self) -> i32 {
        (self.spec.extent_chunks / 2) as i32
    }

    /// The lowest chunk index the realm covers on either axis.
    ///
    /// The realm is centred on the origin, so a fresh character spawning at
    /// `(0, 0)` is in the middle of it.
    #[must_use]
    pub const fn min_chunk(self) -> i32 {
        -self.half_extent_chunks()
    }

    /// One past the highest chunk index the realm covers on either axis.
    #[must_use]
    pub const fn max_chunk(self) -> i32 {
        self.half_extent_chunks()
    }

    /// Whether `chunk` lies inside the realm.
    #[must_use]
    pub const fn holds_chunk(self, x: i32, y: i32) -> bool {
        x >= self.min_chunk()
            && x < self.max_chunk()
            && y >= self.min_chunk()
            && y < self.max_chunk()
    }
}

#[cfg(test)]
mod tests;
