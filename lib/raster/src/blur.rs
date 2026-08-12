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

use crate::color::{mix, Pixel};
use crate::dither::DitherRow;
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
    // The window is the same size for every output of both passes, so its
    // divisor is resolved here rather than per pixel.
    let recip = Reciprocal::new(radius.saturating_mul(2).saturating_add(1));
    for y in 0..height {
        let Some((src, dst)) = row_pair(region, aux, y, width) else {
            return;
        };
        blur_line(src, dst, 1, width, radius, recip);
    }
    for x in 0..width {
        let Some((src, dst)) = column_pair(aux, region, x, count) else {
            return;
        };
        blur_line(src, dst, width, height, radius, recip);
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
    /// A partial coverage is a translucent field over a picture, so the mix
    /// rounds through the surface's own ordered dither: a frosted bar over a
    /// smooth wallpaper keeps the wallpaper's gradient instead of stepping it
    /// into plateaus. Full coverage takes the blurred pixel exactly and no
    /// coverage leaves the destination exactly, at every bias.
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
            let Some((first, target)) = self.row_span_mut(row, columns.start, cols) else {
                continue;
            };
            let ly = row - y;
            let dither = DitherRow::at(row);
            for (((dst, src), lx), column) in
                target.iter_mut().zip(blurred).zip(lead..).zip(first..)
            {
                *dst = mix(*dst, *src, coverage(lx, ly), dither.bias(column));
            }
        }
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
/// the divisor at `2 * radius + 1` for every output — constant for the whole
/// pass, which is why `recip` is resolved once by the caller.
///
/// The output slot and the two samples the window trades are each monotone
/// along the line, so all three are walked as strided iterators and the
/// furthest offset any of them can reach is bounds-checked once here instead
/// of per sample. An iterator that runs out is exactly a clamped end, so the
/// replicated edge pixel stands in for it.
fn blur_line(
    src: &[Pixel],
    dst: &mut [Pixel],
    stride: usize,
    len: usize,
    radius: usize,
    recip: Reciprocal,
) {
    let Some(last) = len.checked_sub(1) else {
        return;
    };
    let Some(last_offset) = last.checked_mul(stride) else {
        return;
    };
    let (Some(&first), Some(&edge)) = (src.first(), src.get(last_offset)) else {
        return;
    };
    if dst.len() <= last_offset {
        return;
    }

    // Prime the window over `-radius..=radius`. The replicated ends are
    // counted arithmetically rather than sample by sample, so priming costs
    // the line's length at most however wide the radius is.
    let span = radius.min(last);
    let mut sum = Sum::default();
    sum.add_many(first, radius.saturating_add(1));
    for &pixel in src.iter().step_by(stride).skip(1).take(span) {
        sum.add(pixel);
    }
    sum.add_many(edge, radius.saturating_sub(span));

    let mut entering = src
        .get(radius.saturating_add(1).min(last).saturating_mul(stride)..)
        .unwrap_or_default()
        .iter()
        .step_by(stride);
    let mut leaving = src.iter().step_by(stride).skip(1);
    let mut out = dst.iter_mut().step_by(stride);

    // Until the window's trailing edge clears the start of the line, the
    // sample leaving it is the replicated first pixel. Splitting the walk
    // there costs the second sample's clamp nothing at all.
    for slot in out.by_ref().take(span.saturating_add(1)) {
        *slot = sum.mean(recip);
        sum.add(*entering.next().unwrap_or(&edge));
        sum.sub(first);
    }
    for slot in out {
        *slot = sum.mean(recip);
        sum.add(*entering.next().unwrap_or(&edge));
        sum.sub(*leaving.next().unwrap_or(&first));
    }
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
    fn add_many(&mut self, pixel: Pixel, times: usize) {
        let times = u32::try_from(times).unwrap_or(u32::MAX);
        let weighted = |channel: u8| u32::from(channel).saturating_mul(times);
        self.r = self.r.saturating_add(weighted(pixel.r));
        self.g = self.g.saturating_add(weighted(pixel.g));
        self.b = self.b.saturating_add(weighted(pixel.b));
        self.a = self.a.saturating_add(weighted(pixel.a));
    }

    /// Add one copy of `pixel` to the window.
    fn add(&mut self, pixel: Pixel) {
        self.r = self.r.saturating_add(u32::from(pixel.r));
        self.g = self.g.saturating_add(u32::from(pixel.g));
        self.b = self.b.saturating_add(u32::from(pixel.b));
        self.a = self.a.saturating_add(u32::from(pixel.a));
    }

    /// Remove one copy of `pixel` from the window.
    fn sub(&mut self, pixel: Pixel) {
        self.r = self.r.saturating_sub(u32::from(pixel.r));
        self.g = self.g.saturating_sub(u32::from(pixel.g));
        self.b = self.b.saturating_sub(u32::from(pixel.b));
        self.a = self.a.saturating_sub(u32::from(pixel.a));
    }

    /// The window's mean over `recip`'s divisor, rounded to nearest.
    fn mean(self, recip: Reciprocal) -> Pixel {
        Pixel {
            r: recip.apply(self.r),
            g: recip.apply(self.g),
            b: recip.apply(self.b),
            a: recip.apply(self.a),
        }
    }
}

