//! The program-library catalog engine.
//!
//! TAIRiX keeps the folder-organised catalog of launchable applications —
//! what the desktop's Program Library presents — in two layers: a
//! machine-wide store at [`LIBRARY_PATH`] and an optional per-user overlay
//! in the library-admin command's **published** app-data scope
//! ([`LIBRARY_PUBLISHER`]). This crate is the **single definition** of that
//! document: the validated entry model ([`LibraryEntry`], filed under the
//! closed [`LibraryCategory`] folder taxonomy the manifest ABI fixes), the
//! `<id>.<field>` key registry ([`EntryKey`]), the fail-closed reading
//! ([`load`]), the canonical render ([`document`]), the one machine ∪
//! overlay [`merge`], and the [`Catalog::reconcile`] fold that registers
//! newly discovered bundles. The command app that edits the catalog
//! (`applib`), the installer that registers a bundle, and the session that
//! draws the library all go through this engine, so a writer and a reader
//! can never disagree about what a catalog says.
//!
//! # The two layers, and why only one of them moved
//!
//! The **machine** store stays an ordinary `/System/Settings` administrator
//! document, beside the machine's network and configuration stores: it is
//! machine policy rather than any one application's data, every account
//! reads it, and only a principal that tree's policy admits may rewrite it.
//!
//! The **overlay** is per-user, per-application data, and every other
//! application of that user could previously read *and rewrite* it — a
//! hostile program could file a launcher row named "Terminal" against a
//! bundle of its choosing. It therefore lives in the app-data store
//! (`plans/APPDATA.md` §1.1, AD10), in `applib`'s published scope, so
//! `applib` is the only principal that can write it and the desktop session
//! reads it through the one sanctioned foreign-read shape — which carries no
//! scope field, and so cannot name a private document at all.
//!
//! # The document
//!
//! A catalog is a plain [`tairix_appconf`] `key = value` document — the one
//! format engine the app-data store speaks — so this crate defines the
//! *registry* over it and no grammar, comment rule, or length bound of its
//! own. Every setting is one field of one record: a key of the shape
//! `<id>.<field>`. The `<id>` part is the bundle identifier the entry is
//! keyed by ([`EntryId`]) — a reverse-DNS spelling is deliberately valid,
//! because the key is split at its **last** `.` and no field name contains
//! one. The `<field>` part is drawn from the closed [`EntryKey`] registry.
//!
//! All the settings sharing an id form one [`Record`]: a complete
//! [`LibraryEntry`] where the record names a bundle and a display name, and
//! an [`EntryPatch`] — a user overlay's rename, re-file, re-icon, or
//! visibility verdict on an entry it does not own — where it does not. A
//! declaration may also carry its own `hidden true` suppression, which keeps
//! its identifier claimed (so a discovery rescan cannot resurrect it) while
//! the resolved catalog drops it.
//!
//! # Security
//!
//! A catalog is **untrusted input** to every consumer, including the store a
//! hostile or corrupted installer wrote. It is read **strictly**: [`load`]
//! validates every field through the model's own validators and fails closed
//! ([`CatalogError`]) on anything it does not fully understand — a line the
//! grammar did not read as a setting, an unknown field, a folder outside the
//! taxonomy, an entry whose bundle path is not an application bundle in an
//! application store, or more records than [`MAX_ENTRIES`]. That is the
//! opposite of a *settings* registry, where one bad value costs only its own
//! field, and deliberately so: a catalog is a list, and a half-read one
//! silently drops or mis-files an application a user expects to find. A
//! reader that cannot fully read a store runs on the empty catalog
//! ([`Catalog::default`]) rather than guessing at a partial intent, and a
//! writer refuses the edit outright. The document's own length, line, key
//! and value bounds are the format engine's.
//!
//! The engine performs no I/O and holds no authority: the machine store is
//! read and written through the secured VFS under the caller's own
//! kernel-attested identity, so its per-inode owner/mode/ACL record is what
//! admits or refuses a write, and the overlay is reached only through the
//! app-data service, gated on the writer's attested bundle identity.
//! Launching an entry remains subject to the loader's signature and
//! capability gate; the bundle-path confinement here is the earlier, cheaper
//! refusal.

#![no_std]
#![deny(missing_docs)]

extern crate alloc;

pub mod catalog;
pub mod entry;
pub mod store;

pub use catalog::{merge, Catalog, CatalogFull, EntryPatch, Record, MAX_ENTRIES};
pub use entry::{
    BundlePath, DisplayName, EntryError, EntryId, IconAsset, LibraryEntry, MAX_BUNDLE_PATH_LEN,
    MAX_DISPLAY_NAME_LEN, MAX_ENTRY_ID_LEN, MAX_ICON_ASSET_LEN,
};
pub use store::{document, load, CatalogError, EntryKey, ParseError};
pub use tairix_abi::LibraryCategory;

/// The catalog's own directory name inside the machine's `/System/Settings`
/// tree, spelled once so every path here derives from it.
macro_rules! library_component {
    () => {
        "ProgramLibrary"
    };
}

/// The catalog's directory name inside the `/System/Settings` tree — the one
/// component an image builder or settings browser creates — shared with
/// [`LIBRARY_DIR`] so the spellings cannot drift.
pub const LIBRARY_SETTINGS_SUBDIR: &str = library_component!();

/// The directory that holds the machine-wide catalog, inside the writable
/// `/System/Settings` subtree of the root volume.
pub const LIBRARY_DIR: &str = concat!("/System/Settings/", library_component!());

/// The catalog document's file name.
pub const LIBRARY_FILE: &str = "library.conf";

/// The machine-wide catalog the installer and the library-admin command
/// write and the session reads.
///
/// This layer stays an ordinary `/System/Settings` administrator document,
/// beside the machine's network and configuration stores: it is *machine*
/// policy rather than any one application's data, every account reads it,
/// and only a principal that tree's policy admits may rewrite it. The
/// per-user overlay beside it is what moved into the app-data store
/// ([`LIBRARY_PUBLISHER`]), because that one is the per-user, per-app data
/// every other application of that user could otherwise rewrite.
pub const LIBRARY_PATH: &str = concat!("/System/Settings/", library_component!(), "/library.conf");

/// The signed bundle identifier of the library-admin command — the
/// application that owns the per-user catalog overlay and publishes it.
///
/// The one place the identifier is spelled, because two principals need it
/// and they must agree: `applib` names nothing at all (an application never
/// names its own store — the app-data service derives it from the identity
/// the kernel attests), and a reader hands exactly this to
/// `tairix_appdata::read_published` to obtain the account's overlay. Getting
/// it wrong reaches a store that publishes nothing, never another
/// application's private one, because a foreign read is a request shape with
/// no scope field at all (`plans/APPDATA.md` §3.6).
pub const LIBRARY_PUBLISHER: &str = "os.tairix.applib";

#[cfg(test)]
mod tests {
    extern crate std;

    use alloc::format;

    use super::*;

    #[test]
    fn the_machine_path_is_the_library_file_inside_the_library_directory() {
        assert_eq!(LIBRARY_PATH, format!("{LIBRARY_DIR}/{LIBRARY_FILE}"));
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
    fn the_overlays_publisher_is_a_legal_bundle_identifier() {
        // A reader hands it to a foreign read, which validates it at decode
        // against the one identifier grammar; a spelling outside that grammar
        // would be a request no reader could ever send.
        assert_eq!(
            tairix_abi::appinfo::validate_bundle_id(LIBRARY_PUBLISHER),
            Ok(())
        );
    }
}
