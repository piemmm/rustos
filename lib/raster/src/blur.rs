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

use alloc::vec::Vec;

use crate::color::{div255, Pixel};
use crate::surface::Surface;

/// Blur `region` in place: a dense, row-major, premultiplied
/// `width`×`height` block of pixels, blurred by a separable box blur of
/// `radius` physical pixels using `aux` as the intermediate buffer.
///
/// `aux` is supplied by the caller — so a blur costs no allocation on the
/// frame path. A caller frosting a rectangle of a surface takes
/// [`Surface::frost_region`] instead, which owns the copy-and-mix around
/// this pass and carries both buffers in a [`BlurScratch`].
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

/// The buffers a frost works in: the copy of the region that is blurred,
/// and the intermediate [`box_blur`]'s two passes hand over through.
///
/// Both grow to the largest region asked of them and are then reused, so a
/// per-frame caller — the compositor frosting a window's backdrop on every
/// composite — allocates nothing once its scratch is warm.
#[derive(Default)]
pub struct BlurScratch {
    region: Vec<Pixel>,
    aux: Vec<Pixel>,
}

impl BlurScratch {
    /// An empty scratch, which allocates on its first frost and not before.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            region: Vec::new(),
            aux: Vec::new(),
        }
    }

    /// Give the memory back, so a scratch that frosted a large region stops
    /// holding it; the next frost grows it again.
    pub fn release(&mut self) {
        self.region = Vec::new();
        self.aux = Vec::new();
    }

    /// Both buffers as exactly `count` pixels, lengthened if they were
    /// shorter and never shortened, or `None` when the memory could not be
    /// reserved — a frost with no scratch to work in leaves the surface
    /// untouched rather than frosting part of it.
    fn buffers(&mut self, count: usize) -> Option<(&mut [Pixel], &mut [Pixel])> {
        if !grow(&mut self.region, count) || !grow(&mut self.aux, count) {
            return None;
        }
        Some((self.region.get_mut(..count)?, self.aux.get_mut(..count)?))
    }
}

/// Lengthen `buffer` to `count` pixels, reserving first so exhaustion is
/// answered with `false` rather than an allocation abort.
fn grow(buffer: &mut Vec<Pixel>, count: usize) -> bool {
    let Some(extra) = count.checked_sub(buffer.len()) else {
        return true;
    };
    if buffer.try_reserve(extra).is_err() {
        return false;
    }
    buffer.resize(count, Pixel::TRANSPARENT);
    true
}

impl Surface {
    /// Frost `[x, x+w) × [y, y+h)`: blur the pixels already there by
    /// `radius` and mix the blurred copy back over them at each pixel's
    /// `coverage` — `255` takes the blurred pixel, `0` keeps the original,
    /// and the values between are the weighted mix.
    ///
    /// This is the desktop's one frosted glass: the compositor frosts a
    /// window's backdrop before the window's own translucent pixels blend
    /// over it, and the login screen frosts the wallpaper behind a selected
    /// account tile. Weighting the mix rather than clipping it is what lets
    /// a rounded shape fade from frosted to untouched across its own arc
    /// instead of showing a square edge.
    ///
    /// `coverage` is asked about a pixel's position relative to the
    /// rectangle's **own** top-left, so a caller whose rectangle the surface
    /// edge or the clip window cuts short still reads the whole shape rather
    /// than re-fitting it to what survives.
    ///
    /// The frost is confined to what the surface bounds and the active clip
    /// window admit and reads only the pixels it may write: samples past
    /// that edge replicate it, so the effect can neither pull a neighbour's
    /// pixels in nor mark a pixel outside. `scratch` carries the copy and
    /// the blur's intermediate between calls and holds nothing from one
    /// frost to the next.
    ///
    /// The surface is left exactly as it was, never partly frosted, when
    /// `radius` is `0` (the effect is disabled), when the rectangle is empty
    /// or lands nowhere the surface admits, or when the scratch could not be
    /// grown.
    #[expect(
        clippy::too_many_arguments,
        reason = "the rectangle is spelled as the four scalars every other \
                  Surface primitive takes, plus the blur radius, the caller's \
                  reused scratch, and the shape being frosted"
    )]
    pub fn frost_region(
        &mut self,
        x: u32,
        y: u32,
        w: u32,
        h: u32,
        radius: u32,
        scratch: &mut BlurScratch,
        coverage: impl Fn(u32, u32) -> u8,
    ) {
        if radius == 0 {
            return;
        }
        let Some((columns, rows)) = self.admitted(x, y, w, h) else {
            return;
        };
        let cols = columns.end - columns.start;
        let (Ok(width), Ok(height)) = (
            usize::try_from(cols),
            usize::try_from(rows.end - rows.start),
        ) else {
            return;
        };
        let Some(count) = width.checked_mul(height) else {
            return;
        };
        let Some((region, aux)) = scratch.buffers(count) else {
            return;
        };
        for (row, copy) in rows.clone().zip(region.chunks_exact_mut(width)) {
            let Some((_, source)) = self.row_span_mut(row, columns.start, cols) else {
                continue;
            };
            for (dst, src) in copy.iter_mut().zip(source.iter()) {
                *dst = *src;
            }
        }
        box_blur(
            region,
            width,
            height,
            usize::try_from(radius).unwrap_or(usize::MAX),
            aux,
        );
        let lead = columns.start - x;
        for (row, blurred) in rows.zip(region.chunks_exact(width)) {
            let Some((_, target)) = self.row_span_mut(row, columns.start, cols) else {
                continue;
            };
            let ly = row - y;
            for ((dst, src), lx) in target.iter_mut().zip(blurred).zip(lead..) {
                *dst = mix(*dst, *src, coverage(lx, ly));
            }
        }
    }
}

/// `weight`/255 of the way from `from` to `to`, per premultiplied channel.
///
/// Every channel is weighted identically, so a colour channel can never
/// come out above the alpha it is premultiplied by, and the two extremes
/// return their end exactly.
fn mix(from: Pixel, to: Pixel, weight: u8) -> Pixel {
    if weight == 255 {
        return to;
    }
    if weight == 0 {
        return from;
    }
    let keep = u32::from(255 - weight);
    let take = u32::from(weight);
    let channel = |from: u8, to: u8| div255(u32::from(from) * keep + u32::from(to) * take);
    Pixel {
        r: channel(from.r, to.r),
        g: channel(from.g, to.g),
        b: channel(from.b, to.b),
        a: channel(from.a, to.a),
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
