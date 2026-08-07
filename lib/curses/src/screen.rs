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
//! driver host-testable without a kernel. Reads decode through
//! [`Input`] into typed [`Event`]s.
//!
//! [renderer]: mod@crate::render

use alloc::collections::VecDeque;
use alloc::vec::Vec;
use core::time::Duration;

use tairix_termcap::{Capabilities, TermType};
use tairix_vt::{encode_all_into, Attributes, Color, EraseMode, MouseMode, Op};

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

    /// Block until at least one input byte is available, then return the bytes
    /// read (an empty vector signals end-of-input — the far end has closed).
    ///
    /// This backs the blocking [`getch`](Screen::getch). The kernel-backed
    /// channel parks the task until the tty is readable (never busy-spins); the default delegates to [`Tty::read`] for channels
    /// (such as a fixed test queue) that cannot truly block.
    ///
    /// # Errors
    ///
    /// [`CursesError::Io`](crate::CursesError::Io) if the channel cannot be read.
    fn read_blocking(&mut self) -> Result<Vec<u8>> {
        self.read()
    }

    /// Wait up to `timeout` for input, then return whatever bytes arrived
    /// (possibly an empty vector when the timeout elapsed first).
    ///
    /// This backs [`InputMode::Timeout`]. The default delegates to
    /// [`Tty::read`] for channels that cannot honour a deadline.
    ///
    /// # Errors
    ///
    /// [`CursesError::Io`](crate::CursesError::Io) if the channel cannot be read.
    fn read_timeout(&mut self, _timeout: Duration) -> Result<Vec<u8>> {
        self.read()
    }

    /// Report the channel's current character-cell geometry, or [`None`]
    /// when the channel cannot answer — the honest default for a channel
    /// (such as a fixed test queue) with no terminal behind it.
    ///
    /// Neither `Ok(None)` nor an [`Err`] is fatal to the caller: both, and a
    /// degenerate (zero-dimension) size, are treated identically as "no
    /// resize noticed this time" by [`Screen`]'s detection — the safe
    /// direction when a channel cannot, or momentarily fails to, answer.
    ///
    /// # Errors
    ///
    /// [`CursesError::Io`](crate::CursesError::Io) if the channel is present but the query itself fails.
    fn size(&mut self) -> Result<Option<Size>> {
        Ok(None)
    }
}

/// How [`Screen::getch`] waits for input (curses `nodelay` / `timeout`).
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum InputMode {
    /// Block until an event is available (curses default).
    Blocking,
    /// Return immediately, yielding [`None`] when nothing is pending (curses
    /// `nodelay(true)`).
    NonBlocking,
    /// Wait up to the given duration, then give up (curses `timeout(ms)`).
    Timeout(Duration),
}

