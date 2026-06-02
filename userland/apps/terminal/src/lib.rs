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
//!   [`Parser`] state machine — printable text, the C0 controls (backspace,
//!   tab, line feed, carriage return) and a subset of ANSI CSI escape
//!   sequences (cursor movement and positioning, erase-in-line,
//!   erase-in-display). Bytes outside that set are consumed without
//!   corrupting the screen (`AGENTS.md` §2.9).
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
//! # Module map
//!
//! * [`grid`] — the [`Grid`]/[`Cell`] character-cell screen.
//! * [`parser`] — the [`Parser`] byte-stream control state machine.
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

pub use grid::{Cell, Grid};
pub use parser::Parser;
pub use render::render;
pub use shell::ShellSource;
pub use terminal::Terminal;

#[cfg(test)]
mod tests;
