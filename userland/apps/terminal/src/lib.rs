//! TAIRiX **terminal emulator** — the default graphical terminal
//! (`userland/apps/`, `PLAN.md` Stage 7).
//!
//! The terminal runs the default shell and shows its output on a
//! fixed-size character grid. Like the filesystem browser it is a graphical
//! app, so it consumes the same `lib/*` building blocks the taskbar does
//! (`lib/geometry`, `lib/theme`, `lib/raster`, `lib/font`) and never depends
//! on the window manager.
//!
//! # What this crate is
//!
//! A screen **model** plus a **renderer**, both driven by an injected
//! [`ShellSource`]:
//!
//! * [`Grid`] is the character-cell screen: a rectangle of [`Cell`]s and a
//!   cursor. It interprets the control behaviour of a byte stream through the
//!   [`Parser`], which is a *consumer* of the shared
//!   [`lib/vt`](tairix_vt) ANSI/VT/xterm vocabulary — there is no second
//!   escape-sequence definition in this app. The emulator
//!   is xterm-class: printable text and Unicode, the C0 controls, SGR
//!   rendition with the 16/256/truecolour models, cursor movement and
//!   positioning, the erase operations, the scroll region and explicit
//!   scrolling, the alternate screen, cursor visibility, and the saved
//!   cursor. A [`Cell`] therefore carries [`Attributes`] (the folded SGR
//!   state), not just a glyph. Bytes outside the recognised vocabulary are
//!   consumed without corrupting the screen.
//! * [`Terminal`] ties the model to the shell: [`Terminal::pump`] reads the
//!   bytes the shell has produced and feeds them to the grid, and
//!   [`Terminal::send`] forwards keystrokes to the shell. Neither side echoes
//!   on the terminal's behalf — echo is the pty slave's line discipline, the
//!   shell's tty, exactly as on the hardware console.
//! * [`render()`] paints the grid into a `lib/raster`
//!   [`Surface`](tairix_raster::Surface) using the active theme's palette and
//!   the shared `lib/font` monospace face — the same surface the compositor
//!   places and rounds.
//!
//! # The shell seam
//!
//! [`ShellSource`] is the one thing the terminal needs from outside: a way to
//! read the shell's output bytes and write the user's input bytes. On a
//! running system it is [`spawned::StreamShellSource`] over the master end of
//! one kernel pseudo-terminal — the shell child the terminal spawned under
//! its own `CAP_PROC_SPAWN` runs over the pty slave (a console-class tty),
//! wired at spawn through [`spawned::shell_wires`] (`plans/APPWIN.md` AW4,
//! `plans/PTY.md`); tests wire
//! an in-memory queue, so the screen model and the renderer are exhaustively
//! testable without a kernel. The `Run` binary (`src/run.rs`) composes the
//! live syscalls under the seam and parks on its wait-set for window events,
//! shell output, and the shell child's exit — never a poll loop.
//!
//! # The terminal type it advertises
//!
//! The emulator consumes the SGR colour models (16/256/truecolour), cursor
//! addressing, the erase operations, the scroll region, the alternate screen,
//! and cursor visibility, so it honestly advertises itself as [`TERM`]
//! (`xterm-256color`) — every capability that name implies is really parsed,
//! not faked. The compiled-in capability database that maps
//! a `TERM` to its full record is the next `plans/CURSES.md` stage (`lib/termcap`).
//!
//! # Module map
//!
//! * [`grid`] — the [`Grid`] character-cell screen of [`Cell`]s.
//! * [`parser`] — the [`Parser`] adapter onto `lib/vt`'s shared parser.
//! * [`shell`] — the [`ShellSource`] seam.
//! * [`spawned`] — the production seam: the spawned shell's pty wiring.
//! * [`terminal`] — the [`Terminal`] model gluing the grid to the shell.
//! * [`render`](mod@render) — painting the grid into a `Surface`.
//!
//! # Layering & safety
//!
//! `no_std` (with `alloc`); the only dependencies are the
//! audited `lib/abi` ABI crate and the shared `lib/*` desktop libraries, so
//! this app never links a kernel, driver, or window-manager crate. No `unsafe`, and no `unwrap`/`expect`/`panic!` in
//! production paths.

#![no_std]
#![forbid(unsafe_code)]
#![deny(missing_docs)]

extern crate alloc;

pub mod grid;
pub mod parser;
pub mod render;
pub mod shell;
pub mod spawned;
pub mod terminal;

pub use grid::Grid;
pub use parser::Parser;
pub use render::render;
pub use shell::ShellSource;
pub use spawned::{shell_env, shell_load_failure, shell_wires, StreamShellSource};
pub use terminal::Terminal;
// The cell and rendition vocabulary the emulator consumes is `lib/vt`'s, not a
// second definition; re-export it so callers name one type.
pub use tairix_vt::{Attributes, Cell, Color};

/// The terminal type this emulator honestly advertises for itself.
///
/// The emulator parses the full SGR colour set (16/256/truecolour), cursor
/// addressing, the erase operations, the scroll region, the alternate screen,
/// and cursor visibility — exactly the capabilities `xterm-256color` implies —
/// so advertising this name is not a lie. The compiled-in
/// capability database keyed by this value is the `lib/termcap` stage of
/// `plans/CURSES.md`.
pub const TERM: &str = "xterm-256color";

/// Columns of the terminal's fixed screen grid — the conventional 80×24
/// text screen.
pub const COLS: u16 = 80;

/// Rows of the terminal's fixed screen grid.
pub const ROWS: u16 = 24;

/// Width in pixels of the terminal's window: the grid rendered with the
/// shared monospace face, one advance per column. Derived from the same
/// metrics the renderer draws with, so the window and the paint can never
/// disagree; the QEMU vertical's runner imports it for its click
/// coordinates exactly as it imports the file browser's.
pub const WIN_WIDTH: u32 = tairix_font::BitmapFont::inconsolata().advance() * COLS as u32;

/// Height in pixels of the terminal's window: one line height per row.
pub const WIN_HEIGHT: u32 = tairix_font::BitmapFont::inconsolata().line_height() * ROWS as u32;

#[cfg(test)]
mod tests;
