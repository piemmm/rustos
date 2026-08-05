//! The desktop's one image resampler: a separable, integer, filtered
//! resample of straight-alpha RGBA8 pixels.
//!
//! Two consumers scale decoded images: the icon pipeline fits a bundle's
//! artwork into a square slot, and the wallpaper pipeline places a
//! photograph onto a screen. Both want the same arithmetic — reconstruct a
//! continuous image from the source samples, band-limit it to what the
//! destination grid can carry, and weight the colour channels by alpha so a
//! transparent neighbour cannot bleed its colour into an opaque pixel — so
//! that arithmetic lives here once rather than beside each caller.
//!
//! # The filter
//!
//! Resampling is reconstruction followed by prefiltering, and which of the
//! two dominates is decided entirely by the ratio between the source extent
//! and the destination extent on that axis. This resampler therefore uses
//! the one kernel that is correct for the direction it is going, per axis:
//!
//! - **Reducing** (more source samples than destination samples) the
//!   prefilter dominates: a destination sample is the exact integral of the
//!   source over the footprint it covers, with *fractional* weights on the
//!   two partly-covered end samples. That is the area average, and it is
//!   aliasing-free at every ratio. Weighting the end samples by their real
//!   coverage is what matters: a filter that instead averaged whole samples
//!   would take one source sample for some destination pixels and two for
//!   the next at a ratio of, say, 1.4, and that alternation is visible as
//!   hard-edged, blocky texture across a photograph.
//! - **Enlarging** (or 1:1) reconstruction dominates: the destination
//!   samples the Catmull-Rom cubic through the source samples. Holding each
//!   source sample across the destination pixels it lands on — a
//!   sample-and-hold — reproduces the source grid as visible blocks, which
//!   is precisely the artefact a wallpaper drawn larger than its decoded
//!   source must not show. The cubic is interpolating, so at 1:1 its
//!   weights collapse to a single unit tap and the resample is an exact
//!   copy; there is no needless blur on the ratio callers hit most.
//!
//! Both arms produce the same shape of answer — a small run of source
//! samples and a weight each — so there is one plan, one accumulation, and
//! one write-out for every resample this crate performs.
//!
//! # Arithmetic
//!
//! Integer throughout: weights are fixed-point fractions, normalised so each
//! destination sample's weights sum to exactly one. That exactness is what
//! keeps a flat region flat — a rounding residual spread across a large image
//! would show as banding — and it means two calls that should agree cannot
//! drift apart the way floating point would.
//!
//! Colour is filtered **premultiplied** and divided back out at the end.
//! That is the only correct way to filter an image with an alpha channel: a
//! fully transparent source pixel contributes its transparency but not its
//! (meaningless) colour, so scaling artwork with transparent padding cannot
//! drag that padding's colour into the visible edge. A destination pixel
//! whose whole footprint is transparent has no colour to report and yields
//! transparent black.
//!
//! # Bands
//!
//! [`resample_rows`] produces an arbitrary contiguous run of destination
//! rows, so a caller that cannot hold (or cannot transport) a whole
//! destination image at once can produce it a band at a time — each band is
//! computed from the source and the plan alone, never from a previous band,
//! so assembling bands yields byte-for-byte the image one call would have
//! produced. [`resample`] is the whole-image case expressed through that
//! same one implementation.
//!
//! Scratch memory is a few destination-width rows regardless of how
//! extreme the ratio is, so reducing a 4K photograph to a thumbnail costs
//! no more working memory than reducing it to a screen.
//!
//! Every entry point is total: degenerate geometry, a source region that
//! does not lie inside its image, a mis-sized output buffer, and a band
//! that runs past the destination all return a typed refusal rather than
//! panicking or writing a partial result.

use alloc::vec;
use alloc::vec::Vec;

/// Bytes per straight-alpha RGBA8 pixel.
const CHANNELS: usize = 4;

/// [`CHANNELS`] for the widened arithmetic every offset is computed in.
const CHANNELS_U64: u64 = 4;

/// Fraction bits every filter weight carries.
const WEIGHT_SHIFT: u32 = 14;

