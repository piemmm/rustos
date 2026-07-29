//! The fullscreen, memtest86-style display for the Supervisor's one-way
//! whole-RAM `memtest` takeover test (`plans/NEW-SUPERVISOR.md` §9, Stage D).
//!
//! Once the takeover has stopped every other CPU, the test owns the machine
//! and the console outright, so this is the natural home for a full-screen
//! progress display. It is built **entirely** on the
//! shared [`Screen`] presenter (`lib/supervisor::screen`), which is itself a
//! thin layer over the `lib/vt` `Op`/`emit` vocabulary — this module never
//! hand-rolls a second copy of the terminal encoding (the charter forbids the
//! duplication) and it names no board, MMIO, or architecture.
//!
//! # A continuous, looping presenter
//!
//! The `memtest` takeover tests all of RAM **continuously**: it cycles a set
//! of thorough patterns (own-address, moving inversions, walking ones/zeros)
//! over every usable frame, over and over, until the operator resets the
//! machine. This presenter renders that live: the elapsed run time, the count
//! of completed test loops, the current pattern, a progress bar for the
//! current pattern, and — beneath the bar — a scrolling log of any RAM errors
//! found, with a running error count.
//!
//! # A presenter, not a source of truth
//!
//! The UI renders **only** from the values the engine hands it — the running
//! `(tested, total)` byte counts, the current pattern name, the elapsed
//! seconds, and each fault's address/expected/observed words — mapped in from
//! the kernel as plain integers and `&str`, so this crate stays free of any
//! kernel type. It computes nothing about the RAM itself; the arithmetic here
//! is purely presentational (bytes → MiB, a fraction → a bar and a
//! percentage, seconds → `HH:MM:SS`).
//!
//! # Degrade gracefully
//!
//! When the backing [`Screen`] is in plain mode (a genuinely dumb serial
//! line, no positioning) the UI falls back to concise, line-oriented output —
//! never a stream of redrawn bars. Nothing here panics on any input, and a
//! degenerate geometry or a zero total is handled, not faulted.

use crate::screen::{Screen, Style};
use tairix_vt::color::{BasicColor, Color};

/// One binary mebibyte, the unit RAM figures are shown in.
const MIB: u64 = 1024 * 1024;

/// Row of the reverse-video title banner (1-based).
const TITLE_ROW: u16 = 1;
/// Row of the one-line explanation of what the test is doing.
const INTRO_ROW: u16 = 2;
/// Row carrying the takeover diagnostics (reserved framebuffer extent and the
/// number of memory regions the sweep walks) — shown so a metal run can see
/// exactly what the sweep kept out and how the map was seen.
const DIAG_ROW: u16 = 3;
/// Row carrying the elapsed run time.
const ELAPSED_ROW: u16 = 4;
/// Row carrying the completed-test-loop count.
const LOOPS_ROW: u16 = 5;
/// Row carrying the total-RAM-under-test figure.
const TOTAL_ROW: u16 = 6;
/// Row carrying the current pattern name.
const PATTERN_ROW: u16 = 7;
/// Row carrying the running tested-so-far figure for the current pattern.
const TESTED_ROW: u16 = 8;
/// Row carrying the progress bar and percentage.
const BAR_ROW: u16 = 9;
/// Row carrying the physical address of the frame currently under test.
const CURRENT_ROW: u16 = 10;
/// Row carrying the running error-count header.
const ERRORS_ROW: u16 = 11;
/// First row of the scrolling fault log, just beneath the error header.
const LOG_TOP_ROW: u16 = 12;
/// Rows of fault log shown at once (a fixed rendering window, not a scalable
/// capacity): the most recent this-many faults.
const LOG_ROWS: usize = 10;
/// Row carrying the per-loop completion status line (the stable marker an
/// automated test keys on).
const STATUS_ROW: u16 = 23;

/// Column labels start at.
const LABEL_COL: u16 = 3;
/// Column the value after a label starts at (so the labels line up).
const VALUE_COL: u16 = 20;

/// The widest the progress bar's interior (between the brackets) may be, so a
/// large discovered geometry cannot grow the on-stack cell buffer without
/// bound. It is a rendering bound, not a scalable capacity.
const BAR_INNER_MAX: usize = 50;

