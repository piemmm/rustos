//! Materials, synthesised rather than shipped.
//!
//! A material is a handful of numbers — a palette ramp, a grain scale, a
//! relief depth, a roughness — from which its texture and its height field
//! are generated on the machine that draws them. Fifteen of those cost a
//! few hundred bytes in the binary where fifteen photographic tile sets
//! would cost megabytes on disk, and being generated they are
//! resolution-independent: a mip is not a downsample of a fixed master,
//! it is the same field evaluated at the scale it will be drawn at.
//!
//! # A texel is a colour and a height
//!
//! Four bytes: red, green, blue, and the height the splat resolves
//! materials by ([`Texel`]). The height is what makes gravel emerge
//! through grass in patches rather than the two averaging into a grey —
//! it is the material's own surface relief, so where the gravel stands
//! proud it wins the pixel outright.
//!
//! # Tiles must tile
//!
//! A tile is drawn end to end across a hillside, so every lattice the
//! synthesis reads wraps at the tile's own side ([`noise::Tiled`]). A
//! sampler that did not would put a seam on every repeat, and no amount
//! of anti-repetition jitter in the lookup would hide a discontinuity in
//! the source.

use alloc::vec::Vec;

use tairix_wintersun_world::biome::Material;

use crate::error::ArtError;
use crate::noise::{self, Field, Tiled};
use crate::palette::{self, Ramp};

/// One synthesised sample: a colour and the surface height at it.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct Texel {
    /// Red channel.
    pub r: u8,
    /// Green channel.
    pub g: u8,
    /// Blue channel.
    pub b: u8,
    /// Surface height, which the splat resolves overlapping materials by.
    pub height: u8,
}

impl Texel {
    /// The texel a read outside a tile resolves to: black, and standing
    /// below every material, so it can never win a splat.
    pub const VOID: Self = Self {
        r: 0,
        g: 0,
        b: 0,
        height: 0,
    };
}

/// Side of a mip-0 tile, in texels.
///
/// A quality and format figure rather than a capacity: it fixes how much
/// detail a material carries, not how many of them a machine may hold —
/// that is the cache's budget, which is derived from the machine. Chosen
/// so a tile at mip 0 is 256 KiB, small enough that several are resident
/// on a modest machine and large enough that the warp has something to
/// work with.
pub const TILE_SIDE: u32 = 256;

/// The number of mip levels the chain holds, down to an 8-texel tile.
///
/// Below that a tile carries no detail worth an entry, and the splat is
/// better served by the flat mid tone than by a four-texel blur.
pub const MIP_LEVELS: u32 = 6;

/// Which level of a material's mip chain a tile is.
///
/// A level beyond the chain cannot be constructed, so nothing downstream
/// has to check one.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct Mip(u32);

impl Mip {
    /// The full-detail level.
    pub const BASE: Self = Self(0);

    /// Level `level`, or `None` beyond the chain.
    #[must_use]
    pub const fn new(level: u32) -> Option<Self> {
        if level < MIP_LEVELS {
            Some(Self(level))
        } else {
            None
        }
    }

    /// The coarsest level in the chain.
    #[must_use]
    pub const fn coarsest() -> Self {
        Self(MIP_LEVELS - 1)
    }

    /// The level, as a number.
    #[must_use]
    pub const fn level(self) -> u32 {
        self.0
    }

    /// The side of a tile at this level, in texels.
    #[must_use]
    pub const fn side(self) -> u32 {
        TILE_SIDE >> self.0
    }

    /// The level whose texels are closest to `sub_units_per_texel` world
    /// sub-units apart, given a material's own grain scale.
    ///
    /// Picking the level from the scale it will be drawn at is what keeps
    /// a distant hillside from shimmering: a tile whose texels are finer
    /// than the pixels sampling them aliases, however good the filtering.
    #[must_use]
    pub fn for_density(material_shift: u32, sub_units_per_pixel: u32) -> Self {
        let texel_span = 1u32.checked_shl(material_shift).unwrap_or(u32::MAX);
        let level = (sub_units_per_pixel / texel_span.max(1)).max(1).ilog2();
        Self(level.min(MIP_LEVELS - 1))
    }
}

/// How detailed a synthesis is allowed to be.
///
/// The one knob the frame's degradation ladder turns in this crate: when a
/// frame overruns, detail octaves are shed before anything structural
/// changes. It is also the material cache's generation token, because a
/// tile synthesised at one octave count is not the tile another count
/// would produce.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub struct Quality {
    octaves: u32,
}

/// The most octaves a synthesis will run, whatever it is asked for.
///
/// Beyond this the lattice cell is a single texel and further octaves are
/// white noise rather than detail.
pub const MAX_OCTAVES: u32 = 5;

impl Quality {
    /// Full detail.
    pub const FULL: Self = Self {
        octaves: MAX_OCTAVES,
    };

    /// A quality of `octaves` detail levels, capped at [`MAX_OCTAVES`].
    #[must_use]
    pub const fn new(octaves: u32) -> Self {
        Self {
            octaves: if octaves > MAX_OCTAVES {
                MAX_OCTAVES
            } else {
                octaves
            },
        }
    }

