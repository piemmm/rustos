//! The parsed shape of an `ln` command line.

use alloc::string::String;
use alloc::vec::Vec;

use crate::error::LnError;

/// What to do with a link name that is already taken.
///
/// GNU keeps `-f` and `-i` as two flags that clear each other, so the last
/// one on the command line wins; one enum expresses that precedence directly.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Clobber {
    /// Refuse the link and report the taken name (the default).
    Refuse,
    /// Remove the existing name, then create the link (`-f`).
    Replace,
    /// Ask, and remove only on an affirmative reply (`-i`).
    Ask,
}

/// How the destination operand is read.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TargetMode {
    /// The default: one destination operand is a link *name* unless the
    /// filesystem says it is a directory (or a link to one), and three or
    /// more operands make the last a directory.
    Inferred,
    /// `-t dir`: every operand is a target and the links go in `dir`, which
    /// must already be a directory.
    Directory,
    /// `-T`: the destination is a link name, never a directory to fill, so
    /// exactly two operands are accepted.
    NoDirectory,
}

/// The full option set of one `ln` invocation.
///
/// The flags are the GNU tool's independent boolean switches; a two-variant
/// enum per flag would only restate `bool` with extra noise, so the field
/// count mirrors the command surface deliberately.
#[allow(clippy::struct_excessive_bools)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Options {
    /// Create symbolic links (`-s`). Without it a hard link is made: a
    /// second directory entry for the target's own inode.
    pub symbolic: bool,
    /// Hard-link what a symbolic-link target *names* rather than the link
    /// itself (`-L`; `-P` is the default and clears it). Meaningless for
    /// `-s`, which stores the target verbatim and resolves nothing.
    pub dereference_target: bool,
    /// Permit a directory operand (`-d` / `-F`). The attempt still fails:
    /// no principal may give a directory a second name, because the tree
    /// staying a tree is what makes physical `..` resolution well-defined.
    /// The switch only stops `ln` refusing the *command line*, matching what
    /// the GNU tool does on a system whose kernel refuses the operation.
    pub allow_directory: bool,
    /// What to do with a link name that is already taken (`-f` / `-i`).
    pub clobber: Clobber,
    /// Treat a destination that is a symbolic link to a directory as the
    /// plain name it also is, rather than a directory to create links in
    /// (`-n`).
    pub no_dereference: bool,
    /// Report each created link on standard output (`-v`).
    pub verbose: bool,
    /// How the destination operand is read (`-t` / `-T`).
    pub target_mode: TargetMode,
}

impl Options {
    /// The defaults of a bare `ln`.
    pub const DEFAULT: Self = Self {
        symbolic: false,
        dereference_target: false,
        allow_directory: false,
        clobber: Clobber::Refuse,
        no_dereference: false,
        verbose: false,
        target_mode: TargetMode::Inferred,
    };
}

/// One thing the `ln` tool can do.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Command {
    /// Create a link per target.
    Link {
        /// The link options.
        options: Options,
        /// The link targets, in operand order. Never empty.
        targets: Vec<String>,
        /// The destination operand: the directory `-t` named, the last
        /// operand otherwise, and `None` for the single-operand form (whose
        /// link takes the target's own leaf name in the working directory).
        destination: Option<String>,
    },
    /// Render `ln`'s own short help (`-h`/`-?`/`--help`): the `NAME`,
    /// `SYNOPSIS`, and compact `OPTIONS` of its Help document, through the
    /// same engine as any other command's short help (plans/APPS.md §4).
    Help,
}

