//! TAIRiX `sleep` — pause for a sum of time intervals
//! (`plans/APPS.md` §12.1 Stage C).
//!
//! The GNU coreutils `sleep`: it pauses for the *sum* of the given
//! `NUMBER[SUFFIX]` intervals and then exits. `SUFFIX` is `s` (seconds,
//! the default), `m` (minutes), `h` (hours), or `d` (days), and `NUMBER`
//! is any C-locale floating-point value — a decimal, a hexadecimal float,
//! or `inf`/`infinity` — matching GNU's `xstrtod`. Several operands are
//! summed (`sleep 1m 30s` pauses ninety seconds), a negative or `nan`
//! value is an invalid interval, and `sleep inf` pauses until the process
//! is killed.
//!
//! # What this crate is
//!
//! The pure, host-testable core of the tool:
//!
//! * [`parse`] — the [`Command`] a command line names, with GNU `sleep`'s
//!   option rules: the only options are the reserved short-help switches
//!   (plans/APPS.md §4), and — like GNU getopt — an option token is
//!   diagnosed wherever it sits, so `sleep 1 --help` serves the help and
//!   `sleep 1 -x` diagnoses `-x`. Every operand is parsed as an interval
//!   through the shared [`tairix_util::cnum::scan_double`] scanner, so the
//!   C number grammar lives in exactly one place (§2.2).
//! * [`run`] — the engine: render the short help, or pause for the summed
//!   interval through the injected [`Sleeper`] seam. The seam keeps the
//!   engine pure and makes the real, off-CPU park a production detail the
//!   host tests never have to perform.
//!
//! # No busy-waiting
//!
//! The production [`Sleeper`] (in the `Run` binary) parks the task
//! off-CPU on the runtime's timed wait — it never spins a core waiting for
//! the clock (`AGENTS.md` §2.23). `sleep inf` re-parks in bounded chunks
//! forever rather than looping on a clock read.
//!
//! # Layering & safety
//!
//! `no_std` (with `alloc`); the only dependencies are the audited `lib/abi`
//! ABI crate, the shared `lib/util` number scanner, and the shared
//! `lib/help` engine, so this userland tool never links a kernel or driver
//! crate. No `unsafe`, and no `unwrap`/`expect`/`panic!` in production
//! paths; nothing writes to fd 3 (`stdinfo`) — pausing omits, summarises,
//! and hides nothing from `stdout`.
//!
//! The bundle's `Help/` documents are **not** embedded in this crate: they
//! are authored once in the bundle's on-disk `Help/` tree, planted onto
//! `/System` by the image builder from that source (`tools/syshelp`), and
//! read back at runtime through the injected [`tairix_help::HelpSource`]
//! seam. Help is never hardcoded into the program (`plans/APPS.md`).

#![no_std]
#![forbid(unsafe_code)]
#![deny(missing_docs)]

extern crate alloc;

use alloc::string::String;
use core::fmt;

use tairix_abi::Errno;
use tairix_help::{own_short_help, HelpSource};
use tairix_util::cnum::scan_double;

/// The usage banner a usage error is reported with, and the fallback the
/// short-help switches print when `sleep`'s own Help tree is unavailable.
pub const USAGE: &str = "\
usage: sleep NUMBER[SUFFIX]...

  pause for the sum of the given intervals; SUFFIX is s, m, h, or d
  -h, -?      show this help";

/// `sleep`'s own command word: the short-help switches render its own
/// Help document through the same engine as any other command's.
const OWN_WORD: &str = "sleep";

/// Seconds in one minute — the `m` suffix multiplier.
const SECONDS_PER_MINUTE: f64 = 60.0;
/// Seconds in one hour — the `h` suffix multiplier.
const SECONDS_PER_HOUR: f64 = 60.0 * 60.0;
/// Seconds in one day — the `d` suffix multiplier.
const SECONDS_PER_DAY: f64 = 60.0 * 60.0 * 24.0;

/// One thing the `sleep` tool can do.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Command {
    /// Pause for `seconds` (a non-negative, possibly infinite total of the
    /// summed operand intervals) and then exit.
    Sleep {
        /// The total interval to pause for, in seconds. Always `>= 0`, and
        /// [`f64::INFINITY`] for `sleep inf`.
        seconds: f64,
    },
    /// Render `sleep`'s own short help (`-h`/`-?`/`--help`): the `NAME`,
    /// `SYNOPSIS`, and compact `OPTIONS` of its Help document, through the
    /// same engine as any other command's short help (plans/APPS.md §4).
    Help,
}

