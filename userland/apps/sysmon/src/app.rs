//! Drawing the [`Model`] and running the input loop.
//!
//! [`render`] turns a model into curses windows; [`run`] is the thin
//! event-driven loop that ties the [`Screen`] driver, the model, and the
//! `sysinfo` transport together. Drawing is pure with respect to the model,
//! so a test renders to an in-memory [`Tty`] and inspects the bytes.
//!
//! # Layout
//!
//! The screen is a fixed summary block above a scrollable detail panel and a
//! key-hint footer:
//!
//! * a full-width **title bar** (uptime, load averages, pin state);
//! * three colour-coded **bar gauges** — memory used, the pressure band, and
//!   aggregate CPU busy — each filled proportionally and coloured by
//!   severity (green → yellow → red), so the machine's state reads at a
//!   glance the way GNU `top`'s plain numbers do not;
//! * a colour-coded pressure-band **history strip**, one glyph per refresh;
//! * a **task census** line;
//! * a **panel tab bar** showing every detail panel with the focused one
//!   highlighted and a scroll indicator when the panel overflows;
//! * the focused **detail table** (reclaim ledger, `ramzip` counters,
//!   per-CPU load, interrupt lines, or the process summary), with a styled
//!   header row and refusal/quarantine rows drawn in their own colour.
//!
//! Every figure a refused query withheld renders as the refusal it is, in
//! its panel, while the session continues (fail closed, degrade gracefully).
//! Colour is always reinforcement: on a monochrome terminal the gauges still
//! fill, the bars still read, and the header falls back to reverse video —
//! the layout never depends on colour to be legible.
//!
//! The view redraws itself every refresh interval without a key press: the
//! input wait is bounded by the interval, and an elapsed wait re-queries
//! exactly as the refresh key does.

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

/// Rows reserved above the detail panel: title, the three gauges, the
/// history strip, the task census, and the panel tab bar.
const HEADER_ROWS: u16 = 7;
/// Rows reserved below the detail panel: the key-hint footer.
const FOOTER_ROWS: u16 = 1;

/// Screen rows of the fixed summary block.
const ROW_TITLE: u16 = 0;
const ROW_MEM: u16 = 1;
const ROW_PRESSURE: u16 = 2;
const ROW_HISTORY: u16 = 3;
const ROW_CPU: u16 = 4;
const ROW_TASKS: u16 = 5;
const ROW_TABS: u16 = 6;

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

// ---- Detail rows -----------------------------------------------------------

/// How a detail-panel row is drawn: its rendition role, so the renderer can
/// colour a table header, a stated refusal, or a quarantined line distinctly
/// from ordinary body rows without the model knowing about attributes.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub(crate) enum RowStyle {
    /// A column header for the table below it.
    Header,
    /// An ordinary data row.
    Body,
    /// An informational notice (e.g. the own-process scope fallback).
    Notice,
    /// A row calling out a degraded condition (e.g. a quarantined line).
    Warn,
    /// A stated capability refusal or honest absence.
    Denied,
}

/// One line of a detail panel: its text plus how it should be drawn. Pure
/// with respect to the model, so tests assert panel content without a
/// terminal.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct PanelRow {
    /// The rendered text.
    pub text: String,
    /// The rendition role.
    pub style: RowStyle,
}

impl PanelRow {
    fn header(text: String) -> PanelRow {
        PanelRow {
            text,
            style: RowStyle::Header,
        }
    }
    fn body(text: String) -> PanelRow {
        PanelRow {
            text,
            style: RowStyle::Body,
        }
    }
    fn notice(text: String) -> PanelRow {
        PanelRow {
            text,
            style: RowStyle::Notice,
        }
    }
    fn warn(text: String) -> PanelRow {
        PanelRow {
            text,
            style: RowStyle::Warn,
        }
    }
    fn denied(text: String) -> PanelRow {
        PanelRow {
            text,
            style: RowStyle::Denied,
        }
    }
}

/// The title-bar text: tool name, uptime, load averages, and the pin state
/// — a refused pin is a standing, stated condition, never a silent blank.
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

