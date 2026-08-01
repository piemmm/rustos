//! Colours and premultiplied-alpha compositing arithmetic.
//!
//! The desktop rasteriser works exclusively in **premultiplied alpha**:
//! each colour channel of a [`Pixel`] is already scaled by its own alpha.
//! Premultiplication is what makes the Porter–Duff *over* operator a
//! single multiply-add per channel and keeps filtering and per-region
//! opacity correct.

/// Exact rounded integer division by 255 for a value in `0..=65025`.
///
/// `value` is always a product of two `u8`s (so at most `255 * 255 =
/// 65025`); the result is the nearest integer to `value / 255` and is
/// therefore itself a valid `u8`. The `min(255)` is a defensive clamp
/// that lets the conversion stay total without an `unwrap`.
#[must_use]
pub fn div255(value: u32) -> u8 {
    let rounded = (value + 128 + ((value + 128) >> 8)) >> 8;
    u8::try_from(rounded.min(255)).unwrap_or(u8::MAX)
}

/// A straight-alpha colour as authored by a client (not premultiplied).
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct Color {
    /// Red channel, `0..=255`.
    pub r: u8,
    /// Green channel, `0..=255`.
    pub g: u8,
    /// Blue channel, `0..=255`.
    pub b: u8,
    /// Alpha channel, `0` fully transparent, `255` fully opaque.
    pub a: u8,
}

impl Color {
    /// Fully transparent black (the cleared-surface value).
    pub const TRANSPARENT: Self = Self {
        r: 0,
        g: 0,
        b: 0,
        a: 0,
    };

    /// Construct an opaque colour.
    #[must_use]
    pub const fn rgb(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b, a: 255 }
    }

    /// Construct a colour with an explicit alpha.
    #[must_use]
    pub const fn rgba(r: u8, g: u8, b: u8, a: u8) -> Self {
        Self { r, g, b, a }
    }

    /// Premultiply this colour for storage in a [`Pixel`].
    #[must_use]
    pub fn premultiply(self) -> Pixel {
        let a = u32::from(self.a);
        Pixel {
            r: div255(u32::from(self.r) * a),
            g: div255(u32::from(self.g) * a),
            b: div255(u32::from(self.b) * a),
            a: self.a,
        }
    }
}

/// A premultiplied-alpha pixel: every colour channel is `<= a`.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct Pixel {
    /// Red channel, premultiplied by `a`.
    pub r: u8,
    /// Green channel, premultiplied by `a`.
    pub g: u8,
    /// Blue channel, premultiplied by `a`.
    pub b: u8,
    /// Alpha channel.
    pub a: u8,
}

impl Pixel {
    /// Fully transparent (all channels zero).
    pub const TRANSPARENT: Self = Self {
        r: 0,
        g: 0,
        b: 0,
        a: 0,
    };

    /// Recover the straight-alpha [`Color`] this pixel encodes.
    ///
    /// Transparent pixels carry no colour, so they un-premultiply to
    /// [`Color::TRANSPARENT`]. Fully opaque pixels are already straight
    /// (premultiplying by `a = 255` is the identity), so they are returned
    /// channel-for-channel without the per-channel divide — this is the
    /// common case for an opaque window frame, and skipping the division on
    /// it keeps a whole-surface conversion (e.g. presenting a maximised
    /// window) a plain copy rather than one integer division per channel per
    /// pixel, mirroring the `factor == 255` fast path in
    /// [`Self::scale_alpha`].
    #[must_use]
    pub fn unpremultiply(self) -> Color {
        if self.a == 0 {
            return Color::TRANSPARENT;
        }
        if self.a == 255 {
            return Color {
                r: self.r,
                g: self.g,
                b: self.b,
                a: 255,
            };
        }
        let a = u32::from(self.a);
        let recover = |c: u8| u8::try_from((u32::from(c) * 255 + a / 2) / a).unwrap_or(u8::MAX);
        Color {
            r: recover(self.r),
            g: recover(self.g),
            b: recover(self.b),
            a: self.a,
        }
    }

    /// Scale every channel (colour **and** alpha) by `factor/255`.
    ///
    /// Scaling a premultiplied pixel by an opacity factor is a single
    /// per-channel multiply; the result stays premultiplied. This is
    /// how per-surface, per-region, and rounded-corner coverage are
    /// applied before compositing.
    #[must_use]
    pub fn scale_alpha(self, factor: u8) -> Self {
        if factor == 255 {
            return self;
        }
        let f = u32::from(factor);
        Self {
            r: div255(u32::from(self.r) * f),
            g: div255(u32::from(self.g) * f),
            b: div255(u32::from(self.b) * f),
            a: div255(u32::from(self.a) * f),
        }
    }

    /// Composite `self` (source) *over* `dst` (destination).
    ///
    /// Both operands are premultiplied; the result is
    /// `src + dst * (1 - src.a)`, the Porter–Duff *over* operator.
    ///
    /// An opaque source keeps none of the destination, so the general form
    /// below already reduces to `self`; returning it directly turns opaque
    /// text, panel fills, and window presents into plain stores instead of
    /// four multiplies and four divisions per pixel, mirroring the
    /// `factor == 255` fast path in [`Self::scale_alpha`].
    #[must_use]
    pub fn over(self, dst: Self) -> Self {
        if self.a == 255 {
            return self;
        }
        let inv = 255 - u32::from(self.a);
        let blend = |s: u8, d: u8| s.saturating_add(div255(u32::from(d) * inv));
        Self {
            r: blend(self.r, dst.r),
            g: blend(self.g, dst.g),
            b: blend(self.b, dst.b),
            a: blend(self.a, dst.a),
        }
    }
}

impl From<tairix_theme::Rgba> for Color {
    /// Adopt a theme colour token as a straight-alpha rasteriser colour.
    ///
    /// The theme owns the authored colour *data*; the rasteriser owns the
    /// premultiplied-alpha arithmetic. This is the one edge where the two
    /// meet, so the colour algebra is never duplicated into the theme
    /// crate. The channel layout is identical, so the
    /// conversion is a field move.
    fn from(rgba: tairix_theme::Rgba) -> Self {
        Self {
            r: rgba.r,
            g: rgba.g,
            b: rgba.b,
            a: rgba.a,
        }
    }
}
