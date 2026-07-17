//! TAIRiX `vim` — the modal text editor (`plans/VIM.md`).
//!
//! The vim core, drawn with the OS curses library: normal, insert,
//! replace, visual (character and line), and command-line modes; counts,
//! named registers, operators (`d`/`c`/`y`) over motions and text
//! objects; undo/redo and dot-repeat; `/`/`?` search with a vim-subset
//! pattern engine; the ex command core (`:w`, `:q`, `:e`, `:r`, `:s`,
//! ranges, `:set number`, the argument list). Everything vim ships beyond
//! this core is deliberately staged, feature by feature, in
//! `plans/VIM.md`.
//!
//! # What this crate is
//!
//! A fully host-testable editor built from three seams:
//!
//! * [`FileIo`] — the named-file channel `:w`/`:e`/`:r` use; the `Run`
//!   binary implements it over the kernel-authorised `fs_*` syscalls, the
//!   tests over an in-memory map.
//! * [`Tty`] — the curses byte channel; an in-memory channel makes the
//!   whole editor drivable without a kernel.
//! * [`Editor`] — the I/O-free state machine (buffer, cursor, modes,
//!   registers, search) that [`render()`] draws and [`run`] drives.
//!
//! # Module map
//!
//! * [`buffer`] — the line buffer with grouped, span-based undo/redo.
//! * [`motion`] — motions and text objects (pure position arithmetic).
//! * [`pattern`] — the bounded vim-subset search-pattern engine.
//! * [`editor`] — the state machine every mode transitions through.
//! * [`normal`] — the normal/visual key grammar.
//! * [`excmd`] — the ex (`:`) command language.
//! * [`mod@render`] — the curses frame (text, highlights, status, message).
//! * [`command`] — the argument-vector parser and [`USAGE`].
//! * [`app`] — the session loop.
//! * [`error`] — [`VimError`], the session outcomes.
//! * [`fileio`] — the [`FileIo`] seam.
//!
//! # Layering & safety
//!
//! `no_std` (with `alloc`). It links only `lib/*` crates — the audited
//! `tairix-abi` and the OS-provided `tairix-curses`/`tairix-termcap`/
//! `tairix-vt` — never a kernel or driver crate. No `unsafe`, and no
//! `unwrap`/`expect`/`panic!` in production paths; hostile input (files,
//! patterns, keys) fails closed. Nothing writes to fd 3 (`stdinfo`).

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

pub mod app;
pub mod buffer;
pub mod command;
pub mod editor;
pub mod error;
pub mod excmd;
pub mod fileio;
pub mod motion;
pub mod normal;
pub mod pattern;
pub mod render;

#[cfg(test)]
mod tests;

pub use app::run;
pub use buffer::{Buffer, Position};
pub use command::{parse, Command, Start, UsageError, USAGE};
pub use editor::{Editor, Mode};
pub use error::VimError;
pub use fileio::FileIo;
pub use render::render;
pub use tairix_curses::{Screen, Tty};
