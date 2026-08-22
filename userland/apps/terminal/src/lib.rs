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
//! * [`Screen`] paints the grid into a `lib/raster`
//!   [`Surface`](tairix_raster::Surface) using the user's own colour
//!   [`Scheme`] and the shared `lib/font` monospace face — the same surface
//!   the compositor places and rounds. It **keeps** that surface between
//!   frames and repaints only the cells that changed, returning them as the
//!   damage rectangle to present, so a keystroke costs two cells rather than
//!   a window. The screen [`Effects`] pipeline runs over a copy of the
//!   finished picture, never into the retained one.
//!
//! # The user's profile
//!
//! Everything a user can change about their terminal — the colour scheme
//! (including one of their own), the text size, translucency, backdrop blur,
//! and the scan-line / fuzz / phosphor / wobble effects — is one
//! [`Profile`], stored as a plain text document under their own home and
//! edited through the right-click [`ContextMenu`] and the [`Settings`]
//! sheet. The staged design is `plans/GUI-TERMINAL.md`.
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
//! * [`scheme`] — the colour schemes a screen is painted with.
//! * [`profile`] — the per-user settings document and its store.
//! * [`layout`] — the screen grid, the text size that fits it, and the window.
//! * [`effects`] — the ordered screen-effect pipeline a frame passes through.
//! * [`render`](mod@render) — the retained [`Screen`] the grid is painted
//!   into, and the cell diff that decides what a frame redraws.
//! * [`menu`] — the right-click context menu and its keyboard shortcuts.
//! * [`settings`] — the in-window settings sheet.
//! * [`swatch`] — the colour-well grid the custom scheme is edited with.
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

use tairix_abi::window_ipc::WindowSizing;

pub mod appbar;
pub mod effects;
pub mod grid;
pub mod layout;
pub mod menu;
pub mod parser;
pub mod profile;
pub mod render;
pub mod scheme;
pub mod settings;
pub mod shell;
pub mod spawned;
pub mod swatch;
pub mod terminal;

pub use appbar::{BarCommand, DEFAULT_ACTION as BAR_DEFAULT_ACTION};
pub use effects::{Afterglow, Effects, Phase};
pub use grid::Grid;
pub use layout::{COLS, ROWS};
pub use menu::{Command, ContextMenu};
pub use parser::Parser;
pub use profile::Profile;
pub use render::Screen;
pub use scheme::{ColorScheme, Painted, Scheme};
pub use settings::{Settings, SheetOutcome};
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

/// The sizing the terminal's window asks the window manager for: resizable,
/// bottoming out at one whole character cell.
///
/// The floor is a parameter rather than a constant because it is whatever
/// [`COLS`]×[`ROWS`] measures in the face the window is actually drawing
/// with, which only the running font service can answer
/// ([`layout::window_size`]).
#[must_use]
pub const fn win_sizing(min_width_px: u32, min_height_px: u32) -> WindowSizing {
    WindowSizing::Resizable {
        min_width_px,
        min_height_px,
    }
}

/// Whether the terminal's window is decorated resizable, which widens the
/// furniture band reserved around the client.
///
/// Derived from [`win_sizing`] rather than stated a second time, so the
/// window the app opens and the on-screen footprint the QEMU vertical
/// reconstructs cannot disagree. The floor does not affect the band, so any
/// value answers it.
pub const WIN_RESIZABLE: bool = win_sizing(0, 0).resizable();

#[cfg(test)]
mod tests;
