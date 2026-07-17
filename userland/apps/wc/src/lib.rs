//! TAIRiX `wc` — print newline, word, and byte counts for each file
//! (`plans/APPS.md` §12.1 Stage C).
//!
//! The GNU coreutils `wc`: count lines, words, characters, bytes, and the
//! maximum line display width of each named file (or standard input),
//! print one aligned row per input in the fixed order lines/words/chars/
//! bytes/max-line-length, and a `total` row per `--total`. The operand
//! list can come NUL-separated from `--files0-from=F`. `-h`/`-?` render
//! the tool's own short help from its bundled `Help/` tree through the
//! shared `lib/help` engine (plans/APPS.md §4).
//!
//! # What this crate is
//!
//! A **streaming counter and row formatter**, not a data source. For one
//! parsed [`Command`] it pulls bytes from each input in fixed-size chunks
//! through the injected seams and decodes them incrementally
//! ([`counter`]); constant memory per input. The operations that touch
//! the outside world are the seams:
//!
//! * [`FileSource`] — read a byte range (and probe the size) of a named
//!   file.
//! * [`Input`] — read the next bytes of standard input.
//! * [`Output`] — write to standard output, and diagnostics to standard
//!   error.
//! * [`tairix_help::HelpSource`] — the tool's own `Help/` tree, read by
//!   the short-help switches.
//!
//! The binary that ships as `wc` wires the real syscall-backed
//! filesystem, console input, and console output; tests wire in-memory
//! fixtures. This is the seam discipline of the other userland crates
//! (`cat`'s and `head`'s `FileSource`/`Input`/`Output`).
//!
//! # Fail closed
//!
//! An unknown option or malformed value is a typed [`WcError`] that reads
//! nothing; an input that cannot be read is diagnosed on standard error
//! with the run continuing to the next input (the GNU model), and the
//! run's success reflects it; a failed output write is fatal. There is no
//! partial-guess path and no panic.
//!
//! # Module map
//!
//! * [`error`] — [`WcError`], the outcomes of [`parse`] and [`run`].
//! * [`command`] — the [`Command`]/[`Job`]/[`Selection`] shapes and their
//!   [`parse`]r.
//! * [`counter`] — the streaming five-count engine ([`Counter`]/
//!   [`Counts`]).
//! * [`io`] — the [`FileSource`], [`Input`], and [`Output`] seams.
//! * [`client`] — the [`run`] entry point: input assembly, GNU column
//!   widths, rows, and totals.
//!
//! The bundle's `Help/` documents are **not** embedded in this crate: they
//! are authored once in the bundle's on-disk `Help/` tree, planted onto
//! `/System` by the image builder from that source (`tools/syshelp`), and
//! read back at runtime through the injected [`tairix_help::HelpSource`]
//! seam. Help is never hardcoded into the program (`plans/APPS.md`).
//!
//! # Layering & safety
//!
//! `no_std` (with `alloc`); the dependencies are the audited `lib/abi`
//! vocabulary, the shared `lib/help` engine, and the one `lib/vt` display
//! width definition, so this userland tool never links a kernel or driver
//! crate. No `unsafe`, and no `unwrap`/`expect`/`panic!` in production
//! paths.

#![no_std]
#![forbid(unsafe_code)]
#![deny(missing_docs)]

extern crate alloc;

pub mod client;
pub mod command;
pub mod counter;
pub mod error;
pub mod io;

pub use client::{run, USAGE};
pub use command::{parse, Command, Job, Operand, Selection, TotalMode};
pub use counter::{Counter, Counts};
pub use error::WcError;
pub use io::{FileSource, Input, Output, SizeProbe};
