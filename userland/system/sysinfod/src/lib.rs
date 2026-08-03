//! TAIRiX System Information service (`sysinfod`) — Stage 6.
//!
//! TAIRiX has no `/proc` and no `/sys`. Every piece of live system
//! information that those trees would have exposed is served by this single
//! user-space service through the typed, versioned `sysinfo-v1` ABI defined
//! in [`tairix_abi::sysinfo`]. `sysinfod` is the *only* server of that API,
//! and the kernel exposes no path that bypasses it; the
//! installed binary lives at `/System/Services/sysinfod.app/Run`.
//!
//! # What this crate is
//!
//! This crate is the **dispatcher**: the policy layer that turns a raw
//! request buffer into a typed, capability-checked, audited answer. It owns
//! exactly four responsibilities and no data of its own:
//!
//! 1. Decode the [`SysinfoRequestHeader`](tairix_abi::SysinfoRequestHeader)
//!    and any typed payload, failing closed on any malformed input.
//! 2. Enforce the capability each query declares in the frozen registry
//!    ([`SYSINFO_QUERIES`](tairix_abi::SYSINFO_QUERIES)) **before** touching
//!    any state.
//! 3. Emit a [`tairix_log`] audit record for every invocation of a query
//!    whose registry row sets `audit` — the cross-principal, kernel, and
//!    hardware queries — and for every denial.
//! 4. Page and encode the answer supplied by an injected [`SysinfoSource`].
//!
//! The live data — the process table, memory accounting, the hardware tree,
//! machine identity, uptime — is read through the [`SysinfoSource`] seam so
//! that the security-relevant dispatch code is independent of any particular
//! kernel plumbing and is exhaustively testable with an in-memory fixture.
//!
//! # Module map
//!
//! * [`events`] — stable [`tairix_log::EventId`] constants (`8000` range).
//! * [`source`] — the [`SysinfoSource`] data seam, the authenticated
//!   [`Caller`], and the [`ProcessScope`] selector.
//! * [`reporters`] — the [`CacheLedgerRegistry`], `sysinfod`'s own record of
//!   the cache ledgers userland processes report for their own caches.
//! * [`service`] — the [`serve`] entry point and its request pipeline.
//!
//! # Layering
//!
//! The crate is `no_std` and depends only on the audited
//! `lib/*` crates `tairix-abi` and `tairix-log`, so a userland service never
//! links a kernel or driver crate.

#![no_std]
#![forbid(unsafe_op_in_unsafe_fn)]
#![deny(missing_docs)]

extern crate alloc;

pub mod events;
pub mod reporters;
pub mod service;
pub mod source;

#[cfg(test)]
mod testing;

pub use reporters::CacheLedgerRegistry;
pub use service::serve;
pub use source::{Caller, ProcessScope, SysinfoSource};