/// The fractional bits the reciprocal multiply is computed with.
const RECIPROCAL_SHIFT: u32 = 40;

/// The largest window the reciprocal multiply is exactly equal to the divide
/// for, and therefore the largest it is used at.
const RECIPROCAL_MAX_COUNT: u32 = 65_536;

/// How one window's mean is divided by its sample count.
///
/// The count is `2 * radius + 1` for every output of a pass, so it is resolved
/// once for the pass instead of dividing four times per pixel per pass — which
/// was the dominant cost of a frosted window.
#[derive(Copy, Clone)]
enum Reciprocal {
    /// Multiply by a fixed-point reciprocal, which for every window the blur
    /// is used at gives *exactly* the same answer as the divide.
    ///
    /// The target is `floor((n + d/2) / d)`, so with `n` the rounded numerator
    /// and `m = ceil(2^S / d)` the claim is `floor(n*m / 2^S) == floor(n/d)`.
    /// Write `n = k*d + s` with `s <= d-1`, and `e = m*d - 2^S` (so `e <= d-1`
    /// by construction). Then `n*m/2^S = n/d + n*e/(d*2^S)`, and the two floors
    /// agree exactly while `s*2^S + n*e < d*2^S`; since `s <= d-1` it suffices
    /// that `n*e < 2^S`.
    ///
    /// A window holds exactly `d` samples of at most 255 each, so `n < 256*d`,
    /// and `(256*d - 1) * (d - 1) < 2^40` holds for every `d` up to 65536 —
    /// which is why that is the cutoff, and why the product stays under `2^48`.
    /// `blur_tests` checks the condition for every count in range, and that a
    /// count above it genuinely breaks the proof, rather than leaving either
    /// argued.
    ///
    /// The product saturates so the answer stays a total function of its
    /// argument: a numerator from outside a window of `d` samples — which no
    /// blur produces — reads as fully bright rather than overflowing.
    Multiply { m: u64, half: u32 },
    /// Divide, for a count past the range the multiply is exact over. No
    /// surface is drawn at such a radius; correctness does not depend on that.
    Divide { d: u32 },
}

impl Reciprocal {
    /// The divisor for a window of `count` samples. A count of zero would make
    /// no window, so it reads as one.
    fn new(count: usize) -> Self {
        let d = u32::try_from(count).unwrap_or(u32::MAX).max(1);
        if d <= RECIPROCAL_MAX_COUNT {
            let m = (1u64 << RECIPROCAL_SHIFT).div_ceil(u64::from(d));
            Self::Multiply { m, half: d / 2 }
        } else {
            Self::Divide { d }
        }
    }

    /// One channel's rounded mean, clamped to the channel range.
    #[inline]
    fn apply(self, sum: u32) -> u8 {
        let rounded = match self {
            Self::Multiply { m, half } => {
                u64::from(sum.saturating_add(half)).saturating_mul(m) >> RECIPROCAL_SHIFT
            }
            Self::Divide { d } => u64::from(sum.saturating_add(d / 2)) / u64::from(d),
        };
        u8::try_from(rounded.min(255)).unwrap_or(u8::MAX)
    }
}

#[cfg(test)]
#[path = "blur_tests.rs"]
mod tests;
