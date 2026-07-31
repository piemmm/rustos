//! Loading the program-library catalog the taskbar's popup lists
//! (`plans/NEW-TASKBAR.md` T5).
//!
//! The catalog is two on-disk documents — the machine-wide store under
//! `/System/Settings/ProgramLibrary/` and the logged-in user's overlay in
//! their home — resolved into one view by `tairix_proglib::merge`. Reading
//! them needs a filesystem capability, so it is the desktop session's job:
//! the `no_std` popup model receives the already-merged
//! [`Catalog`] as a typed view and never touches the VFS.
//!
//! Loading is **total and fail-closed per store**: an absent store is the
//! ordinary fresh-installation state (an empty catalog, no complaint), while
//! an unreadable, oversized, non-UTF-8, or malformed store contributes an
//! empty catalog *and* a ready-to-print warning line — the desktop degrades
//! to a calm empty library and says why on `stderr`, rather than guessing at
//! a half-parsed store or dying over a settings file.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use tairix_abi::Errno;
use tairix_proglib::{
    merge, parse, user_library_path, Catalog, MACHINE_LIBRARY_PATH, MAX_CATALOG_LEN,
};

use crate::assets::SessionFileReader;

/// The resolved program library plus any per-store warnings.
///
/// The warnings are complete `stderr` lines (newline-terminated, prefixed
/// with the session's `desktop:` diagnosis convention) so the embedder only
/// has to write them out.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct LoadedLibrary {
    /// The merged machine ∪ overlay catalog the popup lists.
    pub catalog: Catalog,
    /// One line per store that could not be used, ready for `stderr`.
    pub warnings: Vec<String>,
}

/// Load and merge the program-library stores under the session's own
/// authority: the machine-wide store, then the overlay inside `home` (the
/// logged-in user's home directory; `None` when the session has none, which
/// simply means no overlay).
///
/// Never fails: each store that cannot be used is replaced by the empty
/// catalog and explained by a warning line, so the popup always receives a
/// well-formed view.
pub fn load_library<R>(reader: &mut R, home: Option<&str>) -> LoadedLibrary
where
    R: SessionFileReader + ?Sized,
{
    let mut warnings = Vec::new();
    let machine = load_store(reader, MACHINE_LIBRARY_PATH, &mut warnings);
    let overlay = match home.and_then(user_library_path) {
        Some(path) => load_store(reader, &path, &mut warnings),
        None => Catalog::default(),
    };
    LoadedLibrary {
        catalog: merge(&machine, &overlay),
        warnings,
    }
}

/// Read and parse one store, contributing the empty catalog (and a warning
/// where the store exists but cannot be used) on any failure.
fn load_store<R>(reader: &mut R, path: &str, warnings: &mut Vec<String>) -> Catalog
where
    R: SessionFileReader + ?Sized,
{
    let bytes = match reader.read(path) {
        Ok(bytes) => bytes,
        // No store yet: the ordinary state of a fresh installation or an
        // account that has never personalised its library.
        Err(Errno::NotFound) => return Catalog::default(),
        Err(err) => {
            warnings.push(warning(path, &format!("unreadable ({err:?})")));
            return Catalog::default();
        }
    };
    if bytes.len() > MAX_CATALOG_LEN {
        warnings.push(warning(
            path,
            &format!(
                "oversized ({} bytes exceeds the {MAX_CATALOG_LEN}-byte cap)",
                bytes.len()
            ),
        ));
        return Catalog::default();
    }
    let Ok(text) = core::str::from_utf8(&bytes) else {
        warnings.push(warning(path, "not valid UTF-8"));
        return Catalog::default();
    };
    match parse(text) {
        Ok(catalog) => catalog,
        Err(err) => {
            warnings.push(warning(path, &format!("{err}")));
            Catalog::default()
        }
    }
}

/// One ready-to-print warning line for a store that cannot be used.
fn warning(path: &str, detail: &str) -> String {
    format!("desktop: program library {path}: {detail}; using an empty catalog\n")
}
