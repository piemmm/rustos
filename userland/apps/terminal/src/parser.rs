//! The byte-stream interpreter that drives a [`Grid`].
//!
//! [`Parser`] is a thin adapter over [`lib/vt`](rustos_vt)'s streaming parser:
//! it consumes the shell's output bytes, lets `lib/vt` turn them into the
//! shared [`Op`] vocabulary, and applies each [`Op`] to a [`Grid`]. There is no
//! second escape-sequence definition in this app — the emulator is a *consumer*
//! of the one ANSI/VT/xterm vocabulary, so it understands
//! exactly what `lib/vt`'s emitter produces and nothing it invents privately.
//!
//! Because `lib/vt`'s parser is total — every byte stream is consumed without
//! panic or out-of-bounds access, oversized parameters saturate, and an
//! unrecognised or malformed sequence is dropped — and every
//! [`Grid`] operation is itself total and clamping, a hostile or malformed
//! stream degrades to dropped control rather than a corrupted display or a
//! panic. Holding the escape-sequence state in the parser (rather than the
//! grid) keeps the screen model free of parsing concerns.

use rustos_vt::{Op, Parser as VtParser};

use crate::grid::Grid;

/// A streaming interpreter from shell output bytes to [`Grid`] operations,
/// built on `lib/vt`'s shared parser.
#[derive(Clone, Debug, Eq, PartialEq, Default)]
pub struct Parser {
    inner: VtParser,
}

impl Parser {
    /// A fresh parser in the ground state.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            inner: VtParser::new(),
        }
    }

    /// Feed a slice of shell output, applying each recognised operation to
    /// `grid` in stream order.
    pub fn feed(&mut self, grid: &mut Grid, bytes: &[u8]) {
        self.inner.feed(bytes, |op| apply(grid, op));
    }

    /// Feed one `byte` of shell output, applying any operation it completes to
    /// `grid`.
    pub fn feed_byte(&mut self, grid: &mut Grid, byte: u8) {
        self.inner.feed_byte(byte, |op| apply(grid, op));
    }
}

/// Apply one shared-vocabulary [`Op`] to `grid`.
///
/// The cursor counts and positions `lib/vt` carries are the 1-based values
/// ANSI uses on the wire; positions are converted to the [`Grid`]'s 0-based
/// coordinates here.
fn apply(grid: &mut Grid, op: Op) {
    match op {
        Op::Print(ch) => grid.write_char(ch),
        Op::Backspace => grid.backspace(),
        Op::Tab => grid.tab(),
        Op::LineFeed => grid.line_feed(),
        Op::CarriageReturn => grid.carriage_return(),
        Op::CursorUp(n) => grid.move_up(n),
        Op::CursorDown(n) => grid.move_down(n),
        Op::CursorForward(n) => grid.move_right(n),
        Op::CursorBack(n) => grid.move_left(n),
        Op::CursorNextLine(n) => grid.next_line(n),
        Op::CursorPrevLine(n) => grid.prev_line(n),
        Op::CursorColumn(col) => grid.move_to_column(to_zero_based(col)),
        Op::CursorPosition { row, col } => grid.move_to(to_zero_based(col), to_zero_based(row)),
        Op::EraseInDisplay(mode) => grid.erase_in_display(mode),
        Op::EraseInLine(mode) => grid.erase_in_line(mode),
        Op::ScrollUp(n) => grid.scroll_up(n),
        Op::ScrollDown(n) => grid.scroll_down(n),
        Op::SetScrollRegion { top, bottom } => grid.set_scroll_region(top, bottom),
        Op::ResetScrollRegion => grid.reset_scroll_region(),
        Op::EnterAltScreen => grid.enter_alt_screen(),
        Op::LeaveAltScreen => grid.leave_alt_screen(),
        Op::ShowCursor => grid.set_cursor_visible(true),
        Op::HideCursor => grid.set_cursor_visible(false),
        Op::SaveCursor => grid.save_cursor(),
        Op::RestoreCursor => grid.restore_cursor(),
        Op::Sgr(sgr) => {
            let mut pen = grid.pen();
            pen.apply(sgr);
            grid.set_attributes(pen);
        }
        Op::SetTitle(title) => grid.set_title(title),
        // Operations with no effect on the rendered display. The bell carries
        // no screen change we model (no audible bell is wired). The rest are
        // *input* a terminal reports to the program (named keys, mouse reports,
        // paste-run markers) or requests to turn input-reporting modes on and
        // off: they flow program-ward, and the emulator has no input
        // back-channel to honour the mode requests through, so a consumer that
        // renders shell *output* applies no screen change rather than
        // mislabelling them as display operations.
        Op::Bell
        | Op::Key(_)
        | Op::Mouse(_)
        | Op::SetMouseMode { .. }
        | Op::SetBracketedPaste(_)
        | Op::PasteStart
        | Op::PasteEnd => {}
    }
}

/// Convert a 1-based ANSI coordinate to the grid's 0-based coordinate.
fn to_zero_based(one_based: u16) -> u16 {
    one_based.saturating_sub(1)
}
