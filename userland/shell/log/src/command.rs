//! The parsed shape of a `log` command line.

use rustos_log::Stream;

use crate::error::LogError;

/// How a record is rendered to the terminal.
///
/// Each is a **view** over the authoritative segment bytes (SYSLOG §17):
/// rendering never changes the log.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Format {
    /// One flat readable boot-console line per record ([`rustos_log::render_line`]).
    Line,
    /// One JSON object per record, JSONL-framed ([`rustos_log::render_json`]).
    Json,
    /// A Markdown fragment per record ([`rustos_log::render_markdown`]).
    Markdown,
    /// An aligned terminal table ([`rustos_log::render_table_row`]).
    Table,
}

/// One thing the `log` tool can do.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Command {
    /// Render the records of a stream (or every stream when `stream` is
    /// [`None`]) in `format`. Backs `log show` / `log report` / `log export`.
    Show {
        /// The single stream to render, or [`None`] for every stream.
        stream: Option<Stream>,
        /// The chosen render view.
        format: Format,
    },
    /// Verify the integrity of a stream's segments (or every stream when
    /// `stream` is [`None`]): header/footer checksums, the record and segment
    /// hash chains, and the seal MAC on audit/security segments (SYSLOG §13).
    Verify {
        /// The single stream to verify, or [`None`] for every stream.
        stream: Option<Stream>,
    },
    /// Print the usage banner. The default when no arguments are given.
    Help,
}

/// Parse `args` (the tool's arguments, excluding the program name) into a
/// [`Command`].
///
/// The grammar is one subcommand, an optional positional stream name, and
/// optional flags. It **fails closed**: an unknown subcommand, an unknown
/// stream name, an unknown or disallowed `--format`, a repeated or missing
/// flag value, or a stray argument is a [`LogError::Usage`] rather than a
/// silently ignored token.
///
/// Recognised forms:
///
/// | Subcommand              | Default format | Allowed `--format`        |
/// |-------------------------|----------------|---------------------------|
/// | (none), `help`, `-h`, `--help` | — (usage) | —                    |
/// | `show [stream]`         | line           | line, json, md, markdown, table |
/// | `report [stream]`       | markdown       | markdown, md, table       |
/// | `export [stream]`       | json           | json                      |
/// | `verify [stream]`       | —              | —                         |
///
/// A `stream` operand is one of the closed [`Stream`] names
/// (`boot`/`runtime`/`debug`/`security`/`audit`/`journal`); omitting it
/// selects every stream.
///
/// # Errors
///
/// [`LogError::Usage`] for any input outside the grammar above.
pub fn parse(args: &[&str]) -> Result<Command, LogError> {
    let Some((&subcommand, rest)) = args.split_first() else {
        return Ok(Command::Help);
    };
    match subcommand {
        "help" | "-h" | "--help" => no_more(rest).map(|()| Command::Help),
        "show" => parse_show(rest, Format::Line, show_format),
        "report" => parse_show(rest, Format::Markdown, report_format),
        "export" => parse_show(rest, Format::Json, export_format),
        "verify" => parse_verify(rest),
        _ => Err(LogError::Usage),
    }
}

/// Parse `[stream] [--format F]` for a rendering subcommand: `default` is the
/// format used when no `--format` is given, and `allow` maps a `--format`
/// value to the subset this subcommand permits (rejecting the rest).
fn parse_show(
    args: &[&str],
    default: Format,
    allow: fn(&str) -> Option<Format>,
) -> Result<Command, LogError> {
    let mut stream: Option<Stream> = None;
    let mut format: Option<Format> = None;
    let mut seen_stream = false;
    let mut iter = args.iter();
    while let Some(&arg) = iter.next() {
        if arg == "--format" {
            let &value = iter.next().ok_or(LogError::Usage)?;
            if format.is_some() {
                return Err(LogError::Usage);
            }
            format = Some(allow(value).ok_or(LogError::Usage)?);
        } else if arg.starts_with('-') {
            return Err(LogError::Usage);
        } else {
            // A single positional operand: the stream name.
            if seen_stream {
                return Err(LogError::Usage);
            }
            seen_stream = true;
            stream = Some(Stream::from_name(arg).map_err(|_| LogError::Usage)?);
        }
    }
    Ok(Command::Show {
        stream,
        format: format.unwrap_or(default),
    })
}

/// Parse `verify [stream]`.
fn parse_verify(args: &[&str]) -> Result<Command, LogError> {
    let mut stream: Option<Stream> = None;
    for &arg in args {
        if arg.starts_with('-') || stream.is_some() {
            return Err(LogError::Usage);
        }
        stream = Some(Stream::from_name(arg).map_err(|_| LogError::Usage)?);
    }
    Ok(Command::Verify { stream })
}

