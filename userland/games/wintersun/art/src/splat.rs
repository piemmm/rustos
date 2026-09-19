//! The splat: turning a cell's weight field into pixels.
//!
//! Terrain has no tile grid. A pixel is the blend of the materials its
//! cell holds — but blended by **height**, not by weight alone. Each
//! material carries its own surface relief, and where one stands proud of
//! the others it wins the pixel outright. That is the difference between
//! gravel emerging through grass in patches and the two averaging into a
//! grey that is neither.
//!
//! # A span, not a pixel
//!
//! The unit is a horizontal run of pixels inside one cell row, because
//! everything a pixel needs that is not its own texel read is linear
//! along that run: the weights interpolate between the run's two ends, and
//! so does the anti-repetition warp. Planning a run once and stepping it
//! turns four hash evaluations per pixel into four per *span*, which is
//! the difference between this pass fitting its budget and not.
//!
//! A caller therefore does the vertical interpolation itself — one
//! [`WeightField::lerp`] per cell row per raster row — and hands this
//! module the two ends of each horizontal run.
//!
//! # Repetition, and the one field that breaks it
//!
//! A synthesised tile is finite, so a lookup that was an affine function
//! of world position would show the tile's period across a large
//! grassland. The lookup is therefore displaced by a smooth, low-frequency
//! vector field ([`Warp`]) before it is taken.
//!
//! One warp field is all three of the jitters this needs. Its Jacobian
//! carries a local rotation, a local scale and a local offset together, so
//! there is no separate rotation to seam at a lattice boundary and no
//! separate scale to fight it: the field is continuous, therefore the
//! distortion is, therefore there is no edge anywhere for the eye to find.

use tairix_raster::color::Pixel;
use tairix_wintersun_net::value::WorldPoint;
use tairix_wintersun_world::biome::{Material, BLEND_SLOTS};

use crate::material::{self, MaterialTile, Texel};
use crate::noise::{self, Field, Tiled};
use crate::weight::{WeightField, TOTAL};

/// How much of a material's height counts toward winning a pixel,
/// as a right shift of the `0..=255` height.
///
/// A height contributes at most a quarter of the weight range, so a
/// material has to already hold a comparable share of the cell before its
/// relief can punch through. Without that bound a trace of sand would
/// stand proud across an entire grassland; with it, the height decides
/// where two materials genuinely meet and nothing else.
const HEIGHT_SHIFT: u32 = 2;

/// The band below the winning score within which materials still blend.
///
/// Zero would be a hard argmax — one material per pixel, and a boundary
/// that aliases into stair-steps. The whole weight range would be a plain
/// linear blend with the height doing nothing. This is the value between
/// them: sharp enough that gravel reads as gravel, soft enough that the
/// transition is anti-aliased by the blend itself.
const BLEND_DEPTH: u16 = 24;

/// Fixed-point fraction bits for the per-pixel accumulators.
const STEP_BITS: u32 = 16;

/// A smooth, low-frequency displacement of the material lookup.
///
/// Sampled in world sub-units, so two chunks agree across their seam
/// without either knowing about the other.
#[derive(Copy, Clone, Debug)]
pub struct Warp {
    field: Tiled,
    amplitude: i32,
    cell_log2: u32,
}

/// Log2 of the warp lattice's cell, in world sub-units.
///
/// Deliberately far coarser than any material tile: the warp has to vary
/// slowly enough that the distortion reads as terrain rather than as a
/// ripple, while still being incommensurate with every tile's period.
pub const WARP_CELL_LOG2: u32 = 14;

/// Half-width of the warp's displacement, in world sub-units.
///
/// Comparable to a material tile's world extent, which is what it takes
/// to actually break the period rather than merely soften it.
pub const WARP_AMPLITUDE: i32 = 1 << 12;

impl Warp {
    /// The warp for a realm.
    ///
    /// Keyed on the realm seed, so two realms do not wear the same
    /// distortion, and every client of one realm draws the same ground.
    #[must_use]
    pub const fn new(realm_seed: u64) -> Self {
        Self {
            field: Tiled::unbounded(realm_seed),
            amplitude: WARP_AMPLITUDE,
            cell_log2: WARP_CELL_LOG2,
        }
    }

