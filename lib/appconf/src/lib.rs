//! The per-app configuration document engine (`tairix-appconf`).
//!
//! One document is one [`Document`]: an ordered list of lines, each either a
//! `key = value` setting or text the grammar does not read as one (a comment,
//! a blank line, something a hand-edit got wrong). The engine parses such a
//! document, answers typed reads against it, rewrites single settings, and
//! renders it back — with **no I/O**, so it is entirely host-testable and the
//! same code serves the app-data daemon, a client, and a test.
//!
//! # Why a line format, and why this one
//!
//! The requirement is that a human can open the file in a text editor and
//! that bad formatting is survivable. A nested document format fails the
//! second outright — one misplaced delimiter loses the whole file — so the
//! grammar is one setting per line and structure comes from dotted keys
//! (`effects.blur`), never from nesting syntax a hand-editor can get wrong.
//!
//! ```text
//! # The user's own comment survives a save.
//! scheme       = dark
//! font.size    = 14
//! effects.blur = 500
//! recent.0     = /Users/ada/Documents/notes.txt
//! greeting     = "  leading space, a # sign, and \n an escape "
//! ```
//!
//! # A rewrite never destroys what a human wrote
//!
//! This is the engine's hard requirement, not a nicety. A document is modelled
//! as an ordered list of *lines*, not as a map: [`Document::set`] rewrites the
//! one line it must and leaves every other byte — comments, blank lines, the
//! user's own alignment, key order, and lines the grammar refused — exactly as
//! it found them. An app that saves its settings therefore cannot silently
//! destroy what a user hand-edited in.
//!
//! # Tolerance, and where it stops
//!
//! A line that is not a valid setting is *retained verbatim* and reported by
//! [`Document::unparsed`]; it never aborts the read, so one bad line cannot
//! cost a user every other setting. That tolerance is deliberately confined to
//! line *content*: the document-level bounds ([`MAX_DOCUMENT_LEN`],
//! [`MAX_LINES`], [`MAX_SETTINGS`]) are fixed security bounds on untrusted
//! input and fail closed with a typed [`ConfError`], never by truncating.
//!
//! # A document may hold secrets
//!
//! The app-data store's **sealed** scope is a document of this format, so the
//! engine treats a line's bytes as secret unconditionally: every line it
//! discards — an overwritten setting, a collapsed duplicate, an
//! [`Document::unset`] removal — and every line of a document that goes out of
//! scope is wiped before it is freed. That lives in the engine rather than in
//! its callers so no discard path can forget it. The one copy the engine does
//! not own is [`Document::render`]'s return value, and its rustdoc says so.
//!
//! # Where the bounds live
//!
//! The three bounds a key, a value, and a whole document also cross the
//! app-data channel under — [`MAX_KEY_LEN`], [`MAX_VALUE_LEN`], and
//! [`MAX_DOCUMENT_LEN`] — are `abi-v1` wire field widths as well as this
//! grammar's bounds, so their one definition lives in `lib/abi`
//! (`appdata_ipc`) and this engine imports it. The remaining bounds
//! ([`MAX_KEY_DEPTH`], [`MAX_LINES`], [`MAX_SETTINGS`]) bound a document's
//! *shape* rather than its width, never cross the wire, and are this crate's
//! own.
//!
//! # Relationship to the other line-oriented stores
//!
//! `lib/sysconfig`, `lib/netconfig`, and the service registry read a
//! *space*-separated `key value` line with a closed key registry, and refuse a
//! whole document that deviates — the right semantics for a store the system
//! boots from. This engine is deliberately different on both counts: an open
//! key namespace an app owns, and per-line tolerance. It also cannot share
//! their comment tokenisation ([`tairix_util::conf::strip_comment`], which cuts
//! at the first `#` unconditionally), because here a `#` inside a quoted value
//! is a literal character; the tokenisation has to know about quoting, so it
//! lives with the grammar that has quotes.
//!
//! [`tairix_util::conf::strip_comment`]: https://docs.rs/tairix-util

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
