//! TAIRiX `usermod` — modify a user account.
//!
//! The shadow-utils `usermod` surface over the `users_admin` syscall: the
//! identity fields (`-c`, `-d`, `-s`, `-g`, `-G`), the lock state
//! (`-L`/`-U`), and the TAIRiX-native capability grant ceiling
//! (`--grants`). Every decision is made kernel-side under the caller's
//! attested identity — the `CAP_USER_ADMIN` dispatch gate, the never-widen
//! grant rule, the last-active-administrator guard, referential integrity
//! on the groups named — so this crate builds requests and reports a
//! verdict it did not make.
//!
//! # An identity edit is a whole-record replacement
//!
//! `users_admin`'s `ModifyUser` carries an account's complete non-security
//! field set, because a half-specified record is not one the engine could
//! verify. So a `usermod` that changes one field first **reads** the
//! account's current record through the shared listing and resends the
//! rest unchanged. An account the listing does not hold is refused before
//! anything is sent.
//!
//! # Each switch is its own operation
//!
//! The kernel applies one operation at a time, whole-or-nothing, and there
//! is no combined "modify and lock" request — nor should there be, since
//! the two are validated by different rules. A command line asking for
//! several therefore issues several, in one fixed order (identity, then
//! grants, then lock state), stopping at the first refusal and saying
//! which step it was. This is the same posture GNU `usermod` has, and the
//! tool states it rather than leaving a partly-applied edit silent.
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

pub use client::{run, Output, RunError, Step};
pub use command::{parse, Changes, Command, Lock, UsageError};

/// The usage banner a usage error is reported with, and the fallback the
/// short-help switches print when the bundle's own `Help/` tree is
/// unavailable.
pub const USAGE: &str = "usage: usermod [-c COMMENT] [-d HOME] [-s SHELL] [-g GID] [-G LIST]\n\
                         \x20              [-L | -U] [--grants LIST] [--] NAME\n";
