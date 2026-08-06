//! The compositor's backdrop blur.
//!
//! A window may ask the session to blur whatever is already composited
//! behind its rectangle before its own — typically translucent — pixels are
//! blended over it, so a panel or terminal reads like frosted glass. This
//! module is the one definition of that blur: a separable box blur, run as
//! a horizontal pass and then a vertical one, each carrying a running sum
//! so the cost is proportional to the region's area rather than to the area
//! times the radius.
//!
//! The pixels are premultiplied, and every channel — alpha included — is
//! averaged. Averaging premultiplied channels is the correct operation on
//! premultiplied data: the result is the same convex combination of the
//! contributing colours that compositing them would give, so the
//! `colour <= alpha` invariant survives and no halo appears around a
//! translucent edge.
//!
//! Edges replicate: a sample past the region's edge takes the edge pixel.
//! Every output therefore averages exactly `2 * radius + 1` samples, which
//! keeps the divisor constant across the pass and leaves a uniform field
//! exactly unchanged.

use crate::color::Pixel;

/// Blur `region` in place: a dense, row-major, premultiplied
/// `width`×`height` block of pixels, blurred by a separable box blur of
/// `radius` physical pixels using `aux` as the intermediate buffer.
///
/// `aux` is supplied by the caller — the compositor owns a scratch buffer
/// it grows and reuses — so a blur costs no allocation on the frame path.
///
/// Nothing is blurred, and `region` is left exactly as it was, when the
/// radius is `0` (the effect is disabled), when either dimension is `0`, or
/// when `region` or `aux` is shorter than `width * height`: a caller that
/// mis-sizes a buffer gets the unblurred backdrop, never a partly-blurred
/// or out-of-bounds one.
pub(crate) fn box_blur(
    region: &mut [Pixel],
    width: usize,
    height: usize,
    radius: usize,
    aux: &mut [Pixel],
) {
    let Some(count) = width.checked_mul(height) else {
        return;
    };
    if radius == 0 || count == 0 || region.len() < count || aux.len() < count {
        return;
    }
    for y in 0..height {
        let Some((src, dst)) = row_pair(region, aux, y, width) else {
            return;
        };
        blur_line(src, dst, 1, width, radius);
    }
    for x in 0..width {
        let Some((src, dst)) = column_pair(aux, region, x, count) else {
            return;
        };
        blur_line(src, dst, width, height, radius);
    }
}

/// Row `y` of `src` and of `dst`, both `width` pixels wide.
fn row_pair<'a>(
    src: &'a [Pixel],
    dst: &'a mut [Pixel],
    y: usize,
    width: usize,
) -> Option<(&'a [Pixel], &'a mut [Pixel])> {
    let start = y.checked_mul(width)?;
    let end = start.checked_add(width)?;
    Some((src.get(start..end)?, dst.get_mut(start..end)?))
}

/// Column `x` of `src` and of `dst`, each the `count`-pixel block's tail
/// from that column on: the strided [`blur_line`] walks it a row at a time.
fn column_pair<'a>(
    src: &'a [Pixel],
    dst: &'a mut [Pixel],
    x: usize,
    count: usize,
) -> Option<(&'a [Pixel], &'a mut [Pixel])> {
    Some((src.get(x..count)?, dst.get_mut(x..count)?))
}

