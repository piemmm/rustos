//! The material cache.
//!
//! A synthesised tile is worth keeping while the ground it covers is on
//! screen and worth nothing afterwards, which is a cache — and a cache in
//! TAIRiX is governed rather than merely bounded. Its budget comes from
//! the memory the machine reported, its entries are charged to a ledger,
//! and it gives them back through the same pressure bands as every other
//! reclaimable cache in the system.
//!
//! Nothing here scales with the world's extent. The resident set is the
//! materials visible at the mips they are drawn at — at most the fifteen
//! materials times the handful of levels a camera spans, and in practice
//! far fewer, because a viewport shows a few biomes rather than all of
//! them.
//!
//! The generation token is the [`Quality`] the tiles were synthesised at:
//! shed an octave and every held tile is stale by definition, which is the
//! honest invalidation rule for state derived from nothing else.

use tairix_hash::BuildFastHash;
use tairix_log::Sink;
use tairix_reclaim::{
    CacheBudget, CacheCandidate, CachedBytes, InvalidationSource, PressureGauge, RebuildCost,
    ReclaimCache, ReclaimClass, ReclaimOwner, ReclaimRule, Sensitivity,
};
use tairix_wintersun_world::biome::Material;

use crate::material::{MaterialTile, Mip, Quality};

/// Per-entry bookkeeping the cache charges on top of a tile's texels: the
/// key, the map node, and the recency link.
const ENTRY_METADATA_BYTES: usize = 64;

impl CachedBytes for MaterialTile {
    fn payload_bytes(&self) -> usize {
        MaterialTile::payload_bytes(self)
    }

    fn wipe(&mut self) {
        self.scrub();
    }
}

/// Which tile an entry is.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub struct TileKey {
    /// The material synthesised.
    pub material: Material,
    /// The level it was synthesised at.
    pub mip: Mip,
}

/// A bounded, reclaimable cache of synthesised material tiles.
#[derive(Debug)]
pub struct MaterialCache {
    cache: ReclaimCache<TileKey, MaterialTile, Quality, BuildFastHash>,
}

impl MaterialCache {
    /// A cache sized from the memory the machine reported.
    ///
    /// `backing_bytes` comes from the System Information API at the
    /// caller: this crate is `no_std` and asks the kernel nothing, and a
    /// capacity a library picked for itself would be the hand-chosen
    /// ceiling the charter forbids.
    #[must_use]
    pub fn new(
        owner: &'static str,
        backing_bytes: usize,
        pressure: &'static (dyn PressureGauge + 'static),
        sink: &'static (dyn Sink + Sync),
    ) -> Self {
        let candidate = CacheCandidate {
            class: Some(ReclaimClass::RuntimeCache),
            owner: Some(ReclaimOwner::UserlandProcess(owner)),
            // A tile is a few octaves of value noise per texel over up to
            // 65 536 texels — cheaper than a chunk, dearer than a lookup.
            rebuild_cost: RebuildCost::Expensive,
            sensitivity: Some(Sensitivity::Public),
            invalidation: Some(InvalidationSource::GenerationToken),
            rule: Some(ReclaimRule::Drop),
            entry_metadata_bytes: ENTRY_METADATA_BYTES,
        };
        Self {
            cache: ReclaimCache::new(
                "wintersun-materials",
                candidate,
                CacheBudget::from_backing(backing_bytes),
                pressure,
                sink,
                BuildFastHash::new(),
            ),
        }
    }

    /// Make sure `key` is resident, synthesising it if it is not.
    ///
    /// Returns whether the tile is now held. A `false` is not an error:
    /// the machine is under enough pressure that the cache would not
    /// admit the tile, and the caller's answer is a coarser mip or the
    /// material's flat tone, never a dropped frame.
    ///
    /// Residency and lookup are separate calls on purpose. A splat needs
    /// up to [`BLEND_SLOTS`](tairix_wintersun_world::biome::BLEND_SLOTS)
    /// tiles at once, and four live borrows cannot come out of four
    /// mutable calls; they come out of four [`peek`](Self::peek)s after
    /// one round of this. It is also the shape an interactive loop wants
    /// — ask first, paint from what arrived.
    pub fn ensure(&mut self, quality: Quality, key: TileKey) -> bool {
        self.cache
            .get_or_build(&quality, key, || {
                MaterialTile::synthesise(key.material, key.mip, quality).ok()
            })
            .is_some_and(|served| served.is_cached())
    }

    /// The tile for `key` if it is held, without building one.
    #[must_use]
    pub fn peek(&self, quality: Quality, key: &TileKey) -> Option<&MaterialTile> {
        self.cache.peek(&quality, key)
    }

    /// Shrink to the current pressure band, returning the entries
    /// released.
    ///
    /// Called when the process's band changes, not on a timer: the gauge
    /// is told by the kernel's memory-pressure notice, and nothing here
    /// polls for it.
    pub fn enforce_pressure(&mut self) -> usize {
        self.cache.enforce_pressure()
    }

    /// Payload bytes the cache is currently charged for.
    #[must_use]
    pub fn charged_bytes(&self) -> usize {
        self.cache.charged_bytes()
    }

    /// Entries currently held.
    #[must_use]
    pub fn len(&self) -> usize {
        self.cache.len()
    }

    /// Whether the cache holds nothing.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.cache.is_empty()
    }
}

#[cfg(test)]
mod tests;