/// The reverse-video, bold title banner rendition.
const TITLE_STYLE: Style = Style::DEFAULT.reverse().bold();
/// The rendition of the filled portion of the progress bar (green).
const FILL_STYLE: Style = Style::fg(Color::Basic(BasicColor::Green));
/// The rendition of the error-count header when faults have been seen (red).
const FAULT_STYLE: Style = Style::fg(Color::Basic(BasicColor::Red)).bold();

/// The plain-mode progress line is printed once per this many percent, so a
/// dumb serial log gets a short, readable ladder rather than a line per 2 MiB
/// window.
const PLAIN_PERCENT_STEP: u8 = 10;

/// Divide `bytes` down to whole mebibytes.
const fn mib(bytes: u64) -> u64 {
    bytes / MIB
}

/// The completed fraction as a whole-number percent `0..=100`.
///
/// Saturates at 100 (a `tested` past `total` from rounding never overshoots)
/// and treats a zero `total` as `0`, so no input divides by zero or exceeds a
/// [`u8`].
fn percent(tested: u64, total: u64) -> u8 {
    if total == 0 {
        return 0;
    }
    let numerator = u128::from(tested.min(total)) * 100;
    u8::try_from(numerator / u128::from(total)).unwrap_or(100)
}

/// One recorded RAM fault, kept as plain integers so the log holds no kernel
/// type. No secret was ever in this pre-unlock RAM, so the address and the
/// two word values are safe to display.
#[derive(Clone, Copy, Default)]
struct FaultEntry {
    phys: u64,
    expected: u64,
    observed: u64,
}

/// The fullscreen, memtest86-style presenter for the continuous whole-RAM
/// takeover test.
///
/// It owns the [`Screen`] for the duration of the run (the machine never
/// resumes, so there is no "leave fullscreen"). Every method renders only
/// from the values it is given plus the small counters it keeps (completed
/// loops, faults seen, a bounded ring of the most recent faults).
pub struct MemtestUi<'a> {
    /// The rich/plain presenter every byte goes through.
    screen: Screen<'a>,
    /// The bar interior width in cells, derived from the console geometry
    /// once and clamped into `1..=BAR_INNER_MAX`.
    bar_inner: usize,
    /// Total bytes under test (`0` until [`set_total`](Self::set_total)).
    total: u64,
    /// Completed full test loops (every pattern over all RAM).
    loops: u64,
    /// Total faults reported so far.
    fault_count: u64,
    /// A bounded ring of the most recent faults, newest wrapping over oldest.
    log: [FaultEntry; LOG_ROWS],
    /// The last whole-percent rendered in rich mode, to skip a redraw when it
    /// has not advanced. Reset by [`set_pattern`](MemtestUi::set_pattern) so
    /// each pattern's bar starts fresh.
    last_percent: Option<u8>,
    /// The last whole-second of elapsed time rendered, to skip a redraw within
    /// the same second.
    last_elapsed: Option<u64>,
    /// The last plain-mode progress bucket printed, to keep the fallback to a
    /// handful of lines rather than one per window.
    last_plain_bucket: Option<u8>,
}

impl<'a> MemtestUi<'a> {
    /// Build a presenter over `screen`.
    ///
    /// The progress-bar interior width is derived once from the screen's
    /// [`Geometry`](crate::screen::Geometry) and clamped into
    /// `1..=BAR_INNER_MAX`, so a tiny or a very wide console both yield a
    /// sensible bar without an unbounded buffer.
    #[must_use]
    pub fn new(screen: Screen<'a>) -> Self {
        // Reserve the left margin, the two brackets, and a trailing " 100%".
        let reserve = usize::from(LABEL_COL) + 2 + 5;
        let inner = usize::from(screen.geometry().cols)
            .saturating_sub(reserve)
            .clamp(1, BAR_INNER_MAX);
        Self {
            screen,
            bar_inner: inner,
            total: 0,
            loops: 0,
            fault_count: 0,
            log: [FaultEntry::default(); LOG_ROWS],
            last_percent: None,
            last_elapsed: None,
            last_plain_bucket: None,
        }
    }

    /// Whether the backing screen is in plain (escape-free) mode.
    #[must_use]
    pub fn is_plain(&self) -> bool {
        self.screen.is_plain()
    }