/// Average each of `len` samples of `src` with its neighbours within
/// `radius`, writing the result to `dst`. Both are walked with `stride`
/// pixels between consecutive samples, so one implementation serves the
/// horizontal pass (stride `1`) and the vertical one (stride `width`).
///
/// The window slides by adding the sample entering it and subtracting the
/// one leaving, so each output costs a constant amount of work whatever the
/// radius. Samples outside `0..len` replicate the nearest edge, which keeps
/// the divisor at `2 * radius + 1` for every output.
fn blur_line(src: &[Pixel], dst: &mut [Pixel], stride: usize, len: usize, radius: usize) {
    let Some(last) = len.checked_sub(1) else {
        return;
    };
    let span = radius.min(last);
    let mut sum = Sum::default();
    // Prime the window over `-radius..=radius`. The replicated ends are
    // counted arithmetically rather than sample by sample, so priming costs
    // the line's length at most however wide the radius is.
    sum.add(at(src, stride, 0), radius.saturating_add(1));
    for i in 1..=span {
        sum.add(at(src, stride, i), 1);
    }
    sum.add(at(src, stride, last), radius - span);
    let count = radius.saturating_mul(2).saturating_add(1);
    for i in 0..len {
        if let Some(slot) = index(stride, i).and_then(|offset| dst.get_mut(offset)) {
            *slot = sum.mean(count);
        }
        let entering = i.saturating_add(radius).saturating_add(1).min(last);
        let leaving = i.saturating_sub(radius);
        sum.add(at(src, stride, entering), 1);
        sum.sub(at(src, stride, leaving));
    }
}

/// Sample `i` of a strided line, or a transparent pixel if the index falls
/// outside the buffer — which the caller's clamping already prevents, and
/// which must not fabricate an out-of-bounds read if it ever did not.
fn at(src: &[Pixel], stride: usize, i: usize) -> Pixel {
    index(stride, i)
        .and_then(|offset| src.get(offset))
        .copied()
        .unwrap_or(Pixel::TRANSPARENT)
}

/// Buffer offset of sample `i` of a line with `stride` pixels between
/// samples.
fn index(stride: usize, i: usize) -> Option<usize> {
    i.checked_mul(stride)
}

/// The running channel sums of the samples currently inside the sliding
/// window.
///
/// A `u32` per channel is ample: a channel is at most 255 and the window
/// holds at most one screen dimension's worth of samples, so the sum cannot
/// approach the type's range. The saturating arithmetic is belt-and-braces
/// against a caller that unbalanced the adds and subtractions rather than a
/// reachable state.
#[derive(Copy, Clone, Default)]
struct Sum {
    r: u32,
    g: u32,
    b: u32,
    a: u32,
}

impl Sum {
    /// Add `times` copies of `pixel` to the window.
    fn add(&mut self, pixel: Pixel, times: usize) {
        let times = u32::try_from(times).unwrap_or(u32::MAX);
        self.r = self.r.saturating_add(u32::from(pixel.r) * times);
        self.g = self.g.saturating_add(u32::from(pixel.g) * times);
        self.b = self.b.saturating_add(u32::from(pixel.b) * times);
        self.a = self.a.saturating_add(u32::from(pixel.a) * times);
    }

    /// Remove one copy of `pixel` from the window.
    fn sub(&mut self, pixel: Pixel) {
        self.r = self.r.saturating_sub(u32::from(pixel.r));
        self.g = self.g.saturating_sub(u32::from(pixel.g));
        self.b = self.b.saturating_sub(u32::from(pixel.b));
        self.a = self.a.saturating_sub(u32::from(pixel.a));
    }

    /// The window's mean over `count` samples, rounded to nearest.
    ///
    /// Every channel divides by the same `count` and rounds the same way, so
    /// a channel can never round above the alpha it is premultiplied by.
    fn mean(self, count: usize) -> Pixel {
        let count = u32::try_from(count).unwrap_or(u32::MAX).max(1);
        Pixel {
            r: mean(self.r, count),
            g: mean(self.g, count),
            b: mean(self.b, count),
            a: mean(self.a, count),
        }
    }
}

/// One channel's rounded mean, clamped into a byte.
fn mean(sum: u32, count: u32) -> u8 {
    let rounded = (sum + count / 2) / count;
    u8::try_from(rounded.min(255)).unwrap_or(u8::MAX)
}

#[cfg(test)]
mod tests {
    use alloc::vec;
    use alloc::vec::Vec;

    use super::box_blur;
    use crate::color::Pixel;

    /// An opaque grey.
    fn grey(v: u8) -> Pixel {
        Pixel {
            r: v,
            g: v,
            b: v,
            a: 255,
        }
    }