/// The fixed-point weight that means "all of this sample": every
/// destination sample's weights sum to exactly this.
const WEIGHT_ONE: i32 = 1 << WEIGHT_SHIFT;

/// [`WEIGHT_ONE`] for the widened arithmetic the weights are derived in.
const WEIGHT_ONE_WIDE: i128 = 1 << WEIGHT_SHIFT;

/// Fraction bits the cubic's polynomial is evaluated in, wider than
/// [`WEIGHT_SHIFT`] so the cube of a fraction keeps its precision before it
/// is rounded down into a weight.
const CUBIC_SHIFT: u32 = 16;

/// The taps a cubic reconstruction reads: the two source samples either
/// side of the point being reconstructed.
const CUBIC_TAPS: usize = 4;

/// Horizontally-filtered rows kept for reuse between destination rows.
///
/// Consecutive destination rows share at most one source row while
/// reducing, and at most [`CUBIC_TAPS`] − 1 while enlarging, so this
/// captures every reuse either arm can offer. Capping it keeps the scratch
/// a fixed handful of rows however many source rows one destination row
/// draws from, so a thumbnail of a 4K photograph does not ask for a
/// row cache proportional to the reduction.
const ROW_SLOTS: usize = 8;

/// Why a resample was refused.
///
/// Every variant is a fail-closed refusal of geometry the caller got wrong;
/// no input produces a partially-written destination.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum ResampleError {
    /// The source image's declared size does not match its pixel bytes.
    SourceSizeMismatch,
    /// The source region is empty, or reaches outside the source image.
    SourceRegionOutOfBounds,
    /// The destination extent has a zero side.
    EmptyDestination,
    /// The requested band is empty, or runs past the destination's last row.
    BandOutOfBounds,
    /// The output buffer is not exactly `rows * destination width * 4` bytes.
    OutputSizeMismatch,
    /// The destination pixel count does not fit in memory on this target.
    DestinationTooLarge,
}

/// A borrowed straight-alpha RGBA8 image: the shape
/// `tairix_image::RasterImage` carries and the shape a sandboxed decoder
/// returns.
///
/// Constructed only through [`new`](Self::new), which checks the pixel
/// buffer against the declared geometry, so every method here can index
/// within that geometry without re-validating it.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct Rgba8Image<'a> {
    width: u32,
    height: u32,
    pixels: &'a [u8],
}

impl<'a> Rgba8Image<'a> {
    /// Borrow `pixels` as a `width`×`height` straight-alpha RGBA8 image.
    ///
    /// # Errors
    ///
    /// [`ResampleError::SourceSizeMismatch`] when `pixels` is not exactly
    /// `width * height * 4` bytes, including the degenerate zero-side case.
    pub fn new(width: u32, height: u32, pixels: &'a [u8]) -> Result<Self, ResampleError> {
        if width == 0 || height == 0 {
            return Err(ResampleError::SourceSizeMismatch);
        }
        let expected = pixel_bytes(width, height).ok_or(ResampleError::SourceSizeMismatch)?;
        if pixels.len() != expected {
            return Err(ResampleError::SourceSizeMismatch);
        }
        Ok(Self {
            width,
            height,
            pixels,
        })
    }

    /// The image width in pixels.
    #[must_use]
    pub const fn width(&self) -> u32 {
        self.width
    }

    /// The image height in pixels.
    #[must_use]
    pub const fn height(&self) -> u32 {
        self.height
    }

    /// The whole image as a source region.
    #[must_use]
    pub const fn whole(&self) -> Region {
        Region {
            x: 0,
            y: 0,
            width: self.width,
            height: self.height,
        }
    }

    /// Row `y` as whole pixels, or `None` when the row does not lie in the
    /// image — unreachable for a row a validated plan asks for, but checked
    /// rather than assumed.
    fn row(&self, y: u32) -> Option<&'a [[u8; CHANNELS]]> {
        let start = pixel_offset(0, y, self.width)?;
        let len = pixel_bytes(self.width, 1)?;
        let bytes = self.pixels.get(start..start.checked_add(len)?)?;
        Some(bytes.as_chunks::<CHANNELS>().0)
    }
}

