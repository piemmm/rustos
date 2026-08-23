//! The catalog document: its closed key registry, and the fail-closed
//! reading and canonical render over the one `key = value` document engine.
//!
//! One setting is one field of one record — a `<id>.<field>` dotted key —
//! in a plain [`tairix_appconf`] document, which is the format the app-data
//! store speaks. This module therefore defines the *registry* over that
//! format and no grammar of its own: the document's length, line, key and
//! value bounds, its comment rule, and its quoting are the engine's
//! (`plans/APPDATA.md` §1.3).
//!
//! [`load`] and [`document`] are inverses: reading a rendered catalog yields
//! the same catalog, and rendering a parsed one yields a byte-identical
//! document whatever order the records were built in. That is what lets an
//! editor read a store, change one record, and write it back without
//! disturbing anything else it holds.
//!
//! # Fail closed, whole document
//!
//! A catalog is read **strictly**: a line the grammar did not read as a
//! setting, a key outside the registry, or a value outside a field's
//! validator refuses the whole document. That is the opposite of a
//! *settings* registry, where one bad value costs only its own field, and
//! deliberately so — a catalog is a list, and a half-read one silently drops
//! or mis-files an application a user expects to find, with no field left
//! standing to say that anything is missing.

use alloc::collections::BTreeMap;
use core::fmt;

use tairix_abi::LibraryCategory;
use tairix_appconf::Document;

use crate::catalog::{Catalog, EntryPatch, Record, MAX_ENTRIES};
use crate::entry::{DisplayName, EntryError, EntryId, IconAsset, LibraryEntry};

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
    /// Every key, in the order [`document`] emits them.
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
    /// A line the `key = value` grammar did not read as a setting.
    Unparsed,
    /// A key was not of the shape `<id>.<field>`.
    MalformedKey,
    /// The field named is outside the [`EntryKey`] registry.
    UnknownKey,
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
            Self::Unparsed => f.write_str("line is not a setting"),
            Self::MalformedKey => f.write_str("key is not of the form <id>.<field>"),
            Self::UnknownKey => f.write_str("unknown field"),
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
/// Each field remembers nothing but its value. A field set twice simply
/// takes the later setting, which is the format engine's own rule for a
/// repeated key and not a second opinion about it.
#[derive(Default)]
struct Draft {
    name: Option<DisplayName>,
    bundle: Option<crate::entry::BundlePath>,
    category: Option<LibraryCategory>,
    icon: Option<IconAsset>,
    hidden: Option<bool>,
}

impl Draft {
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

/// Read a catalog out of `document`.
///
/// A duplicate `(id, field)` is not a refusal: the format engine defines
/// what a repeated key means — the last setting of it wins, so appending a
/// line overrides — and this registry does not get a second opinion about
/// it. Everything the registry itself judges is a whole-document refusal.
///
/// # Errors
///
/// [`CatalogError`] — the document is refused whole, never half-applied. A
/// reader falls back to [`Catalog::default`] on refusal rather than guessing
/// at a partial intent; a writer refuses the edit.
pub fn load(document: &Document) -> Result<Catalog, CatalogError> {
    if let Some(line) = document.unparsed().next() {
        return Err(CatalogError {
            line: Some(line.line),
            kind: ParseError::Unparsed,
        });
    }

    let mut drafts: BTreeMap<EntryId, Draft> = BTreeMap::new();
    for setting in document.settings() {
        // The engine reports a line per occurrence and answers `get` with the
        // last, so a repeated key is applied in file order and the last one
        // stands — exactly what the engine's own reader would answer.
        let unplaced = |kind: ParseError| CatalogError { line: None, kind };
        let (id, field) = split_key(setting.key).map_err(unplaced)?;
        let id = EntryId::new(id).map_err(|error| unplaced(error.into()))?;

        // The record bound, enforced here rather than inferred from the
        // format's: a document of one-setting records reaches this long before
        // the engine's setting bound, so the registry has to say no itself.
        if !drafts.contains_key(&id) && drafts.len() >= MAX_ENTRIES {
            return Err(unplaced(ParseError::TooManyEntries));
        }
        let draft = drafts.entry(id).or_default();
        draft.set(field, setting.value).map_err(unplaced)?;
    }

    let mut catalog = Catalog::new();
    for (id, draft) in drafts {
        let record = draft
            .into_record(id.clone())
            .map_err(|kind| CatalogError { line: None, kind })?;
        let held = match record {
            Record::Entry(entry) => catalog.insert(entry),
            Record::Patch(patch) => catalog.patch(id, patch),
        };
        held.map_err(|_| CatalogError {
            line: None,
            kind: ParseError::TooManyEntries,
        })?;
    }
    Ok(catalog)
}

/// `catalog` as the canonical document.
///
/// Records are emitted in identifier order, one block per record with its
/// fields in [`EntryKey::ALL`] order, so a catalog has exactly one spelling:
/// two writers holding the same records produce byte-identical documents,
/// and a diff of a store shows only what actually changed.
///
/// A field the format engine would refuse is dropped rather than written
/// wrong. It cannot happen: every registry key is a dotted identifier inside
/// the key grammar and every value is already bounded by the entry model's
/// own validator, which `every_field_a_record_can_hold_is_a_legal_setting`
/// pins against the engine's own definitions.
#[must_use]
pub fn document(catalog: &Catalog) -> Document {
    let mut document = Document::new();
    for (id, record) in catalog.records() {
        let mut set = |key: EntryKey, value: &str| {
            let _ = document.set(&alloc::format!("{id}.{key}"), value);
        };
        match record {
            Record::Entry(entry) => {
                set(EntryKey::Name, entry.name().as_str());
                set(EntryKey::Bundle, entry.bundle().as_str());
                set(EntryKey::Category, entry.category().as_str());
                if let Some(icon) = entry.icon() {
                    set(EntryKey::Icon, icon.as_str());
                }
                // Visible is the default, so only a suppression is worth a
                // setting; an explicit `hidden false` renders to nothing.
                if entry.hidden() {
                    set(EntryKey::Hidden, render_flag(true));
                }
            }
            Record::Patch(patch) => {
                if let Some(name) = patch.name() {
                    set(EntryKey::Name, name.as_str());
                }
                if let Some(category) = patch.category() {
                    set(EntryKey::Category, category.as_str());
                }
                if let Some(icon) = patch.icon() {
                    set(EntryKey::Icon, icon.as_str());
                }
                if let Some(hidden) = patch.hidden() {
                    set(EntryKey::Hidden, render_flag(hidden));
                }
            }
        }
    }
    document
}

#[cfg(test)]
#[path = "store_tests.rs"]
mod tests;
