//! The pin store document: its closed key registry, bounded fail-closed
//! parser, and canonical render.
//!
//! One line is one pin — a [`PinKey`] registry key (`entry` or `bundle`),
//! whitespace, and the target's identifier or path — and the `#` comment
//! grammar is the one every line-oriented TAIRiX configuration store shares
//! ([`tairix_util::conf::strip_comment`]), so a comment is recognised here
//! exactly as it is in the boot-time and network stores.
//!
//! [`parse`] and [`render`] are inverses: parsing a rendered pin list yields
//! the same list, and rendering a parsed one yields byte-identical text
//! whatever order the pins were built in.

use alloc::string::String;
use core::fmt;

use tairix_proglib::{BundlePath, EntryError, EntryId, MAX_BUNDLE_PATH_LEN};
use tairix_util::conf::strip_comment;

use crate::pin::{PinList, PinTarget};

/// Maximum number of pins a list may hold.
///
/// A taskbar strip is a bounded surface, so this is a fixed security and
/// format bound, not a growable capacity: a store with more pins is
/// refused.
pub const MAX_PINS: usize = 128;

/// Maximum length, in bytes, of a pin store line.
///
/// A line holds one key and one value, and the longest value is a bundle
/// path, so this is that bound plus slack for the key and its separating
/// whitespace.
pub const MAX_LINE_LEN: usize = MAX_BUNDLE_PATH_LEN + 16;

/// Maximum length, in bytes, of a whole pin store document.
///
/// Sized to hold [`MAX_PINS`] entries at [`MAX_LINE_LEN`] width, so every
/// legal render provably fits back inside the parse bound.
pub const MAX_PINS_LEN: usize = MAX_PINS * (MAX_LINE_LEN + 1);

/// The closed set of fields a pin store line may set.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub enum PinKey {
    /// A pin referencing a program-library catalog entry.
    Entry,
    /// A direct pin of an application bundle that is not catalogued.
    Bundle,
}

impl PinKey {
    /// Every key, in the order [`render`] emits them.
    pub const ALL: [Self; 2] = [Self::Entry, Self::Bundle];

    /// The canonical field name, as it is written in the store.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Entry => "entry",
            Self::Bundle => "bundle",
        }
    }

    /// The key `field` names, or `None` when it is outside the registry.
    #[must_use]
    pub fn from_id(field: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|key| key.as_str() == field)
    }
}

impl fmt::Display for PinKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Why a pin store document was refused.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum ParseError {
    /// The document exceeded [`MAX_PINS_LEN`].
    DocumentTooLong,
    /// A line exceeded [`MAX_LINE_LEN`].
    LineTooLong,
    /// A line held a key but no value.
    MissingValue,
    /// The key is outside the [`PinKey`] registry.
    UnknownKey,
    /// The target named is already pinned in the same document.
    DuplicatePin,
    /// The document holds more than [`MAX_PINS`] pins.
    TooManyPins,
    /// A target value was refused by the model's own validator.
    Target(EntryError),
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DocumentTooLong => f.write_str("pin store document is too long"),
            Self::LineTooLong => f.write_str("line is too long"),
            Self::MissingValue => f.write_str("pin has no value"),
            Self::UnknownKey => f.write_str("unknown pin key"),
            Self::DuplicatePin => f.write_str("target is pinned twice"),
            Self::TooManyPins => f.write_str("store holds too many pins"),
            Self::Target(error) => write!(f, "{error}"),
        }
    }
}

/// A refused pin store document, and where in it the refusal was raised.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct PinsError {
    line: Option<usize>,
    kind: ParseError,
}

impl PinsError {
    /// The 1-based line the refusal was raised at, or `None` for a
    /// whole-document refusal that belongs to no single line.
    #[must_use]
    pub fn line(&self) -> Option<usize> {
        self.line
    }

    /// What was wrong.
    #[must_use]
    pub fn kind(&self) -> ParseError {
        self.kind
    }
}

impl fmt::Display for PinsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.line {
            Some(line) => write!(f, "line {line}: {}", self.kind),
            None => write!(f, "{}", self.kind),
        }
    }
}

impl From<EntryError> for ParseError {
    fn from(error: EntryError) -> Self {
        Self::Target(error)
    }
}

/// Parse a pin store document.
///
/// # Errors
///
/// [`PinsError`] — the document is refused whole, never half-applied. A
/// reader falls back to [`PinList::default`] on refusal rather than
/// guessing at a partial intent; a writer refuses the edit.
pub fn parse(text: &str) -> Result<PinList, PinsError> {
    if text.len() > MAX_PINS_LEN {
        return Err(PinsError {
            line: None,
            kind: ParseError::DocumentTooLong,
        });
    }

    let mut list = PinList::new();
    for (index, raw) in text.lines().enumerate() {
        let number = index + 1;
        let at = |kind: ParseError| PinsError {
            line: Some(number),
            kind,
        };
        if raw.len() > MAX_LINE_LEN {
            return Err(at(ParseError::LineTooLong));
        }
        let line = strip_comment(raw).trim();
        if line.is_empty() {
            continue;
        }

        let (key_str, value_str) = line
            .split_once(char::is_whitespace)
            .ok_or_else(|| at(ParseError::MissingValue))?;
        let value_str = value_str.trim();
        if value_str.is_empty() {
            return Err(at(ParseError::MissingValue));
        }

        let key = PinKey::from_id(key_str).ok_or_else(|| at(ParseError::UnknownKey))?;
        let target = match key {
            PinKey::Entry => PinTarget::Entry(EntryId::new(value_str).map_err(|e| at(e.into()))?),
            PinKey::Bundle => {
                PinTarget::Bundle(BundlePath::new(value_str).map_err(|e| at(e.into()))?)
            }
        };

        if list.len() >= MAX_PINS {
            return Err(at(ParseError::TooManyPins));
        }
        list.pin(target).map_err(|_| at(ParseError::DuplicatePin))?;
    }

    Ok(list)
}

/// Render `list` as the canonical document text.
///
/// Pins are emitted in display order, one `{key} {value}\n` line per pin,
/// so a pin store has exactly one spelling: two writers holding the same
/// pins in the same order produce byte-identical text.
#[must_use]
pub fn render(list: &PinList) -> String {
    use core::fmt::Write as _;

    let mut text = String::new();
    for target in list {
        match target {
            PinTarget::Entry(id) => {
                let _ = writeln!(text, "{} {}", PinKey::Entry.as_str(), id.as_str());
            }
            PinTarget::Bundle(path) => {
                let _ = writeln!(text, "{} {}", PinKey::Bundle.as_str(), path.as_str());
            }
        }
    }
    text
}

#[cfg(test)]
#[path = "store_tests.rs"]
mod tests;
