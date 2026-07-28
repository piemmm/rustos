//! The fullscreen, memtest86-style display for the one-way destructive
//! `memtest full` whole-RAM takeover test (`plans/NEW-SUPERVISOR.md` §9,
//! Stage D).
//!
//! Once the takeover has stopped every other CPU and flattened paging, the
//! test owns the machine and the console outright, so this is the natural
//! home for a full-screen progress display. It is built **entirely** on the
//! shared [`Screen`] presenter (`lib/supervisor::screen`), which is itself a
//! thin layer over the `lib/vt` `Op`/`emit` vocabulary — this module never
//! hand-rolls a second copy of the terminal encoding (the charter forbids the
//! duplication) and it names no board, MMIO, or architecture.
//!
//! # A presenter, not a source of truth
//!
//! The UI renders **only** from the values the destructive engine hands it —
//! the running `(tested, total)` byte counts and the final pass / fault /
//! abort outcome — mapped in from the kernel as plain integers so this crate
//! stays free of any kernel type. It computes nothing about the RAM itself;
//! the arithmetic here is purely presentational (bytes → MiB, a fraction →
//! a bar and a percentage).
//!
//! # Degrade gracefully
//!
//! When the backing [`Screen`] is in plain mode (a genuinely dumb serial
//! line, no positioning) the UI falls back to concise, line-oriented progress
//! and result text — never a stream of redrawn bars. Nothing here panics on
//! any input, and a degenerate geometry or a zero total is handled, not
//! faulted.

use tairix_vt::color::{BasicColor, Color};

use crate::screen::{Screen, Style};

/// One binary mebibyte, the unit RAM figures are shown in.
const MIB: u64 = 1024 * 1024;

/// Row of the reverse-video title banner (1-based).
const TITLE_ROW: u16 = 1;
/// Row of the one-line explanation of what the test is doing.
const INTRO_ROW: u16 = 3;
/// Row carrying the total-RAM-under-test figure.
const TOTAL_ROW: u16 = 5;
/// Row carrying the running tested-so-far figure.
const TESTED_ROW: u16 = 6;
/// Row carrying the progress bar and percentage.
const BAR_ROW: u16 = 8;
/// First row of the result / fault panel.
const RESULT_ROW: u16 = 11;

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
/// The rendition of a clean pass result (green, bold).
const PASS_STYLE: Style = Style::fg(Color::Basic(BasicColor::Green)).bold();
/// The rendition of a fault result (red, bold).
const FAULT_STYLE: Style = Style::fg(Color::Basic(BasicColor::Red)).bold();

/// Divide `bytes` down to whole mebibytes.
const fn mib(bytes: u64) -> u64 {
    bytes / MIB
}

/// The completed fraction of the test as a whole-number percent `0..=100`.
///
/// Saturates at 100 (a `tested` past `total` from rounding never overshoots)
/// and treats a zero `total` as `0`, so no input divides by zero or exceeds a
/// [`u8`].
fn percent(tested: u64, total: u64) -> u8 {
    if total == 0 {
        return 0;
    }
    // Widen to `u128` so a very large RAM figure cannot overflow the `* 100`,
    // and clamp the quotient (always `0..=100`) into a `u8` without a lossy
    // cast; the `unwrap_or` can never fire but keeps the path panic-free.
    let numerator = u128::from(tested.min(total)) * 100;
    u8::try_from(numerator / u128::from(total)).unwrap_or(100)
}

/// The fullscreen, memtest86-style presenter for the destructive whole-RAM
/// takeover test.
///
/// It owns the [`Screen`] for the duration of the run (the machine never
/// resumes, so there is no "leave fullscreen"). Every method renders only
/// from the values it is given; it holds no test state of its own beyond what
/// it needs to avoid redundant redraws.
///
/// In rich mode the figures and bar are redrawn in place at fixed positions;
/// in plain mode (a dumb serial line) the same information degrades to
/// concise, deduplicated lines. Neither path panics on any input.
pub struct MemtestUi<'a> {
    /// The rich/plain presenter every byte goes through.
    screen: Screen<'a>,
    /// The bar interior width in cells, derived from the console geometry
    /// once and clamped into `1..=BAR_INNER_MAX`.
    bar_inner: usize,
    /// The total-bytes figure once the engine has reported it (`0` until the
    /// first progress call), so the total line is drawn exactly once.
    total: u64,
    /// The last whole-percent rendered in rich mode, to skip a redraw when it
    /// has not advanced.
    last_percent: Option<u8>,
    /// The last plain-mode progress bucket printed, to keep the fallback to a
    /// handful of lines rather than one per window.
    last_plain_bucket: Option<u8>,
}

