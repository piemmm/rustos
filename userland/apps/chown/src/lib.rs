//! RustOS `chown` — change file owner and group (Stage 6, `AGENTS.md` §3
//! `userland/apps/`).
//!
//! `chown` applies an ownership change to each of its file operands. The owner
//! operand is `OWNER`, `OWNER:GROUP`, or `:GROUP`, where `OWNER` and `GROUP`
//! are **decimal** user/group ids: `OWNER` changes only the owning user,
//! `:GROUP` only the owning group, and `OWNER:GROUP` both. With `-R` a
//! directory operand is changed and then its contents are changed recursively.
//! This is the POSIX model, restricted to numeric ids (RustOS has no
//! name-to-id seam in this tool, so a name would be interface creep,
//! `AGENTS.md` §2.4) and a building block of the §5.3 filesystem permission
//! model.
//!
//! # What this crate is
//!
//! An **ownership-changing engine**, not a data source. For one parsed
//! [`Command`] it asks the injected filesystem seam for each operand's kind,
//! applies the new owner, and walks each directory `-R` must descend. The
//! operations that touch the outside world are the injected seams:
//!
//! * [`FileSystem`] — learn a path's kind, set its owner, and read a
//!   directory's entries (for `-R`).
//! * [`Output`] — write the usage banner to the terminal.
//!
//! The binary that ships as `chown` wires the real syscall-backed filesystem
//! and console output; tests wire in-memory fixtures. This is the seam
//! discipline of the other userland crates (`init`'s `Spawner`/`Reaper`,
//! `login`'s `Prompt`, `sysinfo`'s `Transport`, `cat`'s `FileSource`, `ls`'s
//! `Listing`, `rm`'s `Removal`, `cp`'s and `mv`'s `FileSystem`, `chmod`'s
//! `FileSystem`), and it keeps every parsing, owner-spec, and recursion
//! decision testable without a kernel.
//!
//! # Fail closed
//!
//! An unknown option or a missing operand is a [`ChownError::Usage`] that
//! changes nothing; an owner operand that is not a valid
//! `OWNER`/`OWNER:GROUP`/`:GROUP` spec is a [`ChownError::BadOwner`]; an
//! operand that cannot be inspected surfaces the underlying
//! [`Errno`](rustos_abi::Errno) as [`ChownError::Stat`]; an owner that cannot
//! be applied is [`ChownError::Apply`]; a directory whose entries cannot be
//! read during a recursive descent is [`ChownError::Read`]. The first failure
//! stops the run before any later operand, and there is no panic (`AGENTS.md`
//! §2.9).
//!
//! # Module map
//!
//! * [`error`] — [`ChownError`], the outcomes of [`run`].
//! * [`command`] — the [`Command`]/[`Owner`] shapes and the [`parse`]r.
//! * [`io`] — the [`FileSystem`] and [`Output`] seams and the
//!   [`Entry`]/[`EntryKind`] data they carry.
//! * [`client`] — the [`run`] entry point and the recursive engine.
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
pub use command::{parse, parse_owner, Command, Owner};
pub use error::ChownError;
pub use io::{Entry, EntryKind, FileSystem, Output};
