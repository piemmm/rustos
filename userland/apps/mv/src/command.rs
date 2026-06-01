//! The parsed shape of an `mv` command line.

use alloc::string::String;
use alloc::vec::Vec;

use crate::error::MvError;

/// One thing the `mv` tool can do.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Command {
    /// Move `sources` to `dest`, in operand order.
    Move {
        /// Remove a destination that blocks an atomic rename and retry once
        /// (`-f`). Mutually compatible with — but overridden by — `no_clobber`
        /// when both are set (the last one to apply wins, matching POSIX:
        /// `-n` after `-f` suppresses the overwrite).
        force: bool,
        /// Never overwrite an existing destination (`-n`); such a source is
        /// skipped silently rather than replacing what is there.
        no_clobber: bool,
        /// The source paths to move, in order. Always at least one.
        sources: Vec<String>,
        /// The destination path. When it names an existing directory (and
        /// always when there is more than one source), each source is moved
        /// into it under the source's base name.
        dest: String,
    },
    /// Print the usage banner (`-h`/`--help`).
    Help,
}

/// Parse `args` (the tool's arguments, excluding the program name) into a
/// [`Command`].
///
/// The grammar is the familiar `mv [-f] [-n] [--] source... dest`:
///
/// * `-f` / `--force` — remove a destination that blocks the rename and retry.
/// * `-n` / `--no-clobber` — never overwrite an existing destination.
/// * `-h` / `--help` — print the usage banner (wins immediately).
/// * `--` — end option parsing; every later argument is a path.
/// * any other `-…` — a [`MvError::Usage`] error (fail closed; never a
///   silently ignored token).
/// * anything else (including the bare `-`) — a path.
///
/// Short options may be combined into one argument (e.g. `-fn` is `-f -n`);
/// an unrecognised letter anywhere in such a cluster is a usage error.
///
/// The last path operand is the destination; the rest are the sources. At
/// least two path operands are required.
///
/// # Errors
///
/// [`MvError::Usage`] for any unrecognised option before `--`, or when fewer
/// than two path operands are given.
pub fn parse(args: &[&str]) -> Result<Command, MvError> {
    let mut force = false;
    let mut no_clobber = false;
    let mut operands = Vec::new();
    let mut options_done = false;
    for &arg in args {
        if !options_done {
            if arg == "--" {
                options_done = true;
                continue;
            }
            if let Some(name) = arg.strip_prefix("--") {
                match name {
                    "force" => force = true,
                    "no-clobber" => no_clobber = true,
                    "help" => return Ok(Command::Help),
                    _ => return Err(MvError::Usage),
                }
                continue;
            }
            // A short-option cluster is a leading `-` followed by at least one
            // letter (the bare `-` is a path, not an option).
            if let Some(letters) = arg.strip_prefix('-').filter(|rest| !rest.is_empty()) {
                for letter in letters.chars() {
                    match letter {
                        'f' => force = true,
                        'n' => no_clobber = true,
                        'h' => return Ok(Command::Help),
                        _ => return Err(MvError::Usage),
                    }
                }
                continue;
            }
        }
        operands.push(String::from(arg));
    }
    // The last operand is the destination; everything before it is a source.
    // `mv` needs at least one source and a destination.
    let Some(dest) = operands.pop() else {
        return Err(MvError::Usage);
    };
    if operands.is_empty() {
        return Err(MvError::Usage);
    }
    Ok(Command::Move {
        force,
        no_clobber,
        sources: operands,
        dest,
    })
}

#[cfg(test)]
mod tests {
    use super::{parse, Command};
    use crate::error::MvError;
    use alloc::string::String;
    use alloc::vec::Vec;

    fn mv(force: bool, no_clobber: bool, sources: &[&str], dest: &str) -> Command {
        Command::Move {
            force,
            no_clobber,
            sources: sources.iter().map(|p| String::from(*p)).collect::<Vec<_>>(),
            dest: String::from(dest),
        }
    }

    #[test]
    fn a_single_source_and_dest_parses() {
        assert_eq!(
            parse(&["a.txt", "b.txt"]),
            Ok(mv(false, false, &["a.txt"], "b.txt"))
        );
    }

    #[test]
    fn fewer_than_two_operands_is_usage() {
        assert_eq!(parse(&[]), Err(MvError::Usage));
        assert_eq!(parse(&["only"]), Err(MvError::Usage));
        assert_eq!(parse(&["-f"]), Err(MvError::Usage));
        assert_eq!(parse(&["-f", "only"]), Err(MvError::Usage));
    }

    #[test]
    fn help_flags_win() {
        assert_eq!(parse(&["-h"]), Ok(Command::Help));
        assert_eq!(parse(&["--help"]), Ok(Command::Help));
        // `-h` wins even from inside a cluster and alongside other arguments.
        assert_eq!(parse(&["-fh", "a", "b"]), Ok(Command::Help));
    }

    #[test]
    fn force_and_no_clobber_flags_set_their_fields() {
        assert_eq!(parse(&["-f", "s", "d"]), Ok(mv(true, false, &["s"], "d")));
        assert_eq!(parse(&["-n", "s", "d"]), Ok(mv(false, true, &["s"], "d")));
        assert_eq!(
            parse(&["--force", "--no-clobber", "s", "d"]),
            Ok(mv(true, true, &["s"], "d"))
        );
    }

    #[test]
    fn clustered_short_options_set_both() {
        assert_eq!(parse(&["-fn", "s", "d"]), Ok(mv(true, true, &["s"], "d")));
        assert_eq!(parse(&["-nf", "s", "d"]), Ok(mv(true, true, &["s"], "d")));
    }

    #[test]
    fn several_sources_and_a_trailing_dest_split_at_the_last_operand() {
        assert_eq!(
            parse(&["a", "b", "c", "dir"]),
            Ok(mv(false, false, &["a", "b", "c"], "dir"))
        );
    }

    #[test]
    fn bare_dash_is_a_path() {
        assert_eq!(parse(&["-", "d"]), Ok(mv(false, false, &["-"], "d")));
    }

    #[test]
    fn unknown_option_is_usage() {
        assert_eq!(parse(&["-x", "s", "d"]), Err(MvError::Usage));
        assert_eq!(parse(&["--frobnicate", "s", "d"]), Err(MvError::Usage));
        // An unknown letter inside an otherwise-valid cluster still fails.
        assert_eq!(parse(&["-fx", "s", "d"]), Err(MvError::Usage));
    }

    #[test]
    fn double_dash_ends_options() {
        // After `--`, a leading-dash argument is a path, not an option.
        assert_eq!(
            parse(&["--", "-f", "-n"]),
            Ok(mv(false, false, &["-f"], "-n"))
        );
    }
}
