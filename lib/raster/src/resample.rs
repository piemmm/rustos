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
//! Colour is filtered **premultiplied**, because that is the only correct way
//! to filter an image with an alpha channel: a fully transparent source pixel
//! contributes its transparency but not its (meaningless) colour, so scaling
//! artwork with transparent padding cannot drag that padding's colour into the
//! visible edge. A destination pixel whose whole footprint is transparent has
//! no colour to report and yields transparent black.
//!
//! # Alpha spaces
//!
//! Both spaces a desktop actually holds pixels in are resampled by the one
//! filter above, and each is read and written in its own space:
//!
//! - **Straight-alpha RGBA8** ([`resample`], [`resample_rows`]) is the
//!   interchange form a decoder produces. Its samples are premultiplied on
//!   the way in and divided back out on the way out.
//! - **Premultiplied [`Pixel`]s** ([`crate::Surface::resampled`]) are what
//!   every [`crate::Surface`] already carries, so that path multiplies and
//!   divides nothing: it filters the stored channels as they are. Scaling a
//!   window's frame to a picker thumbnail therefore costs one allocation and
//!   one filter pass, where routing it through the straight-alpha entry point
//!   would copy and convert the whole frame twice over.
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
//! does not lie inside its image, a mis-sized output buffer, a band that
//! runs past the destination, and a buffer the allocator refuses all return
//! a typed refusal rather than panicking or writing a partial result.

use alloc::vec::Vec;

use tairix_util::fallible;

use crate::color::Pixel;

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
/// Every variant but [`OutOfMemory`](Self::OutOfMemory) is a fail-closed
/// refusal of geometry the caller got wrong; no input, and no refusal,
/// produces a partially-written destination.
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
    /// The destination's byte count does not fit `usize` on this target, so
    /// no buffer could ever describe it.
    DestinationTooLarge,
    /// A buffer this resample needs — the destination, or one of the
    /// filter's working buffers — was refused by the allocator. Unlike every
    /// other variant this is a property of the machine, not of the request,
    /// so the same call may be granted later.
    OutOfMemory,
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

/// The alpha space a resample reads and writes, and the two kernels that
/// differ with it: how a source sample enters the filter, and how a filtered
/// sample is encoded back.
///
/// Everything else — the plan, the row cache, the band loop, the geometry
/// validation — is shared, so the two spaces cannot drift apart in anything
/// but the arithmetic that genuinely differs between them.
trait Space {
    /// One pixel as this space stores it.
    type Sample: Copy;

    /// Add `sample`, weighted by the fixed-point `weight`, into `sums`.
    ///
    /// The four sums are in whatever internal scale [`encode`](Self::encode)
    /// expects; they are never read outside this trait's own pair.
    fn weigh(sample: Self::Sample, weight: i64, sums: &mut [i64; CHANNELS]);

    /// The sample the accumulated `channels` encode, carrying one
    /// [`WEIGHT_SHIFT`] of scale each.
    fn encode(channels: [i64; CHANNELS]) -> Self::Sample;
}

/// A borrowed source image of one space, addressed by row.
trait Rows {
    /// The space this image's samples are in.
    type Space: Space;

    /// The image width in samples.
    fn width(&self) -> u32;

    /// The image height in rows.
    fn height(&self) -> u32;

    /// Row `y` as whole samples, or `None` when the row does not lie in the
    /// image.
    fn row(&self, y: u32) -> Option<&[<Self::Space as Space>::Sample]>;
}

/// Straight-alpha RGBA8, where a sample's colour still has to be weighted by
/// its own alpha before it can be filtered.
struct Straight;

/// Premultiplied [`Pixel`]s, where the stored channels are already
/// alpha-weighted and are filtered exactly as they are.
struct Premultiplied;

impl Space for Straight {
    type Sample = [u8; CHANNELS];

    /// Colour is carried as `channel * alpha` and alpha as `alpha * 255`,
    /// both in the same scale, so dividing the first by the second at
    /// encode time recovers the alpha-weighted mean colour exactly.
    fn weigh(sample: [u8; CHANNELS], weight: i64, sums: &mut [i64; CHANNELS]) {
        let weighted_alpha = weight * i64::from(sample[3]);
        sums[0] += weighted_alpha * i64::from(sample[0]);
        sums[1] += weighted_alpha * i64::from(sample[1]);
        sums[2] += weighted_alpha * i64::from(sample[2]);
        sums[3] += weighted_alpha * 255;
    }

