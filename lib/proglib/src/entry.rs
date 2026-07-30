//! The validated model of one program-library item.
//!
//! Everything a launcher needs to draw and start one application — its
//! stable key, the bundle it launches, the name a user reads, the folder
//! it is filed under, and the icon asset that represents it. Each field is
//! its own validated type ([`EntryId`], [`DisplayName`], [`BundlePath`],
//! [`IconAsset`]), so a field that exists is a field that is well-formed
//! and [`LibraryEntry`] itself cannot fail to build: illegal states are
//! unrepresentable rather than re-checked at each use, and the catalog's
//! merge and render paths need no fallible re-validation.

use alloc::string::{String, ToString};
use core::fmt;

use tairix_abi::{
    LibraryCategory, BUNDLE_ID_MAX, BUNDLE_NAME_MAX, BUNDLE_SUFFIX, LIBRARY_ICON_MAX,
};

/// Maximum length, in bytes, of an entry identifier: the bundle identifier
/// bound the signed manifest already fixes, so a catalog key and the
/// `AppInfo` id it is derived from can never disagree on width.
pub const MAX_ENTRY_ID_LEN: usize = BUNDLE_ID_MAX;

/// Maximum length, in bytes, of the display name an entry shows: the
/// bundle-name bound the signed manifest already fixes.
pub const MAX_DISPLAY_NAME_LEN: usize = BUNDLE_NAME_MAX;

/// Maximum length, in bytes, of an entry's bundle path. The store lists
/// bundles in the two app stores, whose paths are short; a longer spelling
/// is a malformed store, not a workload.
pub const MAX_BUNDLE_PATH_LEN: usize = 512;

/// Maximum length, in bytes, of an entry's icon asset name — a plain file
/// name inside the bundle's own `Resources/` directory: the icon bound the
/// signed manifest already fixes, so a catalog icon and the `AppInfo` icon
/// it is derived from can never disagree on width.
pub const MAX_ICON_ASSET_LEN: usize = LIBRARY_ICON_MAX;

/// The `/`-view component naming the machine-wide application store,
/// derived from the one store-path definition
/// ([`tairix_abi::USER_APP_STORE`]) rather than spelled a second time. A
/// unit test pins the two together, so moving the store cannot leave this
/// check matching a stale directory.
const USER_APP_STORE_LEAF: &str = "Apps";

/// The `/`-view component naming the per-user home directories, under
/// which each account's own application store lives
/// (`/Users/<name>/Apps`). Pinned by a unit test to the home-directory
/// layout the account-authoring policy fixes
/// (`tairix_users::policy::default_home`), the single definition of that
/// spelling.
const USERS_VIEW_LEAF: &str = "Users";

/// The `/`-view component naming the read-only `/System` subtree, under
/// which the system app store lives (`/System/Apps`). Pinned by a unit
/// test to the one store-path definition
/// ([`tairix_abi::SYSTEM_APP_STORE`]).
const SYSTEM_VIEW_LEAF: &str = "System";

/// Why a program-library entry field was refused.
///
/// Every variant is a refusal of the whole field: a malformed value never
/// yields a partially-sanitised one.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum EntryError {
    /// The identifier was empty.
    EmptyId,
    /// The identifier exceeded [`MAX_ENTRY_ID_LEN`].
    IdTooLong,
    /// The identifier held a byte outside the permitted set, or began or
    /// ended with a separator.
    MalformedId,
    /// The display name was empty.
    EmptyName,
    /// The display name exceeded [`MAX_DISPLAY_NAME_LEN`].
    NameTooLong,
    /// The display name held a control character, a line break, or a byte
    /// the store grammar cannot round-trip.
    MalformedName,
    /// The bundle path exceeded [`MAX_BUNDLE_PATH_LEN`].
    BundlePathTooLong,
    /// The bundle path was not a valid absolute path, or did not name a
    /// `.app` directory inside a permitted application store.
    MalformedBundlePath,
    /// The icon asset name exceeded [`MAX_ICON_ASSET_LEN`].
    IconAssetTooLong,
    /// The icon asset name was not a plain file name (it was empty, held a
    /// path separator, or named a directory traversal).
    MalformedIconAsset,
}

