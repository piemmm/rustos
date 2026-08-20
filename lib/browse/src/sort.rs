//! The one listing sort order the file manager and the trusted picker share.
//!
//! Both views drive the same [`Browser`](crate::Browser), so the order they
//! show a directory in must come from a single definition — otherwise the two
//! could disagree about which row a click lands on. [`sort_entries`] is that
//! definition: a pure, total, stable order over [`Entry`], parameterised by a
//! [`SortMode`] the app may toggle.
//!
//! The order is **directories first, then the chosen key**. Grouping folders
//! ahead of files (and bundles) is fixed regardless of direction — reversing
//! only reverses the order *within* each group — because a file manager that
//! scattered folders through a descending size list would be harder to scan,
//! not easier. Equal keys fall back to a deterministic case-insensitive name
//! order so the result never depends on the source's incidental ordering.

use core::cmp::Ordering;

use crate::entry::{Entry, EntryKind};
use crate::media::extension;

/// Which field a listing is ordered by.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash, Default)]
pub enum SortKey {
    /// Case-insensitive name order — the sensible default for a general
    /// directory listing (a person scanning for a name).
    #[default]
    Name,
    /// The entry's structural kind — directory, then application bundle,
    /// then plain file — with files further clustered by extension. This is
    /// the coarse, directory-context-free grouping [`Entry`] alone can name;
    /// it is deliberately not the full [`crate::media::MediaType`] registry
    /// (which needs the listed directory's own path to tell a service bundle
    /// from an application one), so it stays a plain field comparison like
    /// every other key here rather than a second classifier.
    Kind,
    /// Apparent byte size. Directories and bundles have size `0`, so within
    /// their group this collapses to the name tiebreak.
    Size,
    /// Last-modification instant.
    Modified,
}

/// Ascending or descending within the [`SortKey`].
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash, Default)]
pub enum SortDirection {
    /// Smallest / earliest / A→Z first.
    #[default]
    Ascending,
    /// Largest / latest / Z→A first.
    Descending,
}

/// How a listing is ordered: a [`SortKey`] and a [`SortDirection`].
///
/// The default — [`SortKey::Name`] ascending — is the general-purpose listing
/// order both views open with.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash, Default)]
pub struct SortMode {
    /// The field ordered by.
    pub key: SortKey,
    /// The direction within the key.
    pub direction: SortDirection,
}

impl SortMode {
    /// The listing order both views open with: by name, ascending.
    #[must_use]
    pub const fn default_order() -> Self {
        Self {
            key: SortKey::Name,
            direction: SortDirection::Ascending,
        }
    }

    /// The next order in the fixed cycle the toolbar's Sort command steps
    /// through: name ↑, name ↓, kind ↑, kind ↓, size ↑, size ↓, modified ↑,
    /// modified ↓, then back to name ↑. A single command that walks every
    /// mode in a well-understood order (a menu of the same modes is the
    /// later, richer surface); the wrap makes it total.
    #[must_use]
    pub const fn next(self) -> Self {
        use SortDirection::{Ascending, Descending};
        use SortKey::{Kind, Modified, Name, Size};
        let (key, direction) = match (self.key, self.direction) {
            (Name, Ascending) => (Name, Descending),
            (Name, Descending) => (Kind, Ascending),
            (Kind, Ascending) => (Kind, Descending),
            (Kind, Descending) => (Size, Ascending),
            (Size, Ascending) => (Size, Descending),
            (Size, Descending) => (Modified, Ascending),
            (Modified, Ascending) => (Modified, Descending),
            (Modified, Descending) => (Name, Ascending),
        };
        Self { key, direction }
    }
}

/// Sort `entries` in place into the shared listing order for `mode`.
///
/// Stable and total: directories come before files and bundles, then the
/// chosen key decides, then a case-insensitive name tiebreak makes the result
/// independent of the input order.
pub fn sort_entries(entries: &mut [Entry], mode: SortMode) {
    entries.sort_by(|a, b| entry_cmp(a, b, mode));
}

/// The group a kind sorts into: directories (`0`) ahead of everything else
/// (`1`). A bundle is a launchable unit, not a folder to descend, so it sorts
/// with files rather than with directories.
fn group_rank(entry: &Entry) -> u8 {
    // Directories (rank 0) sort ahead of everything else (rank 1).
    u8::from(!entry.is_directory())
}

/// The total order two entries take under `mode`.
fn entry_cmp(a: &Entry, b: &Entry, mode: SortMode) -> Ordering {
    group_rank(a).cmp(&group_rank(b)).then_with(|| {
        let primary = match mode.key {
            SortKey::Name => name_cmp(a.name(), b.name()),
            SortKey::Kind => kind_cmp(a, b),
            SortKey::Size => a.size().cmp(&b.size()),
            SortKey::Modified => a.modified().cmp(&b.modified()),
        };
        let primary = match mode.direction {
            SortDirection::Ascending => primary,
            SortDirection::Descending => primary.reverse(),
        };
        // A deterministic, direction-independent tiebreak so equal keys never
        // leave the order at the mercy of the source's incidental ordering.
        primary.then_with(|| name_cmp(a.name(), b.name()))
    })
}

/// Compare two names case-insensitively (ASCII fold), breaking an
/// otherwise-equal comparison by the raw bytes so distinct names never
/// compare equal. Allocation-free: it folds byte by byte rather than building
/// lowercased copies.
fn name_cmp(a: &str, b: &str) -> Ordering {
    let folded = a
        .bytes()
        .map(|byte| byte.to_ascii_lowercase())
        .cmp(b.bytes().map(|byte| byte.to_ascii_lowercase()));
    folded.then_with(|| a.as_bytes().cmp(b.as_bytes()))
}

/// The rank [`SortKey::Kind`] clusters an entry's structural kind into:
/// directories first, then application bundles, then plain files. This is
/// finer than [`group_rank`] (which only separates directories from
/// everything else) but coarser than [`crate::media::MediaType`] (which needs
/// directory context this comparator does not have).
const fn kind_rank(entry: &Entry) -> u8 {
    // A link ranks with what it *names*, so a shortcut to a folder sorts
    // among the folders; one that names nothing ranks with the files, where
    // an unopenable leaf belongs.
    match entry.kind().resolved() {
        Some(EntryKind::Directory) => 0,
        Some(EntryKind::Bundle) => 1,
        Some(EntryKind::File | EntryKind::Link(_)) | None => 2,
    }
}

/// Compare two entries by [`SortKey::Kind`]: [`kind_rank`], then — for two
/// files — a case-insensitive extension fold, so files of one type cluster
/// together within the file group.
fn kind_cmp(a: &Entry, b: &Entry) -> Ordering {
    kind_rank(a).cmp(&kind_rank(b)).then_with(|| {
        let ext_a = extension(a.name()).unwrap_or_default();
        let ext_b = extension(b.name()).unwrap_or_default();
        ext_a
            .bytes()
            .map(|byte| byte.to_ascii_lowercase())
            .cmp(ext_b.bytes().map(|byte| byte.to_ascii_lowercase()))
    })
}
