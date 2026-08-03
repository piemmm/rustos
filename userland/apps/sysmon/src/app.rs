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
//! * three **bar gauges** — memory, the pressure band, and aggregate CPU:
//!   * the **memory** bar is a *stacked* gauge whose cells name what the
//!     RAM holds — `#` user-resident (green), `K` kernel heap (cyan), `=`
//!     other in-use (magenta), and blank track for free — a disjoint
//!     decomposition of physical RAM, so the composition reads at a glance
//!     the way GNU `top`'s single used/free number does not;
//!   * the **pressure** band bar and the aggregate **CPU** busy bar are
//!     severity-coloured (green → yellow → red), the CPU bar filled with
//!     `#` busy cells over blank idle track (the kernel accounts busy vs
//!     idle only — there is no user/system/iowait split to draw);
//!   * the `?` help overlay carries a **key** naming every glyph and colour;
//! * a colour-coded pressure-band **history strip**, one glyph per refresh;
//! * a **task census** line;
//! * a **panel tab bar** showing every detail panel with the focused one
//!   highlighted and a scroll indicator when the panel overflows;
//! * the focused **detail table** (the reclaimable-cache page — the
//!   per-class ledger over the per-cache breakdown behind it — `ramzip`
//!   counters, mounted-volume storage, per-CPU load, interrupt lines, or the
//!   process summary), with a styled header row and refusal/quarantine rows
//!   drawn in their own colour.
//!
//! The detail panel is chosen with the Left/Right arrow keys (or `p`), which
//! step the tab ring in either direction.
//!
//! Every figure a refused query withheld renders as the refusal it is, in
//! its panel, while the session continues (fail closed, degrade gracefully).
//! Colour is always reinforcement: on a monochrome terminal the gauges still
//! fill, the bars still read, and the header falls back to reverse video —
//! the layout never depends on colour to be legible. A figure a process
//! reported about its own cache rather than the kernel measuring it says so
//! in words in its row, for an operator reading over a serial console.
//!
//! The view redraws itself every refresh interval without a key press: the
//! input wait is bounded by the interval, and an elapsed wait re-queries
//! exactly as the refresh key does.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::fmt::Write as _;

use tairix_curses::{char_width, str_width, truncate_to_width, Pos, Screen, Size, Tty, Window};
use tairix_procinfo::{
    field_lossy, format_count, format_load, format_size, format_tenths, format_uptime, state_char,
    Transport, SIZE_WIDTH,
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
        Focus::Storage => storage_rows(model.snapshot()),
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

/// The reclaimable-cache page: the per-class ledger table over the
/// per-cache breakdown behind it, so a class total is never just a number
/// — the caches that make it up are on the same page.
///
/// The two tables carry different columns, so a blank line keeps them from
/// reading as one table; each states its own refusal independently.
fn reclaim_rows(snapshot: &Snapshot) -> Vec<PanelRow> {
    let mut rows = reclaim_class_rows(snapshot);
    rows.push(PanelRow::body(String::new()));
    rows.extend(cache_ledger_rows(snapshot));
    rows
}

/// The name of reclaim class `class`, or `?` for an id this build does not
/// know (a class the service invented is never rendered as a real one).
fn class_name(class: u8) -> &'static str {
    tairix_abi::sysinfo::RECLAIM_CLASS_NAMES
        .get(usize::from(class))
        .copied()
        .unwrap_or("?")
}

/// The reclaim-ledger table: one row per reclaim class, leading with the
/// class's live footprint and how much of it is attested, then the
/// cache-effectiveness figures the panel exists to show at a glance —
/// hits, misses, and the hit ratio — and the health counters.
///
/// The `hit%` column is `hits / (hits + misses)` as a whole percent, or
/// `-` for a class no code has looked up this boot (an idle denominator is
/// never a fabricated ratio). `cached` is the class's whole resident
/// footprint (entry payload plus per-entry bookkeeping), and `self%` is
/// the share of that footprint the holding processes reported about
/// themselves rather than the kernel measuring it — the rest is attested,
/// so the column reads as how much of the total is taken on trust.
/// Counts render through [`format_count`] so an unbounded counter never
/// widens its column; the abbreviated headers (`ref` refusals, `shr`
/// pressure shrinks, `fail` internal failures) are spelled out in the
/// manual.
fn reclaim_class_rows(snapshot: &Snapshot) -> Vec<PanelRow> {
    let Some(records) = snapshot.reclaim.ready() else {
        return alloc::vec![denied_row(&snapshot.reclaim)];
    };
    let mut rows = Vec::with_capacity(records.len() + 1);
    rows.push(PanelRow::header(format!(
        "{:<21} {:>7} {:>SIZE_WIDTH$} {:>5} {:>6} {:>6} {:>4} {:>5} {:>5} {:>5}",
        "class", "entries", "cached", "self%", "hits", "misses", "hit%", "ref", "shr", "fail"
    )));
    for record in records {
        let cached = record.payload_bytes.saturating_add(record.metadata_bytes);
        rows.push(PanelRow::body(format!(
            "{:<21} {:>7} {:>SIZE_WIDTH$} {:>5} {:>6} {:>6} {:>4} {:>5} {:>5} {:>5}",
            class_name(record.class),
            format_count(record.entries),
            format_size(cached),
            ratio_pct(record.self_reported_bytes, cached),
            format_count(record.hits),
            format_count(record.misses),
            ratio_pct(record.hits, record.hits.saturating_add(record.misses)),
            format_count(record.refusals),
            format_count(record.pressure_shrinks),
            format_count(record.failures),
        )));
    }
    rows
}

