//! The parsed shape of a `ps` command line.

use crate::error::PsError;

/// One thing the `ps` tool can do.
///
/// There is intentionally no free-form selection language: the tool only
/// ever issues the two frozen `sysinfo-v1` process-list queries (the
/// caller's own processes, or — with `-e`/`-A` — every process, which the
/// service gates on `CAP_SYSINFO_GLOBAL`).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Command {
    /// List processes. `all` selects the system-wide view
    /// (`SysinfoQueryId::GLOBAL_PROCESS_LIST`, gated on `CAP_SYSINFO_GLOBAL`);
    /// otherwise the caller's own processes (`SELF_PROCESS_LIST`, ungated).
    List {
        /// List every process rather than just the caller's own.
        all: bool,
    },
    /// Render `ps`'s own short help (`-h`/`-?`/`--help`): the `NAME`,
    /// `SYNOPSIS`, and compact `OPTIONS` of its Help document, through the
    /// same engine as any other command's short help (plans/APPS.md §4).
    Help,
}

/// Parse `args` (the tool's arguments, excluding the program name) into a
/// [`Command`].
///
/// The grammar is `ps [-e | -A | --all] [-h | -?]`:
///
/// * `-e` / `-A` / `--all` — list every process (the POSIX "all" selectors),
///   which the service gates on `CAP_SYSINFO_GLOBAL`.
/// * `-h` / `-?` / `--help` — the reserved short-help switches
///   (plans/APPS.md §4; they win immediately).
/// * `--` — end option parsing.
///
/// `ps` takes no file operands. It **fails closed**: an unknown option, an
/// unknown letter inside a cluster, or any positional operand is a
/// [`PsError::Usage`] rather than a silently ignored token.
///
/// # Errors
///
/// [`PsError::Usage`] for any input outside the grammar above.
pub fn parse(args: &[&str]) -> Result<Command, PsError> {
    let mut all = false;
    let mut options_done = false;
    for &arg in args {
        if !options_done {
            if arg == "--" {
                options_done = true;
                continue;
            }
            if let Some(name) = arg.strip_prefix("--") {
                match name {
                    "all" => all = true,
                    "help" => return Ok(Command::Help),
                    _ => return Err(PsError::Usage),
                }
                continue;
            }
            if let Some(letters) = arg.strip_prefix('-').filter(|rest| !rest.is_empty()) {
                for letter in letters.chars() {
                    match letter {
                        'e' | 'A' => all = true,
                        'h' | '?' => return Ok(Command::Help),
                        _ => return Err(PsError::Usage),
                    }
                }
                continue;
            }
        }
        // `ps` accepts no positional operands.
        return Err(PsError::Usage);
    }
    Ok(Command::List { all })
}

#[cfg(test)]
mod tests {
    use super::{parse, Command};
    use crate::error::PsError;

    #[test]
    fn no_arguments_lists_the_callers_own_processes() {
        assert_eq!(parse(&[]), Ok(Command::List { all: false }));
    }

    #[test]
    fn all_selectors_request_every_process() {
        assert_eq!(parse(&["-e"]), Ok(Command::List { all: true }));
        assert_eq!(parse(&["-A"]), Ok(Command::List { all: true }));
        assert_eq!(parse(&["--all"]), Ok(Command::List { all: true }));
    }

    #[test]
    fn help_flags_win() {
        assert_eq!(parse(&["-h"]), Ok(Command::Help));
        assert_eq!(parse(&["-?"]), Ok(Command::Help));
        assert_eq!(parse(&["--help"]), Ok(Command::Help));
        assert_eq!(parse(&["-eh"]), Ok(Command::Help));
    }

    #[test]
    fn unknown_option_is_usage() {
        assert_eq!(parse(&["-x"]), Err(PsError::Usage));
        assert_eq!(parse(&["--frob"]), Err(PsError::Usage));
        assert_eq!(parse(&["-ex"]), Err(PsError::Usage));
    }

    #[test]
    fn positional_operand_is_usage() {
        assert_eq!(parse(&["1234"]), Err(PsError::Usage));
        assert_eq!(parse(&["--", "anything"]), Err(PsError::Usage));
    }

    #[test]
    fn double_dash_then_no_operand_is_a_plain_list() {
        assert_eq!(parse(&["-e", "--"]), Ok(Command::List { all: true }));
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
            let path = format!("{help_root}/{locale}/ps.md");
            let text = fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {path}: {e}"));
            for switch in ["`-e, -A, --all`", "`-h, -?`"] {
                assert!(
                    text.contains(switch),
                    "{locale}/ps.md must document {switch}"
                );
            }
        }
    }
}
