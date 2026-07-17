//! Drawing the [`Model`] and running the input loop.
//!
//! [`render`] turns a model into curses windows; [`run`] is the thin
//! event-driven loop that ties the [`Screen`] driver, the model, and the
//! `sysinfo` transport together. Drawing is pure with respect to the model,
//! so a test renders to an in-memory [`Tty`] and inspects the bytes.
//!
//! The layout is a fixed summary block — the title line (uptime, load
//! averages, pin state), the memory overview, the pressure gauge with its
//! band-history strip, the aggregate CPU line, and the task census — above
//! a scrollable, `p`-cycled detail panel (reclaim ledger, `ramzip`
//! counters, per-CPU load, process top consumers) and a key-hint footer.
//! Every figure a refused query withheld renders as the refusal it is; the
//! session continues. The view redraws itself every refresh interval
//! without a key press: the input wait is bounded by the interval, and an
//! elapsed wait re-queries exactly as the refresh key does.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::fmt::Write as _;

use tairix_curses::{str_width, truncate_to_width, Pos, Screen, Size, Tty, Window};
use tairix_procinfo::{
    field_lossy, format_load, format_mib, format_size, format_tenths, format_uptime, state_char,
    Transport,
};
use tairix_vt::{Attributes, BasicColor, Color};

use crate::error::SysmonError;
use crate::model::{Action, Focus, Gauge, Model, PinState, Snapshot};

/// Rows reserved above the detail panel: title, memory, pressure, history
/// strip, CPU, tasks, and the detail panel's bold header.
const HEADER_ROWS: u16 = 7;
/// Rows reserved below the detail panel: the key-hint footer.
const FOOTER_ROWS: u16 = 1;

/// The stated reason a gated figure is missing for want of the kernel
/// observability capability.
const KERNEL_DENIED: &str = "unavailable (needs CAP_SYSINFO_KERNEL)";
/// The honest reason a figure is missing when the query failed.
const UNAVAILABLE: &str = "unavailable";

/// The number of detail rows that fit on a screen of `size`.
#[must_use]
pub fn detail_capacity(size: Size) -> usize {
    usize::from(size.rows.saturating_sub(HEADER_ROWS + FOOTER_ROWS))
}

/// The first line: tool name, uptime, load averages, and the pin state —
/// a refused pin is a standing, stated condition, never a silent blank.
pub(crate) fn title_line(model: &Model) -> String {
    let snapshot = model.snapshot();
    let mut line = String::from("sysmon - ");
    match snapshot.uptime_ns {
        Some(ns) => {
            line.push_str("up ");
            line.push_str(&format_uptime(ns));
        }
        None => line.push_str("up ?"),
    }
    if let Some(load) = &snapshot.load {
        // `write!` into the existing string cannot fail and avoids an
        // intermediate allocation; the fallback is a benign truncation of
        // an already-degraded line.
        let _ = write!(
            line,
            ", load average: {}, {}, {}",
            format_load(load.load1),
            format_load(load.load5),
            format_load(load.load15)
        );
    }
    match model.pin() {
        PinState::Pinned => line.push_str(" [pinned]"),
        PinState::Unpinned(reason) => {
            line.push_str(" [unpinned: ");
            line.push_str(reason);
            line.push(']');
        }
    }
    line
}

/// The memory overview line: the kernel memory figures in MiB plus the
/// pinned aggregate, or the stated reason they are absent.
pub(crate) fn memory_line(snapshot: &Snapshot) -> String {
    let Some(memory) = snapshot.memory.ready() else {
        return if snapshot.memory.is_denied() {
            format!("MiB Mem : {KERNEL_DENIED}")
        } else {
            format!("MiB Mem : {UNAVAILABLE}")
        };
    };
    let used = memory.total_bytes.saturating_sub(memory.free_bytes);
    let mut line = format!(
        "MiB Mem : {} total, {} free, {} used, {} kernel",
        format_mib(memory.total_bytes),
        format_mib(memory.free_bytes),
        format_mib(used),
        format_mib(memory.kernel_heap_bytes)
    );
    if let Some(ramzip) = snapshot.ramzip.ready() {
        let _ = write!(line, ", {} pinned", format_mib(ramzip.pinned_bytes));
    }
    line
}

