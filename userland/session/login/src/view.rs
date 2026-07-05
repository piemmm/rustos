//! The full-screen curses login view.
//!
//! [`CursesView`] is the production [`LoginView`]: a full-screen terminal
//! page drawn through `lib/curses` that tells whoever is at the console
//! *which system they are logging in to*. The layout is:
//!
//! * a top status bar — machine name, OS version/build, and the current
//!   wall-clock time on the right;
//! * a bordered login box in the middle of the screen carrying the
//!   `Username:` prompt (which becomes the `Password:` prompt, unechoed);
//! * a running `N failed attempts` line in red beneath the box after any
//!   rejected attempt, accumulating until a session launches;
//! * a bottom status bar — memory in use, task count, logged-in users,
//!   and the 1/5/15-minute load averages.
//!
//! The machine identity and figures come through the injected
//! [`StatusSource`] (on a running system: the `sysinfod` queries and the
//! kernel wall clock), so the whole view is host-testable over an
//! in-memory [`Tty`]. A figure the source cannot supply — a refused or
//! unavailable query — renders as `--`: the refusal is reported in the
//! view and the login session carries on (a denied optional query is an
//! answer, never a fatal error).
//!
//! **Refresh model.** The console stream read is blocking (there is no
//! timed keyboard wait), so the clock and figures refresh at every
//! interaction point: when a round begins, and before each field read.
//! The clock renders to the minute, matching that cadence.
//!
//! On [`session_handoff`](LoginView::session_handoff) the view leaves the
//! alternate screen and restores the cooked input discipline, handing the
//! terminal to the launched session; the next round re-enters it.

use alloc::format;
use alloc::string::String;

use core::cell::{Cell, RefCell};

use rustos_abi::sysinfo::LoadAverage;
use rustos_abi::{Errno, Time64};
use rustos_curses::{str_width, Event, InputMode, Pos, Screen, Size, Tty, Window};
use rustos_vt::{Attributes, BasicColor, Color};

use crate::session::LoginView;

/// The placeholder a figure the [`StatusSource`] cannot supply renders as.
const UNKNOWN: &str = "--";

/// One snapshot of the figures the view's status bars render.
///
/// Every field is optional: a source that cannot answer a query reports
/// `None` and the view renders the `--` placeholder — it never
/// fabricates a figure and never fails the login over one.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct LoginStatus {
    /// The machine's hostname; `None` when the system is unprovisioned.
    pub hostname: Option<String>,
    /// OS version as `(major, minor, patch)`.
    pub version: Option<(u16, u16, u16)>,
    /// Physical memory in use and total, in bytes.
    pub memory: Option<(u64, u64)>,
    /// Live tasks on the system.
    pub tasks: Option<u32>,
    /// Distinct logged-in users.
    pub users: Option<u32>,
    /// The 1/5/15-minute load averages, fixed-point with
    /// [`rustos_abi::sysinfo::LOAD_FIXED_SHIFT`] fractional bits.
    pub load: Option<[u32; 3]>,
}

/// Supplies the status figures and the wall clock the view renders.
///
/// On a running system the implementation queries `sysinfod` and the
/// kernel wall clock; in tests it is a fixture. Every answer is optional
/// and best-effort: the view renders what it gets and placeholders for
/// the rest.
pub trait StatusSource {
    /// The current status figures.
    fn status(&self) -> LoginStatus;

    /// The current wall-clock time, or `None` when no wall time has been
    /// established (the view then shows no clock rather than a fabricated
    /// one).
    fn now(&self) -> Option<Time64>;
}

/// Switches the console line discipline between the view's raw
/// (no-echo, per-keystroke) input and the cooked default a session
/// expects.
///
/// On a running system this is the `stream_input_mode` syscall; in tests
/// a recorder. Kept separate from [`Tty`] because the discipline belongs
/// to the kernel console, not to the byte channel.
pub trait ConsoleMode {
    /// Select raw input: no kernel echo, every keystroke delivered.
    /// Returns whether the discipline is now raw — when it is not, the
    /// kernel is still echoing, and the view refuses to read a password
    /// (fail closed: a credential must never be rendered).
    fn raw(&self) -> bool;

