//! The catalog container, the user-overlay patch, and the one
//! machine ∪ overlay merge.
//!
//! A catalog holds exactly one [`Record`] per [`EntryId`]: either the whole
//! [`LibraryEntry`] a document declares, or the [`EntryPatch`] that adjusts
//! an entry declared elsewhere. [`merge`] folds a machine-wide catalog and
//! a user's overlay into the single resolved catalog a launcher draws, so
//! the precedence rules exist exactly once and every surface that presents
//! the library agrees on what it holds.

use alloc::collections::{BTreeMap, BTreeSet};
use alloc::vec::Vec;
use core::fmt;

use tairix_abi::LibraryCategory;

use crate::entry::{DisplayName, EntryId, IconAsset, LibraryEntry};

/// Maximum number of records one catalog document may hold.
///
/// A catalog is untrusted input parsed into memory before any of it is
/// used, so its record count is a fixed security bound rather than a
/// growable capacity: it exists to stop a hostile or corrupted store from
/// forcing unbounded allocation, not to cap a real workload. It is set far
/// above any plausible number of installed applications, and a store that
/// exceeds it is refused outright rather than truncated.
pub const MAX_ENTRIES: usize = 4096;

/// A catalog already holds [`MAX_ENTRIES`] records, so the record offered
/// was refused.
///
/// Refusing rather than evicting keeps the bound fail-closed: a writer
/// learns its edit did not land instead of silently losing another entry.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct CatalogFull;

impl fmt::Display for CatalogFull {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("catalog already holds the maximum number of records")
    }
}

/// A user's adjustment to an entry their own document does not declare.
///
/// The machine-wide catalog is a system-owned file an unprivileged
/// account may read but not rewrite (its per-inode owner/mode/ACL record
/// refuses the write). A patch is how such an account personalises what
/// it sees — rename an application, re-file it into another folder, give
/// it a different icon, or hide it — without holding any authority over
/// the machine store, and without copying the entry, which would leave a
/// stale duplicate behind when the bundle is upgraded or removed.
///
/// A patch carries only the fields it changes, and every field it does
/// carry is already validated, so applying one cannot fail.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct EntryPatch {
    name: Option<DisplayName>,
    category: Option<LibraryCategory>,
    icon: Option<IconAsset>,
    hidden: Option<bool>,
}

impl EntryPatch {
    /// A patch that changes nothing.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether this patch changes nothing.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.name.is_none()
            && self.category.is_none()
            && self.icon.is_none()
            && self.hidden.is_none()
    }

    /// The replacement display name, if this patch renames the entry.
    #[must_use]
    pub fn name(&self) -> Option<&DisplayName> {
        self.name.as_ref()
    }

    /// The folder this patch re-files the entry into, if any.
    #[must_use]
    pub fn category(&self) -> Option<LibraryCategory> {
        self.category
    }

    /// The replacement icon asset, if this patch re-icons the entry.
    #[must_use]
    pub fn icon(&self) -> Option<&IconAsset> {
        self.icon.as_ref()
    }

    /// Whether this patch hides (`Some(true)`) or explicitly re-shows
    /// (`Some(false)`) the entry, or leaves its visibility alone (`None`).
    #[must_use]
    pub fn hidden(&self) -> Option<bool> {
        self.hidden
    }

    /// Rename the entry.
    pub fn set_name(&mut self, name: DisplayName) {
        self.name = Some(name);
    }

    /// Re-file the entry under `category`.
    pub fn set_category(&mut self, category: LibraryCategory) {
        self.category = Some(category);
    }

    /// Draw the entry with `icon` instead of the one it declares.
    pub fn set_icon(&mut self, icon: IconAsset) {
        self.icon = Some(icon);
    }

    /// Hide the entry from the library, or explicitly re-show it.
    pub fn set_hidden(&mut self, hidden: bool) {
        self.hidden = Some(hidden);
    }

    /// Apply this patch's field changes to `entry`, its visibility verdict
    /// included: patches are applied machine-first, so the last verdict
    /// written — the user's own — is the one [`merge`] resolves.
    fn apply_to(&self, entry: &mut LibraryEntry) {
        if let Some(name) = &self.name {
            entry.set_name(name.clone());
        }
        if let Some(category) = self.category {
            entry.set_category(category);
        }
        if let Some(icon) = &self.icon {
            entry.set_icon(icon.clone());
        }
        if let Some(hidden) = self.hidden {
            entry.set_hidden(hidden);
        }
    }
}

