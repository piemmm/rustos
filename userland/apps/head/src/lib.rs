//! RustOS `head` — output the first part of files
//! (`plans/APPS.md` §12.1 Stage C).
//!
//! The GNU coreutils `head`: print the first 10 lines of each named file
//! (or standard input), or the count `-n`/`-c` select — lines or bytes,
//! with a leading `-` on the count meaning "everything but the last COUNT".
//! Multi-file output carries the `==> name <==` headers, controlled by
//! `-q`/`-v`; `-z` switches the line delimiter to NUL; the obsolete
//! `-COUNT[bkm][lqvz]` first-argument form is honoured. `-h`/`-?` render
//! the tool's own short help from its bundled `Help/` tree through the
//! shared `lib/help` engine (plans/APPS.md §4).
//!
//! # What this crate is
//!
//! A **stream-trimming state machine**, not a data source. For one parsed
//! [`Command`] it pulls bytes from each source in fixed-size chunks and
//! writes the selected part; the elide modes retain only the window the
//! semantics require (a byte ring / a line queue). The operations that
//! touch the outside world are the injected seams:
//!
//! * [`FileSource`] — read a byte range of a named file.
//! * [`Input`] — read the next bytes of standard input.
//! * [`Output`] — write to standard output, and diagnostics to standard
//!   error.
//! * [`rustos_help::HelpSource`] — the tool's own `Help/` tree, read by the
//!   short-help switches.
//!
//! The binary that ships as `head` wires the real syscall-backed
//! filesystem, console input, and console output; tests wire in-memory
//! fixtures. This is the seam discipline of the other userland crates
//! (`cat`'s `FileSource`/`Input`/`Output`, `login`'s `Prompt`).
//!
//! # Fail closed
//!
//! An unknown option or invalid count is a typed [`HeadError`] that reads
//! nothing; a source that cannot be read is diagnosed on standard error
//! with the run continuing to the next operand (the GNU model), and the
//! run's success reflects it; a failed output write is fatal. There is no
//! partial-guess path and no panic.
//!
//! # Module map
//!
//! * [`error`] — [`HeadError`], the outcomes of [`parse`] and [`run`].
//! * [`command`] — the [`Command`]/[`Job`]/[`Source`] shapes and their
//!   [`parse`]r.
//! * [`io`] — the [`FileSource`], [`Input`], and [`Output`] seams.
//! * [`client`] — the [`run`] entry point and the trimming engine.
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
//! vocabulary and the shared `lib/help` engine, so this userland tool never
//! links a kernel or driver crate. No `unsafe`, and no
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
pub use command::{parse, Command, Count, HeaderMode, Job, Source};
pub use error::HeadError;
pub use io::{FileSource, Input, Output};
