//! The shipped cursor-set store, and the bounded, fail-closed listing model
//! a chooser draws its choice space from.
//!
//! The desktop ships its read-only cursor artwork under [`CURSOR_STORE`],
//! filed one directory level deep in **sets** whose own names are the labels
//! a chooser offers, all discovered at build time from `lib/cursor/assets/`
//! by `tools/syshelp` — never a hand-maintained list. [`catalog_sets`] is the
//! one definition of which directories a chooser may offer: it performs no
//! I/O — the caller lists the directory — and only filters, validates, and
//! orders what it is given, exactly as the wallpaper catalog does.
//!
//! One asset per [`CursorKind`] lives inside a set, named by the asset id
//! the active theme asks for ([`CursorSet::asset`]); the shipped sets are
//! authored against [`CursorSet::canonical`].
//!
//! [`CursorSet`]: tairix_theme::CursorSet
//! [`CursorSet::asset`]: tairix_theme::CursorSet::asset
//! [`CursorSet::canonical`]: tairix_theme::CursorSet::canonical

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use tairix_abi::desktop::CURSOR_SETS_MAX;
use tairix_theme::{CursorKind, CursorSetId, CURSOR_KINDS};

/// Where the OS ships its cursor sets. Its immediate children are the set
/// directories; the assets themselves are one level below.
pub const CURSOR_STORE: &str = "/System/Graphics/Cursors";

/// The set directory the shipped high-visibility artwork is filed under.
///
/// Named here so the image build can hold the store to actually carrying it
/// — a desktop whose only cursor set is the built-in one offers a choice of
/// one, which is the control that changes nothing the charter forbids.
pub const SHIPPED_CURSOR_SET: &str = "High Visibility";

/// The extension every cursor asset carries: cursors are authored as SVG.
const CURSOR_ASSET_SUFFIX: &str = ".svg";

/// Largest cursor asset the desktop will ever read, in bytes.
///
/// A fixed validation bound on untrusted input, not a growable capacity: a
/// cursor is a handful of small filled outlines, so the ceiling exists
/// purely to bound how much hostile work one asset can demand before any
/// byte is decoded. `tools/syshelp`'s build-time discovery refuses to plant
/// an asset over this bound, so artwork the desktop could not read fails
/// the build rather than the desktop.
pub const MAX_CURSOR_ASSET_BYTES: usize = 64 * 1024;

/// The logical pixel side a cursor is drawn at, before the desktop's UI
/// scale and the user's chosen pointer size are applied.
///
/// Logical, so the one logical-to-physical conversion
/// (`tairix_geometry::Scale::scale_length`) turns it into pixels and this
/// crate carries no density arithmetic of its own.
pub const CURSOR_BASE_SIDE_PX: u32 = 32;

/// One set directory's absolute path (`<CURSOR_STORE>/<set>`).
#[must_use]
pub fn set_path(set: CursorSetId) -> String {
    format!("{CURSOR_STORE}/{set}")
}

/// One asset's absolute path (`<CURSOR_STORE>/<set>/<asset_id>.svg`), or
/// `None` when `asset_id` is not a name the store could hold.
///
/// `asset_id` is what the active theme names the kind, so a theme carrying
/// artwork of its own resolves inside whichever set the user chose. It is
/// therefore *theme* data reaching a path, and a theme naming anything but
/// a plain leaf name would widen that path out of the store — so it is
/// validated here, at the splice, and refused rather than trimmed. The
/// caller then keeps that kind's built-in cursor, exactly as it does for an
/// asset that is absent.
#[must_use]
pub fn cursor_asset_path(set: CursorSetId, asset_id: &str) -> Option<String> {
    let file = format!("{asset_id}{CURSOR_ASSET_SUFFIX}");
    tairix_path::validate_file_name(&file)
        .is_ok()
        .then(|| format!("{}/{file}", set_path(set)))
}

/// Whether `name` is a legal cursor-set directory name.
///
/// The name carries no extension and no case convention, because it is the
/// label a chooser draws. [`catalog_sets`] applies this at runtime to
/// silently drop anything that fails it; `tools/syshelp`'s build-time
/// discovery applies the same definition to fail the image build closed on
/// a set directory no chooser could offer.
#[must_use]
pub fn is_cursor_set_name(name: &str) -> bool {
    CursorSetId::new(name).is_some()
}

/// The [`CursorKind`] a shipped asset file name provides artwork for, or
/// `None` when the name is not one any set may carry.
///
/// The identity check is exact — a name that strips to a stem no kind's own
/// [`asset_id`](CursorKind::asset_id) spells (an unknown id, a wrong
/// extension, an empty name, a path with directory separators) is rejected
/// — so the image build never accepts a file the loader would not later ask
/// for.
#[must_use]
pub fn cursor_asset_kind_for_file(name: &str) -> Option<CursorKind> {
    let stem = name.strip_suffix(CURSOR_ASSET_SUFFIX)?;
    CURSOR_KINDS
        .into_iter()
        .find(|kind| kind.asset_id() == stem)
}

/// Build the cursor-set choice space from a listing of [`CURSOR_STORE`]'s
/// subdirectories.
///
/// `names` is the caller's own listing of that store's *directories* —
/// deciding which listed entries are directories is the caller's I/O, not
/// this function's. A name no set may carry is silently dropped, so a store
/// holding a stray file alongside its sets yields only the sets rather than
/// a refusal of the whole listing. The result is sorted deterministically by
/// name.
///
/// The built-in set is **not** in the result, and a directory claiming its
/// name is dropped: the built-in is always present and is offered beside
/// whatever this listed, so a store could otherwise shadow it with artwork
/// under the same label. That is also why the cap is one below
/// [`CURSOR_SETS_MAX`] — the built-in occupies the slot a reply frame
/// reserves for it.
#[must_use]
pub fn catalog_sets<'a, I>(names: I) -> Vec<CursorSetId>
where
    I: IntoIterator<Item = &'a str>,
{
    let mut out: Vec<CursorSetId> = names
        .into_iter()
        .filter(|name| *name != CursorSetId::BUILTIN_NAME)
        .filter_map(CursorSetId::new)
        .collect();
    out.sort();
    out.dedup();
    out.truncate(CURSOR_SETS_MAX.saturating_sub(1));
    out
}

#[cfg(test)]
#[path = "store_tests.rs"]
mod tests;
