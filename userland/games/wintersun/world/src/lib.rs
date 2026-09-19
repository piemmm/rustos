//! `WinterSun`'s world: a `u64` seed, eight parameters, and every chunk of
//! ground that follows from them.
//!
//! The world is never transmitted and never stored. A client asks this
//! crate for the ground it is standing on and gets the same answer the
//! realm server would have given, because both computed it from the same
//! seed. What the seed cannot predict — entities, and the changes players
//! made — is the realm's to send; what it can is nobody's to send at all.
//! That is what makes a world of this size affordable in bandwidth and in
//! memory.
//!
//! # Two scales, and why
//!
//! Some questions are not local. How much water passes a point depends on
//! everything upstream of it; a rain shadow is a record of everything the
//! wind crossed to get there; a road is only a road if it reaches the town
//! at its far end. None of those can be answered from a window around one
//! chunk, and a generator that tries produces rivers that flow uphill
//! across the seam between two windows.
//!
//! So generation is split across two scales:
//!
//! * **The realm field** ([`realm::RealmField`]) — the whole world, solved
//!   once, coarsely. Plates, relief, drainage, erosion, lakes, climate,
//!   settlements, roads, landmarks. It is **global and exact**, so
//!   everything derived from it is seam-free by construction. Its cost is
//!   a fixed sample count rather than a step in world units, so a realm
//!   four chunks across and one four thousand chunks across pay the same
//!   for it.
//! * **The chunk** ([`chunk::ChunkBuild`]) — fine detail, on demand, from
//!   a bounded halo. It reads the realm field, adds everything below the
//!   coarse step, and depends on nothing outside a fixed ring of cells.
//!
//! The pipeline the plan sets out runs across both: uplift, relief,
//! hydrology, climate and the placement of settlements and roads at realm
//! scale; the fine relief, channel carve, structure stamp, climate
//! correction, biome classification and scatter at chunk scale. Scatter is
//! the last chunk phase because it reads the structure stamp — nothing
//! grows on a road.
//!
//! # What "seed-pure" means here, exactly
//!
//! Every answer is a pure function of `(seed, parameters, position)`. Not
//! of how many chunks have been asked for, nor in what order, nor on which
//! machine. Concretely:
//!
//! * Randomness is a **keyed hash over coordinates**, not a sequence, so a
//!   value has no dependence on traversal ([`seed`]).
//! * Arithmetic is IEEE-754 `f64` restricted to the exactly-specified
//!   operations plus `lib/util::mathf`, TAIRiX's own libm — never a
//!   platform one, which would differ per target.
//! * Everything stored is a **quantised integer** on a power-of-two scale,
//!   so the conversion is exact and the stored value is a bit pattern, not
//!   a rounding.
//! * Every sort, priority queue and traversal has a total order with an
//!   index tiebreak, so no two equal keys can resolve differently.
//!
//! The claim is staked on [`digest::REFERENCE_DIGEST`], one constant every
//! Tier-1 target asserts.
//!
//! # Memory
//!
//! Resident cost is the working set, never the world's extent. The realm
//! field is a fixed grid; chunks live in a [`cache::ChunkCache`] budgeted
//! from the memory the machine reported and reclaimed through the system's
//! pressure bands. A generated chunk is never written to disk — recomputing
//! it is cheaper than reading it back, and only the changes players make
//! are worth storing.
//!
//! # Example
//!
//! Solving a realm and walking a chunk out of it:
//!
//! ```
//! use tairix_wintersun_world::chunk::ChunkBuild;
//! use tairix_wintersun_world::params::RealmParams;
//! use tairix_wintersun_world::realm::RealmField;
//! use tairix_wintersun_net::value::ChunkCoord;
//!
//! # fn main() -> Result<(), tairix_wintersun_world::error::WorldError> {
//! let field = RealmField::generate(RealmParams::winter_default(0x5EED))?;
//! let chunk = ChunkBuild::new(ChunkCoord { x: 0, y: 0 })?.finish(&field)?;
//!
//! // Weights are normalised by construction, at every cell.
//! assert_eq!(chunk.blend(0, 0).total(), 255);
//! # Ok(())
//! # }
//! ```

#![no_std]
#![forbid(unsafe_code)]
#![deny(missing_docs)]

extern crate alloc;

pub mod biome;
pub mod cache;
pub mod chunk;
pub mod climate;
pub mod digest;
pub mod error;
pub mod geom;
pub mod hydrology;
pub mod noise;
pub mod params;
pub mod realm;
pub mod relief;
pub mod scatter;
pub mod seed;
pub mod sites;
pub mod uplift;