/// What a catalog holds under one identifier.
///
/// Which of the two a record is falls out of what its lines say: a record
/// that names a bundle is a whole entry, and one that does not is a patch
/// against an entry declared in another document. There is no separate
/// marker that could disagree, and an identifier holds one record or the
/// other — never both — so a rendered document has exactly one shape.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Record {
    /// A complete entry this document declares.
    Entry(LibraryEntry),
    /// An adjustment to an entry declared in another document.
    Patch(EntryPatch),
}

/// One parsed program-library document, or the resolved catalog [`merge`]
/// produces.
///
/// Records are held in identifier order, so iteration, rendering, and
/// equality are deterministic: two catalogs holding the same records always
/// render to byte-identical text, whatever order they were built in.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Catalog {
    records: BTreeMap<EntryId, Record>,
}

impl Catalog {
    /// An empty catalog — also what a reader falls back to when a store
    /// cannot be read or parsed.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The number of records the catalog holds.
    #[must_use]
    pub fn len(&self) -> usize {
        self.records.len()
    }

    /// Whether the catalog holds no record at all.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }

    /// Declare `entry`, replacing whatever record its identifier already
    /// held and returning it.
    ///
    /// # Errors
    ///
    /// [`CatalogFull`] when the catalog already holds [`MAX_ENTRIES`]
    /// records and this identifier is not one of them.
    pub fn insert(&mut self, entry: LibraryEntry) -> Result<Option<Record>, CatalogFull> {
        let id = entry.id().clone();
        self.put(id, Record::Entry(entry))
    }

    /// Record `patch` against `id`, replacing whatever record that
    /// identifier already held and returning it.
    ///
    /// A patch that changes nothing ([`EntryPatch::is_empty`]) is not a
    /// record a catalog holds — it has no rendering, so holding one would
    /// leave a document that no longer round-trips through
    /// [`render`](crate::render). Recording an empty patch therefore clears
    /// the personalisation held under `id`, which is what "adjust this
    /// entry in no way" means.
    ///
    /// # Errors
    ///
    /// [`CatalogFull`] when the catalog already holds [`MAX_ENTRIES`]
    /// records and this identifier is not one of them.
    pub fn patch(&mut self, id: EntryId, patch: EntryPatch) -> Result<Option<Record>, CatalogFull> {
        if patch.is_empty() {
            return Ok(self.records.remove(&id));
        }
        self.put(id, Record::Patch(patch))
    }

    fn put(&mut self, id: EntryId, record: Record) -> Result<Option<Record>, CatalogFull> {
        if !self.records.contains_key(&id) && self.records.len() >= MAX_ENTRIES {
            return Err(CatalogFull);
        }
        Ok(self.records.insert(id, record))
    }

    /// Drop the record held under `id`, returning it.
    pub fn remove(&mut self, id: &EntryId) -> Option<Record> {
        self.records.remove(id)
    }

    /// The record held under `id`.
    #[must_use]
    pub fn get(&self, id: &EntryId) -> Option<&Record> {
        self.records.get(id)
    }

    /// The entry declared under `id`, if that identifier holds a whole
    /// entry rather than a patch.
    #[must_use]
    pub fn entry(&self, id: &EntryId) -> Option<&LibraryEntry> {
        match self.records.get(id) {
            Some(Record::Entry(entry)) => Some(entry),
            _ => None,
        }
    }

    /// The patch held against `id`, if that identifier holds a patch rather
    /// than a whole entry.
    #[must_use]
    pub fn entry_patch(&self, id: &EntryId) -> Option<&EntryPatch> {
        match self.records.get(id) {
            Some(Record::Patch(patch)) => Some(patch),
            _ => None,
        }
    }

    /// Every record, in identifier order.
    pub fn records(&self) -> impl Iterator<Item = (&EntryId, &Record)> {
        self.records.iter()
    }

    /// Every declared entry, in identifier order.
    pub fn entries(&self) -> impl Iterator<Item = &LibraryEntry> {
        self.records.values().filter_map(|record| match record {
            Record::Entry(entry) => Some(entry),
            Record::Patch(_) => None,
        })
    }

    /// Every patch, in identifier order, paired with the identifier it
    /// adjusts.
    pub fn patches(&self) -> impl Iterator<Item = (&EntryId, &EntryPatch)> {
        self.records.iter().filter_map(|(id, record)| match record {
            Record::Patch(patch) => Some((id, patch)),
            Record::Entry(_) => None,
        })
    }

