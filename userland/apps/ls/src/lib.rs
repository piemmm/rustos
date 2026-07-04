//! RustOS `ls` — list directory contents (Stage 6
//! `userland/apps/`, a `plans/APPS.md` command app).
//!
//! `ls` inspects each of its path operands in order. A non-directory operand
//! is listed by name; a directory operand has its entries listed, sorted by
//! name. With no operand it lists the current directory (`.`). With `-a` it
//! includes entries whose name begins with `.`; with `-l` it prints the long
//! format — the type and permission bits, the size, then the name — the same
//! model as the POSIX tool. `-h`/`-?` render the tool's own short help from
//! its bundled `Help/` tree through the shared `lib/help` engine
//! (plans/APPS.md §4).
//!
//! # What this crate is
//!
//! A **render state machine**, not a data source. For one parsed [`Command`]
//! it asks the injected filesystem seam for the metadata of each operand and
//! the entries of each directory, then writes the sorted, formatted listing
//! to the terminal. The operations that touch the outside world are the
//! injected seams:
//!
//! * [`Listing`] — stat a path and read a directory's entries.
//! * [`Output`] — write the rendered listing to the terminal and advisory
//!   records to the standard information stream (fd 3).
//! * [`rustos_help::HelpSource`] — the tool's own `Help/` tree, read by the
//!   short-help switches.
//!
//! The binary that ships as `ls` wires the real syscall-backed filesystem and
//! standard streams; tests wire in-memory fixtures. This is the seam
//! discipline of the other userland crates (`init`'s `Spawner`/`Reaper`,
//! `login`'s `Prompt`, `sysinfo`'s `Transport`, `cat`'s `FileSource`, `man`'s
//! `BundleStore`), and it keeps every parsing, filtering, sorting, and
//! formatting decision testable without a kernel.
//!
//! # Advisory output
//!
//! When the default dotfile filter hides entries, `ls` notes the omission on
//! the standard information stream (fd 3) with the canonical
//! `fs.hidden_entries_omitted` record (`AGENTS.md` §20.1) — advisory only,
//! never affecting the listing, ordering, or exit status.
//!
//! # Fail closed
//!
//! An unknown option is a [`LsError::Usage`] that lists nothing; an operand
//! (or, under `-l`, a directory entry) that cannot be stat'd surfaces the
//! underlying [`Errno`](rustos_abi::Errno) as [`LsError::Stat`] and stops; a
//! directory that cannot be read is [`LsError::Read`]; a failed terminal
//! write is [`LsError::Output`]. There is no partial-guess path and no panic.
//!
//! # Module map
//!
//! * [`error`] — [`LsError`], the outcomes of [`run`].
//! * [`command`] — the [`Command`] shape and its [`parse`]r.
//! * [`io`] — the [`Listing`] and [`Output`] seams and the
//!   [`Entry`]/[`Metadata`] data they carry.
//! * [`client`] — the [`run`] entry point and the listing/formatting engine.
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
#![forbid(unsafe_code)]
#![deny(missing_docs)]

extern crate alloc;

pub mod client;
pub mod command;
pub mod error;
pub mod io;

pub use client::{run, USAGE};
pub use command::{parse, Command};
pub use error::LsError;
pub use io::{Entry, Listing, Metadata, Output};