impl fmt::Display for EntryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let text = match self {
            Self::EmptyId => "entry identifier is empty",
            Self::IdTooLong => "entry identifier is too long",
            Self::MalformedId => "entry identifier has an invalid character",
            Self::EmptyName => "display name is empty",
            Self::NameTooLong => "display name is too long",
            Self::MalformedName => "display name has an invalid character",
            Self::BundlePathTooLong => "bundle path is too long",
            Self::MalformedBundlePath => "bundle path is not an application bundle",
            Self::IconAssetTooLong => "icon asset name is too long",
            Self::MalformedIconAsset => "icon asset name is not a plain file name",
        };
        f.write_str(text)
    }
}

/// Whether `value` survives a store round-trip unchanged.
///
/// A stored value is the remainder of its line, so it must hold no
/// control character (a line break would forge a second setting), no `#`
/// (which would begin a comment), and no leading or trailing whitespace
/// (which the parser trims). This is the one definition every field type
/// here and the catalog renderer judge a value against, so a value a field
/// accepts is always a value the store can re-read.
pub(crate) fn value_is_renderable(value: &str) -> bool {
    !value.chars().any(|c| c.is_control() || c == '#') && value.trim() == value
}

/// The stable key one program-library entry is addressed by: the bundle
/// identifier its signed `AppInfo` manifest declares.
///
/// The identifier is what a user overlay patches against and what the
/// store grammar keys a line on, so it is validated once here and never
/// re-checked: it is non-empty, bounded by [`MAX_ENTRY_ID_LEN`], built
/// only from ASCII alphanumerics and the `.`, `-`, `_` separators, and
/// begins and ends with an alphanumeric. That charset makes an identifier
/// a single grammar token — it cannot contain whitespace, a comment
/// marker, or a line break — so a hostile bundle name can never inject a
/// second setting into a rendered store.
///
/// A reverse-DNS identifier (`com.example.editor`) is deliberately valid:
/// the store splits a line's key at its **last** `.`, and the key registry
/// holds no dotted key, so the split is unambiguous.
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct EntryId(String);

impl EntryId {
    /// Validate `id` as an entry identifier.
    ///
    /// # Errors
    ///
    /// [`EntryError::EmptyId`], [`EntryError::IdTooLong`], or
    /// [`EntryError::MalformedId`] — the identifier is refused outright
    /// rather than sanitised.
    pub fn new(id: &str) -> Result<Self, EntryError> {
        if id.is_empty() {
            return Err(EntryError::EmptyId);
        }
        if id.len() > MAX_ENTRY_ID_LEN {
            return Err(EntryError::IdTooLong);
        }
        let bytes = id.as_bytes();
        let is_separator = |b: u8| matches!(b, b'.' | b'-' | b'_');
        if !bytes
            .iter()
            .all(|&b| b.is_ascii_alphanumeric() || is_separator(b))
        {
            return Err(EntryError::MalformedId);
        }
        let first = bytes[0];
        let last = bytes[bytes.len() - 1];
        if !first.is_ascii_alphanumeric() || !last.is_ascii_alphanumeric() {
            return Err(EntryError::MalformedId);
        }
        Ok(Self(id.to_string()))
    }

    /// The identifier text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for EntryId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A validated human-readable name a launcher displays.
///
/// The text is untrusted — a bundle manifest wrote it, or a user renamed
/// an entry — and it is rendered back into a line-oriented store, so it is
/// non-empty, bounded by [`MAX_DISPLAY_NAME_LEN`], and renderable: no
/// control character, no comment marker, no surrounding whitespace.
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct DisplayName(String);

impl DisplayName {
    /// Validate `name` as a display name.
    ///
    /// # Errors
    ///
    /// [`EntryError::EmptyName`], [`EntryError::NameTooLong`], or
    /// [`EntryError::MalformedName`].
    pub fn new(name: &str) -> Result<Self, EntryError> {
        if name.is_empty() {
            return Err(EntryError::EmptyName);
        }
        if name.len() > MAX_DISPLAY_NAME_LEN {
            return Err(EntryError::NameTooLong);
        }
        if !value_is_renderable(name) {
            return Err(EntryError::MalformedName);
        }
        Ok(Self(name.to_string()))
    }

