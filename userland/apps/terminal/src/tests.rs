//! Headless unit tests for the terminal model and renderer.
//!
//! Everything is exercised without a kernel: the [`ShellSource`] seam is an
//! in-memory queue, so the grid, the control parser, the shell glue, and the
//! renderer are all testable in isolation.

use alloc::collections::VecDeque;
use alloc::string::String;
use alloc::vec::Vec;

use tairix_abi::Errno;
use tairix_font::BitmapFont;
use tairix_geometry::{Point, Rect};
use tairix_raster::Color;
use tairix_theme::Theme;

use tairix_vt::{encode_all_into, BasicColor, Color as VtColor, Op, Sgr};

/// The face the renderer tests measure: the same monospace face the `Run`
/// binary derives from a desktop at the reference density, so a test's cell
/// arithmetic matches the terminal a user actually sees.
fn test_font() -> BitmapFont {
    BitmapFont::monospace(tairix_font::atlas::CELL_HEIGHT)
}

/// Encode a sequence of operations into a fresh `Vec` over the sink API.
fn encode_all(ops: &[Op]) -> alloc::vec::Vec<u8> {
    let mut out = alloc::vec::Vec::new();
    encode_all_into(ops, &mut out);
    out
}

use crate::grid::{Grid, MAX_DIMENSION};
use crate::parser::Parser;
use crate::render::Screen;
use crate::shell::ShellSource;
use crate::terminal::Terminal;
use crate::Cell;

/// An in-memory shell channel: each `read` returns the next queued chunk (or
/// an empty slice), and every `write` is captured. A pending error makes the
/// next call fail.
#[derive(Default)]
struct QueueShell {
    output: VecDeque<Vec<u8>>,
    written: Vec<u8>,
    read_error: Option<Errno>,
    write_error: Option<Errno>,
}

impl QueueShell {
    fn with_output(chunks: &[&[u8]]) -> Self {
        Self {
            output: chunks.iter().map(|chunk| chunk.to_vec()).collect(),
            ..Self::default()
        }
    }
}

impl ShellSource for QueueShell {
    fn read(&mut self) -> Result<Vec<u8>, Errno> {
        if let Some(errno) = self.read_error.take() {
            return Err(errno);
        }
        Ok(self.output.pop_front().unwrap_or_default())
    }

    fn write(&mut self, bytes: &[u8]) -> Result<(), Errno> {
        if let Some(errno) = self.write_error.take() {
            return Err(errno);
        }
        self.written.extend_from_slice(bytes);
        Ok(())
    }
}

/// The glyph of one cell, or a space when off-grid.
fn glyph(grid: &Grid, col: u16, row: u16) -> char {
    grid.cell(col, row).map_or(' ', |cell| cell.ch)
}

/// The cell at `(col, row)`, expected to be on-grid.
fn cell_at(grid: &Grid, col: u16, row: u16) -> Cell {
    grid.cell(col, row).expect("cell on grid")
}

/// Fill `row` of `grid` with `text`, one glyph per column from column 0.
fn put_row(grid: &mut Grid, row: u16, text: &str) {
    grid.move_to(0, row);
    for ch in text.chars() {
        grid.write_char(ch);
    }
}

/// The visible text of one grid row, with trailing blanks trimmed.
fn row_text(grid: &Grid, row: u16) -> String {
    let mut text = String::new();
    for col in 0..grid.cols() {
        text.push(glyph(grid, col, row));
    }
    String::from(text.trim_end())
}

/// Feed `bytes` through a fresh parser into a fresh `cols`×`rows` grid.
fn render_bytes(cols: u16, rows: u16, bytes: &[u8]) -> Grid {
    let mut grid = Grid::new(cols, rows).expect("valid grid size");
    let mut parser = Parser::new();
    parser.feed(&mut grid, bytes);
    grid
}

#[test]
fn grid_new_rejects_degenerate_sizes() {
    assert!(Grid::new(0, 24).is_none());
    assert!(Grid::new(80, 0).is_none());
    assert!(Grid::new(MAX_DIMENSION + 1, 24).is_none());
    assert!(Grid::new(80, MAX_DIMENSION + 1).is_none());
    assert!(Grid::new(1, 1).is_some());
}

#[test]
fn grid_resize_preserves_top_left_and_clamps_cursor() {
    let mut grid = render_bytes(6, 3, b"abc\r\ndef");
    assert_eq!(row_text(&grid, 0), "abc");
    assert_eq!(row_text(&grid, 1), "def");

    // Shrink: the overlapping top-left content is kept and the cursor is
    // clamped into the new bounds.
    assert!(grid.resize(4, 2));
    assert_eq!((grid.cols(), grid.rows()), (4, 2));
    assert_eq!(row_text(&grid, 0), "abc");
    assert_eq!(row_text(&grid, 1), "def");
    assert!(grid.cursor_col() < 4 && grid.cursor_row() < 2);

    // Grow: the preserved content stays put and the newly exposed row is blank.
    assert!(grid.resize(8, 4));
    assert_eq!((grid.cols(), grid.rows()), (8, 4));
    assert_eq!(row_text(&grid, 0), "abc");
    assert_eq!(row_text(&grid, 1), "def");
    assert_eq!(row_text(&grid, 3), "");

    // A degenerate, oversized, or no-op resize changes nothing (fail closed).
    assert!(!grid.resize(0, 4));
    assert!(!grid.resize(8, MAX_DIMENSION + 1));
    assert!(!grid.resize(8, 4));
    assert_eq!((grid.cols(), grid.rows()), (8, 4));
}

#[test]
fn terminal_resize_reshapes_the_grid() {
    let mut term = Terminal::new(20, 3, QueueShell::default()).expect("valid size");
    term.feed(b"hi");
    assert_eq!(term.grid().cols(), 20);
    assert!(term.resize(40, 10));
    assert_eq!((term.grid().cols(), term.grid().rows()), (40, 10));
    // The content survives the reshape; a no-op resize reports no change.
    assert_eq!(row_text(term.grid(), 0), "hi");
    assert!(!term.resize(40, 10));
}

