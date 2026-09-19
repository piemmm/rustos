//! `WinterSun`'s authoritative simulation: the tick, and every rule it
//! applies.
//!
//! A client of this crate sends *intents* and is told what happened. It
//! never asserts a position, a hit, or a balance — those are computed here,
//! from inputs this crate validated, which is what makes the realm
//! authoritative rather than merely well-behaved.
//!
//! # The tick is fixed-rate and total
//!
//! One step consumes the intents admitted since the last, advances every
//! entity, resolves what they ran into, and emits the notifications. It is
//! *total*: there is no work deferred to "whenever the next one happens",
//! and no phase that runs only sometimes. And its order is fixed and
//! documented ([`zone::Zone::step`]) — "whatever order the map iterated" is
//! a defect, because two realms iterating differently would diverge and
//! nothing downstream could recover.
//!
//! # Determinism, and why this crate barely needs a libm
//!
//! The simulation is bit-identical on `x86_64`, `aarch64`, `riscv64` and
//! `wasm32`, and that is a test rather than an intention
//! ([`digest::REFERENCE_DIGEST`]). It is also cheap to hold here, because
//! almost nothing in it is floating point:
//!
//! * Movement is fixed-point integer arithmetic with a carried remainder,
//!   so a slow walk accumulates exactly rather than rounding to a standstill
//!   ([`motion`]).
//! * Separation uses an exact integer square root, never a float distance.
//! * Every curve, mitigation and duration is integer arithmetic over a
//!   bounded domain ([`stat`], [`damage`], [`status`]).
//! * The single exception is turning a held direction into a heading, which
//!   goes through `lib/util::mathf` — TAIRiX's own libm — so the same source
//!   yields the same bits everywhere.
//!
//! The four-target vertical still runs, because "should be identical" and
//! "is identical" are different claims, and `wasm32` in particular is the
//! only Tier-1 target with a 32-bit `usize`.
//!
//! # What a client cannot lie about
//!
//! The realm's tick counter is the only clock. A client may say where inside
//! a tick it sampled an input, because a fixed step otherwise quantises
//! every action to its boundary; it may not say how much time has passed.
//! [`clock::place`] validates and clamps the one, and nothing accepts the
//! other. Distance moved is computed from the mover's own speed, so it is
//! never a number a client supplies at all, and each intent's sequence is
//! applied once.
//!
//! # Example
//!
//! Spawning a body, holding a direction, and stepping:
//!
//! ```
//! use tairix_wintersun_net::client::{Intent, IntentKind};
//! use tairix_wintersun_net::value::{Direction, EntityKind, TickInstant, TickPhase, WorldPoint};
//! use tairix_wintersun_rules::clock::TickRate;
//! use tairix_wintersun_rules::entity::SpawnSpec;
//! use tairix_wintersun_rules::stat::Stats;
//! use tairix_wintersun_rules::terrain::SyntheticTerrain;
//! use tairix_wintersun_rules::zone::Zone;
//!
//! let mut zone = Zone::new(TickRate::default());
//! let spec = SpawnSpec::new(
//!     EntityKind(1),
//!     WorldPoint { x: 512, y: 512 },
//!     Stats::new(10, 10, 10, 10, 10).expect("inside the domain"),
//!     0,
//!     256,
//! )
//! .expect("a legal body");
//! let id = zone.spawn(spec).expect("room for one body");
//!
//! zone.submit(
//!     id,
//!     &Intent {
//!         sequence: 1,
//!         sampled: TickInstant { tick: 0, phase: TickPhase(0) },
//!         kind: IntentKind::Move(Direction::new(32_767, 0).expect("a unit vector")),
//!     },
//! )
//! .expect("admitted");
//!
//! zone.step(&SyntheticTerrain::open()).expect("stepped");
//! assert!(zone.entity(id).expect("still there").at().x > 512, "it moved east");
//! ```

#![no_std]
#![forbid(unsafe_code)]
#![deny(missing_docs)]

extern crate alloc;

pub mod bounds;
pub mod clock;
pub mod damage;
pub mod digest;
pub mod entity;
pub mod error;
pub mod motion;
pub mod pool;
pub mod space;
pub mod stat;
pub mod status;
pub mod terrain;
pub mod zone;
