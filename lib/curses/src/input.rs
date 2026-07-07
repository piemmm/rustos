//! The input decoder: terminal bytes in, typed [`Event`]s out.
//!
//! [`Input`] wraps the one shared [`rustos_vt::Parser`] and
//! maps the [`rustos_vt::Op`]s it produces to the [`Event`]s a curses
//! application reads: printable characters, the arrow / function / editing
//! keys, mouse reports, and bracketed-paste runs. There is no second
//! escape-sequence table here — every sequence the decoder understands is one
//! `lib/vt` already parses (the keys, mouse reports, and paste markers added to
//! it for this stage).
//!
//! Decoding untrusted bytes never panics: an unrecognised or
//! partial sequence is consumed and produces no event, and a malformed UTF-8
//! run is dropped by the parser rather than corrupting state.

use alloc::string::String;

use rustos_vt::{control, Key, MouseReport, Op, Parser};

/// A decoded input event.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Event {
    /// A printable character was typed.
    Char(char),
    /// The Escape key: a bare `ESC` byte that ended a read with no sequence
    /// following it (see [`rustos_vt::Parser::take_pending_escape`]).
    Esc,
    /// A control-chorded letter key (`Ctrl-A` … `Ctrl-Z`), carried as the
    /// lowercase letter. The controls that are keys in their own right —
    /// `Ctrl-I` (Tab), `Ctrl-J`/`Ctrl-M` (Enter), `Ctrl-H` (Backspace) —
    /// keep their named events and never arrive here.
    Ctrl(char),
    /// The Enter / Return key (carriage return or line feed).
    Enter,
    /// The Tab key.
    Tab,
    /// The Backspace key.
    Backspace,
    /// The up arrow.
    Up,
    /// The down arrow.
    Down,
    /// The left arrow.
    Left,
    /// The right arrow.
    Right,
    /// A function key `F1`…`F12`, carried as its number `1..=12`.
    Function(u8),
    /// The Home key.
    Home,
    /// The End key.
    End,
    /// The Insert key.
    Insert,
    /// The Delete key.
    Delete,
    /// The Page Up key.
    PageUp,
    /// The Page Down key.
    PageDown,
    /// A mouse report (button, position, modifiers).
    Mouse(MouseReport),
    /// A completed bracketed paste: the text pasted between the start and end
    /// markers, free of escape interpretation.
    Paste(String),
}

/// A streaming decoder from terminal bytes to [`Event`]s.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Input {
    parser: Parser,
    in_paste: bool,
    paste: String,
}

impl Default for Input {
    fn default() -> Self {
        Self::new()
    }
}

impl Input {
    /// A fresh decoder.
    #[must_use]
    pub fn new() -> Input {
        Input {
            parser: Parser::new(),
            in_paste: false,
            paste: String::new(),
        }
    }

    /// Feed `bytes`, invoking `sink` once per decoded [`Event`] in order.
    ///
    /// A bracketed paste emits a single [`Event::Paste`] when its end marker
    /// arrives; characters received between the markers are gathered into that
    /// paste rather than delivered individually.
    ///
    /// `DEL` (`0x7f`) is decoded as [`Event::Backspace`]: it is the byte
    /// xterm-class terminals — and the RustOS keymap — send for the
    /// Backspace key (the shared rub-out definition in `rustos_vt::control`).
    /// The screen-op parser deliberately ignores `DEL` because it is a
    /// no-op on *output*; on the input side it is a keystroke. No escape
    /// sequence carries a `DEL` byte, so mapping it before the parser is
    /// sound.
    pub fn feed(&mut self, bytes: &[u8], mut sink: impl FnMut(Event)) {
        // The parser borrows `&mut self.parser` for the call, so the paste
        // state it mutates is captured separately to avoid aliasing.
        let in_paste = &mut self.in_paste;
        let paste = &mut self.paste;
        for &byte in bytes {
            if byte == control::DEL {
                // Pasted rub-outs are not content; they are dropped, like
                // any other non-text key inside a paste run.
                if !*in_paste {
                    sink(Event::Backspace);
                }
                continue;
            }
            // A C0 control at a stream boundary is a control-chorded key,
            // not part of a sequence; the ones that are keys in their own
            // right (Tab, Enter, Backspace) and `ESC` stay with the parser.
            // Inside a paste a control byte is not content and is dropped.
            if let Some(letter) = ctrl_letter(byte) {
                if self.parser.is_ground() {
                    if !*in_paste {
                        sink(Event::Ctrl(letter));
                    }
                    continue;
                }
            }
            self.parser.feed(core::slice::from_ref(&byte), |op| {
                if let Some(event) = translate(&op, in_paste, paste) {
                    sink(event);
                }
            });
        }
        // An `ESC` that ended this read with nothing following it was the
        // Escape key (the chunk-boundary discrimination documented on
        // `Parser::take_pending_escape`). Inside a paste the pending `ESC`
        // is left with the parser: it may be the split start of the paste
        // end marker arriving in the next read.
        if !*in_paste && self.parser.take_pending_escape() {
            sink(Event::Esc);
        }
    }
}

