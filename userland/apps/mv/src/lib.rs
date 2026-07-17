//! TAIRiX `mv` — move (rename) files and directories (Stage 6 `userland/apps/`).
//!
//! `mv` relocates each of its source operands to a destination. With a single
//! source and a destination that does not name a directory, the source is
//! moved to that exact path. When the destination names an existing
//! directory — and always when there is more than one source — each source is
//! moved *into* that directory under its own base name. Unlike `cp`, a
//! directory source needs no flag: a directory is moved like any other
//! operand. This is the POSIX model.
//!
//! # What this crate is
//!
//! A **move engine**, not a data source. For one parsed [`Command`] it asks
//! the injected filesystem seam to rename each source onto its destination.
//! A rename within one filesystem is atomic and is the whole operation. A
//! rename that would cross a filesystem boundary cannot be atomic, so the
//! seam reports [`RenameOutcome::CrossDevice`] and the engine falls back to
//! the POSIX relocation: copy the source's bytes (recursively, for a
//! directory) to the destination, then remove the source. The operations
//! that touch the outside world are the injected seams:
//!
//! * [`FileSystem`] — learn a path's kind, rename a path, read a file's bytes
//!   and a directory's entries, create directories/files/bytes, and remove
//!   files and directories (for the cross-device relocation and for `-f`).
//! * [`Output`] — write the usage banner to the terminal.
//!
//! The binary that ships as `mv` wires the real syscall-backed filesystem and
//! console output; tests wire in-memory fixtures. This is the seam discipline
//! of the other userland crates (`init`'s `Spawner`/`Reaper`, `login`'s
//! `Prompt`, `sysinfo`'s `Transport`, `cat`'s `FileSource`, `ls`'s `Listing`,
//! `rm`'s `Removal`, `cp`'s `FileSystem`), and it keeps every parsing,
//! routing, and fallback decision testable without a kernel.
//!
//! # Fail closed
//!
//! An unknown option, a missing operand, or more than one source aimed at a
//! non-directory destination is a [`MvError::Usage`] that moves nothing; an
//! operand that cannot be inspected surfaces the underlying
//! [`Errno`](tairix_abi::Errno) as [`MvError::Stat`]; a rename that fails for
//! any reason other than crossing a filesystem boundary is
//! [`MvError::Rename`]; during a cross-device relocation an unreadable source
//! is [`MvError::Read`], an uncreatable destination is [`MvError::Create`], a
//! failed write is [`MvError::Write`], and a source that cannot be removed
//! after a successful copy is [`MvError::Remove`]. The first failure stops the
//! run before any later operand, and there is no panic.
//!
//! # Module map
//!
//! * [`error`] — [`MvError`], the outcomes of [`run`].
//! * [`command`] — the [`Command`] shape and its [`parse`]r.
//! * [`io`] — the [`FileSystem`] and [`Output`] seams and the
//!   [`Entry`]/[`EntryKind`]/[`RenameOutcome`] data they carry.
//! * [`client`] — the [`run`] entry point and the move engine.
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
pub use command::{parse, Clobber, Command, Options, TargetMode};
pub use error::MvError;
pub use io::{Entry, EntryKind, FileSystem, Output, Prompt, RenameOutcome};