    /// The displacement at a world point, in sub-units.
    #[must_use]
    pub fn at(&self, point: WorldPoint) -> (i32, i32) {
        let x = self
            .field
            .value(Field::WarpX, point.x, point.y, self.cell_log2);
        let y = self
            .field
            .value(Field::WarpY, point.x, point.y, self.cell_log2);
        (
            noise::centred(x, self.amplitude),
            noise::centred(y, self.amplitude),
        )
    }
}

/// The materials a span draws, and their weights at each of its ends.
///
/// Built once per run and stepped, which is what keeps the per-pixel cost
/// to a texel read and a blend.
#[derive(Copy, Clone, Debug)]
pub struct SpanPlan {
    slots: [PlanSlot; BLEND_SLOTS],
    used: usize,
}

/// One material's part in a span.
#[derive(Copy, Clone, Debug)]
struct PlanSlot {
    material: Material,
    near: u16,
    far: u16,
}

impl SpanPlan {
    /// Plan a run between two weight fields.
    ///
    /// The union of the two fields is kept to the heaviest
    /// [`BLEND_SLOTS`] by their combined prominence, and each end is
    /// renormalised over that set — so the per-pixel interpolation between
    /// two normalised vectors is itself normalised, and the kernel needs
    /// no per-pixel renormalisation at all.
    #[must_use]
    pub fn new(left: &WeightField, right: &WeightField) -> Self {
        let mut entries = [(left.dominant(), 0u32, 0u32); BLEND_SLOTS * 2];
        let mut count = 0;
        for (field, end) in [(left, 0usize), (right, 1usize)] {
            for slot in field.slots() {
                let index = if let Some(held) =
                    entries[..count].iter().position(|e| e.0 == slot.material)
                {
                    held
                } else {
                    entries[count] = (slot.material, 0, 0);
                    count += 1;
                    count - 1
                };
                if end == 0 {
                    entries[index].1 = u32::from(slot.weight);
                } else {
                    entries[index].2 = u32::from(slot.weight);
                }
            }
        }
        entries[..count].sort_unstable_by(|a, b| {
            (b.1 + b.2)
                .cmp(&(a.1 + a.2))
                .then_with(|| a.0.id().cmp(&b.0.id()))
        });

        let kept = count.min(BLEND_SLOTS);
        let near_sum: u32 = entries[..kept].iter().map(|e| e.1).sum();
        let far_sum: u32 = entries[..kept].iter().map(|e| e.2).sum();
        let mut slots = [PlanSlot {
            material: entries[0].0,
            near: 0,
            far: 0,
        }; BLEND_SLOTS];
        for (slot, entry) in slots.iter_mut().zip(entries[..kept].iter()) {
            *slot = PlanSlot {
                material: entry.0,
                near: renormalise(entry.1, near_sum),
                far: renormalise(entry.2, far_sum),
            };
        }
        let mut plan = Self {
            slots,
            used: kept.max(1),
        };
        plan.settle();
        plan
    }

    /// The materials this span needs a tile for, heaviest first.
    pub fn materials(&self) -> impl Iterator<Item = Material> + '_ {
        self.slots[..self.used].iter().map(|s| s.material)
    }

    /// How many materials the span draws.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.used
    }

    /// Whether the span draws nothing, which a planned span never does.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.used == 0
    }

    /// Hand each end's rounding remainder to its own heaviest slot, so
    /// both ends sum to exactly [`TOTAL`].
    ///
    /// Per end, not per plan: the slots are ordered by *combined*
    /// prominence, so slot zero can hold nothing at one end — and giving
    /// it the remainder there would put a material into an end whose
    /// field never had it.
    fn settle(&mut self) {
        let slots = &mut self.slots[..self.used];
        let near: u16 = slots.iter().map(|s| s.near).sum();
        if let Some(slot) = slots.iter_mut().find(|s| s.near > 0) {
            slot.near += TOTAL - near.min(TOTAL);
        }
        let far: u16 = slots.iter().map(|s| s.far).sum();
        if let Some(slot) = slots.iter_mut().find(|s| s.far > 0) {
            slot.far += TOTAL - far.min(TOTAL);
        }
    }
}

