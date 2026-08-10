//! A minimal, fail-closed XML element scanner for the SVG subset.
//!
//! The decoder does not need a general XML tree: every WM/desktop asset is a
//! flat list of shape elements under one `<svg>` root, so the scanner yields
//! the document's **start tags** in order and ignores text, comments,
//! processing instructions, the doctype, and closing tags. Anything
//! structurally broken — an unterminated tag, quote, or comment — is a
//! [`SvgError::Malformed`] rejection, never a panic.
//!
//! Slicing is always at ASCII delimiters (`<`, `>`, `=`, quotes, whitespace),
//! and every UTF-8 continuation byte is `>= 0x80`, so byte offsets never split
//! a multi-byte character: the `&str` slices below are always on char
//! boundaries.

use alloc::borrow::Cow;
use alloc::string::String;
use alloc::vec::Vec;

use crate::error::SvgError;

/// The namespace a prefixed element must resolve to before it is treated as
/// SVG. An unprefixed element is SVG whatever the document declares, so an
/// asset written without an `xmlns` still renders.
const SVG_NAMESPACE: &str = "http://www.w3.org/2000/svg";

/// The deepest element nesting accepted.
///
/// A fixed security bound, not a capacity: real artwork nests a handful of
/// groups deep, and the limit is what stops a document of ten thousand open
/// tags from growing the parse stack without end.
pub const MAX_DEPTH: usize = 64;

/// The most elements accepted in one document.
///
/// A fixed security bound: it caps the memory a hostile asset can make the
/// decoder allocate before it has drawn anything.
pub const MAX_ELEMENTS: usize = 8192;

/// One element of the parsed document: its name, attributes, and children.
///
/// Text, comments, processing instructions, and the doctype are dropped — no
/// part of the supported subset reads character data, so keeping it would be
/// weight the decoder never looks at.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Node<'a> {
    /// The element's name: the local name for SVG elements, and the raw
    /// prefixed name for an element in some other namespace (which therefore
    /// matches nothing the decoder draws).
    pub name: &'a str,
    /// The attributes in document order, each a `(name, value)` pair. A value
    /// carrying an entity reference is decoded, and so owns its text.
    pub attrs: Vec<(&'a str, Cow<'a, str>)>,
    /// The element's children, in document order.
    pub children: Vec<Node<'a>>,
}

impl Node<'_> {
    /// The value of attribute `name`, or `None` if the element lacks it.
    #[must_use]
    pub fn attr(&self, name: &str) -> Option<&str> {
        self.attrs
            .iter()
            .find(|(key, _)| *key == name)
            .map(|(_, value)| value.as_ref())
    }

    /// The element's reference to another element, in either the SVG 2
    /// spelling or the SVG 1.1 `xlink` one.
    #[must_use]
    pub fn href(&self) -> Option<&str> {
        self.attr("href").or_else(|| self.attr("xlink:href"))
    }
}

/// Parse `input` into the document's root element.
///
/// # Errors
/// Returns [`SvgError::Malformed`] for an unterminated tag, quote, comment,
/// or element, or a close tag that does not match the element it ends;
/// [`SvgError::MissingRoot`] when the document holds no element; and
/// [`SvgError::TooComplex`] when it exceeds [`MAX_DEPTH`] or [`MAX_ELEMENTS`].
pub fn parse(input: &str) -> Result<Node<'_>, SvgError> {
    let bytes = input.as_bytes();
    let n = bytes.len();
    let mut root: Option<Node<'_>> = None;
    let mut stack: Vec<Node<'_>> = Vec::new();
    let mut namespaces: Vec<(&str, &str)> = Vec::new();
    let mut count = 0_usize;
    let mut i = 0;

    while i < n {
        if bytes[i] != b'<' {
            i += 1;
            continue;
        }
        let next = bytes.get(i + 1).copied().ok_or(SvgError::Malformed)?;
        if input[i..].starts_with("<!--") {
            i = find_sub(input, i + 4, "-->").ok_or(SvgError::Malformed)? + 3;
            continue;
        }
        if input[i..].starts_with("<![CDATA[") {
            i = find_sub(input, i + 9, "]]>").ok_or(SvgError::Malformed)? + 3;
            continue;
        }
        if next == b'!' || next == b'?' {
            i = find_tag_end(bytes, i + 1).ok_or(SvgError::Malformed)? + 1;
            continue;
        }
        let close = find_tag_end(bytes, i + 1).ok_or(SvgError::Malformed)?;
        let content = &input[i + 1..close];
        i = close + 1;

        if let Some(name) = content.strip_prefix('/') {
            let ended = stack.pop().ok_or(SvgError::Malformed)?;
            if ended.name != local_name(name.trim(), &namespaces) {
                return Err(SvgError::Malformed);
            }
            drop_namespaces(&mut namespaces, &ended);
            match stack.last_mut() {
                Some(parent) => parent.children.push(ended),
                None if root.is_none() => root = Some(ended),
                // A second root-level element is not a well-formed document.
                None => return Err(SvgError::Malformed),
            }
            continue;
        }

        count += 1;
        if count > MAX_ELEMENTS {
            return Err(SvgError::TooComplex);
        }
        let self_closing = content.trim_end().ends_with('/');
        let node = parse_start_tag(content, &mut namespaces)?;
        if self_closing {
            drop_namespaces(&mut namespaces, &node);
            match stack.last_mut() {
                Some(parent) => parent.children.push(node),
                None if root.is_none() => root = Some(node),
                None => return Err(SvgError::Malformed),
            }
        } else {
            if stack.len() >= MAX_DEPTH {
                return Err(SvgError::TooComplex);
            }
            stack.push(node);
        }
    }

    if stack.is_empty() {
        root.ok_or(SvgError::MissingRoot)
    } else {
        Err(SvgError::Malformed)
    }
}

