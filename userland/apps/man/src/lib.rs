//! RustOS `man` — render a command's bundled help (plans/APPS.md §7).
//!
//! RustOS ships no troff/roff man pages and no `/usr/share/man` tree: every
//! program is an application bundle whose single internationalised `Help/`
//! tree holds one structured-Markdown document per command or topic. `man
//! <cmd>` resolves `<cmd>` through the **same** store-then-`PATH` policy the
//! shell launches by (`lib/cmdres`), so the page shown always documents the
//! program the shell would run, then renders that bundle's document in the
//! active locale through the one shared help engine (`lib/help`).
//!
//! # What this crate is
//!
//! A thin front end over `lib/cmdres` and `lib/help`. For one parsed
//! [`Command`] it resolves the owning bundle, loads the document down the
//! deterministic locale-fallback chain, emits a `stdinfo` advisory when the
//! served locale is not the requested one, and writes the rendered page —
//! a screenful at a time on an interactive terminal — to standard output.
//! The [`BundleStore`] and [`Console`] seams keep every effect injectable,
//! so the whole engine is exhaustively testable without a kernel; the
//! binary that ships as `man` wires them to the real `fs_*` and
//! standard-stream syscalls.
//!
//! # Module map
//!
//! * [`error`] — [`ManError`], the outcomes of [`run`].
//! * [`command`] — the [`Command`] enum and its argument [`parse`]r.
//! * [`io`] — the [`BundleStore`] / [`Console`] seams.
//! * [`source`] — [`ScopedHelp`], the engine's per-bundle read seam.
//! * [`client`] — the [`run`] entry point, resolution, and the pager.
//! * [`help`] — the bundle's own embedded `Help/` tree, the one source the
//!   image builder and test fixtures plant on `/System/Apps/man.app/`.
//!
//! # Layering & safety
//!
//! `no_std` + `alloc`; the dependencies are the audited `lib/abi` ABI
//! crate and the shared `lib/cmdres` / `lib/help` / `lib/vt` engines, so
//! this userland tool never links a kernel or driver crate and defines no
//! second resolution policy, locale walker, or escape vocabulary. No
//! `unsafe`, and no `unwrap`/`expect`/`panic!` in production paths; help
//! content is parsed under the engine's fail-closed bounds even though it
//! is signed.

#![no_std]
#![forbid(unsafe_code)]
#![deny(missing_docs)]

extern crate alloc;

pub mod client;
pub mod command;
pub mod error;
pub mod help;
pub mod io;
pub mod source;

pub use client::{run, Request, USAGE};
pub use command::{parse, Command};
pub use error::ManError;
pub use io::{BundleStore, Console};
pub use source::ScopedHelp;

#[cfg(test)]
mod tests;