/// `value` as a share of [`TOTAL`], flooring; zero when there is nothing
/// to share.
fn renormalise(value: u32, sum: u32) -> u16 {
    if sum == 0 {
        return 0;
    }
    #[allow(
        clippy::cast_possible_truncation,
        reason = "a share of TOTAL is below it, and TOTAL is a u16"
    )]
    {
        (value * u32::from(TOTAL) / sum) as u16
    }
}

/// Where a span sits in the world, and how the warp moves across it.
#[derive(Copy, Clone, Debug)]
pub struct Geometry {
    /// World position of the span's first pixel.
    pub origin: WorldPoint,
    /// Eastward world sub-units per pixel.
    pub step: i32,
    /// The warp at the first pixel.
    pub warp_near: (i32, i32),
    /// The warp at the last pixel.
    ///
    /// Interpolated linearly across the span rather than evaluated per
    /// pixel: the field varies over thousands of sub-units and a span is
    /// a few hundred at most, so the line is within a sub-unit of the
    /// curve and costs two evaluations instead of one per pixel.
    pub warp_far: (i32, i32),
}

impl Geometry {
    /// A span starting at `origin`, `step` sub-units per pixel, warped by
    /// `warp` and `pixels` long.
    #[must_use]
    pub fn new(warp: &Warp, origin: WorldPoint, step: i32, pixels: u32) -> Self {
        let span = i32::try_from(pixels.saturating_sub(1)).unwrap_or(i32::MAX);
        let end = WorldPoint {
            x: origin.x.saturating_add(step.saturating_mul(span)),
            y: origin.y,
        };
        Self {
            origin,
            step,
            warp_near: warp.at(origin),
            warp_far: warp.at(end),
        }
    }
}

/// A material's tile for a span, or nothing if none is resident.
///
/// Positional, matching [`SpanPlan::materials`]. A slot whose tile is for
/// some other material is treated as absent rather than drawn, so a
/// caller that mismatched its lists gets a flat material and not another
/// material's pixels.
pub type SpanTiles<'a> = [Option<&'a MaterialTile>; BLEND_SLOTS];

/// Draw a horizontal run of terrain pixels.
///
/// Every slot resolves: a material with no resident tile is drawn from
/// its flat tone, so the pass is total and a frame is never dropped over
/// a texture the cache would not admit.
pub fn splat(dst: &mut [Pixel], plan: &SpanPlan, tiles: &SpanTiles<'_>, geometry: &Geometry) {
    let mut sources = [Source::Flat(Texel::VOID); BLEND_SLOTS];
    let mut shifts = [0u32; BLEND_SLOTS];
    for (index, slot) in plan.slots[..plan.used].iter().enumerate() {
        let params = material::params(slot.material);
        shifts[index] = params.grain_shift;
        sources[index] = match tiles[index] {
            Some(tile) if tile.material() == slot.material => Source::Tile(tile),
            _ => Source::Flat(params.flat()),
        };
    }

    let pixels = dst.len();
    let last = pixels.saturating_sub(1).max(1);
    let mut world_x = geometry.origin.x;
    let mut warp = Accumulator::new(geometry.warp_near, geometry.warp_far, last);
    let mut weights = [WeightStep::still(0); BLEND_SLOTS];
    for (step, slot) in weights.iter_mut().zip(plan.slots[..plan.used].iter()) {
        *step = WeightStep::new(slot.near, slot.far, last);
    }

    for pixel in dst.iter_mut() {
        let (warp_x, warp_y) = warp.value();
        let sample_x = world_x.wrapping_add(warp_x);
        let sample_y = geometry.origin.y.wrapping_add(warp_y);

        *pixel = shade(
            &sources[..plan.used],
            &shifts[..plan.used],
            &weights[..plan.used],
            sample_x,
            sample_y,
        );

        world_x = world_x.wrapping_add(geometry.step);
        warp.advance();
        for step in &mut weights[..plan.used] {
            step.advance();
        }
    }
}

/// Where a slot's texels come from.
#[derive(Copy, Clone, Debug)]
enum Source<'a> {
    /// A resident synthesised tile.
    Tile(&'a MaterialTile),
    /// The material's flat tone, the tier beneath a tile.
    Flat(Texel),
}

