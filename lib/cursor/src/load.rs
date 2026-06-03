//! Assembling a complete cursor set from on-disk SVG assets.
//!
//! A cursor *set* is one SVG asset per [`CursorKind`] (the SVG-first asset
//! rule, `AGENTS.md` §10). The on-disk assets live under `/System/Graphics`
//! and are untrusted input (`AGENTS.md` §19.5): reading the bytes needs a
//! filesystem capability and is the userland desktop's job, so this crate
//! takes the bytes through an injected [`CursorAssetSource`] seam — the same
//! pattern the default apps use for their VFS/shell channels — and stays
//! `no_std` with no path of its own to `/System/Graphics`.
//!
//! [`CursorTheme::from_assets`] is **total and fail-closed per kind**
//! (`AGENTS.md` §2.9 / §5.4): a kind whose asset is absent, malformed, or
//! outside the supported SVG subset keeps its built-in cursor rather than
//! leaving the set without a shape for that kind. A completely empty source
//! therefore yields the built-in set, and a partial set mixes loaded cursors
//! with built-in fallbacks. The result is a plain [`CursorTheme`], so the
//! window manager registers and activates it through the existing
//! [`CursorRegistry`](crate::CursorRegistry) with no compositor change.

use rustos_theme::CursorKind;

use crate::theme::CursorTheme;
use crate::vector::VectorCursor;

/// Every cursor kind a set provides an asset for.
///
/// A fixed table so a loader iterates the closed [`CursorKind`] vocabulary
/// without inventing a second list of kinds (`AGENTS.md` §2.2 / §2.4).
pub const CURSOR_KINDS: [CursorKind; 5] = [
    CursorKind::Arrow,
    CursorKind::Text,
    CursorKind::Pointer,
    CursorKind::Move,
    CursorKind::Busy,
];

/// A source of on-disk SVG cursor assets, one per [`CursorKind`].
///
/// The desktop implements this over the filesystem (reading
/// `/System/Graphics`), tests over an in-memory table. The seam keeps the
/// asset bytes — and the capability needed to read them — out of this
/// `no_std` library (`AGENTS.md` §17.4).
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
    /// Never fails: every kind always resolves to a cursor (`AGENTS.md`
    /// §2.11), so the returned theme is complete even from an empty or
    /// partly-broken source (`AGENTS.md` §2.9).
    #[must_use]
    pub fn from_assets<S: CursorAssetSource + ?Sized>(source: &S) -> Self {
        let builtin = Self::builtin();
        Self::new(
            resolve(source, CursorKind::Arrow, &builtin),
            resolve(source, CursorKind::Text, &builtin),
            resolve(source, CursorKind::Pointer, &builtin),
            resolve(source, CursorKind::Move, &builtin),
            resolve(source, CursorKind::Busy, &builtin),
        )
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
