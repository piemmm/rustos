//! TAIRiX link-local service discovery (`discoveryd`) — multicast DNS and
//! DNS-SD, `plans/ZEROCONF.md`.
//!
//! Every multicast DNS packet is unauthenticated input from a peer that
//! chooses its own arrival rate, so the service is two processes and the one
//! that parses holds nothing:
//!
//! * [`front::Front`] holds the multicast DNS sockets, the group memberships,
//!   and every authority the service will ever exercise. It parses no DNS: it
//!   relays each datagram the network stack delivered — from a sender the
//!   stack found on-link, and within that sender's relay budget — to its
//!   decoder, passes it time, and supervises it.
//! * [`decoder::Decoder`] runs the `tairix_net::mdns` engine, one per
//!   interface, as a capability-empty sandbox worker over one pipe pair
//!   (`tairix_sandbox::supervise`). It cannot open a file, reach an endpoint,
//!   spawn, or read a clock; a decoder a crafted packet kills is reaped,
//!   logged, and replaced after a paced delay, and the front survives it.
//!
//! The channel between them is [`wire`]: fixed-shape frames, each side's
//! reader a bounds-checked field read.

#![no_std]
#![forbid(unsafe_code)]
#![deny(missing_docs)]

extern crate alloc;

pub mod decoder;
pub mod events;
pub mod front;
pub mod grants;
pub mod query;
pub mod questions;
pub mod sessions;
pub mod wire;
