//! RustOS `cat` — concatenate files and standard input to the terminal
//! (Stage 6, `AGENTS.md` §3 `userland/apps/`).
//!
//! `cat` reads each of its sources in order and writes the bytes to the
//! terminal. A source is either a path or standard input (the `-` operand,
//! and the default when no operand is given). With `-n` it numbers the
//! output lines, continuously across every source — the same model as the
//! POSIX tool.
//!
//! # What this crate is
//!
//! A **stream/render state machine**, not a data source. For one parsed
//! [`Command`] it pulls bytes from each source in fixed-size chunks and
//! writes them — optionally line-numbered — to the terminal. The two
//! operations that touch the outside world are the injected seams:
//!
//! * [`FileSource`] — read a byte range of a named file.
//! * [`Input`] — read the next bytes of standard input.
//! * [`Output`] — write rendered bytes to the terminal.
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
//! [`CatError::Output`]. There is no partial-guess path and no panic
//! (`AGENTS.md` §2.9).
//!
//! # Module map
//!
//! * [`error`] — [`CatError`], the outcomes of [`run`].
//! * [`command`] — the [`Command`]/[`Source`] shapes and their [`parse`]r.
//! * [`io`] — the [`FileSource`], [`Input`], and [`Output`] seams.
//! * [`client`] — the [`run`] entry point and the streaming/numbering engine.
//!
//! # Layering & safety
//!
//! `no_std` (with `alloc`, `AGENTS.md` §6); the only dependency is the
//! audited `lib/abi` crate, so this userland tool never links a kernel or
//! driver crate (`AGENTS.md` §17.4). No `unsafe`, and no
//! `unwrap`/`expect`/`panic!` in production paths (`AGENTS.md` §2.9).

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
