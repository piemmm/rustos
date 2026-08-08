//! TAIRiX text login (Stage 6).
//!
//! `tairix-login` authenticates a user against `kernel/sec` and launches a
//! session on their behalf. Which session runs is **system policy, never a
//! per-login prompt**: the graphical desktop by default
//! (`os.loginType graphical`, `lib/sysconfig`) when a graphical session is
//! available, otherwise the authenticated account's text shell — never
//! crashed, never errored — and a shell user starts the desktop on demand
//! with the `desktop` command. Installed to
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
//! 3. On success, decide the session kind from the configured
//!    `os.loginType` and this round's graphical availability, and hand the
//!    authenticated identity to the [`SessionLauncher`].
//!
//! Every security-relevant decision — a failed authentication, a lockout, a
//! session start and end, a refused launch, a dead console — is audited
//! through `lib/log` with a stable [`EventId`](tairix_log::EventId) in the
//! reserved `10000..11000` range. The cause of an
//! authentication failure is audited but **never** disclosed to the caller,
//! so the prompt cannot be used to probe for valid usernames.
//!
//! The operations that touch the outside world — reading/writing the
//! terminal, verifying a credential, launching a session, and running an
//! elevated command — are the injected [`LoginView`], [`Authenticator`],
//! [`SessionLauncher`], and [`ElevateLauncher`] seams. The
//! binary that ships as `/System/Services/login.app/Run` wires the real kernel- and
//! `kernel/sec`-backed implementations; tests wire in-memory fixtures. This
//! mirrors `init`'s `Spawner`/`Reaper` design and keeps the policy logic
//! independent of kernel plumbing and exhaustively testable.
//!
//! # Module map
//!
//! * [`events`] — stable [`tairix_log::EventId`] constants (`10000` range).
//! * [`error`] — [`LoginError`], the fail-closed outcomes of [`Login::run`].
//! * [`session`] — the identity types ([`Uid`], [`Gid`],
//!   [`AuthenticatedUser`], [`SessionKind`]) and the [`LoginView`],
//!   [`Authenticator`], and [`SessionLauncher`] seams.
//! * [`auth`] — [`UsersAuthenticator`], the production [`Authenticator`]
//!   over the `/System/Security/Users` database (`lib/users`), and
//!   [`DenyAll`], the fail-closed authenticator wired when no database is
//!   held.
//! * [`login`] — the [`Login`] state machine.
//! * [`elevate`](mod@elevate) — [`handle_elevate_request`], the
//!   per-invocation elevation broker a running session's shell drives
//!   (`plans/CAPABILITY_USE.md` CU5), over the same [`Authenticator`]. The
//!   same broker answers a caller's own verify-only re-authentication
//!   request — the primitive a graphical session's screen lock needs,
//!   without a second authenticator anywhere in the tree.
//! * [`broker`](mod@broker) — [`handle_session_request`], the `session-v1`
//!   broker the **graphical** login screen drives
//!   (`plans/NEW-DESKTOP-LOGIN.md` G4): the same authenticator again, the
//!   [`SessionDirectory`] the chooser is drawn from and the presenting
//!   session steps aside through, and one reasonless refusal for every
//!   failure.
//! * [`budget`](mod@budget) — [`AttemptBudget`], the per-account,
//!   monotonic-clock-driven guess meter a refusal's cooldown is reported
//!   from.
//! * [`table`](mod@table) — [`LiveSessions`], the record of which accounts
//!   have a live desktop session and which one of them holds the seat, plus
//!   [`end_live_sessions()`], the drain that ends them all when the
//!   authority exits and its wake mailboxes go with it.
//! * [`supervise`](mod@supervise) — [`supervise()`], the per-round
//!   database-reload loop that keeps a `login` spawned before the encrypted
//!   root is unlocked from caching a stale "no database" answer.
//!
//! # Layering & safety
//!
//! `no_std` (with `alloc`); the only dependencies are the
//! audited `lib/*` crates `tairix-abi`, `tairix-caps`, `tairix-log`,
//! `tairix-users`, and `tairix-vt` (the shared terminal-control vocabulary the
//! read line discipline keys off), so a userland service never links a kernel
//! or driver crate.
//! No `unsafe`, and no `unwrap`/`expect`/`panic!` in production paths.

#![no_std]
#![forbid(unsafe_op_in_unsafe_fn)]
#![deny(missing_docs)]

extern crate alloc;

pub mod auth;
pub mod broker;
pub mod budget;
pub(crate) mod decfmt;
pub mod elevate;
pub mod error;
pub mod events;
pub mod login;
pub mod session;
pub mod supervise;
pub mod table;
pub mod view;

pub use auth::{DenyAll, UsersAuthenticator};
pub use broker::{handle_session_request, DbAccounts, SessionDirectory, SessionReply};
pub use budget::AttemptBudget;
pub use elevate::{handle_elevate_request, ElevateLauncher};
pub use error::LoginError;
pub use login::{Login, LoginConfig};
pub use session::{
    configured_session_kind, effective_session_kind, session_environment, session_program,
    AuthenticatedUser, Authenticator, ConfigStore, Credentials, Gid, LoginView, SessionKind,
    SessionLauncher, SessionOutcome, Uid, DESKTOP_SESSION_PATH, FONTD_SERVICE_PATH,
    GREETER_SERVICE_PATH,
};
pub use supervise::{supervise, DbLoad};
pub use table::{end_live_sessions, LiveSession, LiveSessions, SessionWaker};
pub use view::{ConsoleMode, CursesView, LoginStatus, StatusSource};
