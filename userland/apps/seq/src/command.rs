//! The `seq` command line and its parser.

use alloc::string::String;
use alloc::vec::Vec;

use crate::error::SeqError;
use crate::format::{parse_format, Format};

/// One parsed `seq` job: what to print and how.
#[derive(Clone, Debug, PartialEq)]
pub struct Job {
    /// The validated `-f` format, if one was given.
    pub format: Option<Format>,
    /// The `-s` separator between numbers (default `\n`).
    pub separator: String,
    /// `-w` — pad every number to equal width with leading zeros.
    pub equal_width: bool,
    /// The 1–3 raw operands (FIRST, INCREMENT, LAST spellings), kept as
    /// typed as the user spelled them: the fast integer path and the
    /// width/precision inference both read the original text.
    pub operands: Vec<String>,
}

/// One thing the `seq` tool can do.
#[derive(Clone, Debug, PartialEq)]
pub enum Command {
    /// Print the sequence the job describes.
    Print(Job),
    /// Render `seq`'s own short help (`-h`/`-?`/`--help`) through the same
    /// engine as any other command's short help (plans/APPS.md §4).
    Help,
}

/// True when `arg` looks like a negative number (`-5`, `-.3`), which GNU
/// `seq` treats as an operand rather than an option cluster.
fn looks_like_negative_number(arg: &str) -> bool {
    let bytes = arg.as_bytes();
    bytes.first() == Some(&b'-') && matches!(bytes.get(1), Some(b'.' | b'0'..=b'9'))
}

/// Parse `args` (the tool's arguments, excluding the program name) into a
/// [`Command`].
///
/// The scan mirrors GNU `seq`: options are **not** permuted (the first
/// operand ends option parsing, as does `--`), a token that looks like a
/// negative number is an operand, and `-f`/`-s` take their value attached
/// or as the next argument. After the scan: one to three operands are
/// required, the `-f` format must validate, and `-f` may not be combined
/// with `-w` — diagnosed in exactly that order, as in the GNU tool.
///
/// # Errors
///
/// The [`SeqError`] usage variants, mirroring the GNU diagnostics.
pub fn parse(args: &[&str]) -> Result<Command, SeqError> {
    let mut format_str: Option<&str> = None;
    let mut separator: Option<&str> = None;
    let mut equal_width = false;

    let mut index = 0;
    while index < args.len() {
        let arg = args[index];
        if looks_like_negative_number(arg) || arg == "-" || !arg.starts_with('-') {
            break;
        }
        match arg {
            "--" => {
                index += 1;
                break;
            }
            "-h" | "-?" | "--help" => return Ok(Command::Help),
            "--equal-width" => equal_width = true,
            "--format" | "--separator" => {
                index += 1;
                let Some(&value) = args.get(index) else {
                    return Err(SeqError::MissingValue(if arg == "--format" {
                        "--format"
                    } else {
                        "--separator"
                    }));
                };
                if arg == "--format" {
                    format_str = Some(value);
                } else {
                    separator = Some(value);
                }
            }
            _ if arg.starts_with("--format=") => {
                format_str = Some(&arg["--format=".len()..]);
            }
            _ if arg.starts_with("--separator=") => {
                separator = Some(&arg["--separator=".len()..]);
            }
            _ if arg.starts_with("--") => {
                return Err(SeqError::UnknownLong(String::from(arg)));
            }
            _ => {
                // A short-option cluster (`-wf%g`, `-s,`, …): `w` is a
                // flag; `f`/`s` consume the rest of the token or the next
                // argument as their value.
                let mut rest = &arg[1..];
                while let Some(flag) = rest.chars().next() {
                    rest = &rest[flag.len_utf8()..];
                    match flag {
                        'w' => equal_width = true,
                        'h' | '?' => return Ok(Command::Help),
                        'f' | 's' => {
                            let value = if rest.is_empty() {
                                index += 1;
                                let Some(&next) = args.get(index) else {
                                    return Err(SeqError::MissingValue(if flag == 'f' {
                                        "-f"
                                    } else {
                                        "-s"
                                    }));
                                };
                                next
                            } else {
                                rest
                            };
                            if flag == 'f' {
                                format_str = Some(value);
                            } else {
                                separator = Some(value);
                            }
                            rest = "";
                        }
                        other => return Err(SeqError::UnknownShort(other)),
                    }
                }
            }
        }
        index += 1;
    }

    let operands: Vec<String> = args[index..].iter().map(|&s| String::from(s)).collect();
    if operands.is_empty() {
        return Err(SeqError::MissingOperand);
    }
    if operands.len() > 3 {
        return Err(SeqError::ExtraOperand(operands[3].clone()));
    }

    let format = match format_str {
        Some(text) => Some(parse_format(text)?),
        None => None,
    };
    if format.is_some() && equal_width {
        return Err(SeqError::FormatWithEqualWidth);
    }

    Ok(Command::Print(Job {
        format,
        separator: String::from(separator.unwrap_or("\n")),
        equal_width,
        operands,
    }))
}

#[cfg(test)]
mod tests {
    use alloc::string::String;
    use alloc::vec;