#[test]
fn a_wide_glyph_occupies_a_lead_and_a_continuation_cell() {
    // The same two-column layout the curses window writer and the framebuffer
    // console produce, so column arithmetic agrees across the stack.
    let grid = render_bytes(6, 2, "世a".as_bytes());
    assert_eq!(glyph(&grid, 0, 0), '世');
    assert_eq!(glyph(&grid, 1, 0), tairix_vt::CONTINUATION);
    assert_eq!(glyph(&grid, 2, 0), 'a');
    assert_eq!((grid.cursor_col(), grid.cursor_row()), (3, 0));
}

#[test]
fn a_wide_glyph_wraps_whole_when_one_column_remains() {
    let grid = render_bytes(3, 2, "ab世".as_bytes());
    assert_eq!(glyph(&grid, 0, 0), 'a');
    assert_eq!(glyph(&grid, 1, 0), 'b');
    // The leftover column was blanked rather than half-filled.
    assert_eq!(glyph(&grid, 2, 0), ' ');
    assert_eq!(glyph(&grid, 0, 1), '世');
    assert_eq!(glyph(&grid, 1, 1), tairix_vt::CONTINUATION);
}

#[test]
fn a_wide_glyph_on_a_one_column_grid_never_leaks_into_the_next_row() {
    // One column can never hold a two-column glyph: the wrap-whole rule
    // blanks the cell and wraps first, and the continuation is dropped
    // rather than aliasing the next row's first cell. Each glyph then fills
    // its row and owes a wrap the next one pays, so the three rows are the
    // blanked first cell, the lead, and the following narrow glyph.
    let grid = render_bytes(1, 3, "世x".as_bytes());
    assert_eq!(glyph(&grid, 0, 0), ' ');
    assert_eq!(glyph(&grid, 0, 1), '世');
    assert_eq!(glyph(&grid, 0, 2), 'x');
}

// --- The owed wrap at the right edge -------------------------------------

#[test]
fn filling_the_last_column_leaves_the_cursor_resting_on_it() {
    let grid = render_bytes(4, 2, b"abcd");
    assert_eq!(row_text(&grid, 0), "abcd");
    assert_eq!(row_text(&grid, 1), "");
    assert_eq!((grid.cursor_col(), grid.cursor_row()), (3, 0));
}

#[test]
fn the_glyph_after_a_full_row_pays_the_owed_wrap() {
    let grid = render_bytes(4, 2, b"abcde");
    assert_eq!(row_text(&grid, 0), "abcd");
    assert_eq!(row_text(&grid, 1), "e");
    assert_eq!((grid.cursor_col(), grid.cursor_row()), (1, 1));
}

#[test]
fn a_full_width_bottom_row_does_not_scroll_the_screen() {
    // The reported defect: a full-screen monitor paints a status bar across
    // the whole bottom row. Taking the wrap eagerly turned that bar's last
    // glyph into a line feed on the bottom margin, scrolling every row up
    // one — after which the tool's next partial repaint landed a row out.
    let grid = render_bytes(4, 3, b"top\x1b[3;1Habcd");
    assert_eq!(row_text(&grid, 0), "top");
    assert_eq!(row_text(&grid, 1), "");
    assert_eq!(row_text(&grid, 2), "abcd");
    assert_eq!((grid.cursor_col(), grid.cursor_row()), (3, 2));
}

#[test]
fn the_glyph_after_a_full_bottom_row_scrolls_exactly_one_line() {
    let grid = render_bytes(4, 3, b"top\x1b[3;1Habcde");
    assert_eq!(row_text(&grid, 0), "");
    assert_eq!(row_text(&grid, 1), "abcd");
    assert_eq!(row_text(&grid, 2), "e");
}

#[test]
fn an_absolute_move_cancels_the_owed_wrap() {
    let grid = render_bytes(4, 2, b"abcd\x1b[1;1HZ");
    assert_eq!(row_text(&grid, 0), "Zbcd");
    assert_eq!(row_text(&grid, 1), "");
}

#[test]
fn a_carriage_return_cancels_the_owed_wrap() {
    let grid = render_bytes(4, 2, b"abcd\rY");
    assert_eq!(row_text(&grid, 0), "Ybcd");
    assert_eq!(row_text(&grid, 1), "");
}

#[test]
fn a_rubout_at_the_right_edge_erases_the_glyph_that_filled_the_row() {
    // A rubout is backspace, space, backspace: with the wrap owed the cursor
    // already rests on the last column, so it erases the row's final glyph
    // rather than the one before it.
    let grid = render_bytes(4, 2, b"abcd\x08 \x08");
    assert_eq!(row_text(&grid, 0), "abc");
    assert_eq!(glyph(&grid, 3, 0), ' ');
    assert_eq!((grid.cursor_col(), grid.cursor_row()), (3, 0));
}

#[test]
fn erase_to_end_of_line_from_the_owed_wrap_clears_the_last_cell() {
    let grid = render_bytes(4, 2, b"abcd\x1b[K");
    assert_eq!(row_text(&grid, 0), "abc");
    assert_eq!(glyph(&grid, 3, 0), ' ');
}

#[test]
fn setting_the_scroll_region_homes_the_cursor_into_the_region() {
    // `DECSTBM` homes to the top-left of the region, not of the screen, so a
    // program that reserves a header row starts drawing below it.
    let grid = render_bytes(4, 4, b"\x1b[2;4r");
    assert_eq!((grid.cursor_col(), grid.cursor_row()), (0, 1));
}

/// The shared conformance script's view of this screen model.
struct ConformanceGrid {
    grid: Grid,
}

impl tairix_vt::conformance::ScreenModel for ConformanceGrid {
    fn cols(&self) -> u16 {
        self.grid.cols()
    }

    fn rows(&self) -> u16 {
        self.grid.rows()
    }

    fn apply(&mut self, op: &Op) {
        crate::parser::apply(&mut self.grid, op);
    }

    fn glyph(&self, col: u16, row: u16) -> char {
        glyph(&self.grid, col, row)
    }

    fn cursor(&self) -> (u16, u16) {
        (self.grid.cursor_col(), self.grid.cursor_row())
    }
}

