//! The parsed shape of an `mv` command line.

use alloc::string::String;
use alloc::vec::Vec;

use crate::error::MvError;

/// What happens when a destination already exists.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Clobber {
    /// Overwrite it (the default, and `-f`).
    Overwrite,
    /// Ask before overwriting (`-i`).
    Prompt,
    /// Skip the source silently (`-n`).
    Skip,
}

/// How the destination operand is interpreted.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TargetMode {
    /// The last operand; moved *into* when it is an existing directory
    /// (the default).
    Inferred,
    /// An explicit target directory (`-t`); every operand is a source and
    /// the destination must be an existing directory.
    Directory,
    /// The last operand, always treated as a normal file (`-T`); exactly
    /// one source is permitted.
    NoDirectory,
}

/// The full option set of one `mv` run.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Options {
    /// Remove a destination that blocks an atomic rename and retry once
    /// (`-f`).
    pub force: bool,
    /// The existing-destination policy (`-f` / `-i` / `-n`, last one
    /// wins).
    pub clobber: Clobber,
    /// Report each move on standard output (`-v`).
    pub verbose: bool,
    /// How the destination operand is interpreted (`-t` / `-T`).
    pub target_mode: TargetMode,
}

impl Options {
    /// The defaults of a bare `mv`.
    pub const DEFAULT: Self = Self {
        force: false,
        clobber: Clobber::Overwrite,
        verbose: false,
        target_mode: TargetMode::Inferred,
    };
}

/// One thing the `mv` tool can do.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Command {
    /// Move `sources` to `dest`, in operand order.
    Move {
        /// The move options.
        options: Options,
        /// The source paths to move, in order. Always at least one.
        sources: Vec<String>,
        /// The destination path. Under [`TargetMode::Inferred`], when it
        /// names an existing directory (and always when there is more than
        /// one source), each source is moved into it under the source's
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
/// The grammar is the GNU `mv` surface,
/// `mv [-finvT] [-t dir] [--] source... dest`:
///
/// * `-f` / `--force` — remove a destination that blocks the rename and
///   retry; never prompt.
/// * `-i` / `--interactive` — ask before overwriting an existing
///   destination.
/// * `-n` / `--no-clobber` — never overwrite an existing destination.
/// * `-v` / `--verbose` — report each move.
/// * `-t dir` / `--target-directory=dir` — move every source into `dir`.
/// * `-T` / `--no-target-directory` — treat the destination as a normal
///   file (exactly one source).
/// * `-h` / `--help` — print the usage banner (wins immediately).
/// * `--` — end option parsing; every later argument is a path.
/// * any other `-…` — a [`MvError::Usage`] error (fail closed; never a
///   silently ignored token).
/// * anything else (including the bare `-`) — a path.
///
/// Short options may be combined into one argument (e.g. `-fn` is
/// `-f -n`); an unrecognised letter anywhere in such a cluster is a usage
/// error. As in the GNU tool, the last of `-f` / `-i` / `-n` wins, `-t`
/// takes its directory either attached (`-tdir`) or as the next argument,
/// and `-t` with `-T` is a usage error.
///
/// Without `-t`, the last path operand is the destination and the rest are
/// the sources; at least two path operands are required. With `-t`, every
/// path operand is a source and at least one is required.
///
/// # Errors
///
/// [`MvError::Usage`] for any unrecognised option before `--`, a missing
/// `-t` value, `-t` combined with `-T`, or too few path operands.
pub fn parse(args: &[&str]) -> Result<Command, MvError> {
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
                if apply_long(name, &mut options, &mut target_dir, &mut no_target_dir)? {
                    return Ok(Command::Help);
                }
                continue;
            }
            // A short-option cluster is a leading `-` followed by at least one
            // letter (the bare `-` is a path, not an option).
            if let Some(letters) = arg.strip_prefix('-').filter(|rest| !rest.is_empty()) {
                for (pos, letter) in letters.char_indices() {
                    match letter {
                        'f' => {
                            options.force = true;
                            options.clobber = Clobber::Overwrite;
                        }
                        'i' => {
                            options.clobber = Clobber::Prompt;
                            options.force = false;
                        }
                        'n' => {
                            options.clobber = Clobber::Skip;
                            options.force = false;
                        }
                        'v' => options.verbose = true,
                        'T' => no_target_dir = true,
                        't' => {
                            // The value is the rest of the cluster
                            // (`-tdir`) or the next argument (`-t dir`).
                            let rest = &letters[pos + letter.len_utf8()..];
                            let dir = if rest.is_empty() {
                                iter.next().ok_or(MvError::Usage)?
                            } else {
                                rest
                            };
                            if dir.is_empty() {
                                return Err(MvError::Usage);
                            }
                            target_dir = Some(String::from(dir));
                            break;
                        }
                        'h' | '?' => return Ok(Command::Help),
                        _ => return Err(MvError::Usage),
                    }
                }
                continue;
            }
        }
        operands.push(String::from(arg));
    }
    assemble(options, target_dir, no_target_dir, operands)
}