    /// The colour channels and the alpha were filtered in the same scale, so
    /// the straight-alpha colour is the ratio between them. A footprint that
    /// accumulated no alpha has no colour to report.
    ///
    /// The cubic's negative lobes can carry a filtered sample past the range
    /// its neighbours occupy — the overshoot that makes an interpolating
    /// filter look sharp — and each channel is therefore held to what a byte
    /// can say *after* the division, never before it. Dividing by an alpha
    /// that had already been clipped would scale the colour by the clipping:
    /// an opaque pixel beside a transparent one would come back a different
    /// colour than it went in, which is exactly the bleed premultiplied
    /// filtering exists to prevent.
    ///
    /// Both divisions round to nearest rather than truncating. Truncation
    /// would bias every channel of every pixel down by half a level, which is
    /// a systematic darkening of the whole image rather than noise.
    fn encode(channels: [i64; CHANNELS]) -> [u8; CHANNELS] {
        // A descaled sample is a weighted mean of `channel * alpha` over
        // weights summing to one, so it cannot exceed `255 * 255`, and the
        // arithmetic below stays inside a 32-bit word. Saying so lets the
        // divisions be 32-bit, which is worth stating because there are
        // three of them for every pixel of every resampled image.
        let alpha_scaled = narrow(descale(channels[3]));
        if alpha_scaled <= 0 {
            return [0, 0, 0, 0];
        }
        let mut out = [0u8; CHANNELS];
        for (channel, sample) in out.iter_mut().zip(channels).take(CHANNELS - 1) {
            let premultiplied = narrow(descale(sample)).max(0);
            let scaled = premultiplied.saturating_mul(255) + alpha_scaled / 2;
            *channel = clamp_u8(scaled / alpha_scaled);
        }
        out[3] = clamp_u8((alpha_scaled + 127) / 255);
        out
    }
}

impl Space for Premultiplied {
    type Sample = Pixel;

    /// Already alpha-weighted, so the filter weights the stored channels
    /// directly rather than reconstructing a straight colour it would only
    /// premultiply again.
    ///
    /// Every channel is carried in the same `× 255` scale the straight-alpha
    /// space uses, so both spaces keep the same headroom under the
    /// fixed-point descale and an opaque image resamples identically either
    /// way — a sample carried at its plain byte scale would lose up to one
    /// level per axis to the intermediate rounding.
    fn weigh(sample: Pixel, weight: i64, sums: &mut [i64; CHANNELS]) {
        let scaled = weight * 255;
        sums[0] += scaled * i64::from(sample.r);
        sums[1] += scaled * i64::from(sample.g);
        sums[2] += scaled * i64::from(sample.b);
        sums[3] += scaled * i64::from(sample.a);
    }

    /// Each channel is its own weighted mean, so encoding drops the carried
    /// scale and nothing else — no division by alpha, and no colour to
    /// recover.
    ///
    /// A colour channel is held to the alpha rather than to 255: a
    /// premultiplied channel above its own alpha is not a representable
    /// pixel, and the cubic's negative lobes can overshoot into that range
    /// on an enlargement.
    fn encode(channels: [i64; CHANNELS]) -> Pixel {
        let level = |sample: i64| (narrow(descale(sample)).max(0) + 127) / 255;
        let alpha = clamp_u8(level(channels[3]));
        let ceiling = i32::from(alpha);
        let channel = |sample: i64| {
            #[allow(
                clippy::cast_possible_truncation,
                clippy::cast_sign_loss,
                reason = "`level` floors at zero and `ceiling` is a `u8`, leaving exactly what a `u8` holds"
            )]
            let clamped = level(sample).min(ceiling) as u8;
            clamped
        };
        Pixel {
            r: channel(channels[0]),
            g: channel(channels[1]),
            b: channel(channels[2]),
            a: alpha,
        }
    }
}

impl Rows for Rgba8Image<'_> {
    type Space = Straight;

    fn width(&self) -> u32 {
        self.width
    }

    fn height(&self) -> u32 {
        self.height
    }

    fn row(&self, y: u32) -> Option<&[[u8; CHANNELS]]> {
        let start = pixel_offset(0, y, self.width)?;
        let len = pixel_bytes(self.width, 1)?;
        let bytes = self.pixels.get(start..start.checked_add(len)?)?;
        Some(bytes.as_chunks::<CHANNELS>().0)
    }
}

