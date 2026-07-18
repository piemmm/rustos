//! The parsed shape of an `ls` command line.

use alloc::string::String;
use alloc::vec::Vec;

use crate::error::LsError;

/// Which entries a directory listing includes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Hidden {
    /// Hide entries whose name begins with `.` (the default).
    Skip,
    /// Show every entry except `.` and `..` (`-A`).
    AlmostAll,
    /// Show every entry (`-a`).
    All,
}

/// The non-long output arrangement.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Format {
    /// One name per line (`-1`).
    OnePerLine,
    /// Multiple columns filled top-to-bottom, sized to the terminal width
    /// (`-C`; the GNU default when writing to a terminal).
    Columns,
    /// Multiple columns filled left-to-right, sized to the terminal width
    /// (`-x`).
    Across,
    /// Names separated by `, `, wrapped to the terminal width (`-m`).
    Commas,
}

/// The indicator suffix appended to a rendered name.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Indicator {
    /// No suffix (the default).
    None,
    /// `/` after directories (`-p`).
    Slash,
    /// `/` after directories and `*` after executables (`-F`).
    Classify,
}

/// The key rows are ordered by.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Sort {
    /// Lexicographic name order (the default).
    Name,
    /// Largest size first, ties by name (`-S`).
    Size,
}

/// The full option set of one `ls` listing.
///
/// The flags are the GNU tool's independent boolean switches; a
/// two-variant enum per flag would only restate `bool` with extra noise,
/// so the field count mirrors the command surface deliberately.
#[allow(clippy::struct_excessive_bools)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Options {
    /// Which dot-entries the listing includes (`-a` / `-A`).
    pub hidden: Hidden,
    /// Long format (`-l`; also implied by `-n`, `-g`, and `-o`).
    pub long: bool,
    /// List directory operands themselves, not their contents (`-d`).
    pub directory: bool,
    /// Recurse into subdirectories (`-R`).
    pub recursive: bool,
    /// Reverse the sort order (`-r`).
    pub reverse: bool,
    /// Sort key (`-S`).
    pub sort: Sort,
    /// Non-long arrangement (`-1` / `-C` / `-x` / `-m`). `None` selects the
    /// GNU default: multiple columns (`-C`) when the output is a terminal,
    /// one name per line otherwise (decided against the attested console at
    /// render time, not here).
    pub format: Option<Format>,
    /// Explicit output width in columns (`-w` / `--width`); `Some(0)` means
    /// an unlimited line length. `None` takes the attested terminal width,
    /// falling back to 80.
    pub width: Option<usize>,
    /// Name indicator suffixes (`-p` / `-F`).
    pub indicator: Indicator,
    /// Double-quote rendered names (`-Q`).
    pub quote: bool,
    /// Print each entry's allocated size in 1024-byte blocks (`-s`), plus
    /// a `total` line per directory block.
    pub size: bool,
    /// Human-readable sizes in the long format (`-h`).
    pub human_readable: bool,
    /// Omit the owner column from the long format (`-g`).
    pub hide_owner: bool,
    /// Omit the group column from the long format (`-o`).
    pub hide_group: bool,
}

impl Options {
    /// The defaults of a bare `ls`.
    pub const DEFAULT: Self = Self {
        hidden: Hidden::Skip,
        long: false,
        directory: false,
        recursive: false,
        reverse: false,
        sort: Sort::Name,
        format: None,
        width: None,
        indicator: Indicator::None,
        quote: false,
        size: false,
        human_readable: false,
        hide_owner: false,
        hide_group: false,
    };
}

/// One thing the `ls` tool can do.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Command {
    /// List `paths`, in operand order.
    List {
        /// The listing options.
        options: Options,
        /// The paths to list, in order. Never empty: an empty command line
        /// yields the single path `.`.
        paths: Vec<String>,
    },
    /// Render `ls`'s own short help (`-?`/`--help`): the `NAME`,
    /// `SYNOPSIS`, and compact `OPTIONS` of its Help document, through the
    /// same engine as any other command's short help (plans/APPS.md §4).
    Help,
}