    /// Draw the static frame: the reverse-video title banner, the one-line
    /// explanation, the (still empty) figure labels, and the error header. In
    /// plain mode it prints a single introductory line instead.
    pub fn begin(&mut self) {
        if self.screen.is_plain() {
            self.screen
                .write_str("memtest: testing RAM continuously; reset the machine to stop.");
            self.screen.newline();
            return;
        }
        self.screen.enter_fullscreen();
        self.screen.move_to(TITLE_ROW, 1);
        self.screen.set_style(&TITLE_STYLE);
        self.screen.write_str(" TAIRiX memtest \u{2014} RAM test ");
        self.screen.reset_style();
        self.screen.move_to(INTRO_ROW, LABEL_COL);
        self.screen
            .write_str("Testing RAM continuously. Reset the machine to stop.");
        self.label(ELAPSED_ROW, "elapsed:");
        self.label(LOOPS_ROW, "loops completed:");
        self.label(TOTAL_ROW, "free RAM under test:");
        self.label(PATTERN_ROW, "pattern:");
        self.label(TESTED_ROW, "tested:");
        self.label(CURRENT_ROW, "current:");
        self.redraw_loops();
        self.redraw_errors();
    }

    /// Draw the takeover diagnostics: the reserved framebuffer extent the
    /// sweep kept out of the destructive pass, the number of free-memory runs
    /// it walks, and how many ranges it excluded from the sweep.
    ///
    /// `fb` is `(phys_base, len_bytes)` of the excluded scan-out surface, or
    /// `None` when none was excluded (a serial-only boot, or a framebuffer
    /// not in swept RAM). `regions` is the number of currently-free physical
    /// runs the sweep tests. `excluded` is the number of ranges the sweep kept
    /// out explicitly — the firmware framebuffer, the one in-use range the
    /// allocator cannot know about; every other in-use frame (the kernel heap,
    /// DMA buffers, driver and userland memory) is already absent from the
    /// free set. Drawn once after [`set_total`](Self::set_total); in plain
    /// mode it prints one concise line.
    pub fn set_environment(&mut self, fb: Option<(u64, u64)>, regions: u64, excluded: u64) {
        if self.screen.is_plain() {
            self.screen.write_str("memtest: reserved fb ");
            self.write_fb(fb);
            self.screen.write_str(", ");
            self.screen.write_u64(regions);
            self.screen.write_str(" regions, ");
            self.screen.write_u64(excluded);
            self.screen.write_str(" excluded");
            self.screen.newline();
            return;
        }
        self.screen.move_to(DIAG_ROW, LABEL_COL);
        self.screen.write_str("reserved fb ");
        self.write_fb(fb);
        self.screen.write_str("   regions: ");
        self.screen.write_u64(regions);
        self.screen.write_str("   excluded: ");
        self.screen.write_u64(excluded);
        self.screen.clear_line_tail();
    }

    /// Record the physical base of the frame now under test and redraw the
    /// live "current address" line, so the value on screen names the frame
    /// the sweep is on right now (the last one shown pins a stall). A no-op in
    /// plain mode: the periodic plain progress ladder already paces the
    /// output, and a line per window would flood a serial log.
    pub fn set_current(&mut self, phys: u64) {
        if self.screen.is_plain() {
            return;
        }
        self.screen.move_to(CURRENT_ROW, VALUE_COL);
        self.screen.write_hex(phys);
        self.screen.clear_line_tail();
    }

    /// Write an excluded-framebuffer extent as `0x<base>+<n> MiB`, or `none`.
    fn write_fb(&mut self, fb: Option<(u64, u64)>) {
        match fb {
            Some((base, len)) => {
                self.screen.write_hex(base);
                self.screen.write_str("+");
                self.screen.write_u64(mib(len));
                self.screen.write_str(" MiB");
            }
            None => self.screen.write_str("none"),
        }
    }

    /// Record the total bytes under test and draw the total-RAM figure once.
    pub fn set_total(&mut self, total: u64) {
        self.total = total;
        if self.screen.is_plain() {
            self.screen.write_str("memtest: free RAM under test: ");
            self.screen.write_u64(mib(total));
            self.screen.write_str(" MiB");
            self.screen.newline();
            return;
        }
        self.screen.move_to(TOTAL_ROW, VALUE_COL);
        self.write_mib(total);
    }

