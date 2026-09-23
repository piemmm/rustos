//! The SSH protocol engine (`plans/SSH.md`).
//!
//! Pure: no I/O, no clock, no randomness. Every state machine is driven by
//! typed calls and answers with typed results, so the code the fuzz harnesses
//! and tests exercise is the code a connection's sandboxed worker runs — a
//! process that can reach neither a clock nor an RNG. What the protocol needs
//! from either arrives as input instead: random padding from a reserve the host
//! tops up ([`transport::Transport::supply_padding`]), and time-driven events as
//! calls ([`transport::Transport::rekey_interval_elapsed`]).
//!
//! # Modules
//!
//! - [`wire`] — the RFC 4251 §5 data types, both directions.
//! - [`msg`] — message numbers and disconnect reasons (RFC 4250 §4.1, §4.2.2).
//! - [`ident`] — the identification-string exchange (RFC 4253 §4.2).
//! - [`algorithm`] — the ciphers and MACs the packet layer frames, and the key
//!   material one direction is keyed with.
//! - [`packet`] — the binary packet protocol (RFC 4253 §6) under every framing
//!   those algorithms need, and the RFC 4344 rekey thresholds.
//! - [`transport`] — the transport layer's state machine: what may be sent and
//!   received in each phase of a key exchange, strict key exchange, the key
//!   switch at `SSH_MSG_NEWKEYS`, and the generic transport messages.

#![no_std]
#![forbid(unsafe_code)]
#![deny(missing_docs)]

extern crate alloc;

pub mod algorithm;
pub mod ident;
pub mod msg;
pub mod packet;
pub mod transport;
pub mod wire;

/// Which end of a connection an engine is.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub enum Role {
    /// The end that opened the connection.
    Client,
    /// The end that accepted it.
    Server,
}

#[cfg(test)]
#[path = "interop_tests.rs"]
mod interop_tests;
