//! The curses frame: tree pane, file pane, status line, message line, and
//! the sort-menu and help overlays.
//!
//! Layout, left to right: the tree pane, a one-column divider, and the file
//! pane; the last two rows are the reverse-video status line and the
//! message line. Pure drawing over the [`Model`] — no I/O, so golden-grid
//! tests drive it directly.

use alloc::format;
use alloc::string::String;

use rustos_abi::time::Time64;
use rustos_curses::{truncate_to_width, Pos, Window};
use rustos_vt::Attributes;

use crate::model::{Model, Overlay, Pane, SortKey};

/// Columns given to the tree pane for a grid `cols` wide.
fn tree_width(cols: u16) -> u16 {
    (cols / 3).clamp(12, 40).min(cols.saturating_sub(2))
}

/// Draw one full frame of `model` into `window`.
pub fn render(model: &Model, window: &mut Window) {
    let size = window.size();
    let rows = size.rows;
    let cols = size.cols;
    window.erase();
    if rows < 3 || cols < 8 {
        return;
    }
    let body = rows - 2;
    if model.overlay == Overlay::Help {
        render_help(model, window, body, cols);
    } else {
        render_panes(model, window, body, cols);
    }
    render_status(model, window, rows, cols);
    render_message(model, window, rows, cols);
}

/// The two panes and their divider.
fn render_panes(model: &Model, window: &mut Window, body: u16, cols: u16) {
    let plain = Attributes::PLAIN;
    let mut reverse = Attributes::PLAIN;
    reverse.reverse = true;
    let mut dim = Attributes::PLAIN;
    dim.dim = true;

    let tw = tree_width(cols);
    for row in 0..body {
        let _ = window.move_add_str(Pos::new(row, tw), "|");
    }

    let tree_rows = model.tree_rows();
    let visible_tree = tree_rows.iter().enumerate().skip(model.tree_scroll);
    for (line, (index, row)) in (0..body).zip(visible_tree) {
        let marker = if row.expanded { "- " } else { "+ " };
        let text = format!("{:indent$}{marker}{}", "", row.name, indent = row.depth * 2);
        let focused = index == model.tree_cursor;
        let attrs = match (focused, model.pane) {
            (true, Pane::Tree) => reverse,
            (true, Pane::Files) => dim,
            _ => plain,
        };
        window.set_attributes(attrs);
        let _ = window.move_add_str(Pos::new(line, 0), truncate_to_width(&text, usize::from(tw)));
        window.set_attributes(plain);
    }

    let files = model.visible_files();
    let fx = tw + 1;
    let fw = usize::from(cols - fx);
    let visible_files = files.iter().enumerate().skip(model.file_scroll);
    for (line, (index, entry)) in (0..body).zip(visible_files) {
        let text = file_row(
            entry.kind.is_dir(),
            &entry.name,
            entry.size,
            entry.modified,
            fw,
        );
        let focused = index == model.file_cursor;
        let attrs = match (focused, model.pane) {
            (true, Pane::Files) => reverse,
            (true, Pane::Tree) => dim,
            _ => plain,
        };
        window.set_attributes(attrs);
        let _ = window.move_add_str(Pos::new(line, fx), truncate_to_width(&text, fw));
        window.set_attributes(plain);
    }
}

/// One file-pane row: name left, size and stamp right-aligned.
fn file_row(is_dir: bool, name: &str, size: u64, modified: Time64, width: usize) -> String {
    let stamp = format_stamp(modified);
    let size_text = if is_dir {
        String::from("<dir>")
    } else {
        format!("{size}")
    };
    let tail = format!("{size_text:>12} {stamp:>16}");
    let name_width = width.saturating_sub(tail.len() + 1);
    let shown = truncate_to_width(name, name_width);
    format!("{shown:<name_width$} {tail}")
}

/// The reverse-video status line: path, entry count, sort order, free
/// space, and the hidden toggle.
fn render_status(model: &Model, window: &mut Window, rows: u16, cols: u16) {
    let mut reverse = Attributes::PLAIN;
    reverse.reverse = true;
    let key = match model.sort_key {
        SortKey::Name => "name",
        SortKey::Extension => "ext",
        SortKey::Size => "size",
        SortKey::Modified => "mtime",
    };
    let direction = if model.sort_desc { "desc" } else { "asc" };
    let hidden = if model.show_hidden { "  hidden" } else { "" };
    let space = match model.space {
        Some(space) => format!("  free {}/{}", space.free_bytes, space.total_bytes),
        None => String::new(),
    };
    let status = format!(
        "{}  {} entries  sort {key} {direction}{space}{hidden}",
        model.files_dir,
        model.visible_files().len(),
    );
    let mut padded = String::from(truncate_to_width(&status, usize::from(cols)));
    while padded.len() < usize::from(cols) {
        padded.push(' ');
    }
    window.set_attributes(reverse);
    let _ = window.move_add_str(Pos::new(rows - 2, 0), &padded);
    window.set_attributes(Attributes::PLAIN);
}

/// The message line: the mode-editor prompt, an error/notice, the
/// sort-menu prompt, or key hints.
fn render_message(model: &Model, window: &mut Window, rows: u16, cols: u16) {
    let text = if let Some(prompt) = &model.prompt {
        // The trailing underscore is the input point; the bracketed figure
        // is the entry's current mode for reference.
        format!(
            "mode {} [{:o}]: {}_  (octal, Enter applies, Esc cancels)",
            prompt.name, prompt.current, prompt.input
        )
    } else if model.overlay == Overlay::SortMenu {
        String::from("sort: n)ame  e)xtension  s)ize  m)odified  r)everse  Esc cancels")
    } else if let Some(message) = &model.message {
        message.clone()
    } else {
        String::from(
            "arrows/hjkl move  Enter open  Tab pane  s sort  a mode  . hidden  ? help  q quit",
        )
    };
    let _ = window.move_add_str(
        Pos::new(rows - 1, 0),
        truncate_to_width(&text, usize::from(cols)),
    );
}

/// The help overlay: the bundle's own Help document, plainly paged over
/// the body area (never embedded text — the string arrives through the
/// help seam).
fn render_help(model: &Model, window: &mut Window, body: u16, cols: u16) {
    for (line, text) in (0..body).zip(model.help_text.lines()) {
        let _ = window.move_add_str(
            Pos::new(line, 0),
            truncate_to_width(text, usize::from(cols)),
        );
    }
}

/// A stamp rendered as `YYYY-MM-DD HH:MM`, or `-` for the epoch (the
/// documented "no stamp" value — an absent figure, never a fabricated
/// 1970 date).
fn format_stamp(stamp: Time64) -> String {
    if stamp == Time64::UNIX_EPOCH {
        return String::from("-");
    }
    let days = stamp.secs().div_euclid(86_400);
    let tod = stamp.secs().rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    let hour = tod / 3_600;
    let minute = (tod % 3_600) / 60;
    format!("{year:04}-{month:02}-{day:02} {hour:02}:{minute:02}")
}

/// Proleptic-Gregorian date of an epoch day count (Howard Hinnant's
/// `civil_from_days` algorithm), exact over the whole `Time64` range.
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if month <= 2 { y + 1 } else { y };
    (year, month, day)
}
