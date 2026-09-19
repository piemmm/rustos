//! One chunk of ground, and the resumable machine that builds it.
//!
//! # The halo, and why it is exactly this wide
//!
//! Everything here reads the realm field — which is global, so identical
//! for every query — plus pure functions of absolute position, plus a
//! **fixed ring of cells around the chunk**. The ring exists for one
//! stage: the shore distance, which is a distance transform and therefore
//! the only quantity a cell's neighbours can change. Its radius is
//! [`SHORE_CELLS`], and the transform is run over the chunk *plus* that
//! ring, so a cell inside the chunk gets the same answer it would get if
//! the whole world had been transformed at once. That is what makes "a
//! chunk generated alone equals the same chunk generated as part of its
//! neighbourhood" a theorem rather than a hope.
//!
//! # Why it is resumable
//!
//! A client must never block a frame on generation. The build is a
//! sequence of bounded phases, each advanced by one [`ChunkBuild::step`],
//! and the partially-built chunk is readable throughout — so a chunk that
//! is not yet finished draws as the coarse relief the earlier phase
//! already answered rather than as nothing. Stopping between any two
//! phases and resuming later produces the same chunk as running them
//! back to back, because no phase reads anything a later one writes.

use alloc::vec::Vec;

use tairix_util::mathf;
use tairix_wintersun_net::value::ChunkCoord;

use crate::biome::{self, Blend, Conditions, Material};
use crate::error::WorldError;
use crate::geom::{
    chunk_origin, lerp, signed, smoothstep, CellCoord, Elevation, Moisture, Temperature,
    CHUNK_AREA, CHUNK_CELLS,
};
use crate::hydrology::FlowDir;
use crate::noise;
use crate::realm::RealmField;
use crate::scatter::{self, Ground, Scattered};
use crate::seed::Stage;

/// Cells the shore band reaches, and so the halo the build works over.
pub const SHORE_CELLS: u32 = 8;

/// Cells along one edge of the working grid: the chunk plus its halo.
const WORK_CELLS: u32 = CHUNK_CELLS + 2 * SHORE_CELLS;

/// Cells in the working grid.
const WORK_AREA: usize = (WORK_CELLS as usize) * (WORK_CELLS as usize);

/// World units between cycles of the fine detail field.
///
/// Absolute, not realm-relative: a hillside's texture is the same size in
/// a small realm and a large one, because it is texture and not structure.
const DETAIL_UNITS: f64 = 380.0;

/// Peak fine detail, in world units, on open ground.
const DETAIL_AMPLITUDE: f64 = 22.0;

/// Extra detail amplitude at the heart of a mountain belt.
const BELT_AMPLITUDE: f64 = 46.0;

/// World units between cycles of the dune field.
const DUNE_UNITS: f64 = 90.0;

/// Peak dune amplitude, in world units.
const DUNE_AMPLITUDE: f64 = 7.0;

/// Environmental lapse rate applied to the fine elevation's departure
/// from the coarse one, so a fine peak is colder than the coarse sample
/// it stands on.
const LAPSE_RATE: f64 = 0.0065;

/// Widest a channel's bed gets, in cells.
const CHANNEL_MAX_HALF_WIDTH: f64 = 7.0;

/// Deepest a channel cuts below its water surface, in world units.
const CHANNEL_MAX_DEPTH: f64 = 9.0;

/// Coarse discharge at which a channel reaches its widest.
const CHANNEL_FULL_DISCHARGE: f64 = 2600.0;

/// Cells either side of a road's centreline that are levelled.
const ROAD_HALF_WIDTH: f64 = 1.6;

/// What has been built on a cell.
///
/// A byte rather than five booleans: there is one of these per cell of
/// every cached chunk.
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq, Hash)]
pub struct Surface(u8);

impl Surface {
    /// A road runs over the cell.
    const ROAD: u8 = 1 << 0;
    /// A settlement has levelled the cell.
    const SETTLEMENT: u8 = 1 << 1;
    /// A river or stream bed.
    const CHANNEL: u8 = 1 << 2;
    /// Standing fresh water.
    const LAKE: u8 = 1 << 3;
    /// Sea.
    const SEA: u8 = 1 << 4;

