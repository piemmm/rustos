//! [`Op`] — the typed vocabulary of terminal operations.
//!
//! An [`Op`] is one unit of the ANSI / VT / xterm stream: a printed character,
//! a C0 control, or a recognised escape sequence. It is the shared currency of
//! the crate — [`crate::encode_into`] turns an [`Op`] into bytes and
//! [`crate::Parser`] turns bytes back into [`Op`]s — so the emitter and
//! consumer never disagree about what a sequence means.

use crate::attr::Sgr;
use crate::key::Key;
use crate::mouse::{MouseMode, MouseReport};

/// The region an erase operation clears, relative to the cursor.
///
/// The discriminant is the ANSI parameter value, shared by erase-in-line
/// (`EL`) and erase-in-display (`ED`).
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum EraseMode {
    /// `0` — from the cursor to the end of the line / display.
    ToEnd = 0,
    /// `1` — from the start of the line / display to the cursor, inclusive.
    ToStart = 1,
    /// `2` — the whole line / display.
    All = 2,
}

impl EraseMode {
    /// The ANSI parameter value for this mode.
    #[must_use]
    pub const fn value(self) -> u16 {
        self as u16
    }

    /// The [`EraseMode`] for ANSI parameter `value`, or `None` for anything
    /// other than `0`, `1`, or `2` (fail closed).
    #[must_use]
    pub const fn from_value(value: u16) -> Option<EraseMode> {
        match value {
            0 => Some(EraseMode::ToEnd),
            1 => Some(EraseMode::ToStart),
            2 => Some(EraseMode::All),
            _ => None,
        }
    }
}

/// One terminal operation.
///
/// The counts and coordinates carried by the cursor operations are the same
/// 1-based values ANSI uses on the wire (a count is at least `1`; a position is
/// 1-based with `1,1` the home cell). The emitter clamps a count up to `1`, so
/// every [`Op`] the emitter writes parses back unchanged.
#[derive(Clone, Debug, Eq, PartialEq)]
// The `SetTitle` payload is a bounded inline title; owning it inline is the
// only representation that survives the encode/parse round trip, works on the
// no-allocator console build, and stays a fail-closed bound rather than a heap
// box. See `SetTitle` for the full rationale. Title operations are rare, so the
// wider enum does not sit on the character-printing hot path.
#[allow(clippy::large_enum_variant)]
pub enum Op {
    /// Print one character at the cursor.
    Print(char),
    /// Bell (`BEL`).
    Bell,
    /// Backspace (`BS`).
    Backspace,
    /// Horizontal tab (`HT`).
    Tab,
    /// Line feed (`LF`).
    LineFeed,
    /// Carriage return (`CR`).
    CarriageReturn,
    /// Move the cursor up `n` rows (`CUU`).
    CursorUp(u16),
    /// Move the cursor down `n` rows (`CUD`).
    CursorDown(u16),
    /// Move the cursor forward `n` columns (`CUF`).
    CursorForward(u16),
    /// Move the cursor back `n` columns (`CUB`).
    CursorBack(u16),
    /// Move the cursor to column `1` of the row `n` lines down (`CNL`).
    CursorNextLine(u16),
    /// Move the cursor to column `1` of the row `n` lines up (`CPL`).
    CursorPrevLine(u16),
    /// Move the cursor to 1-based column `col` on the current row (`CHA`).
    CursorColumn(u16),
    /// Move the cursor to the 1-based `row` and `col` (`CUP`).
    CursorPosition {
        /// 1-based row.
        row: u16,
        /// 1-based column.
        col: u16,
    },
    /// Erase part of the display (`ED`).
    EraseInDisplay(EraseMode),
    /// Erase part of the current line (`EL`).
    EraseInLine(EraseMode),
    /// Scroll the display up `n` lines (`SU`).
    ScrollUp(u16),
    /// Scroll the display down `n` lines (`SD`).
    ScrollDown(u16),
    /// Set the scroll region to the 1-based rows `top..=bottom` (`DECSTBM`).
    SetScrollRegion {
        /// 1-based top row of the region.
        top: u16,
        /// 1-based bottom row of the region.
        bottom: u16,
    },
    /// Reset the scroll region to the whole display (`DECSTBM` with no
    /// parameters).
    ResetScrollRegion,
    /// Switch to the alternate screen buffer (`CSI ? 1049 h`).
    EnterAltScreen,
    /// Switch back to the main screen buffer (`CSI ? 1049 l`).
    LeaveAltScreen,
    /// Show the cursor (`CSI ? 25 h`).
    ShowCursor,
    /// Hide the cursor (`CSI ? 25 l`).
    HideCursor,
    /// Save the cursor position and attributes (`ESC 7`).
    SaveCursor,
    /// Restore the saved cursor position and attributes (`ESC 8`).
    RestoreCursor,
    /// One Select Graphic Rendition operation (`CSI … m`).
    Sgr(Sgr),
    /// Set the window title (`OSC 0 ; title ST`). The title is a bounded,
    /// allocation-free [`Title`], so `Op` owns no heap and the vocabulary runs
    /// on a target with no global allocator (the framebuffer boot console).
    ///
    /// This is the one large `Op` variant, and it must be: the encode↔parse
    /// round trip requires the title text to be *owned* by `Op` (a borrowed
    /// `&str` could not survive being collected into a queue), the no-allocator
    /// console build rules out boxing it on the heap, and truncation at
    /// [`MAX_TITLE`] is a fail-closed validation bound rather than a growable
    /// capacity. Owning the bytes inline is therefore the only representation
    /// that satisfies all three; the alternative of a heap box is unavailable
    /// where the vocabulary must run. Title operations are rare, so the wider
    /// enum does not sit on the character-printing hot path in practice.
    SetTitle(Title),
    /// A named (function / editing) key (`SS3` or `CSI … ~`). The arrow keys
    /// are *not* here — in normal cursor mode they are the cursor-movement
    /// operations above.
    Key(Key),
    /// Enable (`CSI ? n h`) or disable (`CSI ? n l`) a mouse-tracking mode.
    SetMouseMode {
        /// The tracking protocol.
        mode: MouseMode,
        /// `true` to enable, `false` to disable.
        enable: bool,
    },
    /// One SGR-encoded mouse report (`CSI < Cb ; Cx ; Cy M` / `m`).
    Mouse(MouseReport),
    /// Enable (`CSI ? 2004 h`) or disable (`CSI ? 2004 l`) bracketed paste.
    SetBracketedPaste(bool),
    /// The start of a bracketed paste (`CSI 200 ~`).
    PasteStart,
    /// The end of a bracketed paste (`CSI 201 ~`).
    PasteEnd,
}