/// The task-census text, GNU-style, with the recorded scope: the
/// system-wide census, or the caller's own with the refusal noted.
fn tasks_line(snapshot: &Snapshot) -> String {
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
        "{} total, {running} running, {sleeping} sleeping, {stopped} stopped, {zombie} zombie",
        snapshot.processes.len()
    );
    if snapshot.global_denied {
        // The short marker fits whatever the counts; the full stated
        // refusal leads the processes panel.
        line.push_str(" (own)");
    }
    line
}

// ---- Detail panels ---------------------------------------------------------

/// The focused detail panel's rows. Pure with respect to the model, so
/// tests assert panel content without a terminal.
pub(crate) fn detail_rows(model: &Model) -> Vec<PanelRow> {
    match model.focus() {
        Focus::Reclaim => reclaim_rows(model.snapshot()),
        Focus::Ramzip => ramzip_rows(model.snapshot()),
        Focus::Cpu => cpu_rows(model),
        Focus::Irqs => irq_rows(model.snapshot()),
        Focus::Processes => process_rows(model),
    }
}

/// One stated-reason row for a gauge the panel cannot show.
fn denied_row<T>(gauge: &Gauge<T>) -> PanelRow {
    PanelRow::denied(String::from(if gauge.is_denied() {
        KERNEL_DENIED
    } else {
        UNAVAILABLE
    }))
}

/// The reclaim-ledger table: one row per class.
fn reclaim_rows(snapshot: &Snapshot) -> Vec<PanelRow> {
    let Some(records) = snapshot.reclaim.ready() else {
        return alloc::vec![denied_row(&snapshot.reclaim)];
    };
    let mut rows = Vec::with_capacity(records.len() + 1);
    rows.push(PanelRow::header(format!(
        "{:<22} {:>8} {:>8} {:>8} {:>8} {:>8} {:>8}",
        "class", "payload", "meta", "entries", "shrinks", "refused", "fails"
    )));
    for record in records {
        let name = tairix_abi::sysinfo::RECLAIM_CLASS_NAMES
            .get(usize::from(record.class))
            .copied()
            .unwrap_or("?");
        rows.push(PanelRow::body(format!(
            "{:<22} {:>8} {:>8} {:>8} {:>8} {:>8} {:>8}",
            name,
            format_size(record.payload_bytes),
            format_size(record.metadata_bytes),
            record.entries,
            record.pressure_shrinks,
            record.refusals,
            record.failures
        )));
    }
    rows
}

/// The `ramzip` counter panel: bytes, caps, and the outcome counters.
fn ramzip_rows(snapshot: &Snapshot) -> Vec<PanelRow> {
    let Some(stats) = snapshot.ramzip.ready() else {
        return alloc::vec![denied_row(&snapshot.ramzip)];
    };
    let saved = stats.logical_bytes.saturating_sub(stats.stored_bytes);
    alloc::vec![
        PanelRow::body(format!(
            "stored {}  logical {}  saved {}  metadata {}  entries {}",
            format_size(stats.stored_bytes),
            format_size(stats.logical_bytes),
            format_size(saved),
            format_size(stats.metadata_bytes),
            stats.entries
        )),
        PanelRow::body(format!(
            "caps   min {}  soft {}  hard {}  pinned {}",
            format_size(stats.min_cap_bytes),
            format_size(stats.soft_cap_bytes),
            format_size(stats.hard_cap_bytes),
            format_size(stats.pinned_bytes)
        )),
        PanelRow::body(format!(
            "compress  attempts {}  accepted {}  incompressible {}  policy {}  cap {}",
            stats.attempts,
            stats.accepted,
            stats.rejected_incompressible,
            stats.rejected_policy,
            stats.rejected_cap
        )),
        PanelRow::body(format!(
            "rejected  ineligible {}  reserve {}  task-share {}  thrash {}",
            stats.rejected_ineligible,
            stats.rejected_reserve,
            stats.rejected_task_share,
            stats.rejected_thrash
        )),
        PanelRow::body(format!(
            "restore   fault-ins {}  warm {}  clustered {}  auth-fail {}  decode-fail {}",
            stats.fault_ins,
            stats.warm_restored,
            stats.cluster_restored,
            stats.auth_failures,
            stats.decode_failures
        )),
        PanelRow::body(format!(
            "warm-up   attempts {}  stopped {}  thrash-detected {}",
            stats.warm_attempts, stats.warm_stopped, stats.thrash_detected
        )),
    ]
}

