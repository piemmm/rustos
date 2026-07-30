//! The program-library catalog engine.
//!
//! TAIRiX keeps the folder-organised catalog of launchable applications —
//! what the desktop's Program Library presents — in text documents on the
//! volume: a machine-wide store at [`MACHINE_LIBRARY_PATH`] and an optional
//! per-user overlay at [`user_library_path`]. This crate is the **single
//! definition** of those documents: the validated entry model
//! ([`LibraryEntry`], filed under the closed [`LibraryCategory`] folder
//! taxonomy the manifest ABI fixes), the `<id>.<key>` line grammar and its
//! closed key registry ([`EntryKey`]), the bounded fail-closed parser, the
//! canonical render, the one machine ∪ overlay [`merge`], and the
//! [`Catalog::reconcile`] fold that registers newly discovered bundles. The
//! command app that edits the catalog (`applib`), the installer that
//! registers a bundle, and the session that draws the library all go
//! through this engine, so a writer and a reader can never disagree about
//! what a catalog says.
//!
//! # Grammar
//!
//! The text is a sequence of lines. A `#` begins a comment that runs to the
//! end of the line; blank and comment-only lines carry no setting. Every
//! other line is one setting: a key of the shape `<id>.<field>`, whitespace,
//! and that field's value. The `<id>` part is the bundle identifier the
//! entry is keyed by ([`EntryId`]) — a reverse-DNS spelling is deliberately
//! valid, because the key is split at its **last** `.` and no field name
//! contains one. The `<field>` part is drawn from the closed [`EntryKey`]
//! registry, and each `(id, field)` pair may appear at most once.
//!
//! All the lines sharing an id form one [`Record`]: a complete
//! [`LibraryEntry`] where the record names a bundle and a display name, and
//! an [`EntryPatch`] — a user overlay's rename, re-file, re-icon, or
//! visibility verdict on an entry it does not own — where it does not. A
//! declaration may also carry its own `hidden true` suppression, which keeps
//! its identifier claimed (so a discovery rescan cannot resurrect it) while
//! the resolved catalog drops it.
//!
//! # Security
//!
//! A catalog is **untrusted input** to every consumer, including the store
//! a hostile or corrupted installer wrote: the parser is bounded
//! ([`MAX_CATALOG_LEN`], [`MAX_ENTRIES`]), validates every field through the
//! model's own validators, and fails closed ([`CatalogError`]) on anything
//! it does not fully understand — an unknown field, a folder outside the
//! taxonomy, a duplicate key, an entry whose bundle path is not an
//! application bundle in an application store, or an over-long document. A
//! reader that cannot fully parse a store runs on the empty catalog
//! ([`Catalog::default`]) rather than guessing at a partial intent, and a
//! writer refuses the edit outright.
//!
//! The engine performs no I/O and holds no authority: reading and writing
//! the documents goes through the secured VFS under the caller's own
//! kernel-attested identity. The machine store is a system-owned file under
//! `/System/Settings`, so its per-inode owner/mode/ACL record is what
//! admits or refuses a write — an ordinary account reads it but cannot
//! rewrite it — while the per-user overlay is an ordinary write under the
//! user's own identity into their own home. Launching an entry remains
//! subject to the loader's signature and capability gate; the bundle-path
//! confinement here is the earlier, cheaper refusal.

#![no_std]
#![deny(missing_docs)]

extern crate alloc;

use alloc::format;
use alloc::string::String;

pub mod catalog;
pub mod entry;
pub mod store;

pub use catalog::{merge, Catalog, CatalogFull, EntryPatch, Record, MAX_ENTRIES};
pub use entry::{
    BundlePath, DisplayName, EntryError, EntryId, IconAsset, LibraryEntry, MAX_BUNDLE_PATH_LEN,
    MAX_DISPLAY_NAME_LEN, MAX_ENTRY_ID_LEN, MAX_ICON_ASSET_LEN,
};
pub use store::{parse, render, CatalogError, EntryKey, ParseError, MAX_CATALOG_LEN, MAX_LINE_LEN};
pub use tairix_abi::LibraryCategory;

/// The catalog's own directory name under a `Settings/` tree, spelled once
/// so every path here derives from it.
macro_rules! library_component {
    () => {
        "ProgramLibrary"
    };
}

/// The settings-relative directory a catalog lives in, spelled once so the
/// machine store and every per-user overlay cannot drift apart.
macro_rules! library_subdir {
    () => {
        concat!("Settings/", library_component!())
    };
}

/// The catalog's directory name inside a `Settings/` tree — the one
/// component an image builder or settings browser creates — shared with
/// [`LIBRARY_DIR`] and [`user_library_path`] so the spellings cannot
/// drift.
pub const LIBRARY_SETTINGS_SUBDIR: &str = library_component!();

/// The directory that holds the machine-wide catalog, inside the writable
/// `/System/Settings` subtree of the root volume.
pub const LIBRARY_DIR: &str = concat!("/System/", library_subdir!());

/// The catalog document's file name, spelled once for the machine-wide
/// store and every per-user overlay.
macro_rules! library_file {
    () => {
        "library.conf"
    };
}

/// The catalog document's file name, shared by the machine-wide store and
/// every per-user overlay so the two spellings cannot drift.
pub const LIBRARY_FILE: &str = library_file!();

/// The machine-wide catalog the installer and the library-admin command
/// write and the session reads.
pub const MACHINE_LIBRARY_PATH: &str = concat!("/System/", library_subdir!(), "/", library_file!());

/// The per-user overlay path inside `home`, the account's home directory
/// exactly as the session inherited it (`HOME`) — the runtime truth even
/// for an account whose home was moved off the default layout. A trailing
/// `/` is normalised away; an empty home yields `None` rather than a
/// guessed rootward path, so a caller with no home fails closed.
#[must_use]
pub fn user_library_path(home: &str) -> Option<String> {
    let home = home.strip_suffix('/').unwrap_or(home);
    if home.is_empty() {
        return None;
    }
    Some(format!("{home}/{}/{LIBRARY_FILE}", library_subdir!()))
}

#[cfg(test)]
mod tests {
    extern crate std;

    use super::*;

    #[test]
    fn the_machine_path_is_the_library_file_inside_the_library_directory() {
        assert_eq!(
            MACHINE_LIBRARY_PATH,
            format!("{LIBRARY_DIR}/{LIBRARY_FILE}")
        );
        assert_eq!(
            LIBRARY_DIR,
            format!("/System/Settings/{LIBRARY_SETTINGS_SUBDIR}")
        );
    }

    #[test]
    fn the_machine_store_lives_in_the_writable_settings_subtree() {
        assert!(LIBRARY_DIR.starts_with("/System/Settings/"));
    }

    #[test]
    fn a_user_overlay_lives_under_that_users_own_home() {
        // The default account layout's home is the common case; the helper
        // must agree with the account-authoring policy's spelling.
        assert_eq!(
            user_library_path(&tairix_users::default_home("ada")).as_deref(),
            Some("/Users/ada/Settings/ProgramLibrary/library.conf")
        );
        // A trailing slash on the inherited HOME must not double the
        // separator, and a moved home is honoured as given.
        assert_eq!(
            user_library_path("/Users/ada/").as_deref(),
            Some("/Users/ada/Settings/ProgramLibrary/library.conf")
        );
        assert_eq!(
            user_library_path("/Storage/homes/ada").as_deref(),
            Some("/Storage/homes/ada/Settings/ProgramLibrary/library.conf")
        );
        // No home is a refusal, never a rootward guess.
        assert_eq!(user_library_path(""), None);
        assert_eq!(user_library_path("/"), None);
    }
}
