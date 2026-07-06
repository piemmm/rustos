//! The shapes of a parsed `head` command line, and their parser.
//!
//! The grammar is the GNU `head` surface: `-c`/`--bytes` and `-n`/`--lines`
//! (each taking a count with an optional leading `-` meaning "all but the
//! last COUNT" and an optional multiplier suffix), `-q`/`-v` header control,
//! `-z` NUL line delimiters, the reserved `-h`/`-?`/`--help` short-help
//! switches, `--` end-of-options, and the obsolete `-COUNT[bkm][lqvz]` form
//! accepted only as the first argument — exactly the forms the GNU tool
//! accepts, so a user or script that knows GNU `head` finds this one
//! familiar.

use alloc::string::String;
use alloc::vec::Vec;

use crate::error::HeadError;

/// One input of a `head` run.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Source {
    /// Standard input: the `-` operand, and the default when no operand is
    /// given.
    Stdin,
    /// A named file, read through the filesystem seam.
    Path(String),
}

/// What is counted, and how much of it to keep.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Count {
    /// The first (or, eliding, all but the last) `n` lines.
    Lines(u64),
    /// The first (or, eliding, all but the last) `n` bytes.
    Bytes(u64),
}

/// When the `==> name <==` header precedes each source's output.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HeaderMode {
    /// The default: headers only when more than one source is named.
    MultipleFiles,
    /// `-q`/`--quiet`/`--silent`: never.
    Never,
    /// `-v`/`--verbose`: always.
    Always,
}

/// A parsed, runnable `head` invocation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Job {
    /// How much of each source to emit.
    pub count: Count,
    /// `true` when the count came with a leading `-`: emit everything
    /// *except* the last `count` units.
    pub elide: bool,
    /// `-z`: lines are NUL-delimited instead of newline-delimited.
    pub zero: bool,
    /// Header policy for multi-source output.
    pub headers: HeaderMode,
    /// The inputs, in command-line order. Never empty (the no-operand
    /// default is a single [`Source::Stdin`]).
    pub sources: Vec<Source>,
}

/// One thing the `head` tool can do.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Command {
    /// Emit the selected part of each source.
    Head(Job),
    /// Render `head`'s own short help (`-h`/`-?`/`--help`) through the same
    /// engine as any other command's short help (plans/APPS.md §4).
    Help,
}

/// The default line count when no `-n`/`-c` is given, as in the GNU tool.
const DEFAULT_LINES: u64 = 10;

/// Parse `args` (the tool's arguments, excluding the program name) into a
/// [`Command`].
///
/// Options and operands may be interleaved (the GNU permutation); `--` ends
/// option parsing; a lone `-` is the standard-input operand. The obsolete
/// `-COUNT[bkm][lqvz]` form is honoured only as the first argument, exactly
/// as in the GNU tool; a digit in any later option cluster is the GNU
/// "invalid trailing option" diagnostic.
///
/// # Errors
///
/// The [`HeadError`] usage variants, mirroring the GNU diagnostics.
pub fn parse(args: &[&str]) -> Result<Command, HeadError> {
    let mut count: Option<Count> = None;
    let mut elide = false;
    let mut zero = false;
    let mut headers = HeaderMode::MultipleFiles;
    let mut sources: Vec<Source> = Vec::new();
    let mut options_done = false;

    let mut rest = args;
    if let Some(first) = args.first() {
        if let Some(parsed) = parse_obsolete(first, &mut zero, &mut headers)? {
            count = Some(parsed);
            rest = &args[1..];
        }
    }

    let mut index = 0;
    while index < rest.len() {
        let arg = rest[index];
        if options_done || arg == "-" || !arg.starts_with('-') {
            sources.push(if arg == "-" {
                Source::Stdin
            } else {
                Source::Path(String::from(arg))
            });
        } else {
            match arg {
                "--" => options_done = true,
                "-h" | "-?" | "--help" => return Ok(Command::Help),
                "--quiet" | "--silent" => headers = HeaderMode::Never,
                "--verbose" => headers = HeaderMode::Always,
                "--zero-terminated" => zero = true,
                "--bytes" | "--lines" => {
                    let lines = arg == "--lines";
                    index += 1;
                    let Some(&value) = rest.get(index) else {
                        return Err(HeadError::MissingValue(if lines {
                            "--lines"
                        } else {
                            "--bytes"
                        }));
                    };
                    count = Some(parse_value(value, lines, &mut elide)?);
                }
                _ if arg.starts_with("--bytes=") => {
                    let value = &arg["--bytes=".len()..];
                    count = Some(parse_value(value, false, &mut elide)?);
                }
                _ if arg.starts_with("--lines=") => {
                    let value = &arg["--lines=".len()..];
                    count = Some(parse_value(value, true, &mut elide)?);
                }
                _ if arg.starts_with("--") => {
                    return Err(HeadError::UnknownLong(String::from(arg)))
                }
                _ => {
                    let help = parse_cluster(
                        arg,
                        rest,
                        &mut index,
                        &mut count,
                        &mut elide,
                        &mut zero,
                        &mut headers,
                    )?;
                    if help {
                        return Ok(Command::Help);
                    }
                }
            }
        }
        index += 1;
    }

    if sources.is_empty() {
        sources.push(Source::Stdin);
    }
    Ok(Command::Head(Job {
        count: count.unwrap_or(Count::Lines(DEFAULT_LINES)),
        elide,
        zero,
        headers,
        sources,
    }))
}

