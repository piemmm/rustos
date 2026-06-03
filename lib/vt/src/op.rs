//! [`Op`] — the typed vocabulary of terminal operations.
//!
//! An [`Op`] is one unit of the ANSI / VT / xterm stream: a printed character,
//! a C0 control, or a recognised escape sequence. It is the shared currency of
//! the crate — [`crate::encode`] turns an [`Op`] into bytes and [`crate::Parser`]
//! turns bytes back into [`Op`]s — so the emitter and consumer never disagree
//! about what a sequence means (`AGENTS.md` §2.2).

use alloc::string::String;

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
    /// other than `0`, `1`, or `2` (fail closed, `AGENTS.md` §2.9).
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
    /// Set the window title (`OSC 0 ; title ST`).
    SetTitle(String),
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
