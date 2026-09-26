//! The ground pass: what the generated world looks like, one horizontal
//! span at a time.
//!
//! The world generator answers per *cell*; a frame is per *pixel*, and at
//! the authored zoom a cell is thirty-two of them. So the pass works on a
//! lattice of the visible cells' weight fields, interpolates it vertically
//! once per raster row, and hands each horizontal run between two lattice
//! columns to the splat, which steps the rest. That is what turns four
//! hash evaluations per pixel into four per span.
//!
//! # Nothing here reads the world
//!
//! [`TerrainGrid::rebuild`] samples a window of chunks the caller already
//! holds and a material cache the caller has already asked. A lattice
//! point whose chunk is not resident is *marked*, not fetched and not
//! waited for, and the pass draws it as ground the client cannot vouch
//! for. A paint that fetched would be the frame-loop read the charter
//! forbids, and a paint that guessed would be worse.

use alloc::vec::Vec;
use core::ops::RangeInclusive;

use tairix_raster::color::{Color, Pixel};
use tairix_wintersun_art::cache::{MaterialCache, TileKey};
use tairix_wintersun_art::decal::{Bounds, Decal, Fray};
use tairix_wintersun_art::material::{self, Mip, Quality};
use tairix_wintersun_art::splat::{splat, Geometry, SpanPlan, SpanTiles, Warp};
use tairix_wintersun_art::weight::WeightField;
use tairix_wintersun_net::value::{ChunkCoord, WorldPoint};
use tairix_wintersun_world::biome::{Material, BLEND_SLOTS};
use tairix_wintersun_world::chunk::{Chunk, ChunkWindow};
use tairix_wintersun_world::geom::{CellCoord, CELL_SUB_UNITS, CHUNK_CELLS_LOG2};
use tairix_wintersun_world::realm::RealmField;

use crate::error::ClientError;

/// Half a cell, which is where a cell's sample sits.
const HALF_CELL: i32 = CELL_SUB_UNITS / 2;

/// Ground the client has not got.
///
/// Deliberately not a terrain colour: a placeholder that looked like
/// ground would have the client asserting a world it has not been told
/// about, and a player walking into a region that had not arrived would
/// see plausible fiction rather than an obvious gap.
pub const UNMAPPED: Color = Color::rgb(14, 15, 20);

/// The lattice column or row a world coordinate falls between.
///
/// Samples sit at cell centres, so the lattice is offset half a cell from
/// the cell grid; a coordinate west of the first centre belongs to the
/// span before it, which is why this floors rather than truncates.
fn lattice_of(world: i32) -> i32 {
    clamp_i32(offset(world).div_euclid(i64::from(CELL_SUB_UNITS)))
}

/// How far between two lattice samples a coordinate lies, out of 255.
fn lattice_fraction(world: i32) -> u8 {
    let within = offset(world).rem_euclid(i64::from(CELL_SUB_UNITS));
    u8::try_from(within * 255 / i64::from(CELL_SUB_UNITS)).unwrap_or(u8::MAX)
}

/// A world coordinate measured from the first lattice sample, in 64 bits
/// so a coordinate at the extreme of the wire type cannot overflow it.
fn offset(world: i32) -> i64 {
    i64::from(world) - i64::from(HALF_CELL)
}

/// The world position of a lattice sample.
fn sample_world(lattice: i32) -> i32 {
    clamp_i32(i64::from(lattice) * i64::from(CELL_SUB_UNITS) + i64::from(HALF_CELL))
}

/// A 64-bit world coordinate brought back into the 32-bit wire type.
fn clamp_i32(value: i64) -> i32 {
    i32::try_from(value).unwrap_or(if value < 0 { i32::MIN } else { i32::MAX })
}

/// The visible cells' ground, sampled once per frame.
///
/// Rebuilt into its own buffers rather than reallocated, so a steady
/// camera costs no allocation at all and a moving one costs only the
/// growth.
#[derive(Debug)]
pub struct TerrainGrid {
    origin: CellCoord,
    cols: usize,
    rows: usize,
    weights: Vec<WeightField>,
    ground: Vec<i16>,
    mapped: Vec<bool>,
    materials: u32,
    unmapped: usize,
}

impl Default for TerrainGrid {
    fn default() -> Self {
        Self::new()
    }
}