/// The per-CPU table: busy share over the interval, queue depth, and the
/// switch/preemption counters.
fn cpu_rows(model: &Model) -> Vec<PanelRow> {
    let mut rows = Vec::new();
    rows.push(PanelRow::header(format!(
        "{:>4} {:>6} {:>7} {:>12} {:>12}",
        "cpu", "busy%", "queue", "switches", "preemptions"
    )));
    let busy = model.cpu_busy();
    let loads = model.snapshot().cpu_loads.ready();
    if busy.is_empty() && loads.is_none() {
        rows.push(denied_row(&model.snapshot().cpu_loads));
        return rows;
    }
    for entry in busy {
        let (queue, switches, preemptions) = loads
            .and_then(|records| records.iter().find(|r| r.cpu == entry.cpu))
            .map_or(
                (String::from("?"), String::from("?"), String::from("?")),
                |r| {
                    (
                        format!("{}", r.queue_depth),
                        format!("{}", r.switches),
                        format!("{}", r.preemptions),
                    )
                },
            );
        rows.push(PanelRow::body(format!(
            "{:>4} {:>6} {:>7} {:>12} {:>12}",
            entry.cpu,
            format_tenths(entry.busy_tenths),
            queue,
            switches,
            preemptions
        )));
    }
    rows
}

/// The IRQ table: one row per bound interrupt line, in ascending line
/// order — the line id, the owning driver task, the interrupt count since
/// boot, and whether the line is quarantined (the kernel's runaway-line
/// safety net having disabled it). A quarantined line is drawn in the warn
/// rendition so it reads at a glance. A refused `CAP_SYSINFO_HW` or a failed
/// call shows the stated reason, never a fabricated table.
fn irq_rows(snapshot: &Snapshot) -> Vec<PanelRow> {
    let Some(records) = snapshot.irqs.ready() else {
        return alloc::vec![denied_row(&snapshot.irqs)];
    };
    let mut rows = Vec::with_capacity(records.len() + 1);
    rows.push(PanelRow::header(format!(
        "{:>4} {:>10} {:>14} {}",
        "line", "owner", "count", "state"
    )));
    if records.is_empty() {
        rows.push(PanelRow::notice(String::from("no interrupt lines bound")));
        return rows;
    }
    for record in records {
        let quarantined = record.is_quarantined();
        let text = format!(
            "{:>4} {:>10} {:>14} {}",
            record.line,
            record.owner,
            record.count,
            if quarantined { "quarantined" } else { "active" }
        );
        rows.push(if quarantined {
            PanelRow::warn(text)
        } else {
            PanelRow::body(text)
        });
    }
    rows
}

/// How many top consumers each process list shows.
const TOP_CONSUMERS: usize = 5;

/// The process panel: the top consumers by `%CPU` and by resident bytes.
/// The full interactive list remains `top`'s job; this is the census
/// summary only.
fn process_rows(model: &Model) -> Vec<PanelRow> {
    let snapshot = model.snapshot();
    let mut rows = Vec::new();
    if snapshot.global_denied {
        rows.push(PanelRow::notice(String::from(
            "own processes only (all-process census needs CAP_SYSINFO_GLOBAL)",
        )));
    }
    if snapshot.processes.is_empty() {
        rows.push(PanelRow::denied(String::from(UNAVAILABLE)));
        return rows;
    }
    rows.push(PanelRow::header(format!(
        "top by %cpu    {:>7} {:>6} {:>6} {}",
        "pid", "%cpu", "size", "command"
    )));
    let mut by_cpu: Vec<_> = snapshot.processes.iter().collect();
    by_cpu.sort_by(|a, b| {
        let pa = model.proc_pct(a.proc_id).unwrap_or(0);
        let pb = model.proc_pct(b.proc_id).unwrap_or(0);
        pb.cmp(&pa)
            .then(b.cpu_time_ns.cmp(&a.cpu_time_ns))
            .then(a.pid.cmp(&b.pid))
    });
    for record in by_cpu.iter().take(TOP_CONSUMERS) {
        rows.push(PanelRow::body(format!(
            "               {:>7} {:>6} {:>6} {}",
            record.pid,
            format_tenths(model.proc_pct(record.proc_id).unwrap_or(0)),
            format_size(record.mem_bytes),
            field_lossy(record.name_bytes())
        )));
    }
    rows.push(PanelRow::header(format!(
        "top by memory  {:>7} {:>6} {:>6} {}",
        "pid", "state", "size", "command"
    )));
    let mut by_mem: Vec<_> = snapshot.processes.iter().collect();
    by_mem.sort_by(|a, b| b.mem_bytes.cmp(&a.mem_bytes).then(a.pid.cmp(&b.pid)));
    for record in by_mem.iter().take(TOP_CONSUMERS) {
        rows.push(PanelRow::body(format!(
            "               {:>7} {:>6} {:>6} {}",
            record.pid,
            state_char(record.state),
            format_size(record.mem_bytes),
            field_lossy(record.name_bytes())
        )));
    }
    rows
}

