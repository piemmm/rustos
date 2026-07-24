//! TAIRiX userland device manager (Stage 4.HW).
//!
//! This crate is the user-space service that owns driver **autoload**:
//! it reads the architecture-neutral hardware tree
//! ([`tairix_abi::HwNode`], emitted by the early-boot platform
//! discovery), matches each node's match keys against the bind table
//! every driver declares in its signed manifest,
//! and loads each winning driver through the driver-host load gate.
//! Matching policy is **not** kernel code (microkernel-leaning).
//!
//! # Determinism and failure policy
//!
//! * The strictly highest matched bind `priority` wins; an unbroken tie
//!   between two distinct drivers is a packaging defect and the node is
//!   refused a binding — never a coin-flip.
//! * A node with no matching driver is left **unbound** and logged;
//!   this is never an error and never a panic — a
//!   headless image simply leaves its display node unbound.
//! * A load refusal (missing `CAP_DRV_LOAD`, bad signature, …) fails
//!   only that node, closed; the walk continues so
//!   one bad image cannot block boot.
//! * Every match, load, skip, and failure is logged through
//!   [`tairix_log`] with a stable [`events`] identifier
//!   (`13000..14000`).
//!
//! # Layering
//!
//! The crate is `no_std` and depends only on `lib/*`: the load *mechanism* stays behind the
//! [`DriverLoader`] seam, which the deployment's integration point
//! implements over the drvhost `Host::load` pipeline — capability
//! checks, signature verification, and spawning all remain the gate's
//! job. Candidate bind tables arrive already decoded
//! (fail-closed) by that same gate's `ParsedImage::decode_bind_table`,
//! so this crate never re-parses image bytes.
//!
//! # Stability
//!
//! Tier: `experimental` (). The wire formats consumed
//! (hardware-tree nodes, bind-table entries) are already frozen by
//! `tairix-abi`.

#![no_std]
#![forbid(unsafe_op_in_unsafe_fn)]
#![deny(missing_docs)]

extern crate alloc;

pub mod autoload;
pub mod events;
pub mod manager;
pub mod netbind;
pub mod netcfg;
pub mod observe;
pub mod service;
pub mod store;

pub use autoload::{
    match_and_load, unload_vanished, AutoloadState, NodeBindings, NodeDriver, NodeReport,
    ReportedNodes,
};
pub use manager::{AutoloadReport, DeviceManager, DriverLoader, NodeBinding};
pub use netbind::{bind_new_channels, netchan_endpoint, NetBindState, NetstackBind};
pub use netcfg::{deliver_network_settings, NetConfigState, NetworkConfigSource};
pub use service::{run, HwTreeService};
pub use store::{fetch_catalogue, load_driver, unload_driver, CatalogueDriver, DriverStoreCall};
// The deterministic match policy is the shared `lib/devmatch` definition: re-exported here so existing consumers and the
// crate's public surface are unchanged.
pub use tairix_devmatch::{best_bind_priority, resolve, DriverCandidate, MatchResolution};

// Re-export the `lib/log` items integration tests must implement to
// observe audit records, mirroring `tairix-drvhost`'s surface so
// downstream `Cargo.toml` files stay minimal.
pub use tairix_log::{Event, EventId, Field, Level, Sink};