/// The failures the `sleep` tool reports.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SleepError {
    /// An unrecognised `--long` option (GNU `unrecognized option`).
    UnknownLong(String),
    /// An unrecognised short option (GNU `invalid option`); carries the
    /// offending flag character.
    UnknownShort(char),
    /// No operand was given; `sleep` needs at least one (GNU
    /// `missing operand`).
    MissingOperand,
    /// An operand did not name a non-negative interval (GNU
    /// `invalid time interval`). Carries the offending operand verbatim.
    InvalidInterval(String),
    /// Writing the short help to the terminal failed. Carries the
    /// underlying [`Errno`].
    Output(Errno),
}

impl fmt::Display for SleepError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownLong(token) => write!(f, "unrecognized option '{token}'"),
            Self::UnknownShort(flag) => write!(f, "invalid option -- '{flag}'"),
            Self::MissingOperand => f.write_str("missing operand"),
            Self::InvalidInterval(operand) => write!(f, "invalid time interval '{operand}'"),
            Self::Output(errno) => write!(f, "terminal write failed: {errno}"),
        }
    }
}

/// The off-CPU pause the [`run`] engine performs for a [`Command::Sleep`].
///
/// The production implementation (in the `Run` binary) parks the task on
/// the runtime's timed wait so the CPU sleeps rather than spins
/// (`AGENTS.md` §2.23); tests inject a recorder that captures the requested
/// interval without waiting. The seam keeps the engine pure and its tests
/// instant.
pub trait Sleeper {
    /// Pause for `seconds` (guaranteed `>= 0`; [`f64::INFINITY`] means
    /// pause until the process is killed) and then return.
    fn sleep(&self, seconds: f64);
}

/// The terminal sink the short help is written to.
///
/// The production implementation writes the inherited standard output
/// (fd 1); tests capture the lines. A single injected seam keeps the engine
/// free of ambient I/O and makes "the terminal refused the write" an
/// ordinary, testable failure.
pub trait Output {
    /// Write one line (the callee appends the terminating newline).
    ///
    /// # Errors
    ///
    /// Any [`Errno`] the underlying write raises.
    fn write_line(&self, line: &str) -> Result<(), Errno>;
}

/// Parse `args` (the tool's arguments, excluding the program name) into a
/// [`Command`].
///
/// The grammar is the GNU `sleep` surface — one or more `NUMBER[SUFFIX]`
/// operands, and no options beyond the reserved short-help switches
/// (plans/APPS.md §4):
///
/// * `-h` / `-?` / `--help` — short help; like GNU getopt's permutation, an
///   option is honoured wherever it sits, so `sleep 1 --help` serves the
///   help.
/// * `--` — end option parsing; every later argument is an operand.
/// * any other option-shaped token — [`SleepError::UnknownLong`] /
///   [`SleepError::UnknownShort`], diagnosed left to right exactly as GNU
///   getopt reports the first bad option before it ever looks at operands.
/// * each operand — a C-locale floating-point number
///   ([`tairix_util::cnum::scan_double`]) followed by an optional single
///   `s`/`m`/`h`/`d` suffix; a negative value, a `nan`, trailing junk, or an
///   unknown suffix is [`SleepError::InvalidInterval`]. The intervals are
///   summed. No operand at all is [`SleepError::MissingOperand`].
///
/// # Errors
///
/// The GNU diagnostics above, in getopt's order: the first bad option
/// anywhere wins over the operand checks, then a missing operand, then the
/// first invalid interval left to right.
pub fn parse(args: &[&str]) -> Result<Command, SleepError> {
    let mut options_ended = false;
    let mut first_operand: Option<&str> = None;
    let mut seconds = 0.0_f64;
    let mut invalid: Option<&str> = None;

    for &arg in args {
        if options_ended {
            note_operand(arg, &mut first_operand, &mut seconds, &mut invalid);
            continue;
        }
        match arg {
            "--" => options_ended = true,
            "-h" | "-?" | "--help" => return Ok(Command::Help),
            _ if arg.starts_with("--") => {
                return Err(SleepError::UnknownLong(String::from(arg)));
            }
            _ if arg.len() > 1 && arg.starts_with('-') => {
                // The first flag character of the cluster, exactly as getopt
                // reports it. `sleep` has no short options, so only the exact
                // reserved spellings above are ever honoured.
                let flag = arg.chars().nth(1).unwrap_or('-');
                return Err(SleepError::UnknownShort(flag));
            }
            _ => note_operand(arg, &mut first_operand, &mut seconds, &mut invalid),
        }
    }

    if first_operand.is_none() {
        return Err(SleepError::MissingOperand);
    }
    if let Some(operand) = invalid {
        return Err(SleepError::InvalidInterval(String::from(operand)));
    }
    Ok(Command::Sleep { seconds })
}