/// Width of the cache-label column: a longer label is elided rather than
/// pushing the figures off an 80-column line.
const CACHE_LABEL_COL: usize = 16;
/// Width of the cache-owner column: room for a kind and an everyday id,
/// with a longer one elided rather than widening the row.
const CACHE_OWNER_COL: usize = 13;

/// The per-cache breakdown: one row per registered cache ledger, so "the
/// `disposable-ui` class holds 12 MiB" becomes "and here is which caches
/// hold it". The rows of a class sum to that class's row in the table
/// above.
///
/// `origin` names where the figures came from: `kernel` for a ledger the
/// kernel measures, `self` for one the holding process reported about
/// itself, `?` for a row the service left unattributed. A self-reported
/// figure is a diagnostic — nothing outside that process can see it, and a
/// compromised process can lie about it — so the word is in the row
/// itself, legible on a monochrome serial console, and the notice
/// rendition is reinforcement only.
fn cache_ledger_rows(snapshot: &Snapshot) -> Vec<PanelRow> {
    use tairix_abi::sysinfo::CacheLedgerOrigin;
    let Some(records) = snapshot.caches.ready() else {
        return alloc::vec![denied_row(&snapshot.caches)];
    };
    let mut rows = Vec::with_capacity(records.len() + 1);
    rows.push(PanelRow::header(format!(
        "{:<CACHE_LABEL_COL$} {:<CACHE_OWNER_COL$} {:<6} {:<21} {:>7} {:>SIZE_WIDTH$} {:>4}",
        "cache", "owner", "origin", "class", "entries", "cached", "hit%"
    )));
    if records.is_empty() {
        rows.push(PanelRow::notice(String::from("no caches registered")));
        return rows;
    }
    for record in records {
        let text = format!(
            "{:<CACHE_LABEL_COL$} {:<CACHE_OWNER_COL$} {:<6} {:<21} {:>7} {:>SIZE_WIDTH$} {:>4}",
            elide_middle(record.label(), CACHE_LABEL_COL),
            elide_middle(&cache_owner(record), CACHE_OWNER_COL),
            cache_origin(record.origin),
            class_name(record.class),
            format_count(record.entries),
            format_size(record.resident_bytes()),
            ratio_pct(record.hits, record.hits.saturating_add(record.misses)),
        );
        rows.push(if record.origin == CacheLedgerOrigin::Kernel {
            PanelRow::body(text)
        } else {
            PanelRow::notice(text)
        });
    }
    rows
}

/// The owner column's text: the kind of principal charged for the cache,
/// with the numeric id the kind carries, and `@pid` for a row a process
/// reported.
///
/// An id is a `u64` and a reported row carries two of them, so this can
/// outgrow [`CACHE_OWNER_COL`]; the caller fits it with [`elide_middle`].
/// An elided id is visibly incomplete and so cannot be read as a different
/// volume, task, or process, where a silently cut one could.
fn cache_owner(record: &tairix_abi::sysinfo::CacheLedgerRecord) -> String {
    use tairix_abi::sysinfo::CacheOwnerKind;
    let owner = match record.owner_kind {
        CacheOwnerKind::KernelSubsystem => String::from("kernel"),
        CacheOwnerKind::FilesystemVolume => format!("vol:{}", record.owner_id),
        CacheOwnerKind::Task => format!("task:{}", record.owner_id),
        CacheOwnerKind::DesktopSession => format!("seat:{}", record.owner_id),
        CacheOwnerKind::UserlandProcess => String::from("proc"),
    };
    if record.reporter_pid == 0 {
        owner
    } else {
        format!("{owner}@{}", record.reporter_pid)
    }
}

/// The `origin` column word: whether the row's figures are attested.
fn cache_origin(origin: tairix_abi::sysinfo::CacheLedgerOrigin) -> &'static str {
    use tairix_abi::sysinfo::CacheLedgerOrigin;
    match origin {
        CacheLedgerOrigin::Kernel => "kernel",
        CacheLedgerOrigin::SelfReported => "self",
        CacheLedgerOrigin::Unset => "?",
    }
}

/// The mark standing in for the elided middle of a name too long for its
/// column. ASCII, like every other glyph this view draws, so it reads on a
/// serial console whose font has no `…`.
const ELISION: char = '~';

