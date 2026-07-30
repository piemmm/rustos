//! The catalog document: its closed key registry, bounded fail-closed
//! parser, and canonical render.
//!
//! One line is one setting — a `<id>.<field>` key, whitespace, and the
//! field's value — and the `#` comment grammar is the one every
//! line-oriented TAIRiX configuration store shares
//! ([`tairix_util::conf::strip_comment`]), so a comment is recognised here
//! exactly as it is in the boot-time and network stores.
//!
//! [`parse`] and [`render`] are inverses: parsing a rendered catalog yields
//! the same catalog, and rendering a parsed one yields byte-identical text
//! whatever order the records were built in. That is what lets an editor
//! read a store, change one record, and write it back without disturbing
//! anything else it holds.

use alloc::collections::BTreeMap;
use alloc::string::String;
use core::fmt;

use tairix_abi::LibraryCategory;
use tairix_util::conf::strip_comment;

use crate::catalog::{Catalog, EntryPatch, Record, MAX_ENTRIES};
use crate::entry::{
    DisplayName, EntryError, EntryId, IconAsset, LibraryEntry, MAX_BUNDLE_PATH_LEN,
    MAX_DISPLAY_NAME_LEN, MAX_ENTRY_ID_LEN, MAX_ICON_ASSET_LEN,
};

/// Maximum length, in bytes, of a catalog line.
///
/// A line holds one key and one value, and the longest value any field
/// admits is a bundle path, so this is that bound plus room for the key and
/// its separating whitespace. It is a validation bound on untrusted input,
/// not a capacity: a longer line is a malformed store.
pub const MAX_LINE_LEN: usize = MAX_BUNDLE_PATH_LEN + MAX_ENTRY_ID_LEN + 32;

/// Maximum length, in bytes, of a whole catalog document.
///
/// Sized to hold [`MAX_ENTRIES`] records at the width one full entry
/// occupies, so the record bound is reachable while a hostile store cannot
/// force the reader to hold an unbounded document in memory. Like
/// [`MAX_LINE_LEN`] this is a security bound, so it is fixed rather than
/// grown.
pub const MAX_CATALOG_LEN: usize = MAX_ENTRIES
    * (4 * MAX_ENTRY_ID_LEN
        + MAX_DISPLAY_NAME_LEN
        + MAX_BUNDLE_PATH_LEN
        + MAX_ICON_ASSET_LEN
        + LibraryCategory::MAX_ID_LEN
        + 64);

/// The closed set of fields a catalog line may set.
///
/// The registry is closed and holds no dotted name: that is what makes
/// splitting a key at its **last** `.` unambiguous even for a reverse-DNS
/// identifier, and it is why an unrecognised field is refused rather than
/// ignored — a store the reader does not fully understand is never
/// half-applied.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub enum EntryKey {
    /// The name a launcher displays.
    Name,
    /// The absolute path of the `.app` bundle the entry launches.
    Bundle,
    /// The folder the entry is filed under.
    Category,
    /// The icon asset inside the bundle's own `Resources/`.
    Icon,
    /// Whether the entry is hidden from the resolved library. On a
    /// declared entry it is the curator's own suppression — the record
    /// stays, claiming its identifier against a discovery rescan, but no
    /// launcher lists it. In a patch it is the overlay's verdict on an
    /// entry declared elsewhere: `true` hides it, `false` re-shows what
    /// the layer below hid.
    Hidden,
}

impl EntryKey {
    /// Every key, in the order [`render`] emits them.
    pub const ALL: [Self; 5] = [
        Self::Name,
        Self::Bundle,
        Self::Category,
        Self::Icon,
        Self::Hidden,
    ];

    /// The canonical field name, as it is written in the store.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Name => "name",
            Self::Bundle => "bundle",
            Self::Category => "category",
            Self::Icon => "icon",
            Self::Hidden => "hidden",
        }
    }

    /// The key `field` names, or `None` when it is outside the registry.
    #[must_use]
    pub fn from_id(field: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|key| key.as_str() == field)
    }
}

