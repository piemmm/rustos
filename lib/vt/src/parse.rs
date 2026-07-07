//! The streaming parser: ANSI / VT / xterm bytes back into [`Op`] events.
//!
//! [`Parser`] is a byte-at-a-time state machine. It is the inverse of the
//! [`crate::emit`] emitter over the same tables, so every operation the emitter
//! writes parses back to the identical [`Op`].
//!
//! The parser is total. A terminal consumes bytes it did not produce — local
//! shell output and, in the remote stages of `plans/CURSES.md`, a foreign
//! host's output — so every input must be handled without panic or
//! out-of-bounds access. Numeric parameters saturate
//! ([`PARAM_MAX`]), the parameter and string buffers are bounded
//! ([`MAX_PARAMS`], [`MAX_STRING`]), and an unrecognised, oversized, or
//! malformed sequence is consumed and dropped rather than corrupting state.

use crate::attr::decode_params;
use crate::control;
use crate::key::Key;
use crate::mouse::{MouseMode, MouseReport};
use crate::op::{EraseMode, Op, Title};

/// The largest value a numeric CSI parameter accumulates to; further digits
/// saturate here so a long digit run cannot overflow.
pub const PARAM_MAX: u32 = 0xffff;

/// The largest number of CSI parameters retained; further parameters are
/// consumed but not stored, bounding the parameter buffer.
pub const MAX_PARAMS: usize = 64;

/// The largest OSC/DCS string body retained, in bytes; further bytes are
/// consumed but not stored, bounding the string buffer.
pub const MAX_STRING: usize = 4096;

/// Which kind of string the parser is collecting between an introducer and its
/// terminator.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
enum StringKind {
    /// An Operating System Command (`ESC ] … ST`) — may set the title.
    Osc,
    /// A Device Control String (`ESC P … ST`) — content is not modelled and is
    /// consumed and dropped.
    Dcs,
}

/// Where the parser is in the byte stream.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
enum State {
    /// Ordinary text and C0 controls.
    Ground,
    /// Mid-way through a multi-byte UTF-8 scalar.
    Utf8,
    /// Just saw `ESC`; deciding what kind of sequence follows.
    Escape,
    /// Just saw `ESC O` (`SS3`); the next byte names a function/editing key.
    Ss3,
    /// Inside a CSI sequence, collecting parameters until the final byte.
    Csi,
    /// Inside an OSC or DCS string, collecting until the terminator.
    Str(StringKind),
}

