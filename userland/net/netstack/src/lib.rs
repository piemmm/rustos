//! RustOS network-stack service engine (`plans/NETWORK.md` N3b).
//!
//! `netstack` is the user-space process that owns the network: every
//! managed interface, its addresses and routes, and the frame flow
//! between the pure `lib/net` protocol engine and the link-layer
//! drivers over the shared-memory frame-ring transport. This crate is
//! the host-testable *engine* of that service — the interface table
//! ([`Netstack`]), the ring pump, and the capability-checked request
//! dispatcher ([`serve`]) — while `src/run.rs` is the thin freestanding
//! `Run` binary that binds the reserved
//! [`NETSTACK_ENDPOINT`](rustos_abi::net_ipc::NETSTACK_ENDPOINT) and
//! parks on its wait sources.
//!
//! # Security
//!
//! Every request is decoded whole and capability-checked against the
//! caller's kernel-attested origin **before any state is touched**
//! (fail closed): the admin surface (interface list, address add,
//! route add, counters) demands `CAP_NET_ADMIN`, and the whole-system
//! facts/state reads demand `CAP_SYSINFO_INTROSPECT` — they are served
//! to the System Information broker, which narrows them per client.
//! Every mutation and every refusal is a structured audit record
//! ([`events`]).

#![no_std]
#![forbid(unsafe_code)]
#![deny(missing_docs)]

extern crate alloc;

pub mod events;
mod iface;
mod service;

pub use iface::{Interface, Netstack};
pub use service::{serve, Caller};

#[cfg(test)]
mod tests;