impl fmt::Display for EntryKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Why a catalog document was refused.
///
/// Every variant refuses the **whole** document: a store that is not fully
/// understood is never partially applied, because a half-read library would
/// silently drop or mis-file an application a user expects to find.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum ParseError {
    /// The document exceeded [`MAX_CATALOG_LEN`].
    DocumentTooLong,
    /// A line exceeded [`MAX_LINE_LEN`].
    LineTooLong,
    /// A line held a key but no value.
    MissingValue,
    /// A key was not of the shape `<id>.<field>`.
    MalformedKey,
    /// The field named is outside the [`EntryKey`] registry.
    UnknownKey,
    /// The same `(id, field)` pair was set twice.
    DuplicateKey,
    /// The folder named is outside the [`LibraryCategory`] taxonomy.
    UnknownCategory,
    /// A `hidden` value was neither `true` nor `false`.
    MalformedFlag,
    /// A field value was refused by the entry model's own validator.
    Field(EntryError),
    /// A record named a bundle but no display name, so it declares neither
    /// a complete entry nor a patch.
    IncompleteEntry,
    /// The document held more than [`MAX_ENTRIES`] records.
    TooManyEntries,
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DocumentTooLong => f.write_str("catalog document is too long"),
            Self::LineTooLong => f.write_str("line is too long"),
            Self::MissingValue => f.write_str("setting has no value"),
            Self::MalformedKey => f.write_str("key is not of the form <id>.<field>"),
            Self::UnknownKey => f.write_str("unknown field"),
            Self::DuplicateKey => f.write_str("field is set twice for one entry"),
            Self::UnknownCategory => f.write_str("unknown folder"),
            Self::MalformedFlag => f.write_str("flag is neither true nor false"),
            Self::Field(error) => write!(f, "{error}"),
            Self::IncompleteEntry => f.write_str("entry names a bundle but no display name"),
            Self::TooManyEntries => f.write_str("catalog holds too many records"),
        }
    }
}

/// A refused catalog document, and where in it the refusal was raised.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct CatalogError {
    line: Option<usize>,
    kind: ParseError,
}

impl CatalogError {
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

impl fmt::Display for CatalogError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.line {
            Some(line) => write!(f, "line {line}: {}", self.kind),
            None => write!(f, "{}", self.kind),
        }
    }
}

impl From<EntryError> for ParseError {
    fn from(error: EntryError) -> Self {
        Self::Field(error)
    }
}

/// The fields gathered for one identifier before it is resolved into a
/// [`Record`].
///
/// Each field remembers nothing but its value; the line the identifier was
/// first seen on is kept so a record-level refusal (an incomplete entry, a
/// hidden declaration) points a reader at the block that caused it.
#[derive(Default)]
struct Draft {
    first_line: usize,
    name: Option<DisplayName>,
    bundle: Option<crate::entry::BundlePath>,
    category: Option<LibraryCategory>,
    icon: Option<IconAsset>,
    hidden: Option<bool>,
}

impl Draft {
    fn is_set(&self, key: EntryKey) -> bool {
        match key {
            EntryKey::Name => self.name.is_some(),
            EntryKey::Bundle => self.bundle.is_some(),
            EntryKey::Category => self.category.is_some(),
            EntryKey::Icon => self.icon.is_some(),
            EntryKey::Hidden => self.hidden.is_some(),
        }
    }

    fn set(&mut self, key: EntryKey, value: &str) -> Result<(), ParseError> {
        match key {
            EntryKey::Name => self.name = Some(DisplayName::new(value)?),
            EntryKey::Bundle => self.bundle = Some(crate::entry::BundlePath::new(value)?),
            EntryKey::Category => {
                self.category =
                    Some(LibraryCategory::from_id(value).ok_or(ParseError::UnknownCategory)?);
            }
            EntryKey::Icon => self.icon = Some(IconAsset::new(value)?),
            EntryKey::Hidden => self.hidden = Some(parse_flag(value)?),
        }
        Ok(())
    }

    fn into_record(self, id: EntryId) -> Result<Record, ParseError> {
        if let Some(bundle) = self.bundle {
            let name = self.name.ok_or(ParseError::IncompleteEntry)?;
            let mut entry = LibraryEntry::new(
                id,
                name,
                bundle,
                self.category.unwrap_or_default(),
                self.icon,
            );
            entry.set_hidden(self.hidden.unwrap_or(false));
            return Ok(Record::Entry(entry));
        }

        let mut patch = EntryPatch::new();
        if let Some(name) = self.name {
            patch.set_name(name);
        }
        if let Some(category) = self.category {
            patch.set_category(category);
        }
        if let Some(icon) = self.icon {
            patch.set_icon(icon);
        }
        if let Some(hidden) = self.hidden {
            patch.set_hidden(hidden);
        }
        Ok(Record::Patch(patch))
    }
}

/// The two flag spellings a catalog admits, matching the boot-time
/// configuration store's so one document's `true` cannot mean something
/// else in another.
fn parse_flag(value: &str) -> Result<bool, ParseError> {
    match value {
        "true" => Ok(true),
        "false" => Ok(false),
        _ => Err(ParseError::MalformedFlag),
    }
}

