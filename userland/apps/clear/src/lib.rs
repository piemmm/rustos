//! RustOS `clear` — clear the terminal screen (`plans/APPS.md`).
//!
//! The RustOS counterpart of the ncurses `clear` tool: it writes the byte
//! sequence that moves the cursor home and erases the display, and nothing
//! else. Which bytes those are is decided by the inherited `TERM` through
//! the compiled-in capability database (`lib/termcap`), and the sequence is
//! encoded through the one shared escape vocabulary (`lib/vt`) — never a
//! hand-rolled escape string.
//!
//! # What this crate is
//!
//! The pure, host-testable core of the tool:
//!
//! * [`parse`] — the [`Command`] a command line names.
//! * [`clear_bytes`] — the encoded clear sequence for a capability profile,
//!   or `None` when the terminal cannot clear (the dumb baseline), so the
//!   caller fails honestly instead of emitting sequences the terminal does
//!   not understand.
//!
//! # Divergence from ncurses `clear`
//!
//! GNU/ncurses `clear -x` skips the scrollback-erase (`E3`) extension. A
//! RustOS console keeps no scrollback, so there is nothing to skip: `-x` is
//! accepted for script compatibility and the output is identical with and
//! without it. Documented in the tool's `Help/` documents.
//!
//! # Layering & safety
//!
//! `no_std` (with `alloc`). It links only `lib/*` crates — `lib/termcap`
//! and `lib/vt` — never a kernel or driver crate. No `unsafe`, and no
//! `unwrap`/`expect`/`panic!` in production paths; nothing writes to fd 3
//! (`stdinfo`).

#![no_std]
#![forbid(unsafe_code)]
#![deny(missing_docs)]

extern crate alloc;

use alloc::vec::Vec;

use rustos_termcap::Capabilities;
use rustos_vt::{encode_all_into, EraseMode, Op};

/// The usage banner a usage error is reported with, and the fallback the
/// short-help switches print when `clear`'s own Help tree is unavailable.
pub const USAGE: &str = "usage: clear [-x] [-h | -?]";

/// One thing the `clear` tool can do.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Command {
    /// Clear the screen.
    Run,
    /// Render `clear`'s own short help (`-h`/`-?`/`--help`): the `NAME`,
    /// `SYNOPSIS`, and compact `OPTIONS` of its Help document, through the
    /// same engine as any other command's short help (plans/APPS.md).
    Help,
}

/// The one failure [`parse`] reports.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ClearError {
    /// The command line was not understood.
    Usage,
}

/// Parse `args` (the tool's arguments, excluding the program name) into a
/// [`Command`].
///
/// The grammar is `clear [-x] [-h | -?]`:
///
/// * `-h` / `-?` / `--help` — the reserved short-help switches
///   (plans/APPS.md; they win immediately).
/// * `-x` — accepted for GNU/ncurses compatibility ("do not clear the
///   scrollback"); a RustOS console keeps no scrollback, so the behaviour
///   is identical with and without it.
/// * anything else — a [`ClearError::Usage`] error: the tool takes no
///   operands.
///
/// # Errors
///
/// [`ClearError::Usage`] for any input outside the grammar above.
pub fn parse(args: &[&str]) -> Result<Command, ClearError> {
    let mut seen_x = false;
    for arg in args {
        match *arg {
            "-h" | "-?" | "--help" => return Ok(Command::Help),
            "-x" if !seen_x => seen_x = true,
            _ => return Err(ClearError::Usage),
        }
    }
    Ok(Command::Run)
}

/// The encoded clear sequence for the capability profile `caps`: move the
/// cursor home, then erase the whole display — the shared `lib/vt`
/// operations the profile's terminal accepts.
///
/// Returns `None` when the terminal can neither address the cursor nor
/// erase (the fail-closed dumb baseline an unknown `TERM` degrades to):
/// there is no sequence such a terminal understands, so the caller reports
/// the failure honestly rather than writing bytes that would print as
/// garbage.
#[must_use]
pub fn clear_bytes(caps: &Capabilities) -> Option<Vec<u8>> {
    if !(caps.cursor_addressing && caps.erase) {
        return None;
    }
    let mut bytes = Vec::new();
    encode_all_into(
        &[
            Op::CursorPosition { row: 1, col: 1 },
            Op::EraseInDisplay(EraseMode::All),
        ],
        &mut bytes,
    );
    Some(bytes)
}

#[cfg(test)]
mod tests {
    extern crate std;

    use rustos_termcap::TermType;

    use super::{clear_bytes, parse, ClearError, Command};

    #[test]
    fn no_arguments_clears() {
        assert_eq!(parse(&[]), Ok(Command::Run));
    }

    #[test]
    fn dash_x_is_accepted_once() {
        // `-x` is the GNU "keep the scrollback" switch; a RustOS console
        // keeps none, so it parses to the same command.
        assert_eq!(parse(&["-x"]), Ok(Command::Run));
        // A repeated `-x` is not a valid GNU `clear` line either.
        assert_eq!(parse(&["-x", "-x"]), Err(ClearError::Usage));
    }

    #[test]
    fn help_flags_win() {
        assert_eq!(parse(&["-h"]), Ok(Command::Help));
        assert_eq!(parse(&["-?"]), Ok(Command::Help));
        assert_eq!(parse(&["--help"]), Ok(Command::Help));
        assert_eq!(parse(&["-x", "-h"]), Ok(Command::Help));
    }

    #[test]
    fn any_other_argument_is_usage() {
        assert_eq!(parse(&["--frob"]), Err(ClearError::Usage));
        assert_eq!(parse(&["operand"]), Err(ClearError::Usage));
    }

    #[test]
    fn clear_bytes_emits_home_then_erase_on_a_capable_terminal() {
        for term in [TermType::Xterm256Color, TermType::Vt100] {
            let bytes = clear_bytes(&term.capabilities()).expect("terminal can clear");
            // `CUP` home then `ED` all: the ncurses `clear` sequence,
            // through the shared vocabulary.
            assert_eq!(bytes, b"\x1b[1;1H\x1b[2J");
        }
    }

    #[test]
    fn clear_bytes_refuses_the_dumb_baseline() {
        // An unknown `TERM` degrades to `Dumb`, which cannot clear: the
        // tool must fail honestly, never print escape garbage.
        assert_eq!(clear_bytes(&TermType::Dumb.capabilities()), None);
    }

    /// Every locale's `OPTIONS` section documents exactly the switches this
    /// parser accepts (`plans/APPS.md`): the flag tokens are
    /// language-neutral, so each translated document must carry the same
    /// keys as the canonical one. The documents are read from the bundle's
    /// own on-disk `Help/` tree — the single source the image builder
    /// plants — never a copy embedded in this crate.
    #[test]
    fn help_documents_the_parser_switches() {
        use alloc::format;
        use std::fs;

        let help_root = format!("{}/Help", env!("CARGO_MANIFEST_DIR"));
        let locales = rustos_help::REQUIRED_LOCALES;
        for locale in locales {
            let path = format!("{help_root}/{locale}/clear.md");
            let text = fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {path}: {e}"));
            for switch in ["`-x`", "`-h, -?`"] {
                assert!(
                    text.contains(switch),
                    "{locale}/clear.md must document {switch}"
                );
            }
        }
    }
}
