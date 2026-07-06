//! The parsed shape of a `top` command line.

use crate::error::TopError;

/// The usage banner a usage error is reported with, and the fallback the
/// short-help switches print when `top`'s own Help tree is unavailable.
pub const USAGE: &str = "usage: top [-d secs.tenths] [-h | -?]";

/// The default refresh delay in tenths of a second: GNU `top`'s 3.0 s.
pub const DEFAULT_DELAY_TENTHS: u32 = 30;

/// The smallest accepted refresh delay, in tenths of a second.
///
/// GNU `top` accepts a zero delay and spins as fast as it can; RustOS never
/// busy-loops, so a requested delay below one tenth of a second is clamped
/// to it — a deliberate, documented divergence in the tool's Help.
pub const MIN_DELAY_TENTHS: u32 = 1;

/// One thing the `top` tool can do.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Command {
    /// Run the live process viewer, auto-refreshing every `delay_tenths`
    /// tenths of a second (always at least [`MIN_DELAY_TENTHS`]).
    Run {
        /// The refresh interval in tenths of a second.
        delay_tenths: u32,
    },
    /// Render `top`'s own short help (`-h`/`-?`/`--help`): the `NAME`,
    /// `SYNOPSIS`, and compact `OPTIONS` of its Help document, through the
    /// same engine as any other command's short help (plans/APPS.md §4).
    Help,
}

/// Parse `args` (the tool's arguments, excluding the program name) into a
/// [`Command`].
///
/// The grammar is `top [-d secs.tenths] [-h | -?]`:
///
/// * `-h` / `-?` / `--help` — the reserved short-help switches
///   (plans/APPS.md §4; they win immediately).
/// * `-d secs.tenths` / `-dsecs.tenths` / `--delay secs.tenths` /
///   `--delay=secs.tenths` — the refresh interval between automatic
///   redisplays, exactly GNU `top`'s `-d, --delay` spellings. Seconds with
///   an optional fraction; only the first fractional digit (tenths) is
///   kept, as in GNU `top`. A zero delay is clamped to the 0.1 s minimum
///   rather than busy-looping.
/// * anything else — a [`TopError::Usage`] error. The viewer takes no
///   operands; its scope and navigation are keys pressed inside the
///   session.
///
/// # Errors
///
/// [`TopError::Usage`] for any input outside the grammar above, including a
/// missing or malformed delay value.
pub fn parse(args: &[&str]) -> Result<Command, TopError> {
    let mut delay_tenths = DEFAULT_DELAY_TENTHS;
    let mut rest = args;
    while let Some((&arg, tail)) = rest.split_first() {
        match arg {
            "-h" | "-?" | "--help" => return Ok(Command::Help),
            "-d" | "--delay" => {
                let (&value, tail) = tail.split_first().ok_or(TopError::Usage)?;
                delay_tenths = parse_delay_tenths(value)?;
                rest = tail;
            }
            _ => {
                let value = if let Some(attached) = arg.strip_prefix("--delay=") {
                    attached
                } else if let Some(attached) = arg.strip_prefix("-d") {
                    attached
                } else {
                    return Err(TopError::Usage);
                };
                delay_tenths = parse_delay_tenths(value)?;
                rest = tail;
            }
        }
    }
    Ok(Command::Run { delay_tenths })
}

/// Parse a `secs[.tenths…]` delay into whole tenths of a second, keeping
/// only the first fractional digit (GNU `top` keeps tenths) and clamping a
/// zero up to [`MIN_DELAY_TENTHS`] so the viewer never busy-loops.
///
/// # Errors
///
/// [`TopError::Usage`] when the value is empty, carries a non-digit, has
/// more than one decimal point, or overflows the tenths counter.
fn parse_delay_tenths(value: &str) -> Result<u32, TopError> {
    let (whole, fraction) = match value.split_once('.') {
        Some((whole, fraction)) => (whole, fraction),
        None => (value, ""),
    };
    if whole.is_empty() && fraction.is_empty() {
        return Err(TopError::Usage);
    }
    if !whole.bytes().all(|b| b.is_ascii_digit()) || !fraction.bytes().all(|b| b.is_ascii_digit()) {
        return Err(TopError::Usage);
    }
    let seconds: u32 = if whole.is_empty() {
        0
    } else {
        whole.parse().map_err(|_| TopError::Usage)?
    };
    let tenth = fraction
        .bytes()
        .next()
        .map_or(0, |digit| u32::from(digit - b'0'));
    let tenths = seconds
        .checked_mul(10)
        .and_then(|t| t.checked_add(tenth))
        .ok_or(TopError::Usage)?;
    Ok(tenths.max(MIN_DELAY_TENTHS))
}

