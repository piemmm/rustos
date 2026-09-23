//! The identification-string exchange (RFC 4253 §4.2).
//!
//! Each side opens with one line, `SSH-2.0-softwareversion [comments]`,
//! terminated by CR LF. A server may precede its line with others, which a
//! client reads past; a client may not. The line without its terminator is the
//! `V_C` or `V_S` the exchange hash covers, so a peer's is kept byte for byte.
//!
//! A peer's software field is checked for printable, space-free US-ASCII and
//! nothing stricter: RFC 4253 also forbids a minus sign there, and deployed
//! peers send one (`SSH-2.0-Cisco-1.25`). The field is only ever hashed and
//! displayed, so refusing it would cost interoperability and buy nothing.

use tairix_inline::ArrayVec;

use crate::Role;

/// The longest identification line RFC 4253 §4.2 permits, CR LF included.
pub const MAX_IDENT_LINE: usize = 255;

/// The longest line a server may send before its identification, terminator
/// included. RFC 4253 sets none; this is OpenSSH's.
pub const MAX_BANNER_LINE: usize = 8192;

/// The most lines a client reads before a server's identification. RFC 4253
/// sets none; this is OpenSSH's.
pub const MAX_BANNER_LINES: usize = 1024;

/// An identification's text without its CR LF, at its longest.
const MAX_IDENT_TEXT: usize = MAX_IDENT_LINE - 2;

const PREFIX: &[u8] = b"SSH-";

/// The only protocol version this engine speaks.
const VERSION: &[u8] = b"2.0";

/// The version a server compatible with both protocols announces, which
/// RFC 4253 §5.1 has a version 2 client treat as `2.0`.
const COMPATIBLE_VERSION: &[u8] = b"1.99";

/// Why an identification exchange failed.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum IdentError {
    /// Our own software version was empty or held a byte RFC 4253 forbids
    /// there: a space, a minus sign, or anything not printable US-ASCII.
    InvalidSoftware,
    /// Our own comment was empty or held a byte that is not printable
    /// US-ASCII.
    InvalidComment,
    /// A line ran past its bound before its terminator.
    TooLong,
    /// A line held a NUL or a CR not followed by LF, or an identification
    /// was not of the form `SSH-version-software`.
    Malformed,
    /// The peer speaks a protocol version other than 2.0.
    UnsupportedVersion,
    /// A client sent a line that was not its identification.
    UnexpectedLine,
    /// A server sent more lines ahead of its identification than
    /// [`MAX_BANNER_LINES`].
    TooManyLines,
}

/// An identification string: `SSH-protoversion-softwareversion`, then an
/// optional space and comment, without the CR LF.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Ident {
    text: ArrayVec<u8, MAX_IDENT_TEXT>,
    version_end: usize,
    software_end: usize,
}

impl Ident {
    /// This engine's own identification, `SSH-2.0-{software}` and, if given,
    /// a space and `comment`.
    ///
    /// # Errors
    ///
    /// [`IdentError::InvalidSoftware`], [`IdentError::InvalidComment`], or
    /// [`IdentError::TooLong`] when the line would pass [`MAX_IDENT_LINE`].
    pub fn new(software: &str, comment: Option<&str>) -> Result<Self, IdentError> {
        let software = software.as_bytes();
        if software.is_empty() || !software.iter().all(|&b| is_visible(b) && b != b'-') {
            return Err(IdentError::InvalidSoftware);
        }
        let (separator, comment): (&[u8], &[u8]) = match comment {
            Some(comment)
                if comment.is_empty() || !comment.bytes().all(|b| b == b' ' || is_visible(b)) =>
            {
                return Err(IdentError::InvalidComment);
            }
            Some(comment) => (b" ", comment.as_bytes()),
            None => (b"", b""),
        };
        let mut text = ArrayVec::new();
        for part in [PREFIX, VERSION, b"-", software, separator, comment] {
            text.try_extend_from_slice(part)
                .map_err(|_| IdentError::TooLong)?;
        }
        let version_end = PREFIX.len() + VERSION.len();
        Ok(Self {
            text,
            version_end,
            software_end: version_end + 1 + software.len(),
        })
    }