    /// One step less detailed, or `None` at the flattest.
    ///
    /// The ladder's step, so a caller sheds detail by asking rather than
    /// by choosing a number.
    #[must_use]
    pub const fn shed(self) -> Option<Self> {
        match self.octaves {
            0 => None,
            n => Some(Self { octaves: n - 1 }),
        }
    }

    /// The octave count.
    #[must_use]
    pub const fn octaves(self) -> u32 {
        self.octaves
    }
}

/// What a material is made of.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct MaterialParams {
    /// The three tones the grain is drawn between.
    pub ramp: Ramp,
    /// Log2 of the world sub-units one mip-0 texel spans.
    ///
    /// Sand is fine and rock is coarse, and this is the whole of that
    /// difference. It also decides which mip a given camera distance
    /// picks ([`Mip::for_density`]).
    pub grain_shift: u32,
    /// Log2 of the grain's lattice cell, in texels.
    pub grain_cell_log2: u32,
    /// How far the colour travels along the ramp, out of 255.
    pub roughness: u8,
    /// How far the height field travels, out of 255.
    ///
    /// A material with a deep relief punches through its neighbours in
    /// the splat; a flat one such as still water never does.
    pub relief: u8,
    /// The mid height the relief varies around, out of 255.
    ///
    /// This is the material's standing in the splat: shingle sits above
    /// mud, so a river bank grades rather than cuts.
    pub stand: u8,
}

impl MaterialParams {
    /// A parameter set from its fields in declaration order.
    ///
    /// Positional so the fifteen-row table below reads as a table, where
    /// a column can be compared down the set.
    const fn new(
        ramp: Ramp,
        grain_shift: u32,
        grain_cell_log2: u32,
        roughness: u8,
        relief: u8,
        stand: u8,
    ) -> Self {
        Self {
            ramp,
            grain_shift,
            grain_cell_log2,
            roughness,
            relief,
            stand,
        }
    }

    /// The material with no grain at all: its mid tone at its standing
    /// height.
    ///
    /// The tier beneath every synthesised tile, so material resolution is
    /// total — a tile the cache would not admit degrades to a flat but
    /// correctly coloured, correctly standing material rather than to a
    /// hole in the ground.
    #[must_use]
    pub const fn flat(&self) -> Texel {
        Texel {
            r: self.ramp.mid.r,
            g: self.ramp.mid.g,
            b: self.ramp.mid.b,
            height: self.stand,
        }
    }
}

/// The parameter set for every material.
///
/// Fifteen entries, in discriminant order, so a new material is a new row
/// here and nothing else.
#[must_use]
#[rustfmt::skip]
pub const fn params(material: Material) -> MaterialParams {
    //                                          ramp                      grain  cell  rough  relief  stand
    match material {
        Material::Water           => MaterialParams::new(palette::WATER,            7, 5,  40,  10,  20),
        Material::Glacier         => MaterialParams::new(palette::GLACIER,          7, 5,  70,  60, 190),
        Material::Snowfield       => MaterialParams::new(palette::SNOWFIELD,        6, 4,  50,  45, 170),
        Material::Tundra          => MaterialParams::new(palette::TUNDRA,           5, 3, 120,  70, 110),
        Material::FellHeath       => MaterialParams::new(palette::FELL_HEATH,       5, 3, 130,  80, 115),
        Material::ColdSteppe      => MaterialParams::new(palette::COLD_STEPPE,      5, 2, 140,  60, 100),
        Material::BorealForest    => MaterialParams::new(palette::BOREAL_FOREST,    6, 3, 160, 110, 150),
        Material::TemperateForest => MaterialParams::new(palette::TEMPERATE_FOREST, 6, 3, 150, 100, 145),
        Material::Moor            => MaterialParams::new(palette::MOOR,             5, 3, 110,  50,  75),
        Material::Saltmarsh       => MaterialParams::new(palette::SALTMARSH,        5, 2, 130,  55,  70),
        Material::Ashland         => MaterialParams::new(palette::ASHLAND,          4, 2, 100,  90, 105),
        Material::RiftWaste       => MaterialParams::new(palette::RIFT_WASTE,       6, 4, 170, 130, 125),
        Material::Rock            => MaterialParams::new(palette::ROCK,             7, 4, 120, 104, 200),
        Material::Gravel          => MaterialParams::new(palette::GRAVEL,           3, 1, 150, 150, 160),
        Material::Sand            => MaterialParams::new(palette::SAND,             4, 3,  90,  40,  60),
    }
}

/// A synthesised, tileable material texture at one mip level.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MaterialTile {
    material: Material,
    mip: Mip,
    texels: Vec<Texel>,
}

