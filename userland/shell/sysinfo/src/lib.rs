//! RustOS `sysinfo` — the terminal client of the System Information API
//! (Stage 6).
//!
//! RustOS has no `/proc` and no `/sys`. Every piece of live system
//! information is exposed through the typed, versioned, capability-checked
//! `sysinfo-v1` API served by `/System/Services/sysinfod`. `sysinfo` is the
//! single command-line tool that exposes that same API to the terminal: it
//! does **not** open files in a virtual filesystem, and it has no privileged
//! path that bypasses the capability check.
//!
//! # What this crate is
//!
//! A **request/render state machine**, not a data source. For one parsed
//! [`Command`] it:
//!
//! 1. Builds the typed [`SysinfoRequestHeader`](rustos_abi::sysinfo::SysinfoRequestHeader)
//!    and any typed request payload from the `sysinfo-v1` ABI.
//! 2. Hands the request bytes to the injected [`Transport`] seam, which
//!    carries them to `sysinfod` and returns the reply bytes.
//! 3. Decodes the typed response with the ABI's fail-closed `from_bytes`
//!    decoders and renders human-readable lines to the injected [`Output`]
//!    seam.
//!
//! The capability gate lives in `sysinfod`, not here: a denied query comes
//! back as [`Errno::PermissionDenied`](rustos_abi::Errno::PermissionDenied),
//! which the CLI renders honestly without inventing a parallel policy.
//!
//! The two operations that touch the outside world — issuing the request and
//! writing the terminal — are the injected [`Transport`] and [`Output`]
//! seams. The binary that ships as the `sysinfo` utility wires the real
//! IPC-backed transport and console; tests wire in-memory fixtures. This
//! mirrors the seam design of the other userland crates and keeps the
//! request/render logic independent of kernel plumbing and exhaustively
//! testable.
//!
//! # Module map
//!
//! * [`error`] — [`SysinfoError`], the outcomes of [`run`].
//! * [`command`] — the [`Command`] enum and its argument [`parse`]r.
//! * [`client`] — the [`run`] entry point and the response renderers.
//!
//! The [`Transport`] and [`Output`] seams, the request framing, and the
//! process-list paging and rendering are shared with the `ps` tool through
//! `lib/procinfo` rather than duplicated here.
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

extern crate alloc;

pub mod client;
pub mod command;
pub mod error;

pub use client::{run, USAGE};
pub use command::{parse, Command};
pub use error::SysinfoError;
pub use rustos_procinfo::{Output, Transport};
