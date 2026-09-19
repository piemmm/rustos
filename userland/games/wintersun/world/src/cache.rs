//! The chunk cache.
//!
//! A generated chunk is never stored: recomputing it is cheaper than
//! reading it back, and a world that is a pure function of its seed has
//! nothing to store that the seed does not already say. What a chunk is,
//! is *worth keeping while the player is standing on it* — which is a
//! cache, and a cache in TAIRiX is governed rather than merely bounded.
//!
//! So this is a [`ReclaimCache`]: its budget is derived from the memory
//! the machine actually has, its entries are charged to a ledger, and it
//! gives them back through the same pressure bands as every other
//! reclaimable cache in the system. Nothing here scales with the realm's
//! extent — the resident set is the chunks the player can see, on a
//! hundred-square-kilometre realm exactly as on a small one.
//!
//! The generation token is the realm's own parameter document. Change any
//! of it and every cached chunk is stale by definition, which is the
//! honest invalidation rule for state derived from nothing else.

use tairix_hash::BuildFastHash;
use tairix_log::Sink;
use tairix_reclaim::{
    CacheBudget, CacheCandidate, CachedBytes, InvalidationSource, PressureGauge, RebuildCost,
    ReclaimCache, ReclaimClass, ReclaimOwner, ReclaimRule, Sensitivity, Served,
};
use tairix_wintersun_net::value::ChunkCoord;

use crate::chunk::{Chunk, ChunkBuild};
use crate::error::WorldError;
use crate::params::RealmParams;
use crate::realm::RealmField;

/// Per-entry bookkeeping the cache charges on top of a chunk's payload:
/// the key, the map node, and the recency link.
const ENTRY_METADATA_BYTES: usize = 64;

impl CachedBytes for Chunk {
    fn payload_bytes(&self) -> usize {
        Chunk::payload_bytes(self)
    }

    fn wipe(&mut self) {
        // Terrain is public — a player can see the hills by walking to
        // them — so nothing here is secret. The bytes are still overwritten
        // rather than merely dropped, because "this one is fine to leave"
        // is the habit that eventually leaks something that is not.
        self.scrub();
    }
}

/// A bounded, reclaimable cache of generated chunks.
#[derive(Debug)]
pub struct ChunkCache {
    cache: ReclaimCache<ChunkCoord, Chunk, RealmParams, BuildFastHash>,
}

impl ChunkCache {
    /// A cache sized from the memory the machine reported.
    ///
    /// `backing_bytes` comes from the System Information API at the
    /// caller, not from a constant here: this crate is `no_std` and does
    /// not ask the kernel anything, and a capacity a library picked for
    /// itself would be the hand-chosen ceiling the charter forbids.
    ///
    /// `owner` names the process for the ledger; `pressure` is the
    /// process-wide gauge every cache in the program shares, so they all
    /// shrink on the same band at the same moment.
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
            // A chunk is a coarse-field read, several noise fields, a
            // carve, a classification and a scatter pass.
            rebuild_cost: RebuildCost::Expensive,
            sensitivity: Some(Sensitivity::Public),
            // Nothing invalidates a chunk but the realm it belongs to
            // changing, which is what the parameter document is.
            invalidation: Some(InvalidationSource::GenerationToken),
            rule: Some(ReclaimRule::Drop),
            entry_metadata_bytes: ENTRY_METADATA_BYTES,
        };
        Self {
            cache: ReclaimCache::new(
                "wintersun-chunks",
                candidate,
                CacheBudget::from_backing(backing_bytes),
                pressure,
                sink,
                BuildFastHash::new(),
            ),
        }
    }

    /// The chunk at `coord`, generated if it is not held.
    ///
    /// The result is usable whether or not the cache could admit it: a
    /// machine too short to retain the chunk still gets the chunk.
    ///
    /// # Errors
    ///
    /// Whatever generation refuses — in practice
    /// [`WorldError::OutOfMemory`], since the parameters were validated
    /// before the field was solved.
    pub fn get_or_generate(
        &mut self,
        field: &RealmField,
        coord: ChunkCoord,
    ) -> Result<Served<'_, Chunk>, WorldError> {
        let mut refused = None;
        let served = self.cache.get_or_build(&field.params(), coord, || {
            match ChunkBuild::new(coord).and_then(|build| build.finish(field)) {
                Ok(chunk) => Some(chunk),
                Err(error) => {
                    refused = Some(error);
                    None
                }
            }
        });
        match served {
            Some(served) => Ok(served),
            // The cache returns nothing only when the build returned
            // nothing, and the build only does that after recording why.
            None => Err(refused.unwrap_or(WorldError::OutOfMemory)),
        }
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
}

#[cfg(test)]
mod tests;