    /// Announce the pattern now running, resetting the per-pattern bar so it
    /// restarts from empty. In plain mode this prints a concise line.
    pub fn set_pattern(&mut self, name: &str) {
        self.last_percent = None;
        self.last_plain_bucket = None;
        if self.screen.is_plain() {
            self.screen.write_str("memtest: pattern ");
            self.screen.write_str(name);
            self.screen.newline();
            return;
        }
        self.screen.move_to(PATTERN_ROW, VALUE_COL);
        self.screen.write_str(name);
        self.screen.clear_line_tail();
    }

    /// Update the display from a running `(tested, total)` byte count and the
    /// elapsed run time in whole seconds.
    ///
    /// Rich mode redraws the tested figure and bar whenever the whole percent
    /// advances, and the elapsed clock whenever the second advances; plain
    /// mode prints a concise line each ten percent. Both are idempotent
    /// between advances, so the engine may call this after every window
    /// without flooding the console.
    pub fn progress(&mut self, tested: u64, total: u64, elapsed_secs: u64) {
        if self.screen.is_plain() {
            self.progress_plain(tested, total, elapsed_secs);
            return;
        }
        if self.last_elapsed != Some(elapsed_secs) {
            self.last_elapsed = Some(elapsed_secs);
            self.redraw_elapsed(elapsed_secs);
        }
        let pct = percent(tested, total);
        if self.last_percent == Some(pct) {
            return;
        }
        self.last_percent = Some(pct);
        self.screen.move_to(TESTED_ROW, VALUE_COL);
        self.write_mib(tested);
        self.draw_bar(pct);
    }

    /// Record and display a detected RAM fault: append it to the scrolling
    /// log, bump the running error count, and redraw both. The address and
    /// the two word values are the only data shown — no secret was ever in
    /// this pre-unlock RAM.
    pub fn record_fault(&mut self, phys: u64, expected: u64, observed: u64) {
        let slot = usize::try_from(self.fault_count % LOG_ROWS as u64).unwrap_or(0);
        self.log[slot] = FaultEntry {
            phys,
            expected,
            observed,
        };
        self.fault_count = self.fault_count.saturating_add(1);
        if self.screen.is_plain() {
            self.screen.write_str("memtest: RAM FAULT at ");
            self.screen.write_hex(phys);
            self.screen.write_str(" expected ");
            self.screen.write_hex(expected);
            self.screen.write_str(" read ");
            self.screen.write_hex(observed);
            self.screen.newline();
            return;
        }
        self.redraw_errors();
    }

    /// Mark one full test loop (every pattern over all RAM) complete: bump the
    /// completed-loop count, redraw it, and print the stable per-loop status
    /// line an automated test keys on.
    pub fn loop_complete(&mut self, elapsed_secs: u64) {
        self.loops = self.loops.saturating_add(1);
        if self.screen.is_plain() {
            self.write_loop_marker(elapsed_secs);
            self.screen.newline();
            return;
        }
        self.redraw_loops();
        self.screen.move_to(STATUS_ROW, LABEL_COL);
        self.write_loop_marker(elapsed_secs);
        self.screen.clear_line_tail();
    }

    /// Write the stable per-loop completion sentence. Emitted as one
    /// contiguous run so the exact substring an automated test matches on
    /// (`memtest: completed test loop <n>`) is never split by a cursor move.
    fn write_loop_marker(&mut self, elapsed_secs: u64) {
        self.screen.write_str("memtest: completed test loop ");
        self.screen.write_u64(self.loops);
        self.screen.write_str(" (elapsed ");
        self.write_clock(elapsed_secs);
        self.screen.write_str(")");
    }

    /// Draw a label at `row`, [`LABEL_COL`].
    fn label(&mut self, row: u16, text: &str) {
        self.screen.move_to(row, LABEL_COL);
        self.screen.write_str(text);
    }

    /// Redraw the completed-loop count.
    fn redraw_loops(&mut self) {
        self.screen.move_to(LOOPS_ROW, VALUE_COL);
        self.screen.write_u64(self.loops);
        self.screen.clear_line_tail();
    }

    /// Redraw the elapsed clock at `elapsed_secs`.
    fn redraw_elapsed(&mut self, elapsed_secs: u64) {
        self.screen.move_to(ELAPSED_ROW, VALUE_COL);
        self.write_clock(elapsed_secs);
        self.screen.clear_line_tail();
    }

