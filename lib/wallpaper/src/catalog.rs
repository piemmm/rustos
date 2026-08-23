//! The shipped default wallpaper set, and the bounded, fail-closed listing
//! model a chooser draws its category rail and thumbnail grid from.
//!
//! The desktop ships its read-only wallpaper masters under
//! [`WALLPAPER_STORE`], filed one directory level deep in **categories**
//! (`Space`, `Nature`, `City`, `Abstract`, `TAIRiX`), all discovered at build
//! time from `lib/wallpaper/assets/` by `tools/syshelp` — never a
//! hand-maintained list. [`catalog_categories`] and [`catalog_entries`] are
//! the one definition of which directories and which files in a listing a
//! chooser may offer: neither performs I/O of its own — the caller lists the
//! directory — and both only filter, validate, and order what they are
//! given.
//!
//! **A category's directory name is its display label, verbatim.** There is
//! no title-casing rule and no name → label table, so adding a category is
//! authoring a directory whose name is exactly what the user reads, and no
//! second spelling of it can drift out of step.

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

/// Where the OS ships its desktop wallpaper masters. Its immediate children
/// are the category directories; the masters themselves are one level below.
pub const WALLPAPER_STORE: &str = "/System/Graphics/Wallpapers";

/// The category directory the default wallpaper is filed under.
pub const DEFAULT_WALLPAPER_CATEGORY: &str = "TAIRiX";

/// The default wallpaper's file name inside [`DEFAULT_WALLPAPER_CATEGORY`].
pub const DEFAULT_WALLPAPER: &str = "tairix-dark.jpg";

/// One category directory's absolute path
/// (`<WALLPAPER_STORE>/<category>`).
#[must_use]
pub fn category_path(category: &str) -> String {
    format!("{WALLPAPER_STORE}/{category}")
}

/// One shipped master's absolute path
/// (`<WALLPAPER_STORE>/<category>/<file>`).
#[must_use]
pub fn wallpaper_path(category: &str, file: &str) -> String {
    format!("{WALLPAPER_STORE}/{category}/{file}")
}

/// The default wallpaper's absolute path.
#[must_use]
pub fn default_wallpaper_path() -> String {
    wallpaper_path(DEFAULT_WALLPAPER_CATEGORY, DEFAULT_WALLPAPER)
}

/// Largest wallpaper file any consumer will read, in bytes.
///
/// A fixed validation bound on untrusted input, not a growable capacity:
/// 8 MiB admits every shipped photographic master with headroom while still
/// bounding how much hostile work a single file can demand before any byte
/// is decoded. That relationship is pinned against the real assets rather
/// than a copied figure — `the_byte_bound_admits_the_largest_shipped_master_with_headroom`
/// measures the crate's own `assets/` directory — and `tools/syshelp`'s
/// build-time discovery refuses to plant a master over this bound, so an
/// asset the desktop could not read fails the build rather than the
/// desktop.
pub const MAX_WALLPAPER_BYTES: usize = 8 * 1024 * 1024;

/// Largest number of wallpapers a catalog listing may return.
///
/// A chooser's thumbnail grid is a bounded surface, so this is a fixed
/// security and format bound, not a growable capacity: a directory holding
/// more candidates than this yields only the first [`MAX_WALLPAPER_CATALOG_ENTRIES`]
/// in name order, rather than growing the listing without bound.
pub const MAX_WALLPAPER_CATALOG_ENTRIES: usize = 256;

/// Largest number of categories a catalog listing may return.
///
/// A chooser's category rail is a bounded surface exactly as its grid is, so
/// this is a fixed bound too: a store holding more category directories than
/// this yields only the first [`MAX_WALLPAPER_CATEGORIES`] in name order.
pub const MAX_WALLPAPER_CATEGORIES: usize = 64;

/// The file-name extensions [`catalog_entries`] accepts, checked
/// case-insensitively against the whole name's suffix.
const WALLPAPER_EXTENSIONS: [&str; 3] = [".jpg", ".jpeg", ".png"];

/// Whether `name`'s extension is one [`catalog_entries`] accepts.
fn has_wallpaper_extension(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    WALLPAPER_EXTENSIONS
        .iter()
        .any(|extension| lower.ends_with(extension))
}

