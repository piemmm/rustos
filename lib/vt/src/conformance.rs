//! The screen-semantics conformance script every consumer of [`Op`] runs.
//!
//! Two screens in this workspace apply the same operation stream to a
//! character-cell grid: the framebuffer boot console (`lib/fbcon`), which owns
//! pixels and runs without an allocator, and the desktop terminal emulator,
//! which owns a resizable model and renders whole frames. Their storage and
//! their painting differ, so they are separate implementations — but a
//! program's output must land in the *same cells* on both. A full-screen
//! application laid out correctly on one and misdrawn on the other is a defect
//! the program itself cannot see or work around.
//!
//! [`check`] is that agreement, written once. Each screen implements
//! [`ScreenModel`] over a [`COLS`]×[`ROWS`] grid in its own tests and runs the
//! script; the first expectation a screen misses comes back as a
//! [`Divergence`] naming the step, so a future change that alters one screen's
//! semantics fails the other's test as well.
//!
//! The script pins the rules a text-user-interface actually depends on, in
//! particular the **pending wrap**: filling the last column leaves the cursor
//! resting on that column with the wrap owed, and the wrap is paid only by the
//! next printable character. Wrapping eagerly instead would line-feed — and at
//! the bottom margin *scroll the whole screen* — the moment a program painted
//! a full-width row, which no terminal does and no application expects.

use crate::{EraseMode, Op};

/// Columns the conformance screen is created with.
pub const COLS: u16 = 10;

/// Rows the conformance screen is created with.
pub const ROWS: u16 = 4;

/// A blank conformance row, for writing an expected screen.
const BLANK: &str = "          ";

/// The screen state a consumer exposes so the shared script can check it.
///
/// Implemented in each screen's own tests over a [`COLS`]×[`ROWS`] grid; the
/// production types stay free of a test-only trait.
pub trait ScreenModel {
    /// The grid's column count.
    fn cols(&self) -> u16;

    /// The grid's row count.
    fn rows(&self) -> u16;

    /// Apply one operation to the screen.
    fn apply(&mut self, op: &Op);

    /// The glyph recorded at `(col, row)`, or a space for a blank or
    /// out-of-range cell.
    fn glyph(&self, col: u16, row: u16) -> char;

    /// The cursor's `(col, row)`, zero-based.
    fn cursor(&self) -> (u16, u16);
}

/// What a screen got wrong, and where.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct Divergence {
    /// The script step that failed, named for the rule it pins.
    pub step: &'static str,
    /// The expectation that was missed.
    pub detail: Detail,
}

/// The specific expectation a [`Divergence`] missed.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum Detail {
    /// The screen was not created at the script's grid size.
    Size {
        /// The columns the screen reported.
        cols: u16,
        /// The rows the screen reported.
        rows: u16,
    },
    /// The cursor rested somewhere else.
    Cursor {
        /// The `(col, row)` the script expected.
        want: (u16, u16),
        /// The `(col, row)` the screen reported.
        got: (u16, u16),
    },
    /// A cell held a different glyph.
    Glyph {
        /// The cell's column.
        col: u16,
        /// The cell's row.
        row: u16,
        /// The glyph the script expected.
        want: char,
        /// The glyph the screen recorded.
        got: char,
    },
}

/// Run the shared script against `screen`, returning the first expectation it
/// missed.
///
/// `screen` must be a freshly created, blank [`COLS`]×[`ROWS`] grid with the
/// cursor at the home position; the script erases the display between steps
/// but does not reset the pen, the scroll region, or the alternate screen
/// beyond what it sets itself.
///
/// # Errors
///
/// Returns the first [`Divergence`] found, so a failing screen names one
/// concrete rule rather than a wall of mismatches.
pub fn check<S: ScreenModel>(screen: &mut S) -> Result<(), Divergence> {
    if screen.cols() != COLS || screen.rows() != ROWS {
        return Err(Divergence {
            step: "the screen is created at the script's grid size",
            detail: Detail::Size {
                cols: screen.cols(),
                rows: screen.rows(),
            },
        });
    }

    pending_wrap(screen)?;
    pending_wrap_is_cancelled(screen)?;
    bottom_row_does_not_scroll(screen)?;
    erase_and_tab(screen)?;
    wide_glyph_wraps_whole(screen)?;
    scroll_region(screen)?;
    cursor_addressing(screen)?;
    alternate_screen(screen)?;
    saved_cursor(screen)
}