impl MaterialTile {
    /// Synthesise `material` at `mip` and `quality`.
    ///
    /// # Errors
    ///
    /// [`ArtError::OutOfMemory`] when the tile cannot be allocated.
    pub fn synthesise(material: Material, mip: Mip, quality: Quality) -> Result<Self, ArtError> {
        let params = params(material);
        let side = mip.side();
        let area = usize::try_from(side * side).map_err(|_| ArtError::OutOfMemory)?;
        let mut texels = Vec::new();
        texels.try_reserve_exact(area).map_err(|_| {
            // A tile is reclaimable by definition, so refusing one is a
            // frame drawn flatter, never a failure of the program.
            ArtError::OutOfMemory
        })?;

        // The lattice wraps at the tile's side in lattice cells, so the
        // tile is seamless at whatever mip it was generated for.
        let cell_log2 = params.grain_cell_log2.saturating_sub(mip.level()).max(1);
        let period_log2 = side.ilog2().saturating_sub(cell_log2).max(1);
        let key = material_key(material);
        // The period is derived from the tile's own side and cannot leave
        // range; an unwrapping sampler would still draw, with a seam.
        let grain = Tiled::new(key, period_log2).unwrap_or_else(|| Tiled::unbounded(key));

        for y in 0..side {
            for x in 0..side {
                texels.push(sample(&params, &grain, quality, cell_log2, x, y));
            }
        }
        Ok(Self {
            material,
            mip,
            texels,
        })
    }

    /// The material this tile is of.
    #[must_use]
    pub const fn material(&self) -> Material {
        self.material
    }

    /// The mip level it was synthesised at.
    #[must_use]
    pub const fn mip(&self) -> Mip {
        self.mip
    }

    /// Its side, in texels.
    #[must_use]
    pub const fn side(&self) -> u32 {
        self.mip.side()
    }

    /// The texel at `(x, y)`, wrapping at the tile's side.
    ///
    /// Wrapping rather than clamping, because the caller's coordinate is a
    /// warped world position with no relation to the tile's extent — a
    /// clamp would smear the tile's edge texel across a hillside.
    #[must_use]
    pub fn texel(&self, x: u32, y: u32) -> Texel {
        let mask = self.side() - 1;
        let index = (y & mask) * self.side() + (x & mask);
        usize::try_from(index)
            .ok()
            .and_then(|i| self.texels.get(i))
            .copied()
            .unwrap_or(Texel::VOID)
    }

    /// The texels, in row-major order.
    #[must_use]
    pub fn texels(&self) -> &[Texel] {
        &self.texels
    }

    /// Bytes the texels occupy, for the cache's ledger.
    #[must_use]
    pub fn payload_bytes(&self) -> usize {
        self.texels.len() * core::mem::size_of::<Texel>()
    }

    /// Overwrite the texels before the tile is dropped.
    ///
    /// Ground is public, so nothing here is secret. It is still
    /// overwritten rather than merely dropped, because "this one is fine
    /// to leave" is the habit that eventually leaks something that is not.
    pub fn scrub(&mut self) {
        self.texels.fill(Texel::VOID);
    }
}

/// The lattice key a material's fields are drawn from.
///
/// Derived from the frozen material id, so a material's appearance is the
/// same in every realm — the ground varies because the *blend* varies, not
/// because grass is a different grass per seed.
fn material_key(material: Material) -> u64 {
    0x5749_4E54_4552_0000 | u64::from(material.id())
}

/// One texel of a synthesis.
fn sample(
    params: &MaterialParams,
    grain: &Tiled,
    quality: Quality,
    cell_log2: u32,
    x: u32,
    y: u32,
) -> Texel {
    #[allow(
        clippy::cast_possible_wrap,
        reason = "a tile side is at most TILE_SIDE, far below i32::MAX"
    )]
    let (ix, iy) = (x as i32, y as i32);
    let octaves = quality.octaves();

    let tone = noise::to_byte(grain.fbm(Field::Grain, ix, iy, cell_log2, octaves));
    let color = params.ramp.sample(along(tone, params.roughness));

    let relief = noise::to_byte(grain.fbm(Field::Relief, ix, iy, cell_log2, octaves));
    Texel {
        r: color.r,
        g: color.g,
        b: color.b,
        height: around(params.stand, relief, params.relief),
    }
}

/// `tone` pulled toward the ramp's middle by however smooth the material
/// is, so roughness is how far the grain travels rather than a second
/// noise field.
fn along(tone: u8, roughness: u8) -> u8 {
    let centred = i32::from(tone) - 128;
    let scaled = centred * i32::from(roughness) / i32::from(u8::MAX);
    #[allow(
        clippy::cast_sign_loss,
        clippy::cast_possible_truncation,
        reason = "the sum is clamped into 0..=255 before the cast"
    )]
    {
        (128 + scaled).clamp(0, i32::from(u8::MAX)) as u8
    }
}

/// `stand` varied by `relief`/255 of `span`, centred, and clamped into the
/// byte the splat compares.
fn around(stand: u8, relief: u8, span: u8) -> u8 {
    let centred = i32::from(relief) - 128;
    let scaled = centred * i32::from(span) / i32::from(u8::MAX);
    #[allow(
        clippy::cast_sign_loss,
        clippy::cast_possible_truncation,
        reason = "the sum is clamped into 0..=255 before the cast"
    )]
    {
        (i32::from(stand) + scaled).clamp(0, i32::from(u8::MAX)) as u8
    }
}

#[cfg(test)]
mod tests;
