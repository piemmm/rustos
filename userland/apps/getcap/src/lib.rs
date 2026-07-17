//! TAIRiX `getcap` — report a file's required-capability gate (Stage 6 `userland/apps/`).
//!
//! Every inode in the TAIRiX permission model may carry an **optional
//! capability requirement**: a capability the caller must hold to reach the
//! node at all, on top of the mode/ACL checks. `getcap`
//! reports that gate. For each file operand it prints one line —
//! `path CAP_NAME` — when the file carries a gate, and prints nothing for a
//! file that has none (so a clean tree is silent, the model `getcap` follows
//! on POSIX systems). With `-R` a directory operand is reported and then its
//! contents are reported recursively.
//!
//! # What this crate is
//!
//! A **capability-gate reporter**, not a policy point. For one parsed
//! [`Command`] it asks the injected filesystem seam for each operand's kind
//! and capability gate, renders the gated files, and walks each directory
//! `-R` must descend. The driver only *reports* the stored gate; `getcap`
//! makes no permission decision (the VFS is the policy
//! point). The operations that touch the outside world are the injected
//! seams:
//!
//! * [`FileSystem`] — learn a path's kind, read its capability gate, and read
//!   a directory's entries (for `-R`).
//! * [`Output`] — write the report (and the usage banner) to the terminal.
//!
//! The binary that ships as `getcap` wires the real syscall-backed filesystem
//! and console output; tests wire in-memory fixtures. This is the seam
//! discipline of the other userland crates (`cat`'s `FileSource`, `ls`'s
//! `Listing`, `rm`'s `Removal`, `cp`'s and `mv`'s `FileSystem`, `chmod`'s and
//! `chown`'s `FileSystem`), and it keeps every parsing, rendering, and
//! recursion decision testable without a kernel.
//!
//! # Fail closed
//!
//! An unknown option or a missing operand is a [`GetcapError::Usage`] that
//! reports nothing; an operand that cannot be inspected surfaces the
//! underlying [`Errno`](tairix_abi::Errno) as [`GetcapError::Stat`]; a gate
//! that cannot be read is [`GetcapError::Query`]; a directory whose entries
//! cannot be read during a recursive descent is [`GetcapError::Read`]; a
//! failed write is [`GetcapError::Output`]. The first failure stops the run
//! before any later operand, and there is no panic.
//!
//! # Module map
//!
//! * [`error`] — [`GetcapError`], the outcomes of [`run`].
//! * [`command`] — the [`Command`] shape and the [`parse`]r.
//! * [`io`] — the [`FileSystem`] and [`Output`] seams and the
//!   [`Entry`]/[`EntryKind`] data they carry.
//! * [`client`] — the [`run`] entry point and the recursive engine.
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
pub use command::{parse, Command};
pub use error::GetcapError;
pub use io::{Entry, EntryKind, FileSystem, Output};
