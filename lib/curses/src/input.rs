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

use rustos_vt::{Key, MouseReport, Op, Parser};

/// A decoded input event.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Event {
    /// A printable character was typed.
    Char(char),
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
    pub fn feed(&mut self, bytes: &[u8], mut sink: impl FnMut(Event)) {
        // The parser borrows `&mut self.parser` for the call, so the paste
        // state it mutates is captured separately to avoid aliasing.
        let in_paste = &mut self.in_paste;
        let paste = &mut self.paste;
        self.parser.feed(bytes, |op| {
            if let Some(event) = translate(&op, in_paste, paste) {
                sink(event);
            }
        });
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