/// A rectangular region of a source image, in source pixels.
///
/// Unsigned by construction because a region of an image can never begin
/// outside it; a resample validates that it also never *ends* outside it.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct Region {
    /// Leftmost source column.
    pub x: u32,
    /// Topmost source row.
    pub y: u32,
    /// Width in source pixels; never zero for an accepted region.
    pub width: u32,
    /// Height in source pixels; never zero for an accepted region.
    pub height: u32,
}

/// Resample `region` of `src` into a whole `dest_width`×`dest_height`
/// straight-alpha RGBA8 image.
///
/// # Errors
///
/// Every [`ResampleError`] [`resample_rows`] can raise, plus
/// [`ResampleError::DestinationTooLarge`] when the destination buffer could
/// not be allocated on this target.
pub fn resample(
    src: &Rgba8Image<'_>,
    region: Region,
    dest_width: u32,
    dest_height: u32,
) -> Result<Vec<u8>, ResampleError> {
    let len = pixel_bytes(dest_width, dest_height).ok_or(ResampleError::DestinationTooLarge)?;
    let mut out = vec![0u8; len];
    resample_rows(
        src,
        region,
        dest_width,
        dest_height,
        0,
        dest_height,
        &mut out,
    )?;
    Ok(out)
}

/// Resample `region` of `src` into destination rows `first_row` through
/// `first_row + rows`, of a destination that is `dest_width`×`dest_height`
/// in full, writing exactly `rows * dest_width * 4` bytes into `out`.
///
/// The band is computed from the source and the filter plan alone, so bands
/// are independent: producing a destination one band at a time yields
/// byte-for-byte the same image as producing it in one call.
///
/// # Errors
///
/// - [`ResampleError::SourceRegionOutOfBounds`] — an empty region, or one
///   reaching past the source image.
/// - [`ResampleError::EmptyDestination`] — a zero-sided destination.
/// - [`ResampleError::BandOutOfBounds`] — an empty band, or one running past
///   the destination's last row.
/// - [`ResampleError::OutputSizeMismatch`] — a mis-sized `out`.
pub fn resample_rows(
    src: &Rgba8Image<'_>,
    region: Region,
    dest_width: u32,
    dest_height: u32,
    first_row: u32,
    rows: u32,
    out: &mut [u8],
) -> Result<(), ResampleError> {
    validate_region(src, region)?;
    if dest_width == 0 || dest_height == 0 {
        return Err(ResampleError::EmptyDestination);
    }
    let last = first_row
        .checked_add(rows)
        .ok_or(ResampleError::BandOutOfBounds)?;
    if rows == 0 || last > dest_height {
        return Err(ResampleError::BandOutOfBounds);
    }
    let expected = pixel_bytes(dest_width, rows).ok_or(ResampleError::OutputSizeMismatch)?;
    if out.len() != expected {
        return Err(ResampleError::OutputSizeMismatch);
    }

    let columns = Axis::plan(region.x, region.width, dest_width);
    let rows_plan = Axis::plan(region.y, region.height, dest_height);
    let mut cache = RowCache::new(dest_width, rows_plan.taps);
    let mut accumulator = vec![0i64; samples_per_row(dest_width)];
    let row_bytes = expected / rows_as_usize(rows);
    for (dest_y, chunk) in (first_row..last).zip(out.chunks_exact_mut(row_bytes)) {
        accumulator.fill(0);
        for tap in 0..rows_plan.taps {
            let (weight, source_row) = rows_plan.tap(dest_y, tap);
            if weight == 0 {
                continue;
            }
            let filtered = cache.filtered_row(src, &columns, source_row);
            for (slot, &value) in accumulator.iter_mut().zip(filtered) {
                *slot += i64::from(weight) * i64::from(value);
            }
        }
        write_row(&accumulator, chunk);
    }
    Ok(())
}

