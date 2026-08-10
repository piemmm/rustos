//! Building a [`VectorIcon`] from a decoded SVG asset.
//!
//! Desktop icons are authored as SVG (the SVG-first asset rule). A decoded
//! [`SvgImage`] is already a square design grid plus an ordered stack of
//! filled layers — exactly a [`VectorIcon`] — so the conversion is a direct
//! field map and the glyph still rasterises through `lib/raster`'s single
//! scan converter. Unlike the built-in glyphs, an SVG icon carries its own
//! per-layer paints, so it is not tinted by the caller.

use tairix_svg::{SvgError, SvgImage};

use crate::vector::{IconLayer, VectorIcon};

impl VectorIcon {
    /// Build an icon from a decoded [`SvgImage`], preserving its design grid,
    /// per-layer paints and fill rules, and bottom-first layer order.
    #[must_use]
    pub fn from_svg(image: &SvgImage) -> Self {
        let layers = image
            .layers()
            .iter()
            .map(|layer| IconLayer::filled(layer.paint.clone(), layer.rule, layer.contours.clone()))
            .collect();
        Self::new(image.design(), layers)
    }
}

/// Decode an SVG byte string into a [`VectorIcon`].
///
/// This is the desktop's icon-asset entry point for the SVG-first pipeline.
/// SVG is untrusted input: the decode is total and a
/// malformed or out-of-subset asset returns [`SvgError`] so the caller falls
/// back to a [`builtin_icon`](crate::builtin_icon) rather than crashing the
/// compositor.
///
/// # Errors
/// Propagates the [`SvgError`] from [`tairix_svg::decode`].
pub fn decode(bytes: &[u8]) -> Result<VectorIcon, SvgError> {
    Ok(VectorIcon::from_svg(&tairix_svg::decode(bytes)?))
}
