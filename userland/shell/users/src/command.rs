//! The parsed shape of a `users` command line.

/// The usage banner a usage error is reported with, and the fallback the
/// short-help switches print when `users`'s own Help tree is unavailable.
pub const USAGE: &str = "usage: users [-h | -?]";

/// The command line was not understood (an unknown option or a stray
/// operand). The caller should print [`USAGE`]. The session never starts.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct UsageError;

/// One thing the `users` tool can do.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Command {
    /// Run the interactive account-administration session.
    Session,
    /// Render `users`'s own short help (`-h`/`-?`/`--help`): the `NAME`,
    /// `SYNOPSIS`, and compact `OPTIONS` of its Help document, through the
    /// same engine as any other command's short help (plans/APPS.md §4).
    Help,
}

/// Parse `args` (the tool's arguments, excluding the program name) into a
/// [`Command`].
///
/// The grammar is `users [-h | -?]`:
///
/// * `-h` / `-?` / `--help` — the reserved short-help switches
///   (plans/APPS.md §4; they win immediately).
/// * anything else — a [`UsageError`]. The tool takes no operands and no
///   other options: accounts are administered with commands typed inside
///   the interactive session, not command-line switches.
///
/// # Errors
///
/// [`UsageError`] for any input outside the grammar above.
pub fn parse(args: &[&str]) -> Result<Command, UsageError> {
    match args.first() {
        None => Ok(Command::Session),
        Some(&("-h" | "-?" | "--help")) => Ok(Command::Help),
        Some(_) => Err(UsageError),
    }
}

#[cfg(test)]
mod tests {
    use super::{parse, Command, UsageError};

    #[test]
    fn no_arguments_runs_the_session() {
        assert_eq!(parse(&[]), Ok(Command::Session));
    }

    #[test]
    fn help_flags_win() {
        assert_eq!(parse(&["-h"]), Ok(Command::Help));
        assert_eq!(parse(&["-?"]), Ok(Command::Help));
        assert_eq!(parse(&["--help"]), Ok(Command::Help));
        // The first token decides; trailing noise never reaches the session.
        assert_eq!(parse(&["-h", "extra"]), Ok(Command::Help));
    }

    #[test]
    fn any_other_argument_is_usage() {
        assert_eq!(parse(&["-x"]), Err(UsageError));
        assert_eq!(parse(&["--frob"]), Err(UsageError));
        assert_eq!(parse(&["operand"]), Err(UsageError));
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
        let locales = ["en-US", "fr-FR", "de-DE", "es-ES", "uk-UA", "it-IT"];
        for locale in locales {
            let path = format!("{help_root}/{locale}/users.md");
            let text = fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {path}: {e}"));
            let switch = "`-h, -?`";
            assert!(
                text.contains(switch),
                "{locale}/users.md must document {switch}"
            );
        }
    }
}