impl TerrainGrid {
    /// An empty grid, sized on its first rebuild.
    #[must_use]
    pub fn new() -> Self {
        Self {
            origin: CellCoord::new(0, 0),
            cols: 0,
            rows: 0,
            weights: Vec::new(),
            ground: Vec::new(),
            mapped: Vec::new(),
            materials: 0,
            unmapped: 0,
        }
    }

    /// Sample every lattice point the `visible` extent needs, stamping the
    /// decals that overlap it.
    ///
    /// # Errors
    ///
    /// [`ClientError::OutOfMemory`] if the lattice does not fit. The
    /// buffers are reused, so this is reached only when the view grows.
    pub fn rebuild(
        &mut self,
        window: &ChunkWindow<'_>,
        visible: Bounds,
        decals: &[Decal<'_>],
        fray: &Fray,
    ) -> Result<(), ClientError> {
        let first_x = lattice_of(visible.min_x);
        let first_y = lattice_of(visible.min_y);
        // One sample past the last, because the span ending at the right
        // edge interpolates toward it.
        let cols = span_count(first_x, lattice_of(visible.max_x));
        let rows = span_count(first_y, lattice_of(visible.max_y));
        let points = cols.checked_mul(rows).ok_or(ClientError::OutOfMemory)?;

        self.origin = CellCoord::new(first_x, first_y);
        self.cols = cols;
        self.rows = rows;
        self.materials = 0;
        self.unmapped = 0;
        resize(
            &mut self.weights,
            points,
            WeightField::solid(Material::Rock),
        )?;
        resize(&mut self.ground, points, 0)?;
        resize(&mut self.mapped, points, false)?;

        let touching: Vec<&Decal<'_>> = decals
            .iter()
            .filter(|d| d.bounds().is_some_and(|b| b.overlaps(&visible)))
            .collect();

        for row in 0..rows {
            for col in 0..cols {
                let cell = CellCoord::new(
                    first_x.saturating_add(signed(col)),
                    first_y.saturating_add(signed(row)),
                );
                let index = row * cols + col;
                let Some(chunk) = window.chunk(cell) else {
                    self.mapped[index] = false;
                    self.unmapped += 1;
                    continue;
                };
                let (cx, cy) = cell.within_chunk();
                let mut field = WeightField::from_blend(&chunk.blend(cx, cy));
                let at = WorldPoint {
                    x: sample_world(cell.x),
                    y: sample_world(cell.y),
                };
                for decal in &touching {
                    decal.stamp(&mut field, fray, at);
                }
                for slot in field.slots() {
                    self.materials |= 1 << slot.material.id();
                }
                self.weights[index] = field;
                self.ground[index] = chunk.elevation(cx, cy).0;
                self.mapped[index] = true;
            }
        }
        Ok(())
    }

    /// The lattice extent, in samples.
    #[must_use]
    pub const fn extent(&self) -> (usize, usize) {
        (self.cols, self.rows)
    }

    /// The cell the lattice starts at.
    #[must_use]
    pub const fn origin(&self) -> CellCoord {
        self.origin
    }

    /// How many lattice points had no resident chunk.
    ///
    /// Reported rather than hidden: a frame drawn over a gap is a
    /// diagnosis, not a defect in the pass.
    #[must_use]
    pub const fn unmapped(&self) -> usize {
        self.unmapped
    }

    /// The ground height at a lattice point, or `None` where it is not
    /// resident.
    #[must_use]
    pub fn ground(&self, col: usize, row: usize) -> Option<i16> {
        let index = self.index(col, row)?;
        self.mapped[index].then(|| self.ground[index])
    }

    /// Every material any visible sample carries.
    ///
    /// The ask phase reads this and makes exactly those tiles resident,
    /// so the paint phase only ever peeks.
    pub fn materials(&self) -> impl Iterator<Item = Material> + '_ {
        Material::ALL
            .into_iter()
            .filter(move |m| self.materials & (1 << m.id()) != 0)
    }

    /// The lattice sample nearest and north-west of a world position, or
    /// `None` where it lies outside the grid.
    #[must_use]
    pub fn lattice_at(&self, at: WorldPoint) -> Option<(usize, usize)> {
        let col = usize::try_from(lattice_of(at.x) - self.origin.x).ok()?;
        let row = usize::try_from(lattice_of(at.y) - self.origin.y).ok()?;
        (col < self.cols && row < self.rows).then_some((col, row))
    }

    fn index(&self, col: usize, row: usize) -> Option<usize> {
        (col < self.cols && row < self.rows).then_some(row * self.cols + col)
    }

