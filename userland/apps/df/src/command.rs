//! The `df` command shape and its parser.
//!
//! The option surface follows GNU coreutils `df`: `-a` includes the
//! pseudo/duplicate filesystems the default hides, `-T` adds the type
//! column, `-t`/`-x` filter by filesystem type, `-i` reports inodes,
//! `-P` selects the POSIX portable format, `--total` appends a summary
//! row, and the unit options (`-k`, `-h`, `-H`/`--si`, `-B <size>`)
//! select the reporting scale, the later selection winning as in the
//! GNU tool. `-l` restricts the report to local filesystems (every
//! TAIRiX mount today — the option filters nothing away, exactly as on
//! an all-local Linux machine). Short help is the reserved `-?`/`--help`
//! pair (plans/APPS.md §4); `-h` keeps its GNU meaning
//! (human-readable), exactly as `ls` and `du`.

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::fmt;

use tairix_util::size::{parse_block_size, SizeScale};

/// A parsed `df` invocation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Command {
    /// Render `df`'s own short help (`-?`/`--help`): the `NAME`,
    /// `SYNOPSIS`, and compact `OPTIONS` of its bundled help document,
    /// through the same engine as any other command's short help
    /// (plans/APPS.md §4).
    Help,
    /// Report the mounted filesystems' usage.
    Report(Options),
}

/// Everything a `df` run needs to know, straight from the option grammar.
// The bools mirror GNU `df`'s independent switches one-to-one (the `ls`/`rm`
// Options precedent); folding them into state enums would obscure the
// grammar they transcribe.
#[allow(clippy::struct_excessive_bools)]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Options {
    /// The `file` operands, in command-line order; empty reports every
    /// mounted filesystem.
    pub paths: Vec<String>,
    /// Include the pseudo and duplicate filesystems the default hides
    /// (`-a`).
    pub all: bool,
    /// Add the filesystem-type column (`-T`).
    pub print_type: bool,
    /// Report only filesystems of these types (`-t`, repeatable).
    pub types: Vec<String>,
    /// Exclude filesystems of these types (`-x`, repeatable).
    pub exclude_types: Vec<String>,
    /// Report inode counts instead of block usage (`-i`).
    pub inodes: bool,
    /// POSIX portable format (`-P`): the `1024-blocks`/`Capacity`
    /// header wording.
    pub portability: bool,
    /// Append a summary row labelled `total` (`--total`).
    pub grand_total: bool,
    /// The reporting scale; the default is 1024-byte blocks.
    pub scale: SizeScale,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            paths: Vec::new(),
            all: false,
            print_type: false,
            types: Vec::new(),
            exclude_types: Vec::new(),
            inodes: false,
            portability: false,
            grand_total: false,
            scale: SizeScale::Blocks(1024),
        }
    }
}

/// Why an argument vector was not a valid `df` invocation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ParseError {
    /// An option that is not part of the surface.
    UnknownOption(String),
    /// An option that requires a value reached the end of the arguments.
    MissingValue(&'static str),
    /// A `--block-size`/`-B` value that is not a valid GNU size.
    BadBlockSize(String),
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownOption(opt) => write!(f, "unrecognized option '{opt}'"),
            Self::MissingValue(opt) => write!(f, "option '{opt}' requires an argument"),
            Self::BadBlockSize(value) => write!(f, "invalid --block-size argument '{value}'"),
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
                "print-type" => options.print_type = true,
                "inodes" => options.inodes = true,
                "portability" => options.portability = true,
                "total" => options.grand_total = true,
                "local" => {}
                "human-readable" => options.scale = SizeScale::HumanBinary,
                "si" => options.scale = SizeScale::HumanDecimal,
                "type" => {
                    let value = take_value(attached, args, &mut index, "--type")?;
                    options.types.push(value.to_string());
                }
                "exclude-type" => {
                    let value = take_value(attached, args, &mut index, "--exclude-type")?;
                    options.exclude_types.push(value.to_string());
                }
                "block-size" => {
                    let value = take_value(attached, args, &mut index, "--block-size")?;
                    options.scale = parse_block_size(value)
                        .ok_or_else(|| ParseError::BadBlockSize(value.to_string()))?;
                }
                _ => return Err(ParseError::UnknownOption(arg.to_string())),
            }
            continue;
        }
        // A short-option cluster; `-B`, `-t`, and `-x` consume the rest
        // of the cluster (or the next argument) as their value.
        let mut cluster = arg[1..].chars();
        while let Some(flag) = cluster.next() {
            match flag {
                '?' => return Ok(Command::Help),
                'a' => options.all = true,
                'T' => options.print_type = true,
                'i' => options.inodes = true,
                'P' => options.portability = true,
                'l' => {}
                'h' => options.scale = SizeScale::HumanBinary,
                'H' => options.scale = SizeScale::HumanDecimal,
                'k' => options.scale = SizeScale::Blocks(1024),
                'B' => {
                    let value = take_cluster_value(cluster.as_str(), args, &mut index, "-B")?;
                    options.scale = parse_block_size(value)
                        .ok_or_else(|| ParseError::BadBlockSize(value.to_string()))?;
                    break;
                }
                't' => {
                    let value = take_cluster_value(cluster.as_str(), args, &mut index, "-t")?;
                    options.types.push(value.to_string());
                    break;
                }
                'x' => {
                    let value = take_cluster_value(cluster.as_str(), args, &mut index, "-x")?;
                    options.exclude_types.push(value.to_string());
                    break;
                }
                other => return Err(ParseError::UnknownOption(format!("-{other}"))),
            }
        }
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

