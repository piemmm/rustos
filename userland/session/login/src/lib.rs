//! RustOS text login (Stage 6).
//!
//! `rustos-login` authenticates a user against `kernel/sec` and launches a
//! session on their behalf. It **always starts in text mode** and offers a
//! graphical session only when a display driver and the window manager are
//! present; when they are not, the graphical option is simply hidden — never
//! crashed, never errored. Installed to
//! `/System/Services/login.app/Run`.
//!
//! # What this crate is
//!
//! A **policy state machine**, not a credential store and not a terminal
//! driver. It owns the prompt → authenticate → launch sequence and holds no
//! privileged state of its own:
//!
//! 1. Collect a username and an un-echoed password from the terminal.
//! 2. Hand them to the [`Authenticator`], which verifies them against
//!    `kernel/sec` and the credential store and returns the user's identity
//!    and capability ceiling. A rejected attempt
//!    consumes one of a bounded number of tries; the budget makes login
//!    **fail closed** rather than prompt forever.
//! 3. On success, offer the session choice (text by default, graphical only
//!    when available) and hand the authenticated identity to the
//!    [`SessionLauncher`].
//!
//! Every security-relevant decision — a failed authentication, a lockout, a
//! session start and end, a refused launch, a dead console — is audited
//! through `lib/log` with a stable [`EventId`](rustos_log::EventId) in the
//! reserved `10000..11000` range. The cause of an
//! authentication failure is audited but **never** disclosed to the caller,
//! so the prompt cannot be used to probe for valid usernames.
//!
//! The operations that touch the outside world — reading/writing the
//! terminal, verifying a credential, launching a session, and running an
//! elevated command — are the injected [`Prompt`], [`Authenticator`],
//! [`SessionLauncher`], and [`ElevateLauncher`] seams. The
//! binary that ships as `/System/Services/login.app/Run` wires the real kernel- and
//! `kernel/sec`-backed implementations; tests wire in-memory fixtures. This
//! mirrors `init`'s `Spawner`/`Reaper` design and keeps the policy logic
//! independent of kernel plumbing and exhaustively testable.
//!
//! # Module map
//!
//! * [`events`] — stable [`rustos_log::EventId`] constants (`10000` range).
//! * [`error`] — [`LoginError`], the fail-closed outcomes of [`Login::run`].
//! * [`session`] — the identity types ([`Uid`], [`Gid`],
//!   [`AuthenticatedUser`], [`SessionKind`]) and the [`Prompt`],
//!   [`Authenticator`], and [`SessionLauncher`] seams.
//! * [`auth`] — [`UsersAuthenticator`], the production [`Authenticator`]
//!   over the `/System/Security/Users` database (`lib/users`), and
//!   [`DenyAll`], the fail-closed authenticator wired when no database is
//!   held.
//! * [`login`] — the [`Login`] state machine.
//! * [`elevate`](mod@elevate) — [`handle_elevate_request`], the
//!   per-invocation elevation broker a running session's shell drives
//!   (`plans/CAPABILITY_USE.md` CU5), over the same [`Authenticator`].
//! * [`supervise`](mod@supervise) — [`supervise()`], the per-round
//!   database-reload loop that keeps a `login` spawned before the encrypted
//!   root is unlocked from caching a stale "no database" answer.
//!
//! # Layering & safety
//!
//! `no_std` (with `alloc`); the only dependencies are the
//! audited `lib/*` crates `rustos-abi`, `rustos-caps`, `rustos-log`,
//! `rustos-users`, and `rustos-vt` (the shared terminal-control vocabulary the
//! read line discipline keys off), so a userland service never links a kernel
//! or driver crate.
//! No `unsafe`, and no `unwrap`/`expect`/`panic!` in production paths.

#![no_std]
#![forbid(unsafe_op_in_unsafe_fn)]
#![deny(missing_docs)]

extern crate alloc;

pub mod auth;
pub(crate) mod decfmt;
pub mod elevate;
pub mod error;
pub mod events;
pub mod login;
pub mod session;
pub mod supervise;

pub use auth::{DenyAll, UsersAuthenticator};
pub use elevate::{handle_elevate_request, ElevateLauncher};
pub use error::LoginError;
pub use login::{Login, LoginConfig};
pub use session::{
    session_environment, AuthenticatedUser, Authenticator, Credentials, Gid, Prompt, SessionKind,
    SessionLauncher, SessionOutcome, Uid,
};
pub use supervise::{supervise, DbLoad};