    /// The field at a lattice point, vertically interpolated between rows
    /// `row` and `row + 1` by `t`, or `None` if either is unmapped.
    fn column(&self, col: usize, row: usize, t: u8) -> Option<WeightField> {
        let near = self.index(col, row)?;
        let far = self.index(col, row + 1)?;
        (self.mapped[near] && self.mapped[far])
            .then(|| self.weights[near].lerp(&self.weights[far], t))
    }
}

/// Half the carriageway a road covers completely, in world sub-units.
///
/// The same 1.6 cells the generator levelled the ground over, so the
/// worn surface and the levelled ground are the same road rather than
/// two that nearly coincide.
const ROAD_HALF_WIDTH: u32 = (CELL_SUB_UNITS as u32) * 8 / 5;

/// How far a road's surface frays out into what it crosses.
const ROAD_FEATHER: u32 = CELL_SUB_UNITS as u32;

/// How much of the ground a road takes at its centreline, out of the
/// weight field's total.
///
/// Not all of it: letting a little of what it crosses through is what
/// makes a track over heath look unlike the same track over sand.
const ROAD_COVERAGE: u16 = 220;

/// A realm's roads as paths the decal stamp can read.
///
/// Converted once per realm rather than per frame: the generator answers
/// in cells and a stamp works in sub-units, and a road does not move.
#[derive(Debug, Default)]
pub struct RoadDecals {
    paths: alloc::vec::Vec<alloc::vec::Vec<WorldPoint>>,
}

impl RoadDecals {
    /// Convert every road the realm routed.
    ///
    /// # Errors
    ///
    /// [`ClientError::OutOfMemory`] if the paths do not fit.
    pub fn from_realm(field: &RealmField) -> Result<Self, ClientError> {
        let mut paths = alloc::vec::Vec::new();
        paths
            .try_reserve(field.roads().len())
            .map_err(|_| ClientError::OutOfMemory)?;
        for road in field.roads() {
            let mut path = alloc::vec::Vec::new();
            path.try_reserve(road.path.len())
                .map_err(|_| ClientError::OutOfMemory)?;
            path.extend(road.path.iter().filter_map(|cell| cell.centre()));
            if path.len() >= 2 {
                paths.push(path);
            }
        }
        Ok(Self { paths })
    }

    /// The decal for each road, borrowing the paths.
    ///
    /// # Errors
    ///
    /// [`ClientError::OutOfMemory`] if the list does not fit.
    pub fn decals(&self) -> Result<alloc::vec::Vec<Decal<'_>>, ClientError> {
        let mut out = alloc::vec::Vec::new();
        out.try_reserve(self.paths.len())
            .map_err(|_| ClientError::OutOfMemory)?;
        out.extend(self.paths.iter().map(|path| Decal {
            material: Material::Gravel,
            path,
            half_width: ROAD_HALF_WIDTH,
            feather: ROAD_FEATHER,
            coverage: ROAD_COVERAGE,
        }));
        Ok(out)
    }

    /// How many roads were converted.
    #[must_use]
    pub fn len(&self) -> usize {
        self.paths.len()
    }

    /// Whether the realm routed no roads at all.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.paths.is_empty()
    }
}

/// Every chunk a view needs, in `ChunkCoord` order.
///
/// The order matters and is the coordinate's own: a [`ChunkWindow`] is
/// binary-searched, so its slice has to be sorted, and producing it
/// sorted is cheaper than sorting it. `ChunkCoord` compares eastings
/// before northings, so the walk is column-major to match.
pub fn visible_chunks(visible: Bounds) -> impl Iterator<Item = ChunkCoord> {
    let (eastings, northings) = chunk_span(visible);
    eastings.flat_map(move |x| northings.clone().map(move |y| ChunkCoord { x, y }))
}

/// Whether the chunk at `coord` is worth holding for a view covering
/// `visible`: one the view needs, or one within a chunk of those, so a view
/// panning back and forth over an edge does not give away ground it is
/// about to ask for again.
///
/// What bounds the ground a client holds to the view's working set, however
/// far the player walks.
#[must_use]
pub fn worth_holding(coord: ChunkCoord, visible: Bounds) -> bool {
    let (eastings, northings) = chunk_span(visible);
    let near = |at: i32, span: &RangeInclusive<i32>| {
        at >= span.start().saturating_sub(1) && at <= span.end().saturating_add(1)
    };
    near(coord.x, &eastings) && near(coord.y, &northings)
}