/// The value of a short option: the rest of its cluster (`-t arxfs` as
/// `-tarxfs`) or the next argument.
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
    }

    #[test]
    fn h_is_human_readable_not_help() {
        // As in the GNU tool (and `ls`/`du`), `-h` scales sizes; short
        // help is `-?`/`--help`.
        assert_eq!(report(&["-h"]).scale, SizeScale::HumanBinary);
        assert_eq!(report(&["-H"]).scale, SizeScale::HumanDecimal);
        assert_eq!(report(&["--si"]).scale, SizeScale::HumanDecimal);
        assert_eq!(report(&["-h", "-k"]).scale, SizeScale::Blocks(1024));
    }

    #[test]
    fn flags_cluster_and_operands_interleave() {
        let options = report(&["-aTP", "/Users", "-i", "/Apps"]);
        assert!(options.all);
        assert!(options.print_type);
        assert!(options.portability);
        assert!(options.inodes);
        assert_eq!(
            options.paths,
            vec!["/Users".to_string(), "/Apps".to_string()]
        );
    }

    #[test]
    fn type_filters_accumulate() {
        let options = report(&[
            "-t",
            "arxfs",
            "-tmemfs",
            "--exclude-type=fat32",
            "-x",
            "ext4",
        ]);
        assert_eq!(
            options.types,
            vec!["arxfs".to_string(), "memfs".to_string()]
        );
        assert_eq!(
            options.exclude_types,
            vec!["fat32".to_string(), "ext4".to_string()]
        );
    }

    #[test]
    fn block_size_parses_the_gnu_grammar() {
        assert_eq!(report(&["-B", "4K"]).scale, SizeScale::Blocks(4096));
        assert_eq!(report(&["-B4K"]).scale, SizeScale::Blocks(4096));
        assert_eq!(
            report(&["--block-size=1MiB"]).scale,
            SizeScale::Blocks(1024 * 1024)
        );
    }

    #[test]
    fn local_is_accepted_and_filters_nothing() {
        // Every TAIRiX mount is local, so `-l`/`--local` is the identity
        // filter — accepted for GNU script compatibility.
        assert_eq!(report(&["-l"]), Options::default());
        assert_eq!(report(&["--local"]), Options::default());
    }

    #[test]
    fn malformed_values_fail_closed() {
        assert_eq!(
            parse(&["-B", "banana"]),
            Err(ParseError::BadBlockSize("banana".to_string()))
        );
        assert_eq!(parse(&["-B"]), Err(ParseError::MissingValue("-B")));
        assert_eq!(parse(&["-t"]), Err(ParseError::MissingValue("-t")));
        assert_eq!(
            parse(&["--nonsense"]),
            Err(ParseError::UnknownOption("--nonsense".to_string()))
        );
        assert_eq!(
            parse(&["-z"]),
            Err(ParseError::UnknownOption("-z".to_string()))
        );
    }

    #[test]
    fn double_dash_ends_options() {
        let options = report(&["--", "-a"]);
        assert!(!options.all);
        assert_eq!(options.paths, vec!["-a".to_string()]);
    }
}
