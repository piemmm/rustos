//! Locale and document-name validation, the injected read seam, and the
//! deterministic locale-fallback document lookup.
//!
//! The engine never touches a filesystem: the caller injects a
//! [`HelpSource`] already scoped (by capability) to exactly one bundle's
//! `Help/` tree. Everything the seam is asked for is a spelling this module
//! validated first — a well-formed locale directory name ([`Locale`]) and a
//! well-formed document file name built from a [`DocumentName`] — so a
//! hostile command name or locale tag can never traverse outside that tree.

use alloc::string::String;
use alloc::vec::Vec;
use core::fmt;

use crate::doc::{HelpDoc, HelpError, MAX_DOC_LEN};

/// The canonical locale directory that always exists: the en-US source
/// every fallback chain ends at.
pub const DEFAULT_LOCALE: &str = "en-US";

/// The standing locale set every OS command app's help must ship
/// (`plans/APPS.md` §8.1): the mandatory canonical `en-US/` tree plus
/// the required translations. Adding a language is extending this data,
/// not new code. The set gates *authoring* completeness (the help lint and
/// each app's own switch pins); runtime selection never consults it — a
/// bundle's `Help/` tree is scanned for whatever locales it actually ships.
pub const REQUIRED_LOCALES: &[&str] = &[
    "en-US", "fr-FR", "de-DE", "es-ES", "uk-UA", "it-IT", "pt-PT", "cy-GB", "zh-CN", "ja-JP",
    "ko-KR", "ar-SA", "he-IL",
];

/// Byte length of [`DEFAULT_LOCALE`]'s language subtag (`en`).
const DEFAULT_LANGUAGE_LEN: usize = 2;

/// Maximum byte length of a locale tag this engine accepts
/// (`language` up to 8 letters, `-`, region up to 3 characters).
pub const MAX_LOCALE_LEN: usize = 12;

/// Maximum byte length of a document (command/topic) name.
pub const MAX_DOCUMENT_NAME_LEN: usize = 64;

/// Maximum number of locale directories considered when scanning a `Help/`
/// tree for a same-language fallback. A tree that lists more is refused: the
/// standing translation set is a handful of locales, so a larger listing is
/// a hostile or corrupt tree, not a capacity to grow into.
pub const MAX_LOCALE_DIRS: usize = 256;

/// The suffix a document file name carries on disk.
const DOCUMENT_SUFFIX: &str = ".md";

/// A validated locale directory name: a BCP-47 `language[-REGION]` tag
/// (`fr`, `fr-FR`, `es-419`), of which the canonical [`DEFAULT_LOCALE`]
/// (`en-US`) is one.
///
/// The accepted grammar is deliberately the small subset the `Help/` tree
/// uses: a primary language subtag of 2–8 ASCII lowercase letters,
/// optionally followed by `-` and a region subtag of exactly 2 ASCII
/// uppercase letters or 3 ASCII digits. Anything else — including a
/// differently-cased spelling — is rejected rather than "fixed up", so the
/// canonical directory spelling is the only spelling that works.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Locale {
    tag: String,
    /// Byte length of the language subtag (`tag[..language_len]`).
    language_len: usize,
}

/// Why a locale tag was rejected by [`Locale::parse`].
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum TagError {
    /// The tag is empty.
    Empty,
    /// The tag exceeds [`MAX_LOCALE_LEN`] bytes.
    TooLong,
    /// The tag does not match `language[-REGION]` (2–8 lowercase letters,
    /// then optionally `-` plus 2 uppercase letters or 3 digits).
    Malformed,
}

impl fmt::Display for TagError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TagError::Empty => f.write_str("locale tag is empty"),
            TagError::TooLong => f.write_str("locale tag is too long"),
            TagError::Malformed => f.write_str("locale tag is not `language[-REGION]`"),
        }
    }
}

impl Locale {
    /// Parse and validate a locale tag.
    pub fn parse(tag: &str) -> Result<Self, TagError> {
        if tag.is_empty() {
            return Err(TagError::Empty);
        }
        if tag.len() > MAX_LOCALE_LEN {
            return Err(TagError::TooLong);
        }
        let (language, region) = match tag.split_once('-') {
            Some((language, region)) => (language, Some(region)),
            None => (tag, None),
        };
        let language_ok =
            (2..=8).contains(&language.len()) && language.bytes().all(|b| b.is_ascii_lowercase());
        let region_ok = match region {
            None => true,
            Some(region) => {
                (region.len() == 2 && region.bytes().all(|b| b.is_ascii_uppercase()))
                    || (region.len() == 3 && region.bytes().all(|b| b.is_ascii_digit()))
            }
        };
        if !language_ok || !region_ok {
            return Err(TagError::Malformed);
        }
        Ok(Locale {
            tag: String::from(tag),
            language_len: language.len(),
        })
    }

