//! RustOS `edit` — a full-screen curses text editor (plans/APPS.md).
//!
//! `edit` is a small, honest text editor in the spirit of the `QuickBasic` /
//! MS-DOS editor: a menu bar (`File`, `Search`) across the top, the text
//! filling the screen below it, and a status line carrying the file name,
//! cursor position, and key hints. It edits one buffer, loads and saves
//! whole files, and searches forward with wrap-around. It draws through
//! the OS curses library rather than emitting escape sequences by hand.
//!
//! # What this crate is
//!
//! A thin, fully host-testable editor built from two seams:
//!
//! * [`Fs`] — whole-file read/write; the production implementation wraps
//!   the kernel-authorised `fs_*` syscalls, tests inject an in-memory map.
//! * [`Tty`] — the curses byte channel; an in-memory channel makes the
//!   whole editor testable without a kernel.
//!
//! # Module map
//!
//! * [`error`] — [`EditError`], the outcomes of [`run`].
//! * [`command`] — the [`Command`] shape and its [`parse`]r: the reserved
//!   `-h`/`-?` short-help switches (plans/APPS.md §4) and the single
//!   optional file operand.
//! * [`buffer`] — the [`TextBuffer`]: fail-closed decoding, line storage,
//!   and the editing primitives.
//! * [`model`] — the [`Model`], its [`Mode`] state machine, and the file
//!   and search operations.
//! * [`app`] — [`render`] and the [`run`] input loop.
//!
//! # Layering & safety
//!
//! `no_std` (with `alloc`). It links only `lib/*` crates — the audited
//! `lib/abi` and the OS-provided `lib/curses`/`lib/termcap`/`lib/vt` —
//! never a kernel or driver crate. `lib/curses` is the curated
//! `/System/Libraries/` Terminal/TUI class, dynamically linked at runtime;
//! in the workspace it is an ordinary cargo path dependency. No `unsafe`,
//! and no `unwrap`/`expect`/`panic!` in production paths; nothing writes
//! to fd 3 (`stdinfo`).

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

pub mod app;
pub mod buffer;
pub mod command;
pub mod error;
pub mod model;

#[cfg(test)]
mod tests;

pub use app::{render, run, text_area};
pub use buffer::{DecodeError, LoadNotices, TextBuffer, MAX_FILE_BYTES, TAB_STOP};
pub use command::{parse, Command, USAGE};
pub use error::EditError;
pub use model::{Action, Fs, Mode, Model, Pending, PromptIntent, MENUS};
pub use rustos_curses::{Screen, Tty};
