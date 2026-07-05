//! RustOS `cat` — concatenate files and standard input to the terminal
//! (Stage 6 `userland/apps/`).
//!
//! `cat` reads each of its sources in order and writes the bytes to the
//! terminal. A source is either a path or standard input (the `-` operand,
//! and the default when no operand is given). With `-n` it numbers the
//! output lines, continuously across every source — the same model as the
//! POSIX tool. `-h`/`-?` render the tool's own short help from its bundled
//! `Help/` tree through the shared `lib/help` engine (plans/APPS.md §4).
//!
//! # What this crate is
//!
//! A **stream/render state machine**, not a data source. For one parsed
//! [`Command`] it pulls bytes from each source in fixed-size chunks and
//! writes them — optionally line-numbered — to the terminal. The
//! operations that touch the outside world are the injected seams:
//!
//! * [`FileSource`] — read a byte range of a named file.
//! * [`Input`] — read the next bytes of standard input.
//! * [`Output`] — write rendered bytes to the terminal.
//! * [`rustos_help::HelpSource`] — the tool's own `Help/` tree, read by the
//!   short-help switches.
//!
//! The binary that ships as `cat` wires the real syscall-backed filesystem,
//! console input, and console output; tests wire in-memory fixtures. This is
//! the seam discipline of the other userland crates (`init`'s
//! `Spawner`/`Reaper`, `login`'s `Prompt`, `sysinfo`'s `Transport`), and it
//! keeps every parsing, streaming, and numbering decision testable without a
//! kernel.
//!
//! # Fail closed
//!
//! An unknown option is a [`CatError::Usage`] that reads nothing; a source
//! that cannot be read surfaces the underlying [`Errno`](rustos_abi::Errno)
//! as [`CatError::Read`] and stops; a failed terminal write is
//! [`CatError::Output`]. There is no partial-guess path and no panic.
//!
//! # Module map
//!
//! * [`error`] — [`CatError`], the outcomes of [`run`].
//! * [`command`] — the [`Command`]/[`Source`] shapes and their [`parse`]r.
//! * [`io`] — the [`FileSource`], [`Input`], and [`Output`] seams.
//! * [`client`] — the [`run`] entry point and the streaming/numbering engine.
//!
//! The bundle's `Help/` documents are **not** embedded in this crate: they
//! are authored once in the bundle's on-disk `Help/` tree, planted onto
//! `/System` by the image builder from that source (`tools/syshelp`), and read
//! back at runtime through the injected [`rustos_help::HelpSource`] seam. Help
//! is never hardcoded into the program (`plans/APPS.md`).
//!
//! # Layering & safety
//!
//! `no_std` (with `alloc`); the dependencies are the audited `lib/abi`
//! vocabulary and the shared `lib/help` engine, so this userland
//! tool never links a kernel or driver crate. No `unsafe`, and no
//! `unwrap`/`expect`/`panic!` in production paths.

#![no_std]
#![forbid(unsafe_op_in_unsafe_fn)]
#![deny(missing_docs)]

extern crate alloc;

pub mod client;
pub mod command;
pub mod error;
pub mod io;

pub use client::{run, USAGE};
pub use command::{parse, Command, Source};
pub use error::CatError;
pub use io::{FileSource, Input, Output};