// ---- Theme -----------------------------------------------------------------

/// The resolved rendition palette for one render pass.
///
/// Every attribute is resolved once, up front, from the screen's colour
/// capability, each with a monochrome fallback (reverse video, bold, dim, or
/// plain) so the layout is legible on a terminal that cannot show colour.
/// Rendering is then pure over the theme and the model.
struct Theme {
    /// Title and footer bars: white on blue, else reverse video.
    bar: Attributes,
    /// The focused panel tab: black on cyan, else reverse video.
    tab_active: Attributes,
    /// The unfocused panel tabs: dim.
    tab_inactive: Attributes,
    /// Field labels ("Mem", "Pres", …): bright cyan, else bold.
    label: Attributes,
    /// Table column headers: bold.
    header: Attributes,
    /// Gauge brackets and empty track: dim.
    muted: Attributes,
    /// Low / healthy severity: green.
    ok: Attributes,
    /// Medium severity: yellow.
    warn: Attributes,
    /// High / critical severity: red.
    crit: Attributes,
    /// Informational notices: cyan.
    info: Attributes,
}

impl Theme {
    /// Resolve the palette from `screen`.
    fn resolve<T: Tty>(screen: &mut Screen<T>) -> Theme {
        Theme {
            bar: fg_on_bg(screen, BasicColor::White, BasicColor::Blue, reversed()),
            tab_active: fg_on_bg(screen, BasicColor::Black, BasicColor::Cyan, reversed()),
            tab_inactive: fg_only(screen, BasicColor::BrightBlack, dim()),
            label: fg_only(screen, BasicColor::BrightCyan, bold()),
            header: fg_only(screen, BasicColor::BrightWhite, bold()),
            muted: fg_only(screen, BasicColor::BrightBlack, dim()),
            ok: fg_only(screen, BasicColor::Green, Attributes::PLAIN),
            warn: fg_only(screen, BasicColor::Yellow, bold()),
            crit: fg_only(screen, BasicColor::Red, bold()),
            info: fg_only(screen, BasicColor::Cyan, Attributes::PLAIN),
        }
    }

    /// The severity rendition for a `0..=1000`-tenths fill fraction: green
    /// below 60%, yellow below 85%, red above.
    fn severity(&self, frac_tenths: u32) -> Attributes {
        if frac_tenths < 600 {
            self.ok
        } else if frac_tenths < 850 {
            self.warn
        } else {
            self.crit
        }
    }

    /// The rendition for a pressure band by depth: normal/mild green,
    /// moderate yellow, severe/critical red.
    fn band(&self, depth: usize) -> Attributes {
        match depth {
            0 | 1 => self.ok,
            2 => self.warn,
            _ => self.crit,
        }
    }

    /// The rendition for a detail row's style.
    fn row(&self, style: RowStyle) -> Attributes {
        match style {
            RowStyle::Header => self.header,
            RowStyle::Body => Attributes::PLAIN,
            RowStyle::Notice => self.info,
            RowStyle::Warn => self.warn,
            RowStyle::Denied => self.crit,
        }
    }
}

