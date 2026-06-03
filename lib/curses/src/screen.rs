//! [`Screen`] — the I/O-injected driver that ties the model to a terminal.
//!
//! A [`Screen`] owns the physical-terminal state — the assembled *virtual*
//! screen, the last-flushed *physical* screen, the colour-pair table, and the
//! cursor — and a [`Tty`] byte channel. Drawing is the curses two-step:
//! [`Screen::wnoutrefresh`] composites a [`Window`] onto the virtual screen,
//! and [`Screen::doupdate`] diffs the virtual screen against the physical one
//! through the [renderer] and writes the minimal byte sequence to the tty.
//!
//! The [`Tty`] seam is the same shape as the terminal app's `ShellSource`
//! (`plans/CURSES.md` §C4): injecting an in-memory channel makes the whole
//! driver host-testable without a kernel (`AGENTS.md` §7). Reads decode through
//! [`Input`] into typed [`Event`]s.
//!
//! [renderer]: mod@crate::render

use alloc::vec::Vec;

use rustos_termcap::{Capabilities, TermType};
use rustos_vt::{encode_all, MouseMode, Op};

use crate::buffer::Buffer;
use crate::color::{ColorPairs, DEFAULT_PAIR};
use crate::error::Result;
use crate::geom::{Pos, Size};
use crate::input::{Event, Input};
use crate::render::{render, CursorState};
use crate::window::Window;

/// A bidirectional byte channel to the terminal.
///
/// This is the one thing the driver needs from the outside world: somewhere to
/// write rendered bytes and somewhere to read input bytes from. A real channel
/// is a capability-checked tty; a test channel is an in-memory queue.
pub trait Tty {
    /// Write `bytes` to the terminal.
    ///
    /// # Errors
    ///
    /// [`CursesError::Io`](crate::CursesError::Io) if the channel cannot accept the bytes (for example
    /// once the terminal has gone away).
    fn write(&mut self, bytes: &[u8]) -> Result<()>;

    /// Read whatever input bytes are currently available, returning an empty
    /// vector when there is nothing pending (which is not an error).
    ///
    /// # Errors
    ///
    /// [`CursesError::Io`](crate::CursesError::Io) if the channel cannot be read.
    fn read(&mut self) -> Result<Vec<u8>>;
}

/// The curses screen driver, generic over its [`Tty`] channel.
pub struct Screen<T: Tty> {
    caps: Capabilities,
    tty: T,
    input: Input,
    pairs: ColorPairs,
    staged: Buffer,
    physical: Buffer,
    cursor: CursorState,
}

impl<T: Tty> Screen<T> {
    /// A new driver for a `term` terminal of `size`, over the channel `tty`.
    ///
    /// The virtual and physical screens start blank and identical, so the
    /// first [`Screen::doupdate`] emits only what the application has drawn.
    #[must_use]
    pub fn new(tty: T, term: TermType, size: Size) -> Screen<T> {
        Screen {
            caps: term.capabilities(),
            tty,
            input: Input::new(),
            pairs: ColorPairs::new(),
            staged: Buffer::new(size),
            physical: Buffer::new(size),
            cursor: CursorState {
                visible: true,
                pos: Pos::ORIGIN,
            },
        }
    }

    /// The terminal's capabilities.
    #[must_use]
    pub const fn capabilities(&self) -> &Capabilities {
        &self.caps
    }

    /// The screen dimensions.
    #[must_use]
    pub const fn size(&self) -> Size {
        self.staged.size()
    }

    /// The colour-pair table (for [`crate::ColorPairs::get`]).
    #[must_use]
    pub const fn color_pairs(&self) -> &ColorPairs {
        &self.pairs
    }

    /// Define colour pair `id` as `fg` on `bg` (curses `init_pair`).
    ///
    /// # Errors
    ///
    /// [`CursesError::BadColorPair`](crate::CursesError::BadColorPair) for a reserved or out-of-range id.
    pub fn init_pair(&mut self, id: u16, fg: rustos_vt::Color, bg: rustos_vt::Color) -> Result<()> {
        self.pairs.init_pair(id, fg, bg)
    }

    /// Set whether the cursor is shown after the next [`Screen::doupdate`]
    /// (curses `curs_set`).
    pub fn set_cursor_visible(&mut self, visible: bool) {
        self.cursor.visible = visible;
    }

    /// Composite `window` onto the virtual screen at its origin, and adopt its
    /// cursor as the screen cursor (curses `wnoutrefresh`).
    pub fn wnoutrefresh(&mut self, window: &Window) {
        self.staged.blit(window.buffer(), window.origin());
        self.cursor.pos = window.cursor().offset_by(window.origin());
    }

