//! RustOS `reset` — restore the terminal to a sane state (`plans/APPS.md`).
//!
//! The RustOS counterpart of the ncurses `reset` tool, for the moment a
//! wedged terminal leaves behind: a crashed full-screen program may have
//! parked the console on the alternate screen, hidden the cursor, left a
//! scroll region or colours set, and left the input discipline raw. `reset`
//! writes the restoration sequence and (in its program half) restores the
//! cooked input discipline, so the next prompt behaves normally.
//!
//! Which operations are written is decided by the inherited `TERM` through
//! the compiled-in capability database (`lib/termcap`), and every operation
//! is a `rustos_vt::Op` encoded through the one shared escape vocabulary —
//! never a hand-rolled escape string.
//!
//! # What this crate is
//!
//! The pure, host-testable core of the tool:
//!
//! * [`parse`] — the [`Command`] a command line names.
//! * [`reset_bytes`] — the encoded restoration sequence for a capability
//!   profile. The dumb baseline understands none of the operations, so its
//!   sequence is empty — the program half still restores the input
//!   discipline, which is the part every terminal has.
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
use rustos_vt::{encode_all_into, EraseMode, Op, Sgr};

/// The usage banner a usage error is reported with, and the fallback the
/// short-help switches print when `reset`'s own Help tree is unavailable.
pub const USAGE: &str = "usage: reset [-h | -?]";

/// One thing the `reset` tool can do.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Command {
    /// Restore the terminal to a sane state.
    Run,
    /// Render `reset`'s own short help (`-h`/`-?`/`--help`): the `NAME`,
    /// `SYNOPSIS`, and compact `OPTIONS` of its Help document, through the
    /// same engine as any other command's short help (plans/APPS.md).
    Help,
}

/// The one failure [`parse`] reports.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ResetError {
    /// The command line was not understood.
    Usage,
}

/// Parse `args` (the tool's arguments, excluding the program name) into a
/// [`Command`].
///
/// The grammar is `reset [-h | -?]`:
///
/// * `-h` / `-?` / `--help` — the reserved short-help switches
///   (plans/APPS.md; they win immediately).
/// * anything else — a [`ResetError::Usage`] error: the tool takes no
///   operands.
///
/// # Errors
///
/// [`ResetError::Usage`] for any input outside the grammar above.
pub fn parse(args: &[&str]) -> Result<Command, ResetError> {
    match args.first() {
        None => Ok(Command::Run),
        Some(&("-h" | "-?" | "--help")) => Ok(Command::Help),
        Some(_) => Err(ResetError::Usage),
    }
}

/// The encoded restoration sequence for the capability profile `caps`:
/// leave the alternate screen, re-show the cursor, reset the graphic
/// rendition and the scroll region, then move home and erase the display —
/// each operation emitted only when the profile's terminal accepts it, in
/// the order that leaves the *primary* screen clean (the alternate screen
/// is left first, so the erase lands on the screen the prompt uses).
///
/// The dumb baseline accepts none of the operations, so its sequence is
/// empty: writing nothing is the honest reset for a terminal with no
/// controls (the program half still restores the input discipline).
#[must_use]
pub fn reset_bytes(caps: &Capabilities) -> Vec<u8> {
    let mut ops = Vec::new();
    if caps.alt_screen {
        ops.push(Op::LeaveAltScreen);
    }
    if caps.cursor_visibility {
        ops.push(Op::ShowCursor);
    }
    // Every colour-and-attribute profile understands the plain SGR reset;
    // the monochrome VT100 does too. Only the dumb baseline (no escape
    // processing at all) is excluded, keyed off cursor addressing — the
    // baseline capability every escape-processing terminal has.
    if caps.cursor_addressing {
        ops.push(Op::Sgr(Sgr::Reset));
    }
    if caps.scroll_region {
        ops.push(Op::ResetScrollRegion);
    }
    if caps.cursor_addressing && caps.erase {
        ops.push(Op::CursorPosition { row: 1, col: 1 });
        ops.push(Op::EraseInDisplay(EraseMode::All));
    }
    let mut bytes = Vec::new();
    encode_all_into(&ops, &mut bytes);
    bytes
}

#[cfg(test)]
mod tests {
    extern crate std;

    use rustos_termcap::TermType;

    use super::{parse, reset_bytes, Command, ResetError};

    #[test]
    fn no_arguments_resets() {
        assert_eq!(parse(&[]), Ok(Command::Run));
    }

    #[test]
    fn help_flags_win() {
        assert_eq!(parse(&["-h"]), Ok(Command::Help));
        assert_eq!(parse(&["-?"]), Ok(Command::Help));
        assert_eq!(parse(&["--help"]), Ok(Command::Help));
    }

    #[test]
    fn any_other_argument_is_usage() {
        assert_eq!(parse(&["-x"]), Err(ResetError::Usage));
        assert_eq!(parse(&["operand"]), Err(ResetError::Usage));
    }

    #[test]
    fn reset_bytes_restores_everything_an_xterm_can_wedge() {
        // Leave the alternate screen, show the cursor, reset SGR and the
        // scroll region, then home + erase — in that order, so the erase
        // lands on the restored primary screen.
        let bytes = reset_bytes(&TermType::Xterm256Color.capabilities());
        assert_eq!(bytes, b"\x1b[?1049l\x1b[?25h\x1b[0m\x1b[r\x1b[1;1H\x1b[2J");
    }

    #[test]
    fn reset_bytes_omits_operations_the_terminal_lacks() {
        // A VT100 has no alternate screen and no cursor-visibility control:
        // only the SGR reset, scroll-region reset, and home + erase remain.
        let bytes = reset_bytes(&TermType::Vt100.capabilities());
        assert_eq!(bytes, b"\x1b[0m\x1b[r\x1b[1;1H\x1b[2J");
    }

    #[test]
    fn reset_bytes_is_empty_on_the_dumb_baseline() {
        // The dumb terminal processes no escapes: the honest reset writes
        // nothing (the program half still restores the input discipline).
        assert!(reset_bytes(&TermType::Dumb.capabilities()).is_empty());
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
        let locales = ["en-US", "fr-FR", "de-DE", "es-ES", "uk-UA", "it-IT"];
        for locale in locales {
            let path = format!("{help_root}/{locale}/reset.md");
            let text = fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {path}: {e}"));
            let switch = "`-h, -?`";
            assert!(
                text.contains(switch),
                "{locale}/reset.md must document {switch}"
            );
        }
    }
}