/// A streaming interpreter from terminal bytes to [`Op`] events.
///
/// The parameter and string buffers are fixed-size inline arrays bounded by
/// [`MAX_PARAMS`] and [`MAX_STRING`], so the parser holds no heap allocation
/// and runs on an allocator-free target (the framebuffer boot console). The
/// bounds are the same ones that keep the parser total against hostile input:
/// a parameter or string byte past the bound is consumed and dropped.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Parser {
    state: State,
    params: [u16; MAX_PARAMS],
    params_len: usize,
    accumulator: u32,
    private: bool,
    mouse_sgr: bool,
    utf8_remaining: u8,
    utf8_acc: u32,
    utf8_min: u32,
    str_buf: [u8; MAX_STRING],
    str_len: usize,
    str_esc: bool,
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
            params: [0; MAX_PARAMS],
            params_len: 0,
            accumulator: 0,
            private: false,
            mouse_sgr: false,
            utf8_remaining: 0,
            utf8_acc: 0,
            utf8_min: 0,
            str_buf: [0; MAX_STRING],
            str_len: 0,
            str_esc: false,
        }
    }

    /// The parameters accumulated for the CSI sequence in progress.
    fn params(&self) -> &[u16] {
        &self.params[..self.params_len]
    }

    /// Feed a slice of bytes, invoking `sink` once per recognised [`Op`] in
    /// stream order.
    pub fn feed(&mut self, bytes: &[u8], mut sink: impl FnMut(Op)) {
        for &byte in bytes {
            self.process(byte, &mut sink);
        }
    }

    /// Feed one byte, invoking `sink` for each [`Op`] it completes.
    pub fn feed_byte(&mut self, byte: u8, mut sink: impl FnMut(Op)) {
        self.process(byte, &mut sink);
    }

    /// Whether the parser is at a byte-stream boundary (the ground state,
    /// not mid-way through an escape sequence, a UTF-8 scalar, or a string).
    ///
    /// An input decoder uses this to tell a C0 control byte that *is* a
    /// keystroke (a control-chorded key pressed at the keyboard) from one
    /// that aborts a sequence in progress: only at the ground state is the
    /// byte an independent key.
    #[must_use]
    pub const fn is_ground(&self) -> bool {
        matches!(self.state, State::Ground)
    }

    /// Resolve a bare `ESC` left dangling at the end of a read: if the last
    /// byte fed was an `ESC` that nothing has followed (the escape state
    /// with no sequence kind decided), return to the ground state and
    /// report `true`; otherwise leave the parser untouched and report
    /// `false`.
    ///
    /// A terminal sends the Escape *key* as a single `0x1b` byte — the same
    /// byte that introduces every escape sequence — so a byte-at-a-time
    /// parser alone cannot tell the key from a sequence still in flight. The
    /// discriminator is arrival: a sequence's continuation bytes arrive in
    /// the same read as their introducer, so an `ESC` that ends a read with
    /// nothing after it was the key. An input decoder calls this once per
    /// read chunk (the same boundary `ncurses` resolves with `ESCDELAY`).
    pub fn take_pending_escape(&mut self) -> bool {
        if matches!(self.state, State::Escape) {
            self.state = State::Ground;
            return true;
        }
        false
    }

    /// Dispatch one byte to the current state's handler.
    fn process(&mut self, byte: u8, sink: &mut impl FnMut(Op)) {
        match self.state {
            State::Ground => self.ground(byte, sink),
            State::Utf8 => self.utf8(byte, sink),
            State::Escape => self.escape(byte, sink),
            State::Ss3 => self.ss3(byte, sink),
            State::Csi => self.csi(byte, sink),
            State::Str(kind) => self.string(kind, byte, sink),
        }
    }

    /// Handle a byte in the ground state.
    fn ground(&mut self, byte: u8, sink: &mut impl FnMut(Op)) {
        match byte {
            control::ESC => self.state = State::Escape,
            control::BEL => sink(Op::Bell),
            control::BS => sink(Op::Backspace),
            control::HT => sink(Op::Tab),
            control::LF => sink(Op::LineFeed),
            control::CR => sink(Op::CarriageReturn),
            0x20..=0x7e => sink(Op::Print(char::from(byte))),
            // Other C0 controls and DEL carry no operation we model.
            0x00..=0x1f | 0x7f => {}
            // A UTF-8 lead byte begins a multi-byte scalar.
            _ => self.begin_utf8(byte),
        }
    }

    /// Begin decoding a multi-byte UTF-8 scalar from its lead byte.
    fn begin_utf8(&mut self, lead: u8) {
        let (remaining, acc, min) = match lead {
            0xc0..=0xdf => (1u8, u32::from(lead & 0x1f), 0x80),
            0xe0..=0xef => (2u8, u32::from(lead & 0x0f), 0x800),
            0xf0..=0xf7 => (3u8, u32::from(lead & 0x07), 0x1_0000),
            // A stray continuation byte or an invalid lead: not the start of a
            // scalar, so drop it and stay in the ground state.
            _ => return,
        };
        self.utf8_remaining = remaining;
        self.utf8_acc = acc;
        self.utf8_min = min;
        self.state = State::Utf8;
    }

    /// Handle a UTF-8 continuation byte.
    fn utf8(&mut self, byte: u8, sink: &mut impl FnMut(Op)) {
        if !matches!(byte, 0x80..=0xbf) {
            // Not a continuation byte: the scalar is truncated. Drop it and
            // reprocess this byte from the ground state so an `ESC` (or any
            // other meaningful byte) is not lost.
            self.state = State::Ground;
            self.ground(byte, sink);
            return;
        }
        self.utf8_acc = (self.utf8_acc << 6) | u32::from(byte & 0x3f);
        self.utf8_remaining -= 1;
        if self.utf8_remaining > 0 {
            return;
        }
        self.state = State::Ground;
        // Accept only a correctly-encoded, non-overlong scalar value.
        if self.utf8_acc >= self.utf8_min {
            if let Some(ch) = char::from_u32(self.utf8_acc) {
                sink(Op::Print(ch));
            }
        }
    }

    /// Handle the byte after `ESC`.
    fn escape(&mut self, byte: u8, sink: &mut impl FnMut(Op)) {
        match byte {
            control::CSI => self.begin_csi(),
            control::SS3 => self.state = State::Ss3,
            control::OSC => self.begin_string(StringKind::Osc),
            control::DCS => self.begin_string(StringKind::Dcs),
            control::SAVE_CURSOR => {
                self.state = State::Ground;
                sink(Op::SaveCursor);
            }
            control::RESTORE_CURSOR => {
                self.state = State::Ground;
                sink(Op::RestoreCursor);
            }
            // A fresh `ESC` restarts the escape; anything else is an escape we
            // do not model, so consume it and return to the ground state.
            control::ESC => {}
            _ => self.state = State::Ground,
        }
    }

    /// Handle the byte after `ESC O` (`SS3`): a single final byte that names a
    /// function or editing key. An unrecognised byte fails closed and is
    /// dropped.
    fn ss3(&mut self, byte: u8, sink: &mut impl FnMut(Op)) {
        self.state = State::Ground;
        if let Some(key) = Key::from_ss3_final(byte) {
            sink(Op::Key(key));
        }
    }

    /// Enter the CSI state with cleared parameter accumulation.
    fn begin_csi(&mut self) {
        self.state = State::Csi;
        self.params_len = 0;
        self.accumulator = 0;
        self.private = false;
        self.mouse_sgr = false;
    }

    /// Handle a byte inside a CSI sequence.
    fn csi(&mut self, byte: u8, sink: &mut impl FnMut(Op)) {
        match byte {
            b'0'..=b'9' => {
                let digit = u32::from(byte - b'0');
                self.accumulator = (self.accumulator * 10 + digit).min(PARAM_MAX);
            }
            control::SEPARATOR => self.push_param(),
            control::PRIVATE => self.private = true,
            control::MOUSE_SGR => self.mouse_sgr = true,
            0x40..=0x7e => {
                self.push_param();
                self.dispatch(byte, sink);
                self.state = State::Ground;
            }
            // Other intermediates and private markers: consume and keep going.
            0x20..=0x3f => {}
            // Anything else (e.g. a C0 control or `ESC`) aborts the sequence;
            // reprocess the byte from the ground state.
            _ => {
                self.state = State::Ground;
                self.ground(byte, sink);
            }
        }
    }

    /// Commit the parameter currently being accumulated, dropping it once the
    /// buffer is full.
    fn push_param(&mut self) {
        if self.params_len < MAX_PARAMS {
            let value = u16::try_from(self.accumulator).unwrap_or(u16::MAX);
            self.params[self.params_len] = value;
            self.params_len += 1;
        }
        self.accumulator = 0;
    }

    /// Apply the completed CSI sequence whose final byte is `final_byte`.
    fn dispatch(&mut self, final_byte: u8, sink: &mut impl FnMut(Op)) {
        match final_byte {
            control::CUU => sink(Op::CursorUp(self.count())),
            control::CUD => sink(Op::CursorDown(self.count())),
            control::CUF => sink(Op::CursorForward(self.count())),
            control::CUB => sink(Op::CursorBack(self.count())),
            control::CNL => sink(Op::CursorNextLine(self.count())),
            control::CPL => sink(Op::CursorPrevLine(self.count())),
            control::CHA => sink(Op::CursorColumn(self.position(0))),
            control::CUP | control::HVP => sink(Op::CursorPosition {
                row: self.position(0),
                col: self.position(1),
            }),
            control::ED => {
                if let Some(mode) = EraseMode::from_value(self.mode()) {
                    sink(Op::EraseInDisplay(mode));
                }
            }
            control::EL => {
                if let Some(mode) = EraseMode::from_value(self.mode()) {
                    sink(Op::EraseInLine(mode));
                }
            }
            control::SU => sink(Op::ScrollUp(self.count())),
            control::SD => sink(Op::ScrollDown(self.count())),
            control::DECSTBM => self.dispatch_scroll_region(sink),
            // The SGR mouse arms must precede `SGR`, since `MOUSE_RELEASE`
            // (`m`) is the same byte as the SGR final.
            control::MOUSE_PRESS if self.mouse_sgr => self.dispatch_mouse(true, sink),
            control::MOUSE_RELEASE if self.mouse_sgr => self.dispatch_mouse(false, sink),
            control::SGR => decode_params(self.params(), |sgr| sink(Op::Sgr(sgr))),
            control::SET_MODE => self.dispatch_mode(true, sink),
            control::RESET_MODE => self.dispatch_mode(false, sink),
            control::TILDE => self.dispatch_tilde(sink),
            _ => {}
        }
    }

    /// Dispatch a `CSI <n> ~` sequence: a bracketed-paste marker or a named
    /// function / editing key. An unrecognised parameter fails closed and is
    /// dropped.
    fn dispatch_tilde(&self, sink: &mut impl FnMut(Op)) {
        match self.params().first().copied() {
            Some(control::PASTE_START) => sink(Op::PasteStart),
            Some(control::PASTE_END) => sink(Op::PasteEnd),
            Some(param) => {
                if let Some(key) = Key::from_tilde_param(param) {
                    sink(Op::Key(key));
                }
            }
            None => {}
        }
    }

    /// Dispatch a `CSI < Cb ; Cx ; Cy M` (press) or `… m` (release) SGR mouse
    /// report. A report missing its coordinates is malformed and dropped.
    fn dispatch_mouse(&self, pressed: bool, sink: &mut impl FnMut(Op)) {
        let (Some(&cb), Some(&col), Some(&row)) = (
            self.params().first(),
            self.params().get(1),
            self.params().get(2),
        ) else {
            return;
        };
        sink(Op::Mouse(MouseReport::decode(cb, col, row, pressed)));
    }

    /// Dispatch `DECSTBM`: two parameters set the region, fewer reset it.
    fn dispatch_scroll_region(&self, sink: &mut impl FnMut(Op)) {
        if self.params_len >= 2 {
            sink(Op::SetScrollRegion {
                top: self.position(0),
                bottom: self.position(1),
            });
        } else {
            sink(Op::ResetScrollRegion);
        }
    }

    /// Dispatch a DEC private mode set/reset (`CSI ? n h` / `l`).
    fn dispatch_mode(&self, set: bool, sink: &mut impl FnMut(Op)) {
        if !self.private {
            return;
        }
        let op = match self.params().first().copied() {
            Some(control::MODE_CURSOR_VISIBLE) => {
                if set {
                    Op::ShowCursor
                } else {
                    Op::HideCursor
                }
            }
            Some(control::MODE_ALT_SCREEN) => {
                if set {
                    Op::EnterAltScreen
                } else {
                    Op::LeaveAltScreen
                }
            }
            Some(control::MODE_BRACKETED_PASTE) => Op::SetBracketedPaste(set),
            Some(number) => match MouseMode::from_mode_number(number) {
                Some(mode) => Op::SetMouseMode { mode, enable: set },
                None => return,
            },
            None => return,
        };
        sink(op);
    }

    /// A movement count: the first parameter, with a missing or zero value
    /// meaning `1` (ANSI's default).
    fn count(&self) -> u16 {
        self.params()
            .first()
            .copied()
            .filter(|&v| v != 0)
            .unwrap_or(1)
    }

    /// A 1-based position parameter at `index`, with a missing or zero value
    /// meaning `1` (the home coordinate).
    fn position(&self, index: usize) -> u16 {
        self.params()
            .get(index)
            .copied()
            .filter(|&v| v != 0)
            .unwrap_or(1)
    }

    /// An erase mode value: the first parameter, defaulting to `0`.
    fn mode(&self) -> u16 {
        self.params().first().copied().unwrap_or(0)
    }

    /// Enter a string (OSC/DCS) collection state with an empty buffer.
    fn begin_string(&mut self, kind: StringKind) {
        self.state = State::Str(kind);
        self.str_len = 0;
        self.str_esc = false;
    }

    /// Handle a byte inside an OSC or DCS string.
    fn string(&mut self, kind: StringKind, byte: u8, sink: &mut impl FnMut(Op)) {
        if self.str_esc {
            self.str_esc = false;
            if byte == control::ST_FINAL {
                self.finish_string(kind, sink);
            } else {
                // A lone `ESC` did not begin a String Terminator: the string is
                // malformed. Drop it and reprocess the byte from the ground
                // state.
                self.state = State::Ground;
                self.ground(byte, sink);
            }
            return;
        }
        match byte {
            control::BEL => self.finish_string(kind, sink),
            control::ESC => self.str_esc = true,
            _ => {
                if self.str_len < MAX_STRING {
                    self.str_buf[self.str_len] = byte;
                    self.str_len += 1;
                }
            }
        }
    }

    /// Terminate a string and, for an OSC that set the title, emit it as a
    /// bounded inline [`Title`] (no heap).
    fn finish_string(&mut self, kind: StringKind, sink: &mut impl FnMut(Op)) {
        self.state = State::Ground;
        if kind == StringKind::Osc {
            if let Some(title) = title_from_osc(&self.str_buf[..self.str_len]) {
                sink(Op::SetTitle(title));
            }
        }
        self.str_len = 0;
    }
}

/// Extract a window title from an OSC body of the form `Ps ; text`, accepting
/// the title-setting commands `0` (icon + title) and `2` (title). Returns
/// `None` for any other command or a body without a `;` separator.
fn title_from_osc(body: &[u8]) -> Option<Title> {
    let separator = body.iter().position(|&b| b == control::SEPARATOR)?;
    let (command, rest) = body.split_at(separator);
    let text = rest.get(1..)?;
    if matches!(command, b"0" | b"2") {
        Some(Title::from_bytes(text))
    } else {
        None
    }
}
