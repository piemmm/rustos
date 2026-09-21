//! The parsed shape of a `groupdel` command line.

use alloc::string::String;

/// One thing the `groupdel` tool can do.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Command {
    /// Delete the named group.
    Delete(String),
    /// Render this command's own short help (`-h`/`-?`/`--help`).
    Help,
}

/// The command line was not understood. The caller prints
/// [`USAGE`](crate::USAGE) and deletes nothing.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct UsageError;

/// Parse `args` (the tool's arguments, excluding the program name).
///
/// The grammar is `groupdel [--] NAME`:
///
/// * `-h` / `-?` / `--help` — this command's own short help (wins
///   immediately).
/// * `--` — end option parsing; every later argument is an operand.
/// * any other `-…` — a [`UsageError`] (fail closed).
///
/// Exactly one operand is required. The group name is not validated
/// here: the registry is the authority on what names exist, and a name it
/// does not hold is simply not found.
///
/// # Errors
///
/// [`UsageError`] for an unrecognised option or an operand count other
/// than one.
pub fn parse(args: &[&str]) -> Result<Command, UsageError> {
    let mut operand: Option<&str> = None;
    let mut options_done = false;
    let mut extra = false;
    for arg in args {
        if !options_done {
            match *arg {
                "--" => {
                    options_done = true;
                    continue;
                }
                "-h" | "-?" | "--help" => return Ok(Command::Help),
                _ if arg.starts_with('-') && arg.len() > 1 => return Err(UsageError),
                _ => {}
            }
        }
        if operand.replace(arg).is_some() {
            extra = true;
        }
    }
    match (operand, extra) {
        (Some(name), false) => Ok(Command::Delete(String::from(name))),
        _ => Err(UsageError),
    }
}

#[cfg(test)]
mod tests {
    use super::{parse, Command, UsageError};
    use alloc::string::String;

    #[test]
    fn one_operand_is_the_deletion() {
        assert_eq!(
            parse(&["staff"]),
            Ok(Command::Delete(String::from("staff")))
        );
    }

    #[test]
    fn the_help_switches_win_immediately() {
        for flag in ["-h", "-?", "--help"] {
            assert_eq!(parse(&[flag]), Ok(Command::Help));
            assert_eq!(parse(&[flag, "staff"]), Ok(Command::Help));
        }
    }

    #[test]
    fn end_of_options_lets_a_leading_dash_be_a_name() {
        assert_eq!(
            parse(&["--", "-odd"]),
            Ok(Command::Delete(String::from("-odd")))
        );
    }

    #[test]
    fn no_operand_too_many_operands_and_unknown_options_fail_closed() {
        assert_eq!(parse(&[]), Err(UsageError));
        assert_eq!(parse(&["a", "b"]), Err(UsageError));
        assert_eq!(parse(&["-x", "a"]), Err(UsageError));
        assert_eq!(parse(&["--frob", "a"]), Err(UsageError));
    }
}
