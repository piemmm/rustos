//! RustOS shared System Information API client helpers (Stage 6,
//! `AGENTS.md` §6 / §16.6).
//!
//! RustOS has no `/proc` and no `/sys`: every piece of live system
//! information is read through the typed, versioned, capability-checked
//! `sysinfo-v1` API served by `/System/Services/sysinfod` (`AGENTS.md`
//! §16.6). Two terminal tools speak that API — the umbrella `sysinfo`
//! command and the POSIX-named `ps` — and they
//! share the same request envelope, the same capability-aware call mapping,
//! and the same process-list paging and row rendering. Sibling userland
//! crates may not depend on one another (`AGENTS.md` §17.4), so that shared
//! shape lives here, in one place, rather than being copied (`AGENTS.md`
//! §2.2).
//!
//! # What this crate is
//!
//! A small, dependency-light client toolkit, **not** a data source. It
//! provides:
//!
//! * The [`Transport`] and [`Output`] seams through which a tool issues a
//!   request and writes a line. Keeping them behind object-safe traits is
//!   what lets the consuming tools run against in-memory fixtures with no
//!   kernel, mirroring the seam design of the other userland crates.
//! * [`encode_request`], which frames a
//!   [`SysinfoQueryId`](rustos_abi::sysinfo::SysinfoQueryId) and its typed
//!   payload into the `sysinfo-v1`
//!   [`SysinfoRequestHeader`](rustos_abi::sysinfo::SysinfoRequestHeader)
//!   envelope, and [`call`], which issues a request and maps a capability
//!   denial onto the distinguished [`CallError::PermissionDenied`].
//! * [`for_each_process`], the paged process-list walk, plus
//!   [`PROCESS_HEADER`], [`render_process`], and [`state_char`] — the
//!   shared columnar rendering.
//!
//! Each consuming tool keeps its own argument grammar, usage banner, and
//! error enum; this crate owns only the parts they would otherwise
//! duplicate.
//!
//! # Module map
//!
//! * [`transport`] — the [`Transport`] and [`Output`] seams.
//! * [`request`] — [`encode_request`], [`call`], and [`CallError`].
//! * [`process`] — the process-list paging walk and row rendering.
//!
//! # Layering & safety
//!
//! `no_std` (with `alloc`, `AGENTS.md` §6); the only dependency is the
//! audited `lib/abi` crate, so this helper never links a kernel or driver
//! crate (`AGENTS.md` §17.4). No `unsafe`, and no `unwrap`/`expect`/`panic!`
//! in production paths (`AGENTS.md` §2.9).

#![no_std]
#![forbid(unsafe_op_in_unsafe_fn)]
#![deny(missing_docs)]

extern crate alloc;

pub mod process;
pub mod request;
pub mod transport;

pub use process::{
    for_each_process, render_process, state_char, ProcessListError, PROCESS_HEADER, PROCESS_PAGE,
};
pub use request::{call, encode_request, CallError};
pub use transport::{Output, Transport};
