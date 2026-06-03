//! Headless unit tests for the terminal model and renderer.
//!
//! Everything is exercised without a kernel: the [`ShellSource`] seam is an
//! in-memory queue, so the grid, the control parser, the shell glue, and the
//! renderer are all testable in isolation (`AGENTS.md` §7).

use alloc::collections::VecDeque;
use alloc::string::String;
use alloc::vec::Vec;

use rustos_abi::Errno;
use rustos_geometry::Rect;
use rustos_raster::Color;
use rustos_theme::Theme;

use rustos_vt::{encode_all, BasicColor, Color as VtColor, Op, Sgr};

use crate::grid::{Grid, MAX_DIMENSION};
use crate::parser::Parser;
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
    // intact: the shared parser drops what it does not recognise (§2.9).
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

#[test]
fn render_produces_a_surface_of_the_viewport_size() {
    let shell = QueueShell::with_output(&[b"hi"]);
    let mut term = Terminal::new(20, 3, shell).expect("valid size");
    term.pump().expect("read succeeds");
    let theme = Theme::dark();
    let surface = crate::render(&term, &theme, Rect::new(0, 0, 120, 40)).expect("surface");
    assert_eq!(surface.width(), 120);
    assert_eq!(surface.height(), 40);
}

#[test]
fn render_highlights_the_cursor_cell() {
    let mut term = Terminal::new(20, 3, QueueShell::default()).expect("valid size");
    term.feed(b"\x1b[H");
    let theme = Theme::dark();
    let surface = crate::render(&term, &theme, Rect::new(0, 0, 120, 40)).expect("surface");
    let accent: Color = theme.palette().accent.into();
    let surface_bg: Color = theme.palette().surface.into();
    // The top-left pixel sits under the home cursor, so it carries the accent
    // fill rather than the plain surface colour.
    let top_left = surface.get(0, 0).map(rustos_raster::Pixel::unpremultiply);
    assert_eq!(top_left, Some(accent));
    assert_ne!(accent, surface_bg);
}

#[test]
fn render_handles_a_zero_sized_viewport_without_panicking() {
    let term = Terminal::new(20, 3, QueueShell::default()).expect("valid size");
    let theme = Theme::dark();
    // A zero-width viewport is degenerate but allocatable: it paints nothing.
    let surface = crate::render(&term, &theme, Rect::new(0, 0, 0, 40)).expect("surface");
    assert_eq!(surface.width(), 0);
    assert!(surface.pixels().is_empty());
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
    assert_eq!(cell_at(&grid, 1, 0).attrs, rustos_vt::Attributes::PLAIN);
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
    let surface = crate::render(&term, &theme, Rect::new(0, 0, 120, 40)).expect("surface");
    let surface_bg: Color = theme.palette().surface.into();
    // With the cursor hidden the home cell shows the plain surface, not accent.
    let top_left = surface.get(0, 0).map(rustos_raster::Pixel::unpremultiply);
    assert_eq!(top_left, Some(surface_bg));
}

#[test]
fn osc_sets_the_window_title() {
    let grid = render_bytes(10, 2, b"\x1b]0;RustOS\x07rest");
    assert_eq!(grid.title(), "RustOS");
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
    // The §2.2 "one vocabulary" guarantee: the emulator consumes exactly what
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
    assert_eq!(plain.attrs, rustos_vt::Attributes::PLAIN);
}
