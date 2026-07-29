//! The theme colour token.
//!
//! A [`Rgba`] is *design data*: a straight-alpha, 8-bit-per-channel colour
//! authored by a theme. It carries no compositing arithmetic — that lives
//! in the shared rasteriser's premultiplied-alpha pixel type
//! (`lib/raster`). Keeping the two apart is deliberate: a theme is a
//! table of colours, and the rasteriser is what blends them, so neither
//! reimplements the other.

/// A straight-alpha colour with 8 bits per channel.
///
/// Channels are **not** premultiplied; `a` is an independent opacity where
/// `0` is fully transparent and `255` fully opaque. A consumer that needs
/// to composite the colour converts it into the shared rasteriser's
/// premultiplied pixel type (`From<Rgba> for tairix_raster::Color`).
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

    /// This colour mixed toward `other` by `permille` (`0` keeps `self`,
    /// `1000` returns `other`), each channel interpolated independently.
    ///
    /// A theme derives the near-neighbours of a role this way — the slightly
    /// darker plate a pressed filled control shows, the slightly brighter one
    /// it shows on hover — instead of the palette carrying a hand-authored
    /// token per interaction state. Straight-alpha channels interpolate
    /// directly; compositing still belongs to the rasteriser.
    #[must_use]
    pub fn mix(self, other: Self, permille: u16) -> Self {
        Self {
            r: mix_channel(self.r, other.r, permille),
            g: mix_channel(self.g, other.g, permille),
            b: mix_channel(self.b, other.b, permille),
            a: mix_channel(self.a, other.a, permille),
        }
    }
}

/// Interpolate one channel toward `to` by `permille`, clamped so an
/// out-of-range weight saturates at the endpoints rather than wrapping.
fn mix_channel(from: u8, to: u8, permille: u16) -> u8 {
    let weight = u32::from(permille.min(1000));
    let blended = (u32::from(from) * (1000 - weight) + u32::from(to) * weight + 500) / 1000;
    u8::try_from(blended.min(u32::from(u8::MAX))).unwrap_or(u8::MAX)
}

#[cfg(test)]
mod tests {
    use super::Rgba;

    const BLACK: Rgba = Rgba::rgb(0, 0, 0);
    const WHITE: Rgba = Rgba::rgb(255, 255, 255);

    #[test]
    fn mix_endpoints_are_exact() {
        assert_eq!(BLACK.mix(WHITE, 0), BLACK);
        assert_eq!(BLACK.mix(WHITE, 1000), WHITE);
    }

    #[test]
    fn mix_interpolates_each_channel_independently() {
        let from = Rgba::new(0, 100, 200, 40);
        let to = Rgba::new(200, 100, 0, 240);
        assert_eq!(from.mix(to, 500), Rgba::new(100, 100, 100, 140));
    }

    #[test]
    fn mix_saturates_an_out_of_range_weight() {
        assert_eq!(BLACK.mix(WHITE, u16::MAX), WHITE);
    }

    #[test]
    fn mix_rounds_to_nearest() {
        let from = Rgba::rgb(0, 0, 0);
        let to = Rgba::rgb(1, 1, 1);
        assert_eq!(from.mix(to, 499), from);
        assert_eq!(from.mix(to, 500), to);
    }
}
