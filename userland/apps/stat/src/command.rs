//! The parsed shape of a `stat` command line, and the format grammar its
//! output is rendered from.

use alloc::string::{String, ToString};
use alloc::vec::Vec;

use crate::error::StatError;

/// Whether the operands are described as files or as the filesystems that
/// hold them (`-f`).
///
/// The two readings have **different** specifier vocabularies — GNU's `%b`
/// is a file's block count in one and the volume's total block count in the
/// other — so this is one value the format parser is given rather than a
/// flag the renderer re-checks per field.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum Subject {
    /// Describe each operand's own node (the default).
    #[default]
    File,
    /// Describe the filesystem each operand lies on (`-f`).
    Filesystem,
}

/// How a format string ends each rendering.
///
/// GNU's two format switches differ in exactly this and nothing else, so it
/// is one value: `-c`/`--format` appends a newline and leaves backslashes
/// alone, `--printf` interprets escapes and appends nothing.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Trailer {
    /// `-c`/`--format`: a newline after each operand, escapes left as typed.
    Newline,
    /// `--printf`: no trailing newline, backslash escapes interpreted.
    None,
}

/// How a rendered field is padded to a width.
///
/// GNU's format directives take the printf-style flags and width, so a
/// report can line up in columns; this is that subset which has a meaning
/// for a field rendered as text. A flag with no such meaning — `+`, a
/// leading space, `#`, all of which only qualify a *numeric conversion* —
/// is refused by name rather than accepted and ignored.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Pad {
    /// `-`: pad on the right instead of the left.
    pub left_justify: bool,
    /// `0`: pad with zeroes instead of spaces. Applied only to an all-digit
    /// field, since a zero-padded name means nothing.
    pub zero: bool,
    /// The minimum width, in characters.
    pub width: usize,
    /// `.N`: truncate the field to at most `N` characters.
    pub precision: Option<usize>,
}

impl Pad {
    /// `text`, padded and truncated as this directive asks.
    #[must_use]
    pub fn apply(self, text: &str) -> String {
        let mut value = String::from(text);
        if let Some(precision) = self.precision {
            value.truncate(
                value
                    .char_indices()
                    .nth(precision)
                    .map_or(value.len(), |(index, _)| index),
            );
        }
        let len = value.chars().count();
        if len >= self.width {
            return value;
        }
        let fill = if self.zero && value.chars().all(|c| c.is_ascii_digit()) {
            '0'
        } else {
            ' '
        };
        let padding: String = core::iter::repeat_n(fill, self.width - len).collect();
        if self.left_justify {
            value.push_str(&padding);
            value
        } else {
            let mut out = padding;
            out.push_str(&value);
            out
        }
    }
}

/// One piece of a parsed format string.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Piece {
    /// Literal text, with any escapes already resolved.
    Text(String),
    /// A field to render, named by its GNU specifier letter, and how it is
    /// padded.
    Field(char, Pad),
}

/// The parsed switches of a `stat` run.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Options {
    /// What the operands describe (`-f`).
    pub subject: Subject,
    /// `-L`/`--dereference`: describe what a link names rather than the link.
    pub dereference: bool,
    /// The format, already parsed into pieces, and how it ends. [`None`] is
    /// the default full report.
    pub format: Option<(Vec<Piece>, Trailer)>,
    /// `-t`/`--terse`: the one-line summary form.
    pub terse: bool,
}

impl Options {
    /// The defaults of a bare `stat`.
    pub const DEFAULT: Self = Self {
        subject: Subject::File,
        dereference: false,
        format: None,
        terse: false,
    };
}

/// One thing the `stat` tool can do.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Command {
    /// Describe each operand.
    Describe {
        /// The parsed switches.
        options: Options,
        /// The path operands, in command-line order. Never empty.
        paths: Vec<String>,
    },
    /// Render `stat`'s own short help (`-?`/`--help`).
    Help,
}