/// One-character band glyphs for the history strip, indexed by band depth
/// (normal, mild, moderate, severe, critical).
const BAND_GLYPHS: [char; tairix_abi::sysinfo::PRESSURE_BAND_COUNT] = ['.', '-', '=', '#', '!'];

/// The pressure line: the current band name, a five-step depth gauge, the
/// reserve floor, and the entry counters — or the stated reason.
pub(crate) fn pressure_line(snapshot: &Snapshot) -> String {
    let Some(pressure) = snapshot.pressure.ready() else {
        return if snapshot.pressure.is_denied() {
            format!("Pressure: {KERNEL_DENIED}")
        } else {
            format!("Pressure: {UNAVAILABLE}")
        };
    };
    let band = usize::from(pressure.band);
    let name = tairix_abi::sysinfo::PRESSURE_BAND_NAMES
        .get(band)
        .copied()
        .unwrap_or("?");
    let mut gauge = String::new();
    for depth in 0..tairix_abi::sysinfo::PRESSURE_BAND_COUNT {
        gauge.push(if depth <= band { '#' } else { '-' });
    }
    let entries: u64 = pressure.band_entries.iter().sum();
    format!(
        "Pressure: {name} [{gauge}]  free {} MiB, reserve {} MiB, {entries} band entries",
        format_mib(pressure.free_bytes),
        format_mib(pressure.reserve_bytes),
    )
}

/// The band-history strip: one glyph per refresh, oldest leftmost. Empty
/// history renders its absence honestly.
pub(crate) fn history_line(model: &Model, cols: usize) -> String {
    let mut line = String::from("history : ");
    let history = model.band_history();
    if history.is_empty() {
        line.push_str("(no samples yet)");
        return line;
    }
    let room = cols.saturating_sub(str_width(&line)).max(1);
    let start = history.len().saturating_sub(room);
    for &band in &history[start..] {
        line.push(BAND_GLYPHS.get(usize::from(band)).copied().unwrap_or('?'));
    }
    line
}

/// The aggregate CPU line: CPU count, all-CPU busy share over the last
/// interval, and the summed switch/preemption counters.
pub(crate) fn cpu_line(model: &Model) -> String {
    let busy = model.cpu_busy();
    let mut line = String::from("CPU     : ");
    if busy.is_empty() {
        line.push_str(UNAVAILABLE);
    } else {
        let total: u64 = busy.iter().map(|b| u64::from(b.busy_tenths)).sum();
        let avg = total / busy.len() as u64;
        let _ = write!(
            line,
            "{} cpus, {} busy",
            busy.len(),
            format_tenths(u32::try_from(avg).unwrap_or(1000))
        );
    }
    match &model.snapshot().cpu_loads {
        Gauge::Ready(loads) => {
            let switches: u64 = loads.iter().map(|l| l.switches).sum();
            let preemptions: u64 = loads.iter().map(|l| l.preemptions).sum();
            let _ = write!(line, ", {switches} switches, {preemptions} preemptions");
        }
        Gauge::Denied => {
            let _ = write!(line, ", counters {KERNEL_DENIED}");
        }
        Gauge::Unavailable => {}
    }
    line
}

/// The task-census line, GNU-style, with the recorded scope: the
/// system-wide census, or the caller's own with the refusal stated.
pub(crate) fn tasks_line(snapshot: &Snapshot) -> String {
    use tairix_abi::sysinfo::ProcessState;
    let mut running = 0usize;
    let mut sleeping = 0usize;
    let mut stopped = 0usize;
    let mut zombie = 0usize;
    for record in &snapshot.processes {
        match record.state {
            ProcessState::Running | ProcessState::Runnable => running += 1,
            ProcessState::Blocked => sleeping += 1,
            ProcessState::Stopped => stopped += 1,
            ProcessState::Zombie => zombie += 1,
        }
    }
    let mut line = format!(
        "Tasks   : {} total, {running} running, {sleeping} sleeping, {stopped} stopped, {zombie} zombie",
        snapshot.processes.len()
    );
    if snapshot.global_denied {
        // The short marker fits the 80-column line whatever the counts;
        // the full stated refusal leads the processes panel.
        line.push_str(" (own)");
    }
    line
}

