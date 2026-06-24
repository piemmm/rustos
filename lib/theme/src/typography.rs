//! The fonts a theme selects.
//!
//! A theme names fonts by *role*, not by widget: the UI font and the
//! monospace font. A font is referenced by the family name of an installed
//! face under `/System/Fonts` plus a size and weight;
//! this crate stores the reference, it does not rasterise glyphs.
//!
//! Sizes are *logical* pixels at the reference density
//! (`rustos_geometry::REFERENCE_DPI`); the desktop's DPI / UI scale
//! (`rustos_geometry::Scale`) converts a size to physical pixels when a face
//! is rasterised, so text stays a comfortable physical size across panel
//! densities.

use alloc::string::String;

/// The weight of a font face.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub enum FontWeight {
    /// Normal weight.
    Regular,
    /// A medium weight for subtle emphasis.
    Medium,
    /// Bold weight.
    Bold,
}

/// A reference to one font face at one size and weight.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FontSpec {
    /// Family name of an installed face under `/System/Fonts`.
    pub family: String,
    /// Nominal size in logical pixels at the reference density (scaled to
    /// physical pixels by `rustos_geometry::Scale`).
    pub size_px: u16,
    /// Face weight.
    pub weight: FontWeight,
}

impl FontSpec {
    /// A font specification from its parts.
    #[must_use]
    pub fn new(family: impl Into<String>, size_px: u16, weight: FontWeight) -> Self {
        Self {
            family: family.into(),
            size_px,
            weight,
        }
    }
}

/// The fonts a theme provides, one per role.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Fonts {
    /// The font used for window titles, menus, the taskbar, and app UI
    /// chrome.
    pub ui: FontSpec,
    /// The fixed-width font used by the terminal emulator and any
    /// code/log view.
    pub monospace: FontSpec,
}