/// Parse `args` (the tool's arguments, excluding the program name) into a
/// [`Command`].
///
/// The grammar is the GNU `ls` surface,
/// `ls [-aACdFghlmnopQrRsSx1] [-w cols] [--] [path...]`:
///
/// * `-a` / `--all` — do not hide entries whose name begins with `.`.
/// * `-A` / `--almost-all` — like `-a`, but never list `.` or `..`.
/// * `-C` — list entries in columns, filled top-to-bottom.
/// * `-d` / `--directory` — list directory operands themselves.
/// * `-F` / `--classify` — append `/` to directories and `*` to
///   executables.
/// * `-g` — long format without the owner column; implies `-l`.
/// * `-h` / `--human-readable` — human-readable sizes in the long format
///   (as in the GNU tool; short help is `-?` / `--help`).
/// * `-l` — long format.
/// * `-m` — comma-separated names, wrapped to the output width.
/// * `-n` / `--numeric-uid-gid` — long format with numeric owner and
///   group; implies `-l` (owner and group are always numeric here — see
///   the Help document).
/// * `-o` — long format without the group column; implies `-l`.
/// * `-p` — append `/` to directories.
/// * `-Q` / `--quote-name` — double-quote each rendered name.
/// * `-r` / `--reverse` — reverse the sort order.
/// * `-R` / `--recursive` — list subdirectories recursively.
/// * `-s` / `--size` — print each entry's allocated size in 1024-byte
///   blocks (scaled by `-h`), with a `total` line per directory block.
/// * `-S` — sort by size, largest first.
/// * `-w` / `--width <cols>` — set the output width; `0` means unlimited.
/// * `-x` — list entries in columns, filled left-to-right.
/// * `-1` — one name per line.
/// * `-?` / `--help` — the reserved short-help switches (plans/APPS.md §4;
///   they win immediately).
/// * `--` — end option parsing; every later argument is a path.
/// * any other `-…` — an [`LsError::Usage`] error (fail closed; never a
///   silently ignored token).
/// * anything else (including the bare `-`) — a path.
///
/// Short options may be combined into one argument (e.g. `-la` is
/// `-l -a`); an unrecognised letter anywhere in such a cluster is a usage
/// error. `-w` takes the rest of its cluster as its value (`-w80`), or the
/// next argument (`-w 80`) when it ends the cluster. The last of
/// `-1` / `-C` / `-x` / `-m` wins; any of `-l` / `-n` / `-g` / `-o`
/// selects the long format regardless of order; the later of `-p` / `-F`
/// wins.
///
/// With no path operand the single path is the current directory (`.`).
///
/// # Errors
///
/// [`LsError::Usage`] for any unrecognised option before `--`, a `-w` /
/// `--width` without a valid non-negative integer value, or a value given
/// to a long option that takes none.
pub fn parse(args: &[&str]) -> Result<Command, LsError> {
    let mut options = Options::DEFAULT;
    let mut paths = Vec::new();
    let mut options_done = false;
    let mut args = args.iter();
    while let Some(&arg) = args.next() {
        if options_done {
            paths.push(String::from(arg));
            continue;
        }
        if arg == "--" {
            options_done = true;
            continue;
        }
        if let Some(name) = arg.strip_prefix("--") {
            let (key, inline) = match name.split_once('=') {
                Some((key, value)) => (key, Some(value)),
                None => (name, None),
            };
            match key {
                "all" => options.hidden = Hidden::All,
                "almost-all" => options.hidden = Hidden::AlmostAll,
                "directory" => options.directory = true,
                "classify" => options.indicator = Indicator::Classify,
                "human-readable" => options.human_readable = true,
                "numeric-uid-gid" => options.long = true,
                "quote-name" => options.quote = true,
                "size" => options.size = true,
                "reverse" => options.reverse = true,
                "recursive" => options.recursive = true,
                "help" => return Ok(Command::Help),
                "width" => {
                    let raw = match inline {
                        Some(value) => value,
                        None => *args.next().ok_or(LsError::Usage)?,
                    };
                    options.width = Some(parse_width(raw)?);
                    continue;
                }
                _ => return Err(LsError::Usage),
            }
            // A value on a long option that takes none (`--all=x`) is a
            // usage error, not a silently ignored token.
            if inline.is_some() {
                return Err(LsError::Usage);
            }
            continue;
        }
        // A short-option cluster is a leading `-` followed by at least one
        // letter (the bare `-` is a path, not an option).
        if let Some(letters) = arg.strip_prefix('-').filter(|rest| !rest.is_empty()) {
            let mut rest = letters;
            while let Some(letter) = rest.chars().next() {
                rest = &rest[letter.len_utf8()..];
                match letter {
                    'a' => options.hidden = Hidden::All,
                    'A' => options.hidden = Hidden::AlmostAll,
                    'C' => options.format = Some(Format::Columns),
                    'd' => options.directory = true,
                    'F' => options.indicator = Indicator::Classify,
                    'g' => {
                        options.long = true;
                        options.hide_owner = true;
                    }
                    'h' => options.human_readable = true,
                    // `-n` selects the long format like `-l`; its
                    // numeric owner/group is already the only output.
                    'l' | 'n' => options.long = true,
                    'm' => options.format = Some(Format::Commas),
                    'o' => {
                        options.long = true;
                        options.hide_group = true;
                    }
                    'p' => options.indicator = Indicator::Slash,
                    'Q' => options.quote = true,
                    'r' => options.reverse = true,
                    'R' => options.recursive = true,
                    's' => options.size = true,
                    'S' => options.sort = Sort::Size,
                    // `-w` takes a value: the rest of the cluster, or the
                    // next argument when it ends the cluster.
                    'w' => {
                        let raw = if rest.is_empty() {
                            *args.next().ok_or(LsError::Usage)?
                        } else {
                            rest
                        };
                        options.width = Some(parse_width(raw)?);
                        break;
                    }
                    'x' => options.format = Some(Format::Across),
                    '1' => options.format = Some(Format::OnePerLine),
                    '?' => return Ok(Command::Help),
                    _ => return Err(LsError::Usage),
                }
            }
            continue;
        }
        paths.push(String::from(arg));
    }
    if paths.is_empty() {
        paths.push(String::from("."));
    }
    Ok(Command::List { options, paths })
}