    /// Redraw the error-count header and the scrolling fault log beneath it.
    fn redraw_errors(&mut self) {
        self.screen.move_to(ERRORS_ROW, LABEL_COL);
        if self.fault_count > 0 {
            self.screen.set_style(&FAULT_STYLE);
        }
        self.screen.write_str("errors: ");
        self.screen.write_u64(self.fault_count);
        self.screen.reset_style();
        self.screen.clear_line_tail();
        self.redraw_log();
    }

    /// Redraw the most recent [`LOG_ROWS`] faults, oldest-visible first.
    fn redraw_log(&mut self) {
        let shown = usize::try_from(self.fault_count.min(LOG_ROWS as u64)).unwrap_or(LOG_ROWS);
        // Work in the u64 fault-count space for the absolute index so a very
        // large count never overflows a `usize` on a 32-bit target; only the
        // final ring slot (always < LOG_ROWS) is narrowed.
        let start = self.fault_count - shown as u64;
        for r in 0..LOG_ROWS {
            let row = LOG_TOP_ROW + u16::try_from(r).unwrap_or(0);
            self.screen.move_to(row, LABEL_COL);
            if r < shown {
                let abs = start + r as u64;
                let slot = usize::try_from(abs % LOG_ROWS as u64).unwrap_or(0);
                let entry = self.log[slot];
                self.screen.write_hex(entry.phys);
                self.screen.write_str("  exp ");
                self.screen.write_hex(entry.expected);
                self.screen.write_str("  got ");
                self.screen.write_hex(entry.observed);
            }
            self.screen.clear_line_tail();
        }
    }

    /// Plain-mode progress: print a concise line each `PLAIN_PERCENT_STEP`
    /// percent, deduplicated so a whole-RAM sweep yields a short ladder rather
    /// than one line per 2 MiB window.
    fn progress_plain(&mut self, tested: u64, total: u64, elapsed_secs: u64) {
        let pct = percent(tested, total);
        let bucket = pct / PLAIN_PERCENT_STEP;
        if self.last_plain_bucket == Some(bucket) {
            return;
        }
        self.last_plain_bucket = Some(bucket);
        self.screen.write_str("memtest: ");
        self.write_clock(elapsed_secs);
        self.screen.write_str("  ");
        self.screen.write_u64(u64::from(pct));
        self.screen.write_str("% (");
        self.screen.write_u64(mib(tested));
        self.screen.write_str(" / ");
        self.screen.write_u64(mib(total));
        self.screen.write_str(" MiB)");
        self.screen.newline();
    }

    /// Write a byte figure as `<n> MiB`, then clear any stale tail so a
    /// shrinking value can never leave a digit behind.
    fn write_mib(&mut self, bytes: u64) {
        self.screen.write_u64(mib(bytes));
        self.screen.write_str(" MiB");
        self.screen.clear_line_tail();
    }

    /// Write `secs` as a zero-padded `HH:MM:SS` clock. Hours are not clamped —
    /// a run of many hours reads honestly — but minutes and seconds always
    /// occupy two digits.
    fn write_clock(&mut self, secs: u64) {
        let hours = secs / 3600;
        let minutes = (secs % 3600) / 60;
        let seconds = secs % 60;
        self.screen.write_u64(hours);
        self.screen.write_str(":");
        self.write_two(minutes);
        self.screen.write_str(":");
        self.write_two(seconds);
    }

    /// Write `value` (`0..=99`) as exactly two decimal digits.
    fn write_two(&mut self, value: u64) {
        let v = value % 100;
        let tens = u8::try_from(v / 10).unwrap_or(0);
        let ones = u8::try_from(v % 10).unwrap_or(0);
        let digits = [b'0' + tens, b'0' + ones];
        self.screen.write_bytes(&digits);
    }

    /// Draw the bracketed progress bar and the trailing percentage at
    /// [`BAR_ROW`], the filled portion in [`FILL_STYLE`].
    fn draw_bar(&mut self, pct: u8) {
        let inner = self.bar_inner;
        let filled = inner * usize::from(pct) / 100;
        let mut cells = [b' '; BAR_INNER_MAX];
        for cell in &mut cells[..filled] {
            *cell = b'#';
        }
        self.screen.move_to(BAR_ROW, LABEL_COL);
        self.screen.write_bytes(b"[");
        self.screen.set_style(&FILL_STYLE);
        self.screen.write_bytes(&cells[..filled]);
        self.screen.reset_style();
        self.screen.write_bytes(&cells[filled..inner]);
        self.screen.write_bytes(b"] ");
        self.screen.write_u64(u64::from(pct));
        self.screen.write_bytes(b"%");
        self.screen.clear_line_tail();
    }
}

