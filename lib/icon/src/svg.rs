//! Building a [`VectorIcon`] from a decoded SVG asset.
//!
//! Desktop icons are authored as SVG (the SVG-first asset rule). A decoded
//! [`SvgImage`] is already a square design grid plus the shared artwork tree
//! — exactly a [`VectorIcon`] — so the conversion is a direct field map and
//! the glyph still rasterises through `lib/raster`'s single scan converter.
//! Unlike the built-in glyphs, an SVG icon carries its own per-layer paints,
//! so it is not tinted by the caller.

use tairix_svg::font::FontProvider;
use tairix_svg::{SvgError, SvgImage};

use crate::vector::VectorIcon;

impl VectorIcon {
    /// Build an icon from a decoded [`SvgImage`], preserving its design grid
    /// and its artwork exactly.
    #[must_use]
    pub fn from_svg(image: &SvgImage) -> Self {
        Self::from_artwork(image.design(), image.nodes().to_vec())
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
/// `fonts` is the seam an icon carrying `<text>` resolves its faces
/// through. A caller with no font authority supplies
/// [`tairix_svg::font::NoFonts`], and such an icon is then refused rather
/// than drawn with its lettering silently missing — which is exactly what
/// makes the built-in glyph the answer instead.
///
/// # Errors
/// Propagates the [`SvgError`] from [`tairix_svg::decode`].
pub fn decode(bytes: &[u8], fonts: &mut dyn FontProvider) -> Result<VectorIcon, SvgError> {
    Ok(VectorIcon::from_svg(&tairix_svg::decode(
        bytes,
        tairix_svg::Viewport::Square,
        fonts,
    )?))
}