/// Resolve `fg` on the terminal default background, or `fallback` on a
/// monochrome terminal.
fn fg_only<T: Tty>(screen: &mut Screen<T>, fg: BasicColor, fallback: Attributes) -> Attributes {
    screen
        .colored_attributes(Color::Basic(fg), Color::Default)
        .unwrap_or(fallback)
}

/// Resolve `fg` on `bg`, or `fallback` on a monochrome terminal.
fn fg_on_bg<T: Tty>(
    screen: &mut Screen<T>,
    fg: BasicColor,
    bg: BasicColor,
    fallback: Attributes,
) -> Attributes {
    screen
        .colored_attributes(Color::Basic(fg), Color::Basic(bg))
        .unwrap_or(fallback)
}

/// Reverse-video attributes (the colourless bar fallback).
fn reversed() -> Attributes {
    let mut attrs = Attributes::PLAIN;
    attrs.reverse = true;
    attrs
}

/// Bold attributes.
fn bold() -> Attributes {
    let mut attrs = Attributes::PLAIN;
    attrs.bold = true;
    attrs
}

/// Dim attributes.
fn dim() -> Attributes {
    let mut attrs = Attributes::PLAIN;
    attrs.dim = true;
    attrs
}

// ---- Drawing primitives ----------------------------------------------------

/// One-character band glyphs for the history strip, indexed by band depth
/// (normal, mild, moderate, severe, critical).
const BAND_GLYPHS: [char; tairix_abi::sysinfo::PRESSURE_BAND_COUNT] = ['.', '-', '=', '#', '!'];

/// The filled and empty gauge-cell glyphs. ASCII so the bars read on any
/// console font; colour carries the richness.
const BAR_FILLED: char = '|';
const BAR_EMPTY: char = ' ';

/// The gauge bar width for a screen of `cols` columns: a third of the width,
/// bounded so it neither vanishes on a narrow terminal nor dominates a wide
/// one.
fn bar_width(cols: usize) -> usize {
    (cols / 3).clamp(8, 40)
}

/// Draw `text` at `(row, col)` with `attrs`, truncated to the columns left
/// on the line (never splitting a double-width glyph), and return the column
/// just past it.
fn span(win: &mut Window, row: u16, col: u16, cols: usize, text: &str, attrs: Attributes) -> u16 {
    if usize::from(col) >= cols {
        return col;
    }
    let room = cols - usize::from(col);
    let visible = truncate_to_width(text, room);
    if win.move_to(Pos::new(row, col)).is_ok() {
        win.set_attributes(attrs);
        win.add_str(visible);
        win.set_attributes(Attributes::PLAIN);
    }
    col.saturating_add(u16::try_from(str_width(visible)).unwrap_or(0))
}

/// Draw `ch` at `(row, col)` with `attrs`, if it is on-screen.
fn cell(win: &mut Window, row: u16, col: u16, cols: usize, ch: char, attrs: Attributes) {
    if usize::from(col) < cols && win.move_to(Pos::new(row, col)).is_ok() {
        win.set_attributes(attrs);
        win.add_char(ch);
        win.set_attributes(Attributes::PLAIN);
    }
}

/// Fill the whole of `row` with spaces in `attrs` (the coloured background of
/// the title/footer bars), then return column 0 for text to be drawn over it.
fn bar_row(win: &mut Window, row: u16, cols: usize, attrs: Attributes) {
    if win.move_to(Pos::new(row, 0)).is_ok() {
        win.set_attributes(attrs);
        for _ in 0..cols {
            win.add_char(' ');
        }
        win.set_attributes(Attributes::PLAIN);
    }
}

/// Draw a proportional bar gauge: a `[` bracket, `width` cells filled to
/// `frac_tenths / 1000` in `fill` and the remainder in the muted track, and a
/// `]` bracket. Returns the column just past the closing bracket.
fn gauge(
    win: &mut Window,
    theme: &Theme,
    row: u16,
    col: u16,
    cols: usize,
    frac_tenths: u32,
    fill: Attributes,
) -> u16 {
    let width = bar_width(cols);
    let mut c = span(win, row, col, cols, "[", theme.muted);
    let filled = usize::try_from(u64::from(frac_tenths) * width as u64 / 1000)
        .unwrap_or(width)
        .min(width);
    for i in 0..width {
        let (ch, attrs) = if i < filled {
            (BAR_FILLED, fill)
        } else {
            (BAR_EMPTY, theme.muted)
        };
        cell(win, row, c, cols, ch, attrs);
        c = c.saturating_add(1);
    }
    span(win, row, c, cols, "]", theme.muted)
}

