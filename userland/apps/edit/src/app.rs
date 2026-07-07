//! Drawing the [`Model`] and running the input loop.
//!
//! [`render`] turns the model into curses windows; [`run`] is the thin
//! event-driven loop that ties the [`Screen`] driver, the model, and the
//! [`Fs`] seam together. Drawing is pure with respect to the model, so a
//! test renders to an in-memory [`Tty`] and inspects the bytes.
//!
//! The layout follows the QuickBasic/MS-DOS editor: a menu bar across the
//! top, the text area filling the middle (white on blue on a colour
//! terminal), and a status line along the bottom carrying the key hints,
//! the file name, and the cursor position — or, when one is up, the active
//! prompt or confirm question. The menu opens as a boxed drop-down under
//! its title; the F1 key summary is a centred overlay.

use alloc::format;
use alloc::string::String;

use rustos_curses::{str_width, truncate_to_width, Pos, Screen, Size, Tty, Window};
use rustos_vt::{char_width, Attributes, BasicColor, Color};

use crate::buffer::width_of_prefix;
use crate::error::EditError;
use crate::model::{Action, Fs, Mode, Model, Pending, PromptIntent, MENUS};

/// Rows reserved above the text area: the menu bar.
const MENU_ROWS: u16 = 1;
/// Rows reserved below the text area: the status line.
const STATUS_ROWS: u16 = 1;
/// Columns of padding before the first menu title, and between titles.
const MENU_GAP: usize = 2;

/// The text-area size for a screen of `size` (rows, cols).
#[must_use]
pub fn text_area(size: Size) -> (usize, usize) {
    (
        usize::from(size.rows.saturating_sub(MENU_ROWS + STATUS_ROWS)),
        usize::from(size.cols),
    )
}

/// Draw `model` onto `screen` and flush it (the curses two-step:
/// `wnoutrefresh` each window, then `doupdate`).
///
/// The whole screen is composed in one base [`Window`]; the drop-down menu
/// and the key-summary overlay are composited on top through the same
/// renderer — overlays stack by draw order.
///
/// # Errors
///
/// [`EditError::Terminal`] if the underlying tty write fails.
pub fn render<T: Tty>(model: &Model, screen: &mut Screen<T>) -> Result<(), EditError> {
    let size = screen.size();
    let cols = usize::from(size.cols);
    let (text_rows, _) = text_area(size);

    let bar_attrs = bar_attributes(screen);
    let text_attrs = text_attributes(screen);
    let selected_attrs = selection_attributes(screen);
    let accel_attrs = accelerator_attributes(screen);

    let mut win = Window::new(Pos::ORIGIN, size);

    draw_menu_bar(
        &mut win,
        model,
        cols,
        bar_attrs,
        selected_attrs,
        accel_attrs,
    );

    for offset in 0..text_rows {
        let row = MENU_ROWS + u16::try_from(offset).unwrap_or(0);
        let index = model.scroll_top() + offset;
        let line = if index < model.buffer().line_count() {
            window_slice(model.buffer().line(index), model.scroll_left(), cols)
        } else {
            String::new()
        };
        put_line(&mut win, row, cols, &line, text_attrs);
    }

    if size.rows >= MENU_ROWS + STATUS_ROWS {
        let status_row = size.rows - 1;
        put_line(&mut win, status_row, cols, &status_line(model), bar_attrs);
    }

    place_cursor(&mut win, model, size, text_rows);
    screen.set_cursor_visible(cursor_visible(model));
    screen.wnoutrefresh(&win);

    if let Mode::Menu { menu, item } = model.mode() {
        if let Some(dropdown) = menu_window(*menu, *item, size, bar_attrs, selected_attrs) {
            screen.wnoutrefresh(&dropdown);
        }
    }

    if model.help_visible() {
        if let Some(overlay) = help_window(size) {
            screen.wnoutrefresh(&overlay);
        }
    }

    screen.doupdate().map_err(EditError::from)
}

/// Run the interactive editor until the user exits.
///
/// The loop redraws, blocks for one event (the kernel parks the read until
/// a key arrives — never a poll loop), and dispatches it to the model,
/// which loads and saves through `fs` as the user asks.
///
/// # Errors
///
/// [`EditError::Terminal`] — a tty read or write failed.
pub fn run<T: Tty>(
    model: &mut Model,
    fs: &dyn Fs,
    screen: &mut Screen<T>,
) -> Result<(), EditError> {
    loop {
        let (rows, cols) = text_area(screen.size());
        model.set_viewport(rows, cols);
        render(model, screen)?;
        let Some(event) = screen.getch().map_err(EditError::from)? else {
            // Blocking input yields no event only at end-of-input; there
            // is no terminal left to edit on, so the session ends.
            return Ok(());
        };
        match model.handle_event(&event, fs) {
            Action::Quit => return Ok(()),
            Action::Continue => {}
        }
    }
}

