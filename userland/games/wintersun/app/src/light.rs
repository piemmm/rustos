//! The low sun, and what it does to the ground.
//!
//! `WinterSun` is lit by a single directional light at a shallow angle.
//! That is an art direction and a simplification at once: long shadows, a
//! cold-to-warm gradient across every slope, and no light to trace — the
//! shading is the terrain's own gradient dotted with one vector.
//!
//! # Why a buffer rather than a term per pixel
//!
//! The result is low-frequency: it is a function of the ground's slope
//! and height, both of which vary over cells rather than pixels. So it is
//! accumulated into a buffer at a fraction of the render resolution and
//! upsampled, which is what makes it affordable and what gives the
//! degradation ladder a knob to turn that costs almost no fidelity for
//! most of its travel. The point lights a later item adds — lanterns,
//! fires, spell effects — accumulate into this same buffer rather than a
//! second one.
//!
//! The upsample has the same shape as the terrain pass: the two buffer
//! rows either side of an output row are interpolated once into a
//! scratch row, and the pixels step along it.

use alloc::vec::Vec;

use tairix_parallel::{bands, for_each, JobRunner};
use tairix_raster::color::{Color, Pixel};
use tairix_wintersun_art::palette;
use tairix_wintersun_net::value::WorldPoint;
use tairix_wintersun_rules::terrain::MAX_STEP_RISE_SUB_UNITS;
use tairix_wintersun_world::geom::ELEVATION_SUB_UNITS;

use crate::error::ClientError;
use crate::quality::Shadow;
use crate::terrain::TerrainGrid;
use crate::view::Viewport;

/// The gradient at which relief shading saturates, in elevation
/// sub-units per cell.
///
/// The greatest rise a body may step up, which is the line the terrain
/// itself draws between a slope and a cliff face. Shading that saturates
/// exactly there means the ground a player can walk over is shaded
/// across its whole range, and everything steeper reads as the wall it
/// is.
const SLOPE_FULL: i32 = MAX_STEP_RISE_SUB_UNITS;

/// Elevation above which no ground mist collects, in elevation sub-units.
///
/// Mist pools in hollows, so the term is driven by how far *below* this a
/// cell sits. Sixty world units is the valley floor of the realm's own
/// relief rather than a number picked to look right at one seed.
const MIST_CEILING: i32 = 60 * ELEVATION_SUB_UNITS;

/// A direction and two colours: everything the ground is lit by.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct Sun {
    /// Eastward component of the in-plane direction, as a 1/256 fraction.
    dx: i32,
    /// Southward component, likewise.
    dy: i32,
    /// The colour a slope facing the sun takes.
    pub warm: Color,
    /// The colour a slope facing away takes.
    pub cool: Color,
    /// How far the slope moves the mix, out of 255.
    pub relief: u8,
    /// The tint a slope facing neither way takes, which every gain is
    /// measured against.
    neutral: Color,
}

/// The fixed-point scale the sun's direction is held at.
const UNIT: i32 = 256;

impl Sun {
    /// A sun shining toward `(dx, dy)`, normalised.
    ///
    /// A zero direction is a sun directly overhead, which shades nothing
    /// — the honest answer rather than a refusal, since the rest of the
    /// frame is unaffected by it.
    #[must_use]
    pub fn new(dx: i32, dy: i32, warm: Color, cool: Color, relief: u8) -> Self {
        let length = i64::from(dx)
            .saturating_mul(i64::from(dx))
            .saturating_add(i64::from(dy).saturating_mul(i64::from(dy)))
            .isqrt();
        let scale = |v: i32| {
            if length == 0 {
                0
            } else {
                i32::try_from(i64::from(v) * i64::from(UNIT) / length).unwrap_or(0)
            }
        };
        Self {
            dx: scale(dx),
            dy: scale(dy),
            warm,
            cool,
            relief,
            neutral: palette::lerp(cool, warm, UNSHADED),
        }
    }

    /// The gain a slope at `level` applies to each channel.
    #[must_use]
    pub fn gain(&self, level: u8) -> Color {
        let tint = palette::lerp(self.cool, self.warm, level);
        let about = |lit: u8, neutral: u8| {
            let scaled = u32::from(lit) * u32::from(UNSHADED) / u32::from(neutral).max(1);
            u8::try_from(scaled.min(u32::from(u8::MAX))).unwrap_or(u8::MAX)
        };
        Color::rgb(
            about(tint.r, self.neutral.r),
            about(tint.g, self.neutral.g),
            about(tint.b, self.neutral.b),
        )
    }