/// Filling the last column owes a wrap; the next glyph pays it.
fn pending_wrap<S: ScreenModel>(screen: &mut S) -> Result<(), Divergence> {
    let step = "a full row leaves the cursor on the last column with the wrap owed";
    home(screen);
    print(screen, "0123456789");
    cursor_is(screen, step, (COLS - 1, 0))?;
    rows_are(screen, step, ["0123456789", BLANK, BLANK, BLANK])?;

    let step = "the glyph after a full row pays the owed wrap";
    print(screen, "A");
    cursor_is(screen, step, (1, 1))?;
    rows_are(screen, step, ["0123456789", "A         ", BLANK, BLANK])
}

/// Anything that moves the cursor cancels the owed wrap.
fn pending_wrap_is_cancelled<S: ScreenModel>(screen: &mut S) -> Result<(), Divergence> {
    let step = "an absolute move cancels the owed wrap";
    home(screen);
    print(screen, "0123456789");
    screen.apply(&Op::CursorPosition { row: 1, col: 1 });
    print(screen, "Z");
    cursor_is(screen, step, (1, 0))?;
    rows_are(screen, step, ["Z123456789", BLANK, BLANK, BLANK])?;

    let step = "a carriage return cancels the owed wrap";
    home(screen);
    print(screen, "0123456789");
    screen.apply(&Op::CarriageReturn);
    print(screen, "Y");
    cursor_is(screen, step, (1, 0))?;
    rows_are(screen, step, ["Y123456789", BLANK, BLANK, BLANK])?;

    // A rubout is backspace, space, backspace: it must erase the glyph just
    // written, so the backspace has to land on the last column, not before it.
    let step = "backspace from the owed wrap rubs out the glyph just written";
    home(screen);
    print(screen, "0123456789");
    screen.apply(&Op::Backspace);
    print(screen, "!");
    rows_are(screen, step, ["012345678!", BLANK, BLANK, BLANK])
}

/// The rule a full-width status bar on the bottom row depends on.
fn bottom_row_does_not_scroll<S: ScreenModel>(screen: &mut S) -> Result<(), Divergence> {
    let step = "filling the bottom row does not scroll the screen";
    home(screen);
    print(screen, "top");
    screen.apply(&Op::CursorPosition { row: ROWS, col: 1 });
    print(screen, "0123456789");
    cursor_is(screen, step, (COLS - 1, ROWS - 1))?;
    rows_are(screen, step, ["top       ", BLANK, BLANK, "0123456789"])?;

    let step = "the glyph after a full bottom row scrolls exactly one line";
    print(screen, "B");
    cursor_is(screen, step, (1, ROWS - 1))?;
    rows_are(screen, step, [BLANK, BLANK, "0123456789", "B         "])
}

/// Erasing and tabbing from the owed-wrap position.
fn erase_and_tab<S: ScreenModel>(screen: &mut S) -> Result<(), Divergence> {
    let step = "erase to end of line from the owed wrap clears the last cell";
    home(screen);
    print(screen, "0123456789");
    screen.apply(&Op::EraseInLine(EraseMode::ToEnd));
    rows_are(screen, step, ["012345678 ", BLANK, BLANK, BLANK])?;

    let step = "a tab lands on the next multiple of eight, clamped to the last column";
    home(screen);
    screen.apply(&Op::Tab);
    cursor_is(screen, step, (8, 0))?;
    screen.apply(&Op::Tab);
    cursor_is(screen, step, (COLS - 1, 0))
}

/// A double-width glyph never straddles the right edge.
fn wide_glyph_wraps_whole<S: ScreenModel>(screen: &mut S) -> Result<(), Divergence> {
    let step = "a wide glyph with one column left wraps whole";
    home(screen);
    print(screen, "123456789");
    print(screen, "\u{65E5}");
    glyph_is(screen, step, (COLS - 1, 0), ' ')?;
    glyph_is(screen, step, (0, 1), '\u{65E5}')?;
    cursor_is(screen, step, (2, 1))
}

/// `DECSTBM` homes into the region, and a line feed at its bottom margin
/// scrolls only the region.
fn scroll_region<S: ScreenModel>(screen: &mut S) -> Result<(), Divergence> {
    let step = "setting the scroll region homes the cursor to the region's top row";
    home(screen);
    screen.apply(&Op::SetScrollRegion { top: 2, bottom: 4 });
    cursor_is(screen, step, (0, 1))?;

    let step = "a line feed at the region's bottom scrolls only the region";
    print(screen, "b");
    screen.apply(&Op::CursorPosition { row: 1, col: 1 });
    print(screen, "T");
    screen.apply(&Op::CursorPosition { row: ROWS, col: 1 });
    print(screen, "d");
    screen.apply(&Op::LineFeed);
    rows_are(screen, step, ["T         ", BLANK, "d         ", BLANK])?;
    screen.apply(&Op::ResetScrollRegion);
    Ok(())
}

