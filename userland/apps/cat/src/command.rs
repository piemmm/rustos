//! The parsed shape of a `cat` command line.

use alloc::string::String;
use alloc::vec::Vec;

use crate::error::CatError;

/// One source `cat` reads from, in command-line order.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Source {
    /// Standard input — the `-` operand, and the default when no operand is
    /// given.
    Stdin,
    /// A named file.
    Path(String),
}

/// One thing the `cat` tool can do.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Command {
    /// Concatenate `sources` to the terminal, numbering output lines when
    /// `number` is set.
    Concat {
        /// Number output lines continuously across every source (`-n`).
        number: bool,
        /// The sources to read, in order. Never empty: an empty command line
        /// yields a single [`Source::Stdin`].
        sources: Vec<Source>,
    },
    /// Render `cat`'s own short help (`-h`/`-?`/`--help`): the `NAME`,
    /// `SYNOPSIS`, and compact `OPTIONS` of its Help document, through the
    /// same engine as any other command's short help (plans/APPS.md §4).
    Help,
}

/// Parse `args` (the tool's arguments, excluding the program name) into a
/// [`Command`].
///
/// The grammar is the familiar `cat [-n] [--] [file...]`:
///
/// * `-n` / `--number` — number output lines.
/// * `-h` / `-?` / `--help` — the reserved short-help switches
///   (plans/APPS.md §4; they win immediately).
/// * `--` — end option parsing; every later argument is a path.
/// * `-` — standard input.
/// * any other `-…` — an [`CatError::Usage`] error (fail closed; never a
///   silently ignored token).
/// * anything else — a file path.
///
/// With no path (or `-`) operand the single source is standard input.
///
/// # Errors
///
/// [`CatError::Usage`] for any unrecognised option before `--`.
pub fn parse(args: &[&str]) -> Result<Command, CatError> {
    let mut number = false;
    let mut sources = Vec::new();
    let mut options_done = false;
    for &arg in args {
        if !options_done {
            match arg {
                "--" => {
                    options_done = true;
                    continue;
                }
                "-h" | "-?" | "--help" => return Ok(Command::Help),
                "-n" | "--number" => {
                    number = true;
                    continue;
                }
                // An unrecognised option (anything starting with `-` that is
                // not the bare `-` stdin operand) is refused, never ignored.
                _ if arg.starts_with('-') && arg != "-" => return Err(CatError::Usage),
                _ => {}
            }
        }
        sources.push(if arg == "-" {
            Source::Stdin
        } else {
            Source::Path(String::from(arg))
        });
    }
    if sources.is_empty() {
        sources.push(Source::Stdin);
    }
    Ok(Command::Concat { number, sources })
}

#[cfg(test)]
mod tests {
    use super::{parse, Command, Source};
    use crate::error::CatError;
    use alloc::string::String;
    use alloc::vec;

    fn path(name: &str) -> Source {
        Source::Path(String::from(name))
    }

    #[test]
    fn no_arguments_reads_stdin() {
        assert_eq!(
            parse(&[]),
            Ok(Command::Concat {
                number: false,
                sources: vec![Source::Stdin],
            })
        );
    }

    #[test]
    fn help_flags_win() {
        assert_eq!(parse(&["-h"]), Ok(Command::Help));
        assert_eq!(parse(&["-?"]), Ok(Command::Help));
        assert_eq!(parse(&["--help"]), Ok(Command::Help));
        // `--help` is recognised even alongside other arguments.
        assert_eq!(parse(&["-n", "file", "--help"]), Ok(Command::Help));
    }

    #[test]
    fn number_flag_sets_numbering() {
        assert_eq!(
            parse(&["-n", "a.txt"]),
            Ok(Command::Concat {
                number: true,
                sources: vec![path("a.txt")],
            })
        );
        assert_eq!(
            parse(&["--number"]),
            Ok(Command::Concat {
                number: true,
                sources: vec![Source::Stdin],
            })
        );
    }

    #[test]
    fn paths_preserve_order() {
        assert_eq!(
            parse(&["a", "b", "c"]),
            Ok(Command::Concat {
                number: false,
                sources: vec![path("a"), path("b"), path("c")],
            })
        );
    }

    #[test]
    fn dash_is_stdin_mixed_with_files() {
        assert_eq!(
            parse(&["a", "-", "b"]),
            Ok(Command::Concat {
                number: false,
                sources: vec![path("a"), Source::Stdin, path("b")],
            })
        );
    }

    #[test]
    fn unknown_option_is_usage() {
        assert_eq!(parse(&["-x"]), Err(CatError::Usage));
        assert_eq!(parse(&["--frobnicate"]), Err(CatError::Usage));
    }

    #[test]
    fn double_dash_ends_options() {
        // After `--`, a leading-dash argument is a path, not an option.
        assert_eq!(
            parse(&["--", "-n"]),
            Ok(Command::Concat {
                number: false,
                sources: vec![path("-n")],
            })
        );
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
            let path = format!("{help_root}/{locale}/cat.md");
            let text = fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {path}: {e}"));
            for switch in ["`-n, --number`", "`-h, -?`"] {
                assert!(
                    text.contains(switch),
                    "{locale}/cat.md must document {switch}"
                );
            }
        }
    }
}
