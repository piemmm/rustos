//! TAIRiX `rm` — remove files and directories (Stage 6
//! `userland/apps/`).
//!
//! `rm` removes each of its operands in order. A non-directory operand (a
//! regular file, a symbolic link, a device node, …) is removed directly. A
//! directory operand is removed only with `-r`, which removes its contents
//! first, depth-first, and then the directory itself; without `-r`, naming a
//! directory is a [`RmError::IsDirectory`]. With `-f` a non-existent operand
//! is skipped rather than reported. This is the POSIX model.
//!
//! # What this crate is
//!
//! A **removal engine**, not a data source. For one parsed [`Command`] it asks
//! the injected filesystem seam for the kind of each operand, walks each
//! directory it must descend, and removes every reachable object. The
//! operations that touch the outside world are the injected seams:
//!
//! * [`Removal`] — learn a path's kind, read a directory's entries by index,
//!   and remove a file or an (emptied) directory.
//! * [`Output`] — write the usage banner to the terminal.
//!
//! The binary that ships as `rm` wires the real syscall-backed filesystem and
//! console output; tests wire in-memory fixtures. This is the seam discipline
//! of the other userland crates (`init`'s `Spawner`/`Reaper`, `login`'s
//! `Prompt`, `sysinfo`'s `Transport`, `cat`'s `FileSource`, `ls`'s
//! `Listing`), and it keeps every parsing, recursion, and force decision
//! testable without a kernel.
//!
//! # Fail closed
//!
//! An unknown option is a [`RmError::Usage`] that removes nothing; a directory
//! named without `-r` is a [`RmError::IsDirectory`]; an operand that cannot be
//! inspected surfaces the underlying [`Errno`](tairix_abi::Errno) as
//! [`RmError::Stat`] (unless `-f` makes a [`NotFound`](tairix_abi::Errno::NotFound)
//! a no-op); a directory that cannot be enumerated is [`RmError::Read`]; a
//! removal that fails is [`RmError::Remove`]. The first failure stops the run
//! before any later operand, and there is no panic.
//!
//! # Module map
//!
//! * [`error`] — [`RmError`], the outcomes of [`run`].
//! * [`command`] — the [`Command`] shape and its [`parse`]r.
//! * [`io`] — the [`Removal`] and [`Output`] seams and the
//!   [`Entry`]/[`EntryKind`] data they carry.
//! * [`client`] — the [`run`] entry point and the recursive removal engine.
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
pub use command::{parse, Command, Interactive, Options};
pub use error::RmError;
pub use io::{Entry, EntryKind, Output, Prompt, Removal};
