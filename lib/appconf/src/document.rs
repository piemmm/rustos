//! The document model: an ordered list of lines, some of them settings.

use alloc::string::String;
use alloc::vec::Vec;
use core::fmt::Write as _;

use zeroize::Zeroize;

use crate::key::validate_key;
use crate::value;
use crate::{ConfError, MAX_DOCUMENT_LEN, MAX_LINES, MAX_SETTINGS};

/// One `key = value` setting, as a reader sees it.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct Setting<'a> {
    /// The setting's key.
    pub key: &'a str,
    /// The setting's decoded value.
    pub value: &'a str,
}

/// One line the grammar did not read as a setting, as
/// [`Document::unparsed`] reports it.
///
/// A caller logs these so a user learns *which* line of their file the engine
/// could not use — the line is still in the document and a save will put it
/// back untouched.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct Unparsed<'a> {
    /// The line's 1-based number in the document.
    pub line: usize,
    /// The line's text, exactly as read.
    pub text: &'a str,
}

/// What a line is.
enum Kind {
    /// A blank line or a whole-line comment: carries no setting and nothing
    /// went wrong.
    Inert,
    /// A line that looks like it meant to be a setting but is not one.
    Unparsed,
    /// A setting, with its decoded key and value and the raw comment suffix
    /// (empty when the line carries none) a rewrite puts back.
    Setting {
        key: String,
        value: String,
        comment: String,
    },
}

/// One line of the document: its text exactly as read, and what the grammar
/// made of it.
///
/// Keeping the original text is what makes preservation exact: an untouched
/// line renders as the bytes it arrived as, so a user's own alignment,
/// spacing, and comments survive a save that rewrote a different line.
struct Line {
    text: String,
    kind: Kind,
}

impl Line {
    /// Whether this line sets `key`.
    fn is_setting_of(&self, key: &str) -> bool {
        matches!(&self.kind, Kind::Setting { key: k, .. } if k == key)
    }

    /// The line's inline comment suffix, empty when it carries none.
    fn comment(&self) -> &str {
        match &self.kind {
            Kind::Setting { comment, .. } => comment.as_str(),
            _ => "",
        }
    }

    /// Overwrite every byte the line holds, leaving it empty.
    ///
    /// `Zeroize` uses volatile writes the optimiser may not elide, so the
    /// bytes are really gone rather than merely unreachable.
    fn wipe(&mut self) {
        self.text.zeroize();
        if let Kind::Setting {
            key,
            value,
            comment,
        } = &mut self.kind
        {
            key.zeroize();
            value.zeroize();
            comment.zeroize();
        }
    }
}

impl Drop for Line {
    /// Wipe the line's bytes before they are freed.
    ///
    /// The app-data store's sealed scope is a document of this format, so a
    /// line the engine discards — an overwritten setting, a collapsed
    /// duplicate, an [`Document::unset`] removal, or a whole document going
    /// out of scope — may hold a secret. Wiping here rather than at each call
    /// site is what makes it hold for *every* discard path, including ones
    /// added later: a caller cannot forget.
    fn drop(&mut self) {
        self.wipe();
    }
}

/// A parsed configuration document.
///
/// Reads are by key ([`get`](Self::get) and the typed accessors); writes are
/// by key too ([`set`](Self::set), [`unset`](Self::unset)) and touch only the
/// lines they must. [`render`](Self::render) returns the document's text.
///
/// A document may hold secrets — the app-data store's sealed scope is one of
/// these — so every line the document discards is wiped before it is freed,
/// and so is every line of a document that goes out of scope. The one thing
/// that escapes that is [`render`](Self::render)'s return value, which the
/// caller owns.
pub struct Document {
    lines: Vec<Line>,
    /// Whether the source text ended with a newline, so a render reproduces
    /// the file the user actually had rather than normalising its last byte.
    final_newline: bool,
}

impl Document {
    /// An empty document — what an app reads when it has never saved
    /// settings, and what a first [`set`](Self::set) grows.
    #[must_use]
    pub fn new() -> Self {
        Self {
            lines: Vec::new(),
            final_newline: true,
        }
    }

