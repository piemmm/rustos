//! RustOS `ls` — list directory contents (Stage 6, `AGENTS.md` §3
//! `userland/apps/`).
//!
//! `ls` inspects each of its path operands in order. A non-directory operand
//! is listed by name; a directory operand has its entries listed, sorted by
//! name. With no operand it lists the current directory (`.`). With `-a` it
//! includes entries whose name begins with `.`; with `-l` it prints the long
//! format — the type and permission bits, the size, then the name — the same
//! model as the POSIX tool.
//!
//! # What this crate is
//!
//! A **render state machine**, not a data source. For one parsed [`Command`]
//! it asks the injected filesystem seam for the metadata of each operand and
//! the entries of each directory, then writes the sorted, formatted listing
//! to the terminal. The two operations that touch the outside world are the
//! injected seams:
//!
//! * [`Listing`] — stat a path and read a directory's entries by index.
//! * [`Output`] — write the rendered listing to the terminal.
//!
//! The binary that ships as `ls` wires the real syscall-backed filesystem and
//! console output; tests wire in-memory fixtures. This is the seam discipline
//! of the other userland crates (`init`'s `Spawner`/`Reaper`, `login`'s
//! `Prompt`, `sysinfo`'s `Transport`, `cat`'s `FileSource`), and it keeps
//! every parsing, filtering, sorting, and formatting decision testable
//! without a kernel.
//!
//! # Fail closed
//!
//! An unknown option is a [`LsError::Usage`] that lists nothing; an operand
//! that cannot be stat'd surfaces the underlying [`Errno`](rustos_abi::Errno)
//! as [`LsError::Stat`] and stops; a directory that cannot be read is
//! [`LsError::Read`]; a failed terminal write is [`LsError::Output`]. There
//! is no partial-guess path and no panic (`AGENTS.md` §2.9).
//!
//! # Module map
//!
//! * [`error`] — [`LsError`], the outcomes of [`run`].
//! * [`command`] — the [`Command`] shape and its [`parse`]r.
//! * [`io`] — the [`Listing`] and [`Output`] seams and the
//!   [`Entry`]/[`Metadata`]/[`EntryKind`] data they carry.
//! * [`client`] — the [`run`] entry point and the listing/formatting engine.
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
pub use command::{parse, Command};
pub use error::LsError;
pub use io::{Entry, EntryKind, Listing, Metadata, Output};