#[cfg(test)]
mod tests {
    use super::{percent, MemtestUi};
    use crate::commands::test_support::VecReport;
    use crate::screen::{Geometry, Screen};

    /// One binary mebibyte, so the tests read in the same unit the UI shows.
    const MIB: u64 = 1024 * 1024;

    /// Whether `bytes` contains an ESC (`0x1b`), i.e. any terminal control.
    fn has_escape(bytes: &[u8]) -> bool {
        bytes.contains(&0x1b)
    }

    /// Count line endings by their `\n`.
    fn lines(bytes: &[u8]) -> usize {
        bytes.split(|&b| b == b'\n').count().saturating_sub(1)
    }

    #[test]
    fn percent_saturates_and_handles_a_zero_total() {
        assert_eq!(percent(0, 0), 0);
        assert_eq!(percent(5, 0), 0);
        assert_eq!(percent(50, 100), 50);
        assert_eq!(percent(100, 100), 100);
        assert_eq!(percent(150, 100), 100);
    }

    #[test]
    fn plain_mode_emits_no_escape_bytes_and_the_key_figures() {
        let mut out = VecReport::default();
        {
            let screen = Screen::new(&mut out, Geometry::DEFAULT, true);
            let mut ui = MemtestUi::new(screen);
            ui.begin();
            ui.set_total(100 * MIB);
            ui.set_pattern("moving inversions (zeros/ones)");
            ui.progress(50 * MIB, 100 * MIB, 5);
            ui.loop_complete(12);
        }
        assert!(!has_escape(out.bytes()));
        assert!(out.contains("testing RAM continuously"));
        assert!(out.contains("50%"));
        assert!(out.contains("moving inversions"));
        assert!(out.contains("memtest: completed test loop 1"));
    }

    #[test]
    fn plain_progress_dedupes_within_one_bucket() {
        let mut out = VecReport::default();
        {
            let screen = Screen::new(&mut out, Geometry::DEFAULT, true);
            let mut ui = MemtestUi::new(screen);
            ui.set_pattern("own-address");
            for i in 1..=9u64 {
                ui.progress(i * MIB, 1000 * MIB, 0);
            }
        }
        // The pattern line plus one progress line (all nine are one bucket).
        assert_eq!(lines(out.bytes()), 2);
    }

    #[test]
    fn rich_begin_enters_fullscreen_and_shows_the_title() {
        let mut out = VecReport::default();
        {
            let screen = Screen::new(&mut out, Geometry::DEFAULT, false);
            let mut ui = MemtestUi::new(screen);
            ui.begin();
        }
        assert!(has_escape(out.bytes()));
        assert!(out.contains("TAIRiX memtest"));
        assert!(out.contains("Reset the machine to stop"));
        assert!(!out.contains("destructive"));
    }

    #[test]
    fn rich_progress_draws_a_bar_a_percentage_and_a_clock() {
        let mut out = VecReport::default();
        {
            let screen = Screen::new(&mut out, Geometry::DEFAULT, false);
            let mut ui = MemtestUi::new(screen);
            ui.set_total(100 * MIB);
            ui.set_pattern("walking ones");
            ui.progress(50 * MIB, 100 * MIB, 3661);
        }
        assert!(out.contains("["));
        assert!(out.contains("50%"));
        assert!(out.contains("MiB"));
        // 3661 s == 01:01:01.
        assert!(out.contains("1:01:01"));
    }

    #[test]
    fn a_recorded_fault_shows_a_count_and_the_values() {
        let mut out = VecReport::default();
        {
            let screen = Screen::new(&mut out, Geometry::DEFAULT, false);
            let mut ui = MemtestUi::new(screen);
            ui.begin();
            ui.record_fault(0x1234, 0xAAAA_AAAA, 0x0);
        }
        assert!(out.contains("errors: 1"));
        assert!(out.contains("0x1234"));
        assert!(out.contains("exp "));
        assert!(out.contains("got "));
    }