/// Fit `text` into `width` columns by dropping its *middle*, keeping both
/// ends.
///
/// The names these tables identify a row by are dotted — a namespace head
/// and a leaf — and it is usually the leaf that tells two of them apart
/// (`clean_fs.data` from `clean_fs.metadata`), so cutting the tail would
/// throw away the identity the column exists to show. The middle goes
/// instead and [`ELISION`] says so, which also keeps an elided owner id
/// from reading as a smaller whole number.
///
/// Text that already fits is returned unchanged; an elided result is
/// exactly `width` columns. Below three columns there is no room for a
/// head, a mark and a leaf, so the text is cut to the budget instead —
/// narrower than asked for, never wider.
pub(crate) fn elide_middle(text: &str, width: usize) -> String {
    if str_width(text) <= width {
        return String::from(text);
    }
    if width < 3 {
        return String::from(truncate_to_width(text, width));
    }
    let leaf = suffix_to_width(text, (width - 1) / 2);
    let head = truncate_to_width(text, width - 1 - str_width(leaf));
    let mut elided = String::with_capacity(head.len() + leaf.len() + 1);
    elided.push_str(head);
    elided.push(ELISION);
    elided.push_str(leaf);
    elided
}

/// The longest suffix of `text` that fits `cols` columns — the tail-side
/// counterpart of [`truncate_to_width`], which fits a prefix.
///
/// A double-width glyph that would straddle the limit is dropped whole, so
/// the result never exceeds `cols` columns.
fn suffix_to_width(text: &str, cols: usize) -> &str {
    let mut used = 0usize;
    let mut start = text.len();
    for (offset, ch) in text.char_indices().rev() {
        used += usize::from(char_width(ch));
        if used > cols {
            break;
        }
        start = offset;
    }
    &text[start..]
}

/// The width of the leading section-name column in the `ramzip` table, so
/// every figure lines up beneath its group.
const RAMZIP_SECTION: usize = 11;

/// The `ramzip` compressed-tier panel: a clean, section-aligned table of the
/// tier's live footprint, its derived caps, and the compression and restore
/// outcome counters — each of the two cache paths carrying its hit ratio (the
/// compression accept rate and the restore success rate), so the cache's
/// effectiveness reads at a glance rather than being inferred from raw
/// counters.
fn ramzip_rows(snapshot: &Snapshot) -> Vec<PanelRow> {
    let Some(stats) = snapshot.ramzip.ready() else {
        return alloc::vec![denied_row(&snapshot.ramzip)];
    };
    let saved = stats.logical_bytes.saturating_sub(stats.stored_bytes);
    let restored = stats
        .fault_ins
        .saturating_add(stats.warm_restored)
        .saturating_add(stats.cluster_restored);
    let restore_failures = stats.auth_failures.saturating_add(stats.decode_failures);
    let section = |name: &str, body: String| -> PanelRow {
        PanelRow::body(format!("{name:<RAMZIP_SECTION$}{body}"))
    };
    alloc::vec![
        section(
            "tier",
            format!(
                "entries {}   logical {}   stored {}   metadata {}",
                stats.entries,
                format_size(stats.logical_bytes),
                format_size(stats.stored_bytes),
                format_size(stats.metadata_bytes),
            ),
        ),
        section(
            "",
            format!(
                "saved {} ({} of logical)",
                format_size(saved),
                ratio_pct(saved, stats.logical_bytes),
            ),
        ),
        section(
            "capacity",
            format!(
                "min {}   soft {}   hard {}   pinned {}",
                format_size(stats.min_cap_bytes),
                format_size(stats.soft_cap_bytes),
                format_size(stats.hard_cap_bytes),
                format_size(stats.pinned_bytes),
            ),
        ),
        section(
            "compress",
            format!(
                "attempts {}   accepted {}   accept-rate {}",
                stats.attempts,
                stats.accepted,
                ratio_pct(stats.accepted, stats.attempts),
            ),
        ),
        section(
            "",
            format!(
                "rejected: incompressible {}  policy {}  cap {}  ineligible {}",
                stats.rejected_incompressible,
                stats.rejected_policy,
                stats.rejected_cap,
                stats.rejected_ineligible,
            ),
        ),
        section(
            "",
            format!(
                "          reserve {}  task-share {}  thrash {}",
                stats.rejected_reserve, stats.rejected_task_share, stats.rejected_thrash,
            ),
        ),
        section(
            "restore",
            format!(
                "faults {}   warm {}   clustered {}   restored {}",
                stats.fault_ins, stats.warm_restored, stats.cluster_restored, restored,
            ),
        ),
        section(
            "",
            format!(
                "failures: auth {}  decode {}   success-rate {}",
                stats.auth_failures,
                stats.decode_failures,
                ratio_pct(restored, restored.saturating_add(restore_failures)),
            ),
        ),
        section(
            "warm-up",
            format!(
                "attempts {}   stopped {}   thrash-detected {}",
                stats.warm_attempts, stats.warm_stopped, stats.thrash_detected,
            ),
        ),
    ]
}

/// Width of the mount-point column: a deeper path is elided rather than
/// pushing the capacity figures off the line.
const MOUNT_TARGET_COL: usize = 20;
/// Width of the filesystem-type column, which holds every type name the
/// system can mount.
const MOUNT_FSTYPE_COL: usize = 7;