/// The curses screen driver, generic over its [`Tty`] channel.
pub struct Screen<T: Tty> {
    caps: Capabilities,
    tty: T,
    input: Input,
    pending: VecDeque<Event>,
    input_mode: InputMode,
    pairs: ColorPairs,
    staged: Buffer,
    physical: Buffer,
    cursor: CursorState,
    pending_resize: Option<Size>,
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
            pending: VecDeque::new(),
            input_mode: InputMode::Blocking,
            pairs: ColorPairs::new(),
            staged: Buffer::new(size),
            physical: Buffer::new(size),
            cursor: CursorState {
                visible: true,
                pos: Pos::ORIGIN,
            },
            pending_resize: None,
        }
    }

    /// The terminal's capabilities.
    #[must_use]
    pub const fn capabilities(&self) -> &Capabilities {
        &self.caps
    }

    /// Consume the driver and return its [`Tty`] channel.
    ///
    /// Used at shutdown to reclaim the underlying channel (and, in tests, to
    /// inspect what was written).
    #[must_use]
    pub fn into_tty(self) -> T {
        self.tty
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
    pub fn init_pair(&mut self, id: u16, fg: tairix_vt::Color, bg: tairix_vt::Color) -> Result<()> {
        self.pairs.init_pair(id, fg, bg)
    }

    /// Return the id of the colour pair `fg` on `bg`, defining it if needed
    /// (curses `alloc_pair`). An identical existing pair is reused, so
    /// requesting the same colours on every redraw never fills the table.
    ///
    /// # Errors
    ///
    /// [`CursesError::BadColorPair`](crate::CursesError::BadColorPair) if the
    /// pair is new and the table is full.
    pub fn alloc_pair(&mut self, fg: tairix_vt::Color, bg: tairix_vt::Color) -> Result<u16> {
        self.pairs.alloc_pair(fg, bg)
    }

    /// Attributes for `fg` on `bg` through the colour-pair table, or `None`
    /// when this terminal cannot show either colour or the pair table is
    /// exhausted — the caller falls back to a monochrome rendition (reverse
    /// video, bold, plain) rather than mis-colouring.
    ///
    /// An identical existing pair is reused (via [`Screen::alloc_pair`]), so
    /// requesting the same colours on every redraw never fills the table.
    pub fn colored_attributes(&mut self, fg: Color, bg: Color) -> Option<Attributes> {
        if !self.caps.color.supports(fg) || !self.caps.color.supports(bg) {
            return None;
        }
        let pair = self.pairs.alloc_pair(fg, bg).ok()?;
        let colors = self.pairs.get(pair);
        let mut attrs = Attributes::PLAIN;
        attrs.foreground = colors.fg;
        attrs.background = colors.bg;
        Some(attrs)
    }

    /// Select how [`Screen::getch`] waits for input (curses `nodelay` /
    /// `timeout`). The default is [`InputMode::Blocking`].
    pub fn set_input_mode(&mut self, mode: InputMode) {
        self.input_mode = mode;
    }

    /// The current input-wait mode.
    #[must_use]
    pub const fn input_mode(&self) -> InputMode {
        self.input_mode
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
    /// A terminal resize noticed here is applied before the diff, so the
    /// repaint is already sized correctly; the [`Event::Resize`] itself
    /// still reaches the application through [`Screen::getch`] /
    /// [`Screen::read_events`] on their next call.
    ///
    /// # Errors
    ///
    /// [`CursesError::Io`](crate::CursesError::Io) if the tty write fails.
    pub fn doupdate(&mut self) -> Result<()> {
        self.poll_resize();
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

    /// Resize the screen to `size` (curses `resizeterm`): the virtual screen
    /// preserves the cells that remain in range, but the physical diff base
    /// is reset to blank — after a terminal resize the terminal's actual
    /// on-screen contents can no longer be trusted — so the next
    /// [`Screen::doupdate`] re-emits every cell, the same reset
    /// [`Screen::enter_full_screen`] performs. The application's windows are
    /// resized separately with [`Window::resize`].
    pub fn resize(&mut self, size: Size) {
        self.staged.resize(size);
        self.physical = Buffer::new(size);
        let max = Pos::new(size.rows.saturating_sub(1), size.cols.saturating_sub(1));
        self.cursor.pos = Pos::new(
            self.cursor.pos.row.min(max.row),
            self.cursor.pos.col.min(max.col),
        );
    }

    /// Take over the display for a full-screen session (curses `initscr` /
    /// terminfo `smcup`): switch to the alternate screen buffer on a
    /// terminal that has one, then erase the display from the home
    /// position, so stale text from the previous session never shows
    /// through cells the application leaves blank. A terminal that can do
    /// neither (the dumb baseline) is left unchanged.
    ///
    /// The erase is emitted even alongside the alternate-screen switch:
    /// switching only presents a cleared buffer when the terminal was on
    /// the primary screen. A console a predecessor left on the alternate
    /// screen (a full-screen program that exited without leaving it) treats
    /// the switch as a no-op and keeps the predecessor's frame — xterm and
    /// the framebuffer console alike — so the display is erased explicitly
    /// rather than assumed blank.
    ///
    /// The physical diff base is reset to blank to match the now-empty
    /// display, so the next [`Screen::doupdate`] paints exactly what the
    /// application drew.
    ///
    /// # Errors
    ///
    /// [`CursesError::Io`](crate::CursesError::Io) if the tty write fails.
    pub fn enter_full_screen(&mut self) -> Result<()> {
        let mut ops = Vec::new();
        if self.caps.alt_screen {
            // Saves the primary screen for restoration on leave.
            ops.push(Op::EnterAltScreen);
        }
        if self.caps.erase {
            ops.push(Op::CursorPosition { row: 1, col: 1 });
            ops.push(Op::EraseInDisplay(EraseMode::All));
        }
        if ops.is_empty() {
            return Ok(());
        }
        self.write_ops(&ops)?;
        self.physical = Buffer::new(self.size());
        Ok(())
    }

    /// Give the display back after a full-screen session (curses `endwin` /
    /// terminfo `rmcup`): switch back to the main screen buffer on a
    /// terminal with an alternate screen, restoring whatever the session
    /// covered. A terminal without one keeps the session's final frame —
    /// there is nothing saved to restore it from.
    ///
    /// # Errors
    ///
    /// [`CursesError::Io`](crate::CursesError::Io) if the tty write fails.
    pub fn leave_full_screen(&mut self) -> Result<()> {
        if !self.caps.alt_screen {
            return Ok(());
        }
        self.write_ops(&[Op::LeaveAltScreen])?;
        // The restored primary content is unknown to the driver: reset the
        // diff base so a later redraw repaints from blank rather than
        // trusting stale knowledge of the covered screen.
        self.physical = Buffer::new(self.size());
        Ok(())
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

    /// Read the next input [`Event`] (curses `getch`), waiting according to
    /// the current [`InputMode`].
    ///
    /// A resize the tty has noticed since the last call is queued ahead of
    /// everything else, so a buffered event from an earlier decode, or a
    /// freshly decoded one, is returned only after it. Otherwise input is
    /// read — blocking, polling, or waiting up to a timeout per the mode —
    /// and decoded; the first decoded event is returned and any further
    /// events are buffered for the next call.
    ///
    /// In the blocking mode, a read whose bytes decode to no event (an
    /// unmodelled escape sequence the decoder consumed and dropped) is not
    /// an answer: the read repeats until an event arrives, so [`None`] means
    /// exactly one thing — the channel has closed (end of input). In the
    /// non-blocking and timeout modes [`None`] simply means no event was
    /// available within the mode's wait.
    ///
    /// # Errors
    ///
    /// [`CursesError::Io`](crate::CursesError::Io) if the tty read fails.
    pub fn getch(&mut self) -> Result<Option<Event>> {
        self.queue_pending_resize();
        loop {
            if let Some(event) = self.pending.pop_front() {
                return Ok(Some(event));
            }
            let bytes = match self.input_mode {
                InputMode::Blocking => self.tty.read_blocking()?,
                InputMode::NonBlocking => self.tty.read()?,
                InputMode::Timeout(timeout) => self.tty.read_timeout(timeout)?,
            };
            let pending = &mut self.pending;
            self.input.feed(&bytes, |event| pending.push_back(event));
            if pending.is_empty()
                && (bytes.is_empty() || !matches!(self.input_mode, InputMode::Blocking))
            {
                return Ok(None);
            }
        }
    }

    /// Read all currently pending input and decode it into [`Event`]s (a
    /// batched, non-blocking `getch`).
    ///
    /// A resize the tty has noticed since the last call leads the result,
    /// ahead of any event buffered by an earlier [`Screen::getch`] and of
    /// anything freshly read here, so the two readers share one stream.
    ///
    /// # Errors
    ///
    /// [`CursesError::Io`](crate::CursesError::Io) if the tty read fails.
    pub fn read_events(&mut self) -> Result<Vec<Event>> {
        self.queue_pending_resize();
        let bytes = self.tty.read()?;
        let mut events: Vec<Event> = self.pending.drain(..).collect();
        self.input.feed(&bytes, |event| events.push(event));
        Ok(events)
    }

    /// Test-only borrow of the underlying channel, so a test can prove
    /// [`Screen::getch`] dispatches to the right [`Tty`] read method per
    /// [`InputMode`].
    #[cfg(test)]
    pub(crate) fn tty_ref(&self) -> &T {
        &self.tty
    }

    /// Ask the tty for its current size and, if it has genuinely changed,
    /// resize the screen to match and latch the change as a pending
    /// [`Event::Resize`] — the one detection point [`Screen::doupdate`],
    /// [`Screen::getch`], and [`Screen::read_events`] all call.
    ///
    /// `Ok(None)`, an [`Err`], a degenerate (zero-dimension) size, and a
    /// size equal to the screen's current one are all "nothing changed": a
    /// channel that cannot report, or momentarily fails to, never
    /// manufactures a resize.
    fn poll_resize(&mut self) {
        let reported = match self.tty.size() {
            Ok(Some(size)) if !size.is_empty() => size,
            _ => return,
        };
        if reported == self.size() {
            return;
        }
        self.resize(reported);
        self.pending_resize = Some(reported);
    }

    /// Poll for a resize, then move any pending one to the front of the
    /// input queue, ahead of everything else — the one funnel
    /// [`Screen::getch`] and [`Screen::read_events`] share, so a resize
    /// noticed by either (or latched earlier by [`Screen::doupdate`]) is
    /// never missed and never delivered twice.
    fn queue_pending_resize(&mut self) {
        self.poll_resize();
        if let Some(size) = self.pending_resize.take() {
            self.pending.push_front(Event::Resize(size));
        }
    }

    /// Encode `ops` and write them to the tty, writing nothing when `ops` is
    /// empty.
    fn write_ops(&mut self, ops: &[Op]) -> Result<()> {
        if ops.is_empty() {
            return Ok(());
        }
        let mut bytes = Vec::new();
        encode_all_into(ops, &mut bytes);
        self.tty.write(&bytes)
    }
}

/// The reserved default colour-pair id, re-exported for ergonomic use at the
/// driver surface.
pub const DEFAULT_COLOR_PAIR: u16 = DEFAULT_PAIR;