    /// The name text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for DisplayName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// The application stores a library entry may name a bundle in, expressed
/// as the leading `/`-view components of a bundle path.
///
/// The machine-wide store is `/Apps`; a user's own store is
/// `/Users/<name>/Apps`; and the read-only system app store
/// (`/System/Apps`) holds the OS-shipped programs, among them the
/// graphical applications a fresh install's library lists (the file
/// manager, the terminal). The service store is deliberately absent:
/// daemons are not the user-facing applications a launcher offers, and a
/// catalog claiming one is malformed.
///
/// Every store permits plain nesting (`/Apps/games/chess.app`), so only
/// the prefix is fixed here; the depth beneath it is not.
fn store_body(components: &[String]) -> Option<&[String]> {
    match components {
        [store, rest @ ..] if store == USER_APP_STORE_LEAF => Some(rest),
        [system, store, rest @ ..]
            if system == SYSTEM_VIEW_LEAF && store == USER_APP_STORE_LEAF =>
        {
            Some(rest)
        }
        [users, _account, store, rest @ ..]
            if users == USERS_VIEW_LEAF && store == USER_APP_STORE_LEAF =>
        {
            Some(rest)
        }
        _ => None,
    }
}

/// The validated absolute path of the application bundle an entry
/// launches.
///
/// The stored spelling is the parser's canonical rendering, not the
/// caller's input, so two spellings of one bundle are one value: they
/// compare equal, hash alike, and the store writes back the same text it
/// re-reads rather than preserving a redundant `.`/`..` detour.
///
/// The path parses under the one shared path-spelling parser, is rooted in
/// the `/` session view, sits inside one of the application stores a
/// launcher may start a bundle from, and ends at a `<name>.app` directory
/// whose ancestors are plain directories — a bundle is a sealed unit,
/// never a container of further bundles. Confining the path here is what
/// stops a hostile or corrupted catalog from turning a launcher click into
/// the execution of an arbitrary directory: authority still rests with the
/// loader's signature and capability gate, and this is the earlier,
/// cheaper refusal.
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct BundlePath(String);

impl BundlePath {
    /// Validate `path` as an application-bundle path.
    ///
    /// # Errors
    ///
    /// [`EntryError::BundlePathTooLong`] or
    /// [`EntryError::MalformedBundlePath`].
    pub fn new(path: &str) -> Result<Self, EntryError> {
        if path.len() > MAX_BUNDLE_PATH_LEN {
            return Err(EntryError::BundlePathTooLong);
        }
        let parsed = tairix_path::parse(path).map_err(|_| EntryError::MalformedBundlePath)?;
        if !matches!(parsed.root(), tairix_path::Root::View) {
            return Err(EntryError::MalformedBundlePath);
        }
        let Some(body) = store_body(parsed.components()) else {
            return Err(EntryError::MalformedBundlePath);
        };
        let Some((leaf, ancestors)) = body.split_last() else {
            return Err(EntryError::MalformedBundlePath);
        };
        if !leaf.ends_with(BUNDLE_SUFFIX) || leaf.len() == BUNDLE_SUFFIX.len() {
            return Err(EntryError::MalformedBundlePath);
        }
        if ancestors.iter().any(|c| c.ends_with(BUNDLE_SUFFIX)) {
            return Err(EntryError::MalformedBundlePath);
        }
        Ok(Self(parsed.to_string()))
    }

