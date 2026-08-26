//! The **driver side** of the `netchan-v1` NIC device-channel contract
//! (`plans/NETWORK.md` §2.3, N4c/N4d).
//!
//! The network stack (`userland/net/netstack`) owns the shared frame-ring
//! region and is the channel's *client*; a link-layer driver process owns
//! the device (MMIO/DMA/IRQ) and is its *server*. This crate is that server,
//! written once over the [`Net`](tairix_abi::driver::net::Net) trait so every
//! NIC driver process — whatever silicon it drives — shares one control
//! plane instead of re-deriving it.
//!
//! Two layers:
//!
//! * [`NetChannelServer`] — the pure, host-testable per-request handler:
//!   attach state, geometry validation, and the ring/service logic. No I/O,
//!   so the whole control plane is exercised on the host against a mock
//!   device. [`Drain`] is its interrupt-path counterpart: the whole
//!   mask/unmask/stop policy as a state machine over
//!   [`ServiceReport`](tairix_abi::driver::net_ring::ServiceReport)s.
//! * `serve` — the freestanding process loop the driver binary hands its
//!   opened device to: claim a reserved device-channel endpoint, publish the
//!   [`NETCHAN_NODE_COMPATIBLE`](tairix_abi::driver::net_channel::NETCHAN_NODE_COMPATIBLE)
//!   hardware-tree node carrying it, and park on
//!   a wait set over {call endpoint, device interrupt} for the life of the
//!   driver. Compiled only for the bare-metal targets a driver binary is
//!   built for; the host build carries just the pure handler.
//!
//! # Fail closed
//!
//! Every reply is a fully-encoded `netchan-v1` frame carrying a typed
//! [`Errno`](tairix_abi::Errno) — a service call before attach, a region too
//! small for the agreed geometry, or a device fault is never a panic and
//! never a partially-applied action. Every set-up refusal in `serve` exits
//! with a reserved code from [`exit`] rather than degrading.

#![no_std]
#![forbid(unsafe_op_in_unsafe_fn)]
#![deny(missing_docs)]

mod server;
pub use server::{Drain, DrainAction, DrainStep, Drained, Masked, NetChannelServer};

#[cfg(target_os = "none")]
mod serve;
#[cfg(target_os = "none")]
pub use serve::serve;

/// The reserved, fail-closed process exit codes a NIC driver binary ends
/// with when it cannot serve its device.
///
/// One definition for every link-layer driver process: the codes are the
/// diagnosis a supervisor reads off a driver that gave up, so two drivers
/// reporting the same failure must report the same number. `serve` returns
/// [`NO_SERVICE`](exit::NO_SERVICE) itself; the others are returned by the
/// binary's own bring-up, before it hands the device over.
pub mod exit {
    /// The rt-backed driver host could not be built from the
    /// kernel-delivered grants (the `resource_grants` query was refused, or
    /// the delivery did not fit).
    pub const NO_HOST: i32 = 80;
    /// The delivered grants do not name the resources this driver needs — an
    /// unbound, mis-provisioned, or malformed node.
    pub const NO_RESOURCES: i32 = 81;
    /// Device bring-up failed: a window could not be mapped, the device is
    /// not the one the node claimed, it rejected its init sequence, or the
    /// granted interrupt line could not be bound (the serve loop parks on
    /// it, so a driver that cannot bind it would degrade into the busy
    /// re-poll the charter forbids).
    pub const BRINGUP_FAILED: i32 = 82;
    /// The device channel could not be stood up: no free reserved endpoint
    /// id, the bind was refused, the `netchan` node could not be published,
    /// or the wait set could not be built.
    pub const NO_SERVICE: i32 = 83;
}
