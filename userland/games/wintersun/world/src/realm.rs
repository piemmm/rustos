//! The realm field: the whole world, solved once, coarsely.
//!
//! # Why there is a coarse global field at all
//!
//! Hydrology is not a local question. How much water passes a point
//! depends on every point that drains to it, which can be most of a
//! continent, and a river that "flows downhill" only inside the window one
//! chunk happened to look at is a river that flows uphill across the seam
//! between two windows. Climate is the same: a rain shadow is the record
//! of everything the wind crossed before it arrived.
//!
//! So the world is solved on two scales. This field is the coarse one:
//! **global, exact, and the same for every query**. Every derived quantity
//! it holds — drainage, discharge, lake surfaces, temperature, moisture,
//! settlements, roads — is therefore free of seams by construction, not by
//! a halo that happens to be wide enough.
//!
//! # Why it is affordable
//!
//! Its resolution is a fixed sample count, never a step in world units. A
//! realm four chunks across and one four thousand chunks across get the
//! same grid; the larger simply has a coarser step. So the field's cost is
//! the same on every machine and for every realm, and nothing here grows
//! with the world's extent. Fine detail is not its job — that is the chunk
//! stage's, which reads a bounded window of this and adds everything below
//! its step.

use alloc::vec::Vec;

use tairix_wintersun_net::value::ChunkCoord;

use crate::climate;
use crate::error::WorldError;
use crate::geom::{chunk_origin, signed, CellCoord, Elevation, Moisture, Temperature, CHUNK_CELLS};
use crate::hydrology::{self, FlowDir};
use crate::params::RealmParams;
use crate::relief;
use crate::seed::SeedKey;
use crate::sites::{self, Landmark, Places, Road, Site};
use crate::uplift::Plates;

/// One coarse sample's terrain, climate and drainage.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct CoarseSample {
    /// Ground surface after uplift, noise and erosion.
    pub elevation: Elevation,
    /// Surface of the water standing here — sea or lake. Equal to
    /// [`Self::elevation`] where the ground is dry.
    pub water: Elevation,
    /// Where this sample drains to.
    pub flow: FlowDir,
    /// Coarse samples draining through here, including itself. The
    /// proxy for discharge: it grows strictly downstream, so a channel
    /// widens on its way to the sea.
    pub discharge: u32,
    /// Air temperature.
    pub temperature: Temperature,
    /// Relative moisture after advection, orographic lift and rain shadow.
    pub moisture: Moisture,
    /// How strongly this sample sits in a mountain belt.
    pub belt: u8,
}

impl CoarseSample {
    /// Whether standing water covers this sample.
    #[must_use]
    pub const fn is_water(self) -> bool {
        self.water.0 > self.elevation.0 || self.elevation.is_submerged()
    }
}

/// A realm, solved coarsely.
#[derive(Debug)]
pub struct RealmField {
    params: RealmParams,
    key: SeedKey,
    samples: Vec<CoarseSample>,
    places: Places,
}

impl RealmField {
    /// Solve `params` into a realm field.
    ///
    /// The whole coarse pipeline runs here, in order: relief from plates
    /// and noise, a sea level cut to the requested submerged fraction,
    /// drainage and erosion, climate, then settlements and the roads
    /// between them. Each stage reads the one before it and nothing else,
    /// so the order is the dependency order and there is no fixed point to
    /// iterate to.
    ///
    /// # Errors
    ///
    /// [`WorldError::OutOfMemory`] if the field does not fit. Nothing here
    /// panics on a short machine.
    pub fn generate(params: RealmParams) -> Result<Self, WorldError> {
        let key = SeedKey::new(params.seed());
        let plate_field = Plates::new(params);
        let side = params.coarse_samples() as usize;
        let area = side * side;

        let mut samples = try_filled(area, CoarseSample::default())?;
        relief::solve(params, key, plate_field, &mut samples)?;
        hydrology::solve(params, &mut samples)?;
        climate::solve(params, key, &mut samples)?;
        let places = sites::solve(params, key, &samples)?;

        Ok(Self {
            params,
            key,
            samples,
            places,
        })
    }

    /// The parameters this field was solved from.
    #[must_use]
    pub const fn params(&self) -> RealmParams {
        self.params
    }

    /// The realm's generation key.
    #[must_use]
    pub const fn key(&self) -> SeedKey {
        self.key
    }

    /// Coarse samples along one edge.
    #[must_use]
    pub const fn side(&self) -> u32 {
        self.params.coarse_samples()
    }

    /// The settlements the realm placed.
    #[must_use]
    pub fn sites(&self) -> &[Site] {
        &self.places.sites
    }

    /// The roads between them.
    #[must_use]
    pub fn roads(&self) -> &[Road] {
        &self.places.roads
    }

    /// The dungeon and shrine entrances, ruins and rift scars.
    ///
    /// Entrances only. What is *behind* an entrance is the realm's secret
    /// and is generated server-side, so a client holding this field learns
    /// nothing it could not see by walking there.
    #[must_use]
    pub fn landmarks(&self) -> &[Landmark] {
        &self.places.landmarks
    }

