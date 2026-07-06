//! Drawing the [`Model`] and running the input loop.
//!
//! [`render`] turns a model into curses windows; [`run`] is the thin
//! event-driven loop that ties the [`Screen`] driver, the model, and the
//! `sysinfo` transport together. Drawing is pure with respect to the model,
//! so a test renders to an in-memory [`Tty`] and inspects the bytes.
//!
//! The layout follows GNU `top`: three summary lines (uptime + load
//! average, the task census, and the memory figures), a bold column
//! header, the process rows sorted by `%CPU`, and a key-hint footer. The
//! single-letter state column is coloured by state on a colour terminal so
//! a runnable, sleeping, stopped, or zombie process reads at a glance.

use alloc::format;
use alloc::string::String;
use core::fmt::Write as _;

use rustos_abi::sysinfo::LoadAverage;
use rustos_curses::{str_width, truncate_to_width, Pos, Screen, Size, Tty, Window};
use rustos_procinfo::{state_char, Transport};
use rustos_vt::{Attributes, BasicColor, Color};

use crate::error::TopError;
use crate::model::{Action, Model, Row, Summary};

/// Rows reserved above the process list: the three summary lines and the
/// column header.
const HEADER_ROWS: u16 = 4;
/// Rows reserved below the process list: the key-hint footer.
const FOOTER_ROWS: u16 = 1;

/// The column header, matching [`process_row`]'s layout.
const COLUMN_HEADER: &str = "    PID USER       SIZE S  %CPU  WCPU      TIME+ COMMAND";

/// Zero-based column of the single-letter state cell inside a process row:
/// `PID` (7) + space + `USER` (8) + space + `SIZE` (6) + space.
const STATE_COL: u16 = 24;

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
/// needed.
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

    put_line(&mut win, 0, cols, &title_line(model), header_attrs);
    if size.rows > 1 {
        put_line(&mut win, 1, cols, &tasks_line(model), plain);
    }
    if size.rows > 2 {
        put_line(&mut win, 2, cols, &memory_line(model.summary()), plain);
    }
    if size.rows > 3 {
        put_line(&mut win, 3, cols, COLUMN_HEADER, bold());
    }

    let capacity = list_capacity(size);
    let scroll_top = model.scroll_top();
    for offset in 0..capacity {
        let row = HEADER_ROWS + u16::try_from(offset).unwrap_or(0);
        let index = scroll_top + offset;
        let Some(entry) = model.rows().get(index) else {
            break;
        };
        let line = process_row(model, entry);
        let selected = model.selected() == Some(index);
        let attrs = if selected { selected_attrs } else { plain };
        put_line(&mut win, row, cols, &line, attrs);
        // Recolour the state letter by state so it reads at a glance — but
        // never inside the selection highlight, whose reverse video must
        // span the row unbroken.
        if !selected && usize::from(STATE_COL) < cols {
            let state = state_char(entry.record.state);
            if let Some(attrs) = state_attributes(screen, state) {
                if win.move_to(Pos::new(row, STATE_COL)).is_ok() {
                    win.set_attributes(attrs);
                    win.add_char(state);
                    win.set_attributes(Attributes::PLAIN);
                }
            }
        }
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
            // The recovering refresh absorbs a refused system-wide view
            // (falling back to the caller's own processes with a status
            // notice) — a capability the user does not hold must never
            // kill the whole session; only a real service or terminal
            // failure may end it.
            Action::Refresh => model.refresh_recovering(transport)?,
            Action::Redraw | Action::Ignore => {}
        }
    }
}

/// The footer key hints.
const FOOTER_HINT: &str = " q quit  a all/own  r refresh  up/down scroll  ? help ";

