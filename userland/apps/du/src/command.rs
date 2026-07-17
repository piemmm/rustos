//! The `du` command shape and its parser.
//!
//! The option surface follows GNU coreutils `du`: `-a` reports every
//! file, `-s` only the operands, `-c` appends a grand total, `-d`
//! bounds the reported depth, `-S` excludes subdirectories from a
//! directory's own row, `--apparent-size`/`-b` measure apparent bytes
//! instead of allocated storage, and the unit options (`-k`, `-m`,
//! `-h`, `--si`, `-B <size>`, `-b`) select the reporting scale, the
//! later selection winning as in the GNU tool. `-0` NUL-terminates
//! rows. Short help is the reserved `-?`/`--help` pair (plans/APPS.md
//! §4); `-h` keeps its GNU meaning (human-readable), exactly as `ls`.

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::fmt;

use tairix_util::size::{parse_block_size, SizeScale};

/// A parsed `du` invocation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Command {
    /// Render `du`'s own short help (`-?`/`--help`): the `NAME`,
    /// `SYNOPSIS`, and compact `OPTIONS` of its bundled help document,
    /// through the same engine as any other command's short help
    /// (plans/APPS.md §4).
    Help,
    /// Walk the operands and report their usage.
    Report(Options),
}

/// Everything a `du` run needs to know, straight from the option grammar.
// The bools mirror GNU `du`'s independent switches one-to-one (the `ls`/`rm`
// Options precedent); folding them into state enums would obscure the
// grammar they transcribe.
#[allow(clippy::struct_excessive_bools)]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Options {
    /// The path operands, in command-line order; empty means the current
    /// directory (`.`), applied by the client.
    pub paths: Vec<String>,
    /// Report an entry for every file, not just directories (`-a`).
    pub all: bool,
    /// Report only each operand's total (`-s`).
    pub summarize: bool,
    /// Append a grand-total row (`-c`).
    pub grand_total: bool,
    /// Measure apparent byte lengths instead of allocated on-disk
    /// storage (`--apparent-size`, `-b`).
    pub apparent_size: bool,
    /// The reporting scale; the default is 1024-byte blocks.
    pub scale: SizeScale,
    /// Report directories at most this many levels below an operand
    /// (`-d`); `0` reports the operands only.
    pub max_depth: Option<u64>,
    /// A directory's row excludes its subdirectories (`-S`).
    pub separate_dirs: bool,
    /// End each row with NUL instead of newline (`-0`).
    pub null_terminated: bool,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            paths: Vec::new(),
            all: false,
            summarize: false,
            grand_total: false,
            apparent_size: false,
            scale: SizeScale::Blocks(1024),
            max_depth: None,
            separate_dirs: false,
            null_terminated: false,
        }
    }
}

/// Why an argument vector was not a valid `du` invocation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ParseError {
    /// An option that is not part of the surface.
    UnknownOption(String),
    /// An option that requires a value reached the end of the arguments.
    MissingValue(&'static str),
    /// A `--block-size`/`-B` value that is not a valid GNU size.
    BadBlockSize(String),
    /// A `--max-depth`/`-d` value that is not a non-negative integer.
    BadMaxDepth(String),
    /// `-s` combined with `-a`: the two select contradictory row sets.
    SummarizeConflictsWithAll,
    /// `-s` combined with `-d`: `-s` already means `--max-depth=0`.
    SummarizeConflictsWithDepth,
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownOption(opt) => write!(f, "unrecognized option '{opt}'"),
            Self::MissingValue(opt) => write!(f, "option '{opt}' requires an argument"),
            Self::BadBlockSize(value) => write!(f, "invalid --block-size argument '{value}'"),
            Self::BadMaxDepth(value) => write!(f, "invalid maximum depth '{value}'"),
            Self::SummarizeConflictsWithAll => {
                write!(f, "cannot both summarize and show all entries")
            }
            Self::SummarizeConflictsWithDepth => {
                write!(f, "summarizing conflicts with --max-depth")
            }
        }
    }
}

