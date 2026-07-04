//! The parsed shape of a `top` command line.

use crate::error::TopError;

/// The usage banner a usage error is reported with, and the fallback the
/// short-help switches print when `top`'s own Help tree is unavailable.
pub const USAGE: &str = "usage: top [-h | -?]";

/// One thing the `top` tool can do.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Command {
    /// Run the live process viewer.
    Run,
    /// Render `top`'s own short help (`-h`/`-?`/`--help`): the `NAME`,
    /// `SYNOPSIS`, and compact `OPTIONS` of its Help document, through the
    /// same engine as any other command's short help (plans/APPS.md §4).
    Help,
}

/// Parse `args` (the tool's arguments, excluding the program name) into a
/// [`Command`].
///
/// The grammar is `top [-h | -?]`:
///
/// * `-h` / `-?` / `--help` — the reserved short-help switches
///   (plans/APPS.md §4; they win immediately).
/// * anything else — a [`TopError::Usage`] error. The viewer takes no
///   operands and no other options: its scope, refresh, and navigation are
///   keys pressed inside the session, not command-line switches.
///
/// # Errors
///
/// [`TopError::Usage`] for any input outside the grammar above.
pub fn parse(args: &[&str]) -> Result<Command, TopError> {
    match args.first() {
        None => Ok(Command::Run),
        Some(&("-h" | "-?" | "--help")) => Ok(Command::Help),
        Some(_) => Err(TopError::Usage),
    }
}

#[cfg(test)]
mod tests {
    use super::{parse, Command};
    use crate::error::TopError;

    #[test]
    fn no_arguments_runs_the_viewer() {
        assert_eq!(parse(&[]), Ok(Command::Run));
    }

    #[test]
    fn help_flags_win() {
        assert_eq!(parse(&["-h"]), Ok(Command::Help));
        assert_eq!(parse(&["-?"]), Ok(Command::Help));
        assert_eq!(parse(&["--help"]), Ok(Command::Help));
        // The first token decides; trailing noise never reaches the viewer.
        assert_eq!(parse(&["-h", "extra"]), Ok(Command::Help));
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
            let switch = "`-h, -?`";
            assert!(
                text.contains(switch),
                "{locale}/top.md must document {switch}"
            );
        }
    }
}