    /// `WinterSun`'s own light: a low sun from the north-west, warm on
    /// the faces it reaches and cold on the ones it does not.
    #[must_use]
    pub fn winter() -> Self {
        Self::new(
            3,
            2,
            Color::rgb(255, 228, 186),
            Color::rgb(118, 136, 172),
            200,
        )
    }

    /// Where a slope of `(gx, gy)` elevation sub-units per cell sits
    /// between the cold and warm ends, out of 255.
    #[must_use]
    pub fn level(&self, gx: i32, gy: i32) -> u8 {
        let dot = i64::from(gx) * i64::from(self.dx) + i64::from(gy) * i64::from(self.dy);
        let full = i64::from(UNIT) * i64::from(SLOPE_FULL);
        let swing = dot.clamp(-full, full) * i64::from(self.relief) / (2 * full);
        u8::try_from((128 + swing).clamp(0, 255)).unwrap_or(128)
    }
}

/// The sky the ground sits under.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct Sky {
    /// What ground mist is coloured.
    pub mist: Color,
    /// How opaque the mist becomes in the deepest hollow, out of 255.
    pub mist_depth: u8,
}

impl Sky {
    /// `WinterSun`'s own: a pale cold haze in the valleys.
    #[must_use]
    pub const fn winter() -> Self {
        Self {
            mist: Color::rgb(150, 164, 186),
            mist_depth: 96,
        }
    }
}

/// The gain that leaves a channel exactly as the palette authored it.
///
/// Shading is *relative*: a slope facing neither toward nor away from
/// the sun draws the material's own colour, and the sun brightens or
/// cools it from there. A tint applied as a plain multiply would instead
/// darken every surface in the world by whatever its mid-point happened
/// to be, which is a palette change disguised as lighting.
pub const UNSHADED: u8 = 128;

/// What one buffer texel says about its patch of ground.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct Lit {
    /// Per-channel gain about [`UNSHADED`].
    pub gain: Color,
    /// How far toward the sky's mist it is mixed, out of 255.
    pub mist: u8,
}

impl Lit {
    /// Ground the light leaves alone, which is what an unshaded buffer
    /// holds.
    pub const NEUTRAL: Self = Self {
        gain: Color::rgb(UNSHADED, UNSHADED, UNSHADED),
        mist: 0,
    };
}

/// The light term for a frame, at a fraction of its resolution.
#[derive(Debug, Default)]
pub struct LightBuffer {
    width: u32,
    height: u32,
    shift: u32,
    texels: Vec<Lit>,
}

