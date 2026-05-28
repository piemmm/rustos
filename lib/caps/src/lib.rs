//! Capability primitives for RustOS.
//!
//! Per `AGENTS.md` §5.2 a process's authority is the set of unforgeable
//! kernel-issued capabilities it holds. This crate provides the building
//! blocks every other RustOS component uses to talk about that authority:
//!
//! * [`CapabilitySet`] — an efficient, allocation-free set of
//!   [`CapabilityId`](rustos_abi::CapabilityId) values, with the
//!   subset-only delegation operator wired in at the type level.
//! * [`CapabilityToken`] — the on-wire, Ed25519-signed envelope a privileged
//!   authority issues to delegate a (necessarily narrower) capability set to
//!   another task. Tokens carry a *revocation epoch* so that a token issued
//!   under epoch `N` is automatically void on epoch `N + 1`.
//!
//! All operations are infallible or return a [`rustos_abi::Errno`]: no
//! `panic!`, no `unwrap`. The crate is `no_std` and performs no allocation.
//!
//! # Delegation invariant
//!
//! The single rule that makes capabilities sound is **a delegated set is
//! always a subset of the parent set**. It is enforced in one place
//! ([`CapabilitySet::delegate`]) and re-checked by [`CapabilityToken::verify`]
//! so a forged "wider" token cannot be accepted even if its signature
//! verifies.

#![no_std]
#![forbid(unsafe_op_in_unsafe_fn)]
#![deny(missing_docs)]

pub mod set;
pub mod token;

pub use set::CapabilitySet;
pub use token::{CapabilityToken, RevocationEpoch};
