//! The parsed shape of a `passwd` command line.

use alloc::string::String;

use tairix_util::argv::option_value;

/// The command line was not understood: an unknown option, a missing
/// value, or an operand count other than one. Nothing is changed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct UsageError;

/// Where the new credential comes from.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Secret {
    /// Prompt the terminal twice, echo off, and hash what is typed.
    Prompt,
    /// Use the ready salted PBKDF2 record the caller built, for a caller
    /// with no terminal to prompt at.
    Record(String),
}

/// One thing the `passwd` tool can do.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Command {
    /// Replace the named account's password.
    Set {
        /// The account whose password is replaced.
        name: String,
        /// Where the new credential comes from.
        secret: Secret,
    },
    /// Render this command's own short help (`-h`/`-?`/`--help`).
    Help,
}

/// Parse `args` (the tool's arguments, excluding the program name).
///
/// The grammar is `passwd [--record RECORD] [--] NAME`:
///
/// * `--record` — a ready salted PBKDF2 record, for a caller with no
///   terminal to prompt at. Long-only and TAIRiX-native, so it cannot
///   collide with a shadow-utils switch.
/// * `-h` / `-?` / `--help` — this command's own short help (wins
///   immediately).
/// * `--` — end option parsing; every later argument is an operand.
///
/// Exactly one operand — the account name — is required; see the crate
/// docs for why there is no operand-less self-service form.
///
/// # Errors
///
/// [`UsageError`] for an unrecognised option, a missing value, or an
/// operand count other than one.
pub fn parse(args: &[&str]) -> Result<Command, UsageError> {
    let mut record: Option<String> = None;
    let mut operand: Option<&str> = None;
    let mut extra = false;
    let mut options_done = false;
    let mut index = 0;
    while index < args.len() {
        let arg = args[index];
        index += 1;
        if !options_done {
            if arg == "--" {
                options_done = true;
                continue;
            }
            if let Some(long) = arg.strip_prefix("--") {
                let (name, inline) = match long.split_once('=') {
                    Some((name, value)) => (name, Some(value)),
                    None => (long, None),
                };
                match name {
                    "help" => return Ok(Command::Help),
                    "record" => {
                        record = Some(String::from(
                            option_value(inline, args, &mut index).ok_or(UsageError)?,
                        ));
                    }
                    _ => return Err(UsageError),
                }
                continue;
            }
            if matches!(arg, "-h" | "-?") {
                return Ok(Command::Help);
            }
            if arg.starts_with('-') && arg.len() > 1 {
                return Err(UsageError);
            }
        }
        if operand.replace(arg).is_some() {
            extra = true;
        }
    }
    let (Some(name), false) = (operand, extra) else {
        return Err(UsageError);
    };
    Ok(Command::Set {
        name: String::from(name),
        secret: record.map_or(Secret::Prompt, Secret::Record),
    })
}

#[cfg(test)]
mod tests {
    use super::{parse, Command, Secret, UsageError};
    use alloc::string::String;

    #[test]
    fn one_operand_prompts_for_the_new_password() {
        assert_eq!(
            parse(&["ada"]),
            Ok(Command::Set {
                name: String::from("ada"),
                secret: Secret::Prompt,
            })
        );
    }

    #[test]
    fn a_ready_record_is_taken_attached_or_detached() {
        let want = Ok(Command::Set {
            name: String::from("ada"),
            secret: Secret::Record(String::from("pbkdf2-sha256$1000$AAAA$BBBB")),
        });
        assert_eq!(
            parse(&["--record", "pbkdf2-sha256$1000$AAAA$BBBB", "ada"]),
            want
        );
        assert_eq!(
            parse(&["--record=pbkdf2-sha256$1000$AAAA$BBBB", "ada"]),
            want
        );
    }

    #[test]
    fn the_help_switches_win_immediately() {
        for flag in ["-h", "-?", "--help"] {
            assert_eq!(parse(&[flag]), Ok(Command::Help));
            assert_eq!(parse(&[flag, "ada"]), Ok(Command::Help));
        }
    }

    #[test]
    fn end_of_options_lets_a_leading_dash_be_a_name() {
        assert_eq!(
            parse(&["--", "-odd"]),
            Ok(Command::Set {
                name: String::from("-odd"),
                secret: Secret::Prompt,
            })
        );
    }

    #[test]
    fn a_missing_value_an_unknown_option_and_a_wrong_operand_count_fail_closed() {
        assert_eq!(parse(&["--record"]), Err(UsageError));
        assert_eq!(parse(&[]), Err(UsageError));
        assert_eq!(parse(&["ada", "bob"]), Err(UsageError));
        assert_eq!(parse(&["-x", "ada"]), Err(UsageError));
        assert_eq!(parse(&["--frob", "ada"]), Err(UsageError));
    }
}
