//! Value decoding and rendering: the bare and quoted forms, and the escapes.

use alloc::string::String;

use tairix_abi::appdata_ipc::APPDATA_VALUE_MAX;

use crate::ConfError;

/// Maximum length, in bytes, of a decoded value.
///
/// A fixed validation bound on untrusted input: a setting holds a short
/// scalar or a path, so this bounds the work one line can demand and the
/// memory one key can pin. A longer value makes its line unparsed — retained
/// verbatim, never truncated into a setting that means something else.
///
/// The number is the app-data channel's own value field width, imported for
/// the reason [`MAX_KEY_LEN`](crate::MAX_KEY_LEN) is.
pub const MAX_VALUE_LEN: usize = APPDATA_VALUE_MAX;

/// The permille value denoting "fully applied" — the upper bound of a
/// [`Document::permille`](crate::Document::permille) reading.
///
/// Fractions are spelled in permille integers rather than as decimals: a
/// permille round-trips through text exactly, needs no float parser in a
/// `no_std` build, and is already how the shipped effect strengths are
/// expressed.
pub const PERMILLE_FULL: u32 = 1000;

/// Decode a value's text as a boolean.
///
/// Accepts `true`/`false` and `on`/`off` — two spellings because both read
/// naturally to a hand-editor, and neither is ambiguous. A write always
/// renders `true`/`false`.
///
/// This is the one definition every layered reader shares: a document's own
/// accessor and a client that falls back across layers must never disagree
/// about what a value means.
///
/// # Errors
///
/// [`ConfError::ValueMalformed`] for anything outside those spellings.
pub fn as_bool(text: &str) -> Result<bool, ConfError> {
    match text {
        "true" | "on" => Ok(true),
        "false" | "off" => Ok(false),
        _ => Err(ConfError::ValueMalformed),
    }
}

/// Decode a value's text as an unsigned decimal.
///
/// # Errors
///
/// [`ConfError::ValueMalformed`] for a sign, a suffix, or an overflow — all
/// refusals, never a saturation.
pub fn as_u32(text: &str) -> Result<u32, ConfError> {
    text.parse::<u32>().map_err(|_| ConfError::ValueMalformed)
}

/// Decode a value's text as a signed decimal.
///
/// # Errors
///
/// [`ConfError::ValueMalformed`] for anything that is not a decimal `i64`.
pub fn as_i64(text: &str) -> Result<i64, ConfError> {
    text.parse::<i64>().map_err(|_| ConfError::ValueMalformed)
}

/// Decode a value's text as a permille fraction (`0..=`[`PERMILLE_FULL`]).
///
/// # Errors
///
/// [`ConfError::ValueMalformed`] for a non-decimal, or a value naming more
/// than [`PERMILLE_FULL`].
pub fn as_permille(text: &str) -> Result<u32, ConfError> {
    match as_u32(text)? {
        value if value <= PERMILLE_FULL => Ok(value),
        _ => Err(ConfError::ValueMalformed),
    }
}

/// The canonical text a boolean setting is written as.
///
/// One spelling for a write, two accepted for a read: a document the engine
/// wrote is always canonical, and one a human wrote is still understood.
#[must_use]
pub const fn bool_text(value: bool) -> &'static str {
    if value {
        "true"
    } else {
        "false"
    }
}

/// The two shapes a value can take on a line.
pub(crate) enum Form {
    /// Whitespace-trimmed text, ending at an unquoted `#`.
    Bare,
    /// `"…"` with `\\`, `\"`, `\n`, and `\t` escapes — the form a value needs
    /// when it carries leading or trailing space, a `#`, a quote, or a
    /// newline.
    Quoted,
}

/// Decode the value part of a setting line: everything after the `=`.
///
/// Returns the decoded value and the raw comment suffix (from the first
/// unquoted `#` to the end of the line, including the whitespace before it),
/// so a rewrite can put the user's own inline comment back.
///
/// # Errors
///
/// [`ConfError::ValueInvalid`] if the value is longer than
/// [`MAX_VALUE_LEN`], carries a character no value may hold, or is a quoted
/// form that is unterminated, holds an unknown escape, or has anything but a
/// comment after its closing quote. The caller turns that into an *unparsed
/// line* rather than a document error, so a hostile or fumbled line costs
/// only itself.
pub(crate) fn decode(rest: &str) -> Result<(String, &str), ConfError> {
    let trimmed = rest.trim_start();
    if trimmed.starts_with('"') {
        return decode_quoted(trimmed);
    }
    let (text, comment) = split_unquoted_comment(rest);
    let value = text.trim();
    check_bare(value)?;
    Ok((String::from(value), comment))
}

/// Split a line's value part at its first `#`, which in the bare form is
/// always a comment because a bare value may not contain one.
///
/// The comment half keeps the whitespace that preceded the marker, so a
/// rewrite of the line puts the user's own spacing back rather than jamming
/// their note against the new value.
fn split_unquoted_comment(rest: &str) -> (&str, &str) {
    let Some(at) = rest.find('#') else {
        return (rest, "");
    };
    let value = rest[..at].trim_end();
    (value, &rest[value.len()..])
}

/// Reject a bare value that carries a character the bare form cannot mean.
fn check_bare(value: &str) -> Result<(), ConfError> {
    if value.len() > MAX_VALUE_LEN {
        return Err(ConfError::ValueInvalid);
    }
    if value.chars().any(char::is_control) {
        return Err(ConfError::ValueInvalid);
    }
    Ok(())
}

