//! RustOS userland device manager (Stage 4.HW — `AGENTS.md` §18).
//!
//! This crate is the user-space service that owns driver **autoload**:
//! it reads the architecture-neutral hardware tree
//! ([`rustos_abi::HwNode`], emitted by the §17.2 early-boot platform
//! discovery), matches each node's match keys against the bind table
//! every driver declares in its signed manifest (`AGENTS.md` §18.3),
//! and loads each winning driver through the §8 driver-host load gate.
//! Matching policy is **not** kernel code (`AGENTS.md` §4 —
//! microkernel-leaning).
//!
//! # Determinism and failure policy
//!
//! * The strictly highest matched bind `priority` wins; an unbroken tie
//!   between two distinct drivers is a packaging defect and the node is
//!   refused a binding — never a coin-flip (`AGENTS.md` §18.3, §2.1).
//! * A node with no matching driver is left **unbound** and logged;
//!   this is never an error and never a panic (`AGENTS.md` §18.4) — a
//!   headless image simply leaves its display node unbound.
//! * A load refusal (missing `CAP_DRV_LOAD`, bad signature, …) fails
//!   only that node, closed (`AGENTS.md` §5.4); the walk continues so
//!   one bad image cannot block boot.
//! * Every match, load, skip, and failure is logged through
//!   [`rustos_log`] with a stable [`events`] identifier
//!   (`13000..14000`).
//!
//! # Layering
//!
//! The crate is `no_std` and depends only on `lib/*`
//! (`AGENTS.md` §17.4): the load *mechanism* stays behind the
//! [`DriverLoader`] seam, which the deployment's integration point
//! implements over the drvhost `Host::load` pipeline — capability
//! checks, signature verification, and spawning all remain the gate's
//! job (`AGENTS.md` §5.4). Candidate bind tables arrive already decoded
//! (fail-closed) by that same gate's `ParsedImage::decode_bind_table`,
//! so this crate never re-parses image bytes.
//!
//! # Stability
//!
//! Tier: `experimental` (per `AGENTS.md` §6). The wire formats consumed
//! (hardware-tree nodes, bind-table entries) are already frozen by
//! `rustos-abi`.

#![no_std]
#![forbid(unsafe_op_in_unsafe_fn)]
#![deny(missing_docs)]

extern crate alloc;

pub mod events;
pub mod manager;
pub mod matcher;

pub use manager::{AutoloadReport, DeviceManager, DriverLoader, NodeBinding};
pub use matcher::{best_bind_priority, resolve, DriverCandidate, MatchResolution};

// Re-export the `lib/log` items integration tests must implement to
// observe audit records, mirroring `rustos-drvhost`'s surface so
// downstream `Cargo.toml` files stay minimal.
pub use rustos_log::{Event, EventId, Field, Level, Sink};