/// The absolute and relative cursor operations clamp into the grid.
fn cursor_addressing<S: ScreenModel>(screen: &mut S) -> Result<(), Divergence> {
    let step = "column addressing is one-based and clamps to the last column";
    home(screen);
    screen.apply(&Op::CursorColumn(5));
    cursor_is(screen, step, (4, 0))?;
    screen.apply(&Op::CursorColumn(99));
    cursor_is(screen, step, (COLS - 1, 0))?;

    let step = "position addressing is one-based and clamps to the last cell";
    screen.apply(&Op::CursorPosition { row: 99, col: 99 });
    cursor_is(screen, step, (COLS - 1, ROWS - 1))?;

    let step = "relative moves stop at the edges";
    screen.apply(&Op::CursorUp(99));
    screen.apply(&Op::CursorBack(99));
    cursor_is(screen, step, (0, 0))?;
    screen.apply(&Op::CursorDown(1));
    screen.apply(&Op::CursorForward(2));
    cursor_is(screen, step, (2, 1))?;
    screen.apply(&Op::CursorNextLine(1));
    cursor_is(screen, step, (0, 2))?;
    screen.apply(&Op::CursorPrevLine(1));
    cursor_is(screen, step, (0, 1))
}

/// The alternate screen is a separate surface and the main one survives it.
fn alternate_screen<S: ScreenModel>(screen: &mut S) -> Result<(), Divergence> {
    let step = "entering the alternate screen shows a blank surface at home";
    home(screen);
    print(screen, "main");
    screen.apply(&Op::EnterAltScreen);
    cursor_is(screen, step, (0, 0))?;
    rows_are(screen, step, [BLANK, BLANK, BLANK, BLANK])?;

    let step = "leaving the alternate screen restores the main surface";
    print(screen, "alt");
    screen.apply(&Op::LeaveAltScreen);
    rows_are(screen, step, ["main      ", BLANK, BLANK, BLANK])
}

/// `DECSC`/`DECRC`, including the restore with nothing saved.
fn saved_cursor<S: ScreenModel>(screen: &mut S) -> Result<(), Divergence> {
    let step = "a restore returns to the saved position";
    home(screen);
    screen.apply(&Op::CursorPosition { row: 2, col: 3 });
    screen.apply(&Op::SaveCursor);
    screen.apply(&Op::CursorPosition { row: ROWS, col: 1 });
    screen.apply(&Op::RestoreCursor);
    cursor_is(screen, step, (2, 1))
}

/// Blank the display and home the cursor, so each step starts from a known
/// screen.
fn home<S: ScreenModel>(screen: &mut S) {
    screen.apply(&Op::EraseInDisplay(EraseMode::All));
    screen.apply(&Op::CursorPosition { row: 1, col: 1 });
}

/// Print each character of `text` in turn.
fn print<S: ScreenModel>(screen: &mut S, text: &str) {
    for ch in text.chars() {
        screen.apply(&Op::Print(ch));
    }
}

/// Check the cursor rests at `want`.
fn cursor_is<S: ScreenModel>(
    screen: &S,
    step: &'static str,
    want: (u16, u16),
) -> Result<(), Divergence> {
    let got = screen.cursor();
    if got == want {
        return Ok(());
    }
    Err(Divergence {
        step,
        detail: Detail::Cursor { want, got },
    })
}

/// Check the cell at `(col, row)` holds `want`.
fn glyph_is<S: ScreenModel>(
    screen: &S,
    step: &'static str,
    (col, row): (u16, u16),
    want: char,
) -> Result<(), Divergence> {
    let got = screen.glyph(col, row);
    if got == want {
        return Ok(());
    }
    Err(Divergence {
        step,
        detail: Detail::Glyph {
            col,
            row,
            want,
            got,
        },
    })
}

/// Check the whole screen, row by row, against `want` (one string of [`COLS`]
/// characters per row).
fn rows_are<S: ScreenModel>(
    screen: &S,
    step: &'static str,
    want: [&str; ROWS as usize],
) -> Result<(), Divergence> {
    for (row, text) in (0u16..).zip(want) {
        for (col, ch) in (0u16..).zip(text.chars()) {
            glyph_is(screen, step, (col, row), ch)?;
        }
    }
    Ok(())
}