    /// Restore the cooked line-editing default.
    fn cooked(&self);
}

/// The full-screen curses [`LoginView`] over an injected terminal
/// channel, status source, and console-mode switch.
pub struct CursesView<T: Tty, S: StatusSource, M: ConsoleMode> {
    screen: RefCell<Screen<T>>,
    source: S,
    mode: M,
    /// Rejected attempts since the last successful login.
    failures: Cell<u32>,
    /// Whether the console is in the raw (echo-off) discipline; a secret
    /// is read only while this holds.
    raw_active: Cell<bool>,
    /// The figures currently on the bars, re-queried at each refresh.
    status: RefCell<LoginStatus>,
}

/// Rows the login box occupies (border to border).
const BOX_ROWS: u16 = 5;
/// Preferred interior-plus-border width of the login box.
const BOX_COLS: u16 = 46;
/// The title on the box's top border.
const TITLE: &str = " RustOS Login ";

/// Render `t` as a minute-granular clock line, `YYYY-MM-DD HH:MM` (UTC).
fn format_clock(t: Time64) -> String {
    let days = t.secs().div_euclid(86_400);
    let second_of_day = t.secs().rem_euclid(86_400);
    let (year, month, day) = rustos_fsmeta::calendar::civil_from_days(days);
    let hour = second_of_day / 3_600;
    let minute = (second_of_day % 3_600) / 60;
    format!("{year:04}-{month:02}-{day:02} {hour:02}:{minute:02}")
}

/// Render a fixed-point load value as `W.CC`.
fn format_load(fixed: u32) -> String {
    format!(
        "{}.{:02}",
        LoadAverage::whole(fixed),
        LoadAverage::centis(fixed)
    )
}

/// Render a byte count as whole mebibytes.
fn format_mib(bytes: u64) -> String {
    format!("{}", bytes / (1024 * 1024))
}

/// The top status line: machine name and OS version on the left, the
/// clock on the right, padded to `cols`.
fn top_line(status: &LoginStatus, now: Option<Time64>, cols: usize) -> String {
    let host = status.hostname.as_deref().unwrap_or(UNKNOWN);
    let left = match status.version {
        Some((major, minor, patch)) => {
            format!(" {host} - RustOS {major}.{minor}.{patch}")
        }
        None => format!(" {host} - RustOS"),
    };
    let right = now.map(format_clock).unwrap_or_default();
    pad_between(&left, &right, cols)
}

/// The bottom status line: memory, tasks, users, and load averages,
/// padded to `cols`.
fn bottom_line(status: &LoginStatus, cols: usize) -> String {
    let memory = match status.memory {
        Some((used, total)) => {
            format!("mem {}/{} MiB", format_mib(used), format_mib(total))
        }
        None => format!("mem {UNKNOWN}"),
    };
    let tasks = match status.tasks {
        Some(n) => format!("tasks {n}"),
        None => format!("tasks {UNKNOWN}"),
    };
    let users = match status.users {
        Some(n) => format!("users {n}"),
        None => format!("users {UNKNOWN}"),
    };
    let load = match status.load {
        Some([one, five, fifteen]) => format!(
            "load {} {} {}",
            format_load(one),
            format_load(five),
            format_load(fifteen)
        ),
        None => format!("load {UNKNOWN}"),
    };
    pad_between(&format!(" {memory} | {tasks} | {users} | {load}"), "", cols)
}