/// The ground a client holds: generated chunks, in coordinate order, kept
/// to the working set of the view looking at them ([`worth_holding`]).
///
/// Solving a chunk is tens of milliseconds of work, so a view that moves or
/// resizes keeps the chunks it still covers rather than solving them again.
#[derive(Debug, Default)]
pub struct HeldGround {
    held: Vec<Chunk>,
}

impl HeldGround {
    /// Nothing held.
    #[must_use]
    pub const fn new() -> Self {
        Self { held: Vec::new() }
    }

    /// Whether the chunk at `coord` is held.
    #[must_use]
    pub fn holds(&self, coord: ChunkCoord) -> bool {
        self.held.binary_search_by_key(&coord, Chunk::coord).is_ok()
    }

    /// Hold `chunk` in coordinate order; a chunk already held is kept as it
    /// is.
    ///
    /// # Errors
    ///
    /// [`ClientError::OutOfMemory`] when there is no room to hold it, which
    /// leaves the ground drawn as missing until there is.
    pub fn adopt(&mut self, chunk: Chunk) -> Result<(), ClientError> {
        let Err(at) = self.held.binary_search_by_key(&chunk.coord(), Chunk::coord) else {
            return Ok(());
        };
        self.held
            .try_reserve(1)
            .map_err(|_| ClientError::OutOfMemory)?;
        self.held.insert(at, chunk);
        Ok(())
    }

    /// Give back every chunk a view covering `visible` no longer needs.
    pub fn release_distant(&mut self, visible: Bounds) {
        self.held
            .retain(|chunk| worth_holding(chunk.coord(), visible));
    }

    /// The held chunks, borrowed in the coordinate order a [`ChunkWindow`]
    /// binary-searches.
    ///
    /// # Errors
    ///
    /// [`ClientError::OutOfMemory`] when the borrow list does not fit.
    pub fn borrow(&self) -> Result<Vec<&Chunk>, ClientError> {
        let mut borrowed = Vec::new();
        borrowed
            .try_reserve(self.held.len())
            .map_err(|_| ClientError::OutOfMemory)?;
        borrowed.extend(self.held.iter());
        Ok(borrowed)
    }
}

/// The chunk eastings and northings a view covering `visible` needs.
fn chunk_span(visible: Bounds) -> (RangeInclusive<i32>, RangeInclusive<i32>) {
    // One cell past each edge, because the lattice the pass interpolates
    // over reaches a sample beyond the last pixel.
    (
        chunk_of(lattice_of(visible.min_x) - 1)..=chunk_of(lattice_of(visible.max_x) + 2),
        chunk_of(lattice_of(visible.min_y) - 1)..=chunk_of(lattice_of(visible.max_y) + 2),
    )
}

/// The chunk a cell belongs to.
fn chunk_of(cell: i32) -> i32 {
    cell >> CHUNK_CELLS_LOG2
}

/// The mip each material is drawn at for a given pixel span.
///
/// A function of the zoom alone, so the ask phase and the paint phase
/// cannot disagree about which tile they meant.
#[must_use]
pub fn mip_for(material: Material, sub_units_per_pixel: i32) -> Mip {
    let density = u32::try_from(sub_units_per_pixel).unwrap_or(1).max(1);
    Mip::for_density(material::params(material).grain_shift, density)
}

/// Make every material the grid needs resident at `quality`.
///
/// Returns how many the cache would not admit — never an error: a tile
/// that is not held is drawn from its flat tone, so the pass is total.
pub fn ensure_tiles(
    cache: &mut MaterialCache,
    grid: &TerrainGrid,
    quality: Quality,
    sub_units_per_pixel: i32,
) -> usize {
    let mut refused = 0;
    for material in grid.materials() {
        let key = TileKey {
            material,
            mip: mip_for(material, sub_units_per_pixel),
        };
        if !cache.ensure(quality, key) {
            refused += 1;
        }
    }
    refused
}

/// Everything a row of the ground pass needs that does not change within
/// a frame.
#[derive(Copy, Clone, Debug)]
pub struct Pass<'a> {
    /// The warp that breaks the materials' repetition.
    pub warp: &'a Warp,
    /// The tiles the ask phase made resident.
    pub cache: &'a MaterialCache,
    /// The octave count those tiles were synthesised at.
    pub quality: Quality,
    /// World sub-units per pixel.
    pub step: i32,
    /// The world position of the render target's top-left pixel.
    pub origin: WorldPoint,
}

