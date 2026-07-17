//! TAIRiX `printf` — format and print data
//! (`plans/APPS.md` §12.1 Stage C).
//!
//! The GNU coreutils `printf`: print ARGUMENTs under the control of
//! FORMAT — literal text, backslash escapes (`\c` ends all output), and
//! `%` directives (`diouxX`, `eEfFgGaA`, `c`, `s`, `b`, `q`, `%%`) with
//! the C flags, width, and precision (both settable to `*`). The FORMAT
//! is reused until every ARGUMENT is consumed; conversion problems are
//! diagnosed and the run continues with exit status `1`; an invalid
//! conversion specification or malformed escape is fatal. `-h`/`-?`/
//! `--help` as the first argument render the tool's own short help from
//! its bundled `Help/` tree through the shared `lib/help` engine
//! (plans/APPS.md §4).
//!
//! # What this crate is
//!
//! A **pure template engine**: for one parsed [`Command`] it walks the
//! FORMAT, converts arguments, and writes bytes. The operations that
//! touch the outside world are the injected seams:
//!
//! * [`Output`] — one instance each for standard output and standard
//!   error (the diagnostics stream).
//! * [`tairix_help::HelpSource`] — the tool's own `Help/` tree, read by
//!   the short-help switches.
//!
//! The binary that ships as `printf` wires the real inherited standard
//! streams; tests wire in-memory fixtures. This is the seam discipline
//! of the other userland crates (`seq`'s `Output`, `head`'s
//! `FileSource`).
//!
//! # Fail closed
//!
//! A missing FORMAT, an invalid conversion specification, and a
//! malformed escape are typed [`PrintfError`]s with GNU's diagnostics; a
//! misconvertible argument is diagnosed on standard error with the run
//! continuing (the GNU model); a failed output write is fatal. There is
//! no guess path and no panic.
//!
//! # Module map
//!
//! * [`error`] — [`PrintfError`], the fatal outcomes.
//! * [`command`] — the [`Command`] shape and its [`parse`]r.
//! * [`number`] — C-locale numeric argument conversion over the shared
//!   `tairix_util::cnum` scanner.
//! * [`template`] — the FORMAT walk: escapes and directives, rendering
//!   floats through the shared `tairix_util::cfloat` engine.
//! * [`quote`] — `%q` shell quoting.
//! * [`client`] — the [`run`] loop: format reuse, diagnostics, status.
//!
//! The bundle's `Help/` documents are **not** embedded in this crate:
//! they are authored once in the bundle's on-disk `Help/` tree, planted
//! onto `/System` by the image builder from that source
//! (`tools/syshelp`), and read back at runtime through the injected
//! [`tairix_help::HelpSource`] seam. Help is never hardcoded into the
//! program (`plans/APPS.md`).
//!
//! # Layering & safety
//!
//! `no_std` (with `alloc`); the dependencies are the shared `lib/help`
//! engine and `lib/util`'s C-locale number engines, so this userland
//! tool never links a kernel or driver crate. No `unsafe`, and no
//! `unwrap`/`expect`/`panic!` in production paths.

#![no_std]
#![forbid(unsafe_code)]
#![deny(missing_docs)]

extern crate alloc;

pub mod client;
pub mod command;
pub mod error;
pub mod number;
pub mod quote;
pub mod template;

pub use client::{run, Output, USAGE};
pub use command::{parse, Command};
pub use error::PrintfError;
