//! The parsed shape of a `cp` command line.

use alloc::string::String;
use alloc::vec::Vec;

use crate::error::CpError;

/// What happens when a destination file already exists.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Clobber {
    /// Overwrite it (the default).
    Overwrite,
    /// Ask before overwriting (`-i`).
    Prompt,
    /// Skip it silently (`-n`).
    Skip,
}

/// How the destination operand is interpreted.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TargetMode {
    /// The last operand; copied *into* when it is an existing directory
    /// (the default).
    Inferred,
    /// An explicit target directory (`-t`); every operand is a source and
    /// the destination must be an existing directory.
    Directory,
    /// The last operand, always treated as a normal file (`-T`); exactly
    /// one source is permitted.
    NoDirectory,
}

/// The full option set of one `cp` run.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Options {
    /// Descend into and reproduce directories (`-r`/`-R`). Without it, a
    /// directory source is a [`CpError::IsDirectory`].
    pub recursive: bool,
    /// Remove a destination that cannot be created and retry once (`-f`).
    pub force: bool,
    /// The existing-destination policy (`-i` / `-n`, last one wins).
    pub clobber: Clobber,
    /// Report each copy on standard output (`-v`).
    pub verbose: bool,
    /// How the destination operand is interpreted (`-t` / `-T`).
    pub target_mode: TargetMode,
}

impl Options {
    /// The defaults of a bare `cp`.
    pub const DEFAULT: Self = Self {
        recursive: false,
        force: false,
        clobber: Clobber::Overwrite,
        verbose: false,
        target_mode: TargetMode::Inferred,
    };
}

/// One thing the `cp` tool can do.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Command {
    /// Copy `sources` to `dest`, in operand order.
    Copy {
        /// The copy options.
        options: Options,
        /// The source paths to copy, in order. Always at least one.
        sources: Vec<String>,
        /// The destination path. Under [`TargetMode::Inferred`], when it
        /// names an existing directory (and always when there is more than
        /// one source), each source is copied into it under the source's
        /// base name; under [`TargetMode::Directory`] it is the `-t`
        /// directory; under [`TargetMode::NoDirectory`] it is used exactly
        /// as given.
        dest: String,
    },
    /// Print the usage banner (`-h`/`--help`).
    Help,
}