    /// The entries filed under `category`, in the order a launcher presents
    /// them: by display name, then by identifier so two identically-named
    /// applications still have a stable order.
    ///
    /// Presentation order lives here, once, so the launcher's folder view
    /// and any other surface listing a folder cannot disagree.
    #[must_use]
    pub fn folder(&self, category: LibraryCategory) -> Vec<&LibraryEntry> {
        let mut listed: Vec<&LibraryEntry> = self
            .entries()
            .filter(|entry| entry.category() == category)
            .collect();
        listed.sort_unstable_by(|left, right| {
            left.name()
                .cmp(right.name())
                .then_with(|| left.id().cmp(right.id()))
        });
        listed
    }

    /// The folders that actually hold an entry, in the taxonomy's
    /// presentation order.
    ///
    /// A launcher draws the library as the folders it lists here, each
    /// filled by [`folder`](Self::folder), so an empty folder is never
    /// offered to a user as something to open into nothing.
    #[must_use]
    pub fn folders(&self) -> Vec<LibraryCategory> {
        LibraryCategory::ALL
            .into_iter()
            .filter(|&category| self.entries().any(|entry| entry.category() == category))
            .collect()
    }

    /// Fold newly `discovered` applications into the catalog: declare every
    /// entry whose identifier no existing record claims, and leave every
    /// existing record — a curated entry or a patch — exactly as it stands,
    /// so a rescan can register what an installer missed without disturbing
    /// an administrator's curation. Within `discovered`, the first entry
    /// under an identifier wins; a later duplicate is skipped, so a caller's
    /// deterministic discovery order decides.
    ///
    /// Returns how many entries were declared; `0` means the catalog is
    /// unchanged and a caller need not rewrite its store.
    ///
    /// # Errors
    ///
    /// [`CatalogFull`] when declaring the new entries would exceed
    /// [`MAX_ENTRIES`]; the catalog is left **unchanged** — the fold is
    /// refused whole rather than half-applied.
    pub fn reconcile(&mut self, discovered: &[LibraryEntry]) -> Result<usize, CatalogFull> {
        let mut new_ids: BTreeSet<&EntryId> = BTreeSet::new();
        for entry in discovered {
            if !self.records.contains_key(entry.id()) {
                new_ids.insert(entry.id());
            }
        }
        let added = new_ids.len();
        if self.records.len() + added > MAX_ENTRIES {
            return Err(CatalogFull);
        }
        for entry in discovered {
            if new_ids.remove(entry.id()) {
                self.records
                    .insert(entry.id().clone(), Record::Entry(entry.clone()));
            }
        }
        Ok(added)
    }
}

/// Fold a machine-wide catalog and a user's overlay into the resolved
/// catalog a launcher draws.
///
/// Precedence, applied in this order:
///
/// 1. The machine catalog's declared entries.
/// 2. The overlay's declared entries, which replace a machine entry of the
///    same identifier outright: a user's own bundle wins over a
///    machine-wide one it shadows.
/// 3. The machine catalog's patches, then the overlay's, applied to
///    whatever entry stands under each identifier — so a user's adjustment
///    wins field by field over a machine-wide one, the visibility verdict
///    included: a user's own overlay re-shows an application the machine
///    store declared hidden, and hides one it shows. Hiding is
///    presentation, never authority (launching stays behind the loader's
///    signature and capability gate), so the account that owns the view
///    has the last word on it.
/// 4. An entry whose resolved verdict is hidden is dropped from the
///    result. Its record stays in the document that declared it, so its
///    identifier remains claimed and a discovery rescan cannot resurrect
///    what a curator suppressed.
///
/// A patch whose identifier names no entry is discarded: its bundle has
/// been removed or was never installed, which is an ordinary state of the
/// world rather than an error — and because the patch stays in the user's
/// own document, re-installing the application restores the
/// personalisation.
///
/// The result declares visible entries only; every patch has been resolved
/// into the entry it adjusted or discarded. Applying a patch cannot fail,
/// so neither can this. The result can never exceed [`MAX_ENTRIES`]
/// records either, since it holds at most one entry per identifier the two
/// bounded inputs declare between them.
#[must_use]
pub fn merge(machine: &Catalog, overlay: &Catalog) -> Catalog {
    let mut entries: BTreeMap<EntryId, LibraryEntry> = BTreeMap::new();
    for entry in machine.entries().chain(overlay.entries()) {
        entries.insert(entry.id().clone(), entry.clone());
    }

    for (id, patch) in machine.patches().chain(overlay.patches()) {
        if let Some(entry) = entries.get_mut(id) {
            patch.apply_to(entry);
        }
    }

    Catalog {
        records: entries
            .into_iter()
            .filter(|(_, entry)| !entry.hidden())
            .map(|(id, entry)| (id, Record::Entry(entry)))
            .collect(),
    }
}

#[cfg(test)]
#[path = "catalog_tests.rs"]
mod tests;