/// Apply one `--name` long option to the parse state.
///
/// Returns `Ok(true)` when the option was `--help`, which wins
/// immediately.
///
/// # Errors
///
/// [`MvError::Usage`] for an unrecognised long option or an empty
/// `--target-directory=` value.
fn apply_long(
    name: &str,
    options: &mut Options,
    target_dir: &mut Option<String>,
    no_target_dir: &mut bool,
) -> Result<bool, MvError> {
    if let Some(dir) = name.strip_prefix("target-directory=") {
        if dir.is_empty() {
            return Err(MvError::Usage);
        }
        *target_dir = Some(String::from(dir));
        return Ok(false);
    }
    match name {
        "force" => {
            options.force = true;
            options.clobber = Clobber::Overwrite;
        }
        "interactive" => {
            options.clobber = Clobber::Prompt;
            options.force = false;
        }
        "no-clobber" => {
            options.clobber = Clobber::Skip;
            options.force = false;
        }
        "verbose" => options.verbose = true,
        "no-target-directory" => *no_target_dir = true,
        "help" => return Ok(true),
        _ => return Err(MvError::Usage),
    }
    Ok(false)
}

/// Assemble the parsed options and operands into a [`Command::Move`],
/// resolving the destination per the `-t` / `-T` target mode.
fn assemble(
    mut options: Options,
    target_dir: Option<String>,
    no_target_dir: bool,
    mut operands: Vec<String>,
) -> Result<Command, MvError> {
    if no_target_dir && target_dir.is_some() {
        return Err(MvError::Usage);
    }
    if no_target_dir {
        options.target_mode = TargetMode::NoDirectory;
    }
    let (sources, dest) = if let Some(dir) = target_dir {
        options.target_mode = TargetMode::Directory;
        if operands.is_empty() {
            return Err(MvError::Usage);
        }
        (operands, dir)
    } else {
        // The last operand is the destination; everything before it is a
        // source. `mv` needs at least one source and a destination.
        let Some(dest) = operands.pop() else {
            return Err(MvError::Usage);
        };
        if operands.is_empty() {
            return Err(MvError::Usage);
        }
        (operands, dest)
    };
    Ok(Command::Move {
        options,
        sources,
        dest,
    })
}

#[cfg(test)]
mod tests {
    use super::{parse, Clobber, Command, Options, TargetMode};
    use crate::error::MvError;
    use alloc::string::String;
    use alloc::vec::Vec;

    fn mv_with(options: Options, sources: &[&str], dest: &str) -> Command {
        Command::Move {
            options,
            sources: sources.iter().map(|p| String::from(*p)).collect::<Vec<_>>(),
            dest: String::from(dest),
        }
    }

    fn mv(force: bool, no_clobber: bool, sources: &[&str], dest: &str) -> Command {
        mv_with(
            Options {
                force,
                clobber: if no_clobber {
                    Clobber::Skip
                } else {
                    Clobber::Overwrite
                },
                ..Options::DEFAULT
            },
            sources,
            dest,
        )
    }

    #[test]
    fn a_single_source_and_dest_parses() {
        assert_eq!(
            parse(&["a.txt", "b.txt"]),
            Ok(mv(false, false, &["a.txt"], "b.txt"))
        );
    }

    #[test]
    fn fewer_than_two_operands_is_usage() {
        assert_eq!(parse(&[]), Err(MvError::Usage));
        assert_eq!(parse(&["only"]), Err(MvError::Usage));
        assert_eq!(parse(&["-f"]), Err(MvError::Usage));
        assert_eq!(parse(&["-f", "only"]), Err(MvError::Usage));
    }

