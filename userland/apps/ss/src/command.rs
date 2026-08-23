//! The `ss` command shape and its parser.
//!
//! The option surface follows iproute2 `ss`: `-t`/`-u` select the TCP and
//! UDP protocol families (both when neither is given), `-a` shows both
//! listening and connected sockets, `-l` shows only listening sockets
//! (the default hides them), `-n` prints numeric ports and addresses
//! (TAIRiX has no service-name database, so this is always in force and
//! the switch is accepted for familiarity), `-p` adds the owning-process
//! column, `-4`/`-6` restrict the family, and `-H` suppresses the header
//! line, and `-s` prints the stack-wide connection-defence summary instead
//! of the socket table. Short help is the reserved `-?`/`--help` pair
//! (plans/APPS.md §4).

use alloc::format;
use alloc::string::{String, ToString};
use core::fmt;

/// A parsed `ss` invocation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Command {
    /// Render `ss`'s own short help (`-?`/`--help`).
    Help,
    /// List the open sockets the filters select.
    Report(Options),
    /// Print the stack-wide TCP connection-defence summary (`-s`): the
    /// SYN-backlog, SYN-cookie, accept-queue, and reset totals a flood in
    /// progress shows up in.
    Summary,
}

/// Everything an `ss` run needs, straight from the option grammar.
// The bools mirror iproute2 `ss`'s independent switches one-to-one (the
// `df`/`ls` Options precedent); folding them into state enums would obscure
// the grammar they transcribe.
#[allow(clippy::struct_excessive_bools)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Options {
    /// Show TCP sockets (`-t`). With neither `-t` nor `-u`, both show.
    pub tcp: bool,
    /// Show UDP sockets (`-u`). With neither `-t` nor `-u`, both show.
    pub udp: bool,
    /// Show both listening and connected sockets (`-a`).
    pub all: bool,
    /// Show only listening sockets (`-l`).
    pub listening: bool,
    /// Numeric ports/addresses (`-n`). Always in force on TAIRiX (there
    /// is no service-name database); accepted for familiarity.
    pub numeric: bool,
    /// Add the owning-process column (`-p`).
    pub processes: bool,
    /// Restrict to IPv4 sockets (`-4`). With neither, both families show.
    pub ipv4: bool,
    /// Restrict to IPv6 sockets (`-6`). With neither, both families show.
    pub ipv6: bool,
    /// Suppress the header line (`-H`).
    pub no_header: bool,
}

/// Why an argument vector was not a valid `ss` invocation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ParseError {
    /// An option that is not part of the surface.
    UnknownOption(String),
    /// A positional operand: `ss` takes options only (its filter-expression
    /// grammar is not implemented), so a bare word is a usage error rather
    /// than a silently ignored argument.
    UnexpectedOperand(String),
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownOption(opt) => write!(f, "unrecognized option '{opt}'"),
            Self::UnexpectedOperand(word) => write!(f, "unexpected operand '{word}'"),
        }
    }
}

/// Parse an argument vector (without the program name).
///
/// `--` ends option parsing; any operand (before or after it) is a usage
/// error, since `ss`'s filter-expression grammar is not implemented.
///
/// # Errors
///
/// A [`ParseError`] naming the offending argument; the caller reports it
/// with the usage banner and exits `2`.
pub fn parse(args: &[&str]) -> Result<Command, ParseError> {
    let mut options = Options::default();
    let mut options_ended = false;
    for arg in args {
        let arg = *arg;
        if options_ended || arg == "-" || !arg.starts_with('-') {
            return Err(ParseError::UnexpectedOperand(arg.to_string()));
        }
        if arg == "--" {
            options_ended = true;
            continue;
        }
        if let Some(long) = arg.strip_prefix("--") {
            match long {
                "help" => return Ok(Command::Help),
                "tcp" => options.tcp = true,
                "udp" => options.udp = true,
                "all" => options.all = true,
                "listening" => options.listening = true,
                "numeric" => options.numeric = true,
                "processes" => options.processes = true,
                "ipv4" => options.ipv4 = true,
                "ipv6" => options.ipv6 = true,
                "no-header" => options.no_header = true,
                "summary" => return Ok(Command::Summary),
                _ => return Err(ParseError::UnknownOption(arg.to_string())),
            }
            continue;
        }
        for flag in arg[1..].chars() {
            match flag {
                '?' => return Ok(Command::Help),
                't' => options.tcp = true,
                'u' => options.udp = true,
                'a' => options.all = true,
                'l' => options.listening = true,
                'n' => options.numeric = true,
                'p' => options.processes = true,
                '4' => options.ipv4 = true,
                '6' => options.ipv6 = true,
                'H' => options.no_header = true,
                's' => return Ok(Command::Summary),
                other => return Err(ParseError::UnknownOption(format!("-{other}"))),
            }
        }
    }
    Ok(Command::Report(options))
}

#[cfg(test)]
mod tests {
    use super::{parse, Command, Options, ParseError};

    #[test]
    fn no_arguments_reports_with_defaults() {
        assert_eq!(parse(&[]), Ok(Command::Report(Options::default())));
    }

    #[test]
    fn long_and_short_forms_agree() {
        let long = parse(&["--tcp", "--listening", "--processes"]).unwrap();
        let short = parse(&["-t", "-l", "-p"]).unwrap();
        assert_eq!(long, short);
        let Command::Report(options) = short else {
            panic!("report");
        };
        assert!(options.tcp && options.listening && options.processes);
        assert!(!options.udp && !options.all);
    }

    #[test]
    fn a_short_cluster_sets_every_flag() {
        let Command::Report(options) = parse(&["-tuan4"]).unwrap() else {
            panic!("report");
        };
        assert!(options.tcp && options.udp && options.all && options.numeric && options.ipv4);
    }

    #[test]
    fn help_wins_from_either_form() {
        assert_eq!(parse(&["--help"]), Ok(Command::Help));
        assert_eq!(parse(&["-a", "-?"]), Ok(Command::Help));
    }

    #[test]
    fn an_unknown_option_fails_closed() {
        assert_eq!(parse(&["-z"]), Err(ParseError::UnknownOption("-z".into())));
        assert_eq!(
            parse(&["--bogus"]),
            Err(ParseError::UnknownOption("--bogus".into()))
        );
    }

    #[test]
    fn an_operand_is_a_usage_error() {
        assert_eq!(
            parse(&["dst", ":80"]),
            Err(ParseError::UnexpectedOperand("dst".into()))
        );
        // `--` does not turn the next word into a valid operand.
        assert_eq!(
            parse(&["--", "state"]),
            Err(ParseError::UnexpectedOperand("state".into()))
        );
    }

    #[test]
    fn every_option_key_is_covered() {
        // Guards against a flag drifting out of the parser without its
        // Help OPTIONS row (and vice versa): the switch keys the help
        // document pins must all parse here.
        for arg in ["-t", "-u", "-a", "-l", "-n", "-p", "-4", "-6", "-H"] {
            assert!(matches!(parse(&[arg]), Ok(Command::Report(_))), "{arg}");
        }
        for arg in [
            "--tcp",
            "--udp",
            "--all",
            "--listening",
            "--numeric",
            "--processes",
            "--ipv4",
            "--ipv6",
            "--no-header",
        ] {
            assert!(matches!(parse(&[arg]), Ok(Command::Report(_))), "{arg}");
        }
        // `-s` selects a different report, not a filter, so it parses to
        // its own command rather than to `Report`.
        for arg in ["-s", "--summary"] {
            assert_eq!(parse(&[arg]), Ok(Command::Summary), "{arg}");
        }
    }
}
