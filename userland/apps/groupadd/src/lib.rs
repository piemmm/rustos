//! RustOS `groupadd` — create a group (Stage 6
//! `userland/apps/`).
//!
//! `groupadd` adds a single group to the group database that persists under
//! `/System/Security/Groups`. It names the new group
//! and an optional numeric id (auto-allocated by the database when omitted),
//! and hands that record to the database through an injected seam. The group
//! id is a **decimal** value: RustOS has no name-to-id seam in this tool, so a
//! name would be interface creep, the same choice `chown`
//! and `useradd` make.
//!
//! It is the natural sibling of `useradd`: the same parser/seam/error
//! discipline, narrowed to the two fields a group record carries.
//!
//! # What this crate is
//!
//! A **group-spec parser and presenter**, not a policy point. For one parsed
//! [`Command`] it asks the injected database whether the name is already
//! taken, then writes the new record. The operations that touch the outside
//! world are the injected seams:
//!
//! * [`GroupDb`] — learn whether a group name is already in use and create the
//!   group record.
//! * [`Output`] — write the usage banner to the terminal.
//!
//! The binary that ships as `groupadd` wires the real syscall-backed database
//! and console output; tests wire in-memory fixtures. This is the seam
//! discipline of the other userland crates (`useradd`'s `UserDb`, `init`'s
//! `Spawner`/`Reaper`, `login`'s `Authenticator`, `setcap`'s `FileSystem`),
//! and it keeps every parsing and validation decision testable without a
//! kernel.
//!
//! # Not a policy point
//!
//! Creating a group is privileged — it needs `CAP_USER_ADMIN` — but the **database** makes that decision, not this
//! tool. `groupadd` builds and presents a record; an
//! unauthorised attempt is refused by the seam and surfaced as
//! [`GroupaddError::Create`]. The database is likewise the authority on gid
//! collisions; `groupadd` validates only the syntactic shape of its arguments
//! and never guesses a default gid.
//!
//! # Fail closed
//!
//! An unknown option or anything other than exactly one name operand is a
//! [`GroupaddError::Usage`] that creates nothing; a group name that is not a
//! valid `[a-z_][a-z0-9_-]*` (within the length bound) is a
//! [`GroupaddError::BadName`]; a `-g` value that is not a decimal id is a
//! [`GroupaddError::BadId`]; a name already present is a
//! [`GroupaddError::Exists`]. A database that cannot be consulted surfaces the
//! underlying [`Errno`](rustos_abi::Errno) as [`GroupaddError::Lookup`], and a
//! refused or failed creation as [`GroupaddError::Create`]. There is no panic.
//!
//! # Module map
//!
//! * [`error`] — [`GroupaddError`], the outcomes of [`run`].
//! * [`command`] — the [`Command`]/[`NewGroup`] shapes and the [`parse`]r.
//! * [`io`] — the [`GroupDb`] and [`Output`] seams and the [`GroupSpec`] they
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
pub use command::{parse, validate_name, Command, NewGroup, MAX_NAME_LEN};
pub use error::GroupaddError;
pub use io::{GroupDb, GroupSpec, Output};
