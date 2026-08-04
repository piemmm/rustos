//! TAIRiX `man` — render a command's bundled help (plans/APPS.md §7).
//!
//! TAIRiX ships no troff/roff man pages and no `/usr/share/man` tree: every
//! program is an application bundle whose single internationalised `Help/`
//! tree holds one structured-Markdown document per command or topic. `man
//! <cmd>` resolves `<cmd>` through the **same** store-then-`PATH` policy the
//! shell launches by (`lib/cmdres`), so the page shown always documents the
//! program the shell would run, then renders that bundle's document in the
//! active locale through the one shared help engine (`lib/help`).
//!
//! # What this crate is
//!
//! A thin front end over `lib/cmdres`, `lib/help`, and `lib/sandbox`. For
//! one parsed [`Command`] it resolves the owning bundle, locates and reads
//! the document down the deterministic locale-fallback chain
//! (`tairix_help::load_raw`), emits a `stdinfo` advisory when the served
//! locale is not the requested one, and writes the rendered page — a
//! screenful at a time on an interactive terminal — to standard output.
//! The document itself is **never parsed in this process**: it is foreign
//! bundle content, so its parse and render run in a minimum-capability
//! parser-sandbox worker (`tairix_sandbox::helpdoc`) and only the
//! whitelist-validated render comes back — a crashed or hostile worker
//! costs the page, never the tool. The [`BundleStore`] and [`Console`]
//! seams keep every effect injectable and the sandbox runs over an
//! in-process loopback on the host, so the whole engine is exhaustively
//! testable without a kernel; the binary that ships as `man` wires them to
//! the real `fs_*` and standard-stream syscalls and re-spawns itself as
//! the render worker.
//!
//! # Module map
//!
//! * [`error`] — [`ManError`], the outcomes of [`run`].
//! * [`command`] — the [`Command`] enum and its argument [`parse`]r.
//! * [`io`] — the [`BundleStore`] / [`Console`] seams.
//! * [`source`] — [`ScopedHelp`], the engine's per-bundle read seam.
//! * [`client`] — the [`run`] entry point, resolution, and the pager.
//!
//! The bundle's own `Help/` tree is **not** embedded in this crate: it is
//! authored once in the bundle's on-disk `Help/` directory, planted onto
//! `/System/Commands/man.app/` by the image builder from that source
//! (`tools/syshelp`), and read back at runtime through the [`BundleStore`]
//! seam. Help is never hardcoded into the program (`plans/APPS.md`).
//!
//! # Layering & safety
//!
//! `no_std` + `alloc`; the dependencies are the audited `lib/abi` ABI
//! crate and the shared `lib/cmdres` / `lib/help` / `lib/sandbox` /
//! `lib/log` engines, so this userland tool never links a kernel or driver
//! crate and defines no second resolution policy, locale walker, or escape
//! vocabulary. No `unsafe`, and no `unwrap`/`expect`/`panic!` in
//! production paths; help content is parsed under the engine's fail-closed
//! bounds, in the sandbox, even though it is signed.

#![no_std]
#![forbid(unsafe_code)]
#![deny(missing_docs)]

extern crate alloc;

pub mod client;
pub mod command;
pub mod error;
pub mod io;
pub mod source;

pub use client::{run, Request, USAGE};
pub use command::{parse, Command};
pub use error::ManError;
pub use io::{BundleStore, Console};
pub use source::ScopedHelp;

#[cfg(test)]
mod tests;