/// Check that `region` is non-empty and lies wholly inside `src`.
fn validate_region(src: &Rgba8Image<'_>, region: Region) -> Result<(), ResampleError> {
    let right = region
        .x
        .checked_add(region.width)
        .ok_or(ResampleError::SourceRegionOutOfBounds)?;
    let bottom = region
        .y
        .checked_add(region.height)
        .ok_or(ResampleError::SourceRegionOutOfBounds)?;
    if region.width == 0 || region.height == 0 || right > src.width || bottom > src.height {
        return Err(ResampleError::SourceRegionOutOfBounds);
    }
    Ok(())
}

/// One axis of a resample, fully resolved: for each destination sample, the
/// source samples it reads and the weight of each.
///
/// Resolving the geometry up front is what keeps the accumulation loops
/// pure arithmetic — no division and no branch on the direction of the
/// resample once a row is being filtered — and the table is small: a
/// destination sample's tap count is about the ratio between the extents,
/// so the whole plan is on the order of the source extent plus the
/// destination extent regardless of how extreme that ratio is.
struct Axis {
    /// Local index of each destination sample's first tap. Signed because a
    /// cubic reconstruction reaches one sample past the region's leading
    /// edge, where the edge sample stands in.
    first: Vec<i64>,
    /// Fixed-point weight per tap, destination-major, summing to
    /// [`WEIGHT_ONE`] across each destination sample.
    weights: Vec<i32>,
    /// Taps per destination sample. Samples needing fewer carry zero
    /// weights, so one stride serves the whole axis.
    taps: usize,
    /// First source sample of the region this axis samples.
    origin: u32,
    /// Source samples in the region this axis samples.
    extent: u32,
}

impl Axis {
    /// Plan `dest_extent` destination samples over the `extent` source
    /// samples starting at `origin`.
    fn plan(origin: u32, extent: u32, dest_extent: u32) -> Self {
        let reducing = extent > dest_extent;
        let taps = if reducing {
            area_taps(extent, dest_extent)
        } else {
            CUBIC_TAPS
        };
        let dest = dest_extent as usize;
        let mut first = vec![0i64; dest];
        let mut weights = vec![0i32; dest.saturating_mul(taps)];
        for dest_sample in 0..dest_extent {
            let span = dest_sample as usize * taps;
            let Some(row) = weights.get_mut(span..span + taps) else {
                continue;
            };
            let (start, written) = if reducing {
                area_weights(dest_sample, extent, dest_extent, row)
            } else {
                cubic_weights(dest_sample, extent, dest_extent, row)
            };
            if let Some(slot) = row.get_mut(..written.max(1)) {
                normalise(slot);
            }
            if let Some(slot) = first.get_mut(dest_sample as usize) {
                *slot = start;
            }
        }
        Self {
            first,
            weights,
            taps,
            origin,
            extent,
        }
    }

    /// Tap `tap` of destination sample `dest_sample`: its weight and the
    /// absolute source sample it reads.
    fn tap(&self, dest_sample: u32, tap: usize) -> (i32, u32) {
        let index = (dest_sample as usize)
            .saturating_mul(self.taps)
            .saturating_add(tap);
        let weight = self.weights.get(index).copied().unwrap_or(0);
        let local = self
            .first
            .get(dest_sample as usize)
            .copied()
            .unwrap_or(0)
            .saturating_add(i64::try_from(tap).unwrap_or(i64::MAX));
        (weight, absolute_sample(local, self.origin, self.extent))
    }
}

/// The most source samples one destination sample reads while reducing an
/// `extent`-sample axis to `dest_extent` samples: the samples its footprint
/// spans, plus the one partly-covered sample at each end.
fn area_taps(extent: u32, dest_extent: u32) -> usize {
    let ratio = u64::from(extent).div_ceil(u64::from(dest_extent).max(1));
    usize::try_from(ratio.saturating_add(1)).unwrap_or(usize::MAX)
}

