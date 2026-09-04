//! Loading the program-library catalog the taskbar's popup lists
//! (`plans/NEW-TASKBAR.md` T5).
//!
//! The catalog is two layers resolved into one view by `tairix_proglib::merge`:
//!
//! - the **machine-wide** store under `/System/Settings/ProgramLibrary/`,
//!   read as a file. It is machine policy rather than any one application's
//!   data — every account reads it, and only a principal that tree's policy
//!   admits may rewrite it — so it stays an ordinary administrator document
//!   beside the machine's network and configuration stores. Reading it needs
//!   a filesystem capability, which is why it is the desktop session's job:
//!   the `no_std` popup model receives the already-merged [`Catalog`] as a
//!   typed view and never touches the VFS.
//! - the logged-in user's **overlay**, read from the library-admin command's
//!   *published* app-data scope (`plans/APPDATA.md` §3.11). That layer is
//!   per-user, per-application data, and every other application of the
//!   account could previously read *and rewrite* it — a hostile program could
//!   file a launcher row named "Terminal" against a bundle of its choosing.
//!   The session now reads it by naming the publisher on a request shape that
//!   carries no scope field, so it can obtain what `applib` publishes about
//!   the account's library and nothing else `applib` keeps.
//!
//! Loading is **total and fail-closed per layer**: an absent layer is the
//! ordinary fresh-installation state (an empty catalog, no complaint), while
//! one that is unreadable, oversized, non-UTF-8, or malformed contributes an
//! empty catalog *and* a ready-to-print warning line — the desktop degrades
//! to a calm empty library and says why on `stderr`, rather than guessing at
//! a half-read store or dying over a settings file.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use tairix_abi::Errno;
use tairix_appconf::Document;
use tairix_appdata::{read_published, AppDataHost};
use tairix_browse::open_with::{association_from_appinfo, AppAssociation};
use tairix_proglib::{
    load, merge, Catalog, EntryId, LibraryEntry, LIBRARY_PATH, LIBRARY_PUBLISHER,
};

use crate::assets::SessionFileReader;

/// The resolved program library plus any per-layer warnings.
///
/// The warnings are complete `stderr` lines (newline-terminated, prefixed
/// with the session's `desktop:` diagnosis convention) so the embedder only
/// has to write them out.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct LoadedLibrary {
    /// The merged machine ∪ overlay catalog the popup lists.
    pub catalog: Catalog,
    /// One line per layer that could not be used, ready for `stderr`.
    pub warnings: Vec<String>,
}

/// Load and merge the program-library layers: the machine-wide store, read
/// under the session's own authority, then the account's overlay, read from
/// what the library-admin command publishes.
///
/// Never fails: each layer that cannot be used is replaced by the empty
/// catalog and explained by a warning line, so the popup always receives a
/// well-formed view.
pub fn load_library<R>(reader: &mut R, host: &mut dyn AppDataHost) -> LoadedLibrary
where
    R: SessionFileReader + ?Sized,
{
    let mut warnings = Vec::new();
    let machine = load_machine_store(reader, &mut warnings);
    let overlay = load_overlay(host, &mut warnings);
    LoadedLibrary {
        catalog: merge(&machine, &overlay),
        warnings,
    }
}

/// The resolved program library and the file associations its bundles declare.
///
/// One snapshot rather than two, because the associations are derived from the
/// catalog: computing them separately would let a click resolve a bundle
/// against a catalog it was not read from.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct LoadedPrograms {
    /// The merged machine ∪ overlay catalog the popup lists.
    pub catalog: Catalog,
    /// Which installed application opens which file, from the `AppInfo` of
    /// every bundle the catalog names.
    pub associations: Vec<AppAssociation>,
    /// One line per layer or manifest that could not be used, ready for
    /// `stderr`.
    pub warnings: Vec<String>,
}