/// Parse an argument vector (without the program name).
///
/// Options and operands may interleave; `--` ends option parsing. Later
/// unit selections override earlier ones, exactly as in the GNU tool.
///
/// # Errors
///
/// A [`ParseError`] naming the offending argument; the caller reports it
/// with the usage banner and exits `2`.
pub fn parse(args: &[&str]) -> Result<Command, ParseError> {
    let mut options = Options::default();
    let mut depth_selected = false;
    let mut index = 0;
    let mut options_ended = false;
    while index < args.len() {
        let arg = args[index];
        index += 1;
        if options_ended || arg == "-" || !arg.starts_with('-') {
            options.paths.push(arg.to_string());
            continue;
        }
        if arg == "--" {
            options_ended = true;
            continue;
        }
        if let Some(long) = arg.strip_prefix("--") {
            let (name, attached) = match long.split_once('=') {
                Some((name, value)) => (name, Some(value)),
                None => (long, None),
            };
            match name {
                "help" => return Ok(Command::Help),
                "all" => options.all = true,
                "summarize" => options.summarize = true,
                "total" => options.grand_total = true,
                "apparent-size" => options.apparent_size = true,
                "human-readable" => options.scale = SizeScale::HumanBinary,
                "si" => options.scale = SizeScale::HumanDecimal,
                "separate-dirs" => options.separate_dirs = true,
                "null" => options.null_terminated = true,
                "bytes" => {
                    options.apparent_size = true;
                    options.scale = SizeScale::Blocks(1);
                }
                "block-size" => {
                    let value = take_value(attached, args, &mut index, "--block-size")?;
                    options.scale = parse_block_size(value)
                        .ok_or_else(|| ParseError::BadBlockSize(value.to_string()))?;
                }
                "max-depth" => {
                    let value = take_value(attached, args, &mut index, "--max-depth")?;
                    options.max_depth = Some(
                        value
                            .parse()
                            .map_err(|_| ParseError::BadMaxDepth(value.to_string()))?,
                    );
                    depth_selected = true;
                }
                _ => return Err(ParseError::UnknownOption(arg.to_string())),
            }
            continue;
        }
        // A short-option cluster; `-B` and `-d` consume the rest of the
        // cluster (or the next argument) as their value.
        let mut cluster = arg[1..].chars();
        while let Some(flag) = cluster.next() {
            match flag {
                '?' => return Ok(Command::Help),
                'a' => options.all = true,
                's' => options.summarize = true,
                'c' => options.grand_total = true,
                'S' => options.separate_dirs = true,
                '0' => options.null_terminated = true,
                'h' => options.scale = SizeScale::HumanBinary,
                'k' => options.scale = SizeScale::Blocks(1024),
                'm' => options.scale = SizeScale::Blocks(1024 * 1024),
                'b' => {
                    options.apparent_size = true;
                    options.scale = SizeScale::Blocks(1);
                }
                'B' => {
                    let rest = cluster.as_str();
                    let value = take_cluster_value(rest, args, &mut index, "-B")?;
                    options.scale = parse_block_size(value)
                        .ok_or_else(|| ParseError::BadBlockSize(value.to_string()))?;
                    break;
                }
                'd' => {
                    let rest = cluster.as_str();
                    let value = take_cluster_value(rest, args, &mut index, "-d")?;
                    options.max_depth = Some(
                        value
                            .parse()
                            .map_err(|_| ParseError::BadMaxDepth(value.to_string()))?,
                    );
                    depth_selected = true;
                    break;
                }
                other => return Err(ParseError::UnknownOption(format!("-{other}"))),
            }
        }
    }
    if options.summarize && options.all {
        return Err(ParseError::SummarizeConflictsWithAll);
    }
    if options.summarize && depth_selected {
        return Err(ParseError::SummarizeConflictsWithDepth);
    }
    if options.summarize {
        options.max_depth = Some(0);
    }
    Ok(Command::Report(options))
}

/// The value of a `--name value` / `--name=value` long option.
fn take_value<'a>(
    attached: Option<&'a str>,
    args: &[&'a str],
    index: &mut usize,
    option: &'static str,
) -> Result<&'a str, ParseError> {
    if let Some(value) = attached {
        return Ok(value);
    }
    let Some(value) = args.get(*index) else {
        return Err(ParseError::MissingValue(option));
    };
    *index += 1;
    Ok(value)
}