/// Parse `args` (the tool's arguments, excluding the program name).
///
/// The grammar is the GNU `stat` surface,
/// `stat [-Lft] [-c FORMAT | --printf=FORMAT] [--] file...`:
///
/// * `-L` / `--dereference` — describe what a symbolic link names.
/// * `-f` / `--file-system` — describe the filesystem, not the file.
/// * `-c FORMAT` / `--format=FORMAT` — render `FORMAT` per operand,
///   followed by a newline.
/// * `--printf=FORMAT` — as `-c`, but interpret backslash escapes and
///   append no newline.
/// * `-t` / `--terse` — the one-line summary form.
/// * `-?` / `--help` — the reserved short-help switches (they win
///   immediately).
/// * `--` — end option parsing; every later argument is an operand.
///
/// The later of `-c` and `--printf` wins, as GNU's own last-one-wins
/// argument handling gives.
///
/// # Errors
///
/// * [`StatError::Usage`] — an unrecognised option, or `-c` without a
///   value.
/// * [`StatError::MissingOperand`] — no operand at all.
/// * [`StatError::UnknownSpecifier`] — a format naming a letter that is not
///   a specifier.
/// * [`StatError::Unsupported`] — a format naming a specifier this platform
///   cannot answer honestly.
pub fn parse(args: &[&str]) -> Result<Command, StatError> {
    let mut options = Options::DEFAULT;
    let mut format: Option<(String, Trailer)> = None;
    let mut paths: Vec<String> = Vec::new();
    let mut options_done = false;
    let mut args = args.iter();

    while let Some(&arg) = args.next() {
        if options_done {
            paths.push(String::from(arg));
            continue;
        }
        match arg {
            "--" => options_done = true,
            "-?" | "--help" => return Ok(Command::Help),
            _ if arg.starts_with("--") => apply_long(arg, &mut options, &mut format, &mut args)?,
            _ if arg.len() > 1 && arg.starts_with('-') => {
                apply_short(&arg[1..], &mut options, &mut format, &mut args)?;
            }
            _ => paths.push(String::from(arg)),
        }
    }

    if paths.is_empty() {
        return Err(StatError::MissingOperand);
    }
    // The format is parsed once, here, so an unknown or unanswerable
    // specifier is a usage failure before any path is touched rather than a
    // surprise partway through the operands.
    options.format = match format {
        Some((text, trailer)) => Some((parse_format(&text, trailer, options.subject)?, trailer)),
        None => None,
    };
    Ok(Command::Describe { options, paths })
}

/// Apply one long option (`--name` or `--name=value`).
fn apply_long<'a, I: Iterator<Item = &'a &'a str>>(
    arg: &str,
    options: &mut Options,
    format: &mut Option<(String, Trailer)>,
    args: &mut I,
) -> Result<(), StatError> {
    let (key, inline) = match arg[2..].split_once('=') {
        Some((key, value)) => (key, Some(value)),
        None => (&arg[2..], None),
    };
    // A value on a switch that takes none is a mistake, not a token to drop.
    let flag = |options: &mut Options, set: fn(&mut Options)| -> Result<(), StatError> {
        if inline.is_some() {
            return Err(StatError::Usage);
        }
        set(options);
        Ok(())
    };
    match key {
        "dereference" => flag(options, |o| o.dereference = true),
        "file-system" => flag(options, |o| o.subject = Subject::Filesystem),
        "terse" => flag(options, |o| o.terse = true),
        "format" | "printf" => {
            let trailer = if key == "format" {
                Trailer::Newline
            } else {
                Trailer::None
            };
            let value = match inline {
                Some(value) => String::from(value),
                None => String::from(*args.next().ok_or(StatError::Usage)?),
            };
            *format = Some((value, trailer));
            Ok(())
        }
        _ => Err(StatError::Usage),
    }
}

/// Apply one cluster of short options (the text after the leading `-`).
///
/// `-c` consumes the rest of the cluster as its value, or the next argument
/// when it ends the cluster.
fn apply_short<'a, I: Iterator<Item = &'a &'a str>>(
    cluster: &str,
    options: &mut Options,
    format: &mut Option<(String, Trailer)>,
    args: &mut I,
) -> Result<(), StatError> {
    for (index, flag) in cluster.char_indices() {
        match flag {
            'L' => options.dereference = true,
            'f' => options.subject = Subject::Filesystem,
            't' => options.terse = true,
            'c' => {
                let rest = &cluster[index + flag.len_utf8()..];
                let value = if rest.is_empty() {
                    String::from(*args.next().ok_or(StatError::Usage)?)
                } else {
                    String::from(rest)
                };
                *format = Some((value, Trailer::Newline));
                return Ok(());
            }
            _ => return Err(StatError::Usage),
        }
    }
    Ok(())
}