#[test]
fn the_screen_model_passes_the_shared_conformance_script() {
    // The framebuffer console runs this same script, so the two screens a
    // program can be drawn on cannot disagree about where its output lands.
    let mut screen = ConformanceGrid {
        grid: Grid::new(tairix_vt::conformance::COLS, tairix_vt::conformance::ROWS)
            .expect("valid conformance size"),
    };
    if let Err(divergence) = tairix_vt::conformance::check(&mut screen) {
        panic!("screen conformance: {divergence:?}");
    }
}

#[test]
fn overwriting_either_half_of_a_wide_glyph_clears_the_other_half() {
    for col in [0, 1] {
        let mut grid = Grid::new(4, 1).expect("valid size");
        grid.write_char('日');
        grid.move_to(col, 0);
        grid.write_char('x');

        assert_eq!(glyph(&grid, col, 0), 'x');
        assert_eq!(glyph(&grid, 1 - col, 0), ' ');
    }
}

#[test]
fn partial_erase_expands_to_clear_a_complete_wide_glyph() {
    let mut grid = Grid::new(4, 1).expect("valid size");
    grid.write_char('日');
    grid.move_to(0, 0);
    grid.erase_in_line(tairix_vt::EraseMode::ToStart);
    assert_eq!((glyph(&grid, 0, 0), glyph(&grid, 1, 0)), (' ', ' '));

    grid.move_to(0, 0);
    grid.write_char('日');
    grid.move_to(1, 0);
    grid.erase_in_line(tairix_vt::EraseMode::ToEnd);
    assert_eq!((glyph(&grid, 0, 0), glyph(&grid, 1, 0)), (' ', ' '));
}

#[test]
fn writing_text_fills_cells_and_advances_cursor() {
    let grid = render_bytes(20, 3, b"hi");
    assert_eq!(row_text(&grid, 0), "hi");
    assert_eq!(grid.cursor_col(), 2);
    assert_eq!(grid.cursor_row(), 0);
}

#[test]
fn writing_past_the_right_edge_wraps_to_the_next_row() {
    let grid = render_bytes(3, 3, b"abcd");
    assert_eq!(row_text(&grid, 0), "abc");
    assert_eq!(row_text(&grid, 1), "d");
    assert_eq!(grid.cursor_row(), 1);
    assert_eq!(grid.cursor_col(), 1);
}

#[test]
fn line_feed_on_the_last_row_scrolls_the_screen_up() {
    // A tty emits CRLF between lines; line feed alone only moves down.
    let grid = render_bytes(4, 2, b"aa\r\nbb\r\ncc");
    // "aa" scrolled off the top; "bb" moved up and "cc" is on the last row.
    assert_eq!(row_text(&grid, 0), "bb");
    assert_eq!(row_text(&grid, 1), "cc");
    assert_eq!(grid.cursor_row(), 1);
}

#[test]
fn line_feed_alone_moves_down_without_returning_to_column_zero() {
    let grid = render_bytes(10, 3, b"ab\ncd");
    assert_eq!(row_text(&grid, 0), "ab");
    // No carriage return, so the second word starts under the cursor column.
    assert_eq!(glyph(&grid, 2, 1), 'c');
    assert_eq!(glyph(&grid, 3, 1), 'd');
    assert_eq!(grid.cursor_row(), 1);
}

#[test]
fn carriage_return_returns_to_column_zero_and_overwrites() {
    let grid = render_bytes(10, 2, b"abc\rX");
    assert_eq!(row_text(&grid, 0), "Xbc");
    assert_eq!(grid.cursor_col(), 1);
}

#[test]
fn backspace_moves_left_without_erasing() {
    let grid = render_bytes(10, 2, b"abc\x08");
    assert_eq!(row_text(&grid, 0), "abc");
    assert_eq!(grid.cursor_col(), 2);
}

#[test]
fn tab_advances_to_the_next_tab_stop() {
    let grid = render_bytes(20, 2, b"a\tb");
    assert_eq!(glyph(&grid, 8, 0), 'b');
    assert_eq!(glyph(&grid, 0, 0), 'a');
}

#[test]
fn csi_cursor_position_is_one_based() {
    // ESC[2;3H places the cursor on row 2, column 3 (1-based).
    let mut grid = render_bytes(10, 5, b"\x1b[2;3H");
    assert_eq!(grid.cursor_row(), 1);
    assert_eq!(grid.cursor_col(), 2);
    grid.write_char('Z');
    assert_eq!(glyph(&grid, 2, 1), 'Z');
}

#[test]
fn csi_home_with_no_params_is_top_left() {
    let grid = render_bytes(10, 5, b"abc\x1b[H");
    assert_eq!(grid.cursor_row(), 0);
    assert_eq!(grid.cursor_col(), 0);
}

#[test]
fn csi_relative_cursor_moves_default_to_one() {
    // Down two, right three, then up one, left one.
    let grid = render_bytes(10, 10, b"\x1b[2B\x1b[3C\x1b[A\x1b[D");
    assert_eq!(grid.cursor_row(), 1);
    assert_eq!(grid.cursor_col(), 2);
}

#[test]
fn csi_erase_in_line_clears_to_end() {
    let grid = render_bytes(10, 2, b"abcdef\x1b[3D\x1b[K");
    assert_eq!(row_text(&grid, 0), "abc");
}

#[test]
fn csi_erase_in_display_two_clears_everything() {
    let grid = render_bytes(6, 2, b"hello\nworld\x1b[2J");
    assert_eq!(row_text(&grid, 0), "");
    assert_eq!(row_text(&grid, 1), "");
}

#[test]
fn unrecognised_escape_and_high_bytes_are_dropped() {
    // A non-CSI escape (ESC c), a private mode we do not model (bracketed
    // paste, ESC[?2004l), and a stray high byte all leave the visible text
    // intact: the shared parser drops what it does not recognise.
    let grid = render_bytes(10, 2, b"a\x1bcb\x1b[?2004lc\xffd");
    assert_eq!(row_text(&grid, 0), "abcd");
}

