//! The parsed shape of an `elsh` command line — the *invocation*, distinct
//! from the command-line grammar the running shell interprets.

/// The usage banner a usage error is reported with, and the fallback the
/// short-help switches print when `elsh`'s own Help tree is unavailable.
pub const USAGE: &str = "usage: elsh [-h | -?]";

/// The invocation was not understood (an unknown option or a stray
/// operand). The caller should print [`USAGE`]. The session never starts.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct UsageError;

/// One way the `elsh` program can be started.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Invocation {
    /// Run the interactive read-eval-print loop over the inherited
    /// standard streams.
    Repl,
    /// Render `elsh`'s own short help (`-h`/`-?`/`--help`): the `NAME`,
    /// `SYNOPSIS`, and compact `OPTIONS` of its Help document, through the
    /// same engine as any other command's short help (plans/APPS.md §4).
    Help,
}

/// Parse `args` (the program's arguments, excluding the program name) into
/// an [`Invocation`].
///
/// The grammar is `elsh [-h | -?]`:
///
/// * `-h` / `-?` / `--help` — the reserved short-help switches
///   (plans/APPS.md §4; they win immediately).
/// * anything else — a [`UsageError`]. The shell takes no other options
///   and no operands today: script execution is future work with its own
///   grammar, not something to half-accept here.
///
/// # Errors
///
/// [`UsageError`] for any input outside the grammar above.
pub fn parse(args: &[&str]) -> Result<Invocation, UsageError> {
    match args.first() {
        None => Ok(Invocation::Repl),
        Some(&("-h" | "-?" | "--help")) => Ok(Invocation::Help),
        Some(_) => Err(UsageError),
    }
}

#[cfg(test)]
mod tests {
    use super::{parse, Invocation, UsageError};

    #[test]
    fn no_arguments_runs_the_repl() {
        assert_eq!(parse(&[]), Ok(Invocation::Repl));
    }

    #[test]
    fn help_flags_win() {
        assert_eq!(parse(&["-h"]), Ok(Invocation::Help));
        assert_eq!(parse(&["-?"]), Ok(Invocation::Help));
        assert_eq!(parse(&["--help"]), Ok(Invocation::Help));
        // The first token decides; trailing noise never reaches the REPL.
        assert_eq!(parse(&["-h", "extra"]), Ok(Invocation::Help));
    }

    #[test]
    fn any_other_argument_is_usage() {
        assert_eq!(parse(&["-x"]), Err(UsageError));
        assert_eq!(parse(&["--frob"]), Err(UsageError));
        assert_eq!(parse(&["script.sh"]), Err(UsageError));
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
            let path = format!("{help_root}/{locale}/elsh.md");
            let text = fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {path}: {e}"));
            let switch = "`-h, -?`";
            assert!(
                text.contains(switch),
                "{locale}/elsh.md must document {switch}"
            );
        }
    }
}