/// Split a format string into [`Piece`]s, validating every specifier against
/// `subject`'s vocabulary.
///
/// `%%` is a literal `%`; a `%` at the very end is literal too, as GNU
/// leaves it. Backslash escapes are resolved only under [`Trailer::None`]
/// (`--printf`), which is the sole difference GNU draws between its two
/// format switches.
///
/// # Errors
///
/// * [`StatError::UnknownSpecifier`] — a letter that is not a specifier of
///   `subject`'s vocabulary.
/// * [`StatError::Unsupported`] — a specifier this platform cannot answer
///   honestly, named with the reason.
pub(crate) fn parse_format(
    text: &str,
    trailer: Trailer,
    subject: Subject,
) -> Result<Vec<Piece>, StatError> {
    let mut pieces: Vec<Piece> = Vec::new();
    let mut literal = String::new();
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '%' => {
                let (pad, letter) = directive(&mut chars)?;
                match letter {
                    // A trailing `%` and `%%` are both a literal `%`: GNU
                    // leaves the first as typed rather than guessing at a
                    // specifier.
                    None | Some('%') => literal.push('%'),
                    Some(letter) => {
                        unsupported_reason(letter, subject).map_or(Ok(()), Err)?;
                        if !is_specifier(letter, subject) {
                            return Err(StatError::UnknownSpecifier(letter));
                        }
                        if !literal.is_empty() {
                            pieces.push(Piece::Text(core::mem::take(&mut literal)));
                        }
                        pieces.push(Piece::Field(letter, pad));
                    }
                }
            }
            '\\' if trailer == Trailer::None => match chars.next() {
                None => literal.push('\\'),
                Some(escape) => literal.push(unescape(escape)),
            },
            other => literal.push(other),
        }
    }
    if !literal.is_empty() {
        pieces.push(Piece::Text(literal));
    }
    Ok(pieces)
}

/// Read the flags, width, and precision after a `%`, returning them with
/// the specifier letter that follows (or [`None`] at end of string).
///
/// # Errors
///
/// [`StatError::UnknownSpecifier`] for a flag whose meaning is only ever a
/// numeric conversion's (`+`, a leading space, `#`) — refused by name
/// rather than accepted and quietly dropped.
fn directive(
    chars: &mut core::iter::Peekable<core::str::Chars<'_>>,
) -> Result<(Pad, Option<char>), StatError> {
    let mut pad = Pad::default();
    while let Some(&c) = chars.peek() {
        match c {
            '-' => pad.left_justify = true,
            '0' => pad.zero = true,
            '+' | ' ' | '#' => return Err(StatError::UnknownSpecifier(c)),
            _ => break,
        }
        chars.next();
    }
    pad.width = read_number(chars);
    if chars.peek() == Some(&'.') {
        chars.next();
        pad.precision = Some(read_number(chars));
    }
    Ok((pad, chars.next()))
}

/// Read a run of decimal digits, `0` for none.
///
/// A width longer than the report could use is clamped by saturating
/// arithmetic rather than wrapping into a small one.
fn read_number(chars: &mut core::iter::Peekable<core::str::Chars<'_>>) -> usize {
    let mut value = 0usize;
    while let Some(digit) = chars.peek().and_then(|c| c.to_digit(10)) {
        value = value.saturating_mul(10).saturating_add(digit as usize);
        chars.next();
    }
    value
}

/// The character a backslash escape stands for; an unknown escape keeps its
/// own letter, as GNU does rather than guessing.
const fn unescape(escape: char) -> char {
    match escape {
        'a' => '\u{7}',
        'b' => '\u{8}',
        'e' => '\u{1b}',
        'f' => '\u{c}',
        'n' => '\n',
        'r' => '\r',
        't' => '\t',
        'v' => '\u{b}',
        '0' => '\0',
        other => other,
    }
}

/// Whether `letter` names a field of `subject`'s vocabulary.
const fn is_specifier(letter: char, subject: Subject) -> bool {
    match subject {
        Subject::File => matches!(
            letter,
            'a' | 'A'
                | 'b'
                | 'B'
                | 'd'
                | 'D'
                | 'f'
                | 'F'
                | 'g'
                | 'h'
                | 'i'
                | 'm'
                | 'n'
                | 'N'
                | 'o'
                | 's'
                | 'u'
                | 'U'
                | 'w'
                | 'W'
                | 'x'
                | 'X'
                | 'y'
                | 'Y'
                | 'z'
                | 'Z'
        ),
        Subject::Filesystem => matches!(
            letter,
            'a' | 'b' | 'c' | 'd' | 'f' | 'i' | 'l' | 'n' | 's' | 'S' | 'T'
        ),
    }
}

/// The reason `letter` cannot be answered honestly on this platform, or
/// [`None`] when it can.
///
/// Each is a TAIRiX concept that genuinely differs rather than a field left
/// unimplemented, and each is refused with its reason instead of being
/// answered with a fabricated Linux-shaped value.
fn unsupported_reason(letter: char, subject: Subject) -> Option<StatError> {
    let reason = match (subject, letter) {
        (Subject::File, 'G') => {
            "there is no group-name directory: the System Information API \
             publishes user names only, so %g (the numeric id) is the honest field"
        }
        (Subject::File, 't' | 'T') => {
            "TAIRiX has no device special files, so a major/minor device \
             type would be a fabricated value"
        }
        (Subject::Filesystem, 't') => {
            "a TAIRiX volume has no numeric filesystem-type magic; %T names \
             the type the mount records"
        }
        _ => return None,
    };
    Some(StatError::Unsupported(letter, reason.to_string()))
}

#[cfg(test)]
#[path = "command/tests.rs"]
mod tests;