fn render_flag(value: bool) -> &'static str {
    if value {
        "true"
    } else {
        "false"
    }
}

/// Split a `<id>.<field>` key at its **last** `.`.
///
/// Splitting last, not first, is what admits a reverse-DNS identifier
/// (`com.example.editor.name`): no field name in the closed registry holds
/// a `.`, so the trailing component is always the field.
fn split_key(key: &str) -> Result<(&str, EntryKey), ParseError> {
    let (id, field) = key.rsplit_once('.').ok_or(ParseError::MalformedKey)?;
    let key = EntryKey::from_id(field).ok_or(ParseError::UnknownKey)?;
    Ok((id, key))
}

/// Parse a catalog document.
///
/// # Errors
///
/// [`CatalogError`] — the document is refused whole, never half-applied. A
/// reader falls back to [`Catalog::default`] on refusal rather than guessing
/// at a partial intent; a writer refuses the edit.
pub fn parse(text: &str) -> Result<Catalog, CatalogError> {
    if text.len() > MAX_CATALOG_LEN {
        return Err(CatalogError {
            line: None,
            kind: ParseError::DocumentTooLong,
        });
    }

    let mut drafts: BTreeMap<EntryId, Draft> = BTreeMap::new();
    for (index, raw) in text.lines().enumerate() {
        let number = index + 1;
        let at = |kind: ParseError| CatalogError {
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
        let (key, value) = line
            .split_once(char::is_whitespace)
            .ok_or_else(|| at(ParseError::MissingValue))?;
        let value = value.trim();
        if value.is_empty() {
            return Err(at(ParseError::MissingValue));
        }
        let (id, field) = split_key(key).map_err(at)?;
        let id = EntryId::new(id).map_err(|error| at(error.into()))?;

        if !drafts.contains_key(&id) && drafts.len() >= MAX_ENTRIES {
            return Err(at(ParseError::TooManyEntries));
        }
        let draft = drafts.entry(id).or_insert(Draft {
            first_line: number,
            ..Draft::default()
        });
        if draft.is_set(field) {
            return Err(at(ParseError::DuplicateKey));
        }
        draft.set(field, value).map_err(at)?;
    }

    let mut catalog = Catalog::new();
    for (id, draft) in drafts {
        let line = Some(draft.first_line);
        let record = draft
            .into_record(id.clone())
            .map_err(|kind| CatalogError { line, kind })?;
        let held = match record {
            Record::Entry(entry) => catalog.insert(entry),
            Record::Patch(patch) => catalog.patch(id, patch),
        };
        held.map_err(|_| CatalogError {
            line,
            kind: ParseError::TooManyEntries,
        })?;
    }
    Ok(catalog)
}

/// Render `catalog` as the canonical document text.
///
/// Records are emitted in identifier order, one block per record with its
/// fields in [`EntryKey::ALL`] order, so a catalog has exactly one spelling:
/// two writers holding the same records produce byte-identical text, and a
/// diff of a store shows only what actually changed.
#[must_use]
pub fn render(catalog: &Catalog) -> String {
    use core::fmt::Write as _;

    let mut text = String::new();
    for (id, record) in catalog.records() {
        let mut line = |key: EntryKey, value: &str| {
            let _ = writeln!(text, "{id}.{key} {value}");
        };
        match record {
            Record::Entry(entry) => {
                line(EntryKey::Name, entry.name().as_str());
                line(EntryKey::Bundle, entry.bundle().as_str());
                line(EntryKey::Category, entry.category().as_str());
                if let Some(icon) = entry.icon() {
                    line(EntryKey::Icon, icon.as_str());
                }
                // Visible is the default, so only a suppression is worth a
                // line; an explicit `hidden false` re-renders to nothing.
                if entry.hidden() {
                    line(EntryKey::Hidden, render_flag(true));
                }
            }
            Record::Patch(patch) => {
                if let Some(name) = patch.name() {
                    line(EntryKey::Name, name.as_str());
                }
                if let Some(category) = patch.category() {
                    line(EntryKey::Category, category.as_str());
                }
                if let Some(icon) = patch.icon() {
                    line(EntryKey::Icon, icon.as_str());
                }
                if let Some(hidden) = patch.hidden() {
                    line(EntryKey::Hidden, render_flag(hidden));
                }
            }
        }
    }
    text
}

#[cfg(test)]
#[path = "store_tests.rs"]
mod tests;
