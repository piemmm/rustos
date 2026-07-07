//! RustOS `tee` — read from standard input and write to standard output
//! and files (`plans/APPS.md` §12.1 Stage C).
//!
//! The GNU coreutils `tee`: copy standard input to standard output and to
//! each file operand, overwriting (`-a` appends), with `--output-error`
//! selecting how a failed output is treated. `-h`/`-?` render the tool's
//! own short help from its bundled `Help/` tree through the shared
//! `lib/help` engine (plans/APPS.md §4).
//!
//! # What this crate is
//!
//! A **streaming fan-out**, not a data source. For one parsed [`Command`]
//! it pulls bytes from standard input in fixed-size chunks through the
//! injected seams and writes each chunk to every still-live output;
//! constant memory regardless of input size. The operations that touch
//! the outside world are the seams:
//!
//! * [`Input`] — read the next bytes of standard input.
//! * [`FileSink`] — open (create/truncate/append) and sequentially write
//!   the file operands.
//! * [`Output`] — write to standard output, and diagnostics to standard
//!   error.
//! * [`rustos_help::HelpSource`] — the tool's own `Help/` tree, read by
//!   the short-help switches.
//!
//! The binary that ships as `tee` wires the real syscall-backed
//! filesystem, console input, and console output; tests wire in-memory
//! fixtures. This is the seam discipline of the other userland crates
//! (`head`'s and `wc`'s `FileSource`/`Input`/`Output`).
//!
//! # RustOS has no `SIGPIPE`
//!
//! GNU `tee`'s pipe handling keys on `EPIPE`/`SIGPIPE`. Here a consumer
//! going away surfaces as a write error on standard output — the one
//! output of this tool that can be a pipe — so the "pipe" class of the
//! `--output-error` modes maps to exactly that output (documented in the
//! tool's Help; see [`OutputError`]). GNU `tee -i` is deliberately absent:
//! there is no per-process signal disposition to set, so it is staged
//! behind that kernel work, never stubbed (plans/APPS.md §12.1).
//!
//! # Fail closed
//!
//! An unknown option or malformed mode is a typed [`TeeError`] that reads
//! nothing; a failed output is diagnosed and handled per the selected
//! mode (the GNU model), and the run's success reflects it; a failed
//! diagnostic write is fatal. There is no partial-guess path and no
//! panic.
//!
//! # Module map
//!
//! * [`error`] — [`TeeError`], the outcomes of [`parse`] and [`run`].
//! * [`command`] — the [`Command`]/[`Job`]/[`OutputError`] shapes and
//!   their [`parse`]r.
//! * [`io`] — the [`Input`], [`FileSink`], and [`Output`] seams.
//! * [`client`] — the [`run`] entry point: output assembly and the
//!   per-mode failure handling.
//!
//! The bundle's `Help/` documents are **not** embedded in this crate: they
//! are authored once in the bundle's on-disk `Help/` tree, planted onto
//! `/System` by the image builder from that source (`tools/syshelp`), and
//! read back at runtime through the injected [`rustos_help::HelpSource`]
//! seam. Help is never hardcoded into the program (`plans/APPS.md`).
//!
//! # Layering & safety
//!
//! `no_std` (with `alloc`); the dependencies are the audited `lib/abi`
//! vocabulary and the shared `lib/help` engine, so this userland tool
//! never links a kernel or driver crate. No `unsafe`, and no
//! `unwrap`/`expect`/`panic!` in production paths.

#![no_std]
#![forbid(unsafe_code)]
#![deny(missing_docs)]

extern crate alloc;

pub mod client;
pub mod command;
pub mod error;
pub mod io;

pub use client::{run, USAGE};
pub use command::{parse, Command, Job, OutputError};
pub use error::TeeError;
pub use io::{FileSink, Input, Output};
