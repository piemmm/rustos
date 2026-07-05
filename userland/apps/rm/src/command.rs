//! The parsed shape of an `rm` command line.

use alloc::string::String;
use alloc::vec::Vec;

use crate::error::RmError;

/// When the tool asks before removing.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Interactive {
    /// Never prompt (the default, and `-f`).
    Never,
    /// Prompt once before removing more than three operands, or before a
    /// recursive removal (`-I`).
    Once,
    /// Prompt before every removal (`-i` / `--interactive`).
    Always,
}

/// The full option set of one `rm` run.
///
/// The flags are the GNU tool's independent boolean switches; a
/// two-variant enum per flag would only restate `bool` with extra noise,
/// so the field count mirrors the command surface deliberately.
#[allow(clippy::struct_excessive_bools)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Options {
    /// Descend into and remove directories (`-r`/`-R`). Without it, a
    /// directory operand is a [`RmError::IsDirectory`] (unless `dir`
    /// permits an empty one).
    pub recursive: bool,
    /// Ignore operands that do not exist, and remove nothing-to-report
    /// silently (`-f`).
    pub force: bool,
    /// Remove empty directories without `-r` (`-d`).
    pub dir: bool,
    /// Report each removal on standard output (`-v`).
    pub verbose: bool,
    /// The prompting policy (`-i` / `-I` / `-f`, last one wins).
    pub interactive: Interactive,
    /// Refuse to remove `/` (the default; cleared by
    /// `--no-preserve-root`).
    pub preserve_root: bool,
}

impl Options {
    /// The defaults of a bare `rm`.
    pub const DEFAULT: Self = Self {
        recursive: false,
        force: false,
        dir: false,
        verbose: false,
        interactive: Interactive::Never,
        preserve_root: true,
    };
}

/// One thing the `rm` tool can do.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Command {
    /// Remove `paths`, in operand order.
    Remove {
        /// The removal options.
        options: Options,
        /// The paths to remove, in order. May be empty only when `-f` is
        /// set (an empty `rm -f` removes nothing and succeeds).
        paths: Vec<String>,
    },
    /// Print the usage banner (`-h`/`--help`).
    Help,
}

/// Parse `args` (the tool's arguments, excluding the program name) into a
/// [`Command`].
///
/// The grammar is the GNU `rm` surface,
/// `rm [-dfiIrRv] [--] file...`:
///
/// * `-r` / `-R` / `--recursive` — remove directories and their contents.
/// * `-f` / `--force` — ignore non-existent operands; never prompt.
/// * `-d` / `--dir` — remove empty directories without `-r`.
/// * `-i` / `--interactive` — prompt before every removal.
/// * `-I` — prompt once before removing more than three operands, or
///   before a recursive removal.
/// * `-v` / `--verbose` — report each removal.
/// * `--preserve-root` — refuse to remove `/` (the default).
/// * `--no-preserve-root` — allow removing `/`.
/// * `-h` / `--help` — print the usage banner (wins immediately).
/// * `--` — end option parsing; every later argument is a path.
/// * any other `-…` — a [`RmError::Usage`] error (fail closed; never a
///   silently ignored token).
/// * anything else (including the bare `-`) — a path.
///
/// Short options may be combined into one argument (e.g. `-rf` is
/// `-r -f`); an unrecognised letter anywhere in such a cluster is a usage
/// error. As in the GNU tool, the later of `-f` / `-i` / `-I` wins: `-f`
/// cancels prompting and a prompt flag cancels `-f`.
///
/// # Errors
///
/// [`RmError::Usage`] for any unrecognised option before `--`, or when no
/// path operand is given without `-f`.
pub fn parse(args: &[&str]) -> Result<Command, RmError> {
    let mut options = Options::DEFAULT;
    let mut paths = Vec::new();
    let mut options_done = false;
    for &arg in args {
        if !options_done {
            if arg == "--" {
                options_done = true;
                continue;
            }
            if let Some(name) = arg.strip_prefix("--") {
                match name {
                    "recursive" => options.recursive = true,
                    "force" => {
                        options.force = true;
                        options.interactive = Interactive::Never;
                    }
                    "dir" => options.dir = true,
                    "verbose" => options.verbose = true,
                    "interactive" => {
                        options.interactive = Interactive::Always;
                        options.force = false;
                    }
                    "preserve-root" => options.preserve_root = true,
                    "no-preserve-root" => options.preserve_root = false,
                    "help" => return Ok(Command::Help),
                    _ => return Err(RmError::Usage),
                }
                continue;
            }
            // A short-option cluster is a leading `-` followed by at least one
            // letter (the bare `-` is a path, not an option).
            if let Some(letters) = arg.strip_prefix('-').filter(|rest| !rest.is_empty()) {
                for letter in letters.chars() {
                    match letter {
                        'r' | 'R' => options.recursive = true,
                        'f' => {
                            options.force = true;
                            options.interactive = Interactive::Never;
                        }
                        'd' => options.dir = true,
                        'v' => options.verbose = true,
                        'i' => {
                            options.interactive = Interactive::Always;
                            options.force = false;
                        }
                        'I' => {
                            options.interactive = Interactive::Once;
                            options.force = false;
                        }
                        'h' => return Ok(Command::Help),
                        _ => return Err(RmError::Usage),
                    }
                }
                continue;
            }
        }
        paths.push(String::from(arg));
    }
    if paths.is_empty() && !options.force {
        return Err(RmError::Usage);
    }
    Ok(Command::Remove { options, paths })
}

