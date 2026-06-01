//! The parsed shape of an `rm` command line.

use alloc::string::String;
use alloc::vec::Vec;

use crate::error::RmError;

/// One thing the `rm` tool can do.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Command {
    /// Remove `paths`, in operand order.
    Remove {
        /// Descend into and remove directories (`-r`/`-R`). Without it, a
        /// directory operand is a [`RmError::IsDirectory`].
        recursive: bool,
        /// Ignore operands that do not exist, and remove nothing-to-report
        /// silently (`-f`).
        force: bool,
        /// The paths to remove, in order. May be empty only when `force` is
        /// set (an empty `rm -f` removes nothing and succeeds).
        paths: Vec<String>,
    },
    /// Print the usage banner (`-h`/`--help`).
    Help,
}

/// Parse `args` (the tool's arguments, excluding the program name) into a
/// [`Command`].
///
/// The grammar is the familiar `rm [-r] [-f] [--] file...`:
///
/// * `-r` / `-R` / `--recursive` — remove directories and their contents.
/// * `-f` / `--force` — ignore non-existent operands; never prompt.
/// * `-h` / `--help` — print the usage banner (wins immediately).
/// * `--` — end option parsing; every later argument is a path.
/// * any other `-…` — a [`RmError::Usage`] error (fail closed; never a
///   silently ignored token).
/// * anything else (including the bare `-`) — a path.
///
/// Short options may be combined into one argument (e.g. `-rf` is `-r -f`);
/// an unrecognised letter anywhere in such a cluster is a usage error.
///
/// # Errors
///
/// [`RmError::Usage`] for any unrecognised option before `--`, or when no
/// path operand is given without `-f`.
pub fn parse(args: &[&str]) -> Result<Command, RmError> {
    let mut recursive = false;
    let mut force = false;
    let mut paths = Vec::new();
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
                    _ => return Err(RmError::Usage),
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
                        _ => return Err(RmError::Usage),
                    }
                }
                continue;
            }
        }
        paths.push(String::from(arg));
    }
    if paths.is_empty() && !force {
        return Err(RmError::Usage);
    }
    Ok(Command::Remove {
        recursive,
        force,
        paths,
    })
}

#[cfg(test)]
mod tests {
    use super::{parse, Command};
    use crate::error::RmError;
    use alloc::string::String;
    use alloc::vec::Vec;

    fn remove(recursive: bool, force: bool, paths: &[&str]) -> Command {
        Command::Remove {
            recursive,
            force,
            paths: paths.iter().map(|p| String::from(*p)).collect::<Vec<_>>(),
        }
    }

    #[test]
    fn a_single_file_operand_parses() {
        assert_eq!(parse(&["a.txt"]), Ok(remove(false, false, &["a.txt"])));
    }

    #[test]
    fn no_operand_without_force_is_usage() {
        assert_eq!(parse(&[]), Err(RmError::Usage));
        assert_eq!(parse(&["-r"]), Err(RmError::Usage));
    }

    #[test]
    fn no_operand_with_force_succeeds_empty() {
        assert_eq!(parse(&["-f"]), Ok(remove(false, true, &[])));
    }

    #[test]
    fn help_flags_win() {
        assert_eq!(parse(&["-h"]), Ok(Command::Help));
        assert_eq!(parse(&["--help"]), Ok(Command::Help));
        // `-h` wins even from inside a cluster and alongside other arguments.
        assert_eq!(parse(&["-rh", "dir"]), Ok(Command::Help));
    }

    #[test]
    fn recursive_and_force_flags_set_their_fields() {
        assert_eq!(parse(&["-r", "d"]), Ok(remove(true, false, &["d"])));
        assert_eq!(parse(&["-R", "d"]), Ok(remove(true, false, &["d"])));
        assert_eq!(parse(&["-f", "d"]), Ok(remove(false, true, &["d"])));
        assert_eq!(
            parse(&["--recursive", "--force", "d"]),
            Ok(remove(true, true, &["d"]))
        );
    }

    #[test]
    fn clustered_short_options_set_both() {
        assert_eq!(parse(&["-rf", "d"]), Ok(remove(true, true, &["d"])));
        assert_eq!(parse(&["-fR", "d"]), Ok(remove(true, true, &["d"])));
    }

    #[test]
    fn paths_preserve_order() {
        assert_eq!(
            parse(&["a", "b", "c"]),
            Ok(remove(false, false, &["a", "b", "c"]))
        );
    }

    #[test]
    fn bare_dash_is_a_path() {
        assert_eq!(parse(&["-"]), Ok(remove(false, false, &["-"])));
    }

    #[test]
    fn unknown_option_is_usage() {
        assert_eq!(parse(&["-x", "f"]), Err(RmError::Usage));
        assert_eq!(parse(&["--frobnicate", "f"]), Err(RmError::Usage));
        // An unknown letter inside an otherwise-valid cluster still fails.
        assert_eq!(parse(&["-rx", "f"]), Err(RmError::Usage));
    }

    #[test]
    fn double_dash_ends_options() {
        // After `--`, a leading-dash argument is a path, not an option.
        assert_eq!(parse(&["--", "-r"]), Ok(remove(false, false, &["-r"])));
    }
}
