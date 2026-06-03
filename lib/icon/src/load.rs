//! Assembling a desktop icon set from on-disk SVG assets.
//!
//! An icon *set* is one SVG asset per [`IconKind`] (the SVG-first asset rule,
//! `AGENTS.md` §10). The on-disk assets live under `/System/Graphics` and are
//! untrusted input (`AGENTS.md` §19.5): reading the bytes needs a filesystem
//! capability and is the userland desktop's job, so this crate takes the bytes
//! through an injected [`IconAssetSource`] seam — the same pattern the default
//! apps use for their VFS/shell channels — and stays `no_std` with no path of
//! its own to `/System/Graphics`.
//!
//! [`IconSet`] decodes the asset for each kind once and remembers it. A kind
//! whose asset is absent, malformed, or outside the supported SVG subset has
//! no stored icon and falls back to the [`builtin_icon`] glyph at draw time
//! (`AGENTS.md` §2.9 / §5.4) — so the set is **total**:
//! every kind always produces a glyph. An SVG asset carries its own per-layer
//! colours, so [`IconSet::icon`] tints only the built-in fallback, never an
//! authored asset.

use rustos_raster::Color;

use crate::glyph::{builtin_icon, IconKind};
use crate::vector::VectorIcon;

/// Every icon kind a set can provide an asset for.
///
/// A fixed table so a loader iterates the closed [`IconKind`] vocabulary
/// without inventing a second list of kinds (`AGENTS.md` §2.2 / §2.4).
pub const ICON_KINDS: [IconKind; 5] = [
    IconKind::Network,
    IconKind::Volume,
    IconKind::Battery,
    IconKind::Bell,
    IconKind::Generic,
];

/// A source of on-disk SVG icon assets, one per [`IconKind`].
///
/// The desktop implements this over the filesystem (reading
/// `/System/Graphics`), tests over an in-memory table. The seam keeps the
/// asset bytes — and the capability needed to read them — out of this
/// `no_std` library (`AGENTS.md` §17.4).
pub trait IconAssetSource {
    /// The SVG bytes of the asset for `kind`, or `None` when the set provides
    /// no asset for that kind (so the built-in glyph is used).
    fn asset(&self, kind: IconKind) -> Option<&[u8]>;
}

/// A resolved icon set: the decoded SVG glyph for each [`IconKind`] that the
/// source supplied, with a built-in fallback for the rest.
///
/// Stored as fixed fields rather than a map so every kind always resolves and
/// [`icon`](Self::icon) is total (`AGENTS.md` §2.11).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IconSet {
    network: Option<VectorIcon>,
    volume: Option<VectorIcon>,
    battery: Option<VectorIcon>,
    bell: Option<VectorIcon>,
    generic: Option<VectorIcon>,
}

impl IconSet {
    /// The all-built-in set: every kind falls back to its [`builtin_icon`]
    /// glyph, so the desktop has a complete icon set before any on-disk asset
    /// is loaded (`AGENTS.md` §2.9). Swapping a loaded set in later is
    /// [`from_assets`](Self::from_assets).
    #[must_use]
    pub const fn builtin() -> Self {
        Self {
            network: None,
            volume: None,
            battery: None,
            bell: None,
            generic: None,
        }
    }

    /// Build an icon set from an [`IconAssetSource`], decoding each kind's SVG
    /// asset once. A kind whose asset is missing, malformed, or outside the
    /// supported subset is left unset and falls back to its built-in glyph in
    /// [`icon`](Self::icon) (`AGENTS.md` §2.9).
    #[must_use]
    pub fn from_assets<S: IconAssetSource + ?Sized>(source: &S) -> Self {
        Self {
            network: decoded(source, IconKind::Network),
            volume: decoded(source, IconKind::Volume),
            battery: decoded(source, IconKind::Battery),
            bell: decoded(source, IconKind::Bell),
            generic: decoded(source, IconKind::Generic),
        }
    }

    /// The icon for `kind`: the loaded SVG asset if the set supplied one, else
    /// the built-in glyph tinted with `tint`.
    ///
    /// Total — every kind always produces a glyph. The `tint` colours only a
    /// built-in fallback; an authored SVG asset keeps its own colours
    /// (`AGENTS.md` §10).
    #[must_use]
    pub fn icon(&self, kind: IconKind, tint: Color) -> VectorIcon {
        self.loaded(kind)
            .cloned()
            .unwrap_or_else(|| builtin_icon(kind, tint))
    }

    /// Whether `kind` resolved to an authored SVG asset (rather than the
    /// built-in fallback).
    #[must_use]
    pub fn is_loaded(&self, kind: IconKind) -> bool {
        self.loaded(kind).is_some()
    }

    /// The decoded SVG icon stored for `kind`, if any.
    fn loaded(&self, kind: IconKind) -> Option<&VectorIcon> {
        match kind {
            IconKind::Network => self.network.as_ref(),
            IconKind::Volume => self.volume.as_ref(),
            IconKind::Battery => self.battery.as_ref(),
            IconKind::Bell => self.bell.as_ref(),
            IconKind::Generic => self.generic.as_ref(),
        }
    }
}

impl Default for IconSet {
    /// The all-built-in set (see [`builtin`](Self::builtin)).
    fn default() -> Self {
        Self::builtin()
    }
}

/// Decode the asset for `kind`, or `None` when the source has no asset for it
/// or the bytes do not decode (so the built-in glyph is used).
fn decoded<S: IconAssetSource + ?Sized>(source: &S, kind: IconKind) -> Option<VectorIcon> {
    source
        .asset(kind)
        .and_then(|bytes| crate::svg::decode(bytes).ok())
}
