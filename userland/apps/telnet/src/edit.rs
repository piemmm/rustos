//! The client-side line editor RFC 1184 `EDIT` mode requires.
//!
//! In LINEMODE `EDIT` the server sees only finished lines, so the *client* owns
//! the editing and the echo — and which characters do the editing is whatever
//! the SLC negotiation settled on, not a fixed set. That is why this is its own
//! editor rather than a use of [`tairix_vt::LineEditor`]: that one's erase set
//! is fixed and is shared with the kernel console reader and `login`, and
//! threading a server-negotiated table into it would put telnet policy in a
//! crate the console links.
//!
//! It is *assembled* from `lib/vt`'s shared vocabulary rather than
//! re-implementing it: the control-byte spellings and the Delete key's
//! `CSI 3 ~` recogniser are `lib/vt`'s single definition, so telnet agrees with
//! the rest of the system about which keystroke rubs one character out.

use alloc::vec::Vec;

use tairix_vt::control;
use tairix_vt::line::EraseSeq;

use crate::linemode::{slc, Linemode};

/// Longest line the editor accumulates before forwarding it unterminated.
///
/// A fixed bound on attacker-independent but unbounded local input: a user (or
/// a paste) that never presses Return must not grow the buffer without limit,
/// and forwarding early is what a real terminal does when its buffer fills.
pub const MAX_LINE: usize = 1024;

/// Columns a Tab advances to when the editor expands it under `SOFT_TAB`.
const TAB_WIDTH: usize = 8;

/// What feeding one keystroke to the editor asks the session to do.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EditAction {
    /// The line is still being edited.
    Pending,
    /// The user ended the line: forward [`Editor::line`] followed by the NVT
    /// end-of-line, then call [`Editor::take_line`].
    Line,
    /// A forwarding character (RFC 1184 `FORWARDMASK`) or a full buffer:
    /// forward [`Editor::line`] with no end-of-line, then take it.
    Forward,
    /// An SLC signal function fired; its code is the payload. The session maps
    /// it to the telnet command and decides what to flush.
    Signal(u8),
}

/// The line being edited, with the echo state one line needs.
#[derive(Debug, Default)]
pub struct Editor {
    line: Vec<u8>,
    /// Rendered column of the cursor, so an erase rubs out exactly the columns
    /// the echo painted (a control byte shown as `^X` occupies two).
    column: usize,
    seq: EraseSeq,
    /// The next byte is literal, whatever it would otherwise mean (the SLC
    /// `LNEXT` function).
    literal_next: bool,
}

