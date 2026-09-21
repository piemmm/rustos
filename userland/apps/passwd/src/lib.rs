//! TAIRiX `passwd` — set an account's password.
//!
//! The account's stored credential is replaced through the one
//! `users_admin` operation that carries one. A plaintext password never
//! crosses the syscall: the tool prompts with terminal echo off, hashes
//! into a salted PBKDF2 record through the shared `lib/users` builder, and
//! sends the record, zeroising every plaintext buffer it touched.
//!
//! # Where TAIRiX differs from GNU `passwd`
//!
//! The account name is **required**. GNU's operand-less form changes the
//! caller's own password, which on TAIRiX would need an unprivileged
//! self-service path that does not exist: the whole `users_admin` syscall
//! is gated on `CAP_USER_ADMIN`, and widening that gate to carve out "your
//! own record" is a security-model change, not a convenience. Until such a
//! path is designed, `passwd` is administration and says so.
//!
//! # `--record`, and why it exists
//!
//! A graphical caller cannot type into this tool: its standard input is
//! closed when it is run under the supervisor's elevated-read seam. Such a
//! caller hashes the password itself — the same shared builder, a salt
//! from the kernel CSPRNG — and hands the finished record over with
//! `--record`, so no plaintext ever leaves the calling process either.
//! The record is validated as a well-formed one before it is sent; a
//! malformed word is refused rather than stored.
//!
//! # Layering & safety
//!
//! `no_std` (with `alloc`); the dependencies are the audited `lib/abi`
//! vocabulary, the shared `lib/help` engine, the shared `lib/useradmin`
//! client, and the `lib/users` account policy, so this userland tool never
//! links a kernel or driver crate. No `unsafe`, and no
//! `unwrap`/`expect`/`panic!` in production paths.

#![no_std]
#![forbid(unsafe_code)]
#![deny(missing_docs)]

extern crate alloc;

pub mod client;
pub mod command;

pub use client::{run, Output, RunError, SaltSource, Terminal};
pub use command::{parse, Command, Secret, UsageError};

/// The usage banner a usage error is reported with, and the fallback the
/// short-help switches print when the bundle's own `Help/` tree is
/// unavailable.
pub const USAGE: &str = "usage: passwd [--record RECORD] [--] NAME\n";
