//! RustOS `chmod` — change file mode bits (Stage 6
//! `userland/apps/`).
//!
//! `chmod` applies a mode to each of its file operands. The mode is either an
//! absolute octal value (`644`, `0755`, …) that replaces the permission bits
//! outright, or a comma-separated list of symbolic clauses
//! (`[ugoa]*[-+=][rwxXst]*`, e.g. `g+w`, `o-rx`, `a=rx`, `u+s`) that transform
//! the file's current bits. With `-R` a directory operand is changed and then
//! its contents are changed recursively. This is the POSIX model.
//!
//! # What this crate is
//!
//! A **mode-changing engine**, not a data source. For one parsed [`Command`]
//! it asks the injected filesystem seam for each operand's kind and current
//! mode, computes the new mode, applies it, and walks each directory `-R` must
//! descend. The operations that touch the outside world are the injected
//! seams:
//!
//! * [`FileSystem`] — learn a path's kind and current mode, set its mode, and
//!   read a directory's entries (for `-R`).
//! * [`Output`] — write the short help and `-v`/`-c` reports to the terminal.
//! * `HelpSource` (from `lib/help`) — the tool's own bundle `Help/` tree,
//!   rendered by the `-h`/`-?`/`--help` switches through the one shared
//!   engine (never embedded help text).
//!
//! The binary that ships as `chmod` wires the real syscall-backed filesystem
//! and console output; tests wire in-memory fixtures. This is the seam
//! discipline of the other userland crates (`init`'s `Spawner`/`Reaper`,
//! `login`'s `Prompt`, `sysinfo`'s `Transport`, `cat`'s `FileSource`, `ls`'s
//! `Listing`, `rm`'s `Removal`, `cp`'s and `mv`'s `FileSystem`), and it keeps
//! every parsing, mode-algebra, and recursion decision testable without a
//! kernel.
//!
//! # Fail closed
//!
//! An unknown option or a missing operand is a [`ChmodError::Usage`] that
//! changes nothing; a mode operand that is neither octal nor symbolic is a
//! [`ChmodError::BadMode`]; an operand that cannot be inspected surfaces the
//! underlying [`Errno`](rustos_abi::Errno) as [`ChmodError::Stat`]; a mode that
//! cannot be applied is [`ChmodError::Apply`]; a directory whose entries cannot
//! be read during a recursive descent is [`ChmodError::Read`]. The first
//! failure stops the run before any later operand, and there is no panic.
//!
//! # Module map
//!
//! * [`error`] — [`ChmodError`], the outcomes of [`run`].
//! * [`command`] — the [`Command`]/[`Mode`] shapes, the symbolic-mode algebra,
//!   and the [`parse`]r.
//! * [`io`] — the [`FileSystem`] and [`Output`] seams and the
//!   [`Entry`]/[`EntryKind`]/[`Metadata`] data they carry.
//! * [`client`] — the [`run`] entry point and the recursive engine.
//!
//! # Layering & safety
//!
//! `no_std` (with `alloc`); the only dependencies are the audited `lib/abi`
//! crate and the shared `lib/help` engine, so this userland tool never links
//! a kernel or driver crate. No `unsafe`, and no
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
pub use command::{parse, Clause, Command, Mode, Op, Options, Verbosity};
pub use error::ChmodError;
pub use io::{Entry, EntryKind, FileSystem, Metadata, Output};
