//! RustOS `top` — a live process-overview TUI (Stage C5 of
//! `plans/CURSES.md`).
//!
//! `top` is the first in-tree consumer of the OS curses library
//! (`lib/curses`): it draws a scrolling, selectable process list that
//! refreshes on demand, in the spirit of the Linux `top`. It reads the same
//! `sysinfo-v1` process list as `ps` — there is no `/proc` to scrape
//! (`AGENTS.md` §16.6) — and renders it through the curses screen model
//! rather than emitting escape sequences by hand (`AGENTS.md` §2.2).
//!
//! # What this crate is
//!
//! A thin, fully host-testable front end built from three seams:
//!
//! * [`Transport`] — the `sysinfo` request channel (from `lib/procinfo`),
//!   shared with `ps`/`sysinfo` so the paging walk and the columnar row
//!   rendering are not duplicated here (`AGENTS.md` §2.2).
//! * [`Tty`] — the curses byte channel; an in-memory channel makes the whole
//!   viewer testable without a kernel (`AGENTS.md` §7).
//! * [`Model`] — the I/O-free view state (snapshot, selection, scroll,
//!   scope, help) that [`render`] draws and [`run`] drives.
//!
//! # Module map
//!
//! * [`error`] — [`TopError`], the outcomes of [`run`].
//! * [`model`] — the [`Model`], its [`Scope`], and the [`Action`] an event
//!   produces.
//! * [`app`] — [`render`] and the [`run`] input loop.
//!
//! # Layering & safety
//!
//! `no_std` (with `alloc`, `AGENTS.md` §6). It links only `lib/*` crates —
//! the audited `lib/abi`, the shared `lib/procinfo`, and the OS-provided
//! `lib/curses`/`lib/termcap`/`lib/vt` — never a kernel or driver crate
//! (`AGENTS.md` §17.4). `lib/curses` is the curated `/System/Libraries/`
//! Terminal/TUI class, dynamically linked at runtime (`AGENTS.md` §16.4); in
//! the workspace it is an ordinary cargo path dependency. No `unsafe`, and no
//! `unwrap`/`expect`/`panic!` in production paths (`AGENTS.md` §2.9); nothing
//! writes to fd 3 (`stdinfo`, §20).

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

pub mod app;
pub mod error;
pub mod model;

#[cfg(test)]
mod tests;

pub use app::{list_capacity, render, run};
pub use error::TopError;
pub use model::{Action, Model, Scope};
pub use rustos_curses::{Screen, Tty};
pub use rustos_procinfo::Transport;