/// Load the program library and read one `AppInfo` per catalogued bundle for
/// its declared associations.
///
/// This is the program-catalog worker's whole body: two documents and then a
/// manifest per application, which on a machine with a full program store is
/// far more than a frame's worth of reads. It therefore never runs on the serve
/// loop — the popup opens on the catalog already in hand and adopts this the
/// moment it lands.
///
/// Fail-closed per bundle, like the layers above it: a manifest that cannot be
/// read or does not parse simply contributes no association, so one broken
/// bundle costs only its own file types.
pub fn load_programs<R>(reader: &mut R, host: &mut dyn AppDataHost) -> LoadedPrograms
where
    R: SessionFileReader + ?Sized,
{
    let loaded = load_library(reader, host);
    let associations = loaded
        .catalog
        .entries()
        .filter_map(|entry| {
            let bundle = entry.bundle().as_str();
            let manifest = format!("{bundle}/AppInfo");
            let bytes = reader.read(&manifest).ok()?;
            association_from_appinfo(bundle, &bytes)
        })
        .collect();
    LoadedPrograms {
        catalog: loaded.catalog,
        associations,
        warnings: loaded.warnings,
    }
}

/// Read and read-in the machine-wide store, contributing the empty catalog
/// (and a warning where the store exists but cannot be used) on any failure.
fn load_machine_store<R>(reader: &mut R, warnings: &mut Vec<String>) -> Catalog
where
    R: SessionFileReader + ?Sized,
{
    let bytes = match reader.read(LIBRARY_PATH) {
        Ok(bytes) => bytes,
        // No store yet: the ordinary state of a fresh installation.
        Err(Errno::NotFound) => return Catalog::default(),
        Err(err) => {
            warnings.push(machine_warning(&format!("unreadable ({err:?})")));
            return Catalog::default();
        }
    };
    let Ok(text) = core::str::from_utf8(&bytes) else {
        warnings.push(machine_warning("not valid UTF-8"));
        return Catalog::default();
    };
    // The document's length, line, key and value bounds are the format
    // engine's, so an oversized store is refused there rather than by a
    // second ceiling here that could disagree with it.
    let document = match Document::parse(text) {
        Ok(document) => document,
        Err(err) => {
            warnings.push(machine_warning(&format!("{err}")));
            return Catalog::default();
        }
    };
    match load(&document) {
        Ok(catalog) => catalog,
        Err(err) => {
            warnings.push(machine_warning(&format!("{err}")));
            Catalog::default()
        }
    }
}

/// Read the account's overlay from what the library-admin command publishes.
///
/// An application that publishes nothing, one that has never run for the
/// account, and one whose store cannot be attested all answer the same empty
/// document — that indistinguishability is the published scope's own rule, so
/// a reader learns what an application chose to publish and nothing else.
/// Only the *caller's* own refusals come back as themselves, and those are
/// worth a line.
fn load_overlay(host: &mut dyn AppDataHost, warnings: &mut Vec<String>) -> Catalog {
    let document = match read_published(host, LIBRARY_PUBLISHER) {
        Ok(document) => document,
        Err(err) => {
            warnings.push(overlay_warning(&format!("unreadable ({err:?})")));
            return Catalog::default();
        }
    };
    match load(&document) {
        Ok(catalog) => catalog,
        Err(err) => {
            warnings.push(overlay_warning(&format!("{err}")));
            Catalog::default()
        }
    }
}

/// Resolve `entry` against `catalog`, or the ready-to-print reason it no
/// longer names a program.
///
/// Every act on a popup row — launching the bundle, and putting a shortcut to
/// it on the desktop — resolves the same identifier through this one lookup,
/// so a row can never act on two different bundles, and a catalog that changed
/// under the click is refused once, in one wording. The popup only reports
/// entries from the catalog it was handed, so a miss means exactly that: the
/// caller refuses loudly rather than acting on a guessed path.
///
/// # Errors
///
/// The complete `stderr` line (newline-terminated, prefixed with the session's
/// `desktop:` diagnosis convention) naming the identifier that is no longer
/// catalogued.
pub fn catalogued<'a>(catalog: &'a Catalog, entry: &EntryId) -> Result<&'a LibraryEntry, String> {
    catalog
        .entry(entry)
        .ok_or_else(|| format!("desktop: library entry {entry} is no longer catalogued\n"))
}

/// One ready-to-print warning line for a machine store that cannot be used.
fn machine_warning(detail: &str) -> String {
    format!("desktop: program library {LIBRARY_PATH}: {detail}; using an empty catalog\n")
}

/// One ready-to-print warning line for an overlay that cannot be used.
fn overlay_warning(detail: &str) -> String {
    format!(
        "desktop: program library overlay published by {LIBRARY_PUBLISHER}: \
         {detail}; using an empty catalog\n"
    )
}
