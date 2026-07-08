//! The `printf` command line and its parser.

use alloc::string::String;
use alloc::vec::Vec;

use crate::error::PrintfError;

/// One thing the `printf` tool can do.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Command {
    /// Format and print the arguments under the control of the format.
    Print {
        /// The FORMAT template, exactly as the user spelled it.
        format: String,
        /// The ARGUMENTs the conversions consume, in order.
        arguments: Vec<String>,
    },
    /// Render `printf`'s own short help (`-h`/`-?`/`--help` as the first
    /// argument) through the same engine as any other command's short
    /// help (plans/APPS.md §4).
    Help,
}

/// Parse `args` (the tool's arguments, excluding the program name) into a
/// [`Command`].
///
/// GNU `printf` scans no options beyond the first argument: a first
/// argument of `--` is skipped, and everything else — including a token
/// starting with `-` — is the FORMAT, with the remaining arguments its
/// operands. The one RustOS addition is the §4 short-help convention: a
/// *first* argument of `-h`/`-?`/`--help` renders the tool's own short
/// help (GNU `printf` would format it; spell such a format
/// `printf -- -h…`).
///
/// # Errors
///
/// [`PrintfError::MissingOperand`] when no FORMAT remains after the
/// option scan, exactly as GNU diagnoses `printf` and `printf --`.
pub fn parse(args: &[&str]) -> Result<Command, PrintfError> {
    let mut rest = args;
    match rest.first() {
        Some(&"-h" | &"-?" | &"--help") => return Ok(Command::Help),
        Some(&"--") => rest = &rest[1..],
        _ => {}
    }
    let Some((&format, arguments)) = rest.split_first() else {
        return Err(PrintfError::MissingOperand);
    };
    Ok(Command::Print {
        format: String::from(format),
        arguments: arguments.iter().map(|&arg| String::from(arg)).collect(),
    })
}

#[cfg(test)]
mod tests {
    use super::{parse, Command};
    use crate::error::PrintfError;
    use alloc::string::String;
    use alloc::vec;

    #[test]
    fn format_and_arguments_split() {
        assert_eq!(
            parse(&["%s\n", "a", "b"]),
            Ok(Command::Print {
                format: String::from("%s\n"),
                arguments: vec![String::from("a"), String::from("b")],
            })
        );
    }

    #[test]
    fn a_dash_token_is_a_format() {
        // GNU printf scans no short options: `printf -3` prints `-3`.
        assert_eq!(
            parse(&["-3"]),
            Ok(Command::Print {
                format: String::from("-3"),
                arguments: vec![],
            })
        );
    }

    #[test]
    fn double_dash_precedes_the_format() {
        assert_eq!(
            parse(&["--", "-h", "x"]),
            Ok(Command::Print {
                format: String::from("-h"),
                arguments: vec![String::from("x")],
            })
        );
    }

    #[test]
    fn help_flags_are_honoured_first_only() {
        assert_eq!(parse(&["-h"]), Ok(Command::Help));
        assert_eq!(parse(&["-?"]), Ok(Command::Help));
        assert_eq!(parse(&["--help"]), Ok(Command::Help));
        // Beyond the first argument they are ordinary operands.
        assert_eq!(
            parse(&["%s", "--help"]),
            Ok(Command::Print {
                format: String::from("%s"),
                arguments: vec![String::from("--help")],
            })
        );
    }

    #[test]
    fn a_missing_format_is_diagnosed() {
        assert_eq!(parse(&[]), Err(PrintfError::MissingOperand));
        assert_eq!(parse(&["--"]), Err(PrintfError::MissingOperand));
    }
}