/// Parse one short-option cluster (`-qn3`, `-vz`, …): `q`/`v`/`z` are
/// flags, `-c`/`-n` consume the rest of the token (or the next argument,
/// advancing `index`) as their value, and `?` is the reserved short-help
/// switch. Returns `Ok(true)` when the cluster asks for help.
fn parse_cluster(
    arg: &str,
    rest: &[&str],
    index: &mut usize,
    count: &mut Option<Count>,
    elide: &mut bool,
    zero: &mut bool,
    headers: &mut HeaderMode,
) -> Result<bool, HeadError> {
    for (offset, flag) in arg[1..].char_indices() {
        match flag {
            'q' => *headers = HeaderMode::Never,
            'v' => *headers = HeaderMode::Always,
            'z' => *zero = true,
            'c' | 'n' => {
                let lines = flag == 'n';
                let tail = &arg[1 + offset + flag.len_utf8()..];
                let value = if tail.is_empty() {
                    *index += 1;
                    match rest.get(*index) {
                        Some(&value) => value,
                        None => {
                            return Err(HeadError::MissingValue(if lines { "-n" } else { "-c" }))
                        }
                    }
                } else {
                    tail
                };
                *count = Some(parse_value(value, lines, elide)?);
                return Ok(false);
            }
            '?' => return Ok(true),
            _ if flag.is_ascii_digit() => return Err(HeadError::InvalidTrailing(flag)),
            _ => return Err(HeadError::UnknownShort(flag)),
        }
    }
    Ok(false)
}

/// Parse the obsolete `-COUNT[bkm][lqvz]...` first argument, mutating the
/// header/delimiter state its trailing letters set. Returns `Ok(None)` when
/// `first` is not that form (no dash-digit prefix), so the caller falls
/// through to ordinary option parsing.
fn parse_obsolete(
    first: &str,
    zero: &mut bool,
    headers: &mut HeaderMode,
) -> Result<Option<Count>, HeadError> {
    let mut chars = first.chars();
    if chars.next() != Some('-') {
        return Ok(None);
    }
    let body = &first[1..];
    if !body.starts_with(|c: char| c.is_ascii_digit()) {
        return Ok(None);
    }

    let digits_end = body
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(body.len());
    let digits = &body[..digits_end];
    let mut lines = true;
    let mut multiplier: u64 = 1;
    for letter in body[digits_end..].chars() {
        match letter {
            'c' => {
                lines = false;
                multiplier = 1;
            }
            'b' => {
                lines = false;
                multiplier = 512;
            }
            'k' => {
                lines = false;
                multiplier = 1024;
            }
            'm' => {
                lines = false;
                multiplier = 1024 * 1024;
            }
            'l' => lines = true,
            'q' => *headers = HeaderMode::Never,
            'v' => *headers = HeaderMode::Always,
            'z' => *zero = true,
            _ => return Err(HeadError::InvalidTrailing(letter)),
        }
    }

    let value = parse_decimal(digits).ok_or_else(|| invalid(digits, lines))?;
    let units = value.saturating_mul(multiplier);
    Ok(Some(if lines {
        Count::Lines(units)
    } else {
        Count::Bytes(units)
    }))
}