/// The focused detail panel's lines, unscrolled. Pure with respect to the
/// model, so tests assert panel content without a terminal.
pub(crate) fn detail_lines(model: &Model) -> Vec<String> {
    match model.focus() {
        Focus::Reclaim => reclaim_lines(model.snapshot()),
        Focus::Ramzip => ramzip_lines(model.snapshot()),
        Focus::Cpu => cpu_lines(model),
        Focus::Processes => process_lines(model),
    }
}

/// One stated-reason line for a gauge the panel cannot show.
fn denied_lines<T>(gauge: &Gauge<T>) -> Vec<String> {
    let mut lines = Vec::new();
    lines.push(String::from(if gauge.is_denied() {
        KERNEL_DENIED
    } else {
        UNAVAILABLE
    }));
    lines
}

/// The reclaim-ledger table: one row per class.
fn reclaim_lines(snapshot: &Snapshot) -> Vec<String> {
    let Some(records) = snapshot.reclaim.ready() else {
        return denied_lines(&snapshot.reclaim);
    };
    let mut lines = Vec::new();
    lines.push(format!(
        "{:<22} {:>8} {:>8} {:>8} {:>8} {:>8} {:>8}",
        "class", "payload", "meta", "entries", "shrinks", "refused", "fails"
    ));
    for record in records {
        let name = tairix_abi::sysinfo::RECLAIM_CLASS_NAMES
            .get(usize::from(record.class))
            .copied()
            .unwrap_or("?");
        lines.push(format!(
            "{:<22} {:>8} {:>8} {:>8} {:>8} {:>8} {:>8}",
            name,
            format_size(record.payload_bytes),
            format_size(record.metadata_bytes),
            record.entries,
            record.pressure_shrinks,
            record.refusals,
            record.failures
        ));
    }
    lines
}

/// The `ramzip` counter panel: bytes, caps, and the outcome counters.
fn ramzip_lines(snapshot: &Snapshot) -> Vec<String> {
    let Some(stats) = snapshot.ramzip.ready() else {
        return denied_lines(&snapshot.ramzip);
    };
    let saved = stats.logical_bytes.saturating_sub(stats.stored_bytes);
    let mut lines = Vec::new();
    lines.push(format!(
        "stored {}  logical {}  saved {}  metadata {}  entries {}",
        format_size(stats.stored_bytes),
        format_size(stats.logical_bytes),
        format_size(saved),
        format_size(stats.metadata_bytes),
        stats.entries
    ));
    lines.push(format!(
        "caps   min {}  soft {}  hard {}  pinned {}",
        format_size(stats.min_cap_bytes),
        format_size(stats.soft_cap_bytes),
        format_size(stats.hard_cap_bytes),
        format_size(stats.pinned_bytes)
    ));
    lines.push(format!(
        "compress  attempts {}  accepted {}  incompressible {}  policy {}  cap {}",
        stats.attempts,
        stats.accepted,
        stats.rejected_incompressible,
        stats.rejected_policy,
        stats.rejected_cap
    ));
    lines.push(format!(
        "rejected  ineligible {}  reserve {}  task-share {}  thrash {}",
        stats.rejected_ineligible,
        stats.rejected_reserve,
        stats.rejected_task_share,
        stats.rejected_thrash
    ));
    lines.push(format!(
        "restore   fault-ins {}  warm {}  clustered {}  auth-fail {}  decode-fail {}",
        stats.fault_ins,
        stats.warm_restored,
        stats.cluster_restored,
        stats.auth_failures,
        stats.decode_failures
    ));
    lines.push(format!(
        "warm-up   attempts {}  stopped {}  thrash-detected {}",
        stats.warm_attempts, stats.warm_stopped, stats.thrash_detected
    ));
    lines
}

/// The per-CPU table: busy share over the interval, queue depth, and the
/// switch/preemption counters.
fn cpu_lines(model: &Model) -> Vec<String> {
    let mut lines = Vec::new();
    lines.push(format!(
        "{:>4} {:>6} {:>7} {:>12} {:>12}",
        "cpu", "busy%", "queue", "switches", "preemptions"
    ));
    let busy = model.cpu_busy();
    let loads = model.snapshot().cpu_loads.ready();
    if busy.is_empty() && loads.is_none() {
        lines.extend(denied_lines(&model.snapshot().cpu_loads));
        return lines;
    }
    for entry in busy {
        let (queue, switches, preemptions) = loads
            .and_then(|records| records.iter().find(|r| r.cpu == entry.cpu))
            .map_or((String::from("?"), String::from("?"), String::from("?")), {
                |r| {
                    (
                        format!("{}", r.queue_depth),
                        format!("{}", r.switches),
                        format!("{}", r.preemptions),
                    )
                }
            });
        lines.push(format!(
            "{:>4} {:>6} {:>7} {:>12} {:>12}",
            entry.cpu,
            format_tenths(entry.busy_tenths),
            queue,
            switches,
            preemptions
        ));
    }
    lines
}