#[test]
fn pump_applies_shell_output_to_the_screen() {
    let shell = QueueShell::with_output(&[b"hello"]);
    let mut term = Terminal::new(20, 3, shell).expect("valid size");
    let applied = term.pump().expect("read succeeds");
    assert_eq!(applied, 5);
    assert_eq!(row_text(term.grid(), 0), "hello");
}

#[test]
fn pump_with_no_output_changes_nothing() {
    let shell = QueueShell::default();
    let mut term = Terminal::new(20, 3, shell).expect("valid size");
    let applied = term.pump().expect("empty read succeeds");
    assert_eq!(applied, 0);
    assert_eq!(row_text(term.grid(), 0), "");
}

#[test]
fn pump_propagates_a_read_error_and_leaves_the_screen_unchanged() {
    let mut shell = QueueShell::with_output(&[b"after"]);
    shell.read_error = Some(Errno::NotFound);
    let mut term = Terminal::new(20, 3, shell).expect("valid size");
    assert_eq!(term.pump(), Err(Errno::NotFound));
    assert_eq!(row_text(term.grid(), 0), "");
}

#[test]
fn send_forwards_input_to_the_shell() {
    let mut term = Terminal::new(20, 3, QueueShell::default()).expect("valid size");
    term.send_str("ls\n").expect("write succeeds");
    term.send(b"\x03").expect("write succeeds");
    // The model never echoes input to the screen.
    assert_eq!(row_text(term.grid(), 0), "");
}

#[test]
fn shell_seam_captures_written_bytes_verbatim() {
    let mut shell = QueueShell::default();
    shell.write(b"ls\n").expect("write succeeds");
    shell.write(b"\x03").expect("write succeeds");
    assert_eq!(shell.written, b"ls\n\x03");
}

#[test]
fn send_propagates_a_write_error() {
    let shell = QueueShell {
        write_error: Some(Errno::PermissionDenied),
        ..QueueShell::default()
    };
    let mut term = Terminal::new(20, 3, shell).expect("valid size");
    assert_eq!(term.send_str("x"), Err(Errno::PermissionDenied));
}

/// The colours a terminal on the system scheme paints with, fully opaque —
/// what a test that is not about the profile expects to see.
fn system_colors(theme: &Theme) -> crate::scheme::Painted {
    crate::scheme::Painted::resolve(
        crate::scheme::Scheme::System,
        &crate::scheme::ColorScheme::from_theme(theme),
        theme,
        u8::MAX,
    )
}

/// A `width` × `height` screen painted once from `grid` — the whole-window
/// first paint every renderer test that only cares about pixels wants.
fn painted_screen(
    grid: &Grid,
    painted: &crate::scheme::Painted,
    width: u32,
    height: u32,
) -> Screen {
    let mut screen = Screen::new(width, height).expect("surface");
    let _ = screen.paint(grid, painted, test_font());
    screen
}

/// The surface rectangle a `cols` × `rows` block of cells at `(col, row)`
/// occupies in the test face.
fn cell_rect(col: u32, row: u32, cols: u32, rows: u32) -> Rect {
    let font = test_font();
    let (cw, lh) = (font.cell_width(), font.line_height());
    Rect::new(
        i32::try_from(cw * col).expect("in range"),
        i32::try_from(lh * row).expect("in range"),
        cw * cols,
        lh * rows,
    )
}

/// A `cols` × `rows` terminal and a screen sized exactly to it.
fn grid_and_screen(cols: u16, rows: u16) -> (Terminal<QueueShell>, Screen) {
    let font = test_font();
    let terminal = Terminal::new(cols, rows, QueueShell::default()).expect("valid size");
    let screen = Screen::new(
        font.cell_width() * u32::from(cols),
        font.line_height() * u32::from(rows),
    )
    .expect("surface");
    (terminal, screen)
}

#[test]
fn render_produces_a_surface_of_the_viewport_size() {
    let shell = QueueShell::with_output(&[b"hi"]);
    let mut term = Terminal::new(20, 3, shell).expect("valid size");
    term.pump().expect("read succeeds");
    let theme = Theme::dark();
    let screen = painted_screen(term.grid(), &system_colors(&theme), 120, 40);
    assert_eq!(screen.surface().width(), 120);
    assert_eq!(screen.surface().height(), 40);
}

#[test]
fn hebrew_glyphs_occupy_distinct_single_cells() {
    let grid = render_bytes(5, 1, "אבםa".as_bytes());
    assert_eq!(row_text(&grid, 0), "אבםa");

    let overwritten = render_bytes(3, 1, "אב\u{1B}[1;2H ".as_bytes());
    assert_eq!(glyph(&overwritten, 0, 0), 'א');
    assert_eq!(glyph(&overwritten, 1, 0), ' ');
}

#[test]
fn render_keeps_hebrew_ink_inside_each_coloured_cell() {
    let mut term = Terminal::new(4, 1, QueueShell::default()).expect("valid size");
    term.feed("\u{1B}[?25l\u{1B}[48;2;10;20;30mאבם".as_bytes());
    let theme = Theme::dark();
    let screen = painted_screen(term.grid(), &system_colors(&theme), 60, 28);
    let surface = screen.surface();
    let background = Color::rgb(10, 20, 30);
    for column in 0..3 {
        let first_x = column * 15;
        assert!(
            (first_x..first_x + 15).any(|x| {
                (0..28).any(|y| {
                    surface.get(x, y).map(tairix_raster::Pixel::unpremultiply) != Some(background)
                })
            }),
            "Hebrew cell {column} contains only background"
        );
    }
}

#[test]
fn render_keeps_wide_japanese_ink_over_a_coloured_background() {
    let mut term = Terminal::new(3, 1, QueueShell::default()).expect("valid size");
    term.feed("\u{1B}[?25l\u{1B}[48;2;10;20;30m日".as_bytes());
    let theme = Theme::dark();
    let screen = painted_screen(term.grid(), &system_colors(&theme), 45, 28);
    let surface = screen.surface();
    let background = Color::rgb(10, 20, 30);
    assert!(
        (15..30).any(|x| {
            (0..28).any(|y| {
                surface.get(x, y).map(tairix_raster::Pixel::unpremultiply) != Some(background)
            })
        }),
        "the continuation cell contains only background"
    );
}

