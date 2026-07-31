//! TAIRiX `top` — a live process-overview TUI (Stage C5 of
//! `plans/CURSES.md`).
//!
//! `top` is the first in-tree consumer of the OS curses library
//! (`lib/curses`): it draws a scrolling, selectable process list that
//! refreshes itself on the `-d`/`--delay` cadence (and on demand), in the
//! spirit of GNU `top`. It reads the same
//! `sysinfo-v1` process list as `ps` — there is no `/proc` to scrape — and renders it through the curses screen model
//! rather than emitting escape sequences by hand.
//!
//! # What this crate is
//!
//! A thin, fully host-testable front end built from three seams:
//!
//! * [`Transport`] — the `sysinfo` request channel (from `lib/procinfo`),
//!   shared with `ps`/`sysinfo` so the paging walk and the columnar row
//!   rendering are not duplicated here.
//! * [`Tty`] — the curses byte channel; an in-memory channel makes the whole
//!   viewer testable without a kernel.
//! * [`Model`] — the I/O-free view state (snapshot, selection, scroll,
//!   scope, help) that [`render`] draws and [`run`] drives.
//!
//! # Module map
//!
//! * [`error`] — [`TopError`], the outcomes of [`run`].
//! * [`command`] — the [`Command`] shape and its [`parse`]r: the reserved
//!   `-h`/`-?` short-help switches (plans/APPS.md §4) and the GNU
//!   `-d`/`--delay` refresh interval against running the viewer.
//! * [`model`] — the [`Model`], its [`Scope`], and the [`Action`] an event
//!   produces.
//! * [`app`] — [`render`] and the [`run`] input loop.
//!
//! # Layering & safety
//!
//! `no_std` (with `alloc`). It links only `lib/*` crates —
//! the audited `lib/abi`, the shared `lib/procinfo`, and the OS-provided
//! `lib/curses`/`lib/termcap`/`lib/vt` — never a kernel or driver crate. `lib/curses` is the curated `/System/Libraries/`
//! Terminal/TUI class, dynamically linked at runtime; in
//! the workspace it is an ordinary cargo path dependency. No `unsafe`, and no
//! `unwrap`/`expect`/`panic!` in production paths; nothing
//! writes to fd 3 (`stdinfo`).

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

pub mod app;
pub mod command;
pub mod error;
pub mod model;

#[cfg(test)]
mod tests;

pub use app::{list_capacity, render, run};
pub use command::{parse, Command, DEFAULT_DELAY_TENTHS, MIN_DELAY_TENTHS, USAGE};
pub use error::TopError;
pub use model::{Action, CpuSplit, Model, Scope, ALL_DENIED_NOTICE};
pub use tairix_curses::{Screen, Tty};
pub use tairix_procinfo::Transport;
