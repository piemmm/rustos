//! Assembling a complete cursor set from on-disk SVG assets.
//!
//! A cursor *set* is one SVG asset per [`CursorKind`] (the SVG-first asset
//! rule). The on-disk assets live under `/System/Graphics`
//! and are untrusted input: reading the bytes needs a
//! filesystem capability and is the userland desktop's job, so this crate
//! takes the bytes through an injected [`CursorAssetSource`] seam — the same
//! pattern the default apps use for their VFS/shell channels — and stays
//! `no_std` with no path of its own to `/System/Graphics`.
//!
//! [`CursorTheme::from_assets`] is **total and fail-closed per kind**: a kind whose asset is absent, malformed, or
//! outside the supported SVG subset keeps its built-in cursor rather than
//! leaving the set without a shape for that kind. A completely empty source
//! therefore yields the built-in set, and a partial set mixes loaded cursors
//! with built-in fallbacks. The result is a plain [`CursorTheme`], so the
//! window manager registers and activates it through the existing
//! [`CursorRegistry`](crate::CursorRegistry) with no compositor change.

use tairix_theme::CursorKind;

use crate::theme::CursorTheme;
use crate::vector::VectorCursor;

/// A source of on-disk SVG cursor assets, one per [`CursorKind`].
///
/// The desktop implements this over the filesystem (reading
/// `/System/Graphics`), tests over an in-memory table. The seam keeps the
/// asset bytes — and the capability needed to read them — out of this
/// `no_std` library.
pub trait CursorAssetSource {
    /// The SVG bytes of the asset for `kind`, or `None` when the set provides
    /// no asset for that kind (so the built-in cursor is used).
    fn asset(&self, kind: CursorKind) -> Option<&[u8]>;
}

impl CursorTheme {
    /// Build a cursor set from a [`CursorAssetSource`], decoding each kind's
    /// SVG asset and falling back to the built-in cursor for any kind whose
    /// asset is missing, malformed, or outside the supported subset.
    ///
    /// Never fails: every kind always resolves to a cursor, so the returned theme is complete even from an empty or
    /// partly-broken source.
    #[must_use]
    pub fn from_assets<S: CursorAssetSource + ?Sized>(source: &S) -> Self {
        let builtin = Self::builtin();
        Self::from_cursors(|kind| resolve(source, kind, &builtin))
    }
}

/// Decode the asset for `kind`, falling back to `builtin`'s cursor for that
/// kind when the source has no asset or the bytes do not decode.
fn resolve<S: CursorAssetSource + ?Sized>(
    source: &S,
    kind: CursorKind,
    builtin: &CursorTheme,
) -> VectorCursor {
    source
        .asset(kind)
        .and_then(|bytes| crate::svg::decode(bytes).ok())
        .unwrap_or_else(|| builtin.cursor(kind).clone())
}
