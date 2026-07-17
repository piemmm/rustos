//! Shared `no_std` collections that are not in `core` or `alloc`.
//!
//! Per this crate is gated on the "used by at least two
//! places" rule: every type here must justify its existence by serving a
//! concrete caller outside this crate. Adding speculative collections is
//! forbidden.
//!
//! ## Inventory (Stage 1)
//!
//! * [`BitSet256`] — fixed-size bitmap over identifiers in `0..=255`.
//!   Used by `tairix-caps` to represent a process's `CapabilityId`
//!   membership, and reserved (and documented) for the kernel scheduler's
//!   per-CPU ready bitmap landing in Stage 2.

#![no_std]
#![forbid(unsafe_op_in_unsafe_fn)]
#![deny(missing_docs)]

pub mod bitset;

pub use bitset::{BitSet256, BitSet256Iter};