/// Draw the menu bar, highlighting the open menu's title and each closed
/// title's accelerator letter (its first character — the `Alt-<letter>`
/// that opens it).
fn draw_menu_bar(
    win: &mut Window,
    model: &Model,
    cols: usize,
    bar: Attributes,
    sel: Attributes,
    accel: Attributes,
) {
    put_line(win, 0, cols, "", bar);
    let open = match model.mode() {
        Mode::Menu { menu, .. } => Some(*menu),
        _ => None,
    };
    let mut col = MENU_GAP;
    for (index, menu) in MENUS.iter().enumerate() {
        let title = format!(" {} ", menu.title);
        if win
            .move_to(Pos::new(0, u16::try_from(col).unwrap_or(u16::MAX)))
            .is_ok()
        {
            let visible = truncate_to_width(&title, cols.saturating_sub(col));
            if open == Some(index) {
                // The open menu's whole title carries the selection bar.
                win.set_attributes(sel);
                win.add_str(visible);
            } else {
                // " F" + "ile ": the accelerator letter stands out so the
                // `Alt-<letter>` shortcut is discoverable at a glance.
                let mut chars = visible.char_indices();
                let split = chars.nth(2).map_or(visible.len(), |(at, _)| at);
                let (head, tail) = visible.split_at(split);
                win.set_attributes(bar);
                if let Some(space) = head.get(..1) {
                    win.add_str(space);
                }
                win.set_attributes(accel);
                if let Some(letter) = head.get(1..) {
                    win.add_str(letter);
                }
                win.set_attributes(bar);
                win.add_str(tail);
            }
            win.set_attributes(Attributes::PLAIN);
        }
        col += str_width(&title) + MENU_GAP;
    }
}

/// The zero-based column a menu's title starts at on the bar.
fn menu_title_col(menu: usize) -> usize {
    let mut col = MENU_GAP;
    for entry in MENUS.iter().take(menu) {
        col += str_width(entry.title) + 2 + MENU_GAP;
    }
    col
}

/// Build the boxed drop-down for the open `menu`, or `None` when the
/// screen cannot hold it.
fn menu_window(
    menu: usize,
    selected: usize,
    size: Size,
    bar: Attributes,
    sel: Attributes,
) -> Option<Window> {
    let items = MENUS.get(menu)?.items;
    let content_cols = items.iter().map(|item| str_width(item)).max().unwrap_or(0) + 2;
    let box_cols = u16::try_from(content_cols + 2).ok()?;
    let box_rows = u16::try_from(items.len() + 2).ok()?;
    if box_rows + MENU_ROWS > size.rows || box_cols > size.cols {
        return None;
    }
    let origin_col = u16::try_from(menu_title_col(menu))
        .ok()?
        .min(size.cols - box_cols);
    let mut win = Window::new(
        Pos::new(MENU_ROWS, origin_col),
        Size::new(box_rows, box_cols),
    );
    win.set_attributes(bar);
    win.erase();
    win.draw_box();
    for (offset, item) in items.iter().enumerate() {
        let row = 1 + u16::try_from(offset).unwrap_or(0);
        if win.move_to(Pos::new(row, 1)).is_err() {
            continue;
        }
        win.set_attributes(if offset == selected { sel } else { bar });
        let label = format!(" {item:<width$} ", width = content_cols - 2);
        win.add_str(truncate_to_width(&label, content_cols));
    }
    win.set_attributes(Attributes::PLAIN);
    Some(win)
}

/// The status line's text for the current mode.
fn status_line(model: &Model) -> String {
    match model.mode() {
        Mode::Prompt { intent, input } => {
            let label = match intent {
                PromptIntent::Open => "Open file: ",
                PromptIntent::SaveAs => "Save as: ",
                PromptIntent::Find => "Find: ",
            };
            format!(" {label}{input}")
        }
        Mode::Confirm(pending) => {
            let verb = match pending {
                Pending::New => "starting a new file",
                Pending::Open => "opening another file",
                Pending::Exit => "exiting",
            };
            format!(
                " Save changes to {} before {verb}? (Y/N, C or Esc cancels)",
                display_name(model)
            )
        }
        Mode::Menu { .. } => String::from(" Arrows choose, Enter selects, Esc closes the menu"),
        Mode::Edit => {
            if let Some(notice) = model.notice() {
                return format!(" {notice}");
            }
            format!(
                " F1=Help  F10=Menu \u{2502} {}{} \u{2502} Ln {}, Col {}{}",
                display_name(model),
                if model.buffer().is_modified() {
                    "*"
                } else {
                    ""
                },
                model.cursor_row() + 1,
                model.cursor_col() + 1,
                if model.overwrite() {
                    " \u{2502} OVR"
                } else {
                    ""
                }
            )
        }
    }
}

/// The buffer's display name: its file name, or the conventional
/// placeholder before it has one.
fn display_name(model: &Model) -> &str {
    model.path().unwrap_or("(untitled)")
}

