//! Drawing the [`Model`] and running the input loop.
//!
//! [`render`] turns a model into curses windows; [`run`] is the thin
//! event-driven loop that ties the [`Screen`] driver, the model, and the
//! `sysinfo` transport together. Drawing is pure with respect to the model,
//! so a test renders to an in-memory [`Tty`] and inspects the bytes
//! (`AGENTS.md` §7).

use alloc::string::String;

use rustos_curses::{str_width, truncate_to_width, Pos, Screen, Size, Tty, Window};
use rustos_procinfo::{render_process, Transport, PROCESS_HEADER};
use rustos_vt::{Attributes, BasicColor, Color};

use crate::error::TopError;
use crate::model::{Action, Model};

/// Rows reserved above the process list: the title and the column header.
const HEADER_ROWS: u16 = 2;
/// Rows reserved below the process list: the key-hint footer.
const FOOTER_ROWS: u16 = 1;

/// The number of process rows that fit on a screen of `size`.
#[must_use]
pub fn list_capacity(size: Size) -> usize {
    usize::from(size.rows.saturating_sub(HEADER_ROWS + FOOTER_ROWS))
}

/// Draw `model` onto `screen` and flush it (the curses two-step:
/// `wnoutrefresh` each window, then `doupdate`).
///
/// The whole screen is composed in one base [`Window`]; when the help
/// overlay is showing, a second window is composited on top through the same
/// renderer — overlays stack by draw order, no separate panel machinery
/// needed (`AGENTS.md` §2.3).
///
/// # Errors
///
/// [`TopError::Terminal`] if the underlying tty write fails.
pub fn render<T: Tty>(model: &Model, screen: &mut Screen<T>) -> Result<(), TopError> {
    let size = screen.size();
    let cols = usize::from(size.cols);

    let header_attrs = header_attributes(screen);
    let selected_attrs = reversed();
    let plain = Attributes::PLAIN;

    let mut win = Window::new(Pos::ORIGIN, size);

    let title = title_line(model);
    put_line(&mut win, 0, cols, &title, header_attrs);

    if size.rows > 1 {
        put_line(&mut win, 1, cols, PROCESS_HEADER, bold());
    }

    let capacity = list_capacity(size);
    let scroll_top = model.scroll_top();
    for offset in 0..capacity {
        let row = HEADER_ROWS + u16::try_from(offset).unwrap_or(0);
        let index = scroll_top + offset;
        let Some(record) = model.processes().get(index) else {
            break;
        };
        let line = render_process(record);
        let attrs = if model.selected() == Some(index) {
            selected_attrs
        } else {
            plain
        };
        put_line(&mut win, row, cols, &line, attrs);
    }

    if size.rows >= HEADER_ROWS + FOOTER_ROWS {
        let footer_row = size.rows - 1;
        put_line(&mut win, footer_row, cols, FOOTER_HINT, header_attrs);
    }

    screen.wnoutrefresh(&win);

    if model.help_visible() {
        if let Some(overlay) = help_window(size) {
            screen.wnoutrefresh(&overlay);
        }
    }

    screen.doupdate().map_err(TopError::from)
}

/// Run the interactive viewer until the user quits or the input channel
/// closes.
///
/// The loop refreshes the snapshot once up front, then redraws and waits for
/// one event at a time. It re-queries only when the model asks
/// ([`Action::Refresh`], i.e. the scope toggle or the refresh key), so
/// scrolling never disturbs the listing. The cursor is hidden for the
/// duration and restored on exit.
///
/// # Errors
///
/// * [`TopError::PermissionDenied`] / [`TopError::Service`] — a refresh
///   query failed.
/// * [`TopError::Terminal`] — a tty read or write failed.
pub fn run<T: Transport, Y: Tty>(
    model: &mut Model,
    transport: &T,
    screen: &mut Screen<Y>,
) -> Result<(), TopError> {
    screen.set_cursor_visible(false);
    model.refresh(transport)?;
    let result = drive(model, transport, screen);
    screen.set_cursor_visible(true);
    result
}

/// The redraw/await/dispatch loop, separated so [`run`] can always restore
/// the cursor afterwards.
fn drive<T: Transport, Y: Tty>(
    model: &mut Model,
    transport: &T,
    screen: &mut Screen<Y>,
) -> Result<(), TopError> {
    loop {
        model.set_viewport(list_capacity(screen.size()));
        render(model, screen)?;
        let Some(event) = screen.getch().map_err(TopError::from)? else {
            return Ok(());
        };
        match model.handle_event(&event) {
            Action::Quit => return Ok(()),
            Action::Refresh => model.refresh(transport)?,
            Action::Redraw | Action::Ignore => {}
        }
    }
}