#[cfg(test)]
mod tests {
    use super::{parse, Command, DEFAULT_DELAY_TENTHS, MIN_DELAY_TENTHS};
    use crate::error::TopError;

    fn run(delay_tenths: u32) -> Command {
        Command::Run { delay_tenths }
    }

    #[test]
    fn no_arguments_runs_the_viewer_at_the_default_delay() {
        assert_eq!(parse(&[]), Ok(run(DEFAULT_DELAY_TENTHS)));
    }

    #[test]
    fn help_flags_win() {
        assert_eq!(parse(&["-h"]), Ok(Command::Help));
        assert_eq!(parse(&["-?"]), Ok(Command::Help));
        assert_eq!(parse(&["--help"]), Ok(Command::Help));
        // Help wins wherever it appears, exactly as GNU tools treat it.
        assert_eq!(parse(&["-d", "1", "-h"]), Ok(Command::Help));
    }

    #[test]
    fn delay_accepts_the_gnu_spellings() {
        assert_eq!(parse(&["-d", "5"]), Ok(run(50)));
        assert_eq!(parse(&["-d5"]), Ok(run(50)));
        assert_eq!(parse(&["--delay", "5"]), Ok(run(50)));
        assert_eq!(parse(&["--delay=5"]), Ok(run(50)));
    }

    #[test]
    fn delay_keeps_only_the_first_fractional_digit() {
        assert_eq!(parse(&["-d", "1.5"]), Ok(run(15)));
        assert_eq!(parse(&["-d", "1.59"]), Ok(run(15)));
        assert_eq!(parse(&["-d", ".5"]), Ok(run(5)));
        assert_eq!(parse(&["-d", "2."]), Ok(run(20)));
    }

    #[test]
    fn a_zero_delay_is_clamped_never_a_busy_loop() {
        assert_eq!(parse(&["-d", "0"]), Ok(run(MIN_DELAY_TENTHS)));
        assert_eq!(parse(&["-d", "0.0"]), Ok(run(MIN_DELAY_TENTHS)));
    }

    #[test]
    fn the_last_delay_wins() {
        assert_eq!(parse(&["-d", "1", "-d", "2"]), Ok(run(20)));
    }

    #[test]
    fn malformed_delays_are_usage_errors() {
        assert_eq!(parse(&["-d"]), Err(TopError::Usage));
        assert_eq!(parse(&["-d", ""]), Err(TopError::Usage));
        assert_eq!(parse(&["-d", "."]), Err(TopError::Usage));
        assert_eq!(parse(&["-d", "abc"]), Err(TopError::Usage));
        assert_eq!(parse(&["-d", "1.2.3"]), Err(TopError::Usage));
        assert_eq!(parse(&["-d", "-1"]), Err(TopError::Usage));
        assert_eq!(parse(&["-d", "99999999999"]), Err(TopError::Usage));
    }

    #[test]
    fn any_other_argument_is_usage() {
        assert_eq!(parse(&["-x"]), Err(TopError::Usage));
        assert_eq!(parse(&["--frob"]), Err(TopError::Usage));
        assert_eq!(parse(&["operand"]), Err(TopError::Usage));
    }

    /// Every locale's `OPTIONS` section documents exactly the switches this
    /// parser accepts (`plans/APPS.md` §3.1): the flag tokens are
    /// language-neutral, so each translated document must carry the same
    /// keys as the canonical one. The documents are read from the bundle's
    /// own on-disk `Help/` tree — the single source the image builder plants
    /// — never a copy embedded in this crate.
    #[test]
    fn help_documents_the_parser_switches() {
        extern crate std;
        use alloc::format;
        use std::fs;

        let help_root = format!("{}/Help", env!("CARGO_MANIFEST_DIR"));
        let locales = ["default", "fr-FR", "de-DE", "es-ES", "uk-UA", "it-IT"];
        for locale in locales {
            let path = format!("{help_root}/{locale}/top.md");
            let text = fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {path}: {e}"));
            for switch in ["`-d, --delay <seconds>`", "`-h, -?`"] {
                assert!(
                    text.contains(switch),
                    "{locale}/top.md must document {switch}"
                );
            }
        }
    }
}