// ---- Summary rows ----------------------------------------------------------

/// Fraction of `part` in `whole` as tenths of a percent (`0..=1000`).
fn frac_tenths(part: u64, whole: u64) -> u32 {
    if whole == 0 {
        return 0;
    }
    u32::try_from(u128::from(part) * 1000 / u128::from(whole))
        .unwrap_or(1000)
        .min(1000)
}

/// The full-width title bar.
fn draw_title(win: &mut Window, theme: &Theme, model: &Model, cols: usize) {
    bar_row(win, ROW_TITLE, cols, theme.bar);
    span(win, ROW_TITLE, 1, cols, &title_line(model), theme.bar);
}

/// The memory gauge: used / total with a severity-coloured bar.
fn draw_memory(win: &mut Window, theme: &Theme, snapshot: &Snapshot, cols: usize) {
    let col = span(win, ROW_MEM, 0, cols, "Mem  ", theme.label);
    let Some(memory) = snapshot.memory.ready() else {
        let reason = if snapshot.memory.is_denied() {
            KERNEL_DENIED
        } else {
            UNAVAILABLE
        };
        span(win, ROW_MEM, col, cols, reason, theme.muted);
        return;
    };
    let used = memory.total_bytes.saturating_sub(memory.free_bytes);
    let frac = frac_tenths(used, memory.total_bytes);
    let after = gauge(win, theme, ROW_MEM, col, cols, frac, theme.severity(frac));
    let trailing = format!(
        "  {} / {} MiB  {}%  kernel {}",
        format_mib(used),
        format_mib(memory.total_bytes),
        frac / 10,
        format_size(memory.kernel_heap_bytes)
    );
    span(win, ROW_MEM, after, cols, &trailing, Attributes::PLAIN);
}

/// One pressure-band segment is this many gauge cells wide, so the five-band
/// gauge carries visual weight rather than reading as five thin ticks.
const BAND_SEGMENT: usize = 3;

/// The pressure gauge: a five-band segmented bar, each entered band filled in
/// its own severity colour, with the band name and the free/reserve figures.
fn draw_pressure(win: &mut Window, theme: &Theme, snapshot: &Snapshot, cols: usize) {
    let col = span(win, ROW_PRESSURE, 0, cols, "Pres ", theme.label);
    let Some(pressure) = snapshot.pressure.ready() else {
        let reason = if snapshot.pressure.is_denied() {
            KERNEL_DENIED
        } else {
            UNAVAILABLE
        };
        span(win, ROW_PRESSURE, col, cols, reason, theme.muted);
        return;
    };
    let band = usize::from(pressure.band);
    let mut c = span(win, ROW_PRESSURE, col, cols, "[", theme.muted);
    for depth in 0..tairix_abi::sysinfo::PRESSURE_BAND_COUNT {
        let filled = depth <= band;
        for _ in 0..BAND_SEGMENT {
            let (ch, attrs) = if filled {
                (BAR_FILLED, theme.band(depth))
            } else {
                (BAR_EMPTY, theme.muted)
            };
            cell(win, ROW_PRESSURE, c, cols, ch, attrs);
            c = c.saturating_add(1);
        }
    }
    c = span(win, ROW_PRESSURE, c, cols, "]", theme.muted);
    let name = tairix_abi::sysinfo::PRESSURE_BAND_NAMES
        .get(band)
        .copied()
        .unwrap_or("?");
    let entries: u64 = pressure.band_entries.iter().sum();
    // The band name reads in its own severity colour; the rest is plain.
    let c2 = span(win, ROW_PRESSURE, c, cols, "  ", Attributes::PLAIN);
    let c3 = span(win, ROW_PRESSURE, c2, cols, name, theme.band(band));
    let trailing = format!(
        "  free {}  reserve {}  {entries} band entries",
        format_size(pressure.free_bytes),
        format_size(pressure.reserve_bytes),
    );
    span(win, ROW_PRESSURE, c3, cols, &trailing, Attributes::PLAIN);
}