#[test]
fn render_highlights_the_cursor_cell() {
    let mut term = Terminal::new(20, 3, QueueShell::default()).expect("valid size");
    term.feed(b"\x1b[H");
    let theme = Theme::dark();
    let screen = painted_screen(term.grid(), &system_colors(&theme), 120, 40);
    let surface = screen.surface();
    let accent: Color = theme.palette().accent.into();
    let surface_bg: Color = theme.palette().surface.into();
    // The top-left pixel sits under the home cursor, so it carries the accent
    // fill rather than the plain surface colour.
    let top_left = surface.get(0, 0).map(tairix_raster::Pixel::unpremultiply);
    assert_eq!(top_left, Some(accent));
    assert_ne!(accent, surface_bg);
}

#[test]
fn render_handles_a_zero_sized_viewport_without_panicking() {
    let term = Terminal::new(20, 3, QueueShell::default()).expect("valid size");
    let theme = Theme::dark();
    // A zero-width viewport is degenerate but allocatable: it paints nothing.
    let screen = painted_screen(term.grid(), &system_colors(&theme), 0, 40);
    assert_eq!(screen.surface().width(), 0);
    assert!(screen.surface().pixels().is_empty());
}

#[test]
fn a_first_paint_damages_the_whole_window() {
    let theme = Theme::dark();
    let (term, mut screen) = grid_and_screen(20, 3);
    let damage = screen.paint(term.grid(), &system_colors(&theme), test_font());
    assert_eq!(damage, cell_rect(0, 0, 20, 3));
}

/// The reported defect: typing a character must cost the cell it wrote and
/// the two the cursor moved between — never the whole window.
#[test]
fn typing_one_character_damages_only_the_cells_it_touched() {
    let theme = Theme::dark();
    let painted = system_colors(&theme);
    let (mut term, mut screen) = grid_and_screen(80, 24);
    let _ = screen.paint(term.grid(), &painted, test_font());

    term.feed(b"a");

    // Column 0 gained a glyph and the cursor block moved to column 1.
    assert_eq!(
        screen.paint(term.grid(), &painted, test_font()),
        cell_rect(0, 0, 2, 1)
    );
}

#[test]
fn a_repaint_of_an_unchanged_grid_damages_nothing() {
    let theme = Theme::dark();
    let painted = system_colors(&theme);
    let (mut term, mut screen) = grid_and_screen(20, 3);
    term.feed(b"steady");
    let _ = screen.paint(term.grid(), &painted, test_font());

    assert!(screen.paint(term.grid(), &painted, test_font()).is_empty());
}

#[test]
fn a_hidden_cursor_reappearing_damages_only_its_own_cell() {
    let theme = Theme::dark();
    let painted = system_colors(&theme);
    let (mut term, mut screen) = grid_and_screen(20, 3);
    term.feed(b"\x1b[2;3H\x1b[?25l");
    let _ = screen.paint(term.grid(), &painted, test_font());

    term.feed(b"\x1b[?25h");

    assert_eq!(
        screen.paint(term.grid(), &painted, test_font()),
        cell_rect(2, 1, 1, 1)
    );
}

/// Overwriting the *continuation* half of a wide glyph must repaint the lead
/// cell too, or the orphaned left half of the old glyph would survive.
#[test]
fn clobbering_a_continuation_cell_damages_its_lead_cell() {
    let theme = Theme::dark();
    let painted = system_colors(&theme);
    let (mut term, mut screen) = grid_and_screen(10, 1);
    term.feed("\u{1B}[?25l日本".as_bytes());
    let _ = screen.paint(term.grid(), &painted, test_font());

    // Column 1 is 日's continuation; writing there leaves column 0 stale.
    term.feed(b"\x1b[1;2Hx");

    let damage = screen.paint(term.grid(), &painted, test_font());
    assert_eq!(damage.left(), 0, "the lead cell was not repainted");
}

/// The safety property behind repainting only part of the window: whatever
/// the change, the retained surface must end up byte-identical to a fresh
/// whole-window paint of the same grid.
#[test]
fn an_incremental_repaint_matches_a_whole_one_pixel_for_pixel() {
    const CASES: &[(&str, &str)] = &[
        ("hello", "!"),
        ("hello", "\u{1B}[1;1Hx"),
        ("ab", "\u{1B}[?25l"),
        ("\u{1B}[?25labc", "\u{1B}[?25h"),
        ("line one\r\nline two", "\r\n\r\n\r\nscrolled"),
        ("日本", "\u{1B}[1;2Hx"),
        ("日本", "\u{1B}[1;1H\u{1B}[K"),
        ("x", "\u{1B}[41mred"),
        ("\u{1B}[41mred", "\u{1B}[0m plain"),
        ("filled text here", "\u{1B}[2J"),
        ("abc", "\u{1B}[1;1H\u{1B}[1;33mA"),
    ];
    let theme = Theme::dark();
    let painted = system_colors(&theme);
    let font = test_font();
    for (setup, change) in CASES {
        let (mut term, mut incremental) = grid_and_screen(12, 4);
        term.feed(setup.as_bytes());
        let _ = incremental.paint(term.grid(), &painted, font);
        term.feed(change.as_bytes());
        let _ = incremental.paint(term.grid(), &painted, font);

        let whole = painted_screen(
            term.grid(),
            &painted,
            incremental.surface().width(),
            incremental.surface().height(),
        );
        assert_eq!(
            incremental.surface().pixels(),
            whole.surface().pixels(),
            "incremental repaint diverged after {change:?}"
        );
    }
}