/// The footer key hints.
const FOOTER_HINT: &str = " q quit  a all/own  r refresh  up/down scroll  ? help ";

/// The status line: the process count and the active scope.
fn title_line(model: &Model) -> String {
    let mut line = String::from("RustOS top — ");
    push_usize(&mut line, model.processes().len());
    line.push_str(" processes [");
    line.push_str(model.scope().label());
    line.push(']');
    line
}

/// Append a base-ten `usize` without pulling in `format!`'s machinery for a
/// hot path; the counts are small.
fn push_usize(out: &mut String, mut value: usize) {
    if value == 0 {
        out.push('0');
        return;
    }
    let mut digits = [0u8; 20];
    let mut len = 0;
    while value > 0 {
        digits[len] = b'0' + u8::try_from(value % 10).unwrap_or(0);
        value /= 10;
        len += 1;
    }
    for &digit in digits[..len].iter().rev() {
        out.push(char::from(digit));
    }
}

/// Write `text` at the start of `row`, padded with spaces to `cols` columns
/// (so a highlight spans the row) and truncated to fit without splitting a
/// double-width glyph.
fn put_line(win: &mut Window, row: u16, cols: usize, text: &str, attrs: Attributes) {
    if win.move_to(Pos::new(row, 0)).is_err() {
        return;
    }
    win.set_attributes(attrs);
    let visible = truncate_to_width(text, cols);
    win.add_str(visible);
    let used = str_width(visible);
    for _ in used..cols {
        win.add_char(' ');
    }
    win.set_attributes(Attributes::PLAIN);
}

/// The header/footer rendition: white on blue when the terminal has colour,
/// falling back to reverse video when it does not.
fn header_attributes<T: Tty>(screen: &mut Screen<T>) -> Attributes {
    // A terminal that can show the 16 ANSI colours gets white-on-blue; a
    // monochrome terminal falls back to reverse video, so the header is
    // always distinct.
    if screen
        .capabilities()
        .color
        .supports(Color::Basic(BasicColor::Blue))
    {
        if let Ok(pair) = screen.alloc_pair(
            Color::Basic(BasicColor::White),
            Color::Basic(BasicColor::Blue),
        ) {
            let colors = screen.color_pairs().get(pair);
            let mut attrs = Attributes::PLAIN;
            attrs.foreground = colors.fg;
            attrs.background = colors.bg;
            return attrs;
        }
    }
    reversed()
}

/// Reverse-video attributes (the selection highlight, and the colourless
/// header fallback).
fn reversed() -> Attributes {
    let mut attrs = Attributes::PLAIN;
    attrs.reverse = true;
    attrs
}

/// Bold attributes (the column header).
fn bold() -> Attributes {
    let mut attrs = Attributes::PLAIN;
    attrs.bold = true;
    attrs
}

/// The help-overlay text, one entry per line.
const HELP_LINES: &[&str] = &[
    " Keys ",
    "",
    " up / down    move selection",
    " PgUp / PgDn  move a page",
    " Home / End   first / last",
    " a            all / own processes",
    " r            refresh now",
    " q            quit",
    " ?            toggle this help",
];

/// Build the centred help overlay window for a screen of `size`, or `None`
/// if the screen is too small to hold it.
fn help_window(size: Size) -> Option<Window> {
    let mut content_cols = 0usize;
    for line in HELP_LINES {
        content_cols = content_cols.max(str_width(line));
    }
    let box_cols = u16::try_from(content_cols + 2).ok()?;
    let box_rows = u16::try_from(HELP_LINES.len() + 2).ok()?;
    if box_rows > size.rows || box_cols > size.cols {
        return None;
    }
    let origin = Pos::new((size.rows - box_rows) / 2, (size.cols - box_cols) / 2);
    let mut win = Window::new(origin, Size::new(box_rows, box_cols));
    win.draw_box();
    for (offset, line) in HELP_LINES.iter().enumerate() {
        let row = 1 + u16::try_from(offset).unwrap_or(0);
        let _ = win.move_to(Pos::new(row, 1));
        win.add_str(truncate_to_width(line, content_cols));
    }
    Some(win)
}
