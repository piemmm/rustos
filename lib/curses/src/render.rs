//! The minimal-diff renderer: a desired screen plus the last-flushed screen in,
//! the smallest `lib/vt` operation sequence the terminal supports out.
//!
//! [`render`] walks the two [`Buffer`]s cell by cell and emits a [`rustos_vt::Op`]
//! only where they differ — one cursor move per run of changes, one SGR
//! transition per attribute change, and one [`rustos_vt::Op::Print`] per glyph.
//! Every colour is first passed through [`crate::color::downgrade`] for the
//! terminal's [`ColorDepth`](rustos_termcap::ColorDepth), so a truecolour application drawn on a 16-colour
//! `TERM` emits only colours that terminal renders (`plans/CURSES.md` §C4).
//!
//! A terminal that cannot address the cursor (the `dumb` fallback) takes a
//! conservative full-rewrite path instead of absolute positioning, so even the
//! baseline degrades safely rather than emitting sequences it would not honour
//! (`AGENTS.md` §2.9).

use alloc::vec::Vec;

use rustos_termcap::Capabilities;
use rustos_vt::{Attributes, Op, Sgr};

use crate::buffer::Buffer;
use crate::color::downgrade;
use crate::geom::Pos;

/// The terminal cursor's desired end state after an update.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct CursorState {
    /// Whether the cursor should be visible once the update is flushed.
    pub visible: bool,
    /// Where the cursor should rest, in zero-based screen coordinates.
    pub pos: Pos,
}

/// Produce the operations that transform `previous` into `desired` on a
/// terminal with capabilities `caps`, leaving the cursor in `cursor`.
///
/// The returned operations are intended to be encoded with [`rustos_vt::encode_all`]
/// and written to the tty. When `previous == desired` and the cursor is
/// already placed, the result is empty — the renderer emits nothing when
/// nothing changed.
#[must_use]
pub fn render(
    caps: &Capabilities,
    previous: &Buffer,
    desired: &Buffer,
    cursor: CursorState,
) -> Vec<Op> {
    if caps.cursor_addressing {
        render_addressable(caps, previous, desired, cursor)
    } else {
        render_dumb(caps, previous, desired)
    }
}

/// The minimal-diff path for a cursor-addressable terminal.
fn render_addressable(
    caps: &Capabilities,
    previous: &Buffer,
    desired: &Buffer,
    cursor: CursorState,
) -> Vec<Op> {
    let mut out = Vec::new();
    let size = desired.size();
    let mut pen = Pen::new();
    let mut at: Option<Pos> = None;

    for row in 0..size.rows {
        for col in 0..size.cols {
            let pos = Pos::new(row, col);
            let want = desired.get(pos);
            if want == previous.get(pos) {
                continue;
            }
            let Some(cell) = want else {
                continue;
            };
            if at != Some(pos) {
                out.push(Op::CursorPosition {
                    row: row + 1,
                    col: col + 1,
                });
            }
            let want_attrs = resolve(cell.attrs, caps);
            pen.transition_into(&mut out, want_attrs);
            out.push(Op::Print(cell.ch));
            // The terminal's cursor steps right after printing; track it so an
            // adjacent change needs no fresh `CursorPosition`.
            at = if col + 1 < size.cols {
                Some(Pos::new(row, col + 1))
            } else {
                None
            };
        }
    }

    finish_cursor(&mut out, caps, cursor);
    out
}

/// The conservative full-rewrite path for a terminal without cursor
/// addressing: when anything changed, redraw every row's glyphs in order.
fn render_dumb(caps: &Capabilities, previous: &Buffer, desired: &Buffer) -> Vec<Op> {
    let mut out = Vec::new();
    if previous == desired {
        return out;
    }
    let size = desired.size();
    let mut pen = Pen::new();
    out.push(Op::CarriageReturn);
    for row in 0..size.rows {
        if let Some(cells) = desired.row(row) {
            for cell in cells {
                let want_attrs = resolve(cell.attrs, caps);
                pen.transition_into(&mut out, want_attrs);
                out.push(Op::Print(cell.ch));
            }
        }
        out.push(Op::CarriageReturn);
        out.push(Op::LineFeed);
    }
    out
}