    use super::{parse, Command, Job};
    use crate::error::SeqError;
    use crate::format::parse_format;

    fn print_job(command: Command) -> Job {
        match command {
            Command::Print(job) => job,
            Command::Help => panic!("expected a print job"),
        }
    }

    #[test]
    fn bare_operands_parse() {
        let job = print_job(parse(&["10"]).expect("parses"));
        assert_eq!(job.operands, vec![String::from("10")]);
        assert_eq!(job.separator, "\n");
        assert!(!job.equal_width);
        assert!(job.format.is_none());

        let job2 = print_job(parse(&["1", "2", "10"]).expect("parses"));
        assert_eq!(job2.operands.len(), 3);
    }

    #[test]
    fn negative_numbers_are_operands() {
        let job = print_job(parse(&["-5", "5"]).expect("parses"));
        assert_eq!(job.operands, vec![String::from("-5"), String::from("5")]);
        let job2 = print_job(parse(&["-.5"]).expect("parses"));
        assert_eq!(job2.operands, vec![String::from("-.5")]);
    }

    #[test]
    fn first_operand_ends_option_scanning() {
        // GNU seq does not permute: options after an operand are operands
        // (and fail the number scan later, not the option parser).
        let job = print_job(parse(&["1", "-w", "3"]).expect("parses"));
        assert!(!job.equal_width);
        assert_eq!(job.operands.len(), 3);
    }

    #[test]
    fn double_dash_ends_options() {
        let job = print_job(parse(&["--", "5"]).expect("parses"));
        assert_eq!(job.operands, vec![String::from("5")]);
    }

    #[test]
    fn help_flags_win() {
        assert_eq!(parse(&["-h"]), Ok(Command::Help));
        assert_eq!(parse(&["-?"]), Ok(Command::Help));
        assert_eq!(parse(&["--help"]), Ok(Command::Help));
        assert_eq!(parse(&["-wh", "5"]), Ok(Command::Help));
    }

    #[test]
    fn separator_forms() {
        for args in [
            ["-s", ",", "5"].as_slice(),
            ["-s,", "5"].as_slice(),
            ["--separator", ",", "5"].as_slice(),
            ["--separator=,", "5"].as_slice(),
        ] {
            assert_eq!(
                print_job(parse(args).expect("parses")).separator,
                ",",
                "{args:?}"
            );
        }
        // An empty separator is legal.
        assert_eq!(
            print_job(parse(&["--separator=", "5"]).expect("parses")).separator,
            ""
        );
    }

    #[test]
    fn format_forms() {
        let expected = parse_format("%.2f").expect("validates");
        for args in [
            ["-f", "%.2f", "5"].as_slice(),
            ["-f%.2f", "5"].as_slice(),
            ["--format", "%.2f", "5"].as_slice(),
            ["--format=%.2f", "5"].as_slice(),
        ] {
            assert_eq!(
                print_job(parse(args).expect("parses")).format.as_ref(),
                Some(&expected),
                "{args:?}"
            );
        }
    }

    #[test]
    fn cluster_flags() {
        let job = print_job(parse(&["-ws,", "5"]).expect("parses"));
        assert!(job.equal_width);
        assert_eq!(job.separator, ",");
    }

    #[test]
    fn operand_count_is_validated() {
        assert_eq!(parse(&[]), Err(SeqError::MissingOperand));
        assert_eq!(parse(&["-w"]), Err(SeqError::MissingOperand));
        assert_eq!(
            parse(&["1", "2", "3", "4"]),
            Err(SeqError::ExtraOperand(String::from("4")))
        );
    }

    #[test]
    fn unknown_options_are_diagnosed() {
        assert_eq!(
            parse(&["--frob", "5"]),
            Err(SeqError::UnknownLong(String::from("--frob")))
        );
        assert_eq!(parse(&["-x", "5"]), Err(SeqError::UnknownShort('x')));
        assert_eq!(parse(&["-w5"]), Err(SeqError::UnknownShort('5')));
    }

    #[test]
    fn missing_values_are_diagnosed() {
        assert_eq!(parse(&["-f"]), Err(SeqError::MissingValue("-f")));
        assert_eq!(parse(&["-s"]), Err(SeqError::MissingValue("-s")));
        assert_eq!(
            parse(&["--format"]),
            Err(SeqError::MissingValue("--format"))
        );
        assert_eq!(
            parse(&["--separator"]),
            Err(SeqError::MissingValue("--separator"))
        );
    }

    #[test]
    fn diagnostics_follow_the_gnu_order() {
        // Missing operands are diagnosed before the format is validated…
        assert_eq!(parse(&["-f", "%d"]), Err(SeqError::MissingOperand));
        // …the format is validated before the -w conflict…
        assert_eq!(
            parse(&["-w", "-f", "%d", "5"]),
            Err(SeqError::FormatUnknownDirective(String::from("%d"), 'd'))
        );
        // …and a valid format with -w is the conflict diagnostic.
        assert_eq!(
            parse(&["-w", "-f", "%f", "5"]),
            Err(SeqError::FormatWithEqualWidth)
        );
    }
}
