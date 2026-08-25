//! The per-app configuration document engine (`tairix-appconf`).
//!
//! One document is one [`Document`]: an ordered list of lines, each either a
//! `key = value` setting or text the grammar does not read as one (a comment, a
//! blank line, something a hand edit got wrong). The engine parses such a
//! document, answers typed reads against it, rewrites single settings, and
//! renders it back — with **no I/O**, so the same code serves the app-data
//! daemon, a client, and a host test.
//!
//! Three contracts a caller depends on:
//!
//! - [`Document::set`] rewrites the one line it must and leaves every other
//!   byte — comments, blank lines, the user's own alignment, key order, and
//!   lines the grammar refused — as it found them, so an app that saves its
//!   settings cannot destroy what a user hand-edited in.
//! - A line the grammar cannot read is retained verbatim and reported by
//!   [`Document::unparsed`] instead of aborting the read, so one bad line
//!   cannot cost a user every other setting. That tolerance stops at line
//!   content: the document-level bounds fail closed with a [`ConfError`].
//! - The sealed app-data scope is a document of this format, so the engine
//!   treats a line's bytes as secret unconditionally and wipes every line it
//!   discards. The one copy it does not own is [`Document::render`]'s return
//!   value.
//!
//! The grammar, the bounds table, and why the format is line-oriented are
//! `docs/src/lib/appconf.md`.

#![no_std]
#![forbid(unsafe_code)]
#![deny(missing_docs)]

extern crate alloc;

use tairix_abi::appdata_ipc::APPDATA_DOCUMENT_MAX;

mod document;
mod key;
mod value;

pub use document::{Document, Lookup, Setting, Unparsed};
pub use key::{validate_key, MAX_KEY_DEPTH, MAX_KEY_LEN};
pub use value::{as_bool, as_i64, as_permille, as_u32, bool_text, MAX_VALUE_LEN, PERMILLE_FULL};

/// Maximum length, in bytes, of a whole configuration document.
///
/// A fixed security bound on untrusted input, not a growable capacity: a
/// settings document holds a bounded registry of short lines, and this sizes
/// the work a hostile store can demand of the parser before a byte of it is
/// believed. A larger document is refused whole
/// ([`ConfError::DocumentTooLarge`]) rather than read in part.
///
/// The number is the app-data channel's own document field width, imported
/// for the reason [`MAX_KEY_LEN`] is: a whole document crosses the wire as one
/// reply body, so the bound is part of the `abi-v1` contract and the format
/// may not hold a second opinion about it.
pub const MAX_DOCUMENT_LEN: usize = APPDATA_DOCUMENT_MAX;

/// Maximum number of lines a document may hold.
///
/// Bounds the parser's allocation independently of
/// [`MAX_DOCUMENT_LEN`]: a document of nothing but newlines is small in bytes
/// and large in lines, and a per-line record is far wider than the byte that
/// produced it.
pub const MAX_LINES: usize = 4096;

/// Maximum number of `key = value` settings a document may carry.
///
/// Bounds the key space one app can create, so a lookup stays cheap and a
/// runaway writer cannot grow a store without limit.
pub const MAX_SETTINGS: usize = 512;

/// Why a document, key, or value was refused.
///
/// Every variant is a refusal: the engine never guesses at malformed input.
/// A refusal is *typed* so a caller can tell "no such setting" from "the
/// setting is there but does not mean what you asked for", and report the
/// second instead of silently substituting a default.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum ConfError {
    /// The document is longer than [`MAX_DOCUMENT_LEN`].
    DocumentTooLarge,
    /// The document holds more than [`MAX_LINES`] lines.
    TooManyLines,
    /// The document holds more than [`MAX_SETTINGS`] settings, or a write
    /// would.
    TooManySettings,
    /// The key is outside the [`validate_key`] grammar, too long, or too
    /// deeply dotted.
    KeyInvalid,
    /// The value is longer than [`MAX_VALUE_LEN`], or holds a character no
    /// value may carry.
    ValueInvalid,
    /// The setting is present but its text is not the requested type — a
    /// non-numeric number, an out-of-range permille, a boolean that is not one
    /// of the accepted spellings.
    ValueMalformed,
}

impl core::fmt::Display for ConfError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(match self {
            Self::DocumentTooLarge => "configuration document too large",
            Self::TooManyLines => "configuration document has too many lines",
            Self::TooManySettings => "configuration document has too many settings",
            Self::KeyInvalid => "invalid configuration key",
            Self::ValueInvalid => "invalid configuration value",
            Self::ValueMalformed => "configuration value is not of the expected type",
        })
    }
}
