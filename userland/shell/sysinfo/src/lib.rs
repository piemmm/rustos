//! TAIRiX `sysinfo` — the terminal client of the System Information API
//! (Stage 6).
//!
//! TAIRiX has no `/proc` and no `/sys`. Every piece of live system
//! information is exposed through the typed, versioned, capability-checked
//! `sysinfo-v1` API served by `/System/Services/sysinfod.app/Run`. `sysinfo` is the
//! single command-line tool that exposes that same API to the terminal: it
//! does **not** open files in a virtual filesystem, and it has no privileged
//! path that bypasses the capability check.
//!
//! # What this crate is
//!
//! A **request/render state machine**, not a data source. For one parsed
//! [`Command`] it:
//!
//! 1. Builds the typed [`SysinfoRequestHeader`](tairix_abi::sysinfo::SysinfoRequestHeader)
//!    and any typed request payload from the `sysinfo-v1` ABI.
//! 2. Hands the request bytes to the injected [`Transport`] seam, which
//!    carries them to `sysinfod` and returns the reply bytes.
//! 3. Decodes the typed response with the ABI's fail-closed `from_bytes`
//!    decoders and renders human-readable lines to the injected [`Output`]
//!    seam.
//!
//! The capability gate lives in `sysinfod`, not here: a denied query comes
//! back as [`Errno::PermissionDenied`](tairix_abi::Errno::PermissionDenied),
//! which the CLI renders honestly without inventing a parallel policy —
//! naming the capability the frozen query registry declares, so the user
//! learns which grant to ask for.
//!
//! # Reading a resource reference
//!
//! [`Command::Show`] and [`Command::Describe`] read one `info:`/`state:`/`stats:`
//! *resource reference* (`plans/ALIAS.md` §15.4). Those namespaces are
//! value-backed: typed values served through this API, never kernel byte
//! streams, so the kernel resource resolver opens no descriptor on one and
//! every reader goes through the broker.
//!
//! This tool is one such reader, not the only one: the shell's read
//! redirection (`cat < info:mem/physical`) and a tool reading the reference
//! as an operand (`cat info:mem/physical`) both resolve through the same
//! `lib/procinfo` resolver and render with the same
//! [`display_value`](tairix_procinfo::ResourceResponse::display_value), so a
//! value captured any of the three ways is byte-identical. Every one spells
//! the reference with the shared parser (`lib/resref`), so there is no second
//! grammar, no second resolver, and no path around the broker.
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
//! * [`client`] — the [`run`] entry point and the response renderers,
//!   including the `show`/`describe` resource-reference readers.
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
pub use tairix_procinfo::{Output, Transport};