/// A borrowed premultiplied image: one [`crate::Surface`]'s own pixels.
struct PixelImage<'a> {
    width: u32,
    height: u32,
    pixels: &'a [Pixel],
}

impl Rows for PixelImage<'_> {
    type Space = Premultiplied;

    fn width(&self) -> u32 {
        self.width
    }

    fn height(&self) -> u32 {
        self.height
    }

    fn row(&self, y: u32) -> Option<&[Pixel]> {
        let start = sample_offset(y, self.width)?;
        let len = sample_count(self.width, 1)?;
        self.pixels.get(start..start.checked_add(len)?)
    }
}

/// Resample `region` of `src` into a whole `dest_width`×`dest_height`
/// straight-alpha RGBA8 image.
///
/// # Errors
///
/// Every [`ResampleError`] [`resample_rows`] can raise, plus
/// [`ResampleError::DestinationTooLarge`] when the destination's byte count
/// is unrepresentable on this target, and [`ResampleError::OutOfMemory`]
/// when the allocator refuses the buffer.
pub fn resample(
    src: &Rgba8Image<'_>,
    region: Region,
    dest_width: u32,
    dest_height: u32,
) -> Result<Vec<u8>, ResampleError> {
    let len = pixel_bytes(dest_width, dest_height).ok_or(ResampleError::DestinationTooLarge)?;
    let mut out = fallible::filled(len, 0u8).ok_or(ResampleError::OutOfMemory)?;
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
    let band = Band {
        first_row,
        rows,
        dest_width,
        dest_height,
    };
    let samples = validate(src, region, band)?;
    let expected = samples
        .checked_mul(CHANNELS)
        .ok_or(ResampleError::OutputSizeMismatch)?;
    if out.len() != expected {
        return Err(ResampleError::OutputSizeMismatch);
    }
    // A validated byte length is a whole number of samples, so the ragged
    // tail this splits off is always empty.
    let (quads, _tail) = out.as_chunks_mut::<CHANNELS>();
    filter_band(src, region, band, quads)
}

/// Resample `region` of the `width`×`height` premultiplied image `src` into
/// the whole `dest_width`×`dest_height` premultiplied image `out`.
///
/// The pixel-space counterpart of [`resample_rows`], reached through
/// [`Surface::resampled`](crate::Surface::resampled) — the only caller that
/// can hold both buffers.
///
/// # Errors
///
/// As [`resample_rows`], with [`ResampleError::SourceSizeMismatch`] for a
/// `src` whose length does not match its declared geometry and
/// [`ResampleError::OutputSizeMismatch`] for a mis-sized `out`.
pub(crate) fn resample_pixels(
    src: (u32, u32, &[Pixel]),
    region: Region,
    dest: (u32, u32),
    out: &mut [Pixel],
) -> Result<(), ResampleError> {
    let (width, height, pixels) = src;
    if width == 0 || height == 0 {
        return Err(ResampleError::SourceSizeMismatch);
    }
    if pixels.len() != sample_count(width, height).ok_or(ResampleError::SourceSizeMismatch)? {
        return Err(ResampleError::SourceSizeMismatch);
    }
    let image = PixelImage {
        width,
        height,
        pixels,
    };
    let (dest_width, dest_height) = dest;
    let band = Band {
        first_row: 0,
        rows: dest_height,
        dest_width,
        dest_height,
    };
    let samples = validate(&image, region, band)?;
    if out.len() != samples {
        return Err(ResampleError::OutputSizeMismatch);
    }
    filter_band(&image, region, band, out)
}

/// One contiguous run of destination rows, of a destination of a stated size.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
struct Band {
    first_row: u32,
    rows: u32,
    dest_width: u32,
    dest_height: u32,
}

impl Band {
    /// The destination row after the last this band writes, or `None` when
    /// the sum does not fit.
    fn last_row(self) -> Option<u32> {
        self.first_row.checked_add(self.rows)
    }
}