/// The first summary line: uptime, logged-in users, load averages, the
/// active scope, and — when a refused action was recovered from — the
/// reason it did not take effect.
fn title_line(model: &Model) -> String {
    let summary = model.summary();
    let mut line = String::from("top - ");
    match summary.uptime_ns {
        Some(ns) => {
            line.push_str("up ");
            line.push_str(&format_uptime(ns));
        }
        None => line.push_str("up ?"),
    }
    if let Some(load) = &summary.load {
        line.push_str(", ");
        push_usize(&mut line, load.users as usize);
        line.push_str(if load.users == 1 { " user" } else { " users" });
        line.push_str(", load average: ");
        // `write!` into the existing string cannot fail and avoids an
        // intermediate allocation; the fallback is a benign truncation of
        // an already-degraded line.
        let _ = write!(
            line,
            "{}, {}, {}",
            format_load(load.load1),
            format_load(load.load5),
            format_load(load.load15)
        );
    }
    line.push_str(" [");
    line.push_str(model.scope().label());
    line.push(']');
    if let Some(notice) = model.notice() {
        line.push_str(" — ");
        line.push_str(notice);
    }
    line
}

/// The second summary line: the task census by state, GNU-style. `running`
/// counts both the running and runnable (waiting for a CPU) states, as GNU
/// `top`'s `R` bucket does.
fn tasks_line(model: &Model) -> String {
    use rustos_abi::sysinfo::ProcessState;
    let mut running = 0usize;
    let mut sleeping = 0usize;
    let mut stopped = 0usize;
    let mut zombie = 0usize;
    for row in model.rows() {
        match row.record.state {
            ProcessState::Running | ProcessState::Runnable => running += 1,
            ProcessState::Blocked => sleeping += 1,
            ProcessState::Stopped => stopped += 1,
            ProcessState::Zombie => zombie += 1,
        }
    }
    format!(
        "Tasks: {} total, {} running, {} sleeping, {} stopped, {} zombie",
        model.rows().len(),
        running,
        sleeping,
        stopped,
        zombie
    )
}

/// The third summary line: the memory figures in MiB, or the honest reason
/// they are absent. A refusal names the missing capability — the query is
/// optional, so the session continues, but the refusal is reported, never
/// silently blanked.
fn memory_line(summary: &Summary) -> String {
    if let Some(memory) = &summary.memory {
        let used = memory.total_bytes.saturating_sub(memory.free_bytes);
        return format!(
            "MiB Mem : {} total, {} free, {} used, {} kernel",
            format_mib(memory.total_bytes),
            format_mib(memory.free_bytes),
            format_mib(used),
            format_mib(memory.kernel_heap_bytes)
        );
    }
    if summary.memory_denied {
        String::from("MiB Mem : unavailable (needs CAP_SYSINFO_KERNEL)")
    } else {
        String::from("MiB Mem : unavailable")
    }
}

/// Render one process row against [`COLUMN_HEADER`].
pub(crate) fn process_row(model: &Model, row: &Row) -> String {
    let record = &row.record;
    let user = if let Some(name) = model.user_name(record.uid) {
        shorten(name, 8)
    } else {
        let mut uid = String::new();
        push_usize(&mut uid, record.uid as usize);
        shorten(&uid, 8)
    };
    format!(
        "{:>7} {:<8} {:>6} {} {:>5} {:>5} {:>10} {}",
        record.pid,
        user,
        format_size(record.mem_bytes),
        state_char(record.state),
        format_tenths(row.pct_cpu_tenths),
        format_tenths(row.wcpu_tenths),
        format_time_plus(record.cpu_time_ns),
        rustos_procinfo::field_lossy(record.name_bytes()),
    )
}

/// Truncate `text` to `max` columns, marking a truncation with a trailing
/// `+` exactly as GNU `top` marks an over-wide user name.
fn shorten(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return String::from(text);
    }
    let mut out: String = text.chars().take(max.saturating_sub(1)).collect();
    out.push('+');
    out
}

/// Render a tenths-of-a-percent figure as `W.T`, saturating at `999.9` so
/// the column never widens.
pub(crate) fn format_tenths(tenths: u32) -> String {
    let tenths = tenths.min(9_999);
    format!("{}.{}", tenths / 10, tenths % 10)
}

