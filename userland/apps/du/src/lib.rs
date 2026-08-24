//! TAIRiX `du` — estimate file space usage (Stage 6 `userland/apps/`, a
//! `plans/APPS.md` command app).
//!
//! `du` walks each of its path operands and reports, per directory (post-order,
//! deepest first), the on-disk storage the tree beneath it occupies. With no
//! operand it walks the current directory (`.`). The option surface follows GNU
//! coreutils: `-a` adds a row per file, `-s` reports only the operands, `-c`
//! appends a grand total, `-d` bounds the reported depth, `-S` excludes
//! subdirectories from a directory's own row, `-l` counts a multiply-named file
//! once per name, `--apparent-size`/`-b` measure apparent byte lengths, and
//! `-k`/`-m`/`-h`/`--si`/`-B <size>` select the scale. `-?`/`--help` render the
//! tool's own short help from its bundled `Help/` tree through the shared
//! `lib/help` engine (plans/APPS.md §4).
//!
//! # What this crate is
//!
//! A **usage-summing engine**, not a data source. For one parsed
//! [`Command`] it asks the injected filesystem seam for each node's
//! metadata and each directory's entries, folds the sizes bottom-up with
//! an explicit frame stack (a deep tree can never exhaust the call
//! stack), and writes the `size<TAB>path` rows. The operations that touch
//! the outside world are the injected seams:
//!
//! * [`Walk`] — stat a path and read a directory's entries.
//! * [`Output`] — the standard output rows and standard error
//!   diagnostics.
//! * [`tairix_help::HelpSource`] — the tool's own `Help/` tree, read by
//!   the short-help switches.
//!
//! The binary that ships as `du` wires the real syscall-backed filesystem
//! and standard streams; tests wire in-memory fixtures. This is the seam
//! discipline of the other userland crates (`ls`'s `Listing`, `wc`'s
//! `FileSource`).
//!
//! # Measurement
//!
//! The default measure is each node's **allocated** on-disk bytes — the
//! `fs_stat` `allocated` field the mounted format reports from its own
//! accounting — so a sparse or compressed file reports what it really
//! occupies. `--apparent-size` (and `-b`) measure the apparent byte
//! length instead. Block counts round **up** (a partially used block is a
//! used block), through the one shared GNU size vocabulary in
//! `lib/util` (`tairix_util::size`) that `df` uses too.
//!
//! A file reached through more than one name is summed **once**, as the
//! GNU tool does; `-l`/`--count-links` opts out and counts every name. The
//! key is the node identity each `fs_readdir`/`fs_stat` record carries, and
//! only a node whose name count exceeds one is remembered — a node named
//! once cannot be reached twice — so the seen-set holds the hard links the
//! walk meets rather than one entry per node on the volume. It grows on
//! demand from no fixed ceiling; a heap that refuses to grow it makes the
//! walk count every name and say so on standard error (exit `1`), never
//! silently.
//!
//! # Fail loud, degrade gracefully
//!
//! A path that cannot be statted or a directory that cannot be listed is
//! diagnosed on standard error and the walk continues (exit status `1`),
//! exactly as the GNU tool; an unreadable directory contributes nothing
//! rather than a guessed partial sum. Only a failure to write output is
//! fatal. An unknown option is a parse error that reports usage and exits
//! `2`. There is no panic path.
//!
//! # Module map
//!
//! * [`error`] — [`DuError`], the outcomes of [`run`].
//! * [`command`] — the [`Command`]/[`Options`] shapes and their
//!   [`parse`]r.
//! * [`io`] — the [`Walk`] and [`Output`] seams and the
//!   [`Entry`]/[`Metadata`] data they carry.
//! * [`client`] — the [`run`] entry point and the walk engine.
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
//! `no_std` (with `alloc`); the dependencies are the audited `lib/abi`
//! vocabulary, the shared `lib/help` engine, and the shared `lib/util`
//! size vocabulary, so this userland tool never links a kernel or driver
//! crate. No `unsafe`, and no `unwrap`/`expect`/`panic!` in production
//! paths.

#![no_std]
#![forbid(unsafe_code)]
#![deny(missing_docs)]

extern crate alloc;

pub mod client;
pub mod command;
pub mod error;
pub mod io;

pub use client::{run, USAGE};
pub use command::{parse, Command, Options, ParseError};
pub use error::DuError;
pub use io::{Entry, Metadata, Output, Walk};