    /// Parse `text` into a document.
    ///
    /// Per-line tolerance is deliberate: a line the grammar cannot read is
    /// retained verbatim and reported by [`unparsed`](Self::unparsed) rather
    /// than costing the reader every other setting in the file. Duplicate
    /// keys are kept as written; a read answers with the **last**, which is
    /// what a hand-edit that appends a line means.
    ///
    /// # Errors
    ///
    /// [`ConfError::DocumentTooLarge`], [`ConfError::TooManyLines`], or
    /// [`ConfError::TooManySettings`] — the fixed bounds on untrusted input,
    /// which fail closed rather than reading a document in part.
    pub fn parse(text: &str) -> Result<Self, ConfError> {
        if text.len() > MAX_DOCUMENT_LEN {
            return Err(ConfError::DocumentTooLarge);
        }
        let final_newline = text.ends_with('\n');
        let body = text.strip_suffix('\n').unwrap_or(text);
        let mut lines = Vec::new();
        let mut settings = 0usize;
        if !text.is_empty() {
            for raw in body.split('\n') {
                if lines.len() == MAX_LINES {
                    return Err(ConfError::TooManyLines);
                }
                let kind = classify(raw);
                if matches!(kind, Kind::Setting { .. }) {
                    settings += 1;
                    if settings > MAX_SETTINGS {
                        return Err(ConfError::TooManySettings);
                    }
                }
                lines.push(Line {
                    text: String::from(raw),
                    kind,
                });
            }
        }
        Ok(Self {
            lines,
            final_newline,
        })
    }

    /// The document's text: the bytes a caller writes back.
    ///
    /// The returned string is the caller's, and it is the one copy of the
    /// document's bytes this engine does not own. A caller rendering a
    /// document that holds secrets wipes it once it has been sealed or sent.
    #[must_use]
    pub fn render(&self) -> String {
        let mut out = String::new();
        if self.lines.is_empty() {
            return out;
        }
        for (index, line) in self.lines.iter().enumerate() {
            if index > 0 {
                out.push('\n');
            }
            out.push_str(&line.text);
        }
        if self.final_newline {
            out.push('\n');
        }
        out
    }

    /// The value of `key`, or [`None`] if the document does not set it.
    ///
    /// Answers with the **last** setting of the key, so appending a line to a
    /// file overrides an earlier one exactly as a reader would expect.
    #[must_use]
    pub fn get(&self, key: &str) -> Option<&str> {
        self.lines.iter().rev().find_map(|line| match &line.kind {
            Kind::Setting { key: k, value, .. } if k == key => Some(value.as_str()),
            _ => None,
        })
    }

    /// Every setting in the document, in file order.
    ///
    /// A duplicated key appears once per line, exactly as written; a caller
    /// listing keys deduplicates by taking the last, as [`get`](Self::get)
    /// does.
    pub fn settings(&self) -> impl Iterator<Item = Setting<'_>> + '_ {
        self.lines.iter().filter_map(|line| match &line.kind {
            Kind::Setting { key, value, .. } => Some(Setting {
                key: key.as_str(),
                value: value.as_str(),
            }),
            _ => None,
        })
    }

    /// Every line the grammar did not read as a setting, in file order.
    ///
    /// A blank line and a whole-line comment are *not* reported: nothing went
    /// wrong with them.
    pub fn unparsed(&self) -> impl Iterator<Item = Unparsed<'_>> + '_ {
        self.lines
            .iter()
            .enumerate()
            .filter_map(|(index, line)| match line.kind {
                Kind::Unparsed => Some(Unparsed {
                    line: index + 1,
                    text: line.text.as_str(),
                }),
                _ => None,
            })
    }

    /// Set `key` to `value`.
    ///
    /// Rewrites the key's own line in place, keeping any inline comment the
    /// user wrote on it, and collapses earlier duplicates of the same key so
    /// the file says once what it means. A key the document does not carry is
    /// appended, terminated — the new last line is the engine's own, and a
    /// file it wrote ends with a newline. No other line is touched.
    ///
    /// # Errors
    ///
    /// [`ConfError::KeyInvalid`] or [`ConfError::ValueInvalid`] for a key or
    /// value outside the grammar, [`ConfError::TooManySettings`] if the
    /// document already holds [`MAX_SETTINGS`] and this would add one, and
    /// [`ConfError::TooManyLines`] if appending would exceed [`MAX_LINES`].
    pub fn set(&mut self, key: &str, value: &str) -> Result<(), ConfError> {
        validate_key(key)?;
        value::validate(value)?;
        let occurrences: Vec<usize> = self
            .lines
            .iter()
            .enumerate()
            .filter(|(_, line)| line.is_setting_of(key))
            .map(|(index, _)| index)
            .collect();
        let Some((&last, earlier)) = occurrences.split_last() else {
            if self.settings().count() >= MAX_SETTINGS {
                return Err(ConfError::TooManySettings);
            }
            if self.lines.len() >= MAX_LINES {
                return Err(ConfError::TooManyLines);
            }
            self.lines.push(render_setting(key, value, ""));
            self.final_newline = true;
            return Ok(());
        };
        let comment = String::from(self.lines[last].comment());
        self.lines[last] = render_setting(key, value, &comment);
        // The surviving line now says what the key means, and a reader took
        // the last one anyway, so the earlier duplicates are noise: drop them
        // back-to-front so the indices stay valid.
        for &index in earlier.iter().rev() {
            self.lines.remove(index);
        }
        Ok(())
    }