    /// Whether a road runs over the cell.
    #[must_use]
    pub const fn is_road(self) -> bool {
        self.0 & Self::ROAD != 0
    }

    /// Whether a settlement has levelled the cell.
    #[must_use]
    pub const fn is_settlement(self) -> bool {
        self.0 & Self::SETTLEMENT != 0
    }

    /// Whether the cell is a river or stream bed.
    #[must_use]
    pub const fn is_channel(self) -> bool {
        self.0 & Self::CHANNEL != 0
    }

    /// Whether standing fresh water covers the cell.
    #[must_use]
    pub const fn is_lake(self) -> bool {
        self.0 & Self::LAKE != 0
    }

    /// Whether sea covers the cell.
    #[must_use]
    pub const fn is_sea(self) -> bool {
        self.0 & Self::SEA != 0
    }

    /// Whether anything has cleared the cell of growth.
    #[must_use]
    pub const fn is_cleared(self) -> bool {
        self.0 & (Self::ROAD | Self::SETTLEMENT) != 0
    }

    /// Whether any standing or running water covers the cell.
    #[must_use]
    pub const fn is_water(self) -> bool {
        self.0 & (Self::CHANNEL | Self::LAKE | Self::SEA) != 0
    }

    /// The packed flags, for a consumer folding a chunk into a digest.
    #[must_use]
    pub const fn bits(self) -> u8 {
        self.0
    }

    /// This surface with `flag` set.
    const fn with(self, flag: u8) -> Self {
        Self(self.0 | flag)
    }
}

/// A generated chunk.
#[derive(Clone, Debug)]
pub struct Chunk {
    coord: ChunkCoord,
    elevation: Vec<Elevation>,
    water: Vec<Elevation>,
    temperature: Vec<Temperature>,
    moisture: Vec<Moisture>,
    blend: Vec<Blend>,
    surface: Vec<Surface>,
    scatter: Vec<Scattered>,
}

impl Chunk {
    /// Which chunk this is.
    #[must_use]
    pub const fn coord(&self) -> ChunkCoord {
        self.coord
    }

    /// Ground height at an in-chunk cell.
    #[must_use]
    pub fn elevation(&self, cx: u32, cy: u32) -> Elevation {
        self.elevation[cell_index(cx, cy)]
    }

    /// Water-surface height at an in-chunk cell, equal to the ground
    /// where it is dry.
    #[must_use]
    pub fn water(&self, cx: u32, cy: u32) -> Elevation {
        self.water[cell_index(cx, cy)]
    }

    /// Air temperature at an in-chunk cell.
    #[must_use]
    pub fn temperature(&self, cx: u32, cy: u32) -> Temperature {
        self.temperature[cell_index(cx, cy)]
    }

    /// Relative moisture at an in-chunk cell.
    #[must_use]
    pub fn moisture(&self, cx: u32, cy: u32) -> Moisture {
        self.moisture[cell_index(cx, cy)]
    }

    /// Material blend at an in-chunk cell.
    #[must_use]
    pub fn blend(&self, cx: u32, cy: u32) -> Blend {
        self.blend[cell_index(cx, cy)]
    }

    /// What has been built on an in-chunk cell.
    #[must_use]
    pub fn surface(&self, cx: u32, cy: u32) -> Surface {
        self.surface[cell_index(cx, cy)]
    }

    /// Everything standing in the chunk.
    #[must_use]
    pub fn scatter(&self) -> &[Scattered] {
        &self.scatter
    }

    /// Overwrite every retained byte.
    ///
    /// Called by the cache before the allocation is freed.
    pub(crate) fn scrub(&mut self) {
        self.elevation.fill(Elevation::SEA_LEVEL);
        self.water.fill(Elevation::SEA_LEVEL);
        self.temperature.fill(Temperature::default());
        self.moisture.fill(Moisture::default());
        self.blend.fill(Blend::solid(Material::Rock));
        self.surface.fill(Surface::default());
        self.scatter.clear();
    }

