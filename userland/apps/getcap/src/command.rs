//! The parsed shape of a `getcap` command line.

use alloc::string::String;
use alloc::vec::Vec;

use crate::error::GetcapError;

/// One thing the `getcap` tool can do.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Command {
    /// Report the capability gate of each of `files`, in operand order.
    Report {
        /// Descend into directories and report their contents
        /// (`-R`/`--recursive`).
        recursive: bool,
        /// The files to report on, in order. Always at least one.
        files: Vec<String>,
    },
    /// Print the usage banner (`-h`/`--help`).
    Help,
}

/// Parse `args` (the tool's arguments, excluding the program name) into a
/// [`Command`].
///
/// The grammar is `getcap [-R] [--] file...`:
///
/// * `-R` / `--recursive` — descend into directories.
/// * `-h` / `--help` — print the usage banner (wins immediately).
/// * `--` — end option parsing; every later argument is an operand.
/// * any other `-…` — a [`GetcapError::Usage`] error (fail closed; never a
///   silently ignored token).
/// * anything else — an operand.
///
/// # Errors
///
/// [`GetcapError::Usage`] for any unrecognised option before `--`, or when no
/// file operand is given.
pub fn parse(args: &[&str]) -> Result<Command, GetcapError> {
    let mut recursive = false;
    let mut files = Vec::new();
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
                    "help" => return Ok(Command::Help),
                    _ => return Err(GetcapError::Usage),
                }
                continue;
            }
            if let Some(letters) = arg.strip_prefix('-').filter(|rest| !rest.is_empty()) {
                for letter in letters.chars() {
                    match letter {
                        'R' => recursive = true,
                        'h' => return Ok(Command::Help),
                        _ => return Err(GetcapError::Usage),
                    }
                }
                continue;
            }
        }
        files.push(String::from(arg));
    }
    if files.is_empty() {
        return Err(GetcapError::Usage);
    }
    Ok(Command::Report { recursive, files })
}

#[cfg(test)]
mod tests {
    use super::{parse, Command};
    use crate::error::GetcapError;
    use alloc::string::String;
    use alloc::vec::Vec;

    fn report(recursive: bool, files: &[&str]) -> Command {
        Command::Report {
            recursive,
            files: files.iter().map(|p| String::from(*p)).collect::<Vec<_>>(),
        }
    }

    #[test]
    fn one_file_parses() {
        assert_eq!(parse(&["/f"]), Ok(report(false, &["/f"])));
    }

    #[test]
    fn several_files_are_all_collected() {
        assert_eq!(
            parse(&["/a", "/b", "/c"]),
            Ok(report(false, &["/a", "/b", "/c"]))
        );
    }

    #[test]
    fn no_operand_is_usage() {
        assert_eq!(parse(&[]), Err(GetcapError::Usage));
        assert_eq!(parse(&["-R"]), Err(GetcapError::Usage));
    }

    #[test]
    fn help_flags_win() {
        assert_eq!(parse(&["-h"]), Ok(Command::Help));
        assert_eq!(parse(&["--help"]), Ok(Command::Help));
        assert_eq!(parse(&["-Rh", "/f"]), Ok(Command::Help));
    }

    #[test]
    fn recursive_flag_sets_its_field() {
        assert_eq!(parse(&["-R", "/d"]), Ok(report(true, &["/d"])));
        assert_eq!(parse(&["--recursive", "/d"]), Ok(report(true, &["/d"])));
    }

    #[test]
    fn lowercase_r_is_not_recursive_and_is_usage() {
        // `getcap` spells recursive `-R`; a bare `-r` is not an option.
        assert_eq!(parse(&["-r", "/f"]), Err(GetcapError::Usage));
    }

    #[test]
    fn unknown_option_is_usage() {
        assert_eq!(parse(&["-x", "/f"]), Err(GetcapError::Usage));
        assert_eq!(parse(&["--frob", "/f"]), Err(GetcapError::Usage));
        assert_eq!(parse(&["-Rx", "/f"]), Err(GetcapError::Usage));
    }

    #[test]
    fn double_dash_ends_options_so_a_dash_named_file_is_an_operand() {
        assert_eq!(parse(&["--", "-weird"]), Ok(report(false, &["-weird"])));
    }
}