/// The other half of the safety argument: every pixel a repaint *changes*
/// must lie inside the rectangle it reported, or the session would leave a
/// stale patch on screen. Walked over a script that scrolls, recolours,
/// erases and clobbers wide glyphs, checking both directions each step.
#[test]
fn every_pixel_a_repaint_changes_lies_inside_the_rect_it_reported() {
    const SCRIPT: &[&str] = &[
        "hello world",
        "\r\n",
        "\u{1B}[41mred on black",
        "\u{1B}[0m\r\n日本語",
        "\u{1B}[1;1Hx",
        "\u{1B}[2;2H\u{1B}[K",
        "\u{1B}[?25l",
        "more text\r\n",
        "\u{1B}[?25h",
        "\r\n\r\n\r\nscrolled off",
        "\u{1B}[H\u{1B}[1;33mA",
        "\u{1B}[2J",
    ];
    let theme = Theme::dark();
    let painted = system_colors(&theme);
    let font = test_font();
    let (mut term, mut screen) = grid_and_screen(12, 4);
    let _ = screen.paint(term.grid(), &painted, font);

    for step in SCRIPT {
        let before = screen.surface().clone();
        term.feed(step.as_bytes());
        let reported = screen.paint(term.grid(), &painted, font);

        for y in 0..screen.surface().height() {
            for x in 0..screen.surface().width() {
                if screen.surface().get(x, y) == before.get(x, y) {
                    continue;
                }
                assert!(
                    reported.contains(Point::new(
                        i32::try_from(x).expect("in range"),
                        i32::try_from(y).expect("in range")
                    )),
                    "{step:?} changed ({x}, {y}) outside the reported {reported:?}"
                );
            }
        }

        let whole = painted_screen(
            term.grid(),
            &painted,
            screen.surface().width(),
            screen.surface().height(),
        );
        assert_eq!(
            screen.surface().pixels(),
            whole.surface().pixels(),
            "the retained screen diverged after {step:?}"
        );
    }
}

#[test]
fn invalidating_forces_the_next_paint_to_cover_the_window() {
    let theme = Theme::dark();
    let painted = system_colors(&theme);
    let (mut term, mut screen) = grid_and_screen(20, 3);
    term.feed(b"text");
    let _ = screen.paint(term.grid(), &painted, test_font());

    screen.invalidate();

    assert_eq!(
        screen.paint(term.grid(), &painted, test_font()),
        cell_rect(0, 0, 20, 3)
    );
}

#[test]
fn a_resize_repaints_the_whole_new_surface() {
    let theme = Theme::dark();
    let painted = system_colors(&theme);
    let font = test_font();
    let (mut term, mut screen) = grid_and_screen(20, 3);
    term.feed(b"text");
    let _ = screen.paint(term.grid(), &painted, font);

    assert!(screen.resize(font.cell_width() * 10, font.line_height() * 2));
    let _ = term.resize(10, 2);

    assert_eq!(
        screen.paint(term.grid(), &painted, font),
        cell_rect(0, 0, 10, 2)
    );
}

/// A grid reshaped under a screen of the same pixel size still repaints
/// whole: the cell snapshot no longer describes the new shape.
#[test]
fn a_reshaped_grid_repaints_the_whole_window() {
    let theme = Theme::dark();
    let painted = system_colors(&theme);
    let (mut term, mut screen) = grid_and_screen(20, 3);
    let _ = screen.paint(term.grid(), &painted, test_font());

    let _ = term.resize(3, 20);

    assert_eq!(
        screen.paint(term.grid(), &painted, test_font()),
        cell_rect(0, 0, 20, 3)
    );
}

#[test]
fn sgr_folds_colour_and_flags_into_the_written_cells() {
    // Bold + red foreground, then "Hi"; both glyphs carry the folded rendition.
    let grid = render_bytes(10, 2, b"\x1b[1;31mHi");
    let cell = cell_at(&grid, 0, 0);
    assert_eq!(cell.ch, 'H');
    assert!(cell.attrs.bold);
    assert_eq!(cell.attrs.foreground, VtColor::Basic(BasicColor::Red));
    assert_eq!(
        cell_at(&grid, 1, 0).attrs.foreground,
        VtColor::Basic(BasicColor::Red)
    );
}

#[test]
fn sgr_reset_returns_following_cells_to_plain() {
    let grid = render_bytes(10, 2, b"\x1b[1ma\x1b[0mb");
    assert!(cell_at(&grid, 0, 0).attrs.bold);
    assert_eq!(cell_at(&grid, 1, 0).attrs, tairix_vt::Attributes::PLAIN);
}

#[test]
fn sgr_256_index_and_truecolour_reach_the_cells() {
    // 256-colour foreground 200, then truecolour background 10;20;30.
    let grid = render_bytes(10, 2, b"\x1b[38;5;200mx\x1b[48;2;10;20;30my");
    assert_eq!(cell_at(&grid, 0, 0).attrs.foreground, VtColor::Indexed(200));
    let y = cell_at(&grid, 1, 0);
    assert_eq!(y.attrs.foreground, VtColor::Indexed(200));
    assert_eq!(y.attrs.background, VtColor::Rgb(10, 20, 30));
}

#[test]
fn scroll_region_confines_scrolling_to_its_rows() {
    // Width 4 leaves a trailing blank column so filling a row never wraps.
    let mut grid = Grid::new(4, 3).expect("valid size");
    put_row(&mut grid, 0, "aaa");
    put_row(&mut grid, 1, "bbb");
    put_row(&mut grid, 2, "ccc");
    // Restrict scrolling to rows 2..=3 (1-based) and scroll that region up one.
    grid.set_scroll_region(2, 3);
    grid.scroll_up(1);
    assert_eq!(row_text(&grid, 0), "aaa"); // outside the region, untouched
    assert_eq!(row_text(&grid, 1), "ccc"); // pulled up within the region
    assert_eq!(row_text(&grid, 2), ""); // freed bottom line blanked
}

#[test]
fn line_feed_at_the_bottom_margin_scrolls_only_the_region() {
    let mut grid = Grid::new(4, 3).expect("valid size");
    put_row(&mut grid, 0, "aaa");
    put_row(&mut grid, 1, "bbb");
    put_row(&mut grid, 2, "ccc");
    // Region rows 2..=3 (1-based); homing leaves the cursor at the top, so step
    // it down to the bottom margin and feed a line: the region scrolls up but
    // row 1 (outside it) is untouched.
    grid.set_scroll_region(2, 3);
    grid.move_to(0, 2);
    grid.line_feed();
    assert_eq!(row_text(&grid, 0), "aaa");
    assert_eq!(row_text(&grid, 1), "ccc");
    assert_eq!(row_text(&grid, 2), "");
    assert_eq!(grid.cursor_row(), 2); // stays on the bottom margin
}