    /// Heap bytes this chunk retains.
    #[must_use]
    pub fn payload_bytes(&self) -> usize {
        core::mem::size_of_val(self.elevation.as_slice())
            + core::mem::size_of_val(self.water.as_slice())
            + core::mem::size_of_val(self.temperature.as_slice())
            + core::mem::size_of_val(self.moisture.as_slice())
            + core::mem::size_of_val(self.blend.as_slice())
            + core::mem::size_of_val(self.surface.as_slice())
            + self.scatter.capacity() * core::mem::size_of::<Scattered>()
    }
}

/// The row-major index of an in-chunk cell, wrapped onto the chunk.
const fn cell_index(cx: u32, cy: u32) -> usize {
    let mask = CHUNK_CELLS - 1;
    (((cy & mask) * CHUNK_CELLS) + (cx & mask)) as usize
}

/// How far a build has got.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub enum Phase {
    /// Fine relief over the working grid. Nothing is readable before it.
    Relief,
    /// Channels carved, lakes and sea filled, shore distance measured.
    Water,
    /// Settlements levelled and roads laid.
    Structures,
    /// Temperature and moisture.
    Climate,
    /// Materials classified.
    Biome,
    /// Vegetation, rock and resource nodes placed.
    Scatter,
    /// Nothing left to do.
    Done,
}

impl Phase {
    /// The phase after this one.
    const fn next(self) -> Self {
        match self {
            Self::Relief => Self::Water,
            Self::Water => Self::Structures,
            Self::Structures => Self::Climate,
            Self::Climate => Self::Biome,
            Self::Biome => Self::Scatter,
            Self::Scatter | Self::Done => Self::Done,
        }
    }
}

/// One coarse drainage link, as a line the fine carve follows.
#[derive(Copy, Clone, Debug)]
struct Channel {
    /// Upstream end, in cells.
    from: (f64, f64),
    /// Downstream end, in cells.
    to: (f64, f64),
    /// Water surface at each end, in world units.
    surface: (f64, f64),
    /// Half-width of the bed, in cells.
    half_width: f64,
    /// Depth below the water surface, in world units.
    depth: f64,
}

/// A chunk under construction.
#[derive(Debug)]
pub struct ChunkBuild {
    coord: ChunkCoord,
    phase: Phase,
    chunk: Chunk,
    /// Fine ground height over the chunk *and* its halo, in world units.
    work_ground: Vec<f64>,
    /// Whether each working cell is under standing water.
    work_wet: Vec<bool>,
    /// Cells to the nearest water by the four-neighbour metric,
    /// saturating at [`u16::MAX`].
    work_shore: Vec<u16>,
}

impl ChunkBuild {
    /// Start building `coord`.
    ///
    /// # Errors
    ///
    /// [`WorldError::OutOfMemory`] if the chunk's arrays do not fit.
    pub fn new(coord: ChunkCoord) -> Result<Self, WorldError> {
        use crate::realm::try_filled;
        Ok(Self {
            coord,
            phase: Phase::Relief,
            chunk: Chunk {
                coord,
                elevation: try_filled(CHUNK_AREA, Elevation::SEA_LEVEL)?,
                water: try_filled(CHUNK_AREA, Elevation::SEA_LEVEL)?,
                temperature: try_filled(CHUNK_AREA, Temperature::default())?,
                moisture: try_filled(CHUNK_AREA, Moisture::default())?,
                blend: try_filled(CHUNK_AREA, Blend::solid(Material::Rock))?,
                surface: try_filled(CHUNK_AREA, Surface::default())?,
                scatter: Vec::new(),
            },
            work_ground: try_filled(WORK_AREA, 0.0)?,
            work_wet: try_filled(WORK_AREA, false)?,
            work_shore: try_filled(WORK_AREA, u16::MAX)?,
        })
    }

    /// How far the build has got.
    #[must_use]
    pub const fn phase(&self) -> Phase {
        self.phase
    }

    /// What has been built so far.
    ///
    /// Readable at every phase, so a client draws the coarse ground it
    /// already has rather than waiting or drawing nothing.
    #[must_use]
    pub const fn partial(&self) -> &Chunk {
        &self.chunk
    }

