//! The `users` account-administration tool's interactive session
//! (`plans/CAPABILITY_USE.md` CU4).
//!
//! This library is the whole behaviour of the `users` CLI — the
//! command-line grammar ([`command`]: the reserved `-h`/`-?` short-help
//! switches, plans/APPS.md §4, against running the session), the in-session
//! command grammar, the typed `users_admin` request encoding, and the
//! response rendering — behind three seams ([`session::ToolIo`],
//! [`session::AdminChannel`], [`session::SaltSource`]) so every path is
//! host-testable without a kernel. The freestanding `Run` binary
//! (`src/run.rs`) merely binds the seams to the inherited standard
//! streams, the `users_admin` syscall wrapper, and the `sys:random`
//! resource.
//!
//! # Security shape
//!
//! The tool adds no authority of its own: every operation is decided
//! kernel-side under the caller's kernel-attested identity — the
//! `CAP_USER_ADMIN` dispatch gate plus the engine's never-widen,
//! last-administrator, and format rules. Passwords are read with terminal
//! echo off, hashed **client-side** into a salted PBKDF2 record (the same
//! `lib/users` builder the image builder uses, with a salt drawn from the
//! kernel CSPRNG), zeroised after use, and never echoed, logged, or sent
//! in plaintext; no operation ever returns stored password material.
//!
//! # Layering & safety
//!
//! `no_std` (with `alloc`); the only dependencies are the audited
//! `lib/abi` ABI crate and the shared `lib/users` account-record policy,
//! so this userland tool never links a kernel or driver crate. No
//! `unsafe`, and no `unwrap`/`expect`/`panic!` in production paths: every
//! refusal renders as a terse error line and the session continues.

#![no_std]
#![deny(missing_docs)]

extern crate alloc;

pub mod command;
pub mod session;

pub use command::{parse, Command, UsageError, USAGE};
pub use session::{run_session, AdminChannel, SaltSource, SessionConfig, ToolIo};