/// The mounted-volume storage panel: one row per mounted filesystem with its
/// space accounting — total size, used, available, use percentage, and an
/// ASCII usage bar — the `df`-class view of the disks the system has mounted.
///
/// A mount point too long for its column keeps both ends and loses its
/// middle ([`elide_middle`]): two volumes filed beside each other agree for
/// the whole width of the column and are told apart by the leaf, which a
/// tail cut would be the one part to throw away. A type name is identified
/// by its head, so it is simply cut.
///
/// A volume whose driver reports no capacity (an all-zero
/// [`VolumeStats`](tairix_abi::driver::filesystem::VolumeStats)) shows its
/// identity with `-` for every figure — the honest "capacity unknown"
/// answer, never a fabricated total. A volume that is not healthy
/// (surprise-removed, recovery-conflicted) is drawn in the warn rendition
/// with the condition named, so a dead disk never looks mounted-and-well.
fn storage_rows(snapshot: &Snapshot) -> Vec<PanelRow> {
    use tairix_abi::sysinfo::MountAvailability;
    let Some(records) = snapshot.mounts.ready() else {
        return alloc::vec![denied_row(&snapshot.mounts)];
    };
    let mut rows = Vec::with_capacity(records.len() + 1);
    rows.push(PanelRow::header(format!(
        "{:<MOUNT_TARGET_COL$} {:<MOUNT_FSTYPE_COL$} {:>SIZE_WIDTH$} {:>SIZE_WIDTH$} {:>SIZE_WIDTH$} {:>4}  usage",
        "mounted on", "type", "size", "used", "avail", "use"
    )));
    if records.is_empty() {
        rows.push(PanelRow::notice(String::from("no volumes mounted")));
        return rows;
    }
    for record in records {
        let usage = record.usage();
        let block = u64::from(usage.block_size);
        let total = usage.total_blocks.saturating_mul(block);
        let target = field_lossy(record.target_bytes());
        let fstype = field_lossy(record.fstype_bytes());
        let condition = match record.availability() {
            MountAvailability::Available => "",
            MountAvailability::UnavailableDirty => "  [unavailable-dirty]",
            MountAvailability::UnavailableLost => "  [unavailable-lost]",
            MountAvailability::RecoveryConflict => "  [recovery-conflict]",
            MountAvailability::Degraded => "  [degraded]",
            MountAvailability::Recovering => "  [recovering]",
        };
        let text = if total == 0 {
            // No capacity known: state the identity, never a fabricated size.
            format!(
                "{:<MOUNT_TARGET_COL$} {:<MOUNT_FSTYPE_COL$} {:>SIZE_WIDTH$} {:>SIZE_WIDTH$} {:>SIZE_WIDTH$} {:>4}  capacity unknown{condition}",
                elide_middle(&target, MOUNT_TARGET_COL),
                truncate_to_width(&fstype, MOUNT_FSTYPE_COL),
                "-",
                "-",
                "-",
                "-",
            )
        } else {
            let used = total.saturating_sub(usage.free_blocks.saturating_mul(block));
            let avail = usage.avail_blocks.saturating_mul(block);
            let frac = frac_tenths(used, total);
            format!(
                "{:<MOUNT_TARGET_COL$} {:<MOUNT_FSTYPE_COL$} {:>SIZE_WIDTH$} {:>SIZE_WIDTH$} {:>SIZE_WIDTH$} {:>3}%  {}{condition}",
                elide_middle(&target, MOUNT_TARGET_COL),
                truncate_to_width(&fstype, MOUNT_FSTYPE_COL),
                format_size(total),
                format_size(used),
                format_size(avail),
                frac / 10,
                text_bar(frac, 12),
            )
        };
        rows.push(if record.availability() == MountAvailability::Available {
            PanelRow::body(text)
        } else {
            PanelRow::warn(text)
        });
    }
    rows
}

/// Format `part`/`whole` as a whole-percent string (`"75%"`), or `"-"` when
/// there is nothing to divide — never a fabricated ratio for an idle counter.
fn ratio_pct(part: u64, whole: u64) -> String {
    if whole == 0 {
        return String::from("-");
    }
    format!("{}%", frac_tenths(part, whole) / 10)
}