/// Fold one operand into the running interval sum, remembering that at least
/// one operand was seen and the first invalid one (GNU reports operand
/// errors after option errors, and we report the first invalid interval).
fn note_operand<'a>(
    operand: &'a str,
    first_operand: &mut Option<&'a str>,
    seconds: &mut f64,
    invalid: &mut Option<&'a str>,
) {
    first_operand.get_or_insert(operand);
    match parse_interval(operand) {
        Some(value) => *seconds += value,
        None => {
            invalid.get_or_insert(operand);
        }
    }
}

/// Parse one `NUMBER[SUFFIX]` operand into a non-negative number of seconds,
/// or `None` when it is not a valid time interval.
///
/// Mirrors GNU `sleep`'s `xstrtod` + `apply_suffix` check exactly: the
/// number is scanned by the shared C-locale scanner, must be non-negative
/// (so a negative value and `nan` are rejected), and may be followed by at
/// most one suffix character, which must be `s`, `m`, `h`, or `d`. `inf`
/// scales to an infinite interval.
fn parse_interval(operand: &str) -> Option<f64> {
    let (value, consumed) = scan_double(operand)?;
    // Non-negative only: a negative value and `nan` are both rejected,
    // matching GNU's `! (0 <= s)` (a `nan` fails the `0 <= s` test).
    if value < 0.0 || value.is_nan() {
        return None;
    }
    let rest = &operand[consumed..];
    let multiplier = match rest.as_bytes() {
        // No suffix, or the explicit seconds suffix.
        [] | [b's'] => 1.0,
        [b'm'] => SECONDS_PER_MINUTE,
        [b'h'] => SECONDS_PER_HOUR,
        [b'd'] => SECONDS_PER_DAY,
        // More than one trailing byte (GNU `*p && *(p+1)`), or an unknown
        // single suffix, is not a valid interval.
        _ => return None,
    };
    Some(value * multiplier)
}

/// Run one [`Command`]: render the short help through `help`/`out`, or pause
/// for the summed interval through `sleeper`. `locale` is the user's `LANG`
/// preference, if set; `help` is the tool's own `Help/` tree, read by the
/// short-help switches.
///
/// Pausing itself cannot fail (the timed park always completes), so the only
/// error path is a failed short-help write.
///
/// # Errors
///
/// * [`SleepError::Output`] — writing the short help to the terminal failed.
pub fn run(
    command: Command,
    locale: Option<&str>,
    sleeper: &dyn Sleeper,
    help: &dyn HelpSource,
    out: &dyn Output,
) -> Result<(), SleepError> {
    match command {
        Command::Help => short_help(locale, help, out),
        Command::Sleep { seconds } => {
            sleeper.sleep(seconds);
            Ok(())
        }
    }
}

/// Render `sleep`'s own short help (`NAME` + `SYNOPSIS` + compact `OPTIONS`)
/// from its own bundle's `Help/` tree through the one shared engine; when no
/// document can be served (a build without the bundle's documents) the usage
/// banner stands in — the tool's own text, not fabricated help content — so
/// `-h` never fails. The rendered page is written as one multi-line
/// `write_line`; the seam owns the final newline.
fn short_help(
    locale: Option<&str>,
    help: &dyn HelpSource,
    out: &dyn Output,
) -> Result<(), SleepError> {
    let bytes = own_short_help(help, locale, OWN_WORD);
    let text = bytes
        .as_deref()
        .and_then(|bytes| core::str::from_utf8(bytes).ok())
        .unwrap_or(USAGE);
    out.write_line(text.trim_end_matches('\n'))
        .map_err(SleepError::Output)
}