/// Parse `args` (the tool's arguments, excluding the program name) into a
/// [`Command`].
///
/// The grammar is the GNU `cp` surface,
/// `cp [-finrRvT] [-t dir] [--] source... dest`:
///
/// * `-r` / `-R` / `--recursive` — copy directories and their contents.
/// * `-f` / `--force` — remove an unwritable destination and retry.
/// * `-i` / `--interactive` — ask before overwriting an existing file.
/// * `-n` / `--no-clobber` — never overwrite an existing file.
/// * `-v` / `--verbose` — report each copy.
/// * `-t dir` / `--target-directory=dir` — copy every source into `dir`.
/// * `-T` / `--no-target-directory` — treat the destination as a normal
///   file (exactly one source).
/// * `-h` / `--help` — print the usage banner (wins immediately).
/// * `--` — end option parsing; every later argument is a path.
/// * any other `-…` — a [`CpError::Usage`] error (fail closed; never a
///   silently ignored token).
/// * anything else (including the bare `-`) — a path.
///
/// Short options may be combined into one argument (e.g. `-rf` is
/// `-r -f`); an unrecognised letter anywhere in such a cluster is a usage
/// error. As in the GNU tool, the later of `-i` / `-n` wins, `-t` takes
/// its directory either attached (`-tdir`) or as the next argument, and
/// `-t` with `-T` is a usage error.
///
/// Without `-t`, the last path operand is the destination and the rest are
/// the sources; at least two path operands are required. With `-t`, every
/// path operand is a source and at least one is required.
///
/// # Errors
///
/// [`CpError::Usage`] for any unrecognised option before `--`, a missing
/// `-t` value, `-t` combined with `-T`, or too few path operands.
pub fn parse(args: &[&str]) -> Result<Command, CpError> {
    let mut options = Options::DEFAULT;
    let mut target_dir: Option<String> = None;
    let mut no_target_dir = false;
    let mut operands = Vec::new();
    let mut options_done = false;
    let mut iter = args.iter().copied();
    while let Some(arg) = iter.next() {
        if !options_done {
            if arg == "--" {
                options_done = true;
                continue;
            }
            if let Some(name) = arg.strip_prefix("--") {
                if let Some(dir) = name.strip_prefix("target-directory=") {
                    if dir.is_empty() {
                        return Err(CpError::Usage);
                    }
                    target_dir = Some(String::from(dir));
                    continue;
                }
                match name {
                    "recursive" => options.recursive = true,
                    "force" => options.force = true,
                    "interactive" => options.clobber = Clobber::Prompt,
                    "no-clobber" => options.clobber = Clobber::Skip,
                    "verbose" => options.verbose = true,
                    "no-target-directory" => no_target_dir = true,
                    "help" => return Ok(Command::Help),
                    _ => return Err(CpError::Usage),
                }
                continue;
            }
            // A short-option cluster is a leading `-` followed by at least one
            // letter (the bare `-` is a path, not an option).
            if let Some(letters) = arg.strip_prefix('-').filter(|rest| !rest.is_empty()) {
                for (pos, letter) in letters.char_indices() {
                    match letter {
                        'r' | 'R' => options.recursive = true,
                        'f' => options.force = true,
                        'i' => options.clobber = Clobber::Prompt,
                        'n' => options.clobber = Clobber::Skip,
                        'v' => options.verbose = true,
                        'T' => no_target_dir = true,
                        't' => {
                            // The value is the rest of the cluster
                            // (`-tdir`) or the next argument (`-t dir`).
                            let rest = &letters[pos + letter.len_utf8()..];
                            let dir = if rest.is_empty() {
                                iter.next().ok_or(CpError::Usage)?
                            } else {
                                rest
                            };
                            if dir.is_empty() {
                                return Err(CpError::Usage);
                            }
                            target_dir = Some(String::from(dir));
                            break;
                        }
                        'h' | '?' => return Ok(Command::Help),
                        _ => return Err(CpError::Usage),
                    }
                }
                continue;
            }
        }
        operands.push(String::from(arg));
    }
    if no_target_dir && target_dir.is_some() {
        return Err(CpError::Usage);
    }
    if no_target_dir {
        options.target_mode = TargetMode::NoDirectory;
    }
    let (sources, dest) = if let Some(dir) = target_dir {
        options.target_mode = TargetMode::Directory;
        if operands.is_empty() {
            return Err(CpError::Usage);
        }
        (operands, dir)
    } else {
        // The last operand is the destination; everything before it is a
        // source. `cp` needs at least one source and a destination.
        let Some(dest) = operands.pop() else {
            return Err(CpError::Usage);
        };
        if operands.is_empty() {
            return Err(CpError::Usage);
        }
        (operands, dest)
    };
    Ok(Command::Copy {
        options,
        sources,
        dest,
    })
}

#[cfg(test)]
mod tests {
    use super::{parse, Clobber, Command, Options, TargetMode};
    use crate::error::CpError;
    use alloc::string::{String, ToString};
    use alloc::vec::Vec;

    fn copy_with(options: Options, sources: &[&str], dest: &str) -> Command {
        Command::Copy {
            options,
            sources: sources.iter().map(|p| String::from(*p)).collect::<Vec<_>>(),
            dest: dest.to_string(),
        }
    }

    fn copy(recursive: bool, force: bool, sources: &[&str], dest: &str) -> Command {
        copy_with(
            Options {
                recursive,
                force,
                ..Options::DEFAULT
            },
            sources,
            dest,
        )
    }

    #[test]
    fn two_operands_parse_as_source_and_dest() {
        assert_eq!(parse(&["a", "b"]), Ok(copy(false, false, &["a"], "b")));
    }

    #[test]
    fn several_sources_precede_the_destination() {
        assert_eq!(
            parse(&["a", "b", "dst"]),
            Ok(copy(false, false, &["a", "b"], "dst"))
        );
    }

    #[test]
    fn too_few_operands_is_usage() {
        assert_eq!(parse(&[]), Err(CpError::Usage));
        assert_eq!(parse(&["only"]), Err(CpError::Usage));
        assert_eq!(parse(&["-r"]), Err(CpError::Usage));
    }

    #[test]
    fn recursive_and_force_spellings_parse() {
        for args in [
            ["-r", "a", "b"],
            ["-R", "a", "b"],
            ["--recursive", "a", "b"],
        ] {
            assert_eq!(parse(&args), Ok(copy(true, false, &["a"], "b")));
        }
        assert_eq!(parse(&["-rf", "a", "b"]), Ok(copy(true, true, &["a"], "b")));
        assert_eq!(
            parse(&["--force", "a", "b"]),
            Ok(copy(false, true, &["a"], "b"))
        );
    }