impl Editor {
    /// A fresh editor holding an empty line.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            line: Vec::new(),
            column: 0,
            seq: EraseSeq::new(),
            literal_next: false,
        }
    }

    /// The line accumulated so far.
    #[must_use]
    pub fn line(&self) -> &[u8] {
        &self.line
    }

    /// Whether anything is being edited.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.line.is_empty()
    }

    /// Take the accumulated line, resetting the editor for the next one.
    pub fn take_line(&mut self) -> Vec<u8> {
        self.column = 0;
        self.literal_next = false;
        core::mem::take(&mut self.line)
    }

    /// Discard the line being edited without forwarding it.
    pub fn discard(&mut self) {
        self.line.clear();
        self.column = 0;
        self.literal_next = false;
        self.seq.reset();
    }

    /// Feed one keystroke, appending whatever the user should see to `echo`.
    ///
    /// `lm` supplies the negotiated SLC table and the `SOFT_TAB` / `LIT_ECHO` /
    /// `TRAPSIG` mode bits, so the editing characters are the ones the server
    /// asked for rather than a hard-coded set.
    pub fn push(&mut self, byte: u8, lm: &Linemode, echo: &mut Vec<u8>) -> EditAction {
        if self.literal_next {
            self.literal_next = false;
            self.insert(byte, lm, echo);
            return EditAction::Pending;
        }

        // The Delete key arrives as a multi-byte sequence that may be split
        // across reads; the shared recogniser holds it so a keypress erases
        // once instead of painting escape glyphs.
        let step = self.seq.feed(byte);
        if step.erase() {
            self.erase_char(lm, echo);
            return EditAction::Pending;
        }
        let mut action = EditAction::Pending;
        // `literal()` is at most the held sequence plus the current byte, and
        // an action from an earlier byte is returned as soon as it is produced,
        // so a released prefix can never be silently dropped behind one.
        for &literal in step.literal() {
            match self.push_literal(literal, lm, echo) {
                EditAction::Pending => {}
                terminal => {
                    action = terminal;
                    break;
                }
            }
        }
        action
    }

    /// Apply one already-disambiguated byte.
    fn push_literal(&mut self, byte: u8, lm: &Linemode, echo: &mut Vec<u8>) -> EditAction {
        let table = lm.slc();
        if let Some(function) = table.function_for(byte) {
            match function {
                slc::EC => {
                    self.erase_char(lm, echo);
                    return EditAction::Pending;
                }
                slc::EL => {
                    self.erase_line(echo);
                    return EditAction::Pending;
                }
                slc::EW => {
                    self.erase_word(lm, echo);
                    return EditAction::Pending;
                }
                slc::RP => {
                    self.reprint(lm, echo);
                    return EditAction::Pending;
                }
                slc::LNEXT => {
                    self.literal_next = true;
                    return EditAction::Pending;
                }
                // The signal functions are the server's to hear, but only when
                // it asked for that mapping; otherwise they are ordinary data.
                slc::IP
                | slc::BRK
                | slc::ABORT
                | slc::SUSP
                | slc::EOF
                | slc::AO
                | slc::AYT
                | slc::EOR
                | slc::SYNCH
                | slc::XON
                | slc::XOFF
                    if lm.trapsig() =>
                {
                    return EditAction::Signal(function)
                }
                _ => {}
            }
        }

        // A single-byte Backspace is an erase even when the server bound `EC`
        // elsewhere: a terminal whose Backspace key did nothing would be
        // unusable, and `lib/vt` owns which bytes those are.
        if control::is_line_erase(byte) {
            self.erase_char(lm, echo);
            return EditAction::Pending;
        }

        if byte == control::CR || byte == control::LF {
            echo.extend_from_slice(b"\r\n");
            self.column = 0;
            return EditAction::Line;
        }

        self.insert(byte, lm, echo);
        if lm.forwards(byte) {
            return EditAction::Forward;
        }
        if self.line.len() >= MAX_LINE {
            return EditAction::Forward;
        }
        EditAction::Pending
    }

    /// Append `byte` to the line and echo it.
    fn insert(&mut self, byte: u8, lm: &Linemode, echo: &mut Vec<u8>) {
        if byte == control::HT && lm.soft_tab() {
            let spaces = TAB_WIDTH - (self.column % TAB_WIDTH);
            for _ in 0..spaces {
                if self.line.len() == MAX_LINE {
                    break;
                }
                self.line.push(b' ');
                echo.push(b' ');
                self.column += 1;
            }
            return;
        }
        if self.line.len() == MAX_LINE {
            return;
        }
        self.line.push(byte);
        self.echo_one(byte, lm, echo);
    }

    /// Echo one stored byte, as `^X` unless the server asked for literal echo.
    fn echo_one(&mut self, byte: u8, lm: &Linemode, echo: &mut Vec<u8>) {
        match render(byte, lm.lit_echo()) {
            Render::Literal => {
                echo.push(byte);
                self.column += 1;
            }
            Render::Caret(shown) => {
                echo.extend_from_slice(&[b'^', shown]);
                self.column += 2;
            }
            Render::Opaque => {
                echo.push(byte);
                // A Tab's advance depends on the terminal's stops, so the
                // column becomes unknowable; snapping to the next stop keeps
                // the estimate monotonic and the reprint path handles erasure.
                self.column += TAB_WIDTH - (self.column % TAB_WIDTH);
            }
        }
    }

    /// Rub out the last byte of the line.
    fn erase_char(&mut self, lm: &Linemode, echo: &mut Vec<u8>) {
        let Some(&last) = self.line.last() else {
            return;
        };
        match render(last, lm.lit_echo()) {
            Render::Literal => {
                self.line.pop();
                echo.extend_from_slice(&control::ERASE_ECHO);
                self.column = self.column.saturating_sub(1);
            }
            Render::Caret(_) => {
                self.line.pop();
                echo.extend_from_slice(&control::ERASE_ECHO);
                echo.extend_from_slice(&control::ERASE_ECHO);
                self.column = self.column.saturating_sub(2);
            }
            // A byte whose painted width the client cannot know (a Tab the
            // terminal expanded to its own stops) cannot be rubbed out one
            // column at a time, so the line is repainted instead of guessed at.
            Render::Opaque => {
                self.line.pop();
                self.reprint(lm, echo);
            }
        }
    }

    /// Erase the whole line (the SLC `EL` "kill" function).
    fn erase_line(&mut self, echo: &mut Vec<u8>) {
        for _ in 0..self.column {
            echo.extend_from_slice(&control::ERASE_ECHO);
        }
        self.line.clear();
        self.column = 0;
    }

    /// Erase the word before the cursor: any trailing blanks, then the
    /// non-blank run before them.
    fn erase_word(&mut self, lm: &Linemode, echo: &mut Vec<u8>) {
        while self
            .line
            .last()
            .is_some_and(|&b| b == b' ' || b == control::HT)
        {
            self.erase_char(lm, echo);
        }
        while self
            .line
            .last()
            .is_some_and(|&b| b != b' ' && b != control::HT)
        {
            self.erase_char(lm, echo);
        }
    }

    /// Repaint the line on a fresh row (the SLC `RP` function).
    fn reprint(&mut self, lm: &Linemode, echo: &mut Vec<u8>) {
        echo.extend_from_slice(b"\r\n");
        self.column = 0;
        let line = core::mem::take(&mut self.line);
        for &byte in &line {
            self.echo_one(byte, lm, echo);
        }
        self.line = line;
    }
}

/// How one stored byte is painted.
enum Render {
    /// As itself, one column.
    Literal,
    /// As `^` plus the carried byte, two columns.
    Caret(u8),
    /// As itself, but occupying a width only the terminal knows.
    Opaque,
}

/// Decide how `byte` is painted. Control bytes show as `^X` so a stray control
/// character is visible rather than moving the cursor, unless the server asked
/// for literal echo (RFC 1184 `LIT_ECHO`).
fn render(byte: u8, lit_echo: bool) -> Render {
    if byte == control::HT {
        return Render::Opaque;
    }
    if lit_echo {
        return Render::Literal;
    }
    match byte {
        // `^@`..`^_` for the C0 controls, `^?` for Delete.
        0x00..=0x1F => Render::Caret(byte | 0x40),
        control::DEL => Render::Caret(b'?'),
        _ => Render::Literal,
    }
}

#[cfg(test)]
mod tests;
