//! `%q` — quote a string so a shell reads it back as one word.
//!
//! The output shape follows GNU `printf`'s `%q` (coreutils `quotearg`'s
//! shell-escape style), pinned by the tests against the observed GNU
//! behaviour:
//!
//! * a string of only safe characters passes through bare;
//! * the empty string prints `''`;
//! * a string whose only special character is `'` wraps in double
//!   quotes (`"it's"`);
//! * everything else uses single-quote style: printable runs in
//!   `'…'`, each `'` spliced in as `\'`, and non-printable *bytes* —
//!   ASCII controls and everything outside `0x20..=0x7E`, including
//!   each byte of non-ASCII text — in `$'…'` groups using the C
//!   mnemonic escapes (`\t`, `\n`, …) or three-digit octal.

use alloc::string::String;

/// Quote `arg` for shell reuse (the `%q` conversion).
#[must_use]
pub fn shell_quote(arg: &str) -> String {
    if arg.is_empty() {
        return String::from("''");
    }
    if !needs_quoting(arg) {
        return String::from(arg);
    }
    if double_quotable(arg) {
        let mut out = String::with_capacity(arg.len() + 2);
        out.push('"');
        out.push_str(arg);
        out.push('"');
        return out;
    }
    single_quote_style(arg.as_bytes())
}

/// True when `arg` cannot pass through bare: any non-safe character, or
/// a leading `~`/`#` (special to a shell only at the start of a word).
fn needs_quoting(arg: &str) -> bool {
    let mut chars = arg.chars();
    match chars.next() {
        Some('~' | '#') => return true,
        Some(first) if !safe_char(first) => return true,
        Some(_) => {}
        None => return false,
    }
    chars.any(|c| !safe_char(c))
}

/// A character no shell treats specially anywhere in a word.
fn safe_char(c: char) -> bool {
    c.is_ascii_alphanumeric()
        || matches!(
            c,
            '%' | '+' | ',' | '-' | '.' | '/' | ':' | '@' | '_' | '{' | '}' | '~' | '#'
        )
}

/// True when wrapping in double quotes suffices: the string contains a
/// `'` but none of the characters that stay special inside `"…"`
/// (`"`, `$`, `` ` ``, `\`) and only printable ASCII.
fn double_quotable(arg: &str) -> bool {
    arg.contains('\'')
        && arg
            .bytes()
            .all(|b| printable(b) && !matches!(b, b'"' | b'$' | b'`' | b'\\'))
}

/// A byte the single-quote style may keep inside `'…'`: printable ASCII.
fn printable(b: u8) -> bool {
    (0x20..=0x7E).contains(&b)
}

/// The general form: printable runs in `'…'`, `'` as `\'`, non-printable
/// byte runs in `$'…'`. A string starting with an escape group carries
/// GNU's leading `''`; no trailing empty pair is emitted.
fn single_quote_style(bytes: &[u8]) -> String {
    use core::fmt::Write;

    let mut out = String::with_capacity(bytes.len() + 8);
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if !printable(b) {
            if i == 0 {
                out.push_str("''");
            }
            out.push_str("$'");
            while i < bytes.len() && !printable(bytes[i]) {
                match bytes[i] {
                    0x07 => out.push_str("\\a"),
                    0x08 => out.push_str("\\b"),
                    b'\t' => out.push_str("\\t"),
                    b'\n' => out.push_str("\\n"),
                    0x0B => out.push_str("\\v"),
                    0x0C => out.push_str("\\f"),
                    b'\r' => out.push_str("\\r"),
                    other => {
                        let _ = write!(out, "\\{other:03o}");
                    }
                }
                i += 1;
            }
            out.push('\'');
        } else if b == b'\'' {
            out.push_str("\\'");
            i += 1;
        } else {
            out.push('\'');
            while i < bytes.len() && printable(bytes[i]) && bytes[i] != b'\'' {
                out.push(char::from(bytes[i]));
                i += 1;
            }
            out.push('\'');
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::shell_quote;

    /// Every expectation here is the observed output of GNU coreutils
    /// `printf '%q'` for the same input.
    #[test]
    fn matches_gnu_printf_q() {
        assert_eq!(shell_quote("plain"), "plain");
        assert_eq!(shell_quote("a,b"), "a,b");
        assert_eq!(shell_quote("a.b"), "a.b");
        assert_eq!(shell_quote("a/b"), "a/b");
        assert_eq!(shell_quote("a-b"), "a-b");
        assert_eq!(shell_quote("a:b"), "a:b");
        assert_eq!(shell_quote("a@b"), "a@b");
        assert_eq!(shell_quote("a_b"), "a_b");
        assert_eq!(shell_quote("a%b"), "a%b");
        assert_eq!(shell_quote("a+b"), "a+b");
        assert_eq!(shell_quote("a{b"), "a{b");
        assert_eq!(shell_quote("a}b"), "a}b");
        assert_eq!(shell_quote("a~b"), "a~b");
        assert_eq!(shell_quote(""), "''");
        assert_eq!(shell_quote("a b"), "'a b'");
        assert_eq!(shell_quote("a=b"), "'a=b'");
        assert_eq!(shell_quote("a^b"), "'a^b'");
        assert_eq!(shell_quote("a!b"), "'a!b'");
        assert_eq!(shell_quote("a[b"), "'a[b'");
        assert_eq!(shell_quote("~ab"), "'~ab'");
        assert_eq!(shell_quote("#ab"), "'#ab'");
        assert_eq!(shell_quote("it's"), "\"it's\"");
        assert_eq!(shell_quote("'a"), "\"'a\"");
        assert_eq!(shell_quote("''"), "\"''\"");
        assert_eq!(shell_quote("a\"b"), "'a\"b'");
        assert_eq!(shell_quote("a'b\"c"), "'a'\\''b\"c'");
        assert_eq!(shell_quote("a\nb"), "'a'$'\\n''b'");
        assert_eq!(shell_quote("a\tb"), "'a'$'\\t''b'");
        assert_eq!(shell_quote("a\t\nb"), "'a'$'\\t\\n''b'");
        assert_eq!(shell_quote("\x01"), "''$'\\001'");
        assert_eq!(shell_quote("é é"), "''$'\\303\\251'' '$'\\303\\251'");
    }
}