/// Check `region` against `src` and `band` against its destination,
/// answering how many samples the band writes.
///
/// The one validation both entry points share, so neither can accept
/// geometry the other refuses; each then checks its own buffer's length in
/// its own units.
fn validate<R: Rows>(src: &R, region: Region, band: Band) -> Result<usize, ResampleError> {
    validate_region(src, region)?;
    if band.dest_width == 0 || band.dest_height == 0 {
        return Err(ResampleError::EmptyDestination);
    }
    let last = band.last_row().ok_or(ResampleError::BandOutOfBounds)?;
    if band.rows == 0 || last > band.dest_height {
        return Err(ResampleError::BandOutOfBounds);
    }
    sample_count(band.dest_width, band.rows).ok_or(ResampleError::OutputSizeMismatch)
}

/// Filter `band` of `region` into `out`, which [`validate`] has already
/// matched against the band's own geometry.
///
/// # Errors
///
/// [`ResampleError::OutOfMemory`] when the allocator refuses one of the
/// plan or scratch buffers, before any of `out` is written.
fn filter_band<R: Rows>(
    src: &R,
    region: Region,
    band: Band,
    out: &mut [<R::Space as Space>::Sample],
) -> Result<(), ResampleError> {
    let columns =
        Axis::plan(region.x, region.width, band.dest_width).ok_or(ResampleError::OutOfMemory)?;
    let rows_plan =
        Axis::plan(region.y, region.height, band.dest_height).ok_or(ResampleError::OutOfMemory)?;
    if columns.is_identity() && rows_plan.is_identity() {
        copy_rows(src, region, band, out);
        return Ok(());
    }
    let mut cache =
        RowCache::new(band.dest_width, rows_plan.stride).ok_or(ResampleError::OutOfMemory)?;
    let mut accumulator = fallible::filled(samples_per_row(band.dest_width), 0i64)
        .ok_or(ResampleError::OutOfMemory)?;
    let width = band.dest_width as usize;
    let last = band.last_row().unwrap_or(band.dest_height);
    for (dest_y, chunk) in (band.first_row..last).zip(out.chunks_exact_mut(width.max(1))) {
        // The first contributing tap *writes* the accumulator and the rest
        // add to it, so a destination row never pays to clear a buffer it
        // is about to overwrite — which for a resample near 1:1, where a
        // row has a single tap, is the whole of the accumulation cost.
        let mut started = false;
        for tap in rows_plan.taps_of(dest_y as usize) {
            if tap.weight == 0 {
                continue;
            }
            let weight = i64::from(tap.weight);
            let filtered = cache.filtered_row(src, &columns, tap.source);
            if started {
                for (slot, &value) in accumulator.iter_mut().zip(filtered) {
                    *slot += weight * i64::from(value);
                }
            } else {
                for (slot, &value) in accumulator.iter_mut().zip(filtered) {
                    *slot = weight * i64::from(value);
                }
                started = true;
            }
        }
        if !started {
            accumulator.fill(0);
        }
        write_row::<R::Space>(&accumulator, chunk);
    }
    Ok(())
}

/// Copy `region` of `src` straight into `band`'s destination rows.
///
/// A resample whose two axis plans are both the identity *is* a copy:
/// every destination sample draws one source sample at full weight, so the
/// filter would premultiply each channel by its alpha and then divide it
/// back out to reproduce the byte it started from. That case is not
/// exotic on a desktop — an image decoded at exactly the size it will be
/// drawn hits it, and a decode is asked for the size the composition
/// wants — and paying a full separable filter to change nothing is work
/// worth not doing.
///
/// A source row the region names but the image does not hold is
/// unreachable for a validated region; it leaves the destination row
/// transparent rather than reporting samples it did not read.
fn copy_rows<R: Rows>(
    src: &R,
    region: Region,
    band: Band,
    out: &mut [<R::Space as Space>::Sample],
) {
    let left = region.x as usize;
    let right = left.saturating_add(region.width as usize);
    let last = band.last_row().unwrap_or(band.dest_height);
    let blank = <R::Space as Space>::encode([0; CHANNELS]);
    for (dest_y, chunk) in
        (band.first_row..last).zip(out.chunks_exact_mut((region.width as usize).max(1)))
    {
        let source_y = region.y.saturating_add(dest_y);
        match src.row(source_y).and_then(|row| row.get(left..right)) {
            Some(span) => chunk.copy_from_slice(span),
            None => chunk.fill(blank),
        }
    }
}

