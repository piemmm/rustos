//! The renderer: one [`Editor`] state drawn into one curses [`Window`].
//!
//! Layout, top to bottom: the text area, the reverse-video status line,
//! and the message/command line. The renderer also *scrolls*: it updates
//! the editor's [`View`](crate::editor::View) so the cursor is always
//! visible, vertically and horizontally (long lines side-scroll — line
//! wrap is staged in `plans/VIM.md`).
//!
//! Highlights: the visual selection renders reverse-video; search matches
//! render underlined while `hlsearch` is lit. Tabs expand to the fixed
//! vim-default 8-column stops. Column arithmetic treats every character
//! as one cell (double-width CJK cell math is staged in `plans/VIM.md`).

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use tairix_curses::{Pos, Window};
use tairix_vt::Attributes;

use crate::editor::{Editor, Mode};

/// The fixed tab stop (vim's `tabstop` default).
pub const TABSTOP: usize = 8;

/// One rendered cell: the character shown and the source character column
/// it came from (a tab yields several cells with the same source column).
struct RenderCell {
    ch: char,
    char_col: usize,
}

/// Expand one line's tabs into screen cells.
fn expand(line: &str) -> Vec<RenderCell> {
    let mut cells = Vec::new();
    for (char_col, ch) in line.chars().enumerate() {
        if ch == '\t' {
            let pad = TABSTOP - (cells.len() % TABSTOP);
            for _ in 0..pad {
                cells.push(RenderCell { ch: ' ', char_col });
            }
        } else {
            cells.push(RenderCell { ch, char_col });
        }
    }
    cells
}

/// The screen column of character column `char_col` in `line`.
fn screen_col(line: &str, char_col: usize) -> usize {
    let mut col = 0usize;
    for (at, ch) in line.chars().enumerate() {
        if at >= char_col {
            break;
        }
        if ch == '\t' {
            col += TABSTOP - (col % TABSTOP);
        } else {
            col += 1;
        }
    }
    col
}

/// The decimal width of a line count (for the `'number'` gutter).
fn digits(mut value: usize) -> usize {
    let mut width = 1;
    while value >= 10 {
        value /= 10;
        width += 1;
    }
    width
}

/// The search-match spans (start..end character columns) of one line.
fn match_spans(editor: &Editor, line: &str) -> Vec<(usize, usize)> {
    let mut spans = Vec::new();
    if !editor.hlsearch {
        return spans;
    }
    let Some(search) = &editor.search else {
        return spans;
    };
    let mut from = 0usize;
    let len = line.chars().count();
    while from <= len {
        let Some((start, end)) = search.pattern.find_at(line, from) else {
            break;
        };
        spans.push((start, end));
        // A zero-width match still advances the scan.
        from = end.max(start + 1);
    }
    spans
}

/// Whether the buffer position `(line, char_col)` is inside the visual
/// selection.
fn selected(editor: &Editor, line: usize, char_col: usize) -> bool {
    let Some((start, end, linewise)) = editor.selection() else {
        return false;
    };
    if line < start.line || line > end.line {
        return false;
    }
    if linewise {
        return true;
    }
    let after_start = line > start.line || char_col >= start.col;
    let before_end = line < end.line || char_col <= end.col;
    after_start && before_end
}

/// Draw the editor into `window` and update the editor's view. The
/// window's cursor is left where the terminal cursor belongs.
pub fn render(editor: &mut Editor, window: &mut Window) {
    let size = window.size();
    let rows = size.rows as usize;
    let cols = size.cols as usize;
    if rows == 0 || cols == 0 {
        return;
    }
    let text_rows = rows.saturating_sub(2).max(1);
    let gutter = if editor.number {
        digits(editor.buffer.len_lines()) + 1
    } else {
        0
    };
    let text_cols = cols.saturating_sub(gutter).max(1);
    editor.view.rows = text_rows;
    editor.view.cols = text_cols;

    // Scroll the view so the cursor is visible.
    if editor.cursor.line < editor.view.top {
        editor.view.top = editor.cursor.line;
    } else if editor.cursor.line >= editor.view.top + text_rows {
        editor.view.top = editor.cursor.line + 1 - text_rows;
    }
    let cursor_screen_col = screen_col(editor.buffer.line(editor.cursor.line), editor.cursor.col);
    if cursor_screen_col < editor.view.left {
        editor.view.left = cursor_screen_col;
    } else if cursor_screen_col >= editor.view.left + text_cols {
        editor.view.left = cursor_screen_col + 1 - text_cols;
    }

    window.erase();

    let plain = Attributes::PLAIN;
    let mut reverse = Attributes::PLAIN;
    reverse.reverse = true;
    let mut underline = Attributes::PLAIN;
    underline.underline = true;
    let mut dim = Attributes::PLAIN;
    dim.dim = true;

    // The text area.
    for row in 0..text_rows {
        let line_index = editor.view.top + row;
        let row16 = row_u16(row);
        if line_index >= editor.buffer.len_lines() {
            window.set_attributes(dim);
            let _ = window.move_add_str(Pos::new(row16, 0), "~");
            window.set_attributes(plain);
            continue;
        }
        let line = editor.buffer.line(line_index);
        if gutter > 0 {
            window.set_attributes(dim);
            let number = format!("{:>width$} ", line_index + 1, width = gutter - 1);
            let _ = window.move_add_str(Pos::new(row16, 0), &number);
            window.set_attributes(plain);
        }
        let spans = match_spans(editor, line);
        let cells = expand(line);
        for (screen_at, cell) in cells
            .iter()
            .enumerate()
            .skip(editor.view.left)
            .take(text_cols)
        {
            let col16 = col_u16(gutter + screen_at - editor.view.left);
            let in_search = spans
                .iter()
                .any(|&(start, end)| cell.char_col >= start && cell.char_col < end);
            let attrs = if selected(editor, line_index, cell.char_col) {
                reverse
            } else if in_search {
                underline
            } else {
                plain
            };
            window.set_attributes(attrs);
            let _ = window.move_add_char(Pos::new(row16, col16), cell.ch);
        }
        window.set_attributes(plain);
    }

    if rows >= 2 {
        draw_status(editor, window, rows, cols);
    }
    let cursor_pos = draw_message(editor, window, rows, cols, gutter, cursor_screen_col);
    let _ = window.move_to(cursor_pos);
}