/// Parse a `-c`/`-n` value: an optional leading `-` (elide from the end,
/// recorded in `elide`) or `+`, decimal digits, and an optional multiplier
/// suffix from the GNU alphabet (`b` = 512; `k`/`K`/`m`/`M`/`G`/`T`/`P`/
/// `E`/`Z`/`Y`/`R`/`Q` = powers of 1024, or of 1000 with a trailing `B`,
/// or of 1024 with a trailing `iB`).
///
/// `elide` is reassigned on every call — the last `-c`/`-n` decides, as in
/// the GNU tool — so `-n -3 -n 4` ends up not eliding.
fn parse_value(text: &str, lines: bool, elide: &mut bool) -> Result<Count, HeadError> {
    let unsigned = if let Some(rest) = text.strip_prefix('-') {
        *elide = true;
        rest
    } else {
        *elide = false;
        text.strip_prefix('+').unwrap_or(text)
    };
    let units = parse_suffixed(unsigned).ok_or_else(|| invalid(text, lines))?;
    Ok(if lines {
        Count::Lines(units)
    } else {
        Count::Bytes(units)
    })
}

/// The count a decimal-plus-suffix spelling names, saturating at
/// [`u64::MAX`] (a count larger than every possible input is served
/// exactly by the maximum). `None` when the spelling is not a count.
fn parse_suffixed(text: &str) -> Option<u64> {
    let digits_end = text
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(text.len());
    if digits_end == 0 {
        return None;
    }
    let value = parse_decimal(&text[..digits_end])?;
    let multiplier = suffix_multiplier(&text[digits_end..])?;
    Some(value.saturating_mul(multiplier))
}

/// The multiplier a suffix spelling names (`""` is 1), or `None` for an
/// unknown suffix. Powers beyond what a `u64` holds saturate.
fn suffix_multiplier(suffix: &str) -> Option<u64> {
    if suffix.is_empty() {
        return Some(1);
    }
    if suffix == "b" {
        return Some(512);
    }
    let mut chars = suffix.chars();
    let letter = chars.next()?;
    let power: u32 = match letter {
        'k' | 'K' => 1,
        'm' | 'M' => 2,
        'G' => 3,
        'T' => 4,
        'P' => 5,
        'E' => 6,
        'Z' => 7,
        'Y' => 8,
        'R' => 9,
        'Q' => 10,
        _ => return None,
    };
    let base: u64 = match chars.as_str() {
        "" | "iB" => 1024,
        "B" => 1000,
        _ => return None,
    };
    Some(saturating_pow(base, power))
}

/// `base` raised to `power`, saturating at [`u64::MAX`].
fn saturating_pow(base: u64, power: u32) -> u64 {
    let mut value: u64 = 1;
    for _ in 0..power {
        value = value.saturating_mul(base);
    }
    value
}

/// Parse a non-empty all-digit spelling, saturating at [`u64::MAX`].
fn parse_decimal(digits: &str) -> Option<u64> {
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let mut value: u64 = 0;
    for digit in digits.bytes() {
        value = value
            .saturating_mul(10)
            .saturating_add(u64::from(digit - b'0'));
    }
    Some(value)
}

/// The invalid-count error for the unit being parsed.
fn invalid(text: &str, lines: bool) -> HeadError {
    if lines {
        HeadError::InvalidLines(String::from(text))
    } else {
        HeadError::InvalidBytes(String::from(text))
    }
}

#[cfg(test)]
mod tests {
    use alloc::string::String;
    use alloc::vec;

    use super::{parse, Command, Count, HeaderMode, Job, Source};
    use crate::error::HeadError;

    fn job(
        count: Count,
        elide: bool,
        zero: bool,
        headers: HeaderMode,
        sources: &[Source],
    ) -> Command {
        Command::Head(Job {
            count,
            elide,
            zero,
            headers,
            sources: sources.to_vec(),
        })
    }