/// The largest window title retained, in bytes.
///
/// A longer title is truncated at a UTF-8 character boundary. This bound is
/// what keeps [`Title`] — and therefore [`Op`] — allocation-free (`AGENTS.md`
/// §24.4: a fixed validation bound, not a growable capacity).
pub const MAX_TITLE: usize = 256;

/// A bounded, allocation-free window title: the payload of [`Op::SetTitle`].
///
/// It stores up to [`MAX_TITLE`] bytes of UTF-8 inline, so it owns no heap and
/// works on a target with no global allocator. Construction truncates an
/// over-long title at a character boundary (fail-closed), so [`Title::as_str`]
/// is always valid UTF-8.
#[derive(Copy, Clone, Eq, PartialEq)]
pub struct Title {
    bytes: [u8; MAX_TITLE],
    len: usize,
}

impl Title {
    /// An empty title.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            bytes: [0; MAX_TITLE],
            len: 0,
        }
    }

    /// A title from a string, truncated at a character boundary to at most
    /// [`MAX_TITLE`] bytes.
    #[must_use]
    pub fn from_text(text: &str) -> Self {
        let mut end = text.len().min(MAX_TITLE);
        while end > 0 && !text.is_char_boundary(end) {
            end -= 1;
        }
        let mut title = Self::new();
        title.bytes[..end].copy_from_slice(&text.as_bytes()[..end]);
        title.len = end;
        title
    }

    /// A title from raw bytes: the longest valid UTF-8 prefix, truncated to at
    /// most [`MAX_TITLE`] bytes (fail-closed on invalid encoding).
    #[must_use]
    pub fn from_bytes(bytes: &[u8]) -> Self {
        let capped = &bytes[..bytes.len().min(MAX_TITLE)];
        let valid = match core::str::from_utf8(capped) {
            Ok(text) => text,
            Err(error) => {
                // Keep the valid prefix; drop the malformed tail.
                core::str::from_utf8(&capped[..error.valid_up_to()]).unwrap_or("")
            }
        };
        Self::from_text(valid)
    }

    /// The title as a string slice (always valid UTF-8).
    #[must_use]
    pub fn as_str(&self) -> &str {
        // The bytes were validated as UTF-8 at construction; fall back to the
        // empty string rather than ever panicking (`AGENTS.md` §2.9).
        core::str::from_utf8(&self.bytes[..self.len]).unwrap_or("")
    }

    /// The title's UTF-8 bytes.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes[..self.len]
    }
}

impl Default for Title {
    fn default() -> Self {
        Self::new()
    }
}

impl core::fmt::Debug for Title {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_tuple("Title").field(&self.as_str()).finish()
    }
}