    /// Advance one phase, returning the phase now reached.
    ///
    /// # Errors
    ///
    /// [`WorldError::OutOfMemory`] if a phase's working memory does not
    /// fit. The build keeps whatever it had, so a caller may retry when
    /// the machine has recovered.
    pub fn step(&mut self, field: &RealmField) -> Result<Phase, WorldError> {
        match self.phase {
            Phase::Relief => self.relief(field),
            Phase::Water => self.water(field)?,
            Phase::Structures => self.structures(field),
            Phase::Climate => self.climate(field),
            Phase::Biome => self.biome(field),
            Phase::Scatter => self.scatter(field)?,
            Phase::Done => return Ok(Phase::Done),
        }
        self.phase = self.phase.next();
        Ok(self.phase)
    }

    /// Run every remaining phase.
    ///
    /// # Errors
    ///
    /// Whatever [`Self::step`] refuses.
    pub fn finish(mut self, field: &RealmField) -> Result<Chunk, WorldError> {
        while self.phase != Phase::Done {
            self.step(field)?;
        }
        Ok(self.chunk)
    }

    /// The working-grid cell at a chunk-relative offset, which the halo
    /// shifts.
    fn work_index(wx: u32, wy: u32) -> usize {
        (wy as usize) * (WORK_CELLS as usize) + (wx as usize)
    }

    /// The absolute cell a working-grid position refers to.
    fn work_cell(&self, wx: u32, wy: u32) -> CellCoord {
        let origin = chunk_origin(self.coord);
        CellCoord::new(
            origin.x + signed(wx) - signed(SHORE_CELLS),
            origin.y + signed(wy) - signed(SHORE_CELLS),
        )
    }

    /// Fine relief over the working grid.
    fn relief(&mut self, field: &RealmField) {
        let key = field.key();
        for wy in 0..WORK_CELLS {
            for wx in 0..WORK_CELLS {
                let cell = self.work_cell(wx, wy);
                let (gx, gy) = field.grid_position(cell);
                let coarse = field.elevation_units_at(gx, gy);
                let belt = field.belt_at(gx, gy);
                let damp = field.moisture_at(gx, gy);

                let (nx, ny) = (
                    f64::from(cell.x) / DETAIL_UNITS,
                    f64::from(cell.y) / DETAIL_UNITS,
                );
                // Ridged inside a belt, ordinary fractal outside it, mixed
                // across the transition — which is what makes a range read
                // as ridges and a plain as ground.
                let rough = lerp(
                    noise::fbm(key, Stage::Detail, nx, ny),
                    noise::ridged(key, Stage::Ridge, nx, ny) * 2.0 - 1.0,
                    smoothstep(belt),
                );
                let amplitude = lerp(DETAIL_AMPLITUDE, BELT_AMPLITUDE, smoothstep(belt));

                // Dunes only where it is dry and already flat: a billow on
                // a hillside would read as noise, not as sand.
                let arid = mathf::clamp(1.0 - damp * 2.2, 0.0, 1.0);
                let dune = noise::billow(
                    key,
                    Stage::Dune,
                    f64::from(cell.x) / DUNE_UNITS,
                    f64::from(cell.y) / DUNE_UNITS,
                ) * DUNE_AMPLITUDE
                    * arid;

                // Detail fades out below the waterline: a sea floor the
                // player never sees does not need texture, and letting it
                // poke above sea level would move the coastline the realm
                // asked for.
                let submerged_fade = mathf::clamp(coarse / 12.0, 0.0, 1.0);
                let height = coarse + (rough * amplitude + dune) * submerged_fade;

                self.work_ground[Self::work_index(wx, wy)] = height;
            }
        }
    }