    /// Remove every setting of `key`, leaving every other line untouched.
    ///
    /// Removing a key the document does not carry changes nothing.
    pub fn unset(&mut self, key: &str) {
        self.lines.retain(|line| !line.is_setting_of(key));
    }

    /// The boolean value of `key`.
    ///
    /// Accepts `true`/`false` and `on`/`off` — two spellings because both read
    /// naturally to a hand-editor, and neither is ambiguous. A write always
    /// renders `true`/`false`.
    ///
    /// # Errors
    ///
    /// [`ConfError::ValueMalformed`] if the setting is present but is not one
    /// of those spellings.
    pub fn bool(&self, key: &str) -> Result<Option<bool>, ConfError> {
        self.get(key).map(value::as_bool).transpose()
    }

    /// The unsigned value of `key`.
    ///
    /// # Errors
    ///
    /// [`ConfError::ValueMalformed`] if the setting is present but is not a
    /// decimal `u32` (a sign, a suffix, or an overflow are all refusals, not
    /// a saturation).
    pub fn u32(&self, key: &str) -> Result<Option<u32>, ConfError> {
        self.get(key).map(value::as_u32).transpose()
    }

    /// The signed value of `key`.
    ///
    /// # Errors
    ///
    /// [`ConfError::ValueMalformed`] if the setting is present but is not a
    /// decimal `i64`.
    pub fn i64(&self, key: &str) -> Result<Option<i64>, ConfError> {
        self.get(key).map(value::as_i64).transpose()
    }

    /// The fraction `key` names, in permille (`0..=`[`PERMILLE_FULL`]).
    ///
    /// # Errors
    ///
    /// [`ConfError::ValueMalformed`] if the setting is present but is not a
    /// decimal integer, or names more than [`PERMILLE_FULL`].
    ///
    /// [`PERMILLE_FULL`]: crate::PERMILLE_FULL
    pub fn permille(&self, key: &str) -> Result<Option<u32>, ConfError> {
        self.get(key).map(value::as_permille).transpose()
    }

    /// Set `key` to a boolean, rendered `true`/`false`.
    ///
    /// # Errors
    ///
    /// As [`set`](Self::set).
    pub fn set_bool(&mut self, key: &str, value: bool) -> Result<(), ConfError> {
        self.set(key, value::bool_text(value))
    }

    /// Set `key` to an unsigned decimal.
    ///
    /// # Errors
    ///
    /// As [`set`](Self::set).
    pub fn set_u32(&mut self, key: &str, value: u32) -> Result<(), ConfError> {
        let mut text = String::new();
        let _ = write!(text, "{value}");
        self.set(key, &text)
    }

    /// Set `key` to a signed decimal.
    ///
    /// # Errors
    ///
    /// As [`set`](Self::set).
    pub fn set_i64(&mut self, key: &str, value: i64) -> Result<(), ConfError> {
        let mut text = String::new();
        let _ = write!(text, "{value}");
        self.set(key, &text)
    }

    /// Set `key` to a permille fraction.
    ///
    /// # Errors
    ///
    /// [`ConfError::ValueMalformed`] if `value` exceeds
    /// [`PERMILLE_FULL`](crate::PERMILLE_FULL), else as [`set`](Self::set).
    pub fn set_permille(&mut self, key: &str, value: u32) -> Result<(), ConfError> {
        if value > value::PERMILLE_FULL {
            return Err(ConfError::ValueMalformed);
        }
        self.set_u32(key, value)
    }
}

impl Default for Document {
    fn default() -> Self {
        Self::new()
    }
}

/// Build the line a `set` writes: the canonical `key = value` plus the
/// caller's own inline comment, if the line it replaces had one.
fn render_setting(key: &str, value: &str, comment: &str) -> Line {
    let mut text = String::from(key);
    text.push_str(" = ");
    value::render(value, &mut text);
    text.push_str(comment);
    Line {
        text,
        kind: Kind::Setting {
            key: String::from(key),
            value: String::from(value),
            comment: String::from(comment),
        },
    }
}

/// Decide what one line of the document is.
fn classify(raw: &str) -> Kind {
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed.starts_with('#') {
        return Kind::Inert;
    }
    let Some((key, rest)) = raw.split_once('=') else {
        return Kind::Unparsed;
    };
    let key = key.trim();
    if validate_key(key).is_err() {
        return Kind::Unparsed;
    }
    match value::decode(rest) {
        Ok((value, comment)) => Kind::Setting {
            key: String::from(key),
            value,
            comment: String::from(comment),
        },
        Err(_) => Kind::Unparsed,
    }
}

#[cfg(test)]
#[path = "document_tests.rs"]
mod tests;