/// Weights for destination sample `dest_sample` as the exact area integral
/// of the source over the footprint it covers, returning the first local
/// source index and how many taps carry weight.
///
/// The footprint of destination sample `d` is the source interval
/// `[d * extent / dest_extent, (d + 1) * extent / dest_extent)`, and a
/// source sample's weight is how much of its own unit interval that
/// footprint covers. Working in units of `1 / dest_extent` keeps every
/// overlap an exact integer, so the weights of a footprint sum to `extent`
/// of those units — the whole footprint — with nothing lost to rounding
/// before the fixed-point conversion.
fn area_weights(dest_sample: u32, extent: u32, dest_extent: u32, out: &mut [i32]) -> (i64, usize) {
    let extent_wide = i128::from(extent);
    let dest_wide = i128::from(dest_extent);
    let low = i128::from(dest_sample) * extent_wide;
    let high = low + extent_wide;
    let first = low / dest_wide;
    let mut written = 0;
    for (tap, slot) in out.iter_mut().enumerate() {
        let sample = first + i128::try_from(tap).unwrap_or(i128::MAX);
        let sample_low = sample * dest_wide;
        let sample_high = sample_low + dest_wide;
        let overlap = high.min(sample_high) - low.max(sample_low);
        if overlap <= 0 {
            *slot = 0;
            continue;
        }
        // `overlap <= extent`, so the quotient is at most `WEIGHT_ONE`.
        *slot = i32::try_from(overlap * WEIGHT_ONE_WIDE / extent_wide).unwrap_or(WEIGHT_ONE);
        written = tap + 1;
    }
    (i64::try_from(first).unwrap_or(i64::MAX), written)
}

/// Weights for destination sample `dest_sample` as the Catmull-Rom cubic
/// through the source samples, returning the first local source index and
/// the tap count.
///
/// The destination sample's centre sits at
/// `(dest_sample + 0.5) * extent / dest_extent - 0.5` in source samples —
/// pixel centres against pixel centres, which is what keeps a 1:1 resample
/// an exact copy instead of a half-pixel shift. Carried as a rational in
/// halves of a source sample so the floor and the fraction are both exact.
fn cubic_weights(dest_sample: u32, extent: u32, dest_extent: u32, out: &mut [i32]) -> (i64, usize) {
    let denominator = 2 * i128::from(dest_extent);
    let numerator =
        (2 * i128::from(dest_sample) + 1) * i128::from(extent) - i128::from(dest_extent);
    let floor = numerator.div_euclid(denominator);
    let fraction = numerator.rem_euclid(denominator);
    let one = 1i128 << CUBIC_SHIFT;
    let t = (fraction << CUBIC_SHIFT) / denominator;
    let t2 = (t * t) >> CUBIC_SHIFT;
    let t3 = (t2 * t) >> CUBIC_SHIFT;
    // The Catmull-Rom basis, doubled to keep its halves integral. The four
    // rows sum to `2 * one` for every `t`, so the weights sum to one.
    let doubled = [
        -t3 + 2 * t2 - t,
        3 * t3 - 5 * t2 + 2 * one,
        -3 * t3 + 4 * t2 + t,
        t3 - t2,
    ];
    for (slot, value) in out.iter_mut().zip(doubled) {
        *slot = i32::try_from(value * WEIGHT_ONE_WIDE / (2 * one)).unwrap_or(0);
    }
    let first = i64::try_from(floor).unwrap_or(i64::MAX).saturating_sub(1);
    (first, CUBIC_TAPS)
}

/// Adjust `weights` so they sum to exactly [`WEIGHT_ONE`].
///
/// The residual is at most one unit per tap and lands on the largest
/// weight, where it cannot change the result's rounding. Exactness matters:
/// weights that summed to slightly less than one would darken every
/// destination pixel by the same fraction, which a large flat region shows
/// as banding rather than as noise.
fn normalise(weights: &mut [i32]) {
    let sum: i32 = weights.iter().copied().sum();
    let residual = WEIGHT_ONE - sum;
    if residual == 0 {
        return;
    }
    let Some(largest) = weights
        .iter()
        .enumerate()
        .max_by_key(|(_, &weight)| weight)
        .map(|(index, _)| index)
    else {
        return;
    };
    if let Some(slot) = weights.get_mut(largest) {
        *slot += residual;
    }
}