    /// Composite the `region`-sized view of a pad whose top-left is
    /// `pad_origin` onto the virtual screen at `screen_origin` (curses
    /// `pnoutrefresh`).
    pub fn pnoutrefresh(
        &mut self,
        pad: &Window,
        pad_origin: Pos,
        screen_origin: Pos,
        region: Size,
    ) {
        self.staged
            .blit_region(pad.buffer(), pad_origin, screen_origin, region);
    }

    /// Flush the accumulated virtual screen to the terminal (curses
    /// `doupdate`): diff it against the physical screen, write the minimal byte
    /// sequence, and adopt the virtual screen as the new physical screen.
    ///
    /// # Errors
    ///
    /// [`CursesError::Io`](crate::CursesError::Io) if the tty write fails.
    pub fn doupdate(&mut self) -> Result<()> {
        let ops = render(&self.caps, &self.physical, &self.staged, self.cursor);
        self.write_ops(&ops)?;
        self.physical.clone_from(&self.staged);
        Ok(())
    }

    /// Composite `window` and flush in one step (curses `wrefresh`).
    ///
    /// # Errors
    ///
    /// [`CursesError::Io`](crate::CursesError::Io) if the tty write fails.
    pub fn refresh(&mut self, window: &Window) -> Result<()> {
        self.wnoutrefresh(window);
        self.doupdate()
    }

    /// Resize the screen to `size` (curses `resizeterm`), preserving the cells
    /// that remain in range. The application's windows are resized separately
    /// with [`Window::resize`].
    pub fn resize(&mut self, size: Size) {
        self.staged.resize(size);
        self.physical.resize(size);
        let max = Pos::new(size.rows.saturating_sub(1), size.cols.saturating_sub(1));
        self.cursor.pos = Pos::new(
            self.cursor.pos.row.min(max.row),
            self.cursor.pos.col.min(max.col),
        );
    }

    /// Enable a mouse-tracking `mode` on terminals that report the mouse,
    /// writing the enabling sequence (and the SGR extended encoding the decoder
    /// expects). A terminal without mouse support is left unchanged.
    ///
    /// # Errors
    ///
    /// [`CursesError::Io`](crate::CursesError::Io) if the tty write fails.
    pub fn enable_mouse(&mut self, mode: MouseMode) -> Result<()> {
        if !self.caps.mouse.is_supported() {
            return Ok(());
        }
        self.write_ops(&[
            Op::SetMouseMode { mode, enable: true },
            Op::SetMouseMode {
                mode: MouseMode::Sgr,
                enable: true,
            },
        ])
    }

    /// Disable all mouse tracking the driver may have enabled.
    ///
    /// # Errors
    ///
    /// [`CursesError::Io`](crate::CursesError::Io) if the tty write fails.
    pub fn disable_mouse(&mut self) -> Result<()> {
        if !self.caps.mouse.is_supported() {
            return Ok(());
        }
        self.write_ops(&[
            Op::SetMouseMode {
                mode: MouseMode::AnyMotion,
                enable: false,
            },
            Op::SetMouseMode {
                mode: MouseMode::Sgr,
                enable: false,
            },
        ])
    }

    /// Enable or disable bracketed paste on terminals that support it.
    ///
    /// # Errors
    ///
    /// [`CursesError::Io`](crate::CursesError::Io) if the tty write fails.
    pub fn set_bracketed_paste(&mut self, enable: bool) -> Result<()> {
        if !self.caps.bracketed_paste {
            return Ok(());
        }
        self.write_ops(&[Op::SetBracketedPaste(enable)])
    }

    /// Read whatever input is pending and decode it into [`Event`]s (curses
    /// `getch`, batched).
    ///
    /// # Errors
    ///
    /// [`CursesError::Io`](crate::CursesError::Io) if the tty read fails.
    pub fn read_events(&mut self) -> Result<Vec<Event>> {
        let bytes = self.tty.read()?;
        let mut events = Vec::new();
        self.input.feed(&bytes, |event| events.push(event));
        Ok(events)
    }

    /// Encode `ops` and write them to the tty, writing nothing when `ops` is
    /// empty.
    fn write_ops(&mut self, ops: &[Op]) -> Result<()> {
        if ops.is_empty() {
            return Ok(());
        }
        let bytes = encode_all(ops);
        self.tty.write(&bytes)
    }
}

/// The reserved default colour-pair id, re-exported for ergonomic use at the
/// driver surface.
pub const DEFAULT_COLOR_PAIR: u16 = DEFAULT_PAIR;