    /// Carve channels, fill lakes and sea, and measure the shore.
    fn water(&mut self, field: &RealmField) -> Result<(), WorldError> {
        let channels = self.channels(field)?;

        for wy in 0..WORK_CELLS {
            for wx in 0..WORK_CELLS {
                let index = Self::work_index(wx, wy);
                let cell = self.work_cell(wx, wy);
                let (gx, gy) = field.grid_position(cell);
                let point = (f64::from(cell.x), f64::from(cell.y));

                let mut ground = self.work_ground[index];
                let mut surface = f64::MIN;
                let mut in_channel = false;

                for channel in &channels {
                    let (distance, along) = distance_to_segment(channel.from, channel.to, point);
                    if distance >= channel.half_width {
                        continue;
                    }
                    let level = lerp(channel.surface.0, channel.surface.1, along);
                    // A parabolic bed, so a bank rises out of the water
                    // rather than stepping out of it.
                    let across = distance / channel.half_width;
                    let bed = level - channel.depth * (1.0 - across * across);
                    ground = mathf::fmin(ground, bed);
                    surface = mathf::fmax(surface, level);
                    in_channel = true;
                }

                // Standing water: the coarse field already knows where a
                // lake's surface is and where the sea is.
                let coarse_water = field.water_units_at(gx, gy);
                let standing = coarse_water > ground;
                if standing {
                    surface = mathf::fmax(surface, coarse_water);
                }

                let wet = in_channel || standing;
                self.work_ground[index] = ground;
                self.work_wet[index] = wet;
                self.work_shore[index] = if wet { 0 } else { u16::MAX };

                if let Some((cx, cy)) = Self::in_chunk(wx, wy) {
                    let slot = cell_index(cx, cy);
                    self.chunk.elevation[slot] = Elevation::from_units(ground);
                    self.chunk.water[slot] =
                        Elevation::from_units(if wet { surface } else { ground });
                    if in_channel {
                        self.chunk.surface[slot] = self.chunk.surface[slot].with(Surface::CHANNEL);
                    }
                    if standing {
                        let flag = if coarse_water > 0.0 {
                            Surface::LAKE
                        } else {
                            Surface::SEA
                        };
                        self.chunk.surface[slot] = self.chunk.surface[slot].with(flag);
                    }
                }
            }
        }

        self.measure_shore();
        Ok(())
    }

    /// The coarse drainage links that can reach this chunk.
    fn channels(&self, field: &RealmField) -> Result<Vec<Channel>, WorldError> {
        let params = field.params();
        let step = f64::from(params.cells_per_coarse());
        // One coarse sample beyond the widest a bed gets, so a channel
        // whose centreline is outside the working grid still carves the
        // bank that reaches into it.
        let margin = mathf::round_i32(mathf::ceil(CHANNEL_MAX_HALF_WIDTH / step)) + 1;

        let (gx, gy) = field.chunk_grid_position(self.coord);
        let lo = (
            mathf::round_i32(mathf::floor(gx)) - margin,
            mathf::round_i32(mathf::floor(gy)) - margin,
        );
        let span = signed(CHUNK_CELLS) / signed(params.cells_per_coarse()).max(1) + 2 * margin + 2;

        let mut channels = Vec::new();
        let across = usize::try_from(span).unwrap_or(0) + 1;
        channels
            .try_reserve(across * across)
            .map_err(|_| WorldError::OutOfMemory)?;

        for dy in 0..=span {
            for dx in 0..=span {
                let (sx, sy) = (lo.0 + dx, lo.1 + dy);
                let sample = field.sample(sx, sy);
                if sample.flow == FlowDir::Sink || sample.elevation.is_submerged() {
                    continue;
                }
                let Some((ox, oy)) = sample.flow.offset() else {
                    continue;
                };
                let downstream = field.sample(sx + ox, sy + oy);
                let discharge = f64::from(sample.discharge) / CHANNEL_FULL_DISCHARGE;
                let fullness =
                    mathf::clamp(mathf::sqrt(mathf::clamp(discharge, 0.0, 1.0)), 0.0, 1.0);
                let half_width = 0.9 + fullness * (CHANNEL_MAX_HALF_WIDTH - 0.9);
                if sample.discharge < 8 {
                    continue;
                }
                channels.push(Channel {
                    from: grid_to_cells(field, sx, sy),
                    to: grid_to_cells(field, sx + ox, sy + oy),
                    surface: (sample.water.units(), downstream.water.units()),
                    half_width,
                    depth: 1.2 + fullness * (CHANNEL_MAX_DEPTH - 1.2),
                });
            }
        }
        Ok(channels)
    }