/// Whether `name` is a legal wallpaper file name: a plain leaf name (no
/// control character, no path separator, non-empty, not `.`/`..`) with one
/// of the decodable extensions (`.jpg`, `.jpeg`, `.png`, checked
/// case-insensitively).
///
/// [`catalog_entries`] applies this at runtime to silently drop anything
/// that fails it; `tools/syshelp`'s build-time discovery applies the same
/// definition to fail the image build closed on a shipped asset that would
/// never be offered.
#[must_use]
pub fn is_wallpaper_file_name(name: &str) -> bool {
    tairix_path::validate_file_name(name).is_ok() && has_wallpaper_extension(name)
}

/// Whether `name` is a legal wallpaper category directory name: a plain leaf
/// name (no control character, no path separator, non-empty, not `.`/`..`).
///
/// The name carries no extension and no case convention, because it is the
/// label the chooser draws: `TAIRiX` reads as `TAIRiX`. [`catalog_categories`]
/// applies this at runtime to silently drop anything that fails it;
/// `tools/syshelp`'s build-time discovery applies the same definition to fail
/// the image build closed on a category directory no chooser could offer.
#[must_use]
pub fn is_wallpaper_category_name(name: &str) -> bool {
    tairix_path::validate_file_name(name).is_ok()
}

/// One wallpaper a consumer may offer, discovered from a directory listing.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CatalogEntry {
    /// The file's plain name inside the listed directory — never a path.
    pub name: String,
    /// The file's size in bytes, as the caller's directory listing
    /// reported it.
    pub bytes: usize,
}

/// Build the wallpaper catalog a chooser may offer, from a directory
/// listing.
///
/// `entries` is the caller's own directory listing — `(name, byte length)`
/// pairs for one directory, ordinarily [`WALLPAPER_STORE`] or a directory
/// the user picked through the trusted file picker. This performs **no**
/// I/O of its own: every entry is filtered, validated, and ordered from
/// exactly what the caller supplies.
///
/// An entry is admitted only when its name is a legal plain file name (no
/// control character, no path separator, non-empty, not `.`/`..` —
/// [`tairix_path::validate_file_name`]), its extension is one of `.jpg`,
/// `.jpeg`, or `.png` (checked case-insensitively), and its size is at most
/// [`MAX_WALLPAPER_BYTES`]. Anything else is silently dropped: a directory
/// mixing wallpapers with unrelated files yields only the wallpapers, never
/// a refusal of the whole listing. The result is sorted deterministically
/// by name and capped at [`MAX_WALLPAPER_CATALOG_ENTRIES`].
#[must_use]
pub fn catalog_entries<'a, I>(entries: I) -> Vec<CatalogEntry>
where
    I: IntoIterator<Item = (&'a str, usize)>,
{
    let mut out: Vec<CatalogEntry> = entries
        .into_iter()
        .filter(|(name, bytes)| *bytes <= MAX_WALLPAPER_BYTES && is_wallpaper_file_name(name))
        .map(|(name, bytes)| CatalogEntry {
            name: name.to_string(),
            bytes,
        })
        .collect();
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out.truncate(MAX_WALLPAPER_CATALOG_ENTRIES);
    out
}

/// Build the category list a chooser may offer, from the directory names
/// found directly inside [`WALLPAPER_STORE`].
///
/// `entries` is the caller's own listing of that store's *subdirectories* —
/// deciding which listed entries are directories is the caller's I/O, not
/// this function's. A name that is not a legal plain leaf name is silently
/// dropped, so a store holding a stray file alongside its categories yields
/// only the categories rather than a refusal of the whole listing. The
/// result is sorted deterministically by name, exactly as
/// [`catalog_entries`] is, and capped at [`MAX_WALLPAPER_CATEGORIES`].
///
/// Each returned name is both the directory to list and the label to draw,
/// so a chooser needs no second vocabulary for the categories it offers.
#[must_use]
pub fn catalog_categories<'a, I>(entries: I) -> Vec<String>
where
    I: IntoIterator<Item = &'a str>,
{
    let mut out: Vec<String> = entries
        .into_iter()
        .filter(|name| is_wallpaper_category_name(name))
        .map(ToString::to_string)
        .collect();
    out.sort();
    out.truncate(MAX_WALLPAPER_CATEGORIES);
    out
}

#[cfg(test)]
#[path = "catalog_tests.rs"]
mod tests;