/// `left` and `right` joined with padding so the whole line is `cols`
/// columns; a line that would overflow keeps `left` and truncates on the
/// right edge.
fn pad_between(left: &str, right: &str, cols: usize) -> String {
    let mut line = String::from(left);
    let used = str_width(left) + str_width(right);
    if used < cols {
        for _ in 0..(cols - used) {
            line.push(' ');
        }
        line.push_str(right);
    } else if str_width(left) < cols {
        line.push_str(right);
    }
    let mut out = String::new();
    let mut width = 0usize;
    for ch in line.chars() {
        let w = rustos_curses::char_width(ch) as usize;
        if width + w > cols {
            break;
        }
        width += w;
        out.push(ch);
    }
    // A trailing space shortfall (a truncated wide char) is padded out so
    // a reverse-video bar always spans the full width.
    while width < cols {
        out.push(' ');
        width += 1;
    }
    out
}

/// The failed-attempts line, or `None` while no attempt has failed.
fn failure_line(failures: u32) -> Option<String> {
    match failures {
        0 => None,
        1 => Some(String::from("1 failed attempt")),
        n => Some(format!("{n} failed attempts")),
    }
}

impl<T: Tty, S: StatusSource, M: ConsoleMode> CursesView<T, S, M> {
    /// Build the view over `screen` (already sized to the terminal),
    /// `source`, and `mode`.
    #[must_use]
    pub fn new(screen: Screen<T>, source: S, mode: M) -> Self {
        Self {
            screen: RefCell::new(screen),
            source,
            mode,
            failures: Cell::new(0),
            raw_active: Cell::new(false),
            status: RefCell::new(LoginStatus::default()),
        }
    }

    /// Re-query the status source (bars are stale the moment they render;
    /// this is called at every interaction point).
    fn refresh_status(&self) {
        *self.status.borrow_mut() = self.source.status();
    }

    /// Where the login box's top-left corner sits for screen `size`.
    fn box_origin(size: Size) -> Pos {
        let rows = size.rows.saturating_sub(BOX_ROWS) / 2;
        let cols = size.cols.saturating_sub(Self::box_cols(size)) / 2;
        Pos::new(rows, cols)
    }

    /// The box width for screen `size` (narrow terminals shrink it).
    fn box_cols(size: Size) -> u16 {
        BOX_COLS.min(size.cols.saturating_sub(2)).max(20)
    }

    /// Draw the whole page: bars, box, prompt field, and failure line.
    ///
    /// `label` is the field prompt (`Username:` …), `shown` the text
    /// echoed in the field (already masked for a secret). Drawing is
    /// best-effort: a failed flush surfaces on the next read instead.
    fn draw(&self, label: &str, shown: &str) {
        let mut screen = self.screen.borrow_mut();
        let size = screen.size();
        let status = self.status.borrow();
        let now = self.source.now();

        // The backdrop: status bars and the failure line.
        let mut page = Window::new(Pos::ORIGIN, size);
        page.erase();
        let bar_attrs = Attributes {
            reverse: true,
            ..Attributes::default()
        };
        page.set_attributes(bar_attrs);
        let _ = page.move_add_str(Pos::new(0, 0), &top_line(&status, now, size.cols as usize));
        let _ = page.move_add_str(
            Pos::new(size.rows.saturating_sub(1), 0),
            &bottom_line(&status, size.cols as usize),
        );
        page.set_attributes(Attributes::default());

        let origin = Self::box_origin(size);
        // The red failure count, centred beneath the box.
        if let Some(line) = failure_line(self.failures.get()) {
            let red = Attributes {
                bold: true,
                foreground: Color::Basic(BasicColor::Red),
                ..Attributes::default()
            };
            page.set_attributes(red);
            let width = u16::try_from(str_width(&line)).unwrap_or(u16::MAX);
            let col = size.cols.saturating_sub(width) / 2;
            let _ = page.move_add_str(Pos::new(origin.row + BOX_ROWS + 1, col), &line);
            page.set_attributes(Attributes::default());
        }

        // The bordered login box: its own window composited over the
        // backdrop, whose cursor (at the input point) becomes the screen
        // cursor.
        let box_cols = Self::box_cols(size);
        let mut frame = Window::new(origin, Size::new(BOX_ROWS, box_cols));
        frame.draw_box();
        let title_width = u16::try_from(str_width(TITLE)).unwrap_or(u16::MAX);
        let title_col = box_cols.saturating_sub(title_width) / 2;
        let _ = frame.move_add_str(Pos::new(0, title_col), TITLE);
        let _ = frame.move_add_str(Pos::new(2, 2), label);
        frame.add_str(shown);

        screen.wnoutrefresh(&page);
        screen.wnoutrefresh(&frame);
        screen.set_cursor_visible(true);
        let _ = screen.doupdate();
    }

