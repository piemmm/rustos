//! The byte-stream control state machine that drives a [`Grid`].
//!
//! [`Parser`] consumes the shell's output one byte at a time and turns it into
//! [`Grid`] operations. It recognises:
//!
//! * **Printable ASCII** (`0x20..=0x7e`) — written to the grid at the cursor.
//! * **C0 controls** — backspace, horizontal tab, line feed, and carriage
//!   return move the cursor; the bell and the remaining C0 controls are
//!   consumed and ignored (there is no audible bell wired).
//! * **A subset of ANSI CSI escape sequences** (`ESC [` … final) — cursor
//!   movement (`A`/`B`/`C`/`D`), absolute positioning (`H`/`f`),
//!   erase-in-line (`K`), and erase-in-display (`J`).
//!
//! Anything else — a byte `>= 0x80`, an unrecognised escape, or an
//! unsupported CSI final byte — is consumed without disturbing the screen, so
//! a stream the terminal does not fully understand degrades to dropped control
//! rather than a corrupted display or a panic (`AGENTS.md` §2.9). Holding the
//! escape-sequence state in the parser (rather than the grid) keeps the screen
//! model free of parsing concerns (`AGENTS.md` §2.3).

use alloc::vec::Vec;

use crate::grid::Grid;

/// Escape introducer (`ESC`).
const ESC: u8 = 0x1b;
/// Control Sequence Introducer second byte (`[`).
const CSI: u8 = b'[';

/// The largest value a CSI numeric parameter accumulates to; further digits
/// saturate here so a long run of digits cannot overflow (`AGENTS.md` §2.9).
const PARAM_MAX: u32 = 0xffff;

/// Where the parser is in the byte stream.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
enum State {
    /// Ordinary text and C0 controls.
    Ground,
    /// Just saw `ESC`; expecting `[` to begin a CSI sequence.
    Escape,
    /// Inside a CSI sequence, collecting parameters until the final byte.
    Csi,
}

/// A streaming interpreter from shell output bytes to [`Grid`] operations.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Parser {
    state: State,
    params: Vec<u16>,
    accumulator: u32,
}

impl Default for Parser {
    fn default() -> Self {
        Self::new()
    }
}

impl Parser {
    /// A fresh parser in the ground state.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            state: State::Ground,
            params: Vec::new(),
            accumulator: 0,
        }
    }

    /// Feed one `byte` of shell output, applying its effect to `grid`.
    pub fn feed_byte(&mut self, grid: &mut Grid, byte: u8) {
        match self.state {
            State::Ground => self.ground(grid, byte),
            State::Escape => self.escape(byte),
            State::Csi => self.csi(grid, byte),
        }
    }

    /// Feed a slice of shell output, applying each byte in turn.
    pub fn feed(&mut self, grid: &mut Grid, bytes: &[u8]) {
        for &byte in bytes {
            self.feed_byte(grid, byte);
        }
    }

    /// Handle a byte while in the ground state.
    fn ground(&mut self, grid: &mut Grid, byte: u8) {
        match byte {
            ESC => self.begin_escape(),
            0x08 => grid.backspace(),
            0x09 => grid.tab(),
            0x0a => grid.line_feed(),
            0x0d => grid.carriage_return(),
            0x20..=0x7e => grid.write_char(char::from(byte)),
            // Bell, the remaining C0 controls, DEL, and high bytes carry no
            // screen effect we model; drop them rather than emit garbage.
            _ => {}
        }
    }

    /// Handle the byte after `ESC`.
    fn escape(&mut self, byte: u8) {
        if byte == CSI {
            self.state = State::Csi;
        } else {
            // A non-CSI escape sequence: we model none of them, so consume the
            // single byte and return to the ground state.
            self.state = State::Ground;
        }
    }

    /// Handle a byte inside a CSI sequence.
    fn csi(&mut self, grid: &mut Grid, byte: u8) {
        match byte {
            b'0'..=b'9' => {
                let digit = u32::from(byte - b'0');
                self.accumulator = (self.accumulator * 10 + digit).min(PARAM_MAX);
            }
            b';' => self.push_param(),
            0x40..=0x7e => {
                self.push_param();
                self.dispatch(grid, byte);
                self.reset();
            }
            // Intermediates and private markers (e.g. `?`): consume and keep
            // collecting until the final byte.
            0x20..=0x3f => {}
            // Anything else aborts the malformed sequence.
            _ => self.reset(),
        }
    }

    /// Commit the parameter currently being accumulated.
    fn push_param(&mut self) {
        let value = u16::try_from(self.accumulator).unwrap_or(u16::MAX);
        self.params.push(value);
        self.accumulator = 0;
    }

    /// Apply the completed CSI sequence whose final byte is `final_byte`.
    fn dispatch(&mut self, grid: &mut Grid, final_byte: u8) {
        match final_byte {
            b'A' => grid.move_up(self.distance()),
            b'B' => grid.move_down(self.distance()),
            b'C' => grid.move_right(self.distance()),
            b'D' => grid.move_left(self.distance()),
            b'H' | b'f' => {
                let row = self.coordinate(0);
                let col = self.coordinate(1);
                grid.move_to(col, row);
            }
            b'J' => grid.erase_in_display(self.mode()),
            b'K' => grid.erase_in_line(self.mode()),
            _ => {}
        }
    }

    /// A movement distance: the first parameter, with a missing or zero value
    /// meaning one step (ANSI's default).
    fn distance(&self) -> u16 {
        self.params
            .first()
            .copied()
            .filter(|&v| v != 0)
            .unwrap_or(1)
    }

    /// A 0-based cursor coordinate from the 1-based parameter at `index`, with
    /// a missing or zero value meaning the home position.
    fn coordinate(&self, index: usize) -> u16 {
        let one_based = self
            .params
            .get(index)
            .copied()
            .filter(|&v| v != 0)
            .unwrap_or(1);
        one_based.saturating_sub(1)
    }

    /// An erase mode: the first parameter, defaulting to `0`.
    fn mode(&self) -> u16 {
        self.params.first().copied().unwrap_or(0)
    }

    /// Drop any accumulated state and return to the ground state.
    fn reset(&mut self) {
        self.state = State::Ground;
        self.params.clear();
        self.accumulator = 0;
    }

    /// Enter the escape state, discarding any stale parameter accumulation.
    fn begin_escape(&mut self) {
        self.state = State::Escape;
        self.params.clear();
        self.accumulator = 0;
    }
}
