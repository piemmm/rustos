//! The shapes of a parsed `wc` command line, and their parser.
//!
//! The grammar is the GNU `wc` surface: the count selectors `-c`/`--bytes`,
//! `-m`/`--chars`, `-l`/`--lines`, `-w`/`--words`, `-L`/`--max-line-length`
//! (none selected means the newline/word/byte default), `--total=WHEN`
//! (matched like GNU `argmatch`: an unambiguous prefix of `auto`, `always`,
//! `only`, `never`), `--files0-from=F` (NUL-separated operand list, which
//! excludes file operands), the reserved `-h`/`-?`/`--help` short-help
//! switches, and `--` end-of-options — exactly the forms the GNU tool
//! accepts, so a user or script that knows GNU `wc` finds this one
//! familiar.

use alloc::string::String;
use alloc::vec::Vec;

use crate::error::WcError;

/// Which counts a `wc` run prints, in the fixed GNU output order:
/// lines, words, chars, bytes, max line length.
///
/// The flags are the GNU tool's independent boolean selectors; a
/// two-variant enum per flag would only restate `bool` with extra noise,
/// so the field count mirrors the command surface deliberately.
#[allow(clippy::struct_excessive_bools)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Selection {
    /// `-l`: newline count.
    pub lines: bool,
    /// `-w`: word count.
    pub words: bool,
    /// `-m`: character count.
    pub chars: bool,
    /// `-c`: byte count.
    pub bytes: bool,
    /// `-L`: maximum display width of a line.
    pub max_line: bool,
}

impl Selection {
    /// The GNU default when no selector is given: lines, words, bytes.
    pub const DEFAULT: Self = Self {
        lines: true,
        words: true,
        chars: false,
        bytes: true,
        max_line: false,
    };

    /// No count selected yet (the state before defaulting).
    const NONE: Self = Self {
        lines: false,
        words: false,
        chars: false,
        bytes: false,
        max_line: false,
    };

    /// How many counts this selection prints per row.
    #[must_use]
    pub fn selected(&self) -> usize {
        usize::from(self.lines)
            + usize::from(self.words)
            + usize::from(self.chars)
            + usize::from(self.bytes)
            + usize::from(self.max_line)
    }
}

/// When the `total` row is printed (`--total`, GNU coreutils).
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum TotalMode {
    /// The default: a total row only when more than one operand is named.
    #[default]
    Auto,
    /// Always print the total row.
    Always,
    /// Print *only* the total row (no per-file rows, no `total` label).
    Only,
    /// Never print the total row.
    Never,
}

impl TotalMode {
    /// Match `text` against the mode names the way GNU `argmatch` does: an
    /// exact name or an unambiguous non-empty prefix (`au` names `auto`;
    /// `a` is ambiguous between `auto` and `always` and is refused).
    fn parse(text: &str) -> Result<Self, WcError> {
        const MODES: [(&str, TotalMode); 4] = [
            ("auto", TotalMode::Auto),
            ("always", TotalMode::Always),
            ("only", TotalMode::Only),
            ("never", TotalMode::Never),
        ];
        if text.is_empty() {
            return Err(WcError::InvalidTotal(String::new()));
        }
        let mut matched: Option<TotalMode> = None;
        for (name, mode) in MODES {
            if *name == *text {
                return Ok(mode);
            }
            if name.starts_with(text) {
                if matched.is_some() {
                    return Err(WcError::InvalidTotal(String::from(text)));
                }
                matched = Some(mode);
            }
        }
        matched.ok_or_else(|| WcError::InvalidTotal(String::from(text)))
    }
}

/// One named input of a `wc` run.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Operand {
    /// The explicit `-` operand: standard input, labelled `-` in the row.
    Dash,
    /// A named file, read through the filesystem seam.
    Path(String),
}

/// A parsed, runnable `wc` invocation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Job {
    /// The counts to print.
    pub select: Selection,
    /// When the total row appears.
    pub total: TotalMode,
    /// `--files0-from`: read the operand list, NUL-separated, from this
    /// file (`-` means standard input). Excludes `operands`.
    pub files0_from: Option<String>,
    /// The command-line operands. Empty means "count standard input,
    /// unlabelled" (unless `files0_from` supplies the list).
    pub operands: Vec<Operand>,
}