    /// Read one field: draw `label`, collect keystrokes into `buf`
    /// (echoed when `echo`, hidden otherwise), and return the filled
    /// length on Enter.
    ///
    /// `buf`'s capacity (`INPUT_LINE_MAX`) is a validation bound, not a
    /// capacity: a line that would exceed it is refused whole with
    /// [`Errno::LengthOutOfRange`] — never silently truncated — exactly
    /// as the kernel read line discipline refuses one. Backspace removes
    /// the last character. Any other special key is ignored.
    fn read_field(&self, label: &str, echo: bool, buf: &mut [u8]) -> Result<usize, Errno> {
        self.refresh_status();
        let mut len = 0usize;
        loop {
            let shown = if echo {
                // Only ever the bytes this loop wrote, which are UTF-8.
                core::str::from_utf8(&buf[..len]).unwrap_or("")
            } else {
                ""
            };
            self.draw(label, shown);
            let event = {
                let mut screen = self.screen.borrow_mut();
                screen.getch()
            };
            match event {
                Ok(Some(Event::Enter)) => return Ok(len),
                Ok(Some(Event::Char(ch))) => {
                    if ch.is_control() {
                        continue;
                    }
                    let width = ch.len_utf8();
                    if len + width > buf.len() {
                        // The buffer bound is a validation bound: an
                        // over-long line is refused whole, never silently
                        // truncated.
                        return Err(Errno::LengthOutOfRange);
                    }
                    ch.encode_utf8(&mut buf[len..len + width]);
                    len += width;
                }
                Ok(Some(Event::Backspace)) => {
                    // Step back over the previous UTF-8 boundary.
                    while len > 0 {
                        len -= 1;
                        if buf[len] & 0b1100_0000 != 0b1000_0000 {
                            break;
                        }
                    }
                }
                // Arrows, function keys, pastes, mice: not part of a
                // credential; ignored.
                Ok(Some(_)) => {}
                // A blocking read that yields nothing, or a failed
                // channel: the console is gone. Fail closed, exactly as
                // the line-discipline reader reports a closed stream.
                Ok(None) | Err(_) => return Err(Errno::NotFound),
            }
        }
    }
}

impl<T: Tty, S: StatusSource, M: ConsoleMode> LoginView for CursesView<T, S, M> {
    fn round_begin(&self) {
        self.raw_active.set(self.mode.raw());
        self.refresh_status();
        let mut screen = self.screen.borrow_mut();
        screen.set_input_mode(InputMode::Blocking);
        let _ = screen.enter_full_screen();
        drop(screen);
        self.draw("Username: ", "");
    }

    fn read_username(&self, buf: &mut [u8]) -> Result<usize, Errno> {
        self.read_field("Username: ", true, buf)
    }

    fn read_password(&self, buf: &mut [u8]) -> Result<usize, Errno> {
        // With the kernel still echoing, typing the secret would render
        // it: refuse rather than read a password that would show.
        if !self.raw_active.get() {
            return Err(Errno::PermissionDenied);
        }
        self.read_field("Password: ", false, buf)
    }

    fn read_session_choice(&self, buf: &mut [u8]) -> Result<usize, Errno> {
        self.read_field("Session [text]/[g]raphical: ", true, buf)
    }