    fn path(name: &str) -> Source {
        Source::Path(String::from(name))
    }

    #[test]
    fn defaults_to_ten_lines_of_stdin() {
        assert_eq!(
            parse(&[]),
            Ok(job(
                Count::Lines(10),
                false,
                false,
                HeaderMode::MultipleFiles,
                &[Source::Stdin]
            ))
        );
    }

    #[test]
    fn named_files_and_dash_stdin() {
        assert_eq!(
            parse(&["a", "-", "b"]),
            Ok(job(
                Count::Lines(10),
                false,
                false,
                HeaderMode::MultipleFiles,
                &[path("a"), Source::Stdin, path("b")]
            ))
        );
    }

    #[test]
    fn lines_and_bytes_options() {
        assert_eq!(
            parse(&["-n", "3", "f"]),
            Ok(job(
                Count::Lines(3),
                false,
                false,
                HeaderMode::MultipleFiles,
                &[path("f")]
            ))
        );
        assert_eq!(
            parse(&["-c5", "f"]),
            Ok(job(
                Count::Bytes(5),
                false,
                false,
                HeaderMode::MultipleFiles,
                &[path("f")]
            ))
        );
        assert_eq!(
            parse(&["--lines=2", "f"]),
            Ok(job(
                Count::Lines(2),
                false,
                false,
                HeaderMode::MultipleFiles,
                &[path("f")]
            ))
        );
        assert_eq!(
            parse(&["--bytes", "7", "f"]),
            Ok(job(
                Count::Bytes(7),
                false,
                false,
                HeaderMode::MultipleFiles,
                &[path("f")]
            ))
        );
    }

    #[test]
    fn leading_minus_elides_from_the_end() {
        assert_eq!(
            parse(&["-n", "-2", "f"]),
            Ok(job(
                Count::Lines(2),
                true,
                false,
                HeaderMode::MultipleFiles,
                &[path("f")]
            ))
        );
        assert_eq!(
            parse(&["-c", "-0", "f"]),
            Ok(job(
                Count::Bytes(0),
                true,
                false,
                HeaderMode::MultipleFiles,
                &[path("f")]
            ))
        );
    }

    #[test]
    fn the_last_count_decides_eliding() {
        // As in the GNU tool, each `-n`/`-c` reassigns the elide flag.
        assert_eq!(
            parse(&["-n", "-3", "-n", "4", "f"]),
            Ok(job(
                Count::Lines(4),
                false,
                false,
                HeaderMode::MultipleFiles,
                &[path("f")]
            ))
        );
    }

    #[test]
    fn plus_prefix_is_accepted() {
        assert_eq!(
            parse(&["-n", "+4"]),
            Ok(job(
                Count::Lines(4),
                false,
                false,
                HeaderMode::MultipleFiles,
                &[Source::Stdin]
            ))
        );
    }

    #[test]
    fn multiplier_suffixes() {
        assert_eq!(parse(&["-c", "1b"]), parse(&["-c", "512"]));
        assert_eq!(parse(&["-c", "2K"]), parse(&["-c", "2048"]));
        assert_eq!(parse(&["-c", "1kB"]), parse(&["-c", "1000"]));
        assert_eq!(parse(&["-c", "1MiB"]), parse(&["-c", "1048576"]));
        assert_eq!(parse(&["-c", "3MB"]), parse(&["-c", "3000000"]));
        // A count beyond u64 saturates: larger than any possible input.
        assert_eq!(
            parse(&["-c", "99Y"]),
            parse(&["-c", "18446744073709551615"])
        );
    }

    #[test]
    fn invalid_counts_are_diagnosed_per_unit() {
        assert_eq!(
            parse(&["-n", "x"]),
            Err(HeadError::InvalidLines(String::from("x")))
        );
        assert_eq!(
            parse(&["-c", "5J"]),
            Err(HeadError::InvalidBytes(String::from("5J")))
        );
        assert_eq!(
            parse(&["-c", ""]),
            Err(HeadError::InvalidBytes(String::new()))
        );
    }

