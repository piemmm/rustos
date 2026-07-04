//! The parsed shape of an `ls` command line.

use alloc::string::String;
use alloc::vec::Vec;

use crate::error::LsError;

/// One thing the `ls` tool can do.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Command {
    /// List `paths`, in operand order.
    List {
        /// Include entries whose name begins with `.` (`-a`).
        all: bool,
        /// Render the long format — type and permission bits, size, then
        /// name (`-l`).
        long: bool,
        /// The paths to list, in order. Never empty: an empty command line
        /// yields the single path `.`.
        paths: Vec<String>,
    },
    /// Render `ls`'s own short help (`-h`/`-?`/`--help`): the `NAME`,
    /// `SYNOPSIS`, and compact `OPTIONS` of its Help document, through the
    /// same engine as any other command's short help (plans/APPS.md §4).
    Help,
}

/// Parse `args` (the tool's arguments, excluding the program name) into a
/// [`Command`].
///
/// The grammar is the familiar `ls [-a] [-l] [--] [path...]`:
///
/// * `-a` / `--all` — include entries whose name begins with `.`.
/// * `-l` / `--long` — render the long format.
/// * `-h` / `-?` / `--help` — the reserved short-help switches
///   (plans/APPS.md §4; they win immediately).
/// * `--` — end option parsing; every later argument is a path.
/// * any other `-…` — an [`LsError::Usage`] error (fail closed; never a
///   silently ignored token).
/// * anything else (including the bare `-`) — a path.
///
/// Short options may be combined into one argument (e.g. `-la` is `-l -a`);
/// an unrecognised letter anywhere in such a cluster is a usage error.
///
/// With no path operand the single path is the current directory (`.`).
///
/// # Errors
///
/// [`LsError::Usage`] for any unrecognised option before `--`.
pub fn parse(args: &[&str]) -> Result<Command, LsError> {
    let mut all = false;
    let mut long = false;
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
                    "all" => all = true,
                    "long" => long = true,
                    "help" => return Ok(Command::Help),
                    _ => return Err(LsError::Usage),
                }
                continue;
            }
            // A short-option cluster is a leading `-` followed by at least one
            // letter (the bare `-` is a path, not an option).
            if let Some(letters) = arg.strip_prefix('-').filter(|rest| !rest.is_empty()) {
                for letter in letters.chars() {
                    match letter {
                        'a' => all = true,
                        'l' => long = true,
                        'h' | '?' => return Ok(Command::Help),
                        _ => return Err(LsError::Usage),
                    }
                }
                continue;
            }
        }
        paths.push(String::from(arg));
    }
    if paths.is_empty() {
        paths.push(String::from("."));
    }
    Ok(Command::List { all, long, paths })
}

#[cfg(test)]
mod tests {
    use super::{parse, Command};
    use crate::error::LsError;
    use alloc::string::String;
    use alloc::vec::Vec;

    fn list(all: bool, long: bool, paths: &[&str]) -> Command {
        Command::List {
            all,
            long,
            paths: paths.iter().map(|p| String::from(*p)).collect::<Vec<_>>(),
        }
    }

    #[test]
    fn no_arguments_lists_current_directory() {
        assert_eq!(parse(&[]), Ok(list(false, false, &["."])));
    }

    #[test]
    fn help_flags_win() {
        assert_eq!(parse(&["-h"]), Ok(Command::Help));
        assert_eq!(parse(&["-?"]), Ok(Command::Help));
        assert_eq!(parse(&["--help"]), Ok(Command::Help));
        // `-h` wins even from inside a cluster and alongside other arguments.
        assert_eq!(parse(&["-lh", "dir"]), Ok(Command::Help));
    }

    #[test]
    fn all_and_long_flags_set_their_fields() {
        assert_eq!(parse(&["-a"]), Ok(list(true, false, &["."])));
        assert_eq!(parse(&["-l"]), Ok(list(false, true, &["."])));
        assert_eq!(parse(&["--all", "--long"]), Ok(list(true, true, &["."])));
    }

    #[test]
    fn clustered_short_options_set_both() {
        assert_eq!(parse(&["-la", "dir"]), Ok(list(true, true, &["dir"])));
        assert_eq!(parse(&["-al"]), Ok(list(true, true, &["."])));
    }

    #[test]
    fn paths_preserve_order() {
        assert_eq!(
            parse(&["a", "b", "c"]),
            Ok(list(false, false, &["a", "b", "c"]))
        );
    }

    #[test]
    fn bare_dash_is_a_path() {
        assert_eq!(parse(&["-"]), Ok(list(false, false, &["-"])));
    }

    #[test]
    fn unknown_option_is_usage() {
        assert_eq!(parse(&["-x"]), Err(LsError::Usage));
        assert_eq!(parse(&["--frobnicate"]), Err(LsError::Usage));
        // An unknown letter inside an otherwise-valid cluster still fails.
        assert_eq!(parse(&["-lx"]), Err(LsError::Usage));
    }

    #[test]
    fn double_dash_ends_options() {
        // After `--`, a leading-dash argument is a path, not an option.
        assert_eq!(parse(&["--", "-l"]), Ok(list(false, false, &["-l"])));
    }
}