/// Parse a `-w` / `--width` value: a non-negative decimal integer, where
/// `0` selects an unlimited line length (the GNU meaning). Anything else —
/// an empty string, a sign, a non-digit, or an overflowing value — is a
/// usage error, never a guessed-at width (fail closed).
fn parse_width(raw: &str) -> Result<usize, LsError> {
    raw.parse::<usize>().map_err(|_| LsError::Usage)
}

#[cfg(test)]
mod tests {
    use super::{parse, Command, Format, Hidden, Indicator, Options, Sort};
    use crate::error::LsError;
    use alloc::string::String;
    use alloc::vec::Vec;

    fn list(options: Options, paths: &[&str]) -> Command {
        Command::List {
            options,
            paths: paths.iter().map(|p| String::from(*p)).collect::<Vec<_>>(),
        }
    }

    #[test]
    fn no_arguments_lists_current_directory() {
        assert_eq!(parse(&[]), Ok(list(Options::DEFAULT, &["."])));
    }

    #[test]
    fn help_flags_win() {
        assert_eq!(parse(&["-?"]), Ok(Command::Help));
        assert_eq!(parse(&["--help"]), Ok(Command::Help));
        // `-?` wins even from inside a cluster and alongside other arguments.
        assert_eq!(parse(&["-l?", "dir"]), Ok(Command::Help));
    }

    #[test]
    fn h_is_human_readable_not_help() {
        // As in the GNU tool, `-h` scales sizes; it is not the help switch.
        assert_eq!(
            parse(&["-lh"]),
            Ok(list(
                Options {
                    long: true,
                    human_readable: true,
                    ..Options::DEFAULT
                },
                &["."],
            ))
        );
        assert_eq!(
            parse(&["--human-readable"]),
            Ok(list(
                Options {
                    human_readable: true,
                    ..Options::DEFAULT
                },
                &["."],
            ))
        );
    }

    #[test]
    fn all_and_long_flags_set_their_fields() {
        assert_eq!(
            parse(&["-a"]),
            Ok(list(
                Options {
                    hidden: Hidden::All,
                    ..Options::DEFAULT
                },
                &["."],
            ))
        );
        assert_eq!(
            parse(&["-l"]),
            Ok(list(
                Options {
                    long: true,
                    ..Options::DEFAULT
                },
                &["."],
            ))
        );
        assert_eq!(
            parse(&["--all", "-l"]),
            Ok(list(
                Options {
                    hidden: Hidden::All,
                    long: true,
                    ..Options::DEFAULT
                },
                &["."],
            ))
        );
    }

