//! A minimal, fail-closed XML element scanner for the SVG subset.
//!
//! The decoder does not need a general XML tree: every WM/desktop asset is a
//! flat list of shape elements under one `<svg>` root, so the scanner yields
//! the document's **start tags** in order and ignores text, comments,
//! processing instructions, the doctype, and closing tags. Anything
//! structurally broken — an unterminated tag, quote, or comment — is a
//! [`SvgError::Malformed`] rejection, never a panic (`AGENTS.md` §2.9).
//!
//! Slicing is always at ASCII delimiters (`<`, `>`, `=`, quotes, whitespace),
//! and every UTF-8 continuation byte is `>= 0x80`, so byte offsets never split
//! a multi-byte character: the `&str` slices below are always on char
//! boundaries.

use alloc::vec::Vec;

use crate::error::SvgError;

/// One start tag: its element name and ordered attribute list.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Element<'a> {
    /// The element's local name (e.g. `svg`, `polygon`).
    pub name: &'a str,
    /// The attributes in document order, each a `(key, value)` pair.
    pub attrs: Vec<(&'a str, &'a str)>,
}

impl<'a> Element<'a> {
    /// The value of attribute `key`, or `None` if the element lacks it.
    #[must_use]
    pub fn attr(&self, key: &str) -> Option<&'a str> {
        self.attrs.iter().find(|(k, _)| *k == key).map(|(_, v)| *v)
    }
}

/// Scan every start tag in `input`, in document order.
///
/// # Errors
/// Returns [`SvgError::Malformed`] for an unterminated tag, attribute quote,
/// or comment.
pub fn scan(input: &str) -> Result<Vec<Element<'_>>, SvgError> {
    let bytes = input.as_bytes();
    let n = bytes.len();
    let mut elements = Vec::new();
    let mut i = 0;
    while i < n {
        if bytes[i] != b'<' {
            i += 1;
            continue;
        }
        let next = bytes.get(i + 1).copied().ok_or(SvgError::Malformed)?;
        if input[i..].starts_with("<!--") {
            let end = find_sub(input, i + 4, "-->").ok_or(SvgError::Malformed)?;
            i = end + 3;
            continue;
        }
        if next == b'!' || next == b'?' || next == b'/' {
            let end = find_byte(bytes, i + 1, b'>').ok_or(SvgError::Malformed)?;
            i = end + 1;
            continue;
        }
        let close = find_tag_end(bytes, i + 1).ok_or(SvgError::Malformed)?;
        let content = &input[i + 1..close];
        elements.push(parse_tag(content)?);
        i = close + 1;
    }
    Ok(elements)
}

/// Find the first `>` at or after `from` that is not inside a quoted value.
fn find_tag_end(bytes: &[u8], from: usize) -> Option<usize> {
    let mut i = from;
    let mut quote: Option<u8> = None;
    while i < bytes.len() {
        let b = bytes[i];
        match quote {
            Some(q) if b == q => quote = None,
            None if b == b'"' || b == b'\'' => quote = Some(b),
            None if b == b'>' => return Some(i),
            Some(_) | None => {}
        }
        i += 1;
    }
    None
}

/// Find the byte `needle` at or after `from`.
fn find_byte(bytes: &[u8], from: usize, needle: u8) -> Option<usize> {
    (from..bytes.len()).find(|&i| bytes[i] == needle)
}

/// Find the substring `needle` at or after `from`.
fn find_sub(input: &str, from: usize, needle: &str) -> Option<usize> {
    if from > input.len() {
        return None;
    }
    input[from..].find(needle).map(|pos| from + pos)
}

/// Parse the text *between* `<` and `>` into an [`Element`].
fn parse_tag(content: &str) -> Result<Element<'_>, SvgError> {
    let trimmed = content.trim();
    let body = trimmed.strip_suffix('/').unwrap_or(trimmed).trim_end();
    let (name, rest) = match body.find(char::is_whitespace) {
        Some(idx) => (&body[..idx], &body[idx..]),
        None => (body, ""),
    };
    if name.is_empty() {
        return Err(SvgError::Malformed);
    }
    Ok(Element {
        name,
        attrs: parse_attrs(rest)?,
    })
}

/// Parse an attribute list (`key="value"` pairs). Valueless attributes are
/// ignored; a malformed quoting is a [`SvgError::Malformed`] rejection.
fn parse_attrs(s: &str) -> Result<Vec<(&str, &str)>, SvgError> {
    let bytes = s.as_bytes();
    let n = bytes.len();
    let mut attrs = Vec::new();
    let mut i = 0;
    while i < n {
        while i < n && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        if i >= n {
            break;
        }
        let key_start = i;
        while i < n && bytes[i] != b'=' && !bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        let key = &s[key_start..i];
        while i < n && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        if i < n && bytes[i] == b'=' {
            i += 1;
            while i < n && bytes[i].is_ascii_whitespace() {
                i += 1;
            }
            let quote = bytes.get(i).copied().ok_or(SvgError::Malformed)?;
            if quote != b'"' && quote != b'\'' {
                return Err(SvgError::Malformed);
            }
            i += 1;
            let value_start = i;
            while i < n && bytes[i] != quote {
                i += 1;
            }
            if i >= n {
                return Err(SvgError::Malformed);
            }
            let value = &s[value_start..i];
            i += 1;
            if !key.is_empty() {
                attrs.push((key, value));
            }
        }
    }
    Ok(attrs)
}