/// Check that `region` is non-empty and lies wholly inside `src`.
fn validate_region<R: Rows>(src: &R, region: Region) -> Result<(), ResampleError> {
    let right = region
        .x
        .checked_add(region.width)
        .ok_or(ResampleError::SourceRegionOutOfBounds)?;
    let bottom = region
        .y
        .checked_add(region.height)
        .ok_or(ResampleError::SourceRegionOutOfBounds)?;
    if region.width == 0 || region.height == 0 || right > src.width() || bottom > src.height() {
        return Err(ResampleError::SourceRegionOutOfBounds);
    }
    Ok(())
}

/// One tap of a resample: the source sample it reads and the fixed-point
/// weight it reads it with.
#[derive(Copy, Clone)]
struct Tap {
    source: u32,
    weight: i32,
}

/// One axis of a resample, fully resolved: for each destination sample, the
/// source samples it reads and the weight of each.
///
/// Resolving the geometry up front is what keeps the accumulation loops
/// pure arithmetic — no division, no edge clamp, and no branch on the
/// direction of the resample once a row is being filtered. The table is
/// small: a destination sample's tap count is about the ratio between the
/// extents, so the whole plan is on the order of the source extent plus the
/// destination extent regardless of how extreme that ratio is.
struct Axis {
    /// [`Self::stride`] taps per destination sample, destination-major,
    /// the weights of each summing to [`WEIGHT_ONE`].
    taps: Vec<Tap>,
    /// Taps per destination sample.
    stride: usize,
}

