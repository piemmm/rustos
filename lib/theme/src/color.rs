//! The theme colour token.
//!
//! A [`Rgba`] is *design data*: a straight-alpha, 8-bit-per-channel colour
//! authored by a theme. It carries no compositing arithmetic — that lives
//! in the shared rasteriser's premultiplied-alpha pixel type
//! (`lib/raster`). Keeping the two apart is deliberate: a theme is a
//! table of colours, and the rasteriser is what blends them, so neither
//! reimplements the other (`AGENTS.md` §2.2).

/// A straight-alpha colour with 8 bits per channel.
///
/// Channels are **not** premultiplied; `a` is an independent opacity where
/// `0` is fully transparent and `255` fully opaque. A consumer that needs
/// to composite the colour converts it into the shared rasteriser's
/// premultiplied pixel type (`From<Rgba> for rustos_raster::Color`).
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub struct Rgba {
    /// Red channel, `0..=255`.
    pub r: u8,
    /// Green channel, `0..=255`.
    pub g: u8,
    /// Blue channel, `0..=255`.
    pub b: u8,
    /// Alpha channel: `0` fully transparent, `255` fully opaque.
    pub a: u8,
}

impl Rgba {
    /// Fully transparent (all channels zero).
    pub const TRANSPARENT: Self = Self {
        r: 0,
        g: 0,
        b: 0,
        a: 0,
    };

    /// An opaque colour from its red, green, and blue channels.
    #[must_use]
    pub const fn rgb(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b, a: 255 }
    }

    /// A colour from all four channels.
    #[must_use]
    pub const fn new(r: u8, g: u8, b: u8, a: u8) -> Self {
        Self { r, g, b, a }
    }

    /// The same colour with its alpha replaced by `a`.
    #[must_use]
    pub const fn with_alpha(self, a: u8) -> Self {
        Self { a, ..self }
    }

    /// The channels as `[r, g, b, a]`.
    #[must_use]
    pub const fn to_array(self) -> [u8; 4] {
        [self.r, self.g, self.b, self.a]
    }

    /// True when the colour is fully opaque (`a == 255`).
    #[must_use]
    pub const fn is_opaque(self) -> bool {
        self.a == 255
    }
}