    #[test]
    fn interactive_and_no_clobber_last_one_wins() {
        assert_eq!(
            parse(&["-i", "a", "b"]),
            Ok(copy_with(
                Options {
                    clobber: Clobber::Prompt,
                    ..Options::DEFAULT
                },
                &["a"],
                "b",
            ))
        );
        assert_eq!(
            parse(&["-n", "a", "b"]),
            Ok(copy_with(
                Options {
                    clobber: Clobber::Skip,
                    ..Options::DEFAULT
                },
                &["a"],
                "b",
            ))
        );
        assert_eq!(
            parse(&["-i", "-n", "a", "b"]),
            Ok(copy_with(
                Options {
                    clobber: Clobber::Skip,
                    ..Options::DEFAULT
                },
                &["a"],
                "b",
            ))
        );
        assert_eq!(
            parse(&["--no-clobber", "--interactive", "a", "b"]),
            Ok(copy_with(
                Options {
                    clobber: Clobber::Prompt,
                    ..Options::DEFAULT
                },
                &["a"],
                "b",
            ))
        );
    }

    #[test]
    fn verbose_parses() {
        assert_eq!(
            parse(&["-v", "a", "b"]),
            Ok(copy_with(
                Options {
                    verbose: true,
                    ..Options::DEFAULT
                },
                &["a"],
                "b",
            ))
        );
    }

    #[test]
    fn target_directory_takes_a_value() {
        let expected = copy_with(
            Options {
                target_mode: TargetMode::Directory,
                ..Options::DEFAULT
            },
            &["a", "b"],
            "dst",
        );
        assert_eq!(parse(&["-t", "dst", "a", "b"]), Ok(expected.clone()));
        assert_eq!(parse(&["-tdst", "a", "b"]), Ok(expected.clone()));
        assert_eq!(parse(&["--target-directory=dst", "a", "b"]), Ok(expected));
        // A missing or empty value is a usage error.
        assert_eq!(parse(&["a", "-t"]), Err(CpError::Usage));
        assert_eq!(parse(&["--target-directory=", "a"]), Err(CpError::Usage));
        // With -t, at least one source is still required.
        assert_eq!(parse(&["-t", "dst"]), Err(CpError::Usage));
    }

    #[test]
    fn no_target_directory_parses_and_excludes_t() {
        assert_eq!(
            parse(&["-T", "a", "b"]),
            Ok(copy_with(
                Options {
                    target_mode: TargetMode::NoDirectory,
                    ..Options::DEFAULT
                },
                &["a"],
                "b",
            ))
        );
        assert_eq!(
            parse(&["--no-target-directory", "a", "b"]),
            Ok(copy_with(
                Options {
                    target_mode: TargetMode::NoDirectory,
                    ..Options::DEFAULT
                },
                &["a"],
                "b",
            ))
        );
        assert_eq!(parse(&["-T", "-t", "d", "a"]), Err(CpError::Usage));
    }

    #[test]
    fn help_flags_win() {
        assert_eq!(parse(&["-h"]), Ok(Command::Help));
        assert_eq!(parse(&["-?"]), Ok(Command::Help));
        assert_eq!(parse(&["--help"]), Ok(Command::Help));
        assert_eq!(parse(&["-rh", "a", "b"]), Ok(Command::Help));
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
        let locales = ["en-US", "fr-FR", "de-DE", "es-ES", "uk-UA", "it-IT"];
        for locale in locales {
            let path = format!("{help_root}/{locale}/cp.md");
            let text = fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {path}: {e}"));
            for switch in [
                "`-r, -R, --recursive`",
                "`-f, --force`",
                "`-i, --interactive`",
                "`-n, --no-clobber`",
                "`-v, --verbose`",
                "`-t dir, --target-directory=dir`",
                "`-T, --no-target-directory`",
                "`-h, -?, --help`",
            ] {
                assert!(
                    text.contains(switch),
                    "{locale}/cp.md must document {switch}"
                );
            }
        }
    }

    #[test]
    fn unknown_option_is_usage() {
        assert_eq!(parse(&["-x", "a", "b"]), Err(CpError::Usage));
        assert_eq!(parse(&["--frobnicate", "a", "b"]), Err(CpError::Usage));
        assert_eq!(parse(&["-rx", "a", "b"]), Err(CpError::Usage));
    }

    #[test]
    fn double_dash_ends_options() {
        assert_eq!(
            parse(&["--", "-r", "b"]),
            Ok(copy(false, false, &["-r"], "b"))
        );
    }

    #[test]
    fn bare_dash_is_a_path() {
        assert_eq!(parse(&["-", "b"]), Ok(copy(false, false, &["-"], "b")));
    }
}