    /// The canonical tag spelling (also the `Help/` directory name).
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.tag
    }

    /// The primary language subtag (`fr` of `fr-FR`).
    #[must_use]
    pub fn language(&self) -> &str {
        self.tag.get(..self.language_len).unwrap_or(&self.tag)
    }

    /// Whether this is the canonical [`DEFAULT_LOCALE`] (`en-US`).
    #[must_use]
    pub fn is_default(&self) -> bool {
        self.tag == DEFAULT_LOCALE
    }
}

impl Default for Locale {
    /// The canonical [`DEFAULT_LOCALE`] (`en-US`) — the tree every fallback
    /// chain ends at. Constructing it directly (rather than through
    /// [`Locale::parse`]) lets a caller with no usable locale preference
    /// reach the canonical documents without handling an impossible parse
    /// error.
    fn default() -> Self {
        Locale {
            tag: String::from(DEFAULT_LOCALE),
            language_len: DEFAULT_LANGUAGE_LEN,
        }
    }
}

/// A validated command/topic name: 1–[`MAX_DOCUMENT_NAME_LEN`] bytes of
/// ASCII letters, digits, `_`, or `-`, starting with a letter or digit.
///
/// The grammar exists to make traversal unrepresentable: a name can never
/// spell a path separator, a dot, or an empty component, so the file name
/// handed to the seam always stays inside the scoped tree.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DocumentName {
    name: String,
}

/// Why a document name was rejected by [`DocumentName::parse`].
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum NameError {
    /// The name is empty.
    Empty,
    /// The name exceeds [`MAX_DOCUMENT_NAME_LEN`] bytes.
    TooLong,
    /// The name contains a character outside `[A-Za-z0-9_-]`, or starts
    /// with `_` or `-`.
    Malformed,
}

impl fmt::Display for NameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            NameError::Empty => f.write_str("document name is empty"),
            NameError::TooLong => f.write_str("document name is too long"),
            NameError::Malformed => f.write_str("document name is not `[A-Za-z0-9][A-Za-z0-9_-]*`"),
        }
    }
}

impl DocumentName {
    /// Parse and validate a command/topic name.
    pub fn parse(name: &str) -> Result<Self, NameError> {
        if name.is_empty() {
            return Err(NameError::Empty);
        }
        if name.len() > MAX_DOCUMENT_NAME_LEN {
            return Err(NameError::TooLong);
        }
        let mut bytes = name.bytes();
        let first_ok = bytes.next().is_some_and(|b| b.is_ascii_alphanumeric());
        let rest_ok = bytes.all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-');
        if !first_ok || !rest_ok {
            return Err(NameError::Malformed);
        }
        Ok(DocumentName {
            name: String::from(name),
        })
    }

    /// The bare command/topic name.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.name
    }

    /// The on-disk file name inside a locale directory (`<name>.md`).
    #[must_use]
    pub fn file_name(&self) -> String {
        let mut file = String::with_capacity(self.name.len() + DOCUMENT_SUFFIX.len());
        file.push_str(&self.name);
        file.push_str(DOCUMENT_SUFFIX);
        file
    }
}

/// The backing store failed (an I/O error, a revoked capability, a vanished
/// bundle). The engine treats every source failure the same way — fail
/// closed and report — so the seam carries no further detail; the caller
/// that owns the store logs the specifics.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct SourceError;

impl fmt::Display for SourceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("help source failed")
    }
}

/// The injected, capability-scoped read seam over one bundle's `Help/` tree.
///
/// Implementations expose exactly two read-only operations and no ambient
/// authority. Every `locale_dir`/`file_name` the engine passes in is a
/// spelling it validated first, so an implementation may join them onto its
/// scoped root without re-parsing.
pub trait HelpSource {
    /// The directory names at the top of the `Help/` tree, in any order.
    fn locale_dirs(&self) -> Result<Vec<String>, SourceError>;

    /// The bytes of `Help/<locale_dir>/<file_name>`, or `None` if that
    /// document does not exist in that locale.
    fn read(&self, locale_dir: &str, file_name: &str) -> Result<Option<Vec<u8>>, SourceError>;
}

/// How the served locale relates to the requested one.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum Fallback {
    /// The exact requested locale directory served the document.
    Exact,
    /// A different region of the requested language served it.
    SameLanguage,
    /// The canonical `en-US/` document served it.
    Default,
}

/// Which locale directory served a document, and how it relates to the
/// request — the input for `man`'s locale-fallback `stdinfo` record.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Selection {
    /// The locale directory the document was read from.
    pub locale_dir: String,
    /// How that directory relates to the requested locale.
    pub fallback: Fallback,
}