    /// The sample at grid position `(sx, sy)`, clamped to the grid.
    ///
    /// Clamping rather than refusing: every caller is interpolating or
    /// walking a stencil, and the realm's edge is a real edge — the ground
    /// beyond it is the ground at it, not an error to handle at every
    /// sample.
    #[must_use]
    pub fn sample(&self, sx: i32, sy: i32) -> CoarseSample {
        self.samples[clamped_index(sx, sy, self.side())]
    }

    /// Every sample, in row-major order.
    #[must_use]
    pub fn samples(&self) -> &[CoarseSample] {
        &self.samples
    }

    /// The continuous grid position of a world cell.
    ///
    /// Sample `(sx, sy)` sits at the cell that starts its step, so an
    /// integer result means the query landed exactly on a sample and the
    /// interpolation below returns that sample untouched.
    #[must_use]
    pub fn grid_position(&self, cell: CellCoord) -> (f64, f64) {
        let origin = self.params.min_chunk() * signed(CHUNK_CELLS);
        let step = f64::from(self.params.cells_per_coarse());
        (
            f64::from(cell.x - origin) / step,
            f64::from(cell.y - origin) / step,
        )
    }

    /// The grid position of a chunk's north-west corner.
    #[must_use]
    pub fn chunk_grid_position(&self, chunk: ChunkCoord) -> (f64, f64) {
        self.grid_position(chunk_origin(chunk))
    }

    /// Bilinearly interpolated elevation, in world units, at a grid
    /// position.
    #[must_use]
    pub fn elevation_units_at(&self, gx: f64, gy: f64) -> f64 {
        self.interpolate(gx, gy, |sample| sample.elevation.units())
    }

    /// Bilinearly interpolated water-surface height, in world units.
    #[must_use]
    pub fn water_units_at(&self, gx: f64, gy: f64) -> f64 {
        self.interpolate(gx, gy, |sample| sample.water.units())
    }

    /// Bilinearly interpolated temperature, in degrees Celsius.
    #[must_use]
    pub fn temperature_celsius_at(&self, gx: f64, gy: f64) -> f64 {
        self.interpolate(gx, gy, |sample| sample.temperature.celsius())
    }

    /// Bilinearly interpolated moisture, as a fraction of saturation.
    #[must_use]
    pub fn moisture_at(&self, gx: f64, gy: f64) -> f64 {
        self.interpolate(gx, gy, |sample| sample.moisture.fraction())
    }

    /// Bilinearly interpolated mountain-belt strength, `0.0..1.0`.
    #[must_use]
    pub fn belt_at(&self, gx: f64, gy: f64) -> f64 {
        self.interpolate(gx, gy, |sample| f64::from(sample.belt) / f64::from(u8::MAX))
    }

    /// Bilinear interpolation of one scalar over the four samples around a
    /// grid position.
    fn interpolate(&self, gx: f64, gy: f64, of: impl Fn(CoarseSample) -> f64) -> f64 {
        use crate::geom::lerp;
        use tairix_util::mathf;

        let x0 = mathf::floor(gx);
        let y0 = mathf::floor(gy);
        let (ix, iy) = (mathf::round_i32(x0), mathf::round_i32(y0));
        let (tx, ty) = (gx - x0, gy - y0);

        let top = lerp(of(self.sample(ix, iy)), of(self.sample(ix + 1, iy)), tx);
        let bottom = lerp(
            of(self.sample(ix, iy + 1)),
            of(self.sample(ix + 1, iy + 1)),
            tx,
        );
        lerp(top, bottom, ty)
    }
}

/// The row-major index of a grid position, clamped onto the grid.
#[must_use]
pub fn clamped_index(sx: i32, sy: i32, side: u32) -> usize {
    #[allow(
        clippy::cast_possible_wrap,
        reason = "the coarse side is at most MAX_COARSE_SAMPLES"
    )]
    let last = side as i32 - 1;
    let x = sx.clamp(0, last);
    let y = sy.clamp(0, last);
    #[allow(
        clippy::cast_sign_loss,
        reason = "both components were just clamped to 0..=last"
    )]
    {
        (y as usize) * (side as usize) + (x as usize)
    }
}

/// Allocate `len` copies of `value` without aborting on exhaustion.
///
/// `vec![v; n]` aborts the process when the machine is short, which is not
/// a behaviour a realm open may have. This reserves fallibly first, so the
/// caller gets a refusal it can report.
pub fn try_filled<T: Clone>(len: usize, value: T) -> Result<Vec<T>, WorldError> {
    let mut filled = Vec::new();
    filled
        .try_reserve_exact(len)
        .map_err(|_| WorldError::OutOfMemory)?;
    filled.resize(len, value);
    Ok(filled)
}

#[cfg(test)]
mod tests;
