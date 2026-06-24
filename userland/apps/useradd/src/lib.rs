//! RustOS `useradd` — create a user account (Stage 6
//! `userland/apps/`).
//!
//! `useradd` adds a single account to the user database that persists under
//! `/System/Security/Users`. It names the new account
//! and its numeric identity — a login name, an optional user id (auto-allocated
//! by the database when omitted), a **required** primary group id, an optional
//! supplementary-group set, and the textual comment and home directory — and
//! hands that record to the database through an injected seam. Group and user
//! references are **decimal** ids: RustOS has no name-to-id seam in this tool,
//! so a name would be interface creep, the same choice
//! `chown` makes.
//!
//! # What this crate is
//!
//! An **account-spec parser and presenter**, not a policy point. For one
//! parsed [`Command`] it asks the injected database whether the name is
//! already taken, then writes the new record. The operations that touch the
//! outside world are the injected seams:
//!
//! * [`UserDb`] — learn whether a login name is already in use and create the
//!   account record.
//! * [`Output`] — write the usage banner to the terminal.
//!
//! The binary that ships as `useradd` wires the real syscall-backed database
//! and console output; tests wire in-memory fixtures. This is the seam
//! discipline of the other userland crates (`init`'s `Spawner`/`Reaper`,
//! `login`'s `Authenticator`, `setcap`'s `FileSystem`), and it keeps every
//! parsing and validation decision testable without a kernel.
//!
//! # Not a policy point
//!
//! Creating an account is privileged — it needs `CAP_USER_ADMIN` — but the **database** makes that decision, not this
//! tool. `useradd` builds and presents a record; an
//! unauthorised attempt is refused by the seam and surfaced as
//! [`UseraddError::Create`]. The database is likewise the authority on uid
//! collisions, group existence, and the supplementary-group bound; `useradd`
//! validates only the syntactic shape of its arguments and never guesses a
//! default group, uid, or home directory.
//!
//! # Fail closed
//!
//! An unknown option, a missing `-g`, or anything other than exactly one
//! name operand is a [`UseraddError::Usage`] that creates nothing; a login
//! name that is not a valid `[a-z_][a-z0-9_-]*` (within the length bound) is a
//! [`UseraddError::BadName`]; a `-u`/`-g`/`-G` value that is not a decimal id
//! is a [`UseraddError::BadId`]; a name already present is a
//! [`UseraddError::Exists`]. A database that cannot be consulted surfaces the
//! underlying [`Errno`](rustos_abi::Errno) as [`UseraddError::Lookup`], and a
//! refused or failed creation as [`UseraddError::Create`]. There is no panic.
//!
//! # Module map
//!
//! * [`error`] — [`UseraddError`], the outcomes of [`run`].
//! * [`command`] — the [`Command`]/[`NewUser`] shapes and the [`parse`]r.
//! * [`io`] — the [`UserDb`] and [`Output`] seams and the [`UserSpec`] they
//!   carry.
//! * [`client`] — the [`run`] entry point.
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
pub use command::{parse, validate_name, Command, NewUser, MAX_NAME_LEN};
pub use error::UseraddError;
pub use io::{Output, UserDb, UserSpec};
