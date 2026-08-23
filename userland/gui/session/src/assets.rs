//! Loading the desktop's on-disk SVG graphics assets from `/System/Graphics`.
//!
//! The desktop's cursors and notification icons are authored as SVG under
//! `/System/Graphics` (the SVG-first asset rule).
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

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use tairix_abi::Errno;
use tairix_cursor::{CursorAssetSource, CursorTheme};
use tairix_icon::{icon_vector_path, IconAssetSource, IconKind, IconSet, GRAPHICS_DIR, ICON_KINDS};
use tairix_theme::{CursorKind, CursorSet, CURSOR_KINDS};

/// The desktop session's file-reading seam.
///
/// Reading a file — an SVG asset under [`GRAPHICS_DIR`], a program-library
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

/// The on-disk path of the cursor asset named `asset_id`.
///
/// The asset id comes from the theme's [`CursorSet`]; cursors live in the
/// `Cursors` subdirectory of [`GRAPHICS_DIR`].
fn cursor_path(asset_id: &str) -> String {
    format!("{GRAPHICS_DIR}/Cursors/{asset_id}.svg")
}

/// Build a cursor set from the on-disk SVG assets named by `cursors`.
///
/// Reads one asset per [`CursorKind`] through `reader` and lets `lib/cursor`
/// decode it. A kind whose asset cannot be read, or whose bytes do not decode,
/// keeps the built-in cursor, so this never fails: a missing
/// `/System/Graphics` simply yields the built-in set. The result is a plain
/// [`CursorTheme`] the window manager registers through its existing
/// `CursorRegistry`.
pub fn load_cursor_theme<R>(reader: &mut R, cursors: &CursorSet) -> CursorTheme
where
    R: SessionFileReader + ?Sized,
{
    let mut assets = Vec::new();
    for kind in CURSOR_KINDS {
        if let Ok(bytes) = reader.read(&cursor_path(cursors.asset(kind))) {
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