    /// The canonical path text: the spelling the shared parser normalised
    /// the input to, never the caller's original wording.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for BundlePath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A validated icon asset name: a plain file name resolved inside the
/// entry's **own** bundle `Resources/` directory.
///
/// It is a leaf name judged by the one shared filesystem leaf-name rule —
/// never a path — so it holds no separator, no `.`/`..`, no control
/// character and no `:`. A catalog therefore cannot point the icon loader
/// at a file outside the bundle it describes, whatever the store says.
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct IconAsset(String);

impl IconAsset {
    /// Validate `asset` as an icon asset name.
    ///
    /// # Errors
    ///
    /// [`EntryError::IconAssetTooLong`] or
    /// [`EntryError::MalformedIconAsset`].
    pub fn new(asset: &str) -> Result<Self, EntryError> {
        if asset.len() > MAX_ICON_ASSET_LEN {
            return Err(EntryError::IconAssetTooLong);
        }
        tairix_path::validate_file_name(asset).map_err(|_| EntryError::MalformedIconAsset)?;
        if !value_is_renderable(asset) {
            return Err(EntryError::MalformedIconAsset);
        }
        Ok(Self(asset.to_string()))
    }

    /// The asset name.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for IconAsset {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// One launchable application in the program library.
///
/// Every field is a validated type, so an entry is well-formed by
/// construction: consumers read the accessors without re-checking, the
/// renderer emits it knowing it round-trips, and the merge path applies a
/// user's rename or re-file without a fallible re-validation.
///
/// An entry may be declared **hidden**: it keeps its record — and so keeps
/// its identifier claimed against a discovery rescan that would otherwise
/// re-register the bundle — but the resolved catalog a launcher draws
/// drops it. Hiding is how a curated store suppresses an installed
/// application without deleting the fact of its installation; it is
/// presentation, not authority — launching the bundle stays governed by
/// the loader's signature and capability gate either way.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LibraryEntry {
    id: EntryId,
    name: DisplayName,
    bundle: BundlePath,
    category: LibraryCategory,
    icon: Option<IconAsset>,
    hidden: bool,
}

impl LibraryEntry {
    /// Build an entry from already-validated fields. It is visible until
    /// [`set_hidden`](Self::set_hidden) says otherwise.
    #[must_use]
    pub fn new(
        id: EntryId,
        name: DisplayName,
        bundle: BundlePath,
        category: LibraryCategory,
        icon: Option<IconAsset>,
    ) -> Self {
        Self {
            id,
            name,
            bundle,
            category,
            icon,
            hidden: false,
        }
    }

    /// The stable key this entry is addressed by.
    #[must_use]
    pub fn id(&self) -> &EntryId {
        &self.id
    }

    /// The name a launcher displays.
    #[must_use]
    pub fn name(&self) -> &DisplayName {
        &self.name
    }

    /// The absolute path of the `.app` bundle this entry launches.
    #[must_use]
    pub fn bundle(&self) -> &BundlePath {
        &self.bundle
    }

    /// The folder this entry is filed under.
    #[must_use]
    pub fn category(&self) -> LibraryCategory {
        self.category
    }

    /// The icon asset inside the bundle's `Resources/`, if it declares
    /// one. `None` means the launcher draws its class fallback icon.
    #[must_use]
    pub fn icon(&self) -> Option<&IconAsset> {
        self.icon.as_ref()
    }

    /// Replace the display name (a user rename).
    pub fn set_name(&mut self, name: DisplayName) {
        self.name = name;
    }

    /// Re-file the entry under another folder.
    pub fn set_category(&mut self, category: LibraryCategory) {
        self.category = category;
    }

    /// Replace the icon asset the entry draws with.
    pub fn set_icon(&mut self, icon: IconAsset) {
        self.icon = Some(icon);
    }

    /// Whether the entry is hidden from the resolved library.
    #[must_use]
    pub fn hidden(&self) -> bool {
        self.hidden
    }

    /// Hide the entry from the resolved library, or show it again.
    pub fn set_hidden(&mut self, hidden: bool) {
        self.hidden = hidden;
    }
}

#[cfg(test)]
#[path = "entry_tests.rs"]
mod tests;