/// How many top consumers each process list shows.
const TOP_CONSUMERS: usize = 5;

/// The process panel: the top consumers by `%CPU` and by resident bytes.
/// The full interactive list remains `top`'s job; this is the census
/// summary only.
fn process_lines(model: &Model) -> Vec<String> {
    let snapshot = model.snapshot();
    let mut lines = Vec::new();
    if snapshot.global_denied {
        lines.push(String::from(
            "own processes only (all-process census needs CAP_SYSINFO_GLOBAL)",
        ));
    }
    if snapshot.processes.is_empty() {
        lines.push(String::from(UNAVAILABLE));
        return lines;
    }
    lines.push(format!(
        "top by %cpu    {:>7} {:>6} {:>6} {}",
        "pid", "%cpu", "size", "command"
    ));
    let mut by_cpu: Vec<_> = snapshot.processes.iter().collect();
    by_cpu.sort_by(|a, b| {
        let pa = model.proc_pct(a.proc_id).unwrap_or(0);
        let pb = model.proc_pct(b.proc_id).unwrap_or(0);
        pb.cmp(&pa)
            .then(b.cpu_time_ns.cmp(&a.cpu_time_ns))
            .then(a.pid.cmp(&b.pid))
    });
    for record in by_cpu.iter().take(TOP_CONSUMERS) {
        lines.push(format!(
            "               {:>7} {:>6} {:>6} {}",
            record.pid,
            format_tenths(model.proc_pct(record.proc_id).unwrap_or(0)),
            format_size(record.mem_bytes),
            field_lossy(record.name_bytes())
        ));
    }
    lines.push(format!(
        "top by memory  {:>7} {:>6} {:>6} {}",
        "pid", "state", "size", "command"
    ));
    let mut by_mem: Vec<_> = snapshot.processes.iter().collect();
    by_mem.sort_by(|a, b| b.mem_bytes.cmp(&a.mem_bytes).then(a.pid.cmp(&b.pid)));
    for record in by_mem.iter().take(TOP_CONSUMERS) {
        lines.push(format!(
            "               {:>7} {:>6} {:>6} {}",
            record.pid,
            state_char(record.state),
            format_size(record.mem_bytes),
            field_lossy(record.name_bytes())
        ));
    }
    lines
}

/// The detail-panel header: the focused panel's name and its place in the
/// `p` cycle, plus a scroll cue when the panel overflows the viewport.
fn panel_header(model: &Model, lines: usize, capacity: usize) -> String {
    let mut header = format!("── {} ", model.focus().label());
    if lines > capacity {
        let _ = write!(
            header,
            "({}-{} of {lines}) ",
            model.scroll() + 1,
            (model.scroll() + capacity).min(lines)
        );
    }
    header.push_str("── p next panel");
    header
}

/// The footer key hints.
const FOOTER_HINT: &str = " q quit  p panel  r refresh  +/- interval  up/down scroll  ? help ";

