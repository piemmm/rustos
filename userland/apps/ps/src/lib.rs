//! RustOS `ps` — list processes via the System Information API (Stage 6 `userland/apps/`).
//!
//! RustOS has no `/proc` and no `/sys`. `ps` does not scrape a virtual
//! filesystem; it issues the typed, versioned, capability-checked
//! `sysinfo-v1` process-list queries served by `/System/Services/sysinfod`
//! and has no privileged path that bypasses the capability check. By default it lists the caller's own processes
//! (the ungated `SELF_PROCESS_LIST`); `-e`/`-A` request every process
//! (`GLOBAL_PROCESS_LIST`, which the service gates on `CAP_SYSINFO_GLOBAL`).
//!
//! # What this crate is
//!
//! A thin front end over the shared `lib/procinfo` client helpers. For one
//! parsed [`Command`] it pages through the process list and renders one
//! fixed-column row per process. The request framing, the page walk, the
//! columnar rendering, and the [`Transport`]/[`Output`] seams are shared
//! with the `sysinfo` CLI through `lib/procinfo` rather than duplicated
//! here; `ps` owns only its own argument grammar, usage
//! banner, and error enum.
//!
//! The binary that ships as `ps` wires the real IPC-backed transport and
//! console; tests wire in-memory fixtures, so the request/render logic is
//! exhaustively testable without a kernel.
//!
//! # Module map
//!
//! * [`error`] — [`PsError`], the outcomes of [`run`].
//! * [`command`] — the [`Command`] enum and its argument [`parse`]r.
//! * [`client`] — the [`run`] entry point and the process-list renderer.
//!
//! # Layering & safety
//!
//! `no_std` (with `alloc`); the only dependencies are the
//! audited `lib/abi` ABI crate and the shared `lib/procinfo` client
//! helpers, so this userland tool never links a kernel or driver crate. No `unsafe`, and no `unwrap`/`expect`/`panic!` in
//! production paths.

#![no_std]
#![forbid(unsafe_op_in_unsafe_fn)]
#![deny(missing_docs)]

// `alloc` is used only by the in-memory test fixtures; the production paths
// allocate nothing of their own (the shared `lib/procinfo` renderer owns the
// row `String`).
#[cfg(test)]
extern crate alloc;

pub mod client;
pub mod command;
pub mod error;

pub use client::{run, USAGE};
pub use command::{parse, Command};
pub use error::PsError;
pub use rustos_procinfo::{Output, Transport};
