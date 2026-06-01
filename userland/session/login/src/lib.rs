//! RustOS text login (Stage 6, `AGENTS.md` §10).
//!
//! `rustos-login` authenticates a user against `kernel/sec` and launches a
//! session on their behalf. It **always starts in text mode** and offers a
//! graphical session only when a display driver and the window manager are
//! present; when they are not, the graphical option is simply hidden — never
//! crashed, never errored (`AGENTS.md` §10). Installed to
//! `/System/Services/login`.
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
//!    and capability ceiling (`AGENTS.md` §5.1, §5.2). A rejected attempt
//!    consumes one of a bounded number of tries; the budget makes login
//!    **fail closed** rather than prompt forever (`AGENTS.md` §5.4.5).
//! 3. On success, offer the session choice (text by default, graphical only
//!    when available) and hand the authenticated identity to the
//!    [`SessionLauncher`].
//!
//! Every security-relevant decision — a failed authentication, a lockout, a
//! session start and end, a refused launch, a dead console — is audited
//! through `lib/log` with a stable [`EventId`](rustos_log::EventId) in the
//! reserved `10000..11000` range (`AGENTS.md` §19.4). The cause of an
//! authentication failure is audited but **never** disclosed to the caller,
//! so the prompt cannot be used to probe for valid usernames.
//!
//! The three operations that touch the outside world — reading/writing the
//! terminal, verifying a credential, and launching a session — are the
//! injected [`Prompt`], [`Authenticator`], and [`SessionLauncher`] seams. The
//! binary that ships as `/System/Services/login` wires the real kernel- and
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
//! * [`login`] — the [`Login`] state machine.
//!
//! # Layering & safety
//!
//! `no_std` (with `alloc`, `AGENTS.md` §6); the only dependencies are the
//! audited `lib/*` crates `rustos-abi`, `rustos-caps`, and `rustos-log`, so a
//! userland service never links a kernel or driver crate (`AGENTS.md` §17.4).
//! No `unsafe`, and no `unwrap`/`expect`/`panic!` in production paths
//! (`AGENTS.md` §2.9).

#![no_std]
#![forbid(unsafe_op_in_unsafe_fn)]
#![deny(missing_docs)]

extern crate alloc;

pub mod error;
pub mod events;
pub mod login;
pub mod session;

pub use error::LoginError;
pub use login::{Login, LoginConfig};
pub use session::{
    AuthenticatedUser, Authenticator, Credentials, Gid, Prompt, SessionKind, SessionLauncher,
    SessionOutcome, Uid,
};
