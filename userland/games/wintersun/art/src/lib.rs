//! `WinterSun`'s ground art: what the world the generator decided
//! actually looks like.
//!
//! The world generator answers *what is here* — a normalised blend of
//! materials at every cell, the rivers, the roads. This crate answers
//! *what that looks like*: the palette everything is drawn from, the
//! materials it synthesises rather than ships, the splat that blends
//! them into pixels, the decals that wear a road into grass, and the
//! particles a storm is made of.
//!
//! # Four ideas, and the rest follows
//!
//! **Materials are synthesised, not shipped.** A material is a palette
//! ramp and four numbers, from which its texture and its height field
//! are generated on the machine that draws them ([`material`]).
//! Fifteen of those are a few hundred bytes in the binary rather than
//! megabytes of photographic tiles on disk, and being generated they
//! are resolution-independent — a mip is the same field sampled coarser,
//! not a blur of a fixed master.
//!
//! **A pixel is decided by height, not by weight alone.** Each material
//! carries its own surface relief, and where one stands proud of its
//! neighbours it takes the pixel outright ([`splat`]). That is what
//! makes gravel emerge through grass in patches rather than the two
//! averaging into a grey that is neither, and it costs nothing extra:
//! the height rides in the fourth byte of a texel the splat was reading
//! anyway.
//!
//! **Roads and rivers are weight, not geometry.** A spline raises its
//! own material's share of the cells it passes ([`decal`]), through the
//! one mutation everything that changes the ground goes through
//! ([`weight::WeightField::cover`]). So a road *wears into* the grass
//! with a frayed edge, two roads meeting merge rather than overlap, and
//! snow settling later needs no second mechanism at all.
//!
//! **One warp field breaks the repetition.** A synthesised tile is
//! finite, so an affine lookup would show its period across a large
//! grassland. A smooth, low-frequency vector field displaces the lookup
//! ([`splat::Warp`]); its Jacobian carries a local rotation, scale and
//! offset together, so there is no separate jitter to seam at a lattice
//! boundary.
//!
//! # No floating point, anywhere
//!
//! Nothing in this crate is `f32` or `f64`, and a crate-level `deny`
//! makes that a compile error rather than a habit. Rust's integer
//! arithmetic is exactly specified on every target and the only
//! target-dependent integer type is `usize`, which nothing here folds
//! into a result — so a frame is bit-identical on `x86_64`, `aarch64`,
//! `riscv64` and `wasm32` by construction. It is also the right choice
//! for the pass with the largest share of the frame budget: the whole
//! pipeline is shifts, masks and byte-wide weighted means.
//!
//! [`digest::REFERENCE_DIGEST`] records what the art *is*, so a change
//! nobody intended shows up as a moved number.
//!
//! # Memory
//!
//! Resident cost is what is on screen. Synthesised tiles live in a
//! [`cache::MaterialCache`] budgeted from the memory the machine
//! reported and released through the system's pressure bands; a tile
//! the cache will not admit degrades to a coarser mip and then to the
//! material's flat tone, so the pass is total and a frame is never
//! dropped over a texture. A particle field's budget is derived from
//! the area on screen and the current pressure band, never from a
//! number picked here.
//!
//! # Example
//!
//! Drawing a run of terrain pixels from a generated cell's blend:
//!
//! ```
//! use tairix_raster::color::Pixel;
//! use tairix_wintersun_art::material::{MaterialTile, Mip, Quality};
//! use tairix_wintersun_art::splat::{splat, Geometry, SpanPlan, SpanTiles, Warp};
//! use tairix_wintersun_art::weight::WeightField;
//! use tairix_wintersun_net::value::WorldPoint;
//! use tairix_wintersun_world::biome::{Blend, Material, BLEND_SLOTS};
//!
//! # fn main() -> Result<(), tairix_wintersun_art::error::ArtError> {
//! let realm_seed = 0x5749_4E54_4552;
//!
//! // The two ends of one horizontal run, from the cells it crosses.
//! let left = WeightField::from_blend(&Blend::solid(Material::Snowfield));
//! let mut right = WeightField::from_blend(&Blend::solid(Material::Rock));
//! right.cover(Material::Gravel, 90);
//! let plan = SpanPlan::new(&left, &right);
//!
//! // Resolve a tile per material the plan needs, then draw from them.
//! let held: Vec<MaterialTile> = plan
//!     .materials()
//!     .map(|m| MaterialTile::synthesise(m, Mip::BASE, Quality::FULL))
//!     .collect::<Result<_, _>>()?;
//! let mut tiles: SpanTiles<'_> = [None; BLEND_SLOTS];
//! for (slot, tile) in tiles.iter_mut().zip(held.iter()) {
//!     *slot = Some(tile);
//! }
//!
//! let warp = Warp::new(realm_seed);
//! let geometry = Geometry::new(&warp, WorldPoint { x: 0, y: 0 }, 32, 64);
//! let mut row = [Pixel::TRANSPARENT; 64];
//! splat(&mut row, &plan, &tiles, &geometry);
//!
//! assert!(row.iter().all(|p| p.a == 255));
//! # Ok(())
//! # }
//! ```
#![no_std]
#![forbid(unsafe_code)]
#![deny(missing_docs)]
// Nothing here is floating point, and the lint is what keeps it that way:
// integer arithmetic is bit-identical on every target by the language's
// own rules, which is what makes a frame reproducible without a vertical
// per architecture to prove it.
#![deny(clippy::float_arithmetic)]

extern crate alloc;

pub mod cache;
pub mod decal;
pub mod digest;
pub mod error;
pub mod material;
pub mod noise;
pub mod palette;
pub mod particle;
pub mod splat;
pub mod weight;
