//! The first-party curses / TUI screen-model library (`lib/curses`).
//!
//! A curses application does not write escape sequences by hand. It draws into
//! a [`Window`] — a grid of cells with a cursor and current attributes — and
//! asks the library to make the terminal match. This crate is that library, the
//! fourth stage of `plans/CURSES.md`, built on the one shared vocabulary
//! (`lib/vt`) and the compiled-in capability database (`lib/termcap`).
//!
//! # The model
//!
//! * A [`Window`] is the application's drawing surface (text, attributes,
//!   colours, boxes, lines, scrolling regions, resize). Pads are simply windows
//!   larger than the screen, refreshed through a viewport.
//! * A [`Screen`] is the I/O-injected driver. It keeps the assembled *virtual*
//!   screen and the last-flushed *physical* screen and, on [`Screen::doupdate`],
//!   runs the [minimal-diff renderer](mod@crate::render) to emit the smallest `lib/vt`
//!   sequence the terminal supports — degrading colour by the terminal's
//!   [`rustos_termcap::ColorDepth`] (truecolour → 256 → 16 → mono).
//! * [`Input`] decodes the terminal's bytes (through `lib/vt`'s one parser) into
//!   typed [`Event`]s: characters, the arrow / function / editing keys, mouse
//!   reports, and bracketed-paste runs.
//!
//! # One vocabulary, fail closed
//!
//! Every byte this crate emits or parses is a [`rustos_vt::Op`] — there is no
//! second escape-sequence table here. It is `no_std` +
//! `alloc` and is part of the OS — the curated `/System/Libraries/`
//! Terminal/TUI class that applications dynamically link —
//! and contains
//! no `unwrap` / `expect` / `panic!`: an out-of-range draw is a
//! [`CursesError`], an unknown input sequence produces no event, and a
//! colour the terminal cannot show is degraded, never emitted raw. Nothing here writes to fd 3 (`stdinfo`).
//!
//! ```
//! use rustos_curses::{render, Buffer, CursorState, Pos, Size, Window};
//! use rustos_termcap::TermType;
//!
//! // Draw "hi" into a one-row window.
//! let mut win = Window::new(Pos::ORIGIN, Size::new(1, 5));
//! win.add_str("hi");
//!
//! // Composite it onto an otherwise blank screen and diff against the blank
//! // last-flushed screen.
//! let blank = Buffer::new(Size::new(1, 5));
//! let mut desired = blank.clone();
//! desired.blit(win.buffer(), Pos::ORIGIN);
//!
//! let caps = TermType::Xterm256Color.capabilities();
//! let cursor = CursorState { visible: true, pos: Pos::new(0, 2) };
//! let ops = render(&caps, &blank, &desired, cursor);
//! assert!(!ops.is_empty());
//! ```

#![no_std]
#![forbid(unsafe_code)]
#![deny(missing_docs)]

extern crate alloc;

pub mod buffer;
pub mod color;
pub mod error;
pub mod geom;
pub mod input;
pub mod render;
pub mod screen;
pub mod window;

#[cfg(test)]
mod tests;

pub use buffer::Buffer;
pub use color::{downgrade, ColorPair, ColorPairs, DEFAULT_PAIR, MAX_COLOR_PAIRS};
pub use error::{CursesError, Result};
pub use geom::{Pos, Size};
pub use input::{Event, Input};
pub use render::{render, CursorState};
pub use rustos_vt::{char_width, is_wide, str_width, truncate_to_width, CONTINUATION};
pub use screen::{InputMode, Screen, Tty, DEFAULT_COLOR_PAIR};
pub use window::{BorderChars, Window};