/// The value of a short option: the rest of its cluster (`-B1M`) or the
/// next argument (`-B 1M`).
fn take_cluster_value<'a>(
    rest: &'a str,
    args: &[&'a str],
    index: &mut usize,
    option: &'static str,
) -> Result<&'a str, ParseError> {
    if !rest.is_empty() {
        return Ok(rest);
    }
    let Some(value) = args.get(*index) else {
        return Err(ParseError::MissingValue(option));
    };
    *index += 1;
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::{parse, Command, Options, ParseError, SizeScale};
    use alloc::string::ToString;
    use alloc::vec;

    fn report(args: &[&str]) -> Options {
        match parse(args) {
            Ok(Command::Report(options)) => options,
            other => panic!("expected a report command, got {other:?}"),
        }
    }

    #[test]
    fn defaults_are_the_gnu_defaults() {
        let options = report(&[]);
        assert_eq!(options, Options::default());
        assert_eq!(options.scale, SizeScale::Blocks(1024));
    }

    #[test]
    fn help_switches_win() {
        assert_eq!(parse(&["-?"]), Ok(Command::Help));
        assert_eq!(parse(&["--help"]), Ok(Command::Help));
        assert_eq!(parse(&["-a?", "x"]), Ok(Command::Help));
    }

    #[test]
    fn h_is_human_readable_not_help() {
        // As in the GNU tool (and `ls`), `-h` scales sizes; short help is
        // `-?`/`--help`.
        assert_eq!(report(&["-h"]).scale, SizeScale::HumanBinary);
        assert_eq!(report(&["--human-readable"]).scale, SizeScale::HumanBinary);
        assert_eq!(report(&["--si"]).scale, SizeScale::HumanDecimal);
    }

    #[test]
    fn flags_cluster_and_operands_interleave() {
        let options = report(&["-acS0", "docs", "-k", "notes"]);
        assert!(options.all);
        assert!(options.grand_total);
        assert!(options.separate_dirs);
        assert!(options.null_terminated);
        assert_eq!(options.scale, SizeScale::Blocks(1024));
        assert_eq!(options.paths, vec!["docs".to_string(), "notes".to_string()]);
    }

    #[test]
    fn unit_selections_apply_the_last_one() {
        assert_eq!(report(&["-k", "-m"]).scale, SizeScale::Blocks(1024 * 1024));
        assert_eq!(report(&["-h", "-k"]).scale, SizeScale::Blocks(1024));
        assert_eq!(report(&["-B", "4K"]).scale, SizeScale::Blocks(4096));
        assert_eq!(report(&["-B4K"]).scale, SizeScale::Blocks(4096));
        assert_eq!(
            report(&["--block-size=1MiB"]).scale,
            SizeScale::Blocks(1024 * 1024)
        );
    }

    #[test]
    fn bytes_selects_apparent_size_in_single_bytes() {
        let options = report(&["-b"]);
        assert!(options.apparent_size);
        assert_eq!(options.scale, SizeScale::Blocks(1));
        // `--apparent-size` alone keeps the block scale.
        let options = report(&["--apparent-size"]);
        assert!(options.apparent_size);
        assert_eq!(options.scale, SizeScale::Blocks(1024));
    }

    #[test]
    fn depth_and_summarize_follow_the_gnu_rules() {
        assert_eq!(report(&["-d", "2"]).max_depth, Some(2));
        assert_eq!(report(&["-d2"]).max_depth, Some(2));
        assert_eq!(report(&["--max-depth=0"]).max_depth, Some(0));
        // `-s` is `--max-depth=0`.
        assert_eq!(report(&["-s"]).max_depth, Some(0));
        assert_eq!(
            parse(&["-s", "-a"]),
            Err(ParseError::SummarizeConflictsWithAll)
        );
        assert_eq!(
            parse(&["-s", "-d1"]),
            Err(ParseError::SummarizeConflictsWithDepth)
        );
    }

    #[test]
    fn malformed_values_fail_closed() {
        assert_eq!(
            parse(&["-B", "banana"]),
            Err(ParseError::BadBlockSize("banana".to_string()))
        );
        assert_eq!(parse(&["-B"]), Err(ParseError::MissingValue("-B")));
        assert_eq!(
            parse(&["--block-size"]),
            Err(ParseError::MissingValue("--block-size"))
        );
        assert_eq!(
            parse(&["-d", "-1"]),
            Err(ParseError::BadMaxDepth("-1".to_string()))
        );
        assert_eq!(
            parse(&["--nonsense"]),
            Err(ParseError::UnknownOption("--nonsense".to_string()))
        );
        assert_eq!(
            parse(&["-x"]),
            Err(ParseError::UnknownOption("-x".to_string()))
        );
    }

    #[test]
    fn double_dash_ends_options_and_dash_is_an_operand() {
        let options = report(&["--", "-a"]);
        assert!(!options.all);
        assert_eq!(options.paths, vec!["-a".to_string()]);
        let options = report(&["-"]);
        assert_eq!(options.paths, vec!["-".to_string()]);
    }
}