/// Draw `model` onto `screen` and flush it (the curses two-step:
/// `wnoutrefresh` each window, then `doupdate`).
///
/// The whole screen is composed in one base [`Window`]; when the help
/// overlay is showing, a second window is composited on top through the
/// same renderer — overlays stack by draw order, no separate panel
/// machinery needed.
///
/// # Errors
///
/// [`SysmonError::Terminal`] if the underlying tty write fails.
pub fn render<T: Tty>(model: &mut Model, screen: &mut Screen<T>) -> Result<(), SysmonError> {
    let size = screen.size();
    let cols = usize::from(size.cols);

    let header_attrs = header_attributes(screen);
    let plain = Attributes::PLAIN;

    let capacity = detail_capacity(size);
    model.set_viewport(capacity);
    let lines = detail_lines(model);
    model.clamp_scroll(lines.len());

    let mut win = Window::new(Pos::ORIGIN, size);
    put_line(&mut win, 0, cols, &title_line(model), header_attrs);
    let snapshot = model.snapshot();
    if size.rows > 1 {
        put_line(&mut win, 1, cols, &memory_line(snapshot), plain);
    }
    if size.rows > 2 {
        put_line(&mut win, 2, cols, &pressure_line(snapshot), plain);
    }
    if size.rows > 3 {
        put_line(&mut win, 3, cols, &history_line(model, cols), plain);
    }
    if size.rows > 4 {
        put_line(&mut win, 4, cols, &cpu_line(model), plain);
    }
    if size.rows > 5 {
        put_line(&mut win, 5, cols, &tasks_line(snapshot), plain);
    }
    if size.rows > 6 {
        put_line(
            &mut win,
            6,
            cols,
            &panel_header(model, lines.len(), capacity),
            bold(),
        );
    }

    for offset in 0..capacity {
        let row = HEADER_ROWS + u16::try_from(offset).unwrap_or(0);
        let Some(line) = lines.get(model.scroll() + offset) else {
            // Blank the leftover rows so a shrunken panel leaves no stale
            // text behind.
            put_line(&mut win, row, cols, "", plain);
            continue;
        };
        put_line(&mut win, row, cols, line, plain);
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

    screen.doupdate().map_err(SysmonError::from)
}

/// Run the interactive monitor until the user quits.
///
/// The loop refreshes the snapshot once up front, then redraws and waits
/// for one event at a time. The wait is bounded by the current refresh
/// interval (the `-d` option, adjusted in session by `+`/`-`): an elapsed
/// wait auto-refreshes exactly as the refresh key does, so the display
/// stays live without a key press, and the kernel parks the read for the
/// interval — never a poll loop. The cursor is hidden for the duration and
/// restored on exit.
///
/// # Errors
///
/// [`SysmonError::Terminal`] — a tty read or write failed. Query refusals
/// and failures never end the session; they render as their panels'
/// stated reasons.
pub fn run<T: Transport, Y: Tty>(
    model: &mut Model,
    transport: &T,
    screen: &mut Screen<Y>,
) -> Result<(), SysmonError> {
    screen.set_cursor_visible(false);
    model.refresh(transport);
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
) -> Result<(), SysmonError> {
    loop {
        // Re-arm the input wait from the model each pass, so the `+`/`-`
        // keys take effect on the very next wait.
        screen.set_input_mode(tairix_curses::InputMode::Timeout(
            core::time::Duration::from_millis(u64::from(model.delay_tenths()) * 100),
        ));
        render(model, screen)?;
        let Some(event) = screen.getch().map_err(SysmonError::from)? else {
            // No event inside the refresh interval: the auto-refresh tick.
            // The kernel parks the read until input arrives or the interval
            // elapses, so this loop never spins.
            model.refresh(transport);
            continue;
        };
        match model.handle_event(&event) {
            Action::Quit => return Ok(()),
            Action::Refresh => model.refresh(transport),
            Action::Redraw | Action::Ignore => {}
        }
    }
}

/// Write `text` at the start of `row`, padded with spaces to `cols`
/// columns (so a highlight spans the row) and truncated to fit without
/// splitting a double-width glyph.
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

/// The header/footer rendition: white on blue when the terminal has
/// colour, falling back to reverse video when it does not.
fn header_attributes<T: Tty>(screen: &mut Screen<T>) -> Attributes {
    match screen.colored_attributes(
        Color::Basic(BasicColor::White),
        Color::Basic(BasicColor::Blue),
    ) {
        Some(attrs) => attrs,
        None => reversed(),
    }
}

/// Reverse-video attributes (the colourless header fallback).
fn reversed() -> Attributes {
    let mut attrs = Attributes::PLAIN;
    attrs.reverse = true;
    attrs
}

/// Bold attributes (the detail-panel header).
fn bold() -> Attributes {
    let mut attrs = Attributes::PLAIN;
    attrs.bold = true;
    attrs
}

/// The help-overlay text, one entry per line.
const HELP_LINES: &[&str] = &[
    " Keys ",
    "",
    " p            next detail panel",
    " up / down    scroll the panel",
    " PgUp / PgDn  scroll a page",
    " Home / End   first / last",
    " + / -        lengthen / shorten the interval",
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
