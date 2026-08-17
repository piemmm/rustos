//! Colours and premultiplied-alpha compositing arithmetic.
//!
//! [`blend_span`] is where a whole run of pixels is composited; the operators
//! below are the same arithmetic for one.
//!
//! The desktop rasteriser works exclusively in **premultiplied alpha**:
//! each colour channel of a [`Pixel`] is already scaled by its own alpha.
//! Premultiplication is what makes the Porter–Duff *over* operator a
//! single multiply-add per channel and keeps filtering and per-region
//! opacity correct.

use crate::dither::DitherRow;

/// The bias at which [`div255_biased`] rounds to nearest.
///
/// A quotient's fractional part is a multiple of 1/255, so it can never fall
/// exactly half way and `(value + 127) / 255` *is* the nearest integer to
/// `value / 255`. Every unbiased operator here rounds at this bias, which is
/// what makes the dithered ones the same arithmetic with the rounding point
/// moved rather than a second definition of it.
pub const ROUND_NEAREST: u32 = 127;

/// Integer division by 255, rounded up `bias`/255 of a level early, for a
/// value in `0..=65025`.
///
/// `value` is always a product of two `u8`s (so at most `255 * 255 =
/// 65025`), so the result is itself a valid `u8`; the `min(255)` is a
/// defensive clamp that lets the conversion stay total without an `unwrap`.
///
/// At [`ROUND_NEAREST`] this is exactly the nearest integer. A bias that
/// varies per pixel ([`DitherRow`]) is how a paint whose
/// output holds fewer levels than its input spends the missing resolution
/// across the area instead of contouring; the two are the same divide, and
/// nothing else about the arithmetic changes.
#[must_use]
pub fn div255_biased(value: u32, bias: u32) -> u8 {
    u8::try_from(((value + bias) / 255).min(255)).unwrap_or(u8::MAX)
}