/// Append the cursor-placement (and visibility) operations that conclude an
/// addressable update.
fn finish_cursor(out: &mut Vec<Op>, caps: &Capabilities, cursor: CursorState) {
    out.push(Op::CursorPosition {
        row: cursor.pos.row + 1,
        col: cursor.pos.col + 1,
    });
    if caps.cursor_visibility {
        out.push(if cursor.visible {
            Op::ShowCursor
        } else {
            Op::HideCursor
        });
    }
}

/// Downgrade a cell's colours to what the terminal can render, leaving the
/// rendition flags untouched.
fn resolve(mut attrs: Attributes, caps: &Capabilities) -> Attributes {
    attrs.foreground = downgrade(attrs.foreground, caps.color);
    attrs.background = downgrade(attrs.background, caps.color);
    attrs
}

/// The terminal's current rendition state as the renderer believes it to be.
///
/// It starts *unknown*, so the first styled cell forces a full
/// [`Sgr::Reset`]-led transition; thereafter only the changed attributes are
/// emitted.
struct Pen {
    current: Attributes,
    known: bool,
}

impl Pen {
    fn new() -> Pen {
        Pen {
            current: Attributes::PLAIN,
            known: false,
        }
    }

    /// Emit the minimal SGR run that moves the pen from its current state to
    /// `want`, then adopt `want` as the new state.
    fn transition_into(&mut self, out: &mut Vec<Op>, want: Attributes) {
        if self.known && self.current == want {
            return;
        }
        // A flag that must be cleared has no single "off" SGR here, so a reset
        // is the clean way to drop it; otherwise build additively from the
        // current state.
        let must_reset = !self.known || clears_a_flag(self.current, want);
        let base = if must_reset {
            out.push(Op::Sgr(Sgr::Reset));
            Attributes::PLAIN
        } else {
            self.current
        };
        for sgr in additive_sgrs(base, want) {
            out.push(Op::Sgr(sgr));
        }
        self.current = want;
        self.known = true;
    }
}

/// Whether moving from `from` to `to` turns off any rendition flag (which a
/// purely additive SGR run cannot express).
fn clears_a_flag(from: Attributes, to: Attributes) -> bool {
    (from.bold && !to.bold)
        || (from.dim && !to.dim)
        || (from.italic && !to.italic)
        || (from.underline && !to.underline)
        || (from.blink && !to.blink)
        || (from.reverse && !to.reverse)
        || (from.strike && !to.strike)
}

/// The SGR operations that add to `base` everything `want` has that `base`
/// lacks (set flags and changed colours). Assumes no flag needs clearing.
fn additive_sgrs(base: Attributes, want: Attributes) -> Vec<Sgr> {
    let mut sgrs = Vec::new();
    if want.bold && !base.bold {
        sgrs.push(Sgr::Bold);
    }
    if want.dim && !base.dim {
        sgrs.push(Sgr::Dim);
    }
    if want.italic && !base.italic {
        sgrs.push(Sgr::Italic);
    }
    if want.underline && !base.underline {
        sgrs.push(Sgr::Underline);
    }
    if want.blink && !base.blink {
        sgrs.push(Sgr::Blink);
    }
    if want.reverse && !base.reverse {
        sgrs.push(Sgr::Reverse);
    }
    if want.strike && !base.strike {
        sgrs.push(Sgr::Strike);
    }
    if want.foreground != base.foreground {
        sgrs.push(Sgr::Foreground(want.foreground));
    }
    if want.background != base.background {
        sgrs.push(Sgr::Background(want.background));
    }
    sgrs
}