impl Source<'_> {
    /// The texel at a warped world position, given the material's grain
    /// scale.
    fn texel(&self, x: i32, y: i32, shift: u32) -> Texel {
        match self {
            Self::Tile(tile) =>
            {
                #[allow(
                    clippy::cast_sign_loss,
                    reason = "the tile's own mask makes the wrap two's-complement periodic, \
                              which is what a negative world coordinate needs"
                )]
                tile.texel((x >> shift) as u32, (y >> shift) as u32)
            }
            Self::Flat(texel) => *texel,
        }
    }
}

/// One pixel of the height-offset blend.
fn shade(
    sources: &[Source<'_>],
    shifts: &[u32],
    weights: &[WeightStep],
    sample_x: i32,
    sample_y: i32,
) -> Pixel {
    let mut texels = [Texel::VOID; BLEND_SLOTS];
    let mut scores = [0u16; BLEND_SLOTS];
    let mut top = 0u16;
    for (index, source) in sources.iter().enumerate() {
        let texel = source.texel(sample_x, sample_y, shifts[index]);
        let score = weights[index]
            .value()
            .saturating_add(u16::from(texel.height) >> HEIGHT_SHIFT);
        texels[index] = texel;
        scores[index] = score;
        top = top.max(score);
    }

    let floor = top.saturating_sub(BLEND_DEPTH);
    let (mut red, mut green, mut blue, mut total) = (0u32, 0u32, 0u32, 0u32);
    for (index, texel) in texels[..sources.len()].iter().enumerate() {
        let share = u32::from(scores[index].saturating_sub(floor));
        if share == 0 {
            continue;
        }
        red += u32::from(texel.r) * share;
        green += u32::from(texel.g) * share;
        blue += u32::from(texel.b) * share;
        total += share;
    }
    // The winning slot's share is `BLEND_DEPTH`, so the divisor is never
    // zero for a non-empty span.
    let total = total.max(1);
    #[allow(
        clippy::cast_possible_truncation,
        reason = "each channel is a weighted mean of u8 values"
    )]
    Pixel {
        r: (red / total) as u8,
        g: (green / total) as u8,
        b: (blue / total) as u8,
        // Ground is opaque, and a premultiply by a full alpha is the
        // identity, so the channels above are already premultiplied.
        a: u8::MAX,
    }
}

/// A two-axis fixed-point walk from one value to another over a span.
#[derive(Copy, Clone, Debug)]
struct Accumulator {
    x: i64,
    y: i64,
    dx: i64,
    dy: i64,
}

impl Accumulator {
    fn new(near: (i32, i32), far: (i32, i32), steps: usize) -> Self {
        let steps = i64::try_from(steps).unwrap_or(1).max(1);
        Self {
            x: i64::from(near.0) << STEP_BITS,
            y: i64::from(near.1) << STEP_BITS,
            dx: ((i64::from(far.0) - i64::from(near.0)) << STEP_BITS) / steps,
            dy: ((i64::from(far.1) - i64::from(near.1)) << STEP_BITS) / steps,
        }
    }

    fn value(&self) -> (i32, i32) {
        #[allow(
            clippy::cast_possible_truncation,
            reason = "the walk stays between its two i32 endpoints"
        )]
        {
            ((self.x >> STEP_BITS) as i32, (self.y >> STEP_BITS) as i32)
        }
    }

    fn advance(&mut self) {
        self.x += self.dx;
        self.y += self.dy;
    }
}

/// A fixed-point walk of one material's weight across a span.
#[derive(Copy, Clone, Debug)]
struct WeightStep {
    value: i64,
    delta: i64,
}

impl WeightStep {
    fn new(near: u16, far: u16, steps: usize) -> Self {
        let steps = i64::try_from(steps).unwrap_or(1).max(1);
        Self {
            value: i64::from(near) << STEP_BITS,
            delta: ((i64::from(far) - i64::from(near)) << STEP_BITS) / steps,
        }
    }

    const fn still(weight: u16) -> Self {
        Self {
            value: (weight as i64) << STEP_BITS,
            delta: 0,
        }
    }

    fn value(&self) -> u16 {
        #[allow(
            clippy::cast_sign_loss,
            clippy::cast_possible_truncation,
            reason = "the walk stays between two u16 endpoints"
        )]
        {
            (self.value >> STEP_BITS).clamp(0, i64::from(TOTAL)) as u16
        }
    }

    fn advance(&mut self) {
        self.value += self.delta;
    }
}

#[cfg(test)]
mod tests;