/// Exact rounded integer division by 255 for a value in `0..=65025`.
#[must_use]
pub fn div255(value: u32) -> u8 {
    div255_biased(value, ROUND_NEAREST)
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

    /// Composite this straight-alpha colour *over* premultiplied `dst`,
    /// rounding the result into the destination's 8 bits once, `bias`/255 of
    /// a level above the exact value.
    ///
    /// The same Porter–Duff *over* as [`Pixel::over`] — `src + dst * (1 -
    /// src.a)` — evaluated from the authored colour rather than from a
    /// premultiplied copy of it. That is what makes it a single rounding:
    /// [`premultiply`](Self::premultiply) rounds the source term and
    /// [`Pixel::over`] then rounds the destination term, and here both terms
    /// are summed in `× 255` fixed point and rounded together. A destination
    /// that already holds this very colour therefore comes back exactly
    /// unchanged, whatever the alpha — a wash of the desktop colour over the
    /// desktop colour adds no noise of its own.
    ///
    /// `bias` chooses where in the level the value rounds up
    /// ([`div255_biased`]). Every channel takes the *same* bias, so a colour
    /// channel can never round above the alpha it is premultiplied by.
    #[must_use]
    pub(crate) fn over_biased(self, dst: Pixel, bias: u32) -> Pixel {
        let a = u32::from(self.a);
        let inv = 255 - a;
        let blend = |s: u8, d: u8| div255_biased(u32::from(s) * a + u32::from(d) * inv, bias);
        Pixel {
            r: blend(self.r, dst.r),
            g: blend(self.g, dst.g),
            b: blend(self.b, dst.b),
            // The source's own premultiplied alpha is `a` itself, which is
            // what a source channel of 255 weighs in at.
            a: blend(255, dst.a),
        }
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

    /// Scale every channel (colour **and** alpha) by `factor/255`, rounding
    /// at `bias` ([`div255_biased`]).
    ///
    /// Scaling a premultiplied pixel by an opacity factor is a single
    /// per-channel multiply; the result stays premultiplied, because every
    /// channel takes the same factor and the same bias. This is how
    /// per-surface, per-region, and rounded-corner coverage are applied
    /// before compositing.
    ///
    /// A translucent surface loses tonal resolution here exactly as a wash
    /// does — at opacity `f` the 256 levels its content held arrive in `f`
    /// of them — so a compositor that dithers its blend passes the same
    /// per-pixel bias in here, and the window's own gradients survive it.
    #[must_use]
    pub fn scale_alpha_biased(self, factor: u8, bias: u32) -> Self {
        if factor == 255 {
            return self;
        }
        let f = u32::from(factor);
        let scale = |c: u8| div255_biased(u32::from(c) * f, bias);
        Self {
            r: scale(self.r),
            g: scale(self.g),
            b: scale(self.b),
            a: scale(self.a),
        }
    }

    /// Scale every channel by `factor/255`, rounded to nearest.
    #[must_use]
    pub fn scale_alpha(self, factor: u8) -> Self {
        self.scale_alpha_biased(factor, ROUND_NEAREST)
    }

    /// Pull every colour channel toward this pixel's own luminance, keeping
    /// `saturation/255` of the colour it had.
    ///
    /// `255` returns the pixel untouched and `0` returns pure grey; alpha is
    /// coverage, not colour, so it is never touched. The grey is the BT.601
    /// luma the eye weights the primaries by (0.299 R, 0.587 G, 0.114 B),
    /// whose 8-bit weights sum to exactly 256, so a pixel that is already
    /// grey comes back unchanged.
    ///
    /// A premultiplied pixel may be desaturated in place: luma is linear in
    /// the channels and alpha scales all three alike, so the luma of a
    /// premultiplied pixel is the premultiplied luma, and a weighted average
    /// of two values that are each `<= a` stays `<= a`.
    #[must_use]
    pub fn desaturate(self, saturation: u8) -> Self {
        if saturation == 255 {
            return self;
        }
        let luma =
            (77 * u32::from(self.r) + 150 * u32::from(self.g) + 29 * u32::from(self.b) + 128) >> 8;
        let keep = u32::from(saturation);
        let grey = 255 - keep;
        let toward = |c: u8| div255(luma * grey + u32::from(c) * keep);
        Self {
            r: toward(self.r),
            g: toward(self.g),
            b: toward(self.b),
            a: self.a,
        }
    }

    /// Composite `self` (source) *over* `dst` (destination).
    ///
    /// Both operands are premultiplied; the result is
    /// `src + dst * (1 - src.a)`, the Porter–Duff *over* operator.
    ///
    /// A source authored as a straight-alpha [`Color`] reaches the same result
    /// through that type's own crate-internal `over`, which rounds once
    /// instead of twice and can spread that rounding across a span. That is
    /// what a wash needs; everything else composites here.
    ///
    /// An opaque source keeps none of the destination, so the general form
    /// below already reduces to `self`; returning it directly turns opaque
    /// text, panel fills, and window presents into plain stores instead of
    /// four multiplies and four divisions per pixel, mirroring the
    /// `factor == 255` fast path in [`Self::scale_alpha`].
    ///
    /// `bias` is where in the level the destination's surviving share rounds
    /// up ([`div255_biased`]); [`ROUND_NEAREST`] is the plain operator. A
    /// translucent source admits only `256 - a` of the destination's 256
    /// levels, so a compositor blending over a picture varies the bias per
    /// pixel and keeps the picture's gradients instead of banding them.
    #[must_use]
    pub fn over_biased(self, dst: Self, bias: u32) -> Self {
        if self.a == 255 {
            return self;
        }
        let inv = 255 - u32::from(self.a);
        let blend = |s: u8, d: u8| s.saturating_add(div255_biased(u32::from(d) * inv, bias));
        Self {
            r: blend(self.r, dst.r),
            g: blend(self.g, dst.g),
            b: blend(self.b, dst.b),
            a: blend(self.a, dst.a),
        }
    }

    /// Composite `self` (source) *over* `dst` (destination), rounded to
    /// nearest.
    #[must_use]
    pub fn over(self, dst: Self) -> Self {
        self.over_biased(dst, ROUND_NEAREST)
    }
}

/// Composite `src` over `dst` pixel for pixel, each source scaled by
/// `factor`/255 on its way in, rounding both at the surface row's ordered
/// dither.
///
/// This is the crate's one span composite, and the reason a caller reaches
/// for it rather than looping over [`Pixel::over_biased`] itself is speed, not
/// convenience: the operands are two slices walked in step, so the destination
/// is read and written straight through with no per-pixel bounds check,
/// coordinate conversion, or layer decision around it. A compositor laying a
/// translucent window over the picture beneath it, and [`Surface::blit`]
/// laying a sprite, are the same walk and must stay so — one blended pixel is
/// the same arithmetic wherever it comes from.
///
/// The two slices are paired by position: `dst[i]` takes `src[i]`, and the
/// shorter one ends the walk. `first_x` is the surface column `dst[0]` sits
/// at, so the dither is read at each pixel's own place on the surface and two
/// spans that meet cannot disagree about the phase between them.
///
/// A fully transparent source leaves its destination exactly as it found it,
/// so it is skipped before the arithmetic rather than composited to no effect.
///
/// [`Surface::blit`]: crate::Surface::blit
pub fn blend_span(dst: &mut [Pixel], src: &[Pixel], factor: u8, dither: DitherRow, first_x: u32) {
    blend_span_mapped(dst, src, factor, dither, first_x, |pixel| pixel);
}

/// [`blend_span`], with `map` applied to each source pixel on its way in.
///
/// `map` may not turn a transparent source opaque: the skip above happens
/// first, which is what keeps the mapped walk the same walk.
pub(crate) fn blend_span_mapped(
    dst: &mut [Pixel],
    src: &[Pixel],
    factor: u8,
    dither: DitherRow,
    first_x: u32,
    map: impl Fn(Pixel) -> Pixel,
) {
    for ((dst, src), x) in dst.iter_mut().zip(src).zip(first_x..) {
        if src.a == 0 {
            continue;
        }
        let bias = dither.bias(x);
        *dst = map(*src)
            .scale_alpha_biased(factor, bias)
            .over_biased(*dst, bias);
    }
}

/// `weight`/255 of the way from `from` to `to`, per premultiplied channel.
///
/// The crate's one mixer: the blur mixes a frosted copy back over what it
/// covers with it, and a laid rounded rectangle mixes its arc pixels toward
/// their new colour with it. Every channel is weighted identically, so a
/// colour channel can never come out above the alpha it is premultiplied by,
/// and the two extremes return their end exactly.
///
/// `bias` is where the weighted average rounds up ([`div255_biased`]). A
/// frost laid over a picture at a partial weight holds fewer levels than the
/// picture did, so every caller varies it per pixel from the surface's own
/// ordered dither.
#[must_use]
pub(crate) fn mix(from: Pixel, to: Pixel, weight: u8, bias: u32) -> Pixel {
    if weight == 255 {
        return to;
    }
    if weight == 0 {
        return from;
    }
    let keep = u32::from(255 - weight);
    let take = u32::from(weight);
    let channel =
        |from: u8, to: u8| div255_biased(u32::from(from) * keep + u32::from(to) * take, bias);
    Pixel {
        r: channel(from.r, to.r),
        g: channel(from.g, to.g),
        b: channel(from.b, to.b),
        a: channel(from.a, to.a),
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

#[cfg(test)]
#[path = "color_tests.rs"]
mod tests;