/// Render a byte count for the `SIZE` column: whole KiB below ten MiB
/// (`8432K`), tenths of MiB below ten GiB (`123.4M`), tenths of GiB above
/// (`12.3G`). Zero (no registered address space) renders as `0`.
pub(crate) fn format_size(bytes: u64) -> String {
    const KIB: u64 = 1024;
    const MIB: u64 = 1024 * KIB;
    const GIB: u64 = 1024 * MIB;
    if bytes == 0 {
        return String::from("0");
    }
    if bytes < 10 * MIB {
        return format!("{}K", bytes.div_ceil(KIB));
    }
    if bytes < 10 * GIB {
        let tenths = bytes * 10 / MIB;
        return format!("{}.{}M", tenths / 10, tenths % 10);
    }
    let tenths = bytes * 10 / GIB;
    format!("{}.{}G", tenths / 10, tenths % 10)
}

/// Render a byte count as tenths of MiB (`986.2`), the unit of the GNU
/// `top` memory summary line.
fn format_mib(bytes: u64) -> String {
    let tenths = bytes * 10 / (1024 * 1024);
    format!("{}.{}", tenths / 10, tenths % 10)
}

/// Render cumulative CPU time as GNU `top`'s `TIME+` form:
/// `minutes:seconds.hundredths`.
pub(crate) fn format_time_plus(ns: u64) -> String {
    let centis = ns / 10_000_000;
    let seconds = centis / 100;
    format!("{}:{:02}.{:02}", seconds / 60, seconds % 60, centis % 100)
}

/// Render a monotonic-nanosecond uptime as `D days, H:MM` / `H:MM`.
pub(crate) fn format_uptime(ns: u64) -> String {
    let minutes = ns / 60_000_000_000;
    let hours = minutes / 60;
    let days = hours / 24;
    if days > 0 {
        format!(
            "{} day{}, {}:{:02}",
            days,
            if days == 1 { "" } else { "s" },
            hours % 24,
            minutes % 60
        )
    } else {
        format!("{}:{:02}", hours, minutes % 60)
    }
}

/// Render a fixed-point load-average value as the conventional `W.CC`.
fn format_load(fixed: u32) -> String {
    format!(
        "{}.{:02}",
        LoadAverage::whole(fixed),
        LoadAverage::centis(fixed)
    )
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
    match colored(
        screen,
        Color::Basic(BasicColor::White),
        Color::Basic(BasicColor::Blue),
    ) {
        Some(attrs) => attrs,
        None => reversed(),
    }
}

/// The rendition of the single-letter state cell for `state`, or `None`
/// when the terminal is monochrome (the plain letter already carries the
/// information; colour is reinforcement, never the sole channel).
///
/// The palette follows the conventional traffic-light reading: running
/// green, runnable (waiting for a CPU) cyan, stopped yellow, zombie
/// magenta; a sleeping task stays plain, as almost every task sleeps.
pub(crate) fn state_attributes<T: Tty>(screen: &mut Screen<T>, state: char) -> Option<Attributes> {
    let (fg, emphasised) = match state {
        'R' => (BasicColor::Green, true),
        'r' => (BasicColor::Cyan, false),
        'T' => (BasicColor::Yellow, false),
        'Z' => (BasicColor::Magenta, false),
        _ => return None,
    };
    let mut attrs = colored(screen, Color::Basic(fg), Color::Default)?;
    attrs.bold = emphasised;
    Some(attrs)
}

/// Attributes for `fg` on `bg`, or `None` when the terminal cannot show
/// them (no colour support, or the pair table is exhausted — either way the
/// caller falls back rather than mis-colouring).
fn colored<T: Tty>(screen: &mut Screen<T>, fg: Color, bg: Color) -> Option<Attributes> {
    if !screen.capabilities().color.supports(fg) {
        return None;
    }
    let pair = screen.alloc_pair(fg, bg).ok()?;
    let colors = screen.color_pairs().get(pair);
    let mut attrs = Attributes::PLAIN;
    attrs.foreground = colors.fg;
    attrs.background = colors.bg;
    Some(attrs)
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
