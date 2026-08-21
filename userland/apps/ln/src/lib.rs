//! TAIRiX `ln` — create links between files (`plans/SYMLINKS.md` S4, S6).
//!
//! The GNU coreutils `ln`, making both kinds of link. Without `-s` the link
//! is a **hard** one — a second directory entry for the target's own inode,
//! through `fs_link` — and `-L`/`-P` select whether a target that is itself
//! a symbolic link is resolved first. `-d`/`-F` accept a directory operand,
//! but the link is still refused: no principal may give a directory a second
//! name, because the tree staying a tree is what makes the resolver's
//! physical `..` well-defined.
//!
//! Two switch groups stay refused, neither for a reason about hard links:
//! the backup switches (`-b`, `-S`), which this workspace has no backup
//! machinery for, and `-r`/`--relative`, which would need a canonicalising
//! path resolution the ABI does not offer (a *lexical* approximation would
//! name a different node the moment a link were involved, which is exactly
//! the lexical-`..` collapse the resolver forbids). Both are documented in
//! the tool's `Help/` documents.
//!
//! # What this crate is
//!
//! A **planner**, not a data source. For one parsed [`Command`] it asks the
//! injected filesystem seam what each link name already holds, frees a taken
//! name only when `-f`/`-i` said to, and creates the link. The operations
//! that touch the outside world are the injected seams:
//!
//! * [`FileSystem`] — what a name holds, create a link, remove a name.
//! * [`Prompt`] — the `-i` confirmation.
//! * [`Output`] — the `-v` reports and the usage banner.
//! * [`tairix_help::HelpSource`] — the tool's own `Help/` tree, read by the
//!   short-help switches.
//!
//! The binary that ships as `ln` wires the real syscall-backed filesystem and
//! standard streams; tests wire in-memory fixtures. This is the seam
//! discipline of the other userland crates (`cp`'s `FileSystem`/`Prompt`,
//! `ls`'s `Listing`, `rm`'s `Removal`), and it keeps every parsing, operand,
//! and replacement decision testable without a kernel.
//!
//! # A target is data
//!
//! The target is stored **verbatim** and is never resolved here: it may be
//! relative, may carry `..`, and may name nothing at all, so `ln -s` can
//! legitimately create a dangling link and must not pre-validate what it
//! names. Its *grammar* is still checked kernel-side before it is stored, so
//! a target no resolver could ever walk is refused rather than written.
//! Creating a link grants no authority over what it names: every later use
//! re-authorises each component under the caller's attested identity.
//!
//! # Fail closed
//!
//! An unknown option lists nothing; a name that is already taken is refused
//! unless `-f`/`-i` said to replace it, and replacing it **removes** the
//! name first, so bytes and names never travel through a link that was
//! already there. A directory is never replaced. An unanswerable `-i`
//! question is never consent. The first failure stops the run before any
//! later target, exactly as `cp` and `mv` do. There is no partial-guess path
//! and no panic.
//!
//! # Module map
//!
//! * [`error`] — [`LnError`], the outcomes of [`run`].
//! * [`command`] — the [`Command`] shape and its [`parse`]r.
//! * [`io`] — the [`FileSystem`]/[`Prompt`]/[`Output`] seams and the
//!   [`Occupant`] vocabulary they carry.
//! * [`client`] — the [`run`] entry point and the link-planning engine.
//!
//! The bundle's `Help/` documents are **not** embedded in this crate: they
//! are authored once in the bundle's on-disk `Help/` tree, planted onto
//! `/System` by the image builder from that source (`tools/syshelp`), and read
//! back at runtime through the injected [`tairix_help::HelpSource`] seam
//! (plans/APPS.md).
//!
//! # Layering & safety
//!
//! `no_std` (with `alloc`); the dependencies are the audited `lib/abi`
//! vocabulary, the one `lib/path` spelling grammar, and the shared `lib/help`
//! engine, so this userland tool never links a kernel or driver crate. No
//! `unsafe`, and no `unwrap`/`expect`/`panic!` in production paths; nothing
//! writes to fd 3 (`stdinfo`).

#![no_std]
#![forbid(unsafe_code)]
#![deny(missing_docs)]

extern crate alloc;

pub mod client;
pub mod command;
pub mod error;
pub mod io;

pub use client::{run, USAGE};
pub use command::{parse, Clobber, Command, Options, TargetMode};
pub use error::LnError;
pub use io::{FileSystem, Occupant, Output, Prompt};
