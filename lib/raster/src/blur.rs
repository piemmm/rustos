//! The shared separable box blur.
//!
//! One definition serves every frosted surface: the compositor's backdrop
//! blur (a window's rectangle frosted before its own translucent pixels
//! blend over it) and a control's own soft highlight. A horizontal pass then
//! a vertical one, each carrying a running sum, so the cost is the region's
//! area rather than its area times the radius.
//!
//! Every channel — alpha included — is averaged. Averaging premultiplied
//! channels is the correct operation on premultiplied data: the result is
//! the same convex combination of the contributing colours that compositing
//! them would give, so the `colour <= alpha` invariant survives and no halo
//! appears around a translucent edge.
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
/// it grows and reuses — so a blur costs no allocation on the frame path. A
/// caller that blurs once per repaint rather than per frame can take
/// [`Surface::blur`](crate::Surface::blur) instead and let it allocate.
///
/// Nothing is blurred, and `region` is left exactly as it was, when the
/// radius is `0` (the effect is disabled), when either dimension is `0`, or
/// when `region` or `aux` is shorter than `width * height`: a caller that
/// mis-sizes a buffer gets the unblurred backdrop, never a partly-blurred
/// or out-of-bounds one.
pub fn box_blur(
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
/// approach the type's range for any radius a surface is drawn at. Every
/// operation saturates so that a caller passing an absurd radius — the
/// entry point is public and takes any `usize` — gets a flattened region
/// rather than an arithmetic panic.
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
        let weighted = |channel: u8| u32::from(channel).saturating_mul(times);
        self.r = self.r.saturating_add(weighted(pixel.r));
        self.g = self.g.saturating_add(weighted(pixel.g));
        self.b = self.b.saturating_add(weighted(pixel.b));
        self.a = self.a.saturating_add(weighted(pixel.a));
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
    /// Every channel divides by the same `count` and rounds the same way, and
    /// the sums are monotonic in the channel, so a channel can never round
    /// above the alpha it is premultiplied by.
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
    let rounded = sum.saturating_add(count / 2) / count;
    u8::try_from(rounded.min(255)).unwrap_or(u8::MAX)
}

#[cfg(test)]
#[path = "blur_tests.rs"]
mod tests;