/// The absolute source sample a local index names, holding the edge sample
/// where the filter reaches outside the region.
///
/// Extending the edge is what a resample must do at a boundary: a cubic
/// reconstruction reads a sample either side of its footprint, and the
/// outermost destination samples have no such neighbour. Repeating the edge
/// sample is the neutral answer — it neither invents detail nor darkens the
/// border the way treating the outside as transparent black would.
fn absolute_sample(local: i64, origin: u32, extent: u32) -> u32 {
    let last = i64::from(extent) - 1;
    let clamped = local.clamp(0, last.max(0));
    let index = u64::from(origin).saturating_add(clamped.unsigned_abs());
    u32::try_from(index).unwrap_or(u32::MAX)
}

/// Horizontally-filtered source rows, held for reuse across the
/// destination rows that share them.
///
/// A destination row draws from a run of source rows, and the next
/// destination row's run overlaps it. Filtering a source row is the
/// expensive half of a separable resample, so a row already filtered is
/// answered from here rather than filtered again. The cache is a fixed
/// [`ROW_SLOTS`] rows deep and addressed by the source row itself, so it
/// holds the most recently filtered rows — exactly the ones an overlap
/// asks for — without any eviction bookkeeping.
struct RowCache {
    /// `ROW_SLOTS` filtered rows, each `dest_width` premultiplied samples.
    rows: Vec<i32>,
    /// The source row each slot holds, once it holds one.
    held: Vec<Option<u32>>,
    /// Samples per filtered row.
    stride: usize,
}

impl RowCache {
    /// A cache for `dest_width`-wide rows, deep enough for a destination
    /// row that reads `taps` source rows but never deeper than
    /// [`ROW_SLOTS`].
    fn new(dest_width: u32, taps: usize) -> Self {
        let slots = taps.clamp(1, ROW_SLOTS);
        let stride = samples_per_row(dest_width);
        Self {
            rows: vec![0i32; slots.saturating_mul(stride)],
            held: vec![None; slots],
            stride,
        }
    }

    /// Source row `source_row` of `src`, filtered along `columns`.
    fn filtered_row(&mut self, src: &Rgba8Image<'_>, columns: &Axis, source_row: u32) -> &[i32] {
        let slot = (source_row as usize) % self.held.len().max(1);
        let start = slot * self.stride;
        let end = start + self.stride;
        let fresh = self.held.get(slot).copied().flatten() != Some(source_row);
        if fresh {
            if let Some(row) = self.rows.get_mut(start..end) {
                filter_row(src, columns, source_row, row);
            }
            if let Some(held) = self.held.get_mut(slot) {
                *held = Some(source_row);
            }
        }
        self.rows.get(start..end).unwrap_or(&[])
    }
}

/// Filter source row `source_row` along `columns` into `out`, as
/// premultiplied `[red, green, blue, alpha]` samples per destination
/// column.
///
/// Colour is carried as `channel * alpha` and alpha as `alpha * 255`, both
/// in the same scale, so dividing the first by the second at write-out
/// recovers the alpha-weighted mean colour exactly. A row outside the image
/// — unreachable for a row a validated plan names — contributes nothing.
fn filter_row(src: &Rgba8Image<'_>, columns: &Axis, source_row: u32, out: &mut [i32]) {
    let Some(pixels) = src.row(source_row) else {
        out.fill(0);
        return;
    };
    // A filtered row is a whole number of samples by construction, so the
    // ragged tail this splits off is always empty.
    let (samples, _tail) = out.as_chunks_mut::<CHANNELS>();
    for (dest_x, slot) in samples.iter_mut().enumerate() {
        let dest_x = u32::try_from(dest_x).unwrap_or(u32::MAX);
        let mut sums = [0i64; CHANNELS];
        for tap in 0..columns.taps {
            let (weight, source_x) = columns.tap(dest_x, tap);
            if weight == 0 {
                continue;
            }
            let Some(pixel) = pixels.get(source_x as usize) else {
                continue;
            };
            let weight = i64::from(weight);
            let alpha = i64::from(pixel[3]);
            sums[0] += weight * i64::from(pixel[0]) * alpha;
            sums[1] += weight * i64::from(pixel[1]) * alpha;
            sums[2] += weight * i64::from(pixel[2]) * alpha;
            sums[3] += weight * alpha * 255;
        }
        for (channel, sum) in slot.iter_mut().zip(sums) {
            *channel = i32::try_from(descale(sum)).unwrap_or(i32::MAX);
        }
    }
}