/// Leave the base window's cursor where the terminal cursor belongs: the
/// edit point in the text area, or the end of a prompt's input.
fn place_cursor(win: &mut Window, model: &Model, size: Size, text_rows: usize) {
    match model.mode() {
        Mode::Edit => {
            let row = model.cursor_row().saturating_sub(model.scroll_top());
            let col = width_of_prefix(model.buffer().line(model.cursor_row()), model.cursor_col())
                .saturating_sub(model.scroll_left());
            if row < text_rows {
                let row = MENU_ROWS + u16::try_from(row).unwrap_or(u16::MAX);
                let col = u16::try_from(col).unwrap_or(u16::MAX);
                let _ = win.move_to(Pos::new(
                    row.min(size.rows.saturating_sub(1)),
                    col.min(size.cols.saturating_sub(1)),
                ));
            }
        }
        Mode::Prompt { intent, input } => {
            let label = match intent {
                PromptIntent::Open => "Open file: ",
                PromptIntent::SaveAs => "Save as: ",
                PromptIntent::Find => "Find: ",
            };
            let col = 1 + str_width(label) + str_width(input);
            let _ = win.move_to(Pos::new(
                size.rows.saturating_sub(1),
                u16::try_from(col)
                    .unwrap_or(u16::MAX)
                    .min(size.cols.saturating_sub(1)),
            ));
        }
        _ => {}
    }
}

/// Whether the terminal cursor should be shown: only where there is a
/// meaningful insertion point (editing, or typing into a prompt).
fn cursor_visible(model: &Model) -> bool {
    !model.help_visible() && matches!(model.mode(), Mode::Edit | Mode::Prompt { .. })
}

/// The horizontal window of `line` from display column `left`, `cols`
/// wide: the visible slice under horizontal scrolling. A double-width
/// glyph straddling the left edge is shown as a pad space, and one that
/// would straddle the right edge is dropped, so the slice never tears a
/// glyph in half.
fn window_slice(line: &str, left: usize, cols: usize) -> String {
    let mut out = String::new();
    let mut col = 0usize;
    for ch in line.chars() {
        let width = usize::from(char_width(ch));
        if col + width <= left {
            col += width;
            continue;
        }
        if col < left {
            // A wide glyph straddles the left edge: pad its visible tail.
            for _ in left..col + width {
                out.push(' ');
            }
            col += width;
            continue;
        }
        if col + width > left + cols {
            break;
        }
        out.push(ch);
        col += width;
    }
    out
}

/// Write `text` at the start of `row`, padded with spaces to `cols`
/// columns (so the row's background spans the screen) and truncated to fit
/// without splitting a double-width glyph.
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

/// The menu-bar/status-line rendition: black on white when the terminal
/// has colour (the classic light bar), reverse video when it does not.
fn bar_attributes<T: Tty>(screen: &mut Screen<T>) -> Attributes {
    match screen.colored_attributes(
        Color::Basic(BasicColor::Black),
        Color::Basic(BasicColor::White),
    ) {
        Some(attrs) => attrs,
        None => reversed(),
    }
}

/// The text-area rendition: white on blue when the terminal has colour
/// (the classic editor blue), plain otherwise.
fn text_attributes<T: Tty>(screen: &mut Screen<T>) -> Attributes {
    screen
        .colored_attributes(
            Color::Basic(BasicColor::White),
            Color::Basic(BasicColor::Blue),
        )
        .unwrap_or(Attributes::PLAIN)
}

/// The rendition of the selected menu title/item: the inverse of the bar,
/// so the selection reads on colour and monochrome terminals alike.
fn selection_attributes<T: Tty>(screen: &mut Screen<T>) -> Attributes {
    match screen.colored_attributes(
        Color::Basic(BasicColor::White),
        Color::Basic(BasicColor::Black),
    ) {
        Some(attrs) => attrs,
        // The bar fell back to reverse video, so its selection falls back
        // to plain — still its inverse.
        None => Attributes::PLAIN,
    }
}

/// The rendition of a menu title's accelerator letter: the Borland-style
/// red on the white bar where the terminal has colour, bold reverse video
/// where it does not — either way distinct from the surrounding title.
fn accelerator_attributes<T: Tty>(screen: &mut Screen<T>) -> Attributes {
    screen
        .colored_attributes(
            Color::Basic(BasicColor::Red),
            Color::Basic(BasicColor::White),
        )
        .unwrap_or_else(|| {
            let mut attrs = reversed();
            attrs.bold = true;
            attrs
        })
}

/// Reverse-video attributes (the colourless bar fallback).
fn reversed() -> Attributes {
    let mut attrs = Attributes::PLAIN;
    attrs.reverse = true;
    attrs
}

/// The key-summary overlay text, one entry per line.
const HELP_LINES: &[&str] = &[
    " Keys ",
    "",
    " F10 / Alt-F  open the menu (File, Search)",
    " F2           save",
    " F3           repeat the last find",
    " arrows       move the cursor",
    " Home / End   start / end of line",
    " PgUp / PgDn  move a page",
    " Insert       toggle insert / overwrite",
    " F1           toggle this summary",
];

/// Build the centred key-summary overlay for a screen of `size`, or `None`
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