    #[test]
    fn almost_all_and_directory_parse() {
        assert_eq!(
            parse(&["-A", "-d"]),
            Ok(list(
                Options {
                    hidden: Hidden::AlmostAll,
                    directory: true,
                    ..Options::DEFAULT
                },
                &["."],
            ))
        );
        assert_eq!(
            parse(&["--almost-all", "--directory"]),
            Ok(list(
                Options {
                    hidden: Hidden::AlmostAll,
                    directory: true,
                    ..Options::DEFAULT
                },
                &["."],
            ))
        );
    }

    #[test]
    fn g_and_o_imply_long_and_hide_a_column() {
        assert_eq!(
            parse(&["-g"]),
            Ok(list(
                Options {
                    long: true,
                    hide_owner: true,
                    ..Options::DEFAULT
                },
                &["."],
            ))
        );
        assert_eq!(
            parse(&["-o"]),
            Ok(list(
                Options {
                    long: true,
                    hide_group: true,
                    ..Options::DEFAULT
                },
                &["."],
            ))
        );
    }

    #[test]
    fn numeric_uid_gid_implies_long() {
        for args in [["-n"], ["--numeric-uid-gid"]] {
            assert_eq!(
                parse(&args),
                Ok(list(
                    Options {
                        long: true,
                        ..Options::DEFAULT
                    },
                    &["."],
                ))
            );
        }
    }

    #[test]
    fn format_flags_last_one_wins() {
        assert_eq!(
            parse(&["-m"]),
            Ok(list(
                Options {
                    format: Some(Format::Commas),
                    ..Options::DEFAULT
                },
                &["."],
            ))
        );
        // The later format flag wins; an explicit `-1` is not the same as
        // the unset default (which decides against the terminal at render
        // time).
        assert_eq!(
            parse(&["-m", "-1"]),
            Ok(list(
                Options {
                    format: Some(Format::OnePerLine),
                    ..Options::DEFAULT
                },
                &["."],
            ))
        );
        assert_eq!(
            parse(&["-1", "-C"]),
            Ok(list(
                Options {
                    format: Some(Format::Columns),
                    ..Options::DEFAULT
                },
                &["."],
            ))
        );
        assert_eq!(
            parse(&["-Cx"]),
            Ok(list(
                Options {
                    format: Some(Format::Across),
                    ..Options::DEFAULT
                },
                &["."],
            ))
        );
    }

    #[test]
    fn width_parses_in_every_spelling() {
        // `-w 80` (separate argument), `-w80` (rest of the cluster),
        // `-lw80` (rest of a longer cluster), `--width 80`, `--width=80`.
        for args in [
            &["-w", "80"][..],
            &["-w80"][..],
            &["--width", "80"][..],
            &["--width=80"][..],
        ] {
            assert_eq!(
                parse(args),
                Ok(list(
                    Options {
                        width: Some(80),
                        ..Options::DEFAULT
                    },
                    &["."],
                ))
            );
        }
        assert_eq!(
            parse(&["-lw80"]),
            Ok(list(
                Options {
                    long: true,
                    width: Some(80),
                    ..Options::DEFAULT
                },
                &["."],
            ))
        );
        // `0` is the GNU "unlimited line length" value.
        assert_eq!(
            parse(&["-w0"]),
            Ok(list(
                Options {
                    width: Some(0),
                    ..Options::DEFAULT
                },
                &["."],
            ))
        );
    }

    #[test]
    fn a_bad_or_missing_width_is_a_usage_error() {
        assert_eq!(parse(&["-w"]), Err(LsError::Usage));
        assert_eq!(parse(&["--width"]), Err(LsError::Usage));
        assert_eq!(parse(&["-w", "wide"]), Err(LsError::Usage));
        assert_eq!(parse(&["--width=-5"]), Err(LsError::Usage));
        assert_eq!(parse(&["-wx"]), Err(LsError::Usage));
    }

    #[test]
    fn a_value_on_a_valueless_long_option_is_a_usage_error() {
        assert_eq!(parse(&["--all=yes"]), Err(LsError::Usage));
    }

    #[test]
    fn indicator_flags_last_one_wins() {
        assert_eq!(
            parse(&["-p"]),
            Ok(list(
                Options {
                    indicator: Indicator::Slash,
                    ..Options::DEFAULT
                },
                &["."],
            ))
        );
        assert_eq!(
            parse(&["-p", "-F"]),
            Ok(list(
                Options {
                    indicator: Indicator::Classify,
                    ..Options::DEFAULT
                },
                &["."],
            ))
        );
    }

