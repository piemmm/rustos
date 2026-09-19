//! Loading the desktop's on-disk SVG graphics assets from `/System/Graphics`.
//!
//! The desktop's cursors and notification icons are authored as SVG under
//! `/System/Graphics` — cursors in one directory per *set*, icons in one
//! flat directory (the SVG-first asset rule).
//! `lib/cursor` and `lib/icon` own the decode-and-fall-back logic but stay
//! `no_std` and hold no path of their own: they take the asset bytes through
//! the [`CursorAssetSource`] / [`IconAssetSource`] seams. Reading those bytes
//! needs a filesystem capability, so it is the desktop session's job. This module is that job.
//!
//! A caller supplies a [`SessionFileReader`] — VFS-backed on a running
//! system, an in-memory table in tests — and [`load_cursor_theme`] /
//! [`load_icon_set`] read one asset per kind, decode it, and assemble a
//! complete [`CursorTheme`] / [`IconSet`]. Both are **total and fail-closed
//! per kind**: a kind whose asset is absent, unreadable,
//! malformed, or outside the supported SVG subset keeps its built-in artwork,
//! so a missing or corrupt `/System/Graphics` can never blank the pointer or a
//! status icon — it simply yields the built-in set.

use alloc::vec::Vec;

use tairix_abi::Errno;
use tairix_cursor::{cursor_asset_path, CursorAssetSource, CursorTheme};
use tairix_icon::{icon_vector_path, IconAssetSource, IconKind, IconSet, ICON_KINDS};
use tairix_theme::{CursorKind, CursorSet, CursorSetId, CURSOR_KINDS};

/// The desktop session's file-reading seam.
///
/// Reading a file — an SVG asset under `/System/Graphics`, a program-library
/// store (the [`library`](crate::library) loader) — needs a filesystem
/// capability, so it is the desktop session's job rather than a `no_std`
/// library crate's. On a running system this is backed by the VFS under the
/// session's own kernel-attested identity; tests back it with an in-memory
/// table. There is one seam, not one per consumer, so every session read
/// shares a single production implementation.
pub trait SessionFileReader {
    /// Read the bytes of the file at absolute `path`.
    ///
    /// # Errors
    ///
    /// Returns the kernel boundary's [`Errno`] when the file cannot be read —
    /// for example [`Errno::NotFound`] when it is absent or
    /// [`Errno::PermissionDenied`] when the caller lacks the capability to read
    /// it. A read failure is never fatal to the desktop: each loader falls
    /// back per file (built-in artwork, an empty catalog) and reports.
    fn read(&mut self, path: &str) -> Result<Vec<u8>, Errno>;
}

/// The cursor SVG bytes read from disk, one optional blob per [`CursorKind`],
/// exposed to `lib/cursor`'s decoder through [`CursorAssetSource`].
///
/// A kind absent here was unreadable, so the decoder uses its built-in cursor.
struct LoadedCursorAssets {
    assets: Vec<(CursorKind, Vec<u8>)>,
}

impl CursorAssetSource for LoadedCursorAssets {
    fn asset(&self, kind: CursorKind) -> Option<&[u8]> {
        self.assets
            .iter()
            .find(|(k, _)| *k == kind)
            .map(|(_, bytes)| bytes.as_slice())
    }
}

/// The icon SVG bytes read from disk, one optional blob per [`IconKind`],
/// exposed to `lib/icon`'s decoder through [`IconAssetSource`].
struct LoadedIconAssets {
    assets: Vec<(IconKind, Vec<u8>)>,
}

impl IconAssetSource for LoadedIconAssets {
    fn asset(&self, kind: IconKind) -> Option<&[u8]> {
        self.assets
            .iter()
            .find(|(k, _)| *k == kind)
            .map(|(_, bytes)| bytes.as_slice())
    }
}

/// Build the cursor set `set` from the on-disk SVG assets in its own
/// directory, under the asset names `cursors` gives each kind.
///
/// Reads one asset per [`CursorKind`] through `reader` and lets `lib/cursor`
/// decode it. A kind whose asset cannot be read, or whose bytes do not
/// decode, keeps the built-in cursor, so this never fails: a set directory
/// that is missing or unreadable simply yields the built-in artwork under
/// that set's name. A theme naming an asset the store could not hold is
/// refused at the path itself and keeps its built-in cursor too. The result is a plain [`CursorTheme`] the window manager
/// registers through its existing `CursorRegistry`.
pub fn load_cursor_theme<R>(reader: &mut R, set: CursorSetId, cursors: &CursorSet) -> CursorTheme
where
    R: SessionFileReader + ?Sized,
{
    let mut assets = Vec::new();
    for kind in CURSOR_KINDS {
        let Some(path) = cursor_asset_path(set, cursors.asset(kind)) else {
            continue;
        };
        if let Ok(bytes) = reader.read(&path) {
            assets.push((kind, bytes));
        }
    }
    CursorTheme::from_assets(&LoadedCursorAssets { assets })
}

/// Build a notification-icon set from the on-disk SVG assets under
/// `/System/Graphics/Icons`.
///
/// Reads one asset per [`IconKind`] (named by [`IconKind::asset_id`]) through
/// `reader` and lets `lib/icon` decode it. A kind whose asset cannot be read,
/// or whose bytes do not decode, falls back to its built-in glyph at draw time, so this never fails. The result is an [`IconSet`] the
/// taskbar installs through `TaskbarRenderer::set_icons`.
pub fn load_icon_set<R>(reader: &mut R) -> IconSet
where
    R: SessionFileReader + ?Sized,
{
    let mut assets = Vec::new();
    for kind in ICON_KINDS {
        if let Ok(bytes) = reader.read(&icon_vector_path(kind)) {
            assets.push((kind, bytes));
        }
    }
    IconSet::from_assets(&LoadedIconAssets { assets })
}