/// Forget the namespace prefixes `node` declared, now that its scope has
/// ended.
fn drop_namespaces(namespaces: &mut Vec<(&str, &str)>, node: &Node<'_>) {
    let declared = node
        .attrs
        .iter()
        .filter(|(key, _)| key.starts_with("xmlns:"))
        .count();
    namespaces.truncate(namespaces.len().saturating_sub(declared));
}

/// An element or attribute name with its namespace prefix resolved.
///
/// An unprefixed name, or one whose prefix is bound to the SVG namespace, is
/// SVG and keeps only its local part; anything else keeps its prefix, so it
/// matches none of the elements the decoder draws and is skipped.
fn local_name<'a>(name: &'a str, namespaces: &[(&str, &str)]) -> &'a str {
    let Some((prefix, local)) = name.split_once(':') else {
        return name;
    };
    let bound = namespaces
        .iter()
        .rev()
        .find(|(declared, _)| *declared == prefix)
        .map(|(_, uri)| *uri);
    if bound == Some(SVG_NAMESPACE) {
        local
    } else {
        name
    }
}

/// Parse the text *between* `<` and `>` of a start tag into a childless
/// [`Node`], recording any namespace prefixes it declares.
fn parse_start_tag<'a>(
    content: &'a str,
    namespaces: &mut Vec<(&'a str, &'a str)>,
) -> Result<Node<'a>, SvgError> {
    let trimmed = content.trim();
    let body = trimmed.strip_suffix('/').unwrap_or(trimmed).trim_end();
    let (raw_name, rest) = match body.find(char::is_whitespace) {
        Some(idx) => (&body[..idx], &body[idx..]),
        None => (body, ""),
    };
    if raw_name.is_empty() {
        return Err(SvgError::Malformed);
    }
    let raw = parse_attrs(rest)?;
    // In scope for this element's own name as well as its subtree's.
    for (key, value) in &raw {
        if let Some(prefix) = key.strip_prefix("xmlns:") {
            namespaces.push((prefix, value));
        }
    }
    Ok(Node {
        name: local_name(raw_name, namespaces),
        attrs: raw
            .into_iter()
            .map(|(key, value)| (key, decode_entities(value)))
            .collect(),
        children: Vec::new(),
    })
}

/// Replace XML's five predefined entities and numeric character references
/// with the characters they stand for.
///
/// A value with no `&` is returned borrowed, which is every value in
/// practice; only the rare escaped one allocates. An entity this decoder does
/// not define is left as written rather than rejecting the document, since no
/// attribute it draws from can carry one.
fn decode_entities(value: &str) -> Cow<'_, str> {
    if !value.contains('&') {
        return Cow::Borrowed(value);
    }
    let mut out = String::with_capacity(value.len());
    let mut rest = value;
    while let Some(start) = rest.find('&') {
        out.push_str(&rest[..start]);
        let tail = &rest[start..];
        let Some(end) = tail.find(';') else {
            out.push_str(tail);
            return Cow::Owned(out);
        };
        let entity = &tail[1..end];
        match decode_entity(entity) {
            Some(c) => out.push(c),
            None => out.push_str(&tail[..=end]),
        }
        rest = &tail[end + 1..];
    }
    out.push_str(rest);
    Cow::Owned(out)
}

/// The character one entity body (the text between `&` and `;`) stands for.
fn decode_entity(entity: &str) -> Option<char> {
    match entity {
        "amp" => return Some('&'),
        "lt" => return Some('<'),
        "gt" => return Some('>'),
        "quot" => return Some('"'),
        "apos" => return Some('\''),
        _ => {}
    }
    let digits = entity.strip_prefix('#')?;
    let code = match digits.strip_prefix(['x', 'X']) {
        Some(hex) => u32::from_str_radix(hex, 16).ok()?,
        None => digits.parse::<u32>().ok()?,
    };
    char::from_u32(code)
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

/// Find the substring `needle` at or after `from`.
fn find_sub(input: &str, from: usize, needle: &str) -> Option<usize> {
    if from > input.len() {
        return None;
    }
    input[from..].find(needle).map(|pos| from + pos)
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
