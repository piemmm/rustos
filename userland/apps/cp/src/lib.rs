//! TAIRiX `cp` — copy files and directories (Stage 6
//! `userland/apps/`).
//!
//! `cp` copies each of its source operands to a destination. With a single
//! source and a destination that does not name a directory, the source is
//! copied to that exact path. When the destination names an existing
//! directory — and always when there is more than one source — each source is
//! copied *into* that directory under its own base name. A directory source
//! is copied only with `-r`, which reproduces the whole subtree; without `-r`
//! a directory operand is a [`CpError::IsDirectory`]. This is the POSIX
//! model.
//!
//! # Links, instead of bytes
//!
//! `-l` gives the destination a second *name* for the source's own node
//! rather than a copy of its bytes, so the two cannot diverge on a later
//! write; `-s` makes a symbolic link naming the source. They are
//! alternatives to copying rather than modifiers of it, so they are one
//! last-wins [`Contents`] state that cannot silently combine.
//!
//! `-P` reproduces a symbolic-link source as a link storing the same target,
//! verbatim — so a relative or dangling link survives the copy — instead of
//! following it. `--preserve=links` gives two sources naming one node two
//! names at the destination rather than two copies, keyed on the identity
//! every `fs_stat` and `fs_readdir` record carries; only a node whose name
//! count exceeds one is remembered, so the map holds the hard links a copy
//! meets rather than one entry per node on the volume. `-d` is exactly that
//! pair, spelled as the pair so its halves cannot drift.
//!
//! `-a` and the `--preserve` set other than `--preserve=links` are refused
//! ([`CpError::Unsupported`]): `-a` is `-dR --preserve=all`, and
//! `--preserve=all` includes timestamps no call can set here, so honouring
//! it would report a preservation that did not happen. `-dR` is the rest of
//! it and is available.
//!
//! # What this crate is
//!
//! A **copy engine**, not a data source. For one parsed [`Command`] it asks
//! the injected filesystem seam for the kind of each operand, streams regular
//! files from source to destination, and walks each directory it must
//! reproduce. The operations that touch the outside world are the injected
//! seams:
//!
//! * [`FileSystem`] — probe a path (under either follow posture), read a
//!   file's bytes and a directory's entries, create directories, files and
//!   bytes, make a symbolic link or a second name, and remove a destination
//!   for `-f`.
//! * [`Output`] — write the usage banner to the terminal.
//!
//! The binary that ships as `cp` wires the real syscall-backed filesystem and
//! console output; tests wire in-memory fixtures. This is the seam discipline
//! of the other userland crates (`init`'s `Spawner`/`Reaper`, `login`'s
//! `Prompt`, `sysinfo`'s `Transport`, `cat`'s `FileSource`, `ls`'s `Listing`,
//! `rm`'s `Removal`), and it keeps every parsing, recursion, and force
//! decision testable without a kernel.
//!
//! # Fail closed
//!
//! An unknown option, a missing operand, or more than one source aimed at a
//! non-directory destination is a [`CpError::Usage`] that copies nothing; a
//! directory source named without `-r` is a [`CpError::IsDirectory`]; a
//! directory source whose destination already exists as a non-directory is a
//! [`CpError::NotADirectory`]; an operand that cannot be inspected surfaces
//! the underlying [`Errno`](tairix_abi::Errno) as [`CpError::Stat`]; a file or
//! directory that cannot be read is [`CpError::Read`]; a destination that
//! cannot be created is [`CpError::Create`]; a failed write is
//! [`CpError::Write`]. The first failure stops the run before any later
//! operand, and there is no panic.
//!
//! # Module map
//!
//! * [`error`] — [`CpError`], the outcomes of [`run`].
//! * [`command`] — the [`Command`] shape and its [`parse`]r.
//! * [`io`] — the [`FileSystem`] and [`Output`] seams and the
//!   [`Entry`]/[`EntryKind`] data they carry.
//! * [`client`] — the [`run`] entry point and the recursive copy engine.
//!
//! # Layering & safety
//!
//! `no_std` (with `alloc`); the only dependency is the
//! audited `lib/abi` crate, so this userland tool never links a kernel or
//! driver crate. No `unsafe`, and no
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
pub use command::{parse, Clobber, Command, Contents, Options, TargetMode};
pub use error::CpError;
pub use io::{Entry, EntryKind, FileSystem, Follow, Output, Probe, Prompt};
