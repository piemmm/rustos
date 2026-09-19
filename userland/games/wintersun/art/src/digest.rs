//! The art digest, and the scripted scene the reproducibility claim is
//! staked on.
//!
//! # Why this crate's claim is a proof and a constant, not a QEMU run
//!
//! The world generator and the simulation each carry a four-target
//! vertical, because each does arithmetic whose identity across targets
//! is a property of the code rather than of the language: the one works
//! in `f64`, the other in integers with a single `f64` conversion, and
//! "should be identical" and "is identical" are different claims.
//!
//! Nothing here is floating point at all — a crate-level `deny` makes
//! that a compile error rather than a habit — and Rust's integer
//! arithmetic is exactly specified on every target. The only
//! target-dependent integer type is `usize`, and nothing below folds one.
//! Bit-identity therefore *follows*, and a run on four emulated machines
//! would confirm the language rather than the code.
//!
//! What a constant still buys is the other half of a digest's job: a
//! change to the art that was not meant to happen shows up as a moved
//! number. So [`REFERENCE_DIGEST`] is asserted by the host suite, and
//! the client vertical that hashes a composited frame
//! (`plans/WINTERSUN.md` WS5) folds it in — which is where the
//! cross-target rendering claim is made, over the whole picture rather
//! than over this crate alone.

use core::hash::Hasher;

use tairix_hash::FastHash;
use tairix_raster::color::Pixel;
use tairix_reclaim::PressureBand;
use tairix_wintersun_net::value::{WorldPoint, WorldVector};
use tairix_wintersun_world::biome::Material;

use crate::decal::{Decal, Fray};
use crate::error::ArtError;
use crate::material::{MaterialTile, Mip, Quality};
use crate::particle::{budget, ParticleField, ParticleKind, Spawn};
use crate::splat::{self, Geometry, SpanPlan, SpanTiles, Warp};
use crate::weight::WeightField;

/// The digest of the scripted scene, on every target.
///
/// Changing any parameter row, ramp, kernel or blend constant changes
/// this. That is the point: it is not a magic number to be re-derived
/// when a test fails, it is the record of what the ground looks like. A
/// change that moves it changes every frame anybody will ever see, and
/// the new value is written down deliberately rather than pasted out of
/// a failure.
pub const REFERENCE_DIGEST: u64 = 0x4937_3A84_3DB9_D95B;

/// The realm the scripted scene is drawn for.
pub const REFERENCE_SEED: u64 = 0x5749_4E54_4552_5355;

/// Mip the whole material set is synthesised at for the fold.
///
/// Coarse enough that fifteen of them are quick, fine enough that the
/// grain, the relief and the octave weighting all reach the texels.
const SWEEP_MIP: u32 = 3;

/// Mip the one full-detail tile is synthesised at.
const DETAIL_MIP: u32 = 1;

/// Pixels in each splat span of the fold.
const SPAN_PIXELS: usize = 48;

/// Ticks the particle run is advanced for.
const PARTICLE_TICKS: u16 = 24;

/// Particles the run emits.
const PARTICLE_COUNT: usize = 48;

/// Fold the scripted scene and return its digest.
///
/// # Errors
///
/// [`ArtError::OutOfMemory`] if a tile or the particle field cannot be
/// allocated.
pub fn reference() -> Result<u64, ArtError> {
    let mut hasher = FastHash::with_seed(REFERENCE_SEED);
    materials(&mut hasher)?;
    weights(&mut hasher);
    decals(&mut hasher);
    spans(&mut hasher)?;
    particles(&mut hasher);
    Ok(hasher.finish())
}

/// Every material's synthesis, plus one at full detail.
fn materials(hasher: &mut FastHash) -> Result<(), ArtError> {
    let sweep = Mip::new(SWEEP_MIP).ok_or(ArtError::NoSuchMip)?;
    let detail = Mip::new(DETAIL_MIP).ok_or(ArtError::NoSuchMip)?;
    for material in Material::ALL {
        fold_tile(
            hasher,
            &MaterialTile::synthesise(material, sweep, Quality::FULL)?,
        );
    }
    fold_tile(
        hasher,
        &MaterialTile::synthesise(Material::Gravel, detail, Quality::FULL)?,
    );
    // A shed octave must not leave the tile unchanged, so it is folded
    // too — otherwise the degradation knob could quietly do nothing.
    fold_tile(
        hasher,
        &MaterialTile::synthesise(Material::Rock, sweep, Quality::new(2))?,
    );
    Ok(())
}

/// The weight field's algebra: covering, displacement and interpolation.
fn weights(hasher: &mut FastHash) {
    let mut field = WeightField::solid(Material::ColdSteppe);
    for (step, material) in Material::ALL.iter().enumerate() {
        #[allow(
            clippy::cast_possible_truncation,
            reason = "fifteen materials, and the product is well inside a u16"
        )]
        let coverage = (37 + step * 19) as u16 % 250;
        field.cover(*material, coverage);
        fold_field(hasher, &field);
    }

    let other = WeightField::solid(Material::Glacier);
    for t in (0..=u8::MAX).step_by(17) {
        fold_field(hasher, &field.lerp(&other, t));
    }
}