/// Parse `args` (the tool's arguments, excluding the program name) into a
/// [`Command`].
///
/// The grammar is the GNU `ln` surface,
/// `ln [-sLPdFfinvT] [-t dir] [--] target... [link_name]`:
///
/// * `-s` / `--symbolic` — make symbolic links. Without it `ln` makes a
///   hard link: a second directory entry for the target's own inode.
/// * `-L` / `--logical` — hard-link what a symbolic-link target *names*.
/// * `-P` / `--physical` — hard-link the target as spelled, following no
///   final link. The default, and what POSIX `link()` does.
/// * `-d` / `-F` / `--directory` — accept a directory operand. The link
///   itself is still refused: no principal may give a directory a second
///   name.
/// * `-f` / `--force` — remove an existing link name and retry.
/// * `-i` / `--interactive` — ask before removing an existing link name.
///   The later of `-f` / `-i` wins.
/// * `-n` / `--no-dereference` — treat a destination that is a symbolic
///   link to a directory as the name it is, not the directory it names.
/// * `-v` / `--verbose` — print `'link' -> 'target'` for each link made.
/// * `-t dir` / `--target-directory=dir` — create every link in `dir`.
/// * `-T` / `--no-target-directory` — treat the destination as a link name
///   (exactly two operands).
/// * `-h` / `-?` / `--help` — the reserved short-help switches
///   (plans/APPS.md §4; they win immediately).
/// * `--` — end option parsing; every later argument is an operand.
/// * any other `-…` — a [`LnError::Usage`] (fail closed; never a silently
///   ignored token).
/// * anything else (including the bare `-`) — an operand.
///
/// Short options may be combined into one argument (`-sf` is `-s -f`); an
/// unrecognised letter anywhere in such a cluster is a usage error. `-t`
/// takes the rest of its cluster as its value (`-tdir`), or the next
/// argument (`-t dir`) when it ends the cluster.
///
/// Operand shapes, as the GNU tool reads them:
///
/// * one operand — the link is created in the working directory under the
///   target's own leaf name.
/// * two operands — the second is a directory to fill when it is one (or a
///   link to one, unless `-n`), and the link's name otherwise. `-T` forces
///   the name reading; `-t` makes both operands targets.
/// * three or more — the last must already be a directory (unless `-t`
///   named one, in which case they are all targets).
///
/// # Errors
///
/// * [`LnError::Usage`] — an unrecognised option, `-t` without a value, a
///   value given to a long option that takes none, or `-t` together with
///   `-T` (the two readings contradict).
/// * [`LnError::MissingOperand`] — no operand at all.
/// * [`LnError::MissingDestination`] — `-T` with a single operand.
/// * [`LnError::ExtraOperand`] — `-T` with three or more operands.
pub fn parse(args: &[&str]) -> Result<Command, LnError> {
    let mut options = Options::DEFAULT;
    let mut directory: Option<String> = None;
    let mut operands: Vec<String> = Vec::new();
    let mut options_done = false;
    let mut args = args.iter();

    while let Some(&arg) = args.next() {
        if options_done {
            operands.push(String::from(arg));
            continue;
        }
        match arg {
            "--" => options_done = true,
            "-h" | "-?" | "--help" => return Ok(Command::Help),
            _ if arg.starts_with("--") => {
                apply_long(arg, &mut options, &mut directory, &mut args)?;
            }
            _ if arg.len() > 1 && arg.starts_with('-') => {
                apply_short(&arg[1..], &mut options, &mut directory, &mut args)?;
            }
            _ => operands.push(String::from(arg)),
        }
    }

    // The two destination readings contradict, so a command line asking for
    // both is refused rather than one silently winning.
    if directory.is_some() && options.target_mode == TargetMode::NoDirectory {
        return Err(LnError::Usage);
    }
    if directory.is_some() {
        options.target_mode = TargetMode::Directory;
    }

    if operands.is_empty() {
        return Err(LnError::MissingOperand);
    }

    let (targets, destination) = match options.target_mode {
        // `-t dir`: every operand is a target.
        TargetMode::Directory => (operands, directory),
        TargetMode::NoDirectory => match operands.len() {
            1 => return Err(LnError::MissingDestination(operands.swap_remove(0))),
            2 => {
                let destination = operands.pop().unwrap_or_default();
                (operands, Some(destination))
            }
            _ => return Err(LnError::ExtraOperand(operands.swap_remove(2))),
        },
        TargetMode::Inferred => {
            if operands.len() == 1 {
                (operands, None)
            } else {
                let destination = operands.pop().unwrap_or_default();
                (operands, Some(destination))
            }
        }
    };

    Ok(Command::Link {
        options,
        targets,
        destination,
    })
}