/// The band-history strip: one glyph per refresh, oldest leftmost, each glyph
/// coloured by its band so a stretch of pressure reads as a coloured run.
fn draw_history(win: &mut Window, theme: &Theme, model: &Model, cols: usize) {
    let col = span(win, ROW_HISTORY, 0, cols, "Hist ", theme.label);
    let history = model.band_history();
    if history.is_empty() {
        span(win, ROW_HISTORY, col, cols, "(no samples yet)", theme.muted);
        return;
    }
    let room = (cols - usize::from(col)).max(1);
    let start = history.len().saturating_sub(room);
    let mut c = col;
    for &band in &history[start..] {
        let depth = usize::from(band);
        let glyph = BAND_GLYPHS.get(depth).copied().unwrap_or('?');
        cell(win, ROW_HISTORY, c, cols, glyph, theme.band(depth));
        c = c.saturating_add(1);
    }
}

/// The aggregate CPU gauge: all-CPU busy share with a severity-coloured bar,
/// plus the CPU count and the summed switch/preemption counters.
fn draw_cpu(win: &mut Window, theme: &Theme, model: &Model, cols: usize) {
    let col = span(win, ROW_CPU, 0, cols, "CPU  ", theme.label);
    let busy = model.cpu_busy();
    if busy.is_empty() {
        span(win, ROW_CPU, col, cols, UNAVAILABLE, theme.muted);
        return;
    }
    let total: u64 = busy.iter().map(|b| u64::from(b.busy_tenths)).sum();
    let avg = u32::try_from(total / busy.len() as u64)
        .unwrap_or(1000)
        .min(1000);
    let after = gauge(win, theme, ROW_CPU, col, cols, avg, theme.severity(avg));
    let mut trailing = format!("  {}% busy  {} cpus", format_tenths(avg), busy.len());
    match &model.snapshot().cpu_loads {
        Gauge::Ready(loads) => {
            let switches: u64 = loads.iter().map(|l| l.switches).sum();
            let preemptions: u64 = loads.iter().map(|l| l.preemptions).sum();
            let _ = write!(trailing, "  {switches} sw  {preemptions} preempt");
        }
        Gauge::Denied => {
            let _ = write!(trailing, "  counters {KERNEL_DENIED}");
        }
        Gauge::Unavailable => {}
    }
    span(win, ROW_CPU, after, cols, &trailing, Attributes::PLAIN);
}

/// The task-census row.
fn draw_tasks(win: &mut Window, theme: &Theme, snapshot: &Snapshot, cols: usize) {
    let col = span(win, ROW_TASKS, 0, cols, "Tasks", theme.label);
    let text = format!("  {}", tasks_line(snapshot));
    span(win, ROW_TASKS, col, cols, &text, Attributes::PLAIN);
}

/// The panel tab bar: every panel, the focused one highlighted, with a scroll
/// indicator at the right when the focused panel overflows its viewport.
fn draw_tabs(
    win: &mut Window,
    theme: &Theme,
    model: &Model,
    cols: usize,
    lines: usize,
    capacity: usize,
) {
    let mut col = span(win, ROW_TABS, 0, cols, " ", theme.tab_inactive);
    for focus in Focus::ALL {
        let attrs = if focus == model.focus() {
            theme.tab_active
        } else {
            theme.tab_inactive
        };
        col = span(
            win,
            ROW_TABS,
            col,
            cols,
            &format!(" {} ", focus.tab_label()),
            attrs,
        );
        col = span(win, ROW_TABS, col, cols, " ", theme.tab_inactive);
    }
    if lines > capacity {
        let first = model.scroll() + 1;
        let last = (model.scroll() + capacity).min(lines);
        let indicator = format!("[{first}-{last}/{lines}] p:panel");
        let width = u16::try_from(str_width(&indicator)).unwrap_or(0);
        let start = u16::try_from(cols)
            .unwrap_or(width)
            .saturating_sub(width + 1);
        span(win, ROW_TABS, start.max(col), cols, &indicator, theme.muted);
    } else {
        span(win, ROW_TABS, col, cols, " p:panel", theme.muted);
    }
}

