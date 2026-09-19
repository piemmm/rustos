//! The determinism digest.
//!
//! The claim this crate has to defend is that a realm is bit-identical on
//! `x86_64`, `aarch64`, `riscv64` and `wasm32`. Comparing two runs proves
//! nothing on its own — both could be wrong together — so the claim is
//! staked on **one constant**: every target folds the same fixed realm
//! into a digest and asserts it equals [`REFERENCE_DIGEST`]. Agreement
//! between targets is then a consequence of each agreeing with the
//! constant, and a target that has never been run cannot pass by
//! accident.
//!
//! # What is folded, and what is not
//!
//! Only the **quantised integer** fields — elevations, temperatures,
//! moisture, discharge, material weights, positions. The `f64`
//! intermediates are not folded, and deliberately so: the digest should
//! fail when a *stored value* differs, which is what a consumer can
//! observe, and not when an intermediate differs in a way that rounds
//! away. Since every stage's arithmetic is IEEE-754 basic operations over
//! `lib/util::mathf`, the intermediates are identical anyway — this is
//! belt and braces, not a licence for them not to be.

use core::hash::Hasher;

use tairix_hash::FastHash;
use tairix_wintersun_net::value::ChunkCoord;

use crate::chunk::{Chunk, ChunkBuild};
use crate::error::WorldError;
use crate::params::{RealmParams, RealmSpec};
use crate::realm::RealmField;

/// The digest of the reference realm, on every Tier-1 target.
///
/// Changing any stage's arithmetic changes this. That is the point: the
/// constant is not a magic number to be re-derived when a test fails, it
/// is the record of what the generator produces. A change that moves it
/// is a change to every realm anyone has ever generated, and the new
/// value is written down deliberately, not pasted from a failure.
pub const REFERENCE_DIGEST: u64 = 0x6ECC_9456_C63E_98DC;

/// The chunks the reference digest covers, as offsets from the origin.
///
/// Spread across the realm rather than clustered, so the digest sees
/// coast, interior and edge — a stage that is wrong only near the rim
/// would otherwise pass.
const PROBE_CHUNKS: [(i32, i32); 6] = [(0, 0), (1, 0), (0, 1), (-3, 2), (5, -4), (-7, -7)];

/// The realm the determinism verticals generate.
///
/// Small on purpose: the claim is about arithmetic agreeing, not about
/// throughput, and a guest under emulation should spend its budget on
/// the pipeline rather than on grid size. Every stage still runs.
///
/// # Panics
///
/// Never: the specification below is inside every bound
/// [`RealmParams::new`] enforces, and a test holds it there.
#[must_use]
pub fn reference_params() -> RealmParams {
    let spec = RealmSpec {
        seed: 0x5748_4954_4552_534E,
        extent_chunks: 32,
        coarse_samples: 64,
        plates: 9,
        ocean_permille: 420,
        relief_units: 1500,
        north_celsius: -20,
        south_celsius: 11,
        wind: tairix_wintersun_net::value::Facing(0x0800),
    };
    match RealmParams::new(spec) {
        Ok(params) => params,
        // Unreachable, and a test proves it. Falling back to the shipped
        // default rather than panicking keeps the boot-time path free of
        // an abort even if the constants above are ever edited wrongly —
        // the test is what catches that, not a crash in a player's client.
        Err(_) => RealmParams::winter_default(spec.seed),
    }
}

/// Solve `params` and fold the whole result into one number.
///
/// # Errors
///
/// Whatever generation refuses.
pub fn world(params: RealmParams) -> Result<u64, WorldError> {
    let field = RealmField::generate(params)?;
    let mut hasher = FastHash::new();
    fold_realm(&mut hasher, &field);
    for (dx, dy) in PROBE_CHUNKS {
        let coord = ChunkCoord { x: dx, y: dy };
        let chunk = ChunkBuild::new(coord)?.finish(&field)?;
        fold_chunk(&mut hasher, &chunk);
    }
    Ok(hasher.finish())
}

/// Fold one chunk, for a caller comparing chunks rather than realms.
#[must_use]
pub fn chunk(chunk: &Chunk) -> u64 {
    let mut hasher = FastHash::new();
    fold_chunk(&mut hasher, chunk);
    hasher.finish()
}

/// Fold one realm field, likewise.
#[must_use]
pub fn realm(field: &RealmField) -> u64 {
    let mut hasher = FastHash::new();
    fold_realm(&mut hasher, field);
    hasher.finish()
}

/// Every coarse sample, then every placed thing.
fn fold_realm(hasher: &mut FastHash, field: &RealmField) {
    let spec = field.params().spec();
    hasher.write(&spec.seed.to_le_bytes());
    hasher.write(&spec.extent_chunks.to_le_bytes());
    hasher.write(&spec.coarse_samples.to_le_bytes());
    hasher.write(&spec.plates.to_le_bytes());
    hasher.write(&spec.ocean_permille.to_le_bytes());
    hasher.write(&spec.relief_units.to_le_bytes());
    hasher.write(&spec.north_celsius.to_le_bytes());
    hasher.write(&spec.south_celsius.to_le_bytes());
    hasher.write(&spec.wind.0.to_le_bytes());

    for sample in field.samples() {
        hasher.write(&sample.elevation.0.to_le_bytes());
        hasher.write(&sample.water.0.to_le_bytes());
        hasher.write(&[sample.flow as u8, sample.belt]);
        hasher.write(&sample.discharge.to_le_bytes());
        hasher.write(&sample.temperature.0.to_le_bytes());
        hasher.write(&sample.moisture.0.to_le_bytes());
    }

    for site in field.sites() {
        hasher.write(&site.at.x.to_le_bytes());
        hasher.write(&site.at.y.to_le_bytes());
        hasher.write(&[site.kind as u8]);
        hasher.write(&site.radius_cells.to_le_bytes());
    }
    for road in field.roads() {
        hasher.write(&road.from.to_le_bytes());
        hasher.write(&road.to.to_le_bytes());
        for cell in &road.path {
            hasher.write(&cell.x.to_le_bytes());
            hasher.write(&cell.y.to_le_bytes());
        }
    }
    for landmark in field.landmarks() {
        hasher.write(&landmark.at.x.to_le_bytes());
        hasher.write(&landmark.at.y.to_le_bytes());
        hasher.write(&[landmark.kind as u8]);
    }
}

/// Every cell, then everything standing on it.
fn fold_chunk(hasher: &mut FastHash, chunk: &Chunk) {
    use crate::geom::CHUNK_CELLS;

    hasher.write(&chunk.coord().x.to_le_bytes());
    hasher.write(&chunk.coord().y.to_le_bytes());
    for cy in 0..CHUNK_CELLS {
        for cx in 0..CHUNK_CELLS {
            hasher.write(&chunk.elevation(cx, cy).0.to_le_bytes());
            hasher.write(&chunk.water(cx, cy).0.to_le_bytes());
            hasher.write(&chunk.temperature(cx, cy).0.to_le_bytes());
            hasher.write(&chunk.moisture(cx, cy).0.to_le_bytes());
            let blend = chunk.blend(cx, cy);
            for slot in 0..blend.materials().len() {
                hasher.write(&[blend.materials()[slot] as u8, blend.weights()[slot]]);
            }
            hasher.write(&[chunk.surface(cx, cy).bits()]);
        }
    }
    for item in chunk.scatter() {
        hasher.write(&item.at.x.to_le_bytes());
        hasher.write(&item.at.y.to_le_bytes());
        hasher.write(&[item.kind as u8, item.host as u8, item.variant, item.scale]);
    }
}

#[cfg(test)]
mod tests;
