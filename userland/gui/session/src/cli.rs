//! The `desktop` command's argument grammar (`plans/APPS.md`).
//!
//! The desktop is a system command app: a shell user types `desktop` to
//! start the graphical session, and a graphical login spawns the same
//! bundle. The tool starts the session or serves its own short help —
//! nothing else — so the grammar is deliberately closed: it takes no
//! operands and no options beyond the reserved short-help switches, and
//! anything outside it is a usage error (fail closed), never a guess.

/// The usage banner a usage error is reported with, and the fallback the
/// short-help switches print when the bundle's own Help tree is
/// unavailable.
pub const USAGE: &str = "usage: desktop [-h | -?]";

/// One thing the `desktop` command can do.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Command {
    /// Start the graphical desktop session.
    Run,
    /// Render the command's own short help (`-h`/`-?`/`--help`): the
    /// `NAME`, `SYNOPSIS`, and compact `OPTIONS` of its Help document,
    /// through the same engine as any other command's short help
    /// (plans/APPS.md).
    Help,
}

/// The one failure [`parse`] reports.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CliError {
    /// The command line was not understood.
    Usage,
}

/// Parse `args` (the tool's arguments, excluding the program name) into a
/// [`Command`].
///
/// The grammar is `desktop [-h | -?]`:
///
/// * `-h` / `-?` / `--help` — the reserved short-help switches
///   (plans/APPS.md; they win immediately).
/// * anything else — a [`CliError::Usage`] error: the tool takes no
///   operands and no other options.
///
/// # Errors
///
/// [`CliError::Usage`] for any input outside the grammar above.
pub fn parse(args: &[&str]) -> Result<Command, CliError> {
    match args {
        [] => Ok(Command::Run),
        ["-h" | "-?" | "--help", ..] => Ok(Command::Help),
        _ => Err(CliError::Usage),
    }
}

#[cfg(test)]
mod tests {
    extern crate std;

    use super::{parse, CliError, Command};

    #[test]
    fn no_arguments_starts_the_session() {
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
        // No operands and no other options: refuse whole, never guess.
        assert_eq!(parse(&["--frob"]), Err(CliError::Usage));
        assert_eq!(parse(&["operand"]), Err(CliError::Usage));
        assert_eq!(parse(&["operand", "-h"]), Err(CliError::Usage));
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
            let path = format!("{help_root}/{locale}/desktop.md");
            let text = fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {path}: {e}"));
            let switch = "`-h, -?`";
            assert!(
                text.contains(switch),
                "{locale}/desktop.md must document {switch}"
            );
        }
    }
}