/// A fixed-width ASCII usage bar, `[|||||     ]`, filled to `frac_tenths`
/// (`0..=1000`) over `width` cells — legible on any console font, in a plain
/// detail row where a coloured cell gauge is not available.
fn text_bar(frac_tenths: u32, width: usize) -> String {
    let filled = usize::try_from(u64::from(frac_tenths) * width as u64 / 1000)
        .unwrap_or(width)
        .min(width);
    let mut bar = String::with_capacity(width + 2);
    bar.push('[');
    for i in 0..width {
        bar.push(if i < filled { BAR_FILLED } else { BAR_EMPTY });
    }
    bar.push(']');
    bar
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
        "top by %cpu    {:>7} {:>6} {:>SIZE_WIDTH$} {}",
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
            "               {:>7} {:>6} {:>SIZE_WIDTH$} {}",
            record.pid,
            format_tenths(model.proc_pct(record.proc_id).unwrap_or(0)),
            format_size(record.mem_bytes),
            field_lossy(record.name_bytes())
        )));
    }
    rows.push(PanelRow::header(format!(
        "top by memory  {:>7} {:>6} {:>SIZE_WIDTH$} {}",
        "pid", "state", "size", "command"
    )));
    let mut by_mem: Vec<_> = snapshot.processes.iter().collect();
    by_mem.sort_by(|a, b| b.mem_bytes.cmp(&a.mem_bytes).then(a.pid.cmp(&b.pid)));
    for record in by_mem.iter().take(TOP_CONSUMERS) {
        rows.push(PanelRow::body(format!(
            "               {:>7} {:>6} {:>SIZE_WIDTH$} {}",
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
pub(crate) struct Theme {
    /// Title and footer bars: white on blue, else reverse video.
    bar: Attributes,
    /// The focused panel tab: black on cyan, else reverse video.
    tab_active: Attributes,
    /// The unfocused panel tabs: dim.
    tab_inactive: Attributes,
    /// Field labels ("Mem", "Pres", …): bright cyan, else bold.
    label: Attributes,
    /// Table column headers: inverted (reverse video) and bold, so the
    /// heading reads as a distinct bar above the body on colour and
    /// monochrome terminals alike.
    header: Attributes,
    /// Gauge brackets and empty track: dim.
    muted: Attributes,
    /// Memory bar: user-resident cells (`#`): green.
    mem_user: Attributes,
    /// Memory bar: kernel-heap cells (`K`): cyan.
    mem_kernel: Attributes,
    /// Memory bar: other-in-use cells (`=`): magenta.
    mem_other: Attributes,
    /// Low / healthy severity: green.
    ok: Attributes,
    /// Medium severity: yellow.
    warn: Attributes,
    /// High / critical severity: red.
    crit: Attributes,
    /// Informational notices: cyan.
    info: Attributes,
}

/// A palette selector for a help-legend sample glyph, so the `?` overlay's
/// key draws each glyph in the exact rendition its bar uses. Kept in step
/// with [`Theme`] so the legend can never show a colour the bars do not.
#[derive(Copy, Clone)]
enum Ink {
    /// Plain body text.
    Plain,
    /// A field label / legend heading.
    Label,
    /// The memory bar's user-resident sample.
    MemUser,
    /// The memory bar's kernel-heap sample.
    MemKernel,
    /// The memory bar's other-in-use sample.
    MemOther,
    /// The healthy / low-severity sample.
    Ok,
    /// The medium-severity sample.
    Warn,
    /// The critical-severity sample.
    Crit,
    /// The muted empty-track sample.
    Muted,
}

impl Theme {
    /// Resolve the palette from `screen`.
    pub(crate) fn resolve<T: Tty>(screen: &mut Screen<T>) -> Theme {
        Theme {
            bar: fg_on_bg(screen, BasicColor::White, BasicColor::Blue, reversed()),
            tab_active: fg_on_bg(screen, BasicColor::Black, BasicColor::Cyan, reversed()),
            tab_inactive: fg_only(screen, BasicColor::BrightBlack, dim()),
            label: fg_only(screen, BasicColor::BrightCyan, bold()),
            header: reversed_bold(),
            muted: fg_only(screen, BasicColor::BrightBlack, dim()),
            mem_user: fg_only(screen, BasicColor::Green, Attributes::PLAIN),
            mem_kernel: fg_only(screen, BasicColor::Cyan, Attributes::PLAIN),
            mem_other: fg_only(screen, BasicColor::Magenta, Attributes::PLAIN),
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

    /// The rendition for a help-legend [`Ink`] selector.
    fn ink(&self, ink: Ink) -> Attributes {
        match ink {
            Ink::Plain => Attributes::PLAIN,
            Ink::Label => self.label,
            Ink::MemUser => self.mem_user,
            Ink::MemKernel => self.mem_kernel,
            Ink::MemOther => self.mem_other,
            Ink::Ok => self.ok,
            Ink::Warn => self.warn,
            Ink::Crit => self.crit,
            Ink::Muted => self.muted,
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

/// Reverse-video and bold attributes (the inverted table-header rendition).
fn reversed_bold() -> Attributes {
    let mut attrs = Attributes::PLAIN;
    attrs.reverse = true;
    attrs.bold = true;
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

/// The stacked-memory-bar category glyphs, each naming what a filled cell of
/// the memory gauge represents. ASCII, so the composition reads on a
/// monochrome console where the category colours are absent; the `?` help
/// overlay documents them. The empty track ([`BAR_EMPTY`]) is free memory.
const MEM_USER_GLYPH: char = '#';
const MEM_KERNEL_GLYPH: char = 'K';
const MEM_OTHER_GLYPH: char = '=';

/// The CPU gauge's busy-cell glyph. The kernel accounts busy vs idle only
/// (no user/system/iowait split), so the bar is `#` busy cells over blank
/// idle track, severity-coloured by the busy share.
const CPU_BUSY_GLYPH: char = '#';

/// The narrowest a summary gauge bar shrinks to before [`gauge_line`] drops
/// it entirely in favour of the figures, and the widest it grows on a roomy
/// screen. Sizing the bar from the space the figures leave — rather than a
/// fixed fraction of the width — is what keeps every figure on the line at
/// 80 columns.
const MIN_BAR_WIDTH: usize = 6;
const MAX_BAR_WIDTH: usize = 24;

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

/// One category slice of a [`segmented_gauge`]: the share of the bar it
/// fills (in tenths of a percent of the whole, `0..=1000`), the glyph its
/// cells carry, and the rendition they are drawn in. Slices are stacked
/// left-to-right; the leftover track is free/idle.
#[derive(Copy, Clone)]
struct Segment {
    /// Share of the whole bar this slice fills, in tenths of a percent.
    frac_tenths: u32,
    /// The glyph each filled cell of the slice carries.
    glyph: char,
    /// The rendition the slice's cells are drawn in.
    attrs: Attributes,
}

/// Draw a stacked bar gauge `width` cells wide: a `[` bracket, then each
/// [`Segment`] laid down left-to-right in its own glyph and rendition, the
/// remainder as muted empty track, and a `]` bracket. Each slice is rounded
/// down to whole cells and clamped to the remaining width, so the segments
/// never overrun the bar (a slice starved to zero cells simply does not
/// draw). Returns the column just past the closing bracket.
fn segmented_gauge(
    win: &mut Window,
    theme: &Theme,
    row: u16,
    col: u16,
    cols: usize,
    width: usize,
    segments: &[Segment],
) -> u16 {
    let mut c = span(win, row, col, cols, "[", theme.muted);
    let mut drawn = 0usize;
    for segment in segments {
        let want =
            usize::try_from(u64::from(segment.frac_tenths) * width as u64 / 1000).unwrap_or(width);
        let cells = want.min(width - drawn);
        for _ in 0..cells {
            cell(win, row, c, cols, segment.glyph, segment.attrs);
            c = c.saturating_add(1);
        }
        drawn += cells;
    }
    for _ in drawn..width {
        cell(win, row, c, cols, BAR_EMPTY, theme.muted);
        c = c.saturating_add(1);
    }
    span(win, row, c, cols, "]", theme.muted)
}

/// Draw a summary gauge line — a `label`, an adaptively sized stacked
/// [`segmented_gauge`], then the `trailing` figures — so the figures are
/// never truncated on a narrow (80-column) screen.
///
/// The bar is sized from the columns left after the label and the figures:
/// it grows to at most [`MAX_BAR_WIDTH`], shrinks toward [`MIN_BAR_WIDTH`]
/// as the figures need the room, and is omitted altogether (label straight
/// into figures) when even the minimum bar plus its brackets will not fit.
/// The figures always win the space, which is what makes the mandatory
/// used/pinned/counter figures fit at 80 columns instead of being cut off.
/// `trailing` carries its own leading separator.
fn gauge_line(
    win: &mut Window,
    theme: &Theme,
    row: u16,
    label: &str,
    segments: &[Segment],
    trailing: &str,
    cols: usize,
) {
    let col = span(win, row, 0, cols, label, theme.label);
    let avail = cols.saturating_sub(usize::from(col));
    // Columns the bar (its two brackets included) may take without pushing a
    // figure off the line.
    let bar_room = avail.saturating_sub(str_width(trailing));
    let after = if bar_room >= MIN_BAR_WIDTH + 2 {
        let width = (bar_room - 2).min(MAX_BAR_WIDTH);
        segmented_gauge(win, theme, row, col, cols, width, segments)
    } else {
        col
    };
    span(win, row, after, cols, trailing, Attributes::PLAIN);
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

/// The memory gauge: a stacked bar decomposing physical RAM into what holds
/// it, plus the used/total figures.
///
/// The bar's slices are a *disjoint* decomposition of `used = total - free`,
/// so they never double-count and the filled width is exactly the used
/// fraction: `#` user-resident memory, then `K` the kernel's own heaps, then
/// `=` the remainder of used (caches, buffers, and everything not separately
/// attributed), over blank free track. Each component is capped to the
/// running remainder of `used` so a component's figure that momentarily
/// exceeds the aggregate can never push the fill past the true used width.
///
/// The compressed `ramzip` tier and pinned anonymous memory overlap these
/// buckets (pinned pages are user-resident; the compressed store is kernel
/// memory), so they are reported as trailing figures rather than as
/// separate, double-counting bar slices — honest accounting over a
/// misleading picture.
fn draw_memory(win: &mut Window, theme: &Theme, snapshot: &Snapshot, cols: usize) {
    let Some(memory) = snapshot.memory.ready() else {
        let col = span(win, ROW_MEM, 0, cols, "Mem  ", theme.label);
        let reason = if snapshot.memory.is_denied() {
            KERNEL_DENIED
        } else {
            UNAVAILABLE
        };
        span(win, ROW_MEM, col, cols, reason, theme.muted);
        return;
    };
    let total = memory.total_bytes;
    let used = total.saturating_sub(memory.free_bytes);
    // Disjoint slices of `used`, each capped to what remains, so the sum is
    // exactly `used` and no bucket is counted twice.
    let resident = memory.user_resident_bytes.min(used);
    let heap = memory.kernel_heap_bytes.min(used - resident);
    let rest = used - resident - heap;
    let segments = [
        Segment {
            frac_tenths: frac_tenths(resident, total),
            glyph: MEM_USER_GLYPH,
            attrs: theme.mem_user,
        },
        Segment {
            frac_tenths: frac_tenths(heap, total),
            glyph: MEM_KERNEL_GLYPH,
            attrs: theme.mem_kernel,
        },
        Segment {
            frac_tenths: frac_tenths(rest, total),
            glyph: MEM_OTHER_GLYPH,
            attrs: theme.mem_other,
        },
    ];
    // Compact per-figure units (`format_size`: `64.0M`, `2.0G`) keep the
    // used/total/heap figures — and the overlapping ramzip/pinned figures —
    // on the line at 80 columns; the adaptive bar yields its cells to them.
    let mut trailing = format!(
        " {}/{} {}% used  kernel {}",
        format_size(used),
        format_size(total),
        frac_tenths(used, total) / 10,
        format_size(heap),
    );
    if let Some(ramzip) = snapshot.ramzip.ready() {
        if ramzip.stored_bytes > 0 || ramzip.pinned_bytes > 0 {
            let _ = write!(
                trailing,
                "  ramzip {}  pinned {}",
                format_size(ramzip.stored_bytes),
                format_size(ramzip.pinned_bytes),
            );
        }
    }
    gauge_line(win, theme, ROW_MEM, "Mem  ", &segments, &trailing, cols);
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
    // `format_size`/`format_count` keep the free/reserve/entry figures on the
    // line at 80 columns even as the entry counter grows without bound.
    let c2 = span(win, ROW_PRESSURE, c, cols, "  ", Attributes::PLAIN);
    let c3 = span(win, ROW_PRESSURE, c2, cols, name, theme.band(band));
    let trailing = format!(
        "  free {}  reserve {}  {} entries",
        format_size(pressure.free_bytes),
        format_size(pressure.reserve_bytes),
        format_count(entries),
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

/// The aggregate CPU gauge: all-CPU busy share drawn as `#` busy cells over
/// blank idle track, severity-coloured by the busy share, plus the CPU count
/// and the summed switch/preemption counters.
///
/// The kernel accounts busy vs idle only (there is no user/system/iowait
/// split in the ABI), so the bar honestly carries a single busy category
/// rather than a fabricated breakdown; the per-CPU detail panel carries each
/// core's own share.
fn draw_cpu(win: &mut Window, theme: &Theme, model: &Model, cols: usize) {
    let busy = model.cpu_busy();
    if busy.is_empty() {
        let col = span(win, ROW_CPU, 0, cols, "CPU  ", theme.label);
        span(win, ROW_CPU, col, cols, UNAVAILABLE, theme.muted);
        return;
    }
    let total: u64 = busy.iter().map(|b| u64::from(b.busy_tenths)).sum();
    let avg = u32::try_from(total / busy.len() as u64)
        .unwrap_or(1000)
        .min(1000);
    let mut trailing = format!(" {}% busy  {} cpus", format_tenths(avg), busy.len());
    match &model.snapshot().cpu_loads {
        Gauge::Ready(loads) => {
            let switches: u64 = loads.iter().map(|l| l.switches).sum();
            let preemptions: u64 = loads.iter().map(|l| l.preemptions).sum();
            // Compact counts (`format_count`: `1.5M`) keep the switch and
            // preemption figures on the line at 80 columns.
            let _ = write!(
                trailing,
                "  {} sw  {} preempt",
                format_count(switches),
                format_count(preemptions)
            );
        }
        Gauge::Denied => {
            let _ = write!(trailing, "  counters {KERNEL_DENIED}");
        }
        Gauge::Unavailable => {}
    }
    gauge_line(
        win,
        theme,
        ROW_CPU,
        "CPU  ",
        &[Segment {
            frac_tenths: avg,
            glyph: CPU_BUSY_GLYPH,
            attrs: theme.severity(avg),
        }],
        &trailing,
        cols,
    );
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
        let indicator = format!("[{first}-{last}/{lines}] <-/-> panel");
        let width = u16::try_from(str_width(&indicator)).unwrap_or(0);
        let start = u16::try_from(cols)
            .unwrap_or(width)
            .saturating_sub(width + 1);
        span(win, ROW_TABS, start.max(col), cols, &indicator, theme.muted);
    } else {
        span(win, ROW_TABS, col, cols, " <-/-> panel", theme.muted);
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
        // A column header reads as an inverted full-width bar, so the table's
        // heading stands out from the body rows below it the way a spreadsheet
        // header row does; other rows draw their text only.
        if entry.style == RowStyle::Header {
            bar_row(win, row, cols, theme.row(RowStyle::Header));
        }
        span(win, row, 0, cols, &entry.text, theme.row(entry.style));
    }
}

/// The footer key hints.
const FOOTER_HINT: &str = " q quit  <-/-> panel  up/down scroll  r refresh  +/- interval  ? help ";

/// The full-width footer bar.
fn draw_footer(win: &mut Window, theme: &Theme, size: Size, cols: usize) {
    if size.rows >= HEADER_ROWS + FOOTER_ROWS {
        let footer_row = size.rows - 1;
        bar_row(win, footer_row, cols, theme.bar);
        span(win, footer_row, 0, cols, FOOTER_HINT, theme.bar);
    }
}

// ---- Render and run --------------------------------------------------------

/// Compose the whole base screen into one [`Window`], sized to `size`.
///
/// Every summary line and the detail panel are laid out here against the
/// screen width, so the composition is testable cell-by-cell without a tty:
/// [`render`] draws this window and, when help is toggled, the overlay on
/// top. The model's viewport and scroll are reconciled to the drawable
/// capacity as a side effect, so the tab bar's indicator and the detail
/// panel agree.
pub(crate) fn compose(model: &mut Model, theme: &Theme, size: Size) -> Window {
    let cols = usize::from(size.cols);
    let capacity = detail_capacity(size);
    model.set_viewport(capacity);
    let rows = detail_rows(model);
    model.clamp_scroll(rows.len());
    let scroll = model.scroll();

    let mut win = Window::new(Pos::ORIGIN, size);
    draw_title(&mut win, theme, model, cols);
    if size.rows > ROW_MEM {
        draw_memory(&mut win, theme, model.snapshot(), cols);
    }
    if size.rows > ROW_PRESSURE {
        draw_pressure(&mut win, theme, model.snapshot(), cols);
    }
    if size.rows > ROW_HISTORY {
        draw_history(&mut win, theme, model, cols);
    }
    if size.rows > ROW_CPU {
        draw_cpu(&mut win, theme, model, cols);
    }
    if size.rows > ROW_TASKS {
        draw_tasks(&mut win, theme, model.snapshot(), cols);
    }
    if size.rows > ROW_TABS {
        draw_tabs(&mut win, theme, model, cols, rows.len(), capacity);
    }
    draw_detail(&mut win, theme, &rows, scroll, capacity, cols);
    draw_footer(&mut win, theme, size, cols);
    win
}

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
    let theme = Theme::resolve(screen);
    let win = compose(model, &theme, size);
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

/// One run of help text drawn in a single rendition: the unit the legend is
/// built from, so a sample glyph can carry the exact colour its bar uses.
#[derive(Copy, Clone)]
struct Chunk {
    /// The literal text of the run.
    text: &'static str,
    /// The rendition to draw it in.
    ink: Ink,
}

/// One line of the help overlay: a sequence of [`Chunk`]s drawn left to
/// right. An empty line is a blank separator.
type HelpLine = &'static [Chunk];

/// A plain-text help chunk.
const fn plain(text: &'static str) -> Chunk {
    Chunk {
        text,
        ink: Ink::Plain,
    }
}

/// The help-overlay title, drawn over the top border.
const HELP_TITLE: &str = " sysmon help ";

/// The help overlay: the key bindings, then a **key** naming every gauge
/// glyph and colour, so a reader can decode the memory and CPU bars without
/// leaving the app.
const HELP_CONTENT: &[HelpLine] = &[
    &[plain(" left / right  previous / next panel")],
    &[plain(" p             next panel")],
    &[plain(" up / down     scroll the panel")],
    &[plain(" PgUp / PgDn   scroll a page")],
    &[plain(" Home / End    first / last")],
    &[plain(" + / -         lengthen / shorten the interval")],
    &[plain(" r             refresh now")],
    &[plain(" q             quit")],
    &[plain(" ?             toggle this help")],
    &[],
    &[Chunk {
        text: " bar key",
        ink: Ink::Label,
    }],
    &[
        plain(" Mem   "),
        Chunk {
            text: "#",
            ink: Ink::MemUser,
        },
        plain(" user  "),
        Chunk {
            text: "K",
            ink: Ink::MemKernel,
        },
        plain(" kernel  "),
        Chunk {
            text: "=",
            ink: Ink::MemOther,
        },
        plain(" other  "),
        Chunk {
            text: "blank",
            ink: Ink::Muted,
        },
        plain(" free"),
    ],
    &[
        plain(" CPU   "),
        Chunk {
            text: "#",
            ink: Ink::Ok,
        },
        plain(" busy (coloured by load)  "),
        Chunk {
            text: "blank",
            ink: Ink::Muted,
        },
        plain(" idle"),
    ],
    &[
        plain(" load  "),
        Chunk {
            text: "ok",
            ink: Ink::Ok,
        },
        plain(" <60%  "),
        Chunk {
            text: "warn",
            ink: Ink::Warn,
        },
        plain(" <85%  "),
        Chunk {
            text: "crit",
            ink: Ink::Crit,
        },
        plain(" >=85%"),
    ],
];

/// The rendered width of one help line: the sum of its chunks' widths.
fn help_line_width(line: HelpLine) -> usize {
    line.iter().map(|chunk| str_width(chunk.text)).sum()
}

/// Build the centred help overlay window for a screen of `size`, or `None`
/// if the screen is too small to hold it. The border and title are drawn in
/// the theme's bar rendition so the overlay reads as a distinct surface, and
/// each legend sample glyph in the exact rendition its bar uses.
fn help_window(size: Size, theme: &Theme) -> Option<Window> {
    let mut content_cols = str_width(HELP_TITLE);
    for line in HELP_CONTENT {
        content_cols = content_cols.max(help_line_width(line));
    }
    let box_cols = u16::try_from(content_cols + 2).ok()?;
    let box_rows = u16::try_from(HELP_CONTENT.len() + 2).ok()?;
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
    for (offset, line) in HELP_CONTENT.iter().enumerate() {
        let row = 1 + u16::try_from(offset).unwrap_or(0);
        let mut col = 1u16;
        for chunk in *line {
            col = span(&mut win, row, col, cols, chunk.text, theme.ink(chunk.ink));
        }
    }
    Some(win)
}