    #[test]
    fn header_and_zero_flags() {
        assert_eq!(
            parse(&["-q", "a", "b"]),
            Ok(job(
                Count::Lines(10),
                false,
                false,
                HeaderMode::Never,
                &[path("a"), path("b")]
            ))
        );
        assert_eq!(
            parse(&["-vz", "a"]),
            Ok(job(
                Count::Lines(10),
                false,
                true,
                HeaderMode::Always,
                &[path("a")]
            ))
        );
        assert_eq!(
            parse(&["--silent", "--zero-terminated"]),
            Ok(job(
                Count::Lines(10),
                false,
                true,
                HeaderMode::Never,
                &[Source::Stdin]
            ))
        );
    }

    #[test]
    fn bundled_count_takes_the_token_tail() {
        assert_eq!(
            parse(&["-qn3", "f"]),
            Ok(job(
                Count::Lines(3),
                false,
                false,
                HeaderMode::Never,
                &[path("f")]
            ))
        );
    }

    #[test]
    fn obsolete_first_argument_form() {
        assert_eq!(
            parse(&["-5", "f"]),
            Ok(job(
                Count::Lines(5),
                false,
                false,
                HeaderMode::MultipleFiles,
                &[path("f")]
            ))
        );
        assert_eq!(
            parse(&["-5c", "f"]),
            Ok(job(
                Count::Bytes(5),
                false,
                false,
                HeaderMode::MultipleFiles,
                &[path("f")]
            ))
        );
        assert_eq!(
            parse(&["-2kqz"]),
            Ok(job(
                Count::Bytes(2048),
                false,
                true,
                HeaderMode::Never,
                &[Source::Stdin]
            ))
        );
        // `l` restores line counting but, as in the GNU tool, an earlier
        // multiplier letter still scales the number (`-3bl` is 1536 lines).
        assert_eq!(
            parse(&["-3bl"]),
            Ok(job(
                Count::Lines(1536),
                false,
                false,
                HeaderMode::MultipleFiles,
                &[Source::Stdin]
            ))
        );
        assert_eq!(parse(&["-5x"]), Err(HeadError::InvalidTrailing('x')));
    }

    #[test]
    fn obsolete_form_is_first_argument_only() {
        // Not the first argument: a digit in a cluster is the GNU
        // "invalid trailing option" diagnostic.
        assert_eq!(parse(&["-q", "-5"]), Err(HeadError::InvalidTrailing('5')));
    }

    #[test]
    fn double_dash_ends_options() {
        assert_eq!(
            parse(&["--", "-n"]),
            Ok(job(
                Count::Lines(10),
                false,
                false,
                HeaderMode::MultipleFiles,
                &[path("-n")]
            ))
        );
    }

    #[test]
    fn help_flags_win() {
        assert_eq!(parse(&["-h"]), Ok(Command::Help));
        assert_eq!(parse(&["-?"]), Ok(Command::Help));
        assert_eq!(parse(&["--help"]), Ok(Command::Help));
        assert_eq!(parse(&["-q?"]), Ok(Command::Help));
    }

    #[test]
    fn unknown_options_are_usage_errors() {
        assert_eq!(
            parse(&["--frob"]),
            Err(HeadError::UnknownLong(String::from("--frob")))
        );
        assert_eq!(parse(&["-x"]), Err(HeadError::UnknownShort('x')));
    }

    #[test]
    fn missing_values_are_diagnosed() {
        assert_eq!(parse(&["-n"]), Err(HeadError::MissingValue("-n")));
        assert_eq!(parse(&["--bytes"]), Err(HeadError::MissingValue("--bytes")));
    }

    #[test]
    fn options_and_operands_interleave() {
        assert_eq!(
            parse(&["f", "-q"]),
            Ok(job(
                Count::Lines(10),
                false,
                false,
                HeaderMode::Never,
                &[path("f")]
            ))
        );
        let sources = vec![path("f"), path("g")];
        assert_eq!(
            parse(&["f", "-n2", "g"]),
            Ok(job(
                Count::Lines(2),
                false,
                false,
                HeaderMode::MultipleFiles,
                &sources
            ))
        );
    }
}
