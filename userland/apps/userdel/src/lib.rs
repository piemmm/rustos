//! TAIRiX `userdel` — delete a user account.
//!
//! The deletion half of the shadow-utils family: one operand, one
//! `users_admin` operation, and nothing else. Whether the caller may
//! delete anything is decided kernel-side under its attested identity —
//! the `CAP_USER_ADMIN` dispatch gate, and the engine's referential and
//! last-active-administrator rules — so this crate parses a command line
//! and reports a verdict it did not make.
//!
//! # Fail closed
//!
//! An unknown option, or any operand count other than one, is a usage
//! error that deletes nothing. A refusal surfaces as the one terse
//! wording the shared client gives the kernel's own
//! [`Errno`](tairix_abi::Errno), so this tool and its siblings can never
//! describe the same refusal differently.
//!
//! # Layering & safety
//!
//! `no_std` (with `alloc`); the dependencies are the audited `lib/abi`
//! vocabulary, the shared `lib/help` engine, and the shared
//! `lib/useradmin` client, so this userland tool never links a kernel or
//! driver crate. No `unsafe`, and no `unwrap`/`expect`/`panic!` in
//! production paths.

#![no_std]
#![forbid(unsafe_code)]
#![deny(missing_docs)]

extern crate alloc;

pub mod client;
pub mod command;

pub use client::{run, Output, RunError};
pub use command::{parse, Command};

/// The usage banner a usage error is reported with, and the fallback the
/// short-help switches print when the bundle's own `Help/` tree is
/// unavailable.
pub const USAGE: &str = "usage: userdel [--] NAME\n";
