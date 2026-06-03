//! RustOS **terminal emulator** — the default graphical terminal
//! (`AGENTS.md` §3 `userland/apps/`, §10, `PLAN.md` Stage 7).
//!
//! The terminal runs the default shell and shows its output on a
//! fixed-size character grid. Like the filesystem browser it is a graphical
//! app, so it consumes the same `lib/*` building blocks the taskbar does
//! (`lib/geometry`, `lib/theme`, `lib/raster`, `lib/font`) and never depends
//! on the window manager (`AGENTS.md` §17.4).
//!
//! # What this crate is
//!
//! A screen **model** plus a **renderer**, both driven by an injected
//! [`ShellSource`]:
//!
//! * [`Grid`] is the character-cell screen: a rectangle of [`Cell`]s and a
//!   cursor. It interprets the control behaviour of a byte stream through the
//!   [`Parser`], which is a *consumer* of the shared
//!   [`lib/vt`](rustos_vt) ANSI/VT/xterm vocabulary — there is no second
//!   escape-sequence definition in this app (`AGENTS.md` §2.2). The emulator
//!   is xterm-class: printable text and Unicode, the C0 controls, SGR
//!   rendition with the 16/256/truecolour models, cursor movement and
//!   positioning, the erase operations, the scroll region and explicit
//!   scrolling, the alternate screen, cursor visibility, and the saved
//!   cursor. A [`Cell`] therefore carries [`Attributes`] (the folded SGR
//!   state), not just a glyph. Bytes outside the recognised vocabulary are
//!   consumed without corrupting the screen (`AGENTS.md` §2.9).
//! * [`Terminal`] ties the model to the shell: [`Terminal::pump`] reads the
//!   bytes the shell has produced and feeds them to the grid, and
//!   [`Terminal::send`] forwards keystrokes to the shell. Neither side echoes
//!   on the terminal's behalf — echo is the shell's job, exactly as on a real
//!   tty.
//! * [`render()`] paints the grid into a `lib/raster`
//!   [`Surface`](rustos_raster::Surface) using the active theme's palette and
//!   the shared `lib/font` monospace face — the same surface the compositor
//!   places and rounds (`AGENTS.md` §2.2).
//!
//! # The shell seam
//!
//! [`ShellSource`] is the one thing the terminal needs from outside: a way to
//! read the shell's output bytes and write the user's input bytes. On a
//! running system it is a capability-checked pseudo-terminal channel to the
//! shell process; tests wire an in-memory queue, so the screen model and the
//! renderer are exhaustively testable without a kernel (`AGENTS.md` §7). The
//! binary that ships as the terminal wires the real channel (deferred until
//! the userland process/IPC client lands).
//!
//! # The terminal type it advertises
//!
//! The emulator consumes the SGR colour models (16/256/truecolour), cursor
//! addressing, the erase operations, the scroll region, the alternate screen,
//! and cursor visibility, so it honestly advertises itself as [`TERM`]
//! (`xterm-256color`) — every capability that name implies is really parsed,
//! not faked (`AGENTS.md` §2.2). The compiled-in capability database that maps
//! a `TERM` to its full record is the next `plans/CURSES.md` stage (`lib/termcap`).
//!
//! # Module map
//!
//! * [`grid`] — the [`Grid`] character-cell screen of [`Cell`]s.
//! * [`parser`] — the [`Parser`] adapter onto `lib/vt`'s shared parser.
//! * [`shell`] — the [`ShellSource`] seam.
//! * [`terminal`] — the [`Terminal`] model gluing the grid to the shell.
//! * [`render`](mod@render) — painting the grid into a `Surface`.
//!
//! # Layering & safety
//!
//! `no_std` (with `alloc`, `AGENTS.md` §6); the only dependencies are the
//! audited `lib/abi` ABI crate and the shared `lib/*` desktop libraries, so
//! this app never links a kernel, driver, or window-manager crate
//! (`AGENTS.md` §17.4). No `unsafe`, and no `unwrap`/`expect`/`panic!` in
//! production paths (`AGENTS.md` §2.9).

#![no_std]
#![forbid(unsafe_code)]
#![deny(missing_docs)]

extern crate alloc;

pub mod grid;
pub mod parser;
pub mod render;
pub mod shell;
pub mod terminal;

pub use grid::Grid;
pub use parser::Parser;
pub use render::render;
pub use shell::ShellSource;
pub use terminal::Terminal;
// The cell and rendition vocabulary the emulator consumes is `lib/vt`'s, not a
// second definition (`AGENTS.md` §2.2); re-export it so callers name one type.
pub use rustos_vt::{Attributes, Cell, Color};

/// The terminal type this emulator honestly advertises for itself.
///
/// The emulator parses the full SGR colour set (16/256/truecolour), cursor
/// addressing, the erase operations, the scroll region, the alternate screen,
/// and cursor visibility — exactly the capabilities `xterm-256color` implies —
/// so advertising this name is not a lie (`AGENTS.md` §2.2). The compiled-in
/// capability database keyed by this value is the `lib/termcap` stage of
/// `plans/CURSES.md`.
pub const TERM: &str = "xterm-256color";

#[cfg(test)]
mod tests;
