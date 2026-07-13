//! The parsed shape of a `sysmon` command line.

use crate::error::SysmonError;

/// The usage banner a usage error is reported with, and the fallback the
/// short-help switches print when `sysmon`'s own Help tree is unavailable.
pub const USAGE: &str = "usage: sysmon [-d secs.tenths] [-h | -?]";

/// The default refresh delay in tenths of a second: `top`'s 3.0 s, the
/// familiar monitoring cadence.
pub const DEFAULT_DELAY_TENTHS: u32 = 30;

/// The smallest accepted refresh delay, in tenths of a second.
///
/// GNU `top` accepts a zero delay and spins as fast as it can; RustOS never
/// busy-loops, so a requested delay below one tenth of a second is clamped
/// to it — a deliberate, documented divergence in the tool's Help. The
/// grammar (and this clamp) is the shared full-screen-viewer delay grammar
/// in `lib/curses`, so `top` and `sysmon` can never drift apart.
pub use rustos_curses::MIN_DELAY_TENTHS;

/// The largest refresh delay the in-session `+` key can reach, in tenths
/// of a second (60 s). Bounds the interval so a stray key can never park
/// the display for minutes; the same bound clamps a large `-d` value's
/// in-session adjustment upward, not the parsed option itself.
pub const MAX_DELAY_TENTHS: u32 = 600;

/// The in-session `+`/`-` adjustment step, in tenths of a second (1 s).
pub const DELAY_STEP_TENTHS: u32 = 10;

/// One thing the `sysmon` tool can do.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Command {
    /// Run the live monitor, auto-refreshing every `delay_tenths` tenths of
    /// a second (always at least [`MIN_DELAY_TENTHS`]).
    Run {
        /// The refresh interval in tenths of a second.
        delay_tenths: u32,
    },
    /// Render `sysmon`'s own short help (`-h`/`-?`/`--help`): the `NAME`,
    /// `SYNOPSIS`, and compact `OPTIONS` of its Help document, through the
    /// same engine as any other command's short help (plans/APPS.md §4).
    Help,
}

/// Parse `args` (the tool's arguments, excluding the program name) into a
/// [`Command`].
///
/// The grammar is `sysmon [-d secs.tenths] [-h | -?]`:
///
/// * `-h` / `-?` / `--help` — the reserved short-help switches
///   (plans/APPS.md §4; they win immediately).
/// * `-d secs.tenths` / `-dsecs.tenths` / `--delay secs.tenths` /
///   `--delay=secs.tenths` — the refresh interval between automatic
///   redisplays, exactly GNU `top`'s `-d, --delay` spellings, parsed by the
///   shared full-screen-viewer grammar. A zero delay is clamped to the
///   0.1 s minimum rather than busy-looping.
/// * anything else — a [`SysmonError::Usage`] error. The monitor takes no
///   operands; its panels and interval are keys pressed inside the session.
///
/// # Errors
///
/// [`SysmonError::Usage`] for any input outside the grammar above,
/// including a missing or malformed delay value.
pub fn parse(args: &[&str]) -> Result<Command, SysmonError> {
    let mut delay_tenths = DEFAULT_DELAY_TENTHS;
    let mut rest = args;
    while let Some((&arg, tail)) = rest.split_first() {
        match arg {
            "-h" | "-?" | "--help" => return Ok(Command::Help),
            "-d" | "--delay" => {
                let (&value, tail) = tail.split_first().ok_or(SysmonError::Usage)?;
                delay_tenths = parse_delay_tenths(value)?;
                rest = tail;
            }
            _ => {
                let value = if let Some(attached) = arg.strip_prefix("--delay=") {
                    attached
                } else if let Some(attached) = arg.strip_prefix("-d") {
                    attached
                } else {
                    return Err(SysmonError::Usage);
                };
                delay_tenths = parse_delay_tenths(value)?;
                rest = tail;
            }
        }
    }
    Ok(Command::Run { delay_tenths })
}

/// Parse a `secs[.tenths…]` delay through the shared full-screen-viewer
/// grammar ([`rustos_curses::parse_delay_tenths`]), mapping its fail-closed
/// `None` onto this tool's usage error.
///
/// # Errors
///
/// [`SysmonError::Usage`] when the value is empty, carries a non-digit, has
/// more than one decimal point, or overflows the tenths counter.
fn parse_delay_tenths(value: &str) -> Result<u32, SysmonError> {
    rustos_curses::parse_delay_tenths(value).ok_or(SysmonError::Usage)
}