    fn note_failure(&self) {
        self.failures.set(self.failures.get().saturating_add(1));
        self.draw("Username: ", "");
    }

    fn session_handoff(&self) {
        self.failures.set(0);
        self.raw_active.set(false);
        let mut screen = self.screen.borrow_mut();
        let _ = screen.leave_full_screen();
        drop(screen);
        self.mode.cooked();
    }
}

#[cfg(test)]
mod tests {
    use super::{
        bottom_line, failure_line, format_clock, format_load, pad_between, top_line, ConsoleMode,
        CursesView, LoginStatus, StatusSource,
    };
    use crate::session::LoginView;
    use alloc::collections::VecDeque;
    use alloc::string::String;
    use alloc::vec::Vec;
    use core::cell::{Cell, RefCell};
    use rustos_abi::sysinfo::LOAD_FIXED_SHIFT;
    use rustos_abi::{Errno, Time64};
    use rustos_curses::{CursesError, Screen, Size, Tty};
    use rustos_termcap::TermType;

    /// A scripted terminal: reads replay queued byte chunks and fail once
    /// the script is exhausted (the closed-console signal); writes append
    /// to one transcript.
    struct ScriptTty {
        input: RefCell<VecDeque<Vec<u8>>>,
        written: alloc::rc::Rc<RefCell<Vec<u8>>>,
    }

    impl Tty for ScriptTty {
        fn write(&mut self, bytes: &[u8]) -> rustos_curses::Result<()> {
            self.written.borrow_mut().extend_from_slice(bytes);
            Ok(())
        }
        fn read(&mut self) -> rustos_curses::Result<Vec<u8>> {
            self.input.borrow_mut().pop_front().ok_or(CursesError::Io)
        }
    }

    struct FixtureSource {
        status: LoginStatus,
        now: Option<Time64>,
    }

    impl StatusSource for FixtureSource {
        fn status(&self) -> LoginStatus {
            self.status.clone()
        }
        fn now(&self) -> Option<Time64> {
            self.now
        }
    }

    #[derive(Default)]
    struct RecordingMode {
        raws: Cell<u32>,
        cookeds: Cell<u32>,
    }

    impl ConsoleMode for alloc::rc::Rc<RecordingMode> {
        fn raw(&self) -> bool {
            self.raws.set(self.raws.get() + 1);
            true
        }
        fn cooked(&self) {
            self.cookeds.set(self.cookeds.get() + 1);
        }
    }

    /// A console whose raw discipline cannot be selected.
    struct StuckCookedMode;

    impl ConsoleMode for StuckCookedMode {
        fn raw(&self) -> bool {
            false
        }
        fn cooked(&self) {}
    }

    fn status() -> LoginStatus {
        LoginStatus {
            hostname: Some(String::from("lovelace")),
            version: Some((0, 3, 1)),
            memory: Some((256 * 1024 * 1024, 1024 * 1024 * 1024)),
            tasks: Some(17),
            users: Some(2),
            load: Some([
                (1 << LOAD_FIXED_SHIFT) / 2,
                1 << LOAD_FIXED_SHIFT,
                2 << LOAD_FIXED_SHIFT,
            ]),
        }
    }

    /// A view over a scripted terminal, returning the shared transcript
    /// and the recording console-mode switch.
    #[allow(clippy::type_complexity)]
    fn view_with(
        script: &[&[u8]],
    ) -> (
        CursesView<ScriptTty, FixtureSource, alloc::rc::Rc<RecordingMode>>,
        alloc::rc::Rc<RefCell<Vec<u8>>>,
        alloc::rc::Rc<RecordingMode>,
    ) {
        let written = alloc::rc::Rc::new(RefCell::new(Vec::new()));
        let mode = alloc::rc::Rc::new(RecordingMode::default());
        let tty = ScriptTty {
            input: RefCell::new(script.iter().map(|c| c.to_vec()).collect()),
            written: written.clone(),
        };
        let screen = Screen::new(tty, TermType::Xterm256Color, Size::new(24, 80));
        let source = FixtureSource {
            status: status(),
            // 2026-07-03 17:44:00 UTC.
            now: Some(Time64::from_secs(1_783_100_640)),
        };
        (CursesView::new(screen, source, mode.clone()), written, mode)
    }