#[cfg(test)]
mod tests {
    extern crate std;

    use alloc::format;
    use alloc::string::{String, ToString};
    use alloc::vec::Vec;
    use core::cell::{Cell, RefCell};

    use tairix_abi::Errno;
    use tairix_help::{HelpSource, SourceError};

    use super::{parse, run, Command, Output, SleepError, Sleeper, USAGE};

    /// A Help tree with no documents at all: the short-help fallback path.
    struct NoHelp;

    impl HelpSource for NoHelp {
        fn locale_dirs(&self) -> Result<Vec<String>, SourceError> {
            Ok(Vec::new())
        }

        fn read(
            &self,
            _locale_dir: &str,
            _file_name: &str,
        ) -> Result<Option<Vec<u8>>, SourceError> {
            Ok(None)
        }
    }

    /// Records the last interval it was asked to sleep for, without waiting.
    #[derive(Default)]
    struct Recorder {
        seconds: Cell<Option<f64>>,
    }

    impl Sleeper for Recorder {
        fn sleep(&self, seconds: f64) {
            self.seconds.set(Some(seconds));
        }
    }

    /// Captures written lines; optionally refuses every write.
    struct Sink {
        lines: RefCell<Vec<String>>,
        closed: bool,
    }

    impl Sink {
        fn new() -> Self {
            Self {
                lines: RefCell::new(Vec::new()),
                closed: false,
            }
        }
    }

    impl Output for Sink {
        fn write_line(&self, line: &str) -> Result<(), Errno> {
            if self.closed {
                return Err(Errno::NotFound);
            }
            self.lines.borrow_mut().push(line.to_string());
            Ok(())
        }
    }

    /// Assert `parse(args)` names a finite sleep of exactly `seconds`. The
    /// comparison is on the whole [`Command`] value (not a bare `f64`), so
    /// the intended interval is pinned exactly without an `f64 ==` in the
    /// test body; every expected value here is an integer exactly
    /// representable in `f64`.
    fn assert_sleeps_for(args: &[&str], seconds: f64) {
        assert_eq!(parse(args), Ok(Command::Sleep { seconds }));
    }

    /// The interval a valid sleep command names, or a panic if `args` did
    /// not parse to a finite sleep.
    fn interval_of(args: &[&str]) -> f64 {
        match parse(args) {
            Ok(Command::Sleep { seconds }) => seconds,
            other => panic!("expected a sleep command, got {other:?}"),
        }
    }

    #[test]
    fn parse_sums_the_intervals_with_suffixes() {
        assert_sleeps_for(&["5"], 5.0);
        assert_sleeps_for(&["5s"], 5.0);
        assert_sleeps_for(&["2m"], 120.0);
        assert_sleeps_for(&["1h"], 3600.0);
        assert_sleeps_for(&["1d"], 86400.0);
        // Several operands are summed (GNU `sleep 1m 30s`).
        assert_sleeps_for(&["1m", "30s"], 90.0);
        // A fractional value with a suffix.
        assert_sleeps_for(&["1.5m"], 90.0);
    }

    #[test]
    fn parse_accepts_infinity() {
        assert!(interval_of(&["inf"]).is_infinite());
        assert!(interval_of(&["infinity"]).is_infinite());
        // A suffix on infinity is still infinite, and the sum stays
        // infinite.
        assert!(interval_of(&["infd", "5"]).is_infinite());
    }

    #[test]
    fn parse_names_the_help_switches() {
        assert_eq!(parse(&["-h"]), Ok(Command::Help));
        assert_eq!(parse(&["-?"]), Ok(Command::Help));
        assert_eq!(parse(&["--help"]), Ok(Command::Help));
        // getopt permutation: an option is honoured after an operand.
        assert_eq!(parse(&["1", "--help"]), Ok(Command::Help));
    }

    #[test]
    fn parse_requires_an_operand() {
        assert_eq!(parse(&[]), Err(SleepError::MissingOperand));
        // After `--` there is still no operand.
        assert_eq!(parse(&["--"]), Err(SleepError::MissingOperand));
    }