/// Apply one long option (`--name` or `--name=value`).
fn apply_long<'a, I: Iterator<Item = &'a &'a str>>(
    arg: &str,
    options: &mut Options,
    directory: &mut Option<String>,
    args: &mut I,
) -> Result<(), LnError> {
    let (key, inline) = match arg[2..].split_once('=') {
        Some((key, value)) => (key, Some(value)),
        None => (&arg[2..], None),
    };
    // A value on a switch that takes none is a mistake, not a token to drop.
    let flag = |options: &mut Options, set: fn(&mut Options)| -> Result<(), LnError> {
        if inline.is_some() {
            return Err(LnError::Usage);
        }
        set(options);
        Ok(())
    };
    match key {
        "symbolic" => flag(options, |o| o.symbolic = true),
        "logical" => flag(options, |o| o.dereference_target = true),
        "physical" => flag(options, |o| o.dereference_target = false),
        "directory" => flag(options, |o| o.allow_directory = true),
        "force" => flag(options, |o| o.clobber = Clobber::Replace),
        "interactive" => flag(options, |o| o.clobber = Clobber::Ask),
        "no-dereference" => flag(options, |o| o.no_dereference = true),
        "verbose" => flag(options, |o| o.verbose = true),
        "no-target-directory" => flag(options, |o| o.target_mode = TargetMode::NoDirectory),
        "target-directory" => {
            let value = match inline {
                Some(value) => String::from(value),
                None => String::from(*args.next().ok_or(LnError::Usage)?),
            };
            *directory = Some(value);
            Ok(())
        }
        _ => Err(LnError::Usage),
    }
}

