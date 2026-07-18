//! TAIRiX `tail` — output the last part of files
//! (`plans/APPS.md` §12.1 Stage C).
//!
//! The GNU coreutils `tail`: print the last 10 lines of each named file (or
//! standard input), or the count `-n`/`-c` select — lines or bytes. A leading
//! `+` on the count means "start at unit N" (from the beginning) rather than
//! "the last N". Multi-file output carries the `==> name <==` headers,
//! controlled by `-q`/`-v`; `-z` switches the line delimiter to NUL; the
//! obsolete `{+,-}COUNT[bcl]` first-argument form is honoured. `-h`/`-?`
//! render the tool's own short help from its bundled `Help/` tree through the
//! shared `lib/help` engine (plans/APPS.md §4).
//!
//! # What this crate is
//!
//! A **stream-selecting state machine**, not a data source. For one parsed
//! [`Command`] it pulls bytes from each source in fixed-size chunks and
//! writes the selected part; the last-N modes retain only the window the
//! semantics require (a byte ring / a line queue, from the shared
//! `lib/util` [`tailwindow`](tairix_util::tailwindow)), and the from-start
//! modes stream after skipping the leading units. The operations that touch
//! the outside world are the injected seams:
//!
//! * [`FileSource`] — read a byte range of a named file.
//! * [`Input`] — read the next bytes of standard input.
//! * [`Output`] — write to standard output, and diagnostics to standard
//!   error.
//! * [`Info`] — emit an advisory record to the standard information stream
//!   (fd 3).
//! * [`tairix_help::HelpSource`] — the tool's own `Help/` tree, read by the
//!   short-help switches.
//!
//! The binary that ships as `tail` wires the real syscall-backed filesystem,
//! console input, standard output, and fd 3; tests wire in-memory fixtures.
//! This is the seam discipline of the other userland crates (`head`'s
//! `FileSource`/`Input`/`Output`, `ls`'s advisory `info`).
//!
//! # Follow mode
//!
//! The full GNU follow family (`-f`, `-F`, `--follow[=descriptor|name]`,
//! `--retry`, `--pid`, `--sleep-interval`, `--max-unchanged-stats`, and the
//! obsolete trailing `f`) is implemented. A follow keeps each source open
//! and re-emits appended data as the file grows, blocking off-CPU on the
//! kernel's file-change wait source ([`WaitSourceKind::File`] over a
//! wait-set) between emissions — never a busy poll. `-f` follows by
//! descriptor (a rename keeps following the same node); `-F` follows by
//! name (watching the parent directory so a log rotation reopens the new
//! file), detects truncation (the file shrinking) and rotation, and
//! `--retry`s a name that is not yet present. `--pid` ends the follow when
//! the given process dies, re-checked each `--sleep-interval`. The follow
//! engine reaches the outside world through the [`Watcher`] seam, so it too
//! runs against an in-memory fake with no kernel.
//!
//! [`WaitSourceKind::File`]: tairix_abi::WaitSourceKind::File
//!
//! # Fail closed
//!
//! An unknown option or invalid count is a typed [`TailError`] that reads
//! nothing; a source that cannot be read is diagnosed on standard error with
//! the run continuing to the next operand (the GNU model), and the run's
//! success reflects it; a failed output write is fatal. The fd-3 advisory is
//! ignorable by contract and never affects the output or the exit status.
//! There is no partial-guess path and no panic.
//!
//! # Module map
//!
//! * [`error`] — [`TailError`], the outcomes of [`parse`] and [`run`].
//! * [`command`] — the [`Command`]/[`Job`]/[`Source`] shapes and their
//!   [`parse`]r.
//! * [`io`] — the [`FileSource`], [`Input`], [`Output`], and [`Info`] seams.
//! * [`client`] — the [`run`] entry point and the selection engine.
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
//! vocabulary, the shared `lib/help` engine, and the shared `lib/util` count
//! parser and rolling last-N windows (the `head` tool builds on the same
//! two), so this userland tool never links a kernel or driver crate. No
//! `unsafe`, and no `unwrap`/`expect`/`panic!` in production paths.

#![no_std]
#![forbid(unsafe_code)]
#![deny(missing_docs)]

extern crate alloc;

pub mod client;
pub mod command;
pub mod error;
pub mod follow;
pub mod io;

pub use client::{run, USAGE};
pub use command::{parse, Command, Count, Follow, HeaderMode, Job, Source};
pub use error::TailError;
pub use io::{FileSource, Info, Input, Meta, Output, Watcher};