/// The plain-mode progress line is printed once per this many percent, so a
/// dumb serial log gets a short, readable ladder rather than a line per 2 MiB
/// window.
const PLAIN_PERCENT_STEP: u8 = 10;

impl<'a> MemtestUi<'a> {
    /// Build a presenter over `screen`.
    ///
    /// The progress-bar interior width is derived once from the screen's
    /// [`Geometry`](crate::screen::Geometry) and clamped into `1..=BAR_INNER_MAX`,
    /// so a tiny or a very wide console both yield a sensible bar without an
    /// unbounded buffer.
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
            last_percent: None,
            last_plain_bucket: None,
        }
    }

    /// Whether the backing screen is in plain (escape-free) mode.
    #[must_use]
    pub fn is_plain(&self) -> bool {
        self.screen.is_plain()
    }

    /// Draw the static frame: enter the alternate screen, the reverse-video
    /// title banner, the one-line explanation, and the (still empty) figure
    /// labels. In plain mode it prints a single introductory line instead.
    pub fn begin(&mut self) {
        if self.screen.is_plain() {
            self.screen.write_str(
                "memtest full: testing all of RAM; the machine will reset when finished.",
            );
            self.screen.newline();
            return;
        }
        self.screen.enter_fullscreen();
        self.screen.move_to(TITLE_ROW, 1);
        self.screen.set_style(&TITLE_STYLE);
        self.screen
            .write_str(" TAIRiX memtest \u{2014} destructive whole-RAM test ");
        self.screen.reset_style();
        self.screen.move_to(INTRO_ROW, LABEL_COL);
        self.screen
            .write_str("Testing all of RAM. The machine will reset when the test finishes.");
        self.screen.move_to(TOTAL_ROW, LABEL_COL);
        self.screen.write_str("RAM under test:");
        self.screen.move_to(TESTED_ROW, LABEL_COL);
        self.screen.write_str("tested:");
    }

    /// Update the display from a running `(tested, total)` byte count.
    ///
    /// Rich mode redraws the tested figure and bar in place whenever the whole
    /// percent advances; plain mode prints a concise line each ten percent.
    /// Both are idempotent between advances, so the engine may call this after
    /// every window without flooding the console.
    pub fn progress(&mut self, tested: u64, total: u64) {
        if self.screen.is_plain() {
            self.progress_plain(tested, total);
        } else {
            self.progress_rich(tested, total);
        }
    }

    /// Rich-mode progress: draw the total once, then redraw the tested figure
    /// and the bar whenever the whole percent advances.
    fn progress_rich(&mut self, tested: u64, total: u64) {
        if self.total != total {
            self.total = total;
            self.screen.move_to(TOTAL_ROW, VALUE_COL);
            self.write_mib(total);
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

    /// Plain-mode progress: print a concise line each `PLAIN_PERCENT_STEP`
    /// percent, deduplicated so a whole-RAM run yields a short ladder rather
    /// than one line per 2 MiB window.
    fn progress_plain(&mut self, tested: u64, total: u64) {
        self.total = total;
        let pct = percent(tested, total);
        let bucket = pct / PLAIN_PERCENT_STEP;
        if self.last_plain_bucket == Some(bucket) {
            return;
        }
        self.last_plain_bucket = Some(bucket);
        self.screen.write_str("memtest full: ");
        self.screen.write_u64(u64::from(pct));
        self.screen.write_str("% (");
        self.screen.write_u64(mib(tested));
        self.screen.write_str(" / ");
        self.screen.write_u64(mib(total));
        self.screen.write_str(" MiB)");
        self.screen.newline();
    }

    /// Render a clean pass; the machine resets next.
    pub fn passed(&mut self, tested: u64) {
        self.result_prefix(RESULT_ROW, &PASS_STYLE);
        self.screen.write_str("memtest full: PASSED \u{2014} ");
        self.screen.write_u64(mib(tested));
        self.screen.write_str(" MiB tested. Resetting.");
        self.result_suffix();
    }

    /// Render a detected RAM fault: a small coloured table in rich mode, a
    /// single line in plain mode. The address and the two word values are the
    /// only data shown — no secret was ever in this pre-unlock RAM, and none
    /// could be here regardless.
    pub fn faulted(&mut self, phys: u64, expected: u64, observed: u64) {
        if self.screen.is_plain() {
            self.screen
                .write_str("memtest full: RAM FAULT at physical ");
            self.screen.write_hex(phys);
            self.screen.write_str(" (expected ");
            self.screen.write_hex(expected);
            self.screen.write_str(", read ");
            self.screen.write_hex(observed);
            self.screen.write_str("). Resetting.");
            self.screen.newline();
            return;
        }
        self.result_prefix(RESULT_ROW, &FAULT_STYLE);
        self.screen.write_str("RAM FAULT");
        self.screen.reset_style();
        self.fault_row(RESULT_ROW + 1, "address:  ", phys);
        self.fault_row(RESULT_ROW + 2, "expected: ", expected);
        self.fault_row(RESULT_ROW + 3, "observed: ", observed);
        self.screen.move_to(RESULT_ROW + 5, LABEL_COL);
        self.screen.write_str("The machine will now reset.");
    }

    /// Render an incomplete (aborted) run. The destructive takeover polls no
    /// abort, but the engine's outcome carries the variant, so it is rendered
    /// for completeness rather than silently swallowed.
    pub fn aborted(&mut self, tested: u64) {
        self.result_prefix(RESULT_ROW, &Style::DEFAULT);
        self.screen
            .write_str("memtest full: run did not complete after ");
        self.screen.write_u64(mib(tested));
        self.screen.write_str(" MiB. Resetting.");
        self.result_suffix();
    }

    /// Write a byte figure as `<n> MiB`, then clear any stale tail so a
    /// shrinking value can never leave a digit behind.
    fn write_mib(&mut self, bytes: u64) {
        self.screen.write_u64(mib(bytes));
        self.screen.write_str(" MiB");
        self.screen.clear_line_tail();
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

    /// Position at the result panel and apply `style`. In plain mode the move
    /// and the style are no-ops, so the result simply follows the progress
    /// text.
    fn result_prefix(&mut self, row: u16, style: &Style) {
        self.screen.move_to(row, LABEL_COL);
        self.screen.set_style(style);
    }

    /// Close a result line: reset the rendition and end the line.
    fn result_suffix(&mut self) {
        self.screen.reset_style();
        self.screen.newline();
    }

    /// One `label 0xVALUE` row of the rich-mode fault table.
    fn fault_row(&mut self, row: u16, label: &str, value: u64) {
        self.screen.move_to(row, LABEL_COL + 2);
        self.screen.write_str(label);
        self.screen.write_hex(value);
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

    /// Count CR-LF line endings by their `\n`, so a line-oriented plain-mode
    /// render can be counted without a UTF-8 decode of rich control bytes.
    fn lines(bytes: &[u8]) -> usize {
        bytes.split(|&b| b == b'\n').count().saturating_sub(1)
    }

    #[test]
    fn percent_saturates_and_handles_a_zero_total() {
        assert_eq!(percent(0, 0), 0);
        assert_eq!(percent(5, 0), 0);
        assert_eq!(percent(0, 100), 0);
        assert_eq!(percent(50, 100), 50);
        assert_eq!(percent(100, 100), 100);
        // A `tested` past `total` (rounding) never overshoots 100.
        assert_eq!(percent(150, 100), 100);
    }

    #[test]
    fn plain_mode_emits_no_escape_bytes() {
        let mut out = VecReport::default();
        {
            let screen = Screen::new(&mut out, Geometry::DEFAULT, true);
            let mut ui = MemtestUi::new(screen);
            ui.begin();
            ui.progress(50 * MIB, 100 * MIB);
            ui.passed(100 * MIB);
        }
        assert!(!has_escape(out.bytes()));
        assert!(out.contains("testing all of RAM"));
        assert!(out.contains("50%"));
        assert!(out.contains("PASSED"));
    }

    #[test]
    fn plain_progress_dedupes_within_one_bucket() {
        let mut out = VecReport::default();
        {
            let screen = Screen::new(&mut out, Geometry::DEFAULT, true);
            let mut ui = MemtestUi::new(screen);
            // 1..=9 MiB of 1000 MiB are all under 1% — one bucket, one line.
            for i in 1..=9u64 {
                ui.progress(i * MIB, 1000 * MIB);
            }
        }
        assert_eq!(lines(out.bytes()), 1);
    }

    #[test]
    fn plain_progress_prints_a_line_per_bucket() {
        let mut out = VecReport::default();
        {
            let screen = Screen::new(&mut out, Geometry::DEFAULT, true);
            let mut ui = MemtestUi::new(screen);
            let total = 100 * MIB;
            ui.progress(5 * MIB, total); // 5%  → bucket 0
            ui.progress(15 * MIB, total); // 15% → bucket 1
            ui.progress(25 * MIB, total); // 25% → bucket 2
        }
        assert_eq!(lines(out.bytes()), 3);
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
    }

    #[test]
    fn rich_progress_draws_a_bar_and_a_percentage() {
        let mut out = VecReport::default();
        {
            let screen = Screen::new(&mut out, Geometry::DEFAULT, false);
            let mut ui = MemtestUi::new(screen);
            ui.progress(50 * MIB, 100 * MIB);
        }
        assert!(out.contains("["));
        assert!(out.contains("]"));
        assert!(out.contains("50%"));
        assert!(out.contains("MiB"));
    }

    #[test]
    fn rich_fault_renders_a_table_with_the_values() {
        let mut out = VecReport::default();
        {
            let screen = Screen::new(&mut out, Geometry::DEFAULT, false);
            let mut ui = MemtestUi::new(screen);
            ui.faulted(0x1234, 0xaaaa_aaaa, 0x0);
        }
        assert!(out.contains("RAM FAULT"));
        assert!(out.contains("0x1234"));
        assert!(out.contains("address:"));
        assert!(out.contains("expected:"));
        assert!(out.contains("observed:"));
    }

    #[test]
    fn plain_fault_is_one_secret_free_line() {
        let mut out = VecReport::default();
        {
            let screen = Screen::new(&mut out, Geometry::DEFAULT, true);
            let mut ui = MemtestUi::new(screen);
            ui.faulted(0x2000, 0xff, 0x00);
        }
        assert!(!has_escape(out.bytes()));
        assert!(out.contains("RAM FAULT"));
        assert!(out.contains("0x2000"));
        assert_eq!(lines(out.bytes()), 1);
    }

    #[test]
    fn a_zero_total_never_panics() {
        let mut out = VecReport::default();
        {
            let screen = Screen::new(&mut out, Geometry::DEFAULT, false);
            let mut ui = MemtestUi::new(screen);
            ui.begin();
            ui.progress(0, 0);
            ui.passed(0);
        }
        assert!(out.contains("PASSED"));
    }

    #[test]
    fn a_narrow_geometry_still_yields_a_valid_bar() {
        let mut out = VecReport::default();
        {
            // cols smaller than the reserved margin clamps the bar to width 1.
            let screen = Screen::new(&mut out, Geometry::new(10, 24), false);
            let mut ui = MemtestUi::new(screen);
            ui.progress(100, 100); // 100%
        }
        assert!(out.contains("100%"));
    }

    #[test]
    fn aborted_reports_the_tested_figure() {
        let mut out = VecReport::default();
        {
            let screen = Screen::new(&mut out, Geometry::DEFAULT, true);
            let mut ui = MemtestUi::new(screen);
            ui.aborted(42 * MIB);
        }
        assert!(out.contains("did not complete"));
        assert!(out.contains("42"));
    }
}