/// Apply one cluster of short options (the text after the leading `-`).
///
/// `-t` consumes the rest of the cluster as its value, or the next argument
/// when it ends the cluster.
fn apply_short<'a, I: Iterator<Item = &'a &'a str>>(
    cluster: &str,
    options: &mut Options,
    directory: &mut Option<String>,
    args: &mut I,
) -> Result<(), LnError> {
    for (index, flag) in cluster.char_indices() {
        match flag {
            's' => options.symbolic = true,
            'L' => options.dereference_target = true,
            'P' => options.dereference_target = false,
            'd' | 'F' => options.allow_directory = true,
            'f' => options.clobber = Clobber::Replace,
            'i' => options.clobber = Clobber::Ask,
            'n' => options.no_dereference = true,
            'v' => options.verbose = true,
            'T' => options.target_mode = TargetMode::NoDirectory,
            't' => {
                let rest = &cluster[index + flag.len_utf8()..];
                let value = if rest.is_empty() {
                    String::from(*args.next().ok_or(LnError::Usage)?)
                } else {
                    String::from(rest)
                };
                *directory = Some(value);
                return Ok(());
            }
            _ => return Err(LnError::Usage),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use alloc::string::String;
    use alloc::vec;
    use alloc::vec::Vec;

    use super::{parse, Clobber, Command, Options, TargetMode};
    use crate::error::LnError;

    fn link(options: Options, targets: &[&str], destination: Option<&str>) -> Command {
        Command::Link {
            options,
            targets: targets.iter().map(|&s| String::from(s)).collect(),
            destination: destination.map(String::from),
        }
    }

    fn symbolic() -> Options {
        Options {
            symbolic: true,
            ..Options::DEFAULT
        }
    }

    #[test]
    fn one_operand_takes_the_targets_own_name() {
        assert_eq!(
            parse(&["-s", "/a/b"]),
            Ok(link(symbolic(), &["/a/b"], None))
        );
    }

    #[test]
    fn two_operands_name_the_link() {
        assert_eq!(
            parse(&["-s", "/a/b", "c"]),
            Ok(link(symbolic(), &["/a/b"], Some("c")))
        );
    }

    #[test]
    fn three_operands_make_the_last_the_destination() {
        assert_eq!(
            parse(&["-s", "a", "b", "dir"]),
            Ok(link(symbolic(), &["a", "b"], Some("dir")))
        );
    }

    #[test]
    fn target_directory_makes_every_operand_a_target() {
        let expected = Options {
            target_mode: TargetMode::Directory,
            ..symbolic()
        };
        assert_eq!(
            parse(&["-s", "-t", "dir", "a", "b"]),
            Ok(link(expected, &["a", "b"], Some("dir")))
        );
        // The value may ride in the cluster or in a long option.
        assert_eq!(
            parse(&["-s", "-tdir", "a"]),
            Ok(link(expected, &["a"], Some("dir")))
        );
        assert_eq!(
            parse(&["-s", "--target-directory=dir", "a"]),
            Ok(link(expected, &["a"], Some("dir")))
        );
        assert_eq!(
            parse(&["-s", "--target-directory", "dir", "a"]),
            Ok(link(expected, &["a"], Some("dir")))
        );
    }

    #[test]
    fn no_target_directory_takes_exactly_two_operands() {
        let expected = Options {
            target_mode: TargetMode::NoDirectory,
            ..symbolic()
        };
        assert_eq!(
            parse(&["-sT", "a", "b"]),
            Ok(link(expected, &["a"], Some("b")))
        );
        assert_eq!(
            parse(&["-sT", "a"]),
            Err(LnError::MissingDestination(String::from("a")))
        );
        assert_eq!(
            parse(&["-sT", "a", "b", "c"]),
            Err(LnError::ExtraOperand(String::from("c")))
        );
    }

    #[test]
    fn the_two_destination_readings_cannot_both_hold() {
        assert_eq!(parse(&["-s", "-t", "dir", "-T", "a"]), Err(LnError::Usage));
    }

    #[test]
    fn the_later_of_force_and_interactive_wins() {
        let parsed = |args: &[&str]| match parse(args) {
            Ok(Command::Link { options, .. }) => options.clobber,
            other => panic!("expected a link command, got {other:?}"),
        };
        assert_eq!(parsed(&["-sfi", "a", "b"]), Clobber::Ask);
        assert_eq!(parsed(&["-sif", "a", "b"]), Clobber::Replace);
        assert_eq!(
            parsed(&["-s", "--force", "--interactive", "a"]),
            Clobber::Ask
        );
        assert_eq!(parsed(&["-s", "a"]), Clobber::Refuse);
    }

    #[test]
    fn flags_bundle_and_set_each_axis() {
        let expected = Options {
            symbolic: true,
            clobber: Clobber::Replace,
            no_dereference: true,
            verbose: true,
            ..Options::DEFAULT
        };
        assert_eq!(
            parse(&["-sfnv", "a", "b"]),
            Ok(link(expected, &["a"], Some("b")))
        );
        assert_eq!(
            parse(&[
                "--symbolic",
                "--force",
                "--no-dereference",
                "--verbose",
                "a",
                "b"
            ]),
            Ok(link(expected, &["a"], Some("b")))
        );
    }

    #[test]
    fn double_dash_ends_options() {
        assert_eq!(
            parse(&["-s", "--", "-v", "-n"]),
            Ok(link(symbolic(), &["-v"], Some("-n")))
        );
    }

    #[test]
    fn help_flags_win() {
        assert_eq!(parse(&["-h"]), Ok(Command::Help));
        assert_eq!(parse(&["-?"]), Ok(Command::Help));
        assert_eq!(parse(&["--help"]), Ok(Command::Help));
        assert_eq!(parse(&["-s", "a", "-h"]), Ok(Command::Help));
    }

    #[test]
    fn missing_operand_is_reported() {
        assert_eq!(parse(&[]), Err(LnError::MissingOperand));
        assert_eq!(parse(&["-s"]), Err(LnError::MissingOperand));
    }

    #[test]
    fn unknown_options_and_missing_values_fail_closed() {
        assert_eq!(parse(&["-x", "a"]), Err(LnError::Usage));
        assert_eq!(parse(&["--frob", "a"]), Err(LnError::Usage));
        // A bundle with one bad flag is refused whole.
        assert_eq!(parse(&["-sx", "a"]), Err(LnError::Usage));
        // `-t` at the end of the line has no value to take.
        assert_eq!(parse(&["-s", "-t"]), Err(LnError::Usage));
        // A value on a switch that takes none is a mistake, not a token to
        // ignore.
        assert_eq!(parse(&["--verbose=1", "a"]), Err(LnError::Usage));
    }

    #[test]
    fn a_lone_dash_is_an_operand() {
        assert_eq!(
            parse(&["-s", "-", "l"]),
            Ok(link(symbolic(), &["-"], Some("l")))
        );
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
        for locale in tairix_help::REQUIRED_LOCALES {
            let path = format!("{help_root}/{locale}/ln.md");
            let text = fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {path}: {e}"));
            for switch in [
                "`-s, --symbolic`",
                "`-f, --force`",
                "`-i, --interactive`",
                "`-n, --no-dereference`",
                "`-v, --verbose`",
                "`-t dir, --target-directory=dir`",
                "`-T, --no-target-directory`",
                "`-h, -?, --help`",
            ] {
                assert!(
                    text.contains(switch),
                    "{locale}/ln.md must document {switch}"
                );
            }
        }
    }

    #[test]
    fn without_s_the_options_still_parse() {
        // Whether a hard link can be made is the client's refusal, not the
        // parser's: the command line itself is well formed.
        let expected: Vec<String> = vec![String::from("a")];
        assert_eq!(
            parse(&["a", "b"]),
            Ok(Command::Link {
                options: Options::DEFAULT,
                targets: expected,
                destination: Some(String::from("b")),
            })
        );
    }
}