    fn transcript(written: &RefCell<Vec<u8>>) -> String {
        String::from_utf8(written.borrow().clone()).expect("terminal bytes are UTF-8")
    }

    #[test]
    fn clock_renders_civil_utc_to_the_minute() {
        assert_eq!(format_clock(Time64::from_secs(0)), "1970-01-01 00:00");
        assert_eq!(
            format_clock(Time64::from_secs(1_783_100_640)),
            "2026-07-03 17:44"
        );
    }

    #[test]
    fn load_renders_fixed_point_hundredths() {
        assert_eq!(format_load(0), "0.00");
        assert_eq!(format_load(1 << LOAD_FIXED_SHIFT), "1.00");
        assert_eq!(format_load((1 << LOAD_FIXED_SHIFT) / 2), "0.50");
    }

    #[test]
    fn top_line_carries_host_version_and_clock() {
        let line = top_line(&status(), Some(Time64::from_secs(0)), 60);
        assert!(line.contains("lovelace - RustOS 0.3.1"), "{line}");
        assert!(line.ends_with("1970-01-01 00:00"), "{line}");
        assert_eq!(rustos_curses::str_width(&line), 60);
    }

    #[test]
    fn bottom_line_carries_every_figure() {
        let line = bottom_line(&status(), 80);
        assert!(line.contains("mem 256/1024 MiB"), "{line}");
        assert!(line.contains("tasks 17"), "{line}");
        assert!(line.contains("users 2"), "{line}");
        assert!(line.contains("load 0.50 1.00 2.00"), "{line}");
    }

    #[test]
    fn missing_figures_render_placeholders_not_fabrications() {
        let line = bottom_line(&LoginStatus::default(), 80);
        assert!(line.contains("mem --"), "{line}");
        assert!(line.contains("tasks --"), "{line}");
        assert!(line.contains("users --"), "{line}");
        assert!(line.contains("load --"), "{line}");
        let top = top_line(&LoginStatus::default(), None, 80);
        assert!(top.contains("-- - RustOS"), "{top}");
    }

    #[test]
    fn pad_between_always_spans_the_width() {
        assert_eq!(pad_between("a", "b", 5), "a   b");
        // Overflow keeps the left text and truncates at the edge.
        assert_eq!(pad_between("abcdef", "gh", 4), "abcd");
        assert_eq!(rustos_curses::str_width(&pad_between("", "", 7)), 7);
    }

    #[test]
    fn failure_line_counts_in_english() {
        assert_eq!(failure_line(0), None);
        assert_eq!(failure_line(1).as_deref(), Some("1 failed attempt"));
        assert_eq!(failure_line(3).as_deref(), Some("3 failed attempts"));
    }

    #[test]
    fn round_begin_draws_the_chrome_and_selects_raw_input() {
        let (view, written, mode) = view_with(&[]);
        view.round_begin();
        let text = transcript(&written);
        // The minimal-diff renderer may split runs of blanks into cursor
        // jumps, so each visible word is asserted on its own.
        assert!(text.contains("lovelace - RustOS 0.3.1"), "{text}");
        assert!(text.contains("RustOS") && text.contains("Login"), "{text}");
        assert!(text.contains("Username:"), "{text}");
        assert!(text.contains("load 0.50 1.00 2.00"), "{text}");
        assert!(mode.raws.get() >= 1);
    }