    #[test]
    fn help_flags_win() {
        assert_eq!(parse(&["-h"]), Ok(Command::Help));
        assert_eq!(parse(&["-?"]), Ok(Command::Help));
        assert_eq!(parse(&["--help"]), Ok(Command::Help));
        assert_eq!(parse(&["-fh", "a", "b"]), Ok(Command::Help));
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
        let locales = ["default", "fr-FR", "de-DE", "es-ES", "uk-UA", "it-IT"];
        for locale in locales {
            let path = format!("{help_root}/{locale}/mv.md");
            let text = fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {path}: {e}"));
            for switch in [
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
                    "{locale}/mv.md must document {switch}"
                );
            }
        }
    }

    #[test]
    fn several_sources_precede_the_destination() {
        assert_eq!(
            parse(&["a", "b", "dst"]),
            Ok(mv(false, false, &["a", "b"], "dst"))
        );
    }

    #[test]
    fn force_and_no_clobber_spellings_parse() {
        assert_eq!(parse(&["-f", "a", "b"]), Ok(mv(true, false, &["a"], "b")));
        assert_eq!(
            parse(&["--force", "a", "b"]),
            Ok(mv(true, false, &["a"], "b"))
        );
        assert_eq!(parse(&["-n", "a", "b"]), Ok(mv(false, true, &["a"], "b")));
        assert_eq!(
            parse(&["--no-clobber", "a", "b"]),
            Ok(mv(false, true, &["a"], "b"))
        );
    }

    #[test]
    fn force_interactive_and_no_clobber_last_one_wins() {
        // As in the GNU tool, the last of `-f` / `-i` / `-n` wins.
        assert_eq!(
            parse(&["-n", "-f", "a", "b"]),
            Ok(mv(true, false, &["a"], "b"))
        );
        assert_eq!(
            parse(&["-f", "-n", "a", "b"]),
            Ok(mv(false, true, &["a"], "b"))
        );
        assert_eq!(
            parse(&["-f", "-i", "a", "b"]),
            Ok(mv_with(
                Options {
                    clobber: Clobber::Prompt,
                    ..Options::DEFAULT
                },
                &["a"],
                "b",
            ))
        );
        assert_eq!(
            parse(&["-i", "-f", "a", "b"]),
            Ok(mv(true, false, &["a"], "b"))
        );
    }

    #[test]
    fn interactive_and_verbose_parse() {
        assert_eq!(
            parse(&["-iv", "a", "b"]),
            Ok(mv_with(
                Options {
                    clobber: Clobber::Prompt,
                    verbose: true,
                    ..Options::DEFAULT
                },
                &["a"],
                "b",
            ))
        );
        assert_eq!(
            parse(&["--interactive", "--verbose", "a", "b"]),
            Ok(mv_with(
                Options {
                    clobber: Clobber::Prompt,
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
        let expected = mv_with(
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
        assert_eq!(parse(&["a", "-t"]), Err(MvError::Usage));
        assert_eq!(parse(&["--target-directory=", "a"]), Err(MvError::Usage));
        assert_eq!(parse(&["-t", "dst"]), Err(MvError::Usage));
    }

    #[test]
    fn no_target_directory_parses_and_excludes_t() {
        assert_eq!(
            parse(&["-T", "a", "b"]),
            Ok(mv_with(
                Options {
                    target_mode: TargetMode::NoDirectory,
                    ..Options::DEFAULT
                },
                &["a"],
                "b",
            ))
        );
        assert_eq!(parse(&["-T", "-t", "d", "a"]), Err(MvError::Usage));
    }

    #[test]
    fn unknown_option_is_usage() {
        assert_eq!(parse(&["-x", "a", "b"]), Err(MvError::Usage));
        assert_eq!(parse(&["--frobnicate", "a", "b"]), Err(MvError::Usage));
        assert_eq!(parse(&["-fx", "a", "b"]), Err(MvError::Usage));
    }

    #[test]
    fn double_dash_ends_options() {
        assert_eq!(
            parse(&["--", "-f", "b"]),
            Ok(mv(false, false, &["-f"], "b"))
        );
    }

    #[test]
    fn bare_dash_is_a_path() {
        assert_eq!(parse(&["-", "b"]), Ok(mv(false, false, &["-"], "b")));
    }
}