/// The formats `show` accepts.
fn show_format(value: &str) -> Option<Format> {
    match value {
        "line" => Some(Format::Line),
        "json" => Some(Format::Json),
        "md" | "markdown" => Some(Format::Markdown),
        "table" => Some(Format::Table),
        _ => None,
    }
}

/// The formats `report` (a human report) accepts.
fn report_format(value: &str) -> Option<Format> {
    match value {
        "md" | "markdown" => Some(Format::Markdown),
        "table" => Some(Format::Table),
        _ => None,
    }
}

/// The formats `export` (a machine export) accepts.
fn export_format(value: &str) -> Option<Format> {
    match value {
        "json" => Some(Format::Json),
        _ => None,
    }
}

/// Reject any trailing argument for a subcommand that takes none.
fn no_more(args: &[&str]) -> Result<(), LogError> {
    if args.is_empty() {
        Ok(())
    } else {
        Err(LogError::Usage)
    }
}

#[cfg(test)]
mod tests {
    use super::{parse, Command, Format};
    use crate::error::LogError;
    use rustos_log::Stream;

    #[test]
    fn no_arguments_is_help() {
        assert_eq!(parse(&[]), Ok(Command::Help));
    }

    #[test]
    fn help_aliases_parse() {
        for arg in ["help", "-h", "--help"] {
            assert_eq!(parse(&[arg]), Ok(Command::Help));
        }
    }

    #[test]
    fn show_defaults_to_every_stream_as_lines() {
        assert_eq!(
            parse(&["show"]),
            Ok(Command::Show {
                stream: None,
                format: Format::Line,
            })
        );
    }

    #[test]
    fn show_accepts_a_stream_operand() {
        assert_eq!(
            parse(&["show", "runtime"]),
            Ok(Command::Show {
                stream: Some(Stream::Runtime),
                format: Format::Line,
            })
        );
    }

    #[test]
    fn show_accepts_each_format() {
        for (value, expected) in [
            ("line", Format::Line),
            ("json", Format::Json),
            ("md", Format::Markdown),
            ("markdown", Format::Markdown),
            ("table", Format::Table),
        ] {
            assert_eq!(
                parse(&["show", "--format", value]),
                Ok(Command::Show {
                    stream: None,
                    format: expected,
                })
            );
        }
    }

    #[test]
    fn report_defaults_to_markdown_and_rejects_json() {
        assert_eq!(
            parse(&["report", "audit"]),
            Ok(Command::Show {
                stream: Some(Stream::Audit),
                format: Format::Markdown,
            })
        );
        assert_eq!(parse(&["report", "--format", "json"]), Err(LogError::Usage));
    }

    #[test]
    fn export_defaults_to_json_and_rejects_line() {
        assert_eq!(
            parse(&["export"]),
            Ok(Command::Show {
                stream: None,
                format: Format::Json,
            })
        );
        assert_eq!(parse(&["export", "--format", "line"]), Err(LogError::Usage));
    }

    #[test]
    fn verify_selects_one_or_every_stream() {
        assert_eq!(parse(&["verify"]), Ok(Command::Verify { stream: None }));
        assert_eq!(
            parse(&["verify", "security"]),
            Ok(Command::Verify {
                stream: Some(Stream::Security),
            })
        );
    }

    #[test]
    fn unknown_subcommand_is_usage() {
        assert_eq!(parse(&["frobnicate"]), Err(LogError::Usage));
    }

    #[test]
    fn unknown_stream_is_usage() {
        assert_eq!(parse(&["show", "nope"]), Err(LogError::Usage));
        assert_eq!(parse(&["verify", "Runtime"]), Err(LogError::Usage));
    }

    #[test]
    fn unknown_flag_is_usage() {
        assert_eq!(parse(&["show", "--colour"]), Err(LogError::Usage));
    }

    #[test]
    fn missing_format_value_is_usage() {
        assert_eq!(parse(&["show", "--format"]), Err(LogError::Usage));
    }

    #[test]
    fn repeated_format_is_usage() {
        assert_eq!(
            parse(&["show", "--format", "json", "--format", "table"]),
            Err(LogError::Usage)
        );
    }

    #[test]
    fn two_stream_operands_is_usage() {
        assert_eq!(parse(&["show", "runtime", "debug"]), Err(LogError::Usage));
        assert_eq!(parse(&["verify", "runtime", "debug"]), Err(LogError::Usage));
    }

    #[test]
    fn help_rejects_trailing_arguments() {
        assert_eq!(parse(&["help", "extra"]), Err(LogError::Usage));
    }
}