/// The detail table: the focused panel's rows, scrolled and styled.
fn draw_detail(
    win: &mut Window,
    theme: &Theme,
    rows: &[PanelRow],
    scroll: usize,
    capacity: usize,
    cols: usize,
) {
    for offset in 0..capacity {
        let row = HEADER_ROWS + u16::try_from(offset).unwrap_or(0);
        let Some(entry) = rows.get(scroll + offset) else {
            // A fresh window's rows are already blank, so an overflowed panel
            // leaves no stale text behind; nothing to draw.
            continue;
        };
        span(win, row, 0, cols, &entry.text, theme.row(entry.style));
    }
}

/// The footer key hints.
const FOOTER_HINT: &str = " q quit  p panel  r refresh  +/- interval  up/down scroll  ? help ";

/// The full-width footer bar.
fn draw_footer(win: &mut Window, theme: &Theme, size: Size, cols: usize) {
    if size.rows >= HEADER_ROWS + FOOTER_ROWS {
        let footer_row = size.rows - 1;
        bar_row(win, footer_row, cols, theme.bar);
        span(win, footer_row, 0, cols, FOOTER_HINT, theme.bar);
    }
}

// ---- Render and run --------------------------------------------------------

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
    let theme = Theme::resolve(screen);

    let capacity = detail_capacity(size);
    model.set_viewport(capacity);
    let rows = detail_rows(model);
    model.clamp_scroll(rows.len());
    let scroll = model.scroll();

    let mut win = Window::new(Pos::ORIGIN, size);
    draw_title(&mut win, &theme, model, cols);
    if size.rows > ROW_MEM {
        draw_memory(&mut win, &theme, model.snapshot(), cols);
    }
    if size.rows > ROW_PRESSURE {
        draw_pressure(&mut win, &theme, model.snapshot(), cols);
    }
    if size.rows > ROW_HISTORY {
        draw_history(&mut win, &theme, model, cols);
    }
    if size.rows > ROW_CPU {
        draw_cpu(&mut win, &theme, model, cols);
    }
    if size.rows > ROW_TASKS {
        draw_tasks(&mut win, &theme, model.snapshot(), cols);
    }
    if size.rows > ROW_TABS {
        draw_tabs(&mut win, &theme, model, cols, rows.len(), capacity);
    }
    draw_detail(&mut win, &theme, &rows, scroll, capacity, cols);
    draw_footer(&mut win, &theme, size, cols);

    screen.wnoutrefresh(&win);

    if model.help_visible() {
        if let Some(overlay) = help_window(size, &theme) {
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

// ---- Help overlay ----------------------------------------------------------

/// The help-overlay title and body, drawn inside a bordered box.
const HELP_TITLE: &str = " sysmon keys ";
const HELP_LINES: &[&str] = &[
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
/// if the screen is too small to hold it. The border and title are drawn in
/// the theme's bar rendition so the overlay reads as a distinct surface.
fn help_window(size: Size, theme: &Theme) -> Option<Window> {
    let mut content_cols = str_width(HELP_TITLE);
    for line in HELP_LINES {
        content_cols = content_cols.max(str_width(line));
    }
    let box_cols = u16::try_from(content_cols + 2).ok()?;
    let box_rows = u16::try_from(HELP_LINES.len() + 2).ok()?;
    if box_rows > size.rows || box_cols > size.cols {
        return None;
    }
    let origin = Pos::new((size.rows - box_rows) / 2, (size.cols - box_cols) / 2);
    let cols = usize::from(box_cols);
    let mut win = Window::new(origin, Size::new(box_rows, box_cols));
    win.set_attributes(theme.bar);
    win.draw_box();
    win.set_attributes(Attributes::PLAIN);
    // The title overlays the top border, centred.
    let title_col = u16::try_from((cols.saturating_sub(str_width(HELP_TITLE))) / 2).unwrap_or(1);
    span(&mut win, 0, title_col, cols, HELP_TITLE, theme.bar);
    for (offset, line) in HELP_LINES.iter().enumerate() {
        let row = 1 + u16::try_from(offset).unwrap_or(0);
        span(&mut win, row, 1, cols, line, Attributes::PLAIN);
    }
    Some(win)
}