    #[test]
    fn parse_rejects_invalid_intervals() {
        assert_eq!(
            parse(&["x"]),
            Err(SleepError::InvalidInterval(String::from("x")))
        );
        // A negative value (an operand after `--`) is not a valid interval.
        assert_eq!(
            parse(&["--", "-5"]),
            Err(SleepError::InvalidInterval(String::from("-5")))
        );
        // An unknown suffix.
        assert_eq!(
            parse(&["5x"]),
            Err(SleepError::InvalidInterval(String::from("5x")))
        );
        // More than one trailing byte after the number (GNU `*p && *(p+1)`).
        assert_eq!(
            parse(&["5sm"]),
            Err(SleepError::InvalidInterval(String::from("5sm")))
        );
        // `nan` is rejected by the non-negative check.
        assert_eq!(
            parse(&["nan"]),
            Err(SleepError::InvalidInterval(String::from("nan")))
        );
        // The first invalid interval, left to right, is reported.
        assert_eq!(
            parse(&["1", "bad", "worse"]),
            Err(SleepError::InvalidInterval(String::from("bad")))
        );
    }

    #[test]
    fn parse_diagnoses_options_before_operands() {
        assert_eq!(
            parse(&["--bogus"]),
            Err(SleepError::UnknownLong(String::from("--bogus")))
        );
        assert_eq!(parse(&["-x"]), Err(SleepError::UnknownShort('x')));
        // A cluster diagnoses its first flag.
        assert_eq!(parse(&["-hx"]), Err(SleepError::UnknownShort('h')));
        // An option token wins over operand processing wherever it sits.
        assert_eq!(parse(&["1", "-x"]), Err(SleepError::UnknownShort('x')));
        // But after `--`, a `-x` is an (invalid) operand, not an option.
        assert_eq!(
            parse(&["--", "-x"]),
            Err(SleepError::InvalidInterval(String::from("-x")))
        );
    }

    #[test]
    fn run_sleeps_for_the_parsed_interval() {
        let recorder = Recorder::default();
        let out = Sink::new();
        run(
            Command::Sleep { seconds: 42.0 },
            None,
            &recorder,
            &NoHelp,
            &out,
        )
        .expect("runs");
        assert_eq!(recorder.seconds.get(), Some(42.0));
        assert!(out.lines.borrow().is_empty());
    }

    #[test]
    fn run_serves_short_help_without_sleeping() {
        let recorder = Recorder::default();
        let out = Sink::new();
        run(Command::Help, None, &recorder, &NoHelp, &out).expect("help served");
        assert_eq!(recorder.seconds.get(), None);
        assert_eq!(&*out.lines.borrow(), &[USAGE]);
    }

    #[test]
    fn a_closed_terminal_is_an_output_error() {
        let recorder = Recorder::default();
        let out = Sink {
            lines: RefCell::new(Vec::new()),
            closed: true,
        };
        let outcome = run(Command::Help, None, &recorder, &NoHelp, &out);
        assert_eq!(outcome, Err(SleepError::Output(Errno::NotFound)));
    }

    #[test]
    fn error_messages_match_the_gnu_wording() {
        assert_eq!(format!("{}", SleepError::MissingOperand), "missing operand");
        assert_eq!(
            format!("{}", SleepError::InvalidInterval(String::from("x"))),
            "invalid time interval 'x'"
        );
        assert_eq!(
            format!("{}", SleepError::UnknownLong(String::from("--bogus"))),
            "unrecognized option '--bogus'"
        );
        assert_eq!(
            format!("{}", SleepError::UnknownShort('x')),
            "invalid option -- 'x'"
        );
    }

    /// Every locale's `OPTIONS` section documents exactly the switches this
    /// parser accepts (`plans/APPS.md` §3.1): the flag tokens are
    /// language-neutral, so each translated document must carry the same
    /// keys as the canonical one. The documents are read from the bundle's
    /// own on-disk `Help/` tree — the single source the image builder
    /// plants — never a copy embedded in this crate.
    #[test]
    fn help_documents_the_parser_switches() {
        use std::fs;

        let help_root = format!("{}/Help", env!("CARGO_MANIFEST_DIR"));
        for locale in tairix_help::REQUIRED_LOCALES {
            let path = format!("{help_root}/{locale}/sleep.md");
            let text = fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {path}: {e}"));
            let switch = "`-h, -?`";
            assert!(
                text.contains(switch),
                "{locale}/sleep.md must document {switch}"
            );
        }
    }
}