    /// Two chamfer passes over the working grid, which is exact inside the
    /// chunk because the grid extends [`SHORE_CELLS`] beyond it.
    fn measure_shore(&mut self) {
        for wy in 0..WORK_CELLS {
            for wx in 0..WORK_CELLS {
                let index = Self::work_index(wx, wy);
                let mut best = self.work_shore[index];
                if wx > 0 {
                    best = best.min(self.work_shore[index - 1].saturating_add(1));
                }
                if wy > 0 {
                    best =
                        best.min(self.work_shore[index - (WORK_CELLS as usize)].saturating_add(1));
                }
                self.work_shore[index] = best;
            }
        }
        for wy in (0..WORK_CELLS).rev() {
            for wx in (0..WORK_CELLS).rev() {
                let index = Self::work_index(wx, wy);
                let mut best = self.work_shore[index];
                if wx + 1 < WORK_CELLS {
                    best = best.min(self.work_shore[index + 1].saturating_add(1));
                }
                if wy + 1 < WORK_CELLS {
                    best =
                        best.min(self.work_shore[index + (WORK_CELLS as usize)].saturating_add(1));
                }
                self.work_shore[index] = best;
            }
        }
    }

    /// Level settlements and lay roads.
    fn structures(&mut self, field: &RealmField) {
        let origin = chunk_origin(self.coord);
        let reach = i64::from(CHUNK_CELLS) + i64::from(SHORE_CELLS);

        for site in field.sites() {
            if i64::from(site.at.x - origin.x).abs() > reach + i64::from(site.radius_cells)
                || i64::from(site.at.y - origin.y).abs() > reach + i64::from(site.radius_cells)
            {
                continue;
            }
            let (gx, gy) = field.grid_position(site.at);
            let level = field.elevation_units_at(gx, gy);
            let radius = f64::from(site.radius_cells);
            self.level_around(|point| {
                let dx = point.0 - f64::from(site.at.x);
                let dy = point.1 - f64::from(site.at.y);
                let distance = mathf::hypot(dx, dy);
                (distance < radius).then(|| {
                    (
                        level,
                        1.0 - smoothstep(distance / radius),
                        Surface::SETTLEMENT,
                    )
                })
            });
        }

        for road in field.roads() {
            for pair in road.path.windows(2) {
                let (a, b) = (pair[0], pair[1]);
                let near = i64::from(a.x - origin.x)
                    .abs()
                    .min(i64::from(b.x - origin.x).abs())
                    <= reach
                    && i64::from(a.y - origin.y)
                        .abs()
                        .min(i64::from(b.y - origin.y).abs())
                        <= reach;
                if !near {
                    continue;
                }
                let ga = field.grid_position(a);
                let gb = field.grid_position(b);
                let height_a = field.elevation_units_at(ga.0, ga.1);
                let height_b = field.elevation_units_at(gb.0, gb.1);
                self.level_around(|point| {
                    let (distance, along) = distance_to_segment(
                        (f64::from(a.x), f64::from(a.y)),
                        (f64::from(b.x), f64::from(b.y)),
                        point,
                    );
                    (distance < ROAD_HALF_WIDTH).then(|| {
                        (
                            lerp(height_a, height_b, along),
                            1.0 - smoothstep(distance / ROAD_HALF_WIDTH),
                            Surface::ROAD,
                        )
                    })
                });
            }
        }
    }