/// The lowercase letter of a control-chorded key byte, or [`None`] for a
/// byte that is not one. Only `Ctrl-A` … `Ctrl-Z` qualify, minus the
/// controls that are keys in their own right: `Ctrl-H` (`BS`, Backspace),
/// `Ctrl-I` (`HT`, Tab), and `Ctrl-J`/`Ctrl-M` (`LF`/`CR`, Enter). `ESC`
/// (`0x1b`) is outside the range and always reaches the parser.
const fn ctrl_letter(byte: u8) -> Option<char> {
    match byte {
        control::BS | control::HT | control::LF | control::CR => None,
        0x01..=0x1a => Some((byte + b'a' - 1) as char),
        _ => None,
    }
}

/// Map one parsed [`Op`] to an [`Event`], folding paste runs into the
/// accumulator. Returns `None` for operations that carry no input event (or
/// while inside a paste).
fn translate(op: &Op, in_paste: &mut bool, paste: &mut String) -> Option<Event> {
    match op {
        Op::PasteStart => {
            *in_paste = true;
            paste.clear();
            None
        }
        Op::PasteEnd => {
            *in_paste = false;
            Some(Event::Paste(core::mem::take(paste)))
        }
        // Inside a paste, text-bearing operations are literal content, not
        // keystrokes: gather them (including pasted tabs and newlines) and
        // deliver the whole run as one `Paste` event.
        _ if *in_paste => {
            match op {
                Op::Print(ch) => paste.push(*ch),
                Op::Tab => paste.push('\t'),
                Op::LineFeed => paste.push('\n'),
                Op::CarriageReturn => paste.push('\r'),
                _ => {}
            }
            None
        }
        Op::Print(ch) => Some(Event::Char(*ch)),
        Op::CarriageReturn | Op::LineFeed => Some(Event::Enter),
        Op::Tab => Some(Event::Tab),
        Op::Backspace => Some(Event::Backspace),
        // In normal cursor mode the arrow keys are the cursor-movement
        // sequences (`lib/termcap`'s `ArrowKeys`), so they arrive as these ops.
        Op::CursorUp(_) => Some(Event::Up),
        Op::CursorDown(_) => Some(Event::Down),
        Op::CursorForward(_) => Some(Event::Right),
        Op::CursorBack(_) => Some(Event::Left),
        Op::Key(key) => Some(key_event(*key)),
        Op::Mouse(report) => Some(Event::Mouse(*report)),
        _ => None,
    }
}

/// Map a named [`Key`] to its [`Event`].
fn key_event(key: Key) -> Event {
    match key {
        Key::F1 => Event::Function(1),
        Key::F2 => Event::Function(2),
        Key::F3 => Event::Function(3),
        Key::F4 => Event::Function(4),
        Key::F5 => Event::Function(5),
        Key::F6 => Event::Function(6),
        Key::F7 => Event::Function(7),
        Key::F8 => Event::Function(8),
        Key::F9 => Event::Function(9),
        Key::F10 => Event::Function(10),
        Key::F11 => Event::Function(11),
        Key::F12 => Event::Function(12),
        Key::Home => Event::Home,
        Key::End => Event::End,
        Key::Insert => Event::Insert,
        Key::Delete => Event::Delete,
        Key::PageUp => Event::PageUp,
        Key::PageDown => Event::PageDown,
    }
}
