//! The default RustOS shell — a POSIX-ish command interpreter (Stage 6,
//! `PLAN.md`).
//!
//! `rustos-shell` reads a line of text and runs it: it lexes the line with
//! full quoting and escaping, parses pipelines and `;`/`&&`/`||`/`&`
//! connectors, expands `$`-variables, runs a small set of builtins in-process,
//! and launches everything else through an injected process host with job
//! control over background and stopped jobs.
//!
//! # What this crate is
//!
//! A **pure interpreter**. It decides *what* to run and *with what arguments*,
//! but it never itself touches the kernel or a terminal. The two operations
//! that reach the outside world — launching/waiting/signalling a job and
//! writing output — are the injected [`ProcessHost`] and [`Console`] seams.
//! On a running kernel they are syscall-backed; in tests they are in-memory
//! fixtures. This mirrors `init`'s `Spawner`/`Reaper` design and keeps every
//! parsing, expansion, and control-flow decision exhaustively testable
//! without a kernel.
//!
//! # Pipeline
//!
//! 1. [`lexer::tokenize`] — text to a quoting-aware [`lexer::Token`] stream.
//! 2. [`parser::parse`] — tokens to a [`parser::CommandList`] tree.
//! 3. [`env::Environment::expand_word`] — `$`-expansion of each word.
//! 4. [`Shell::run_line`] — run each pipeline, honouring connectors and the
//!    background flag, dispatching builtins ([`builtin`]) or launching through
//!    the [`ProcessHost`], and tracking jobs in the [`job::JobTable`].
//!
//! # Deliberate simplifications
//!
//! These keep a first shell small and predictable; each is documented where
//! it lives rather than papered over (`AGENTS.md` §2.1, §2.3):
//!
//! * Expansion does not field-split or remove empty results: each word
//!   becomes exactly one argument.
//! * `NAME=VALUE` is an assignment only when the whole simple command is
//!   assignments; it is not a per-command temporary-environment prefix.
//! * The supported expansions are `$NAME`, `${NAME}`, and `$?`.
//!
//! # Layering & safety
//!
//! `no_std` (with `alloc`, `AGENTS.md` §6); the only dependency is the audited
//! `lib/abi` (for [`Errno`](rustos_abi::Errno) on the host seam), so a
//! userland program never links a kernel or driver crate (`AGENTS.md` §17.4).
//! No `unsafe`, and no `unwrap`/`expect`/`panic!` in production paths
//! (`AGENTS.md` §2.9).

#![no_std]
#![forbid(unsafe_op_in_unsafe_fn)]
#![deny(missing_docs)]

extern crate alloc;

pub mod builtin;
pub mod env;
pub mod error;
pub mod host;
pub mod job;
pub mod lexer;
pub mod parser;
pub mod shell;

#[cfg(test)]
pub(crate) mod test_support;

pub use env::Environment;
pub use error::ParseError;
pub use host::{Console, LaunchSpec, ProcessHost, ResolvedCommand, ResolvedRedirection};
pub use job::{ExitStatus, Job, JobId, JobState, JobTable, Pid, Signal, WaitOutcome};
pub use shell::Shell;