#[cfg(test)]
mod tests {
    use super::{parse, Command, Interactive, Options};
    use crate::error::RmError;
    use alloc::string::String;
    use alloc::vec::Vec;

    fn remove_with(options: Options, paths: &[&str]) -> Command {
        Command::Remove {
            options,
            paths: paths.iter().map(|p| String::from(*p)).collect::<Vec<_>>(),
        }
    }

    fn remove(recursive: bool, force: bool, paths: &[&str]) -> Command {
        remove_with(
            Options {
                recursive,
                force,
                ..Options::DEFAULT
            },
            paths,
        )
    }

    #[test]
    fn a_single_file_operand_parses() {
        assert_eq!(parse(&["a.txt"]), Ok(remove(false, false, &["a.txt"])));
    }

    #[test]
    fn no_operand_without_force_is_usage() {
        assert_eq!(parse(&[]), Err(RmError::Usage));
        assert_eq!(parse(&["-r"]), Err(RmError::Usage));
    }

    #[test]
    fn no_operand_with_force_succeeds_empty() {
        assert_eq!(parse(&["-f"]), Ok(remove(false, true, &[])));
    }

    #[test]
    fn recursive_spellings_parse() {
        for args in [["-r", "d"], ["-R", "d"], ["--recursive", "d"]] {
            assert_eq!(parse(&args), Ok(remove(true, false, &["d"])));
        }
    }

    #[test]
    fn clustered_short_options_parse() {
        assert_eq!(parse(&["-rf", "d"]), Ok(remove(true, true, &["d"])));
        assert_eq!(parse(&["-fR", "d"]), Ok(remove(true, true, &["d"])));
    }

    #[test]
    fn dir_and_verbose_parse() {
        assert_eq!(
            parse(&["-d", "-v", "e"]),
            Ok(remove_with(
                Options {
                    dir: true,
                    verbose: true,
                    ..Options::DEFAULT
                },
                &["e"],
            ))
        );
        assert_eq!(
            parse(&["--dir", "--verbose", "e"]),
            Ok(remove_with(
                Options {
                    dir: true,
                    verbose: true,
                    ..Options::DEFAULT
                },
                &["e"],
            ))
        );
    }

    #[test]
    fn interactive_flags_parse() {
        assert_eq!(
            parse(&["-i", "a"]),
            Ok(remove_with(
                Options {
                    interactive: Interactive::Always,
                    ..Options::DEFAULT
                },
                &["a"],
            ))
        );
        assert_eq!(
            parse(&["--interactive", "a"]),
            Ok(remove_with(
                Options {
                    interactive: Interactive::Always,
                    ..Options::DEFAULT
                },
                &["a"],
            ))
        );
        assert_eq!(
            parse(&["-I", "a"]),
            Ok(remove_with(
                Options {
                    interactive: Interactive::Once,
                    ..Options::DEFAULT
                },
                &["a"],
            ))
        );
    }

    #[test]
    fn force_and_interactive_last_one_wins() {
        // As in the GNU tool: `-f` cancels prompting, a prompt flag
        // cancels `-f`.
        assert_eq!(parse(&["-i", "-f", "a"]), Ok(remove(false, true, &["a"])));
        assert_eq!(
            parse(&["-f", "-i", "a"]),
            Ok(remove_with(
                Options {
                    interactive: Interactive::Always,
                    ..Options::DEFAULT
                },
                &["a"],
            ))
        );
    }

    #[test]
    fn preserve_root_toggles() {
        assert_eq!(
            parse(&["--no-preserve-root", "/"]),
            Ok(remove_with(
                Options {
                    preserve_root: false,
                    ..Options::DEFAULT
                },
                &["/"],
            ))
        );
        assert_eq!(
            parse(&["--no-preserve-root", "--preserve-root", "/"]),
            Ok(remove_with(Options::DEFAULT, &["/"]))
        );
    }

    #[test]
    fn help_flags_win() {
        assert_eq!(parse(&["-h"]), Ok(Command::Help));
        assert_eq!(parse(&["--help"]), Ok(Command::Help));
        assert_eq!(parse(&["-rh"]), Ok(Command::Help));
    }

    #[test]
    fn unknown_option_is_usage() {
        assert_eq!(parse(&["-x", "a"]), Err(RmError::Usage));
        assert_eq!(parse(&["--frobnicate", "a"]), Err(RmError::Usage));
        assert_eq!(parse(&["-rx", "a"]), Err(RmError::Usage));
    }

    #[test]
    fn double_dash_ends_options() {
        assert_eq!(parse(&["--", "-r"]), Ok(remove(false, false, &["-r"])));
    }

    #[test]
    fn bare_dash_is_a_path() {
        assert_eq!(parse(&["-"]), Ok(remove(false, false, &["-"])));
    }

    #[test]
    fn paths_preserve_order() {
        assert_eq!(
            parse(&["a", "b", "c"]),
            Ok(remove(false, false, &["a", "b", "c"]))
        );
    }
}
