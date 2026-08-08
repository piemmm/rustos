//! TAIRiX graphical login screen — the engine behind `greeter.app`.
//!
//! # The trust boundary
//!
//! **The greeter draws and types; the authority decides.** This service owns
//! the seat, paints the shared authentication surface (`lib/greeter`), and
//! relays what was typed to the session authority over `session-v1`
//! (`plans/NEW-DESKTOP-LOGIN.md` G3). It holds no credential store, cannot
//! read the user database, cannot start a process, and binds no privileged
//! endpoint — so compromising it yields a screen, not an account.
//!
//! Everything it knows about the machine's accounts is what the authority
//! chose to publish: a display name, a login name, and whether that account
//! already has a live session. Everything it learns about a secret is one of
//! three answers, and only [`Verdict::Verified`] finishes the screen. It runs
//! as the dedicated `greeter` service account, whose ceiling in
//! `lib/users` (`grants::GREETER_CEILING`) is exactly the six capabilities
//! the manifest asks for.
//!
//! # Degrading rather than failing
//!
//! A login screen that refuses to appear locks a user out of their own
//! machine, so every absence short of "there is no screen" is presented
//! rather than fatal:
//!
//! * No account list — the chooser stands with its typed-name tile alone.
//! * The authority unreachable — the surface says so and keeps asking. It
//!   does *not* exit, so a transient fault cannot spend the authority's
//!   restart budget.
//! * No wallpaper, or one that will not decode — the flat desktop colour.
//! * No trusted clock or no host name — that line of chrome is empty. Never
//!   invented.
//!
//! What is genuinely fatal is having no screen at all: an unqueryable or
//! zero-extent display mode, or a seat lease taken away underneath the
//! running screen. Each is stated on `stderr` and exits non-zero, and the
//! authority falls back to a text login.
//!
//! # Module map
//!
//! * [`events`] — the stable audit event ids (`19000` range).
//! * [`accounts`] — the [`SessionTransport`] seam and the bounded paging walk
//!   that turns the authority's account pages into chooser tiles.
//! * [`verify`] — the `session-v1` client behind the surface's `Verifier`.
//! * [`chrome`](mod@chrome) — the clock, date, and host name text.
//! * [`cursor`] — the pointer position the seat's relative motion feeds,
//!   and the arrow drawn at it.
//! * [`frame`] — the surface-to-scan-out composition, the pointer sampled
//!   over it, and the merge that turns a drain's changes into one present.
//! * [`wait`] — the lockout countdown and the park deadline.
//! * [`screen`] — [`LoginScreen`], the whole flow over those seams.
//!
//! # Layering & safety
//!
//! `no_std` with `alloc`; the dependencies are audited `lib/*` crates only,
//! so this userland service links no kernel, driver, or GUI crate. No
//! `unsafe`, and no `unwrap`/`expect`/`panic!` in production paths.
//!
//! [`Verdict::Verified`]: tairix_greeter::Verdict::Verified

#![no_std]
#![forbid(unsafe_code)]
#![deny(missing_docs)]

extern crate alloc;

pub mod accounts;
pub mod chrome;
pub mod cursor;
pub mod events;
pub mod frame;
pub mod screen;
pub mod verify;
pub mod wait;

pub use accounts::{load_accounts, DirectoryError, SessionTransport, MAX_ACCOUNTS};
pub use chrome::chrome;
pub use cursor::{pointer_image, Cursor};
pub use frame::{Present, Scanout};
pub use screen::{LoginScreen, Step};
pub use verify::{Answer, SessionVerifier};
pub use wait::{park_timeout, Cooldown, FOREVER};
