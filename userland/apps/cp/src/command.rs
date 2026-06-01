//! The parsed shape of a `cp` command line.

use alloc::string::String;
use alloc::vec::Vec;

use crate::error::CpError;

/// One thing the `cp` tool can do.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Command {
    /// Copy `sources` to `dest`, in operand order.
    Copy {
        /// Descend into and reproduce directories (`-r`/`-R`). Without it, a
        /// directory source is a [`CpError::IsDirectory`].
        recursive: bool,
        /// Remove a destination that cannot be created and retry once (`-f`).
        force: bool,
        /// The source paths to copy, in order. Always at least one.
        sources: Vec<String>,
        /// The destination path. When it names an existing directory (and
        /// always when there is more than one source), each source is copied
        /// into it under the source's base name.
        dest: String,
    },
    /// Print the usage banner (`-h`/`--help`).
    Help,
}

/// Parse `args` (the tool's arguments, excluding the program name) into a
/// [`Command`].
///
/// The grammar is the familiar `cp [-r] [-f] [--] source... dest`:
///
/// * `-r` / `-R` / `--recursive` — copy directories and their contents.
/// * `-f` / `--force` — remove an unwritable destination and retry.
/// * `-h` / `--help` — print the usage banner (wins immediately).
/// * `--` — end option parsing; every later argument is a path.
/// * any other `-…` — a [`CpError::Usage`] error (fail closed; never a
///   silently ignored token).
/// * anything else (including the bare `-`) — a path.
///
/// Short options may be combined into one argument (e.g. `-rf` is `-r -f`);
/// an unrecognised letter anywhere in such a cluster is a usage error.
///
/// The last path operand is the destination; the rest are the sources. At
/// least two path operands are required.
///
/// # Errors
///
/// [`CpError::Usage`] for any unrecognised option before `--`, or when fewer
/// than two path operands are given.
pub fn parse(args: &[&str]) -> Result<Command, CpError> {
    let mut recursive = false;
    let mut force = false;
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
                    "recursive" => recursive = true,
                    "force" => force = true,
                    "help" => return Ok(Command::Help),
                    _ => return Err(CpError::Usage),
                }
                continue;
            }
            // A short-option cluster is a leading `-` followed by at least one
            // letter (the bare `-` is a path, not an option).
            if let Some(letters) = arg.strip_prefix('-').filter(|rest| !rest.is_empty()) {
                for letter in letters.chars() {
                    match letter {
                        'r' | 'R' => recursive = true,
                        'f' => force = true,
                        'h' => return Ok(Command::Help),
                        _ => return Err(CpError::Usage),
                    }
                }
                continue;
            }
        }
        operands.push(String::from(arg));
    }
    // The last operand is the destination; everything before it is a source.
    // `cp` needs at least one source and a destination.
    let Some(dest) = operands.pop() else {
        return Err(CpError::Usage);
    };
    if operands.is_empty() {
        return Err(CpError::Usage);
    }
    Ok(Command::Copy {
        recursive,
        force,
        sources: operands,
        dest,
    })
}

#[cfg(test)]
mod tests {
    use super::{parse, Command};
    use crate::error::CpError;
    use alloc::string::String;
    use alloc::vec::Vec;

    fn copy(recursive: bool, force: bool, sources: &[&str], dest: &str) -> Command {
        Command::Copy {
            recursive,
            force,
            sources: sources.iter().map(|p| String::from(*p)).collect::<Vec<_>>(),
            dest: String::from(dest),
        }
    }

    #[test]
    fn a_single_source_and_dest_parses() {
        assert_eq!(
            parse(&["a.txt", "b.txt"]),
            Ok(copy(false, false, &["a.txt"], "b.txt"))
        );
    }

    #[test]
    fn fewer_than_two_operands_is_usage() {
        assert_eq!(parse(&[]), Err(CpError::Usage));
        assert_eq!(parse(&["only"]), Err(CpError::Usage));
        assert_eq!(parse(&["-r"]), Err(CpError::Usage));
        assert_eq!(parse(&["-r", "only"]), Err(CpError::Usage));
    }

    #[test]
    fn help_flags_win() {
        assert_eq!(parse(&["-h"]), Ok(Command::Help));
        assert_eq!(parse(&["--help"]), Ok(Command::Help));
        // `-h` wins even from inside a cluster and alongside other arguments.
        assert_eq!(parse(&["-rh", "a", "b"]), Ok(Command::Help));
    }

    #[test]
    fn recursive_and_force_flags_set_their_fields() {
        assert_eq!(parse(&["-r", "s", "d"]), Ok(copy(true, false, &["s"], "d")));
        assert_eq!(parse(&["-R", "s", "d"]), Ok(copy(true, false, &["s"], "d")));
        assert_eq!(parse(&["-f", "s", "d"]), Ok(copy(false, true, &["s"], "d")));
        assert_eq!(
            parse(&["--recursive", "--force", "s", "d"]),
            Ok(copy(true, true, &["s"], "d"))
        );
    }

    #[test]
    fn clustered_short_options_set_both() {
        assert_eq!(parse(&["-rf", "s", "d"]), Ok(copy(true, true, &["s"], "d")));
        assert_eq!(parse(&["-fR", "s", "d"]), Ok(copy(true, true, &["s"], "d")));
    }

    #[test]
    fn several_sources_and_a_trailing_dest_split_at_the_last_operand() {
        assert_eq!(
            parse(&["a", "b", "c", "dir"]),
            Ok(copy(false, false, &["a", "b", "c"], "dir"))
        );
    }

    #[test]
    fn bare_dash_is_a_path() {
        assert_eq!(parse(&["-", "d"]), Ok(copy(false, false, &["-"], "d")));
    }

    #[test]
    fn unknown_option_is_usage() {
        assert_eq!(parse(&["-x", "s", "d"]), Err(CpError::Usage));
        assert_eq!(parse(&["--frobnicate", "s", "d"]), Err(CpError::Usage));
        // An unknown letter inside an otherwise-valid cluster still fails.
        assert_eq!(parse(&["-rx", "s", "d"]), Err(CpError::Usage));
    }

    #[test]
    fn double_dash_ends_options() {
        // After `--`, a leading-dash argument is a path, not an option.
        assert_eq!(
            parse(&["--", "-r", "-f"]),
            Ok(copy(false, false, &["-r"], "-f"))
        );
    }
}