/// Draw the reverse-video status line: name and flags on the left, the
/// cursor position and file percentage on the right.
fn draw_status(editor: &Editor, window: &mut Window, rows: usize, cols: usize) {
    let plain = Attributes::PLAIN;
    let mut reverse = Attributes::PLAIN;
    reverse.reverse = true;
    let status_row = row_u16(rows - 2);
    let name = editor.buffer.name().unwrap_or("[No Name]");
    let modified = if editor.buffer.is_modified() {
        " [+]"
    } else {
        ""
    };
    let readonly = if editor.buffer.is_readonly() {
        " [RO]"
    } else {
        ""
    };
    let left_text = format!("{name}{modified}{readonly}");
    let position = format!(
        "{},{}         {}%",
        editor.cursor.line + 1,
        editor.cursor.col + 1,
        ((editor.cursor.line + 1) * 100) / editor.buffer.len_lines()
    );
    let pad = cols
        .saturating_sub(left_text.chars().count())
        .saturating_sub(position.chars().count());
    let mut status = String::new();
    status.push_str(&left_text);
    for _ in 0..pad {
        status.push(' ');
    }
    status.push_str(&position);
    window.set_attributes(reverse);
    let _ = window.move_add_str(Pos::new(status_row, 0), truncate(&status, cols));
    window.set_attributes(plain);
}

/// Draw the message / command line and return where the terminal cursor
/// belongs: on the command line while one is being edited, in the text
/// area otherwise.
fn draw_message(
    editor: &Editor,
    window: &mut Window,
    rows: usize,
    cols: usize,
    gutter: usize,
    cursor_screen_col: usize,
) -> Pos {
    let plain = Attributes::PLAIN;
    let mut reverse = Attributes::PLAIN;
    reverse.reverse = true;
    let message_row = row_u16(rows - 1);
    let cursor_pos = Pos::new(
        row_u16(editor.cursor.line - editor.view.top),
        col_u16(gutter + cursor_screen_col - editor.view.left),
    );
    if let Some(cmdline) = &editor.cmdline {
        let text = format!("{}{}", cmdline.prefix, cmdline.text);
        let _ = window.move_add_str(Pos::new(message_row, 0), truncate(&text, cols));
        return Pos::new(message_row, col_u16((1 + cmdline.cursor).min(cols - 1)));
    }
    if let Some(message) = &editor.message {
        let attrs = if message.error { reverse } else { plain };
        window.set_attributes(attrs);
        let _ = window.move_add_str(Pos::new(message_row, 0), truncate(&message.text, cols));
        window.set_attributes(plain);
        return cursor_pos;
    }
    let indicator = match editor.mode {
        Mode::Insert => "-- INSERT --",
        Mode::Replace => "-- REPLACE --",
        Mode::Visual { linewise: false } => "-- VISUAL --",
        Mode::Visual { linewise: true } => "-- VISUAL LINE --",
        Mode::Normal | Mode::CmdLine => "",
    };
    if !indicator.is_empty() {
        let mut bold = Attributes::PLAIN;
        bold.bold = true;
        window.set_attributes(bold);
        let _ = window.move_add_str(Pos::new(message_row, 0), indicator);
        window.set_attributes(plain);
    }
    cursor_pos
}

/// Truncate `text` to `cols` characters (cell math is width-1 here).
fn truncate(text: &str, cols: usize) -> &str {
    match text.char_indices().nth(cols) {
        Some((at, _)) => &text[..at],
        None => text,
    }
}

/// A row index as the window's `u16`, saturating.
fn row_u16(row: usize) -> u16 {
    u16::try_from(row).unwrap_or(u16::MAX)
}

/// A column index as the window's `u16`, saturating.
fn col_u16(col: usize) -> u16 {
    u16::try_from(col).unwrap_or(u16::MAX)
}