    /// Blur `region` (given as `width`×`height`) and hand back the result.
    fn blurred(region: &[Pixel], width: usize, height: usize, radius: usize) -> Vec<Pixel> {
        let mut pixels = region.to_vec();
        let mut aux = vec![Pixel::TRANSPARENT; width * height];
        box_blur(&mut pixels, width, height, radius, &mut aux);
        pixels
    }

    #[test]
    fn uniform_field_is_unchanged() {
        let field = vec![grey(40); 7 * 5];
        assert_eq!(blurred(&field, 7, 5, 2), field);
    }

    #[test]
    fn radius_zero_is_identity() {
        let mut field = vec![grey(10); 9];
        field[4] = grey(200);
        assert_eq!(blurred(&field, 3, 3, 0), field);
    }

    #[test]
    fn impulse_spreads_symmetrically() {
        let mut field = vec![grey(0); 5 * 5];
        field[2 * 5 + 2] = grey(255);
        let out = blurred(&field, 5, 5, 1);

        let centre = out[2 * 5 + 2];
        assert!(centre.r > 0 && centre.r < 255, "the impulse spread out");
        for (x, y) in [(1, 2), (3, 2), (2, 1), (2, 3)] {
            assert_eq!(
                out[y * 5 + x],
                centre,
                "a 3x3 box spreads an impulse equally over its whole window"
            );
        }
        assert_eq!(out[0], grey(0), "a pixel two columns away is untouched");
    }

    #[test]
    fn impulse_conserves_total_energy() {
        let mut field = vec![Pixel::TRANSPARENT; 9 * 9];
        field[4 * 9 + 4] = grey(90);
        let out = blurred(&field, 9, 9, 1);
        let total: u32 = out.iter().map(|p| u32::from(p.r)).sum();
        assert_eq!(total, 90, "a box blur redistributes, it does not amplify");
    }

    #[test]
    fn alpha_is_averaged_with_the_colour_channels() {
        let opaque = grey(200);
        let field = vec![
            opaque,
            opaque,
            opaque,
            Pixel::TRANSPARENT,
            Pixel::TRANSPARENT,
            Pixel::TRANSPARENT,
        ];
        let out = blurred(&field, 6, 1, 1);
        let edge = out[3];
        assert!(
            edge.a > 0 && edge.a < 255,
            "alpha blurs across the edge like every other channel"
        );
        assert!(
            edge.r <= edge.a,
            "the premultiplied invariant survives the blur"
        );
    }

    #[test]
    fn single_pixel_region_is_returned_unchanged() {
        let field = vec![grey(77)];
        assert_eq!(blurred(&field, 1, 1, 4), field);
    }

    #[test]
    fn single_column_and_single_row_regions_are_handled() {
        let column = vec![grey(0), grey(255), grey(0)];
        let out = blurred(&column, 1, 3, 3);
        assert_eq!(out[0], out[2], "a clamped column blurs symmetrically");
        assert!(out[1].r > 0 && out[1].r < 255);

        let row = vec![grey(0), grey(255), grey(0)];
        assert_eq!(blurred(&row, 3, 1, 3), out, "a row blurs like a column");
    }

    #[test]
    fn radius_wider_than_the_region_still_averages() {
        let field = vec![grey(0), grey(120), grey(240), grey(0)];
        let out = blurred(&field, 4, 1, 64);
        assert!(
            out.windows(2).all(|pair| pair[0] == pair[1]),
            "a radius that swallows the region flattens it"
        );
    }

    #[test]
    fn short_buffers_leave_the_region_untouched() {
        let field = vec![grey(10), grey(250), grey(10), grey(250)];
        let mut pixels = field.clone();
        let mut aux = vec![Pixel::TRANSPARENT; 2];
        box_blur(&mut pixels, 2, 2, 1, &mut aux[..1]);
        assert_eq!(pixels, field, "a short scratch buffer blurs nothing");

        let mut short = field[..3].to_vec();
        box_blur(&mut short, 2, 2, 1, &mut aux);
        assert_eq!(short, field[..3], "a short region blurs nothing");
    }
}