    /// A peer's identification line, its terminator already removed.
    ///
    /// # Errors
    ///
    /// [`IdentError::TooLong`], [`IdentError::Malformed`], or
    /// [`IdentError::UnsupportedVersion`].
    pub fn parse(line: &[u8]) -> Result<Self, IdentError> {
        if line.len() > MAX_IDENT_TEXT {
            return Err(IdentError::TooLong);
        }
        let Some(after_prefix) = line.strip_prefix(PREFIX) else {
            return Err(IdentError::Malformed);
        };
        if line.iter().any(|&b| matches!(b, 0 | b'\r' | b'\n')) {
            return Err(IdentError::Malformed);
        }
        let Some(dash) = after_prefix.iter().position(|&b| b == b'-') else {
            return Err(IdentError::Malformed);
        };
        let version = &after_prefix[..dash];
        if version != VERSION && version != COMPATIBLE_VERSION {
            return Err(IdentError::UnsupportedVersion);
        }
        let version_end = PREFIX.len() + dash;
        let tail = &line[version_end + 1..];
        let software_len = tail.iter().position(|&b| b == b' ').unwrap_or(tail.len());
        if software_len == 0 || !tail[..software_len].iter().copied().all(is_visible) {
            return Err(IdentError::Malformed);
        }
        let mut text = ArrayVec::new();
        text.try_extend_from_slice(line)
            .map_err(|_| IdentError::TooLong)?;
        Ok(Self {
            text,
            version_end,
            software_end: version_end + 1 + software_len,
        })
    }

    /// The identification as it is hashed: the line without its CR LF.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        self.text.as_slice()
    }

    /// The protocol version field, `2.0` or `1.99`.
    #[must_use]
    pub fn protocol(&self) -> &[u8] {
        &self.text[PREFIX.len()..self.version_end]
    }

    /// The software version field.
    #[must_use]
    pub fn software(&self) -> &[u8] {
        &self.text[self.version_end + 1..self.software_end]
    }

    /// The comment after the software version, if there is one.
    #[must_use]
    pub fn comment(&self) -> Option<&[u8]> {
        self.text.get(self.software_end + 1..)
    }
}

/// What the front of a peer's stream holds while identifications are being
/// exchanged.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Line<'a> {
    /// No complete line yet. The first `scanned` bytes hold no terminator
    /// and nothing refused, so the next call may resume there.
    Incomplete {
        /// Bytes already examined.
        scanned: usize,
    },
    /// A line a server sent ahead of its identification, terminator
    /// excluded, and the bytes it occupied.
    Banner {
        /// The line's text: untrusted, to be sanitised before display.
        text: &'a [u8],
        /// Stream bytes the line used, terminator included.
        consumed: usize,
    },
    /// The peer's identification line, terminator excluded, and the bytes
    /// it occupied. [`Ident::parse`] checks it.
    Ident {
        /// The line's text.
        text: &'a [u8],
        /// Stream bytes the line used, terminator included.
        consumed: usize,
    },
}

/// Split the next line off the front of `stream`, the bytes a peer playing
/// `from` has sent so far, and say whether it is the identification.
///
/// A line ends at LF, optionally preceded by CR, as OpenSSH accepts. A line
/// is refused as soon as the bytes present prove it wrong — a client's first
/// byte that cannot begin `SSH-`, a NUL, a CR followed by anything but LF, or
/// more bytes than its bound with no terminator — rather than once it ends.
///
/// `resume` is the `scanned` an earlier [`Line::Incomplete`] reported for the
/// same line, so a line arriving a byte at a time is examined once rather
/// than once per byte; zero starts afresh.
///
/// # Errors
///
/// [`IdentError::TooLong`], [`IdentError::Malformed`], or
/// [`IdentError::UnexpectedLine`].
pub fn next_line(stream: &[u8], from: Role, resume: usize) -> Result<Line<'_>, IdentError> {
    let probe = &stream[..stream.len().min(PREFIX.len())];
    let limit = if PREFIX.starts_with(probe) {
        MAX_IDENT_LINE
    } else if from == Role::Server {
        MAX_BANNER_LINE
    } else {
        return Err(IdentError::UnexpectedLine);
    };
    let window = &stream[..stream.len().min(limit)];
    let mut at = resume.min(window.len());
    while at < window.len() {
        match window[at] {
            b'\n' => {
                let text = &stream[..at];
                let text = text.strip_suffix(b"\r").unwrap_or(text);
                return classify(text, at + 1, from);
            }
            b'\r' => match window.get(at + 1) {
                Some(b'\n') => {}
                // Examined again once the byte after it arrives.
                None => break,
                Some(_) => return Err(IdentError::Malformed),
            },
            0 => return Err(IdentError::Malformed),
            _ => {}
        }
        at += 1;
    }
    if stream.len() >= limit {
        Err(IdentError::TooLong)
    } else {
        Ok(Line::Incomplete { scanned: at })
    }
}

fn classify(text: &[u8], consumed: usize, from: Role) -> Result<Line<'_>, IdentError> {
    if text.starts_with(PREFIX) {
        Ok(Line::Ident { text, consumed })
    } else if from == Role::Server {
        Ok(Line::Banner { text, consumed })
    } else {
        Err(IdentError::UnexpectedLine)
    }
}

/// Printable US-ASCII other than the space.
const fn is_visible(byte: u8) -> bool {
    byte > b' ' && byte <= b'~'
}

#[cfg(test)]
#[path = "ident_tests.rs"]
mod tests;