impl Axis {
    /// Plan `dest_extent` destination samples over the `extent` source
    /// samples starting at `origin`, or `None` when the allocator refuses
    /// the plan.
    fn plan(origin: u32, extent: u32, dest_extent: u32) -> Option<Self> {
        let reducing = extent > dest_extent;
        let width = if reducing {
            area_taps(extent, dest_extent)
        } else {
            CUBIC_TAPS
        };
        let dest = dest_extent as usize;
        let mut first = fallible::filled(dest, 0i64)?;
        let mut weights = fallible::filled(dest.saturating_mul(width), 0i32)?;
        for dest_sample in 0..dest_extent {
            let span = dest_sample as usize * width;
            let Some(row) = weights.get_mut(span..span + width) else {
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

        let (lead, stride) = live_span(&weights, width);
        let mut taps = Vec::new();
        if !fallible::reserve(&mut taps, dest.saturating_mul(stride)) {
            return None;
        }
        for (dest_sample, row) in weights.chunks_exact(width).enumerate() {
            let base = first.get(dest_sample).copied().unwrap_or(0);
            for tap in lead..lead + stride {
                let local = base.saturating_add(i64::try_from(tap).unwrap_or(i64::MAX));
                taps.push(Tap {
                    source: absolute_sample(local, origin, extent),
                    weight: row.get(tap).copied().unwrap_or(0),
                });
            }
        }
        Some(Self { taps, stride })
    }

    /// The taps of destination sample `dest_sample`, or nothing when the
    /// plan does not describe it.
    fn taps_of(&self, dest_sample: usize) -> &[Tap] {
        let start = dest_sample.saturating_mul(self.stride);
        self.taps
            .get(start..start.saturating_add(self.stride))
            .unwrap_or(&[])
    }

    /// Every destination sample's taps, in destination order.
    fn rows(&self) -> impl Iterator<Item = &[Tap]> {
        self.taps.chunks_exact(self.stride.max(1))
    }

    /// Whether this plan reproduces its source samples unchanged: every
    /// destination sample reads one source sample, in order, at full
    /// weight.
    fn is_identity(&self) -> bool {
        let Some(first) = self.taps.first() else {
            return false;
        };
        self.stride == 1
            && self.taps.iter().enumerate().all(|(dest, tap)| {
                tap.weight == WEIGHT_ONE
                    && u64::from(tap.source) == u64::from(first.source).saturating_add(dest as u64)
            })
    }
}

/// The tap columns of a `width`-wide weight plan that any destination
/// sample can carry weight in, as an offset and a length.
///
/// A plan is sized for the worst destination sample, so its outer columns
/// are often zero for *every* sample: a 1:1 cubic resample weights only its
/// second tap, and an area reduction whose ratio divides exactly leaves its
/// last tap empty. Those columns are dropped here, once, rather than
/// multiplied by zero for every pixel of every row. The span is never
/// empty — normalisation leaves each destination sample summing to one, so
/// some column always carries weight.
fn live_span(weights: &[i32], width: usize) -> (usize, usize) {
    let width = width.max(1);
    let mut lead = width;
    let mut end = 0;
    for row in weights.chunks_exact(width) {
        if let Some(first) = row.iter().position(|&weight| weight != 0) {
            lead = lead.min(first);
        }
        if let Some(last) = row.iter().rposition(|&weight| weight != 0) {
            end = end.max(last + 1);
        }
    }
    if end <= lead {
        return (0, width);
    }
    (lead, end - lead)
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
    /// [`ROW_SLOTS`], or `None` when the allocator refuses the rows.
    fn new(dest_width: u32, taps: usize) -> Option<Self> {
        let slots = taps.clamp(1, ROW_SLOTS);
        let stride = samples_per_row(dest_width);
        Some(Self {
            rows: fallible::filled(slots.saturating_mul(stride), 0i32)?,
            held: fallible::filled(slots, None)?,
            stride,
        })
    }

    /// Source row `source_row` of `src`, filtered along `columns`.
    fn filtered_row<R: Rows>(&mut self, src: &R, columns: &Axis, source_row: u32) -> &[i32] {
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

/// Filter source row `source_row` along `columns` into `out`, as one
/// space-internal four-channel sample per destination column.
///
/// A row outside the image — unreachable for a row a validated plan names —
/// contributes nothing.
fn filter_row<R: Rows>(src: &R, columns: &Axis, source_row: u32, out: &mut [i32]) {
    let Some(pixels) = src.row(source_row) else {
        out.fill(0);
        return;
    };
    // A filtered row is a whole number of samples by construction, so the
    // ragged tail this splits off is always empty.
    let (samples, _tail) = out.as_chunks_mut::<CHANNELS>();
    for (slot, taps) in samples.iter_mut().zip(columns.rows()) {
        let mut sums = [0i64; CHANNELS];
        for tap in taps {
            let Some(&pixel) = pixels.get(tap.source as usize) else {
                continue;
            };
            <R::Space as Space>::weigh(pixel, i64::from(tap.weight), &mut sums);
        }
        for (channel, sum) in slot.iter_mut().zip(sums) {
            *channel = i32::try_from(descale(sum)).unwrap_or(i32::MAX);
        }
    }
}

/// Write one destination row from its accumulated samples, through the
/// space's own [`Space::encode`].
fn write_row<S: Space>(accumulator: &[i64], out: &mut [S::Sample]) {
    let (accumulated, _rest) = accumulator.as_chunks::<CHANNELS>();
    for (samples, sample) in accumulated.iter().zip(out) {
        *sample = S::encode(*samples);
    }
}

/// A descaled sample as an `i32`, saturating rather than wrapping.
///
/// The saturation is unreachable for a sample a filter produced (they are
/// bounded by `255 * 255` either side of zero) and is what keeps this total
/// without a panic if one ever were not.
fn narrow(value: i64) -> i32 {
    i32::try_from(value).unwrap_or(if value < 0 { i32::MIN } else { i32::MAX })
}

/// Clamp `value` into `0..=255`.
fn clamp_u8(value: i32) -> u8 {
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "the clamp leaves exactly the non-negative values a `u8` holds"
    )]
    let clamped = value.clamp(0, 255) as u8;
    clamped
}

/// Remove one [`WEIGHT_SHIFT`] of fixed-point scale, rounding to nearest.
fn descale(value: i64) -> i64 {
    (value + (1 << (WEIGHT_SHIFT - 1))) >> WEIGHT_SHIFT
}

/// Filtered samples in one `width`-pixel row.
fn samples_per_row(width: u32) -> usize {
    (width as usize).saturating_mul(CHANNELS)
}

/// The sample count of a `width`×`height` image, or `None` when it does not
/// fit in a `usize` on this target.
fn sample_count(width: u32, height: u32) -> Option<usize> {
    usize::try_from(u64::from(width).checked_mul(u64::from(height))?).ok()
}

/// Sample offset of row `y` in a row-major buffer `width` samples wide, or
/// `None` when it would not fit a `usize` — unreachable for a buffer that
/// exists, but checked rather than assumed.
fn sample_offset(y: u32, width: u32) -> Option<usize> {
    usize::try_from(u64::from(y).checked_mul(u64::from(width))?).ok()
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

#[cfg(test)]
#[path = "resample_tests.rs"]
mod tests;