/// Write one destination row from its accumulated premultiplied samples.
///
/// The colour channels and the alpha were filtered in the same scale, so
/// the straight-alpha colour is the ratio between them. A footprint that
/// accumulated no alpha has no colour to report.
///
/// The cubic's negative lobes can carry a filtered sample past the range
/// its neighbours occupy — the overshoot that makes an interpolating filter
/// look sharp — and each channel is therefore held to what a byte can say
/// *after* the division, never before it. Dividing by an alpha that had
/// already been clipped would scale the colour by the clipping: an opaque
/// pixel beside a transparent one would come back a different colour than
/// it went in, which is exactly the bleed premultiplied filtering exists to
/// prevent.
///
/// Both divisions round to nearest rather than truncating. Truncation would
/// bias every channel of every pixel down by half a level, which is a
/// systematic darkening of the whole image rather than noise.
fn write_row(accumulator: &[i64], out: &mut [u8]) {
    // A row is a whole number of pixels by construction, so the ragged
    // tail this splits off is always empty.
    let (pixels, _tail) = out.as_chunks_mut::<CHANNELS>();
    let (accumulated, _rest) = accumulator.as_chunks::<CHANNELS>();
    for (samples, pixel) in accumulated.iter().zip(pixels) {
        let alpha_scaled = descale(samples[3]);
        if alpha_scaled <= 0 {
            *pixel = [0, 0, 0, 0];
            continue;
        }
        for (channel, sample) in pixel.iter_mut().zip(samples).take(CHANNELS - 1) {
            let premultiplied = descale(*sample).max(0);
            let scaled = premultiplied.saturating_mul(255) + alpha_scaled / 2;
            let straight = scaled / alpha_scaled;
            *channel = u8::try_from(straight.clamp(0, 255)).unwrap_or(u8::MAX);
        }
        let alpha = (alpha_scaled + 127) / 255;
        pixel[3] = u8::try_from(alpha.clamp(0, 255)).unwrap_or(u8::MAX);
    }
}

/// Remove one [`WEIGHT_SHIFT`] of fixed-point scale, rounding to nearest.
fn descale(value: i64) -> i64 {
    (value + (1 << (WEIGHT_SHIFT - 1))) >> WEIGHT_SHIFT
}

/// Filtered samples in one `width`-pixel row.
fn samples_per_row(width: u32) -> usize {
    (width as usize).saturating_mul(CHANNELS)
}

/// The byte length of a `width`×`height` RGBA8 buffer, or `None` when it
/// does not fit in a `usize` on this target.
fn pixel_bytes(width: u32, height: u32) -> Option<usize> {
    let bytes = u64::from(width)
        .checked_mul(u64::from(height))?
        .checked_mul(CHANNELS_U64)?;
    usize::try_from(bytes).ok()
}

/// Byte offset of pixel `(x, y)` in a row-major RGBA8 buffer `width` pixels
/// wide, or `None` when it would not fit a `usize` — unreachable for a
/// buffer that exists, but checked rather than assumed.
fn pixel_offset(x: u32, y: u32, width: u32) -> Option<usize> {
    let index = u64::from(y)
        .checked_mul(u64::from(width))?
        .checked_add(u64::from(x))?;
    usize::try_from(index.checked_mul(CHANNELS_U64)?).ok()
}

/// A validated band height as a `usize` divisor. The caller has already
/// matched `out` against `rows * dest_width * 4`, so a band of zero rows
/// cannot reach here; the floor of one keeps the division total.
fn rows_as_usize(rows: u32) -> usize {
    usize::try_from(rows).unwrap_or(usize::MAX).max(1)
}

#[cfg(test)]
#[path = "resample_tests.rs"]
mod tests;