/// One thing the `wc` tool can do.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Command {
    /// Count the selected quantities of each input.
    Count(Job),
    /// Render `wc`'s own short help (`-h`/`-?`/`--help`) through the same
    /// engine as any other command's short help (plans/APPS.md §4).
    Help,
}

/// Parse `args` (the tool's arguments, excluding the program name) into a
/// [`Command`].
///
/// Options and operands may be interleaved (the GNU permutation); `--` ends
/// option parsing; a lone `-` is the labelled standard-input operand.
///
/// # Errors
///
/// The [`WcError`] usage variants, mirroring the GNU diagnostics —
/// including the GNU refusal to combine file operands with
/// `--files0-from`.
pub fn parse(args: &[&str]) -> Result<Command, WcError> {
    let mut select = Selection::NONE;
    let mut total = TotalMode::default();
    let mut files0_from: Option<String> = None;
    let mut operands: Vec<Operand> = Vec::new();
    let mut options_done = false;

    let mut index = 0;
    while index < args.len() {
        let arg = args[index];
        if options_done || arg == "-" || !arg.starts_with('-') {
            operands.push(if arg == "-" {
                Operand::Dash
            } else {
                Operand::Path(String::from(arg))
            });
        } else {
            match arg {
                "--" => options_done = true,
                "-h" | "-?" | "--help" => return Ok(Command::Help),
                "--bytes" => select.bytes = true,
                "--chars" => select.chars = true,
                "--lines" => select.lines = true,
                "--words" => select.words = true,
                "--max-line-length" => select.max_line = true,
                "--total" => {
                    index += 1;
                    let Some(&value) = args.get(index) else {
                        return Err(WcError::MissingValue("--total"));
                    };
                    total = TotalMode::parse(value)?;
                }
                "--files0-from" => {
                    index += 1;
                    let Some(&value) = args.get(index) else {
                        return Err(WcError::MissingValue("--files0-from"));
                    };
                    files0_from = Some(String::from(value));
                }
                _ if arg.starts_with("--total=") => {
                    total = TotalMode::parse(&arg["--total=".len()..])?;
                }
                _ if arg.starts_with("--files0-from=") => {
                    files0_from = Some(String::from(&arg["--files0-from=".len()..]));
                }
                _ if arg.starts_with("--") => return Err(WcError::UnknownLong(String::from(arg))),
                _ => {
                    for flag in arg[1..].chars() {
                        match flag {
                            'c' => select.bytes = true,
                            'm' => select.chars = true,
                            'l' => select.lines = true,
                            'w' => select.words = true,
                            'L' => select.max_line = true,
                            '?' => return Ok(Command::Help),
                            _ => return Err(WcError::UnknownShort(flag)),
                        }
                    }
                }
            }
        }
        index += 1;
    }

    if files0_from.is_some() && !operands.is_empty() {
        return Err(WcError::Files0Conflict);
    }
    if select == Selection::NONE {
        select = Selection::DEFAULT;
    }
    Ok(Command::Count(Job {
        select,
        total,
        files0_from,
        operands,
    }))
}

#[cfg(test)]
mod tests {
    use alloc::string::String;
    use alloc::vec;
    use alloc::vec::Vec;

    use super::{parse, Command, Job, Operand, Selection, TotalMode};
    use crate::error::WcError;

    fn job(
        select: Selection,
        total: TotalMode,
        files0_from: Option<&str>,
        operands: Vec<Operand>,
    ) -> Command {
        Command::Count(Job {
            select,
            total,
            files0_from: files0_from.map(String::from),
            operands,
        })
    }

    fn path(name: &str) -> Operand {
        Operand::Path(String::from(name))
    }

    #[test]
    fn defaults_to_lines_words_bytes_of_stdin() {
        assert_eq!(
            parse(&[]),
            Ok(job(Selection::DEFAULT, TotalMode::Auto, None, vec![]))
        );
    }

