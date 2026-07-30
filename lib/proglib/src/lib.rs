//! The program-library catalog engine.
//!
//! TAIRiX keeps the folder-organised catalog of launchable applications —
//! what the desktop's Program Library presents — in text documents on the
//! volume: a machine-wide store at [`MACHINE_LIBRARY_PATH`] and an optional
//! per-user overlay at [`user_library_path`]. This crate is the **single
//! definition** of those documents: the closed folder taxonomy
//! ([`LibraryCategory`]), the validated entry model ([`LibraryEntry`]), the
//! `<id>.<key>` line grammar and its closed key registry ([`EntryKey`]), the
//! bounded fail-closed parser, the canonical render, and the one
//! machine ∪ overlay [`merge`]. The command app that edits the catalog, the
//! installer that registers a bundle, and the session that draws the library
//! all go through this engine, so a writer and a reader can never disagree
//! about what a catalog says.
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
//! an [`EntryPatch`] — a user overlay's rename, re-file, re-icon, or hide of
//! an entry it does not own — where it does not.
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
//! kernel-attested identity, the machine store's write is gated by the
//! machine-wide settings-write capability, and the per-user overlay is an
//! ordinary write under the user's own identity. Launching an entry remains
//! subject to the loader's signature and capability gate; the bundle-path
//! confinement here is the earlier, cheaper refusal.

#![no_std]
#![deny(missing_docs)]

extern crate alloc;

use alloc::format;
use alloc::string::String;

pub mod catalog;
pub mod category;
pub mod entry;
pub mod store;

pub use catalog::{merge, Catalog, CatalogFull, EntryPatch, Record, MAX_ENTRIES};
pub use category::LibraryCategory;
pub use entry::{
    BundlePath, DisplayName, EntryError, EntryId, IconAsset, LibraryEntry, MAX_BUNDLE_PATH_LEN,
    MAX_DISPLAY_NAME_LEN, MAX_ENTRY_ID_LEN, MAX_ICON_ASSET_LEN,
};
pub use store::{parse, render, CatalogError, EntryKey, ParseError, MAX_CATALOG_LEN, MAX_LINE_LEN};

/// The settings-relative directory a catalog lives in, spelled once so the
/// machine store and every per-user overlay cannot drift apart.
macro_rules! library_subdir {
    () => {
        "Settings/ProgramLibrary"
    };
}

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

/// The per-user overlay path for `username`, under that account's own
/// settings directory in its home.
///
/// The home spelling comes from the one account-layout definition
/// ([`tairix_users::default_home`]), so a moved home directory cannot
/// leave the overlay looking in a stale place.
#[must_use]
pub fn user_library_path(username: &str) -> String {
    format!(
        "{}/{}/{LIBRARY_FILE}",
        tairix_users::default_home(username),
        library_subdir!()
    )
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
    }

    #[test]
    fn the_machine_store_lives_in_the_writable_settings_subtree() {
        assert!(LIBRARY_DIR.starts_with("/System/Settings/"));
    }

    #[test]
    fn a_user_overlay_lives_under_that_users_own_home() {
        assert_eq!(
            user_library_path("ada"),
            "/Users/ada/Settings/ProgramLibrary/library.conf"
        );
    }
}
