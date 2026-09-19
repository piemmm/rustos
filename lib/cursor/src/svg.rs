//! Building a [`VectorCursor`] from a decoded SVG asset.
//!
//! Cursors are authored as SVG (the SVG-first asset rule). A decoded
//! [`SvgImage`] is a square design grid plus the shared artwork tree —
//! exactly a cursor's own — and it carries the optional pointer hotspot
//! (`data-hotspot-x`/`data-hotspot-y`). The conversion is a direct field map,
//! so the cursor still rasterises through `lib/raster`'s single scan
//! converter. An asset without a declared hotspot pins it to the design-grid
//! origin.

use tairix_svg::{SvgError, SvgImage};

use crate::vector::VectorCursor;

impl VectorCursor {
    /// Build a cursor from a decoded [`SvgImage`], preserving its design
    /// grid, its artwork, and its pointer hotspot.
    ///
    /// An asset that declares no hotspot pins it to the design-grid origin
    /// `(0, 0)`.
    #[must_use]
    pub fn from_svg(image: &SvgImage) -> Self {
        let (hotspot_x, hotspot_y) = image.hotspot().unwrap_or((0, 0));
        Self::from_artwork(image.design(), hotspot_x, hotspot_y, image.nodes().to_vec())
    }
}

/// Decode an SVG byte string into a [`VectorCursor`].
///
/// This is the desktop's cursor-asset entry point for the SVG-first pipeline.
/// SVG is untrusted input: the decode is total and a
/// malformed or out-of-subset asset returns [`SvgError`] so the caller falls
/// back to a built-in cursor rather than crashing the compositor.
///
/// # Errors
/// Propagates the [`SvgError`] from [`tairix_svg::decode`].
pub fn decode(bytes: &[u8]) -> Result<VectorCursor, SvgError> {
    Ok(VectorCursor::from_svg(&tairix_svg::decode(
        bytes,
        tairix_svg::Viewport::Square,
    )?))
}