impl LightBuffer {
    /// An empty buffer, sized on its first shade.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The buffer's own extent, in texels.
    #[must_use]
    pub const fn extent(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    /// Log2 of the render pixels one texel covers.
    #[must_use]
    pub const fn shift(&self) -> u32 {
        self.shift
    }

    /// Shade every texel covering `view` from the ground `grid` holds.
    ///
    /// Texel rows are independent, so they are handed to `runner` the
    /// same way the frame's pixel bands are. Measured, not assumed: with
    /// the composite distributed and this left on the calling thread the
    /// light pass ran half again over its budget, and the whole of that
    /// overrun was here.
    ///
    /// # Errors
    ///
    /// [`ClientError::OutOfMemory`] if the buffer does not fit. It is
    /// reused between frames, so this is reached only when the view or
    /// the resolution grows.
    pub fn shade(
        &mut self,
        view: &Viewport,
        grid: &TerrainGrid,
        pass: &Shading,
        runner: &dyn JobRunner,
    ) -> Result<(), ClientError> {
        self.resize(view, pass.shift)?;
        let width = self.width as usize;
        let rows = self.height as usize;
        let grain = (MIN_SHADE_TEXELS / width.max(1)).max(1);
        let pieces = bands(runner, rows, grain).max(1);
        let per = rows.div_ceil(pieces).max(1);

        let mut work: Vec<(u32, &mut [Lit])> = Vec::new();
        work.try_reserve(rows.div_ceil(per))
            .map_err(|_| ClientError::OutOfMemory)?;
        for (index, chunk) in self.texels.chunks_mut(per * width).enumerate() {
            let first = u32::try_from(index * per).unwrap_or(u32::MAX);
            work.push((first, chunk));
        }
        let width32 = self.width;
        for_each(runner, &mut work, &|(first, texels)| {
            shade_band(texels, *first, width32, grid, pass);
        });
        Ok(())
    }

    /// Size the buffer for `view` at `shift`, leaving every texel
    /// neutral.
    ///
    /// # Errors
    ///
    /// [`ClientError::OutOfMemory`] if the buffer does not fit. It is
    /// reused between frames, so this is reached only when the view or
    /// the resolution grows.
    pub fn resize(&mut self, view: &Viewport, shift: u32) -> Result<(), ClientError> {
        let (rw, rh) = view.render();
        // One texel past each edge, so the last output pixel has a pair
        // to interpolate between rather than a clamped repeat.
        self.width = (rw >> shift) + 2;
        self.height = (rh >> shift) + 2;
        self.shift = shift;
        let texels = (self.width as usize)
            .checked_mul(self.height as usize)
            .ok_or(ClientError::OutOfMemory)?;
        fit(&mut self.texels, texels)
    }

    /// How wide a scratch row [`Self::composite_row`] needs.
    ///
    /// A band composites on its own thread, so each holds its own rather
    /// than sharing one and serialising on it.
    #[must_use]
    pub const fn scratch_len(&self) -> usize {
        self.width as usize
    }

    /// Interpolate the two texel rows either side of render row `row`
    /// into `scratch`, ready for [`Self::composite_row`].
    fn prepare(&self, scratch: &mut [Lit], row: u32) {
        let step = pixel_span(self.shift);
        let near = (row / step).min(self.height.saturating_sub(2));
        let t = fraction(row % step, self.shift);
        let width = self.width as usize;
        let near_base = (near as usize) * width;
        for (x, slot) in scratch.iter_mut().enumerate().take(width) {
            let (a, b) = (
                self.texels[near_base + x],
                self.texels[near_base + width + x],
            );
            *slot = Lit {
                gain: palette::lerp(a.gain, b.gain, t),
                mist: lerp_u8(a.mist, b.mist, t),
            };
        }
    }

    /// Apply the light to one row of already-painted ground.
    pub fn composite_row(&self, dst: &mut [Pixel], scratch: &mut [Lit], sky: Sky, row: u32) {
        if self.texels.is_empty() || scratch.len() < self.scratch_len() {
            return;
        }
        self.prepare(scratch, row);
        let step = pixel_span(self.shift) as usize;
        let last = self.scratch_len() - 1;
        // Walked in texel-wide runs rather than by pixel index: a run
        // boundary is exactly where the index division and modulo used to
        // fall, so neither survives into a loop the whole frame goes
        // through, and the two texels a run interpolates are read once.
        for (near, run) in dst.chunks_mut(step).enumerate() {
            let near = near.min(last);
            let far = (near + 1).min(last);
            let (near, far) = (scratch[near], scratch[far]);
            for (within, pixel) in run.iter_mut().enumerate() {
                let t = fraction(u32::try_from(within).unwrap_or(u32::MAX), self.shift);
                let lit = Lit {
                    gain: palette::lerp(near.gain, far.gain, t),
                    mist: lerp_u8(near.mist, far.mist, t),
                };
                *pixel = apply(*pixel, lit, sky);
            }
        }
    }
}

/// The fewest texels worth handing to another core.
///
/// A work grain, not a capacity: below roughly this much the dispatch
/// costs more than the shading it is splitting.
const MIN_SHADE_TEXELS: usize = 1 << 12;

/// Shade one run of whole texel rows, starting at row `first`.
fn shade_band(texels: &mut [Lit], first: u32, width: u32, grid: &TerrainGrid, pass: &Shading) {
    let texel_span = pass.step.saturating_mul(axis(pixel_span(pass.shift)));
    let stride = width as usize;
    for (index, texel) in texels.iter_mut().enumerate() {
        let tx = u32::try_from(index % stride.max(1)).unwrap_or(0);
        let ty = first.saturating_add(u32::try_from(index / stride.max(1)).unwrap_or(0));
        let at = WorldPoint {
            x: pass
                .origin
                .x
                .saturating_add(texel_span.saturating_mul(axis(tx))),
            y: pass
                .origin
                .y
                .saturating_add(texel_span.saturating_mul(axis(ty))),
        };
        *texel = shade_at(grid, pass, at);
    }
}

/// What a shading pass needs to know about the frame it is shading.
#[derive(Copy, Clone, Debug)]
pub struct Shading {
    /// The light.
    pub sun: Sun,
    /// Log2 of the render pixels one buffer texel covers.
    pub shift: u32,
    /// How the relief term is measured.
    pub shadow: Shadow,
    /// World sub-units per render pixel.
    pub step: i32,
    /// The world position of the render target's top-left pixel.
    pub origin: WorldPoint,
}

/// The light at one world position.
fn shade_at(grid: &TerrainGrid, pass: &Shading, at: WorldPoint) -> Lit {
    let Some((gx, gy, ground)) = gradient(grid, pass.shadow, at) else {
        return Lit::NEUTRAL;
    };
    let level = pass.sun.level(gx, gy);
    let below = (MIST_CEILING - i32::from(ground)).clamp(0, MIST_CEILING);
    let mist = u8::try_from(i64::from(below) * 255 / i64::from(MIST_CEILING)).unwrap_or(255);
    Lit {
        gain: pass.sun.gain(level),
        mist,
    }
}

/// The ground gradient around `at`, in elevation sub-units per cell, and
/// the height there.
///
/// `None` where the ground is not resident: unmapped ground is drawn as
/// what it is and must not be lit as though it were a plain.
fn gradient(grid: &TerrainGrid, shadow: Shadow, at: WorldPoint) -> Option<(i32, i32, i16)> {
    let (col, row) = grid.lattice_at(at)?;
    let here = grid.ground(col, row)?;
    if shadow == Shadow::Off {
        return Some((0, 0, here));
    }
    // A soft term measures across two cells and a hard one across one:
    // the wider stencil is the penumbra, and it is also the more
    // expensive, which is why the ladder narrows it before it gives up
    // the term entirely.
    let reach = if shadow == Shadow::Soft { 2 } else { 1 };
    let east = grid.ground(col + reach, row).unwrap_or(here);
    let south = grid.ground(col, row + reach).unwrap_or(here);
    let scale = i32::from(here);
    Some((
        (i32::from(east) - scale) / reach_divisor(reach),
        (i32::from(south) - scale) / reach_divisor(reach),
        here,
    ))
}

/// Cells the stencil spans, so a wider one is not read as a steeper
/// slope.
fn reach_divisor(reach: usize) -> i32 {
    i32::try_from(reach).unwrap_or(1).max(1)
}

/// Apply the ground's gain, then mix it toward the mist.
///
/// Terrain pixels are opaque, so the premultiplied channels are the
/// straight ones and neither step needs to divide by alpha.
fn apply(pixel: Pixel, lit: Lit, sky: Sky) -> Pixel {
    let modulate = |channel: u8, gain: u8| {
        let scaled = u32::from(channel) * u32::from(gain) / u32::from(UNSHADED);
        u8::try_from(scaled.min(u32::from(u8::MAX))).unwrap_or(u8::MAX)
    };
    let lit_color = Color::rgb(
        modulate(pixel.r, lit.gain.r),
        modulate(pixel.g, lit.gain.g),
        modulate(pixel.b, lit.gain.b),
    );
    let misted = palette::lerp(lit_color, sky.mist, mist_of(lit, sky));
    Pixel {
        r: misted.r,
        g: misted.g,
        b: misted.b,
        a: pixel.a,
    }
}

/// How far toward the mist a texel's ground is mixed.
fn mist_of(lit: Lit, sky: Sky) -> u8 {
    u8::try_from(u32::from(lit.mist) * u32::from(sky.mist_depth) / 255).unwrap_or(0)
}

/// The render pixels one buffer texel covers.
fn pixel_span(shift: u32) -> u32 {
    1u32 << shift.min(16)
}

/// Where within a texel a pixel sits, out of 255, for a texel span of
/// `1 << shift` ([`pixel_span`]).
///
/// Scaled by a shift rather than a division because the span is a power
/// of two by construction and this runs once per pixel of the frame — a
/// divisor the compiler cannot see is constant costs more here than the
/// interpolation it feeds.
fn fraction(within: u32, shift: u32) -> u8 {
    u8::try_from(within.saturating_mul(255) >> shift.min(16)).unwrap_or(u8::MAX)
}

/// A texel or pixel index as the signed offset the projection multiplies.
fn axis(index: u32) -> i32 {
    i32::try_from(index).unwrap_or(i32::MAX)
}

/// Interpolate two bytes.
fn lerp_u8(a: u8, b: u8, t: u8) -> u8 {
    let span = i32::from(b) - i32::from(a);
    let moved = i32::from(a) + span * i32::from(t) / 255;
    u8::try_from(moved.clamp(0, 255)).unwrap_or(a)
}

/// Grow `buffer` to `len` neutral texels, refusing rather than panicking
/// when the allocation does not fit.
fn fit(buffer: &mut Vec<Lit>, len: usize) -> Result<(), ClientError> {
    buffer.clear();
    buffer
        .try_reserve(len)
        .map_err(|_| ClientError::OutOfMemory)?;
    buffer.resize(len, Lit::NEUTRAL);
    Ok(())
}

#[cfg(test)]
#[path = "light_tests.rs"]
mod tests;
