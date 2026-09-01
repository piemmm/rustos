//! The parsed shape of a `servicectl` command line.

use tairix_abi::service::SERVICE_MANIFEST_MAX_NAME_LEN;
use tairix_abi::ServiceControlOp;

/// The usage banner a usage error is reported with, and the fallback the
/// short-help switches print when `servicectl`'s own Help tree is
/// unavailable.
pub const USAGE: &str = "usage: servicectl [-h | -?] start|stop SERVICE";

/// Why a command line was not understood.
///
/// Distinguished rather than collapsed into one error because the three read
/// very differently at a terminal: a missing operand is a typo, an unknown
/// verb is usually a wrong tool, and an over-long name is a name the manager
/// could not hold whatever it referred to.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UsageError {
    /// No command word at all.
    MissingCommand,
    /// The command word is not one this tool implements.
    UnknownCommand,
    /// The command word was given without the service it applies to, or with
    /// more than one operand.
    WrongOperandCount,
    /// The service name exceeds what the control wire can carry, so it is
    /// refused here rather than sent to be refused there.
    NameTooLong,
}

/// One thing the `servicectl` tool can do.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Command<'a> {
    /// Drive one runtime-lifecycle operation on a named service.
    Control {
        /// The operation to request.
        op: ServiceControlOp,
        /// The service it applies to.
        service: &'a str,
    },
    /// Render `servicectl`'s own short help (`-h`/`-?`/`--help`).
    Help,
}

/// Parse `args` (the tool's arguments, excluding the program name).
///
/// The grammar is `servicectl [-h | -?] start|stop SERVICE`:
///
/// * `-h` / `-?` / `--help` — the reserved short-help switches; they win
///   immediately.
/// * `start SERVICE` / `stop SERVICE` — the runtime-lifecycle operations.
/// * `--` ends the options, so a service whose name begins with a dash is
///   still nameable.
///
/// Enablement (`enable`/`disable`) is deliberately absent: it mutates the
/// registration store rather than the runtime, and is a separate operation
/// on a separate path.
///
/// # Errors
///
/// A [`UsageError`] naming which part of the grammar was not met. Nothing is
/// sent when the line does not parse (fail closed).
pub fn parse<'a>(args: &[&'a str]) -> Result<Command<'a>, UsageError> {
    let mut rest = args;
    if let Some(&("-h" | "-?" | "--help")) = rest.first() {
        return Ok(Command::Help);
    }
    if let Some(&"--") = rest.first() {
        rest = &rest[1..];
    }
    let Some((verb, operands)) = rest.split_first() else {
        return Err(UsageError::MissingCommand);
    };
    let op = match *verb {
        "start" => ServiceControlOp::Start,
        "stop" => ServiceControlOp::Stop,
        _ => return Err(UsageError::UnknownCommand),
    };
    let [service] = operands else {
        return Err(UsageError::WrongOperandCount);
    };
    if service.len() > SERVICE_MANIFEST_MAX_NAME_LEN {
        return Err(UsageError::NameTooLong);
    }
    Ok(Command::Control { op, service })
}

#[cfg(test)]
mod tests {
    use super::{parse, Command, UsageError, USAGE};
    use tairix_abi::service::SERVICE_MANIFEST_MAX_NAME_LEN;
    use tairix_abi::ServiceControlOp;

    fn control(args: &[&str]) -> (ServiceControlOp, alloc::string::String) {
        match parse(args) {
            Ok(Command::Control { op, service }) => (op, alloc::string::String::from(service)),
            other => panic!("expected a control command, got {other:?}"),
        }
    }

    #[test]
    fn both_verbs_parse_with_their_service() {
        assert_eq!(control(&["start", "timed"]).0, ServiceControlOp::Start);
        assert_eq!(control(&["start", "timed"]).1, "timed");
        assert_eq!(control(&["stop", "netstack"]).0, ServiceControlOp::Stop);
        assert_eq!(control(&["stop", "netstack"]).1, "netstack");
    }

    #[test]
    fn help_flags_win_before_anything_else() {
        assert_eq!(parse(&["-h"]), Ok(Command::Help));
        assert_eq!(parse(&["-?"]), Ok(Command::Help));
        assert_eq!(parse(&["--help"]), Ok(Command::Help));
        // The first token decides, so a trailing verb never reaches the
        // manager behind a help request.
        assert_eq!(parse(&["-h", "stop", "timed"]), Ok(Command::Help));
    }

    #[test]
    fn end_of_options_lets_a_dashed_name_through() {
        // Without `--` a leading dash would read as an unknown option; with
        // it, the verb and its operand are taken literally.
        assert_eq!(control(&["--", "stop", "-odd-name"]).1, "-odd-name");
    }

    #[test]
    fn each_grammar_failure_is_named() {
        assert_eq!(parse(&[]), Err(UsageError::MissingCommand));
        assert_eq!(parse(&["--"]), Err(UsageError::MissingCommand));
        assert_eq!(
            parse(&["restart", "timed"]),
            Err(UsageError::UnknownCommand)
        );
        assert_eq!(parse(&["-x", "timed"]), Err(UsageError::UnknownCommand));
        assert_eq!(parse(&["start"]), Err(UsageError::WrongOperandCount));
        assert_eq!(
            parse(&["start", "timed", "extra"]),
            Err(UsageError::WrongOperandCount)
        );
    }

    #[test]
    fn an_over_long_name_is_refused_here_not_on_the_wire() {
        let long = "a".repeat(SERVICE_MANIFEST_MAX_NAME_LEN + 1);
        assert_eq!(parse(&["stop", &long]), Err(UsageError::NameTooLong));

        // Exactly at the bound is fine: the wire can carry it.
        let at_bound = "a".repeat(SERVICE_MANIFEST_MAX_NAME_LEN);
        assert_eq!(control(&["stop", &at_bound]).1, at_bound);
    }

    /// Every locale's `OPTIONS` section documents exactly the switches this
    /// parser accepts: the flag tokens are language-neutral, so each
    /// translated document must carry the same keys as the canonical one. The
    /// documents are read from the bundle's own on-disk `Help/` tree — the
    /// single source the image builder plants — never a copy embedded here.
    #[test]
    fn help_documents_the_parser_switches() {
        extern crate std;
        use alloc::format;
        use std::fs;

        let help_root = format!("{}/Help", env!("CARGO_MANIFEST_DIR"));
        for locale in tairix_help::REQUIRED_LOCALES {
            let path = format!("{help_root}/{locale}/servicectl.md");
            let text = fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {path}: {e}"));
            for key in ["`-h, -?`", "`--`", "`start SERVICE`", "`stop SERVICE`"] {
                assert!(
                    text.contains(key),
                    "{locale}/servicectl.md must document {key}"
                );
            }
            // The exit codes are the tool's contract with a script, so every
            // locale states all three rather than only the happy one.
            for code in ["`0`", "`1`", "`2`"] {
                assert!(
                    text.contains(code),
                    "{locale}/servicectl.md must state exit status {code}"
                );
            }
        }
    }

    #[test]
    fn the_usage_banner_names_both_verbs_and_the_help_switches() {
        assert!(USAGE.contains("start"));
        assert!(USAGE.contains("stop"));
        assert!(USAGE.contains("-h"));
        assert!(USAGE.contains("-?"));
    }
}
