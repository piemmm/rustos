//! TAIRiX `setcap` — set or clear a file's required-capability gate (Stage 6 `userland/apps/`).
//!
//! Every inode in the TAIRiX permission model may carry an **optional
//! capability requirement**: a capability the caller must hold to reach the
//! node at all, on top of the mode/ACL checks. `setcap`
//! changes that gate. The capability operand is either a canonical `CAP_*`
//! name (e.g. `CAP_AUDIT_READ`), which sets the gate to that capability, or
//! the literal `-`, which clears the gate so the node has none. With `-R` a
//! directory operand is changed and then its contents are changed
//! recursively. This is the companion to [`getcap`](../getcap/index.html) and
//! a building block of the filesystem permission model.
//!
//! `setcap` is the policy-*writing* sibling of `chmod`/`chown`: it stores the
//! gate but makes no permission decision itself (the VFS
//! is the policy point). Setting a gate is itself a privileged operation; the
//! filesystem seam refuses an attempt the caller is not authorised to make
//! (it surfaces as [`SetcapError::Apply`]).
//!
//! # What this crate is
//!
//! A **capability-gate-setting engine**, not a data source. For one parsed
//! [`Command`] it asks the injected filesystem seam for each operand's kind,
//! applies the new gate, and walks each directory `-R` must descend. The
//! operations that touch the outside world are the injected seams:
//!
//! * [`FileSystem`] — learn a path's kind, set its capability gate, and read a
//!   directory's entries (for `-R`).
//! * [`Output`] — write the usage banner to the terminal.
//!
//! The binary that ships as `setcap` wires the real syscall-backed filesystem
//! and console output; tests wire in-memory fixtures. This is the seam
//! discipline of the other userland crates (`cat`'s `FileSource`, `ls`'s
//! `Listing`, `rm`'s `Removal`, `cp`'s and `mv`'s `FileSystem`, `chmod`'s and
//! `chown`'s `FileSystem`), and it keeps every parsing, cap-spec, and
//! recursion decision testable without a kernel.
//!
//! # Fail closed
//!
//! An unknown option or a missing operand is a [`SetcapError::Usage`] that
//! changes nothing; a capability operand that is neither a known `CAP_*` name
//! nor `-` is a [`SetcapError::BadCapability`]; an operand that cannot be
//! inspected surfaces the underlying [`Errno`](tairix_abi::Errno) as
//! [`SetcapError::Stat`]; a gate that cannot be applied is
//! [`SetcapError::Apply`]; a directory whose entries cannot be read during a
//! recursive descent is [`SetcapError::Read`]. The first failure stops the
//! run before any later operand, and there is no panic.
//!
//! # Module map
//!
//! * [`error`] — [`SetcapError`], the outcomes of [`run`].
//! * [`command`] — the [`Command`] shape and the [`parse`]r (incl.
//!   [`parse_capability`]).
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
pub use command::{parse, parse_capability, Command};
pub use error::SetcapError;
pub use io::{Entry, EntryKind, FileSystem, Output};
