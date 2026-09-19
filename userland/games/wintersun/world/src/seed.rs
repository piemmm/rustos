//! Domain-separated deterministic randomness.
//!
//! Every stage draws from its own stream, keyed by the realm seed *and* a
//! stage tag, so adding a stage cannot shift an earlier one's output and two
//! stages sampling the same lattice point never see the same number.
//!
//! Generation needs **random access**, not a sequence: a chunk asks for the
//! value at one lattice point without generating any of its neighbours. So
//! the primitive here is a keyed hash over coordinates rather than a
//! generator, and [`Stream`] exists only where a stage genuinely wants a
//! sequence — a plate's several parameters, a settlement's several rolls —
//! that would otherwise need a field tag nobody would keep unique.
//!
//! Neither is a new algorithm. The hash is `lib/hash`'s XXH64, whose writes
//! are little-endian on every port; the stream is `lib/rng`'s xoshiro256++.

use tairix_hash::FastHash;
use tairix_rng::{NonCryptoRng, RandU64};

/// What a stream or lattice value belongs to.
///
/// The discriminants are frozen: changing one re-rolls every realm ever
/// generated with it, so a new stage takes a new value and never reuses a
/// retired one.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
#[repr(u64)]
pub enum Stage {
    /// Continental plate seeds, drift, and boundary character.
    Plates = 1,
    /// Continental-scale relief noise.
    Continent = 2,
    /// Mountain-belt ridged noise.
    Ridge = 3,
    /// Domain warp applied to the relief lattices.
    Warp = 4,
    /// Fine per-cell relief detail.
    Detail = 5,
    /// Dune billow inside arid, low-relief ground.
    Dune = 6,
    /// Prevailing-wind and moisture-field jitter.
    Climate = 7,
    /// Biome classification jitter, which softens a Whittaker boundary.
    Biome = 8,
    /// Vegetation, rock and resource scatter.
    Scatter = 9,
    /// Settlement candidate selection.
    Settlement = 10,
    /// Dungeon, shrine, ruin and rift-scar placement.
    Landmark = 11,
}

/// A realm's generation key: its seed, fixed for the realm's life.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub struct SeedKey {
    seed: u64,
}

impl SeedKey {
    /// The key for a realm seed.
    #[must_use]
    pub const fn new(seed: u64) -> Self {
        Self { seed }
    }

    /// The 64-bit value this stage assigns to the lattice point `(x, y)`.
    ///
    /// A pure function of `(seed, stage, x, y)`: the same four inputs give
    /// the same word on every target, and no traversal order can change it.
    #[must_use]
    pub fn lattice(self, stage: Stage, x: i32, y: i32) -> u64 {
        let mut message = [0u8; 16];
        message[0..8].copy_from_slice(&(stage as u64).to_le_bytes());
        message[8..12].copy_from_slice(&x.to_le_bytes());
        message[12..16].copy_from_slice(&y.to_le_bytes());
        FastHash::hash_bytes(self.seed, &message)
    }

    /// The lattice value scaled into `0.0..1.0`.
    #[must_use]
    pub fn unit(self, stage: Stage, x: i32, y: i32) -> f64 {
        unit_from(self.lattice(stage, x, y))
    }

    /// The lattice value scaled into `-1.0..1.0`.
    #[must_use]
    pub fn signed(self, stage: Stage, x: i32, y: i32) -> f64 {
        self.unit(stage, x, y) * 2.0 - 1.0
    }

    /// A sequential stream for this stage and lattice point.
    #[must_use]
    pub fn stream(self, stage: Stage, x: i32, y: i32) -> Stream {
        Stream {
            rng: NonCryptoRng::seed_from_u64(self.lattice(stage, x, y)),
        }
    }
}

/// Scale a 64-bit word into `0.0..1.0`.
///
/// Built from the top 53 bits, which is every bit an `f64` mantissa holds,
/// so the widening is lossless and the scaling is one exact multiply.
#[must_use]
fn unit_from(word: u64) -> f64 {
    /// `2^-53`, exactly representable.
    const SCALE: f64 = 1.0 / 9_007_199_254_740_992.0;
    #[allow(
        clippy::cast_precision_loss,
        reason = "the shift leaves exactly the 53 bits an f64 mantissa holds"
    )]
    {
        (word >> 11) as f64 * SCALE
    }
}

/// A sequence of draws for one stage at one lattice point.
///
/// Deliberately not [`Clone`]: two holders of one stream drawing in
/// different orders is the one way a caller could make generation depend on
/// traversal order, and the borrow checker guards that better than a note.
#[derive(Debug)]
pub struct Stream {
    rng: NonCryptoRng,
}

impl Stream {
    /// The next draw in `0.0..1.0`.
    #[must_use]
    pub fn unit(&mut self) -> f64 {
        unit_from(self.rng.next_u64())
    }

    /// The next draw in `-1.0..1.0`.
    #[must_use]
    pub fn signed(&mut self) -> f64 {
        self.unit() * 2.0 - 1.0
    }

    /// The next draw in `low..=high`, or `low` when `high` is below `low`.
    ///
    /// Rejection-free and therefore constant-cost. The modulo bias over the
    /// small ranges the generator draws is orders below the noise it
    /// perturbs, where a rejection loop would make the number of draws
    /// depend on the seed — and so make a stage's later draws depend on its
    /// earlier ones in a way no test would catch.
    #[must_use]
    pub fn range_u32(&mut self, low: u32, high: u32) -> u32 {
        let Some(span) = high.checked_sub(low) else {
            return low;
        };
        let width = u64::from(span) + 1;
        #[allow(
            clippy::cast_possible_truncation,
            reason = "the remainder is below `width`, itself at most 2^32"
        )]
        let offset = (self.rng.next_u64() % width) as u32;
        low + offset
    }
}

#[cfg(test)]
mod tests;