#[test]
fn alternate_screen_saves_and_restores_the_main_screen() {
    let mut term = Terminal::new(10, 2, QueueShell::default()).expect("valid size");
    term.feed(b"main");
    term.feed(b"\x1b[?1049h");
    assert!(term.grid().on_alternate_screen());
    // The alternate screen starts blank; what we draw there is independent.
    assert_eq!(row_text(term.grid(), 0), "");
    term.feed(b"alt");
    assert_eq!(row_text(term.grid(), 0), "alt");
    term.feed(b"\x1b[?1049l");
    assert!(!term.grid().on_alternate_screen());
    assert_eq!(row_text(term.grid(), 0), "main");
}

#[test]
fn cursor_visibility_follows_the_show_hide_sequences() {
    let mut term = Terminal::new(10, 2, QueueShell::default()).expect("valid size");
    assert!(term.grid().cursor_visible());
    term.feed(b"\x1b[?25l");
    assert!(!term.grid().cursor_visible());
    term.feed(b"\x1b[?25h");
    assert!(term.grid().cursor_visible());
}

#[test]
fn hidden_cursor_is_not_painted() {
    let mut term = Terminal::new(20, 3, QueueShell::default()).expect("valid size");
    term.feed(b"\x1b[H\x1b[?25l");
    let theme = Theme::dark();
    let screen = painted_screen(term.grid(), &system_colors(&theme), 120, 40);
    let surface = screen.surface();
    let surface_bg: Color = theme.palette().surface.into();
    // With the cursor hidden the home cell shows the plain surface, not accent.
    let top_left = surface.get(0, 0).map(tairix_raster::Pixel::unpremultiply);
    assert_eq!(top_left, Some(surface_bg));
}

#[test]
fn osc_sets_the_window_title() {
    let grid = render_bytes(10, 2, b"\x1b]0;TAIRiX\x07rest");
    assert_eq!(grid.title(), "TAIRiX");
    // The title sequence leaves the screen text alone.
    assert_eq!(row_text(&grid, 0), "rest");
}

#[test]
fn saved_cursor_round_trips_position_and_pen() {
    let grid = render_bytes(10, 3, b"\x1b[2;3H\x1b[1m\x1b7\x1b[H\x1b[0m\x1b8Z");
    // ESC 8 restored the saved (row 2, col 3) position and the bold pen, so the
    // glyph lands there and is bold.
    let cell = cell_at(&grid, 2, 1);
    assert_eq!(cell.ch, 'Z');
    assert!(cell.attrs.bold);
}

#[test]
fn emitter_output_is_parsed_identically_by_the_consumer() {
    // The "one vocabulary" guarantee: the emulator consumes exactly what
    // `lib/vt`'s emitter produces. Encode a representative operation stream and
    // feed the bytes straight into the consumer's grid.
    let ops = [
        Op::Sgr(Sgr::Foreground(VtColor::Rgb(0x30, 0x70, 0xf0))),
        Op::Sgr(Sgr::Bold),
        Op::Print('O'),
        Op::Print('k'),
        Op::CarriageReturn,
        Op::LineFeed,
        Op::Sgr(Sgr::Reset),
        Op::Print('p'),
    ];
    let grid = render_bytes(10, 3, &encode_all(&ops));

    let first = cell_at(&grid, 0, 0);
    assert_eq!(first.ch, 'O');
    assert!(first.attrs.bold);
    assert_eq!(first.attrs.foreground, VtColor::Rgb(0x30, 0x70, 0xf0));
    assert_eq!(cell_at(&grid, 1, 0).ch, 'k');

    let plain = cell_at(&grid, 0, 1);
    assert_eq!(plain.ch, 'p');
    assert_eq!(plain.attrs, tairix_vt::Attributes::PLAIN);
}

// --- The spawned shell's pty wiring (`spawned`) --------------------------

#[test]
fn shell_wires_route_input_output_and_diagnostics_onto_the_pty_slave() {
    use tairix_abi::{FdWire, SpawnAttach, CONSOLE_INHERIT, SPAWN_UID_INHERIT};

    let attach = crate::spawned::shell_wires(7);
    // The child's stdin, stdout, *and* stderr are all the one pty slave (a
    // terminal shows output and diagnostics on the same tty, and the shell
    // sees one controlling terminal); stdinfo is closed.
    assert_eq!(attach.wires[0], FdWire::Handle(7));
    assert_eq!(attach.wires[1], FdWire::Handle(7));
    assert_eq!(attach.wires[2], FdWire::Handle(7));
    assert_eq!(attach.wires[3], FdWire::Closed);
    // Credential and console are inherited: the wires narrow, never widen.
    assert_eq!(attach.target_uid, SPAWN_UID_INHERIT);
    assert_eq!(attach.console, CONSOLE_INHERIT);
    assert_eq!(attach.flags, 0);
    // The block is canonical: it survives the same parse the kernel runs.
    assert_eq!(SpawnAttach::parse(&attach.to_le_bytes()), Ok(attach));
}

#[test]
fn shell_env_forwards_the_inherited_environment_and_sets_term() {
    use crate::spawned::shell_env;

    // The logged-in user's identity and locale reach the shell unchanged, so
    // its prompt shows the real user (never the "user@host" fallback), and
    // the terminal's own TERM is appended.
    let inherited: [&[u8]; 4] = [
        b"USER=root",
        b"HOME=/Users/root",
        b"LOGNAME=root",
        b"LANG=en-US",
    ];
    let env = shell_env("xterm-256color", inherited.iter().copied());
    assert_eq!(
        env,
        alloc::vec![
            b"USER=root".to_vec(),
            b"HOME=/Users/root".to_vec(),
            b"LOGNAME=root".to_vec(),
            b"LANG=en-US".to_vec(),
            b"TERM=xterm-256color".to_vec(),
        ]
    );
}