    #[test]
    fn selectors_accumulate() {
        let expected = Selection {
            lines: true,
            words: false,
            chars: true,
            bytes: false,
            max_line: true,
        };
        assert_eq!(
            parse(&["-l", "-m", "-L", "f"]),
            Ok(job(expected, TotalMode::Auto, None, vec![path("f")]))
        );
        assert_eq!(
            parse(&["-lmL", "f"]),
            Ok(job(expected, TotalMode::Auto, None, vec![path("f")]))
        );
        assert_eq!(
            parse(&["--lines", "--chars", "--max-line-length", "f"]),
            Ok(job(expected, TotalMode::Auto, None, vec![path("f")]))
        );
    }

    #[test]
    fn repeated_selectors_are_idempotent() {
        assert_eq!(parse(&["-ll", "-l"]), parse(&["-l"]));
    }

    #[test]
    fn dash_is_the_labelled_stdin_operand() {
        assert_eq!(
            parse(&["-", "f"]),
            Ok(job(
                Selection::DEFAULT,
                TotalMode::Auto,
                None,
                vec![Operand::Dash, path("f")]
            ))
        );
    }

    #[test]
    fn total_modes_parse_like_argmatch() {
        assert_eq!(
            parse(&["--total=never"]),
            Ok(job(Selection::DEFAULT, TotalMode::Never, None, vec![]))
        );
        assert_eq!(
            parse(&["--total", "only"]),
            Ok(job(Selection::DEFAULT, TotalMode::Only, None, vec![]))
        );
        // An unambiguous prefix is accepted; an ambiguous one is refused.
        assert_eq!(
            parse(&["--total=au"]),
            Ok(job(Selection::DEFAULT, TotalMode::Auto, None, vec![]))
        );
        assert_eq!(
            parse(&["--total=a"]),
            Err(WcError::InvalidTotal(String::from("a")))
        );
        assert_eq!(
            parse(&["--total=sometimes"]),
            Err(WcError::InvalidTotal(String::from("sometimes")))
        );
        assert_eq!(parse(&["--total"]), Err(WcError::MissingValue("--total")));
    }

    #[test]
    fn files0_from_excludes_operands() {
        assert_eq!(
            parse(&["--files0-from=list"]),
            Ok(job(
                Selection::DEFAULT,
                TotalMode::Auto,
                Some("list"),
                vec![]
            ))
        );
        assert_eq!(
            parse(&["--files0-from", "-"]),
            Ok(job(Selection::DEFAULT, TotalMode::Auto, Some("-"), vec![]))
        );
        assert_eq!(
            parse(&["--files0-from=list", "f"]),
            Err(WcError::Files0Conflict)
        );
        assert_eq!(
            parse(&["--files0-from"]),
            Err(WcError::MissingValue("--files0-from"))
        );
    }

    #[test]
    fn double_dash_ends_options() {
        assert_eq!(
            parse(&["--", "-l"]),
            Ok(job(
                Selection::DEFAULT,
                TotalMode::Auto,
                None,
                vec![path("-l")]
            ))
        );
    }

    #[test]
    fn help_flags_win() {
        assert_eq!(parse(&["-h"]), Ok(Command::Help));
        assert_eq!(parse(&["-?"]), Ok(Command::Help));
        assert_eq!(parse(&["--help"]), Ok(Command::Help));
        assert_eq!(parse(&["-l?"]), Ok(Command::Help));
    }

    #[test]
    fn unknown_options_are_usage_errors() {
        assert_eq!(
            parse(&["--frob"]),
            Err(WcError::UnknownLong(String::from("--frob")))
        );
        assert_eq!(parse(&["-x"]), Err(WcError::UnknownShort('x')));
    }

    #[test]
    fn options_and_operands_interleave() {
        assert_eq!(
            parse(&["f", "-l"]),
            Ok(job(
                Selection {
                    lines: true,
                    ..Selection::NONE
                },
                TotalMode::Auto,
                None,
                vec![path("f")]
            ))
        );
    }
}