    #[test]
    fn the_fault_log_scrolls_to_the_most_recent_entries() {
        let mut out = VecReport::default();
        {
            let screen = Screen::new(&mut out, Geometry::DEFAULT, false);
            let mut ui = MemtestUi::new(screen);
            ui.begin();
            // More faults than the log window: the count keeps climbing, and
            // the newest address is shown while the oldest has scrolled off.
            for i in 0..(super::LOG_ROWS as u64 + 3) {
                ui.record_fault(0x1000 + i * 8, 0xFF, 0x0);
            }
        }
        assert!(out.contains("errors: 13"));
        // The most recent fault (i == 12 → 0x1060) is on screen. (The capture
        // is a cumulative byte log, not a snapshot, so an address that has
        // since scrolled off still appears in the history — the running count
        // is what proves the log kept advancing past its window.)
        assert!(out.contains("0x1060"));
    }

    #[test]
    fn plain_fault_is_one_secret_free_line() {
        let mut out = VecReport::default();
        {
            let screen = Screen::new(&mut out, Geometry::DEFAULT, true);
            let mut ui = MemtestUi::new(screen);
            ui.record_fault(0x2000, 0xff, 0x00);
        }
        assert!(!has_escape(out.bytes()));
        assert!(out.contains("RAM FAULT"));
        assert!(out.contains("0x2000"));
        assert_eq!(lines(out.bytes()), 1);
    }

    #[test]
    fn loop_complete_increments_and_prints_the_stable_marker() {
        let mut out = VecReport::default();
        {
            let screen = Screen::new(&mut out, Geometry::DEFAULT, false);
            let mut ui = MemtestUi::new(screen);
            ui.begin();
            ui.loop_complete(1);
            ui.loop_complete(2);
        }
        assert!(out.contains("memtest: completed test loop 1"));
        assert!(out.contains("memtest: completed test loop 2"));
    }

    #[test]
    fn a_zero_total_never_panics() {
        let mut out = VecReport::default();
        {
            let screen = Screen::new(&mut out, Geometry::DEFAULT, false);
            let mut ui = MemtestUi::new(screen);
            ui.begin();
            ui.set_total(0);
            ui.set_pattern("own-address");
            ui.progress(0, 0, 0);
            ui.loop_complete(0);
        }
        assert!(out.contains("completed test loop 1"));
    }

    #[test]
    fn a_narrow_geometry_still_yields_a_valid_bar() {
        let mut out = VecReport::default();
        {
            let screen = Screen::new(&mut out, Geometry::new(10, 24), false);
            let mut ui = MemtestUi::new(screen);
            ui.set_pattern("walking zeros");
            ui.progress(100, 100, 0);
        }
        assert!(out.contains("100%"));
    }

    #[test]
    fn rich_environment_shows_the_reserved_fb_extent_and_region_count() {
        let mut out = VecReport::default();
        {
            let screen = Screen::new(&mut out, Geometry::DEFAULT, false);
            let mut ui = MemtestUi::new(screen);
            ui.begin();
            ui.set_environment(Some((0x3b40_0000, 8 * MIB)), 5, 3);
            ui.set_current(0x1234_0000);
        }
        assert!(out.contains("reserved fb"));
        assert!(out.contains("0x3b400000"));
        assert!(out.contains("8 MiB"));
        assert!(out.contains("regions: 5"));
        // The framebuffer plus the grown kernel-heap regions kept out.
        assert!(out.contains("excluded: 3"));
        // The live current-address line shows the frame under test.
        assert!(out.contains("current:"));
        assert!(out.contains("0x12340000"));
    }

    #[test]
    fn plain_environment_is_a_secret_free_line_and_set_current_is_silent() {
        let mut out = VecReport::default();
        {
            let screen = Screen::new(&mut out, Geometry::DEFAULT, true);
            let mut ui = MemtestUi::new(screen);
            ui.set_environment(None, 3, 2);
            // No framebuffer was excluded; plain mode names it and stays
            // escape-free, and `set_current` prints nothing (no per-window
            // flood on a serial log).
            ui.set_current(0xdead_0000);
        }
        assert!(!has_escape(out.bytes()));
        assert!(out.contains("reserved fb none"));
        assert!(out.contains("3 regions"));
        assert!(out.contains("2 excluded"));
        assert!(!out.contains("dead"));
        assert_eq!(lines(out.bytes()), 1);
    }
}