    #[test]
    fn username_read_echoes_and_returns_the_line() {
        // `z` and `q` appear nowhere in the chrome, so their presence in
        // the transcript proves the echo (the diff renderer emits each
        // keystroke separately, so the whole name is never contiguous).
        let (view, written, _mode) = view_with(&[b"z", b"q", b"\r"]);
        let mut buf = [0u8; 32];
        let len = view.read_username(&mut buf).expect("username read");
        assert_eq!(&buf[..len], b"zq");
        let text = transcript(&written);
        assert!(text.contains('z') && text.contains('q'), "{text}");
    }

    #[test]
    fn backspace_edits_the_field() {
        let (view, _written, _mode) = view_with(&[b"ab\x7fc", b"\r"]);
        let mut buf = [0u8; 32];
        let len = view.read_username(&mut buf).expect("username read");
        assert_eq!(&buf[..len], b"ac");
    }

    #[test]
    fn a_stuck_echo_refuses_the_password_read() {
        // If the raw (echo-off) discipline cannot be selected, typing the
        // secret would render it: the read fails closed instead.
        let written = alloc::rc::Rc::new(RefCell::new(Vec::new()));
        let tty = ScriptTty {
            input: RefCell::new(VecDeque::new()),
            written,
        };
        let screen = Screen::new(tty, TermType::Xterm256Color, Size::new(24, 80));
        let source = FixtureSource {
            status: status(),
            now: None,
        };
        let view = CursesView::new(screen, source, StuckCookedMode);
        view.round_begin();
        let mut buf = [0u8; 8];
        assert_eq!(view.read_password(&mut buf), Err(Errno::PermissionDenied));
    }

    #[test]
    fn password_read_never_renders_the_secret() {
        // `z` and `q` appear nowhere in the chrome, so any echo of the
        // secret would surface them in the transcript.
        let (view, written, _mode) = view_with(&[b"zq", b"\r"]);
        view.round_begin();
        let mut buf = [0u8; 32];
        let len = view.read_password(&mut buf).expect("password read");
        assert_eq!(&buf[..len], b"zq");
        let text = transcript(&written);
        assert!(!text.contains('z') && !text.contains('q'), "{text}");
        // The diff repaints only the changed cells, so the shared trailing
        // colon may be kept from the username prompt.
        assert!(text.contains("Password"), "{text}");
    }

    #[test]
    fn note_failure_renders_the_running_count_in_red() {
        let (view, written, _mode) = view_with(&[]);
        view.note_failure();
        view.note_failure();
        let text = transcript(&written);
        // The first draw paints the whole line; the second diff repaints
        // only the changed count, so assert the stable prefixes.
        assert!(text.contains("1 failed attempt"), "{text}");
        assert!(text.contains("2 failed"), "{text}");
        // The count is painted in the ANSI red foreground.
        assert!(text.contains("[31m"), "{text}");
    }

    #[test]
    fn a_closed_console_fails_closed() {
        let (view, _written, _mode) = view_with(&[]);
        let mut buf = [0u8; 8];
        assert_eq!(view.read_username(&mut buf), Err(Errno::NotFound));
    }

    #[test]
    fn an_over_long_line_is_refused_whole() {
        // One keystroke past the buffer bound refuses the line — the
        // bound is a validation bound, never a silent truncation.
        let (view, _written, _mode) = view_with(&[b"abcdefghi", b"\r"]);
        let mut buf = [0u8; 8];
        assert_eq!(view.read_username(&mut buf), Err(Errno::LengthOutOfRange));
    }

    #[test]
    fn handoff_restores_the_cooked_console_and_resets_failures() {
        let (view, written, mode) = view_with(&[]);
        view.round_begin();
        view.note_failure();
        view.session_handoff();
        assert!(mode.cookeds.get() >= 1);
        // The next round starts with a clean slate: no failure line.
        written.borrow_mut().clear();
        view.round_begin();
        assert!(!transcript(&written).contains("failed attempt"));
    }
}