/// A road and a river crossing, stamped over a grid.
fn decals(hasher: &mut FastHash) {
    let fray = Fray::new(REFERENCE_SEED);
    let road_path = [
        WorldPoint { x: -6000, y: -400 },
        WorldPoint { x: 0, y: 0 },
        WorldPoint { x: 6000, y: 900 },
    ];
    let river_path = [
        WorldPoint { x: -200, y: -6000 },
        WorldPoint { x: 300, y: 6000 },
    ];
    let road = Decal {
        material: Material::Gravel,
        path: &road_path,
        half_width: 140,
        feather: 110,
        coverage: 225,
    };
    let river = Decal {
        material: Material::Water,
        path: &river_path,
        half_width: 240,
        feather: 180,
        coverage: 250,
    };

    for y in (-1200..1200).step_by(97) {
        for x in (-1200..1200).step_by(89) {
            let at = WorldPoint { x, y };
            let mut field = WeightField::solid(Material::Moor);
            road.stamp(&mut field, &fray, at);
            river.stamp(&mut field, &fray, at);
            hasher.write_u16(road.coverage_at(&fray, at));
            hasher.write_u16(river.coverage_at(&fray, at));
            fold_field(hasher, &field);
        }
    }
}

/// The splat kernel over a run of spans walking one material into
/// another, with the warp live.
fn spans(hasher: &mut FastHash) -> Result<(), ArtError> {
    let mip = Mip::new(SWEEP_MIP).ok_or(ArtError::NoSuchMip)?;
    let warp = Warp::new(REFERENCE_SEED);
    let mut row = [Pixel::TRANSPARENT; SPAN_PIXELS];

    for (step, pair) in Material::ALL.windows(2).enumerate() {
        let mut left = WeightField::solid(pair[0]);
        left.cover(Material::Rock, 60);
        let right = WeightField::solid(pair[1]);
        let plan = SpanPlan::new(&left, &right);

        let mut held = [const { None }; 4];
        for (slot, material) in held.iter_mut().zip(plan.materials()) {
            *slot = Some(MaterialTile::synthesise(material, mip, Quality::FULL)?);
        }
        let mut tiles: SpanTiles<'_> = [None; 4];
        for (slot, tile) in tiles.iter_mut().zip(held.iter()) {
            *slot = tile.as_ref();
        }

        let ordinal = i32::try_from(step).unwrap_or(0);
        let origin = WorldPoint {
            x: ordinal * 3301 - 20_000,
            y: ordinal * 1777 + 5_000,
        };
        let pixels = u32::try_from(SPAN_PIXELS).unwrap_or(1);
        let geometry = Geometry::new(&warp, origin, 96, pixels);
        hasher.write_i32(geometry.warp_near.0);
        hasher.write_i32(geometry.warp_near.1);
        hasher.write_i32(geometry.warp_far.0);
        hasher.write_i32(geometry.warp_far.1);

        splat::splat(&mut row, &plan, &tiles, &geometry);
        for pixel in &row {
            hasher.write_u8(pixel.r);
            hasher.write_u8(pixel.g);
            hasher.write_u8(pixel.b);
            hasher.write_u8(pixel.a);
        }
    }
    Ok(())
}

/// A storm: every kind emitted, advanced under a turning wind.
fn particles(hasher: &mut FastHash) {
    let spawn = Spawn::new(REFERENCE_SEED);
    let room = budget(1 << 30, PressureBand::Normal);
    hasher.write_u32(u32::try_from(room).unwrap_or(u32::MAX));
    let Ok(mut field) = ParticleField::with_budget(room.min(128)) else {
        // A machine that cannot hold the run folds nothing for it rather
        // than reporting a digest it did not compute.
        hasher.write_u8(0);
        return;
    };

    for step in 0..PARTICLE_COUNT {
        let kind = ParticleKind::ALL[step % ParticleKind::ALL.len()];
        let ordinal = i32::try_from(step).unwrap_or(0);
        field.emit(
            &spawn,
            kind,
            WorldPoint {
                x: ordinal * 271 - 6000,
                y: ordinal * 133,
            },
            WorldVector {
                x: i16::try_from(ordinal * 7 - 160).unwrap_or(0),
                y: i16::try_from(ordinal * 3).unwrap_or(0),
            },
        );
    }

    for tick in 0..PARTICLE_TICKS {
        field.advance(WorldVector {
            x: i16::try_from(i32::from(tick) * 23 - 240).unwrap_or(0),
            y: i16::try_from(i32::from(tick) * 5).unwrap_or(0),
        });
        for particle in field.particles() {
            hasher.write_u8(particle.kind as u8);
            hasher.write_i32(particle.at.x);
            hasher.write_i32(particle.at.y);
            hasher.write_i16(particle.velocity.x);
            hasher.write_i16(particle.velocity.y);
            hasher.write_u16(particle.age);
            hasher.write_u8(particle.variation);
            let color = particle.color();
            hasher.write_u8(color.r);
            hasher.write_u8(color.g);
            hasher.write_u8(color.b);
            hasher.write_u8(color.a);
            hasher.write_u16(particle.radius());
        }
    }
}

/// Fold a tile: its identity and every texel.
fn fold_tile(hasher: &mut FastHash, tile: &MaterialTile) {
    hasher.write_u16(tile.material().id());
    hasher.write_u32(tile.mip().level());
    hasher.write_u32(tile.side());
    for texel in tile.texels() {
        hasher.write_u8(texel.r);
        hasher.write_u8(texel.g);
        hasher.write_u8(texel.b);
        hasher.write_u8(texel.height);
    }
}

/// Fold a weight field: its slot count, then each material and share.
///
/// The count goes in first so two fields with the same leading slots but
/// different lengths cannot fold alike.
fn fold_field(hasher: &mut FastHash, field: &WeightField) {
    hasher.write_u8(u8::try_from(field.slots().len()).unwrap_or(u8::MAX));
    for slot in field.slots() {
        hasher.write_u16(slot.material.id());
        hasher.write_u16(slot.weight);
    }
}

#[cfg(test)]
mod tests;