/// A located, parsed help document.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Loaded {
    /// Which locale served the document.
    pub selection: Selection,
    /// The parsed document.
    pub doc: HelpDoc,
}

/// Why [`load`] failed.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LoadError {
    /// The backing store failed.
    Source(SourceError),
    /// The `Help/` tree lists more than [`MAX_LOCALE_DIRS`] directories.
    TooManyLocales,
    /// No locale — not even `en-US/` — holds the document. An ordinary,
    /// non-fatal outcome ("no help for this command"), never fabricated
    /// around.
    NotFound,
    /// The selected document exists but does not parse.
    Document(HelpError),
}

impl fmt::Display for LoadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LoadError::Source(err) => err.fmt(f),
            LoadError::TooManyLocales => f.write_str("help tree lists too many locales"),
            LoadError::NotFound => f.write_str("no help document for this name"),
            LoadError::Document(err) => err.fmt(f),
        }
    }
}

impl From<SourceError> for LoadError {
    fn from(err: SourceError) -> Self {
        LoadError::Source(err)
    }
}

impl From<HelpError> for LoadError {
    fn from(err: HelpError) -> Self {
        LoadError::Document(err)
    }
}

/// Locate, read, and parse `name`'s document for `requested`, applying the
/// deterministic fallback chain.
///
/// 1. `Help/<requested>/<name>.md` — the exact locale.
/// 2. The lexicographically first directory of the same language (any
///    region) that holds the document, so the choice is stable across runs.
/// 3. `Help/en-US/<name>.md` — the canonical document.
///
/// Requesting [`DEFAULT_LOCALE`] itself is step 3 directly and reports
/// [`Fallback::Exact`]. A document served by a step later than the request
/// asked for is reported as such in [`Selection::fallback`]; whether to
/// surface that (e.g. on `stdinfo`) is the caller's decision.
pub fn load(
    source: &dyn HelpSource,
    requested: &Locale,
    name: &DocumentName,
) -> Result<Loaded, LoadError> {
    let file = name.file_name();

    if !requested.is_default() {
        if let Some(bytes) = read_bounded(source, requested.as_str(), &file)? {
            return finish(requested.as_str(), Fallback::Exact, &bytes);
        }
        if let Some((dir, bytes)) = read_same_language(source, requested, &file)? {
            return finish(&dir, Fallback::SameLanguage, &bytes);
        }
    }

    match read_bounded(source, DEFAULT_LOCALE, &file)? {
        Some(bytes) => {
            let fallback = if requested.is_default() {
                Fallback::Exact
            } else {
                Fallback::Default
            };
            finish(DEFAULT_LOCALE, fallback, &bytes)
        }
        None => Err(LoadError::NotFound),
    }
}

/// Scan the tree for the same-language step of the chain: the
/// lexicographically first valid same-language directory (excluding the
/// exact tag already tried) that holds the document.
///
/// A listed name that is not a valid [`Locale`] spelling is skipped rather
/// than fatal: selection is allowlist-shaped, so an alien directory can
/// never be chosen — but it also must not deny help that a valid sibling
/// still provides.
fn read_same_language(
    source: &dyn HelpSource,
    requested: &Locale,
    file: &str,
) -> Result<Option<(String, Vec<u8>)>, LoadError> {
    let dirs = source.locale_dirs()?;
    if dirs.len() > MAX_LOCALE_DIRS {
        return Err(LoadError::TooManyLocales);
    }
    let mut candidates: Vec<String> = dirs
        .into_iter()
        .filter(|dir| {
            dir.as_str() != requested.as_str()
                && Locale::parse(dir)
                    .is_ok_and(|l| !l.is_default() && l.language() == requested.language())
        })
        .collect();
    candidates.sort_unstable();
    for dir in candidates {
        if let Some(bytes) = read_bounded(source, &dir, file)? {
            return Ok(Some((dir, bytes)));
        }
    }
    Ok(None)
}

/// Read one document through the seam, refusing an over-sized blob before
/// it reaches the parser.
fn read_bounded(
    source: &dyn HelpSource,
    locale_dir: &str,
    file: &str,
) -> Result<Option<Vec<u8>>, LoadError> {
    match source.read(locale_dir, file)? {
        Some(bytes) if bytes.len() > MAX_DOC_LEN => Err(LoadError::Document(HelpError::TooLarge)),
        other => Ok(other),
    }
}

/// Parse the served bytes and assemble the result.
fn finish(locale_dir: &str, fallback: Fallback, bytes: &[u8]) -> Result<Loaded, LoadError> {
    let doc = HelpDoc::parse(bytes)?;
    Ok(Loaded {
        selection: Selection {
            locale_dir: String::from(locale_dir),
            fallback,
        },
        doc,
    })
}