#[test]
fn shell_env_replaces_any_inherited_term_with_the_emulators_own() {
    use crate::spawned::shell_env;

    // A stale inherited TERM (from the text console the session descended
    // from) must never describe this graphical emulator: it is dropped and
    // the terminal's own value is the only TERM the shell sees.
    let inherited: [&[u8]; 3] = [b"TERM=vt100", b"USER=root", b"TERM=linux"];
    let env = shell_env("xterm-256color", inherited.iter().copied());
    assert_eq!(
        env,
        alloc::vec![b"USER=root".to_vec(), b"TERM=xterm-256color".to_vec()]
    );
    // Exactly one TERM entry, and it is the emulator's.
    assert_eq!(
        env.iter().filter(|e| e.starts_with(b"TERM=")).count(),
        1,
        "the shell must see exactly one TERM, naming this emulator",
    );
}

#[test]
fn shell_env_with_no_inherited_environment_still_sets_term() {
    use crate::spawned::shell_env;

    // A terminal spawned with an empty environment (no login ran) still hands
    // the shell a valid TERM rather than nothing.
    let env = shell_env("xterm-256color", core::iter::empty());
    assert_eq!(env, alloc::vec![b"TERM=xterm-256color".to_vec()]);
}

#[test]
fn pipe_source_read_drains_one_bounded_chunk() {
    let data: &[u8] = b"prompt$ ";
    let mut served = false;
    let mut source = crate::spawned::StreamShellSource::new(
        |out: &mut [u8]| {
            assert_eq!(out.len(), crate::spawned::READ_CHUNK);
            assert!(!served, "one wake drains exactly one chunk");
            served = true;
            out[..data.len()].copy_from_slice(data);
            Ok(data.len())
        },
        |_: &[u8]| -> Result<usize, Errno> { unreachable!("read never writes") },
    );
    assert_eq!(source.read(), Ok(data.to_vec()));
}

#[test]
fn pipe_source_read_surfaces_eof_and_errors_as_refusals() {
    // End-of-stream (the shell exited, pipe drained) is the seam's typed
    // "shell has exited" refusal, never a fabricated empty read.
    let mut source = crate::spawned::StreamShellSource::new(
        |_: &mut [u8]| Ok(0),
        |_: &[u8]| -> Result<usize, Errno> { unreachable!() },
    );
    assert_eq!(source.read(), Err(Errno::NotFound));
    // A failing primitive propagates untouched.
    let mut failing = crate::spawned::StreamShellSource::new(
        |_: &mut [u8]| Err(Errno::BadAddress),
        |_: &[u8]| -> Result<usize, Errno> { unreachable!() },
    );
    assert_eq!(failing.read(), Err(Errno::BadAddress));
}

#[test]
fn pipe_source_write_loops_over_short_writes_until_delivered() {
    let mut delivered: Vec<u8> = Vec::new();
    let mut calls = 0usize;
    {
        let mut source = crate::spawned::StreamShellSource::new(
            |_: &mut [u8]| -> Result<usize, Errno> { unreachable!("write never reads") },
            |bytes: &[u8]| {
                calls += 1;
                // Accept at most three bytes per call: a full pipe accepts
                // its free space, and the source must resume at the tail.
                let n = bytes.len().min(3);
                delivered.extend_from_slice(&bytes[..n]);
                Ok(n)
            },
        );
        assert_eq!(source.write(b"echo hi\n"), Ok(()));
    }
    assert_eq!(delivered, b"echo hi\n");
    assert_eq!(calls, 3);
}

#[test]
fn pipe_source_write_fails_closed_on_a_wedged_or_failing_channel() {
    // A zero-byte acceptance for a non-empty remainder can only mean a
    // broken channel: fail closed rather than spin.
    let mut wedged = crate::spawned::StreamShellSource::new(
        |_: &mut [u8]| -> Result<usize, Errno> { unreachable!() },
        |_: &[u8]| Ok(0),
    );
    assert_eq!(wedged.write(b"x"), Err(Errno::BrokenPipe));
    // A failing primitive propagates untouched.
    let mut failing = crate::spawned::StreamShellSource::new(
        |_: &mut [u8]| -> Result<usize, Errno> { unreachable!() },
        |_: &[u8]| Err(Errno::BrokenPipe),
    );
    assert_eq!(failing.write(b"x"), Err(Errno::BrokenPipe));
    // A zero-length write is complete before the primitive is consulted.
    let mut untouched = crate::spawned::StreamShellSource::new(
        |_: &mut [u8]| -> Result<usize, Errno> { unreachable!() },
        |_: &[u8]| -> Result<usize, Errno> { unreachable!("nothing to deliver") },
    );
    assert_eq!(untouched.write(b""), Ok(()));
}

#[test]
fn shell_load_failure_classifies_reserved_statuses() {
    use tairix_abi::{
        Signal, WaitStatus, LOAD_MALFORMED, LOAD_NOT_FOUND, LOAD_OOM, LOAD_UNVERIFIED,
    };

    use crate::spawned::shell_load_failure;

    // Each reserved asynchronous load-failure status the child can exit with
    // maps to the terse reason the terminal reports fail-loud when its
    // hosted shell never got off the ground.
    assert_eq!(
        shell_load_failure(WaitStatus::Exited(LOAD_NOT_FOUND)),
        Some("program not found or not readable")
    );
    assert_eq!(
        shell_load_failure(WaitStatus::Exited(LOAD_UNVERIFIED)),
        Some("signature or hash verification failed")
    );
    assert_eq!(
        shell_load_failure(WaitStatus::Exited(LOAD_MALFORMED)),
        Some("executable is malformed or incompatible")
    );
    assert_eq!(
        shell_load_failure(WaitStatus::Exited(LOAD_OOM)),
        Some("out of memory while loading")
    );
    // A clean or ordinary exit ends the terminal silently, and a stop is
    // never a terminal exit.
    assert_eq!(shell_load_failure(WaitStatus::Exited(0)), None);
    assert_eq!(shell_load_failure(WaitStatus::Exited(1)), None);
    assert_eq!(
        shell_load_failure(WaitStatus::Stopped(Signal::Terminate)),
        None
    );
}