    /// Pull the ground toward a target height wherever `shape` claims a
    /// cell, and record the flag it claims it with.
    fn level_around(&mut self, shape: impl Fn((f64, f64)) -> Option<(f64, f64, u8)>) {
        for wy in 0..WORK_CELLS {
            for wx in 0..WORK_CELLS {
                let index = Self::work_index(wx, wy);
                if self.work_wet[index] {
                    continue;
                }
                let cell = self.work_cell(wx, wy);
                let point = (f64::from(cell.x), f64::from(cell.y));
                let Some((target, strength, flag)) = shape(point) else {
                    continue;
                };
                let levelled = lerp(
                    self.work_ground[index],
                    target,
                    mathf::clamp(strength, 0.0, 1.0),
                );
                self.work_ground[index] = levelled;
                if let Some((cx, cy)) = Self::in_chunk(wx, wy) {
                    let slot = cell_index(cx, cy);
                    self.chunk.elevation[slot] = Elevation::from_units(levelled);
                    self.chunk.water[slot] = self.chunk.elevation[slot];
                    if strength > 0.35 {
                        self.chunk.surface[slot] = self.chunk.surface[slot].with(flag);
                    }
                }
            }
        }
    }

    /// Interpolate the coarse climate and correct it for the fine relief.
    fn climate(&mut self, field: &RealmField) {
        for cy in 0..CHUNK_CELLS {
            for cx in 0..CHUNK_CELLS {
                let slot = cell_index(cx, cy);
                let cell = self.work_cell(cx + SHORE_CELLS, cy + SHORE_CELLS);
                let (gx, gy) = field.grid_position(cell);
                let coarse_height = field.elevation_units_at(gx, gy);
                let fine_height = self.chunk.elevation[slot].units();

                self.chunk.temperature[slot] = Temperature::from_celsius(
                    field.temperature_celsius_at(gx, gy)
                        - (fine_height - coarse_height) * LAPSE_RATE,
                );
                self.chunk.moisture[slot] = Moisture::from_fraction(field.moisture_at(gx, gy));
            }
        }
    }

    /// Classify every cell.
    fn biome(&mut self, field: &RealmField) {
        for cy in 0..CHUNK_CELLS {
            for cx in 0..CHUNK_CELLS {
                let slot = cell_index(cx, cy);
                self.chunk.blend[slot] = biome::classify(self.conditions(field, cx, cy));
            }
        }
    }

    /// What the classifier reads about an in-chunk cell.
    fn conditions(&self, field: &RealmField, cx: u32, cy: u32) -> Conditions {
        let slot = cell_index(cx, cy);
        let (wx, wy) = (cx + SHORE_CELLS, cy + SHORE_CELLS);
        let index = Self::work_index(wx, wy);
        let cell = self.work_cell(wx, wy);
        let (gx, gy) = field.grid_position(cell);

        Conditions {
            temperature: self.chunk.temperature[slot],
            moisture: self.chunk.moisture[slot],
            elevation_units: self.work_ground[index],
            slope: self.slope(wx, wy),
            belt: field.belt_at(gx, gy),
            submerged: self.work_wet[index],
            dryness: mathf::clamp(
                f64::from(self.work_shore[index].min(u16::from(u8::MAX))) / f64::from(SHORE_CELLS),
                0.0,
                1.0,
            ),
        }
    }

    /// The steepest rise to a four-neighbour, in world units.
    fn slope(&self, wx: u32, wy: u32) -> f64 {
        let here = self.work_ground[Self::work_index(wx, wy)];
        let mut steepest = 0.0;
        for (dx, dy) in [(1_i32, 0_i32), (0, 1), (-1, 0), (0, -1)] {
            let (Some(nx), Some(ny)) = (wx.checked_add_signed(dx), wy.checked_add_signed(dy))
            else {
                continue;
            };
            if nx >= WORK_CELLS || ny >= WORK_CELLS {
                continue;
            }
            let there = self.work_ground[Self::work_index(nx, ny)];
            steepest = mathf::fmax(steepest, mathf::fabs(here - there));
        }
        steepest
    }