    #[test]
    fn size_flag_parses_in_both_spellings() {
        for args in [["-s"], ["--size"]] {
            assert_eq!(
                parse(&args),
                Ok(list(
                    Options {
                        size: true,
                        ..Options::DEFAULT
                    },
                    &["."],
                ))
            );
        }
        // `-s` (allocated blocks) and `-S` (sort by size) stay distinct.
        assert_eq!(
            parse(&["-sS"]),
            Ok(list(
                Options {
                    size: true,
                    sort: Sort::Size,
                    ..Options::DEFAULT
                },
                &["."],
            ))
        );
    }

    #[test]
    fn sort_reverse_recursive_and_quote_parse() {
        assert_eq!(
            parse(&["-S", "-r", "-R", "-Q"]),
            Ok(list(
                Options {
                    sort: Sort::Size,
                    reverse: true,
                    recursive: true,
                    quote: true,
                    ..Options::DEFAULT
                },
                &["."],
            ))
        );
        assert_eq!(
            parse(&["--reverse", "--recursive", "--quote-name"]),
            Ok(list(
                Options {
                    reverse: true,
                    recursive: true,
                    quote: true,
                    ..Options::DEFAULT
                },
                &["."],
            ))
        );
    }

    #[test]
    fn clustered_short_options_set_all_their_fields() {
        assert_eq!(
            parse(&["-laF", "dir"]),
            Ok(list(
                Options {
                    hidden: Hidden::All,
                    long: true,
                    indicator: Indicator::Classify,
                    ..Options::DEFAULT
                },
                &["dir"],
            ))
        );
    }

    #[test]
    fn paths_preserve_order() {
        assert_eq!(
            parse(&["a", "b", "c"]),
            Ok(list(Options::DEFAULT, &["a", "b", "c"]))
        );
    }

    #[test]
    fn bare_dash_is_a_path() {
        assert_eq!(parse(&["-"]), Ok(list(Options::DEFAULT, &["-"])));
    }

    #[test]
    fn unknown_option_is_usage() {
        assert_eq!(parse(&["-Z"]), Err(LsError::Usage));
        assert_eq!(parse(&["--frobnicate"]), Err(LsError::Usage));
        // The retired invented synonym for `-l` stays retired.
        assert_eq!(parse(&["--long"]), Err(LsError::Usage));
        // An unknown letter inside an otherwise-valid cluster still fails.
        assert_eq!(parse(&["-lZ"]), Err(LsError::Usage));
    }

    #[test]
    fn double_dash_ends_options() {
        // After `--`, a leading-dash argument is a path, not an option.
        assert_eq!(parse(&["--", "-l"]), Ok(list(Options::DEFAULT, &["-l"])));
    }

    /// Every locale's `OPTIONS` section documents exactly the switches this
    /// parser accepts (`plans/APPS.md` §3.1): the flag tokens are
    /// language-neutral, so each translated document must carry the same
    /// keys as the canonical one. The documents are read from the bundle's
    /// own on-disk `Help/` tree — the single source the image builder plants
    /// — never a copy embedded in this crate.
    #[test]
    fn help_documents_the_parser_switches() {
        extern crate std;
        use alloc::format;
        use std::fs;

        let help_root = format!("{}/Help", env!("CARGO_MANIFEST_DIR"));
        let locales = tairix_help::REQUIRED_LOCALES;
        for locale in locales {
            let path = format!("{help_root}/{locale}/ls.md");
            let text = fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {path}: {e}"));
            for switch in [
                "`-a, --all`",
                "`-A, --almost-all`",
                "`-d, --directory`",
                "`-F, --classify`",
                "`-g`",
                "`-h, --human-readable`",
                "`-l`",
                "`-m`",
                "`-n, --numeric-uid-gid`",
                "`-o`",
                "`-p`",
                "`-Q, --quote-name`",
                "`-r, --reverse`",
                "`-R, --recursive`",
                "`-s, --size`",
                "`-S`",
                "`-C`",
                "`-x`",
                "`-w, --width <cols>`",
                "`-1`",
                "`-?`",
            ] {
                assert!(
                    text.contains(switch),
                    "{locale}/ls.md must document {switch}"
                );
            }
        }
    }
}