/// Decode a `"…"` value, returning it and the raw comment suffix after the
/// closing quote.
fn decode_quoted(text: &str) -> Result<(String, &str), ConfError> {
    let mut out = String::new();
    let mut chars = text.char_indices();
    // The caller only reaches here on a leading quote.
    let _ = chars.next();
    let mut escaped = false;
    for (at, c) in chars {
        if escaped {
            out.push(match c {
                '\\' => '\\',
                '"' => '"',
                'n' => '\n',
                't' => '\t',
                // An unknown escape is a mistake, not an invitation to
                // guess what the user meant.
                _ => return Err(ConfError::ValueInvalid),
            });
            escaped = false;
            if out.len() > MAX_VALUE_LEN {
                return Err(ConfError::ValueInvalid);
            }
            continue;
        }
        match c {
            '\\' => escaped = true,
            '"' => {
                let tail = &text[at + 1..];
                // Only a comment may follow the closing quote: anything else
                // means the line was not the value it appeared to be.
                let comment = tail.trim_start();
                if !comment.is_empty() && !comment.starts_with('#') {
                    return Err(ConfError::ValueInvalid);
                }
                let comment = if comment.is_empty() { "" } else { tail };
                return Ok((out, comment));
            }
            // A literal control character inside quotes is refused; `\n` and
            // `\t` are the escapes that carry those meanings.
            _ if c.is_control() => return Err(ConfError::ValueInvalid),
            _ => {
                out.push(c);
                if out.len() > MAX_VALUE_LEN {
                    return Err(ConfError::ValueInvalid);
                }
            }
        }
    }
    // Ran off the end without a closing quote.
    Err(ConfError::ValueInvalid)
}

/// Whether `value` can be written bare, or must be quoted to survive a
/// round-trip.
pub(crate) fn form(value: &str) -> Form {
    let needs_quotes = value.is_empty()
        || value.trim() != value
        || value
            .chars()
            .any(|c| c == '#' || c == '"' || c == '\\' || c.is_control());
    if needs_quotes {
        Form::Quoted
    } else {
        Form::Bare
    }
}

/// Validate a value a caller is about to store.
///
/// # Errors
///
/// [`ConfError::ValueInvalid`] if the value is longer than
/// [`MAX_VALUE_LEN`], or carries a control character other than the `\n` and
/// `\t` the quoted form can spell.
pub(crate) fn validate(value: &str) -> Result<(), ConfError> {
    if value.len() > MAX_VALUE_LEN {
        return Err(ConfError::ValueInvalid);
    }
    if value
        .chars()
        .any(|c| c.is_control() && c != '\n' && c != '\t')
    {
        return Err(ConfError::ValueInvalid);
    }
    Ok(())
}

/// Render `value` into the form that reads back as exactly these bytes.
pub(crate) fn render(value: &str, out: &mut String) {
    match form(value) {
        Form::Bare => out.push_str(value),
        Form::Quoted => {
            out.push('"');
            for c in value.chars() {
                match c {
                    '\\' => out.push_str("\\\\"),
                    '"' => out.push_str("\\\""),
                    '\n' => out.push_str("\\n"),
                    '\t' => out.push_str("\\t"),
                    _ => out.push(c),
                }
            }
            out.push('"');
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{decode, render, MAX_VALUE_LEN};
    use crate::ConfError;
    use alloc::string::String;

    fn round_trip(value: &str) {
        let mut rendered = String::new();
        render(value, &mut rendered);
        let (decoded, comment) = decode(&rendered).expect("a rendered value decodes");
        assert_eq!(decoded, value, "rendered as `{rendered}`");
        assert_eq!(comment, "");
    }

    #[test]
    fn every_value_a_setting_may_hold_round_trips() {
        for value in [
            "dark",
            "14",
            "",
            " leading",
            "trailing ",
            "  both  ",
            "has # hash",
            "has \" quote",
            "has \\ backslash",
            "line\nbreak",
            "tab\there",
            "/Users/ada/Documents/notes.txt",
        ] {
            round_trip(value);
        }
    }

    #[test]
    fn a_bare_value_ends_at_its_comment() {
        // The whitespace the user put before their note is theirs, so it is
        // part of the comment a rewrite puts back, not part of the value.
        assert_eq!(
            decode(" dark  # why"),
            Ok((String::from("dark"), "  # why"))
        );
        assert_eq!(decode(" dark"), Ok((String::from("dark"), "")));
        assert_eq!(
            decode("#only a comment"),
            Ok((String::new(), "#only a comment"))
        );
    }

    #[test]
    fn a_hash_inside_quotes_is_a_literal_not_a_comment() {
        assert_eq!(
            decode(" \"a # b\" # real comment"),
            Ok((String::from("a # b"), " # real comment"))
        );
    }

    #[test]
    fn a_quoted_value_refuses_what_it_cannot_mean() {
        // Unterminated, unknown escape, trailing junk, literal control char.
        for text in [
            " \"unterminated",
            " \"bad \\z escape\"",
            " \"closed\" then junk",
            " \"raw\nnewline\"",
        ] {
            assert_eq!(
                decode(text),
                Err(ConfError::ValueInvalid),
                "`{text}` must be refused"
            );
        }
    }

    #[test]
    fn an_over_long_value_is_refused_in_both_forms() {
        let long: String = core::iter::repeat_n('x', MAX_VALUE_LEN + 1).collect();
        assert_eq!(decode(&long), Err(ConfError::ValueInvalid));
        let quoted = alloc::format!("\"{long}\"");
        assert_eq!(decode(&quoted), Err(ConfError::ValueInvalid));
    }
}