    /// Place the scatter.
    fn scatter(&mut self, field: &RealmField) -> Result<(), WorldError> {
        let ground = |cell: CellCoord| -> Ground {
            let origin = chunk_origin(self.coord);
            let halo = signed(SHORE_CELLS);
            let wx = u32::try_from(cell.x - origin.x + halo).unwrap_or(u32::MAX);
            let wy = u32::try_from(cell.y - origin.y + halo).unwrap_or(u32::MAX);
            if wx >= WORK_CELLS || wy >= WORK_CELLS {
                // Outside the working grid a candidate cannot reach a cell
                // of this chunk, so refusing it costs nothing and keeps the
                // query total.
                return Ground {
                    blend: Blend::solid(Material::Rock),
                    slope: f64::MAX,
                    moisture: 0.0,
                    submerged: true,
                    cleared: true,
                };
            }
            let index = Self::work_index(wx, wy);
            let inside = Self::in_chunk(wx, wy);
            let slot = inside.map(|(cx, cy)| cell_index(cx, cy));
            Ground {
                blend: slot.map_or_else(
                    || biome::classify(self.halo_conditions(field, wx, wy)),
                    |slot| self.chunk.blend[slot],
                ),
                slope: self.slope(wx, wy),
                moisture: slot.map_or_else(
                    || {
                        let (gx, gy) = field.grid_position(self.work_cell(wx, wy));
                        field.moisture_at(gx, gy)
                    },
                    |slot| self.chunk.moisture[slot].fraction(),
                ),
                submerged: self.work_wet[index],
                cleared: slot.is_some_and(|slot| self.chunk.surface[slot].is_cleared()),
            }
        };
        let placed = scatter::for_chunk(field.key(), self.coord, &ground)?;
        self.chunk.scatter = placed;
        Ok(())
    }

    /// The classifier's view of a halo cell, which has no chunk slot.
    fn halo_conditions(&self, field: &RealmField, wx: u32, wy: u32) -> Conditions {
        let index = Self::work_index(wx, wy);
        let cell = self.work_cell(wx, wy);
        let (gx, gy) = field.grid_position(cell);
        let coarse_height = field.elevation_units_at(gx, gy);
        let fine_height = self.work_ground[index];
        Conditions {
            temperature: Temperature::from_celsius(
                field.temperature_celsius_at(gx, gy) - (fine_height - coarse_height) * LAPSE_RATE,
            ),
            moisture: Moisture::from_fraction(field.moisture_at(gx, gy)),
            elevation_units: fine_height,
            slope: self.slope(wx, wy),
            belt: field.belt_at(gx, gy),
            submerged: self.work_wet[index],
            dryness: mathf::clamp(
                f64::from(self.work_shore[index].min(u16::from(u8::MAX))) / f64::from(SHORE_CELLS),
                0.0,
                1.0,
            ),
        }
    }

    /// The in-chunk cell a working-grid position refers to, or `None` for
    /// a halo cell.
    fn in_chunk(wx: u32, wy: u32) -> Option<(u32, u32)> {
        let cx = wx.checked_sub(SHORE_CELLS)?;
        let cy = wy.checked_sub(SHORE_CELLS)?;
        (cx < CHUNK_CELLS && cy < CHUNK_CELLS).then_some((cx, cy))
    }
}

/// The cell position of a coarse grid sample.
fn grid_to_cells(field: &RealmField, sx: i32, sy: i32) -> (f64, f64) {
    let params = field.params();
    let origin = params.min_chunk() * signed(CHUNK_CELLS);
    let step = signed(params.cells_per_coarse());
    (f64::from(origin + sx * step), f64::from(origin + sy * step))
}

/// Distance from `point` to the segment `a`–`b`, and the parameter of the
/// nearest point on it.
fn distance_to_segment(a: (f64, f64), b: (f64, f64), point: (f64, f64)) -> (f64, f64) {
    let (dx, dy) = (b.0 - a.0, b.1 - a.1);
    let length2 = dx * dx + dy * dy;
    if length2 <= f64::EPSILON {
        return (mathf::hypot(point.0 - a.0, point.1 - a.1), 0.0);
    }
    let raw = ((point.0 - a.0) * dx + (point.1 - a.1) * dy) / length2;
    let along = mathf::clamp(raw, 0.0, 1.0);
    let nearest = (a.0 + dx * along, a.1 + dy * along);
    (
        mathf::hypot(point.0 - nearest.0, point.1 - nearest.1),
        along,
    )
}

#[cfg(test)]
mod tests;
