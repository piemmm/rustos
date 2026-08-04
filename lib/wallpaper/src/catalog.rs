//! The shipped default wallpaper set, and the bounded, fail-closed listing
//! model a chooser draws its thumbnail grid from.
//!
//! The desktop ships five read-only wallpaper masters under
//! [`WALLPAPER_STORE`], discovered at build time from `lib/wallpaper/assets/`
//! by `tools/syshelp` — never a hand-maintained list. [`catalog_entries`] is
//! the one definition of which files in a directory listing a chooser may
//! offer: it performs no I/O of its own — the caller lists the directory —
//! and only filters, validates, and orders what it is given.

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

/// Where the OS ships its desktop wallpaper masters.
pub const WALLPAPER_STORE: &str = "/System/Graphics/Wallpapers";

/// The default wallpaper's file name inside [`WALLPAPER_STORE`].
pub const DEFAULT_WALLPAPER: &str = "tairix-dark.jpg";

/// The default wallpaper's absolute path (`<WALLPAPER_STORE>/<DEFAULT_WALLPAPER>`).
#[must_use]
pub fn default_wallpaper_path() -> String {
    format!("{WALLPAPER_STORE}/{DEFAULT_WALLPAPER}")
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

#[cfg(test)]
#[path = "catalog_tests.rs"]
mod tests;