/// Draw one raster row of ground into `dst`.
///
/// `row` is the row's index in the render target, so the caller hands
/// this a band's rows and nothing else has to know where the band sits.
pub fn paint_row(dst: &mut [Pixel], grid: &TerrainGrid, pass: &Pass<'_>, row: u32) {
    let world_y = pass
        .origin
        .y
        .saturating_add(pass.step.saturating_mul(row_offset(row)));
    let lattice_row = lattice_of(world_y) - grid.origin.y;
    let t = lattice_fraction(world_y);
    let Ok(lattice_row) = usize::try_from(lattice_row) else {
        dst.fill(UNMAPPED.premultiply());
        return;
    };

    let mut pixel = 0usize;
    while pixel < dst.len() {
        let world_x = pass.origin.x.saturating_add(
            pass.step
                .saturating_mul(row_offset(u32::try_from(pixel).unwrap_or(u32::MAX))),
        );
        let lattice_col = lattice_of(world_x) - grid.origin.x;
        // The run ends where the next lattice sample begins, so a span is
        // always between one pair of columns and the splat's linear step
        // is exact.
        let next_sample = sample_world(lattice_of(world_x) + 1);
        let remaining = (i64::from(next_sample) - i64::from(world_x)).max(1);
        let steps = u64::try_from(remaining)
            .unwrap_or(1)
            .div_ceil(u64::try_from(pass.step).unwrap_or(1));
        let run = usize::try_from(steps)
            .unwrap_or(usize::MAX)
            .max(1)
            .min(dst.len() - pixel);
        let end = pixel + run;

        let plan = usize::try_from(lattice_col)
            .ok()
            .and_then(|col| span_plan(grid, col, lattice_row, t));
        match plan {
            Some(plan) => {
                let tiles = resolve(&plan, pass);
                let geometry = Geometry::new(
                    pass.warp,
                    WorldPoint {
                        x: world_x,
                        y: world_y,
                    },
                    pass.step,
                    u32::try_from(run).unwrap_or(u32::MAX),
                );
                splat(&mut dst[pixel..end], &plan, &tiles, &geometry);
            }
            None => dst[pixel..end].fill(UNMAPPED.premultiply()),
        }
        pixel = end;
    }
}

/// The plan for the run between lattice columns `col` and `col + 1`.
fn span_plan(grid: &TerrainGrid, col: usize, row: usize, t: u8) -> Option<SpanPlan> {
    let left = grid.column(col, row, t)?;
    let right = grid.column(col + 1, row, t)?;
    Some(SpanPlan::new(&left, &right))
}

/// The resident tile for each of a plan's materials, in slot order.
fn resolve<'a>(plan: &SpanPlan, pass: &Pass<'a>) -> SpanTiles<'a> {
    let mut tiles: SpanTiles<'a> = [None; BLEND_SLOTS];
    for (slot, material) in tiles.iter_mut().zip(plan.materials()) {
        *slot = pass.cache.peek(
            pass.quality,
            &TileKey {
                material,
                mip: mip_for(material, pass.step),
            },
        );
    }
    tiles
}

/// How many lattice samples span `first..=last` inclusive, plus the one
/// past the end that the last run interpolates toward.
fn span_count(first: i32, last: i32) -> usize {
    let count = i64::from(last) - i64::from(first) + 2;
    usize::try_from(count.max(2)).unwrap_or(usize::MAX)
}

/// A pixel or row index as the signed offset the projection multiplies.
fn row_offset(index: u32) -> i32 {
    i32::try_from(index).unwrap_or(i32::MAX)
}

/// A lattice index as the signed cell offset it is.
fn signed(index: usize) -> i32 {
    i32::try_from(index).unwrap_or(i32::MAX)
}

/// Grow or shrink `buffer` to `len`, refusing rather than panicking when
/// the allocation does not fit.
fn resize<T: Clone>(buffer: &mut Vec<T>, len: usize, value: T) -> Result<(), ClientError> {
    buffer.clear();
    buffer
        .try_reserve(len)
        .map_err(|_| ClientError::OutOfMemory)?;
    buffer.resize(len, value);
    Ok(())
}

#[cfg(test)]
#[path = "terrain_tests.rs"]
mod tests;
