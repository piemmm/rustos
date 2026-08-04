//! The desktop's one image resampler: an alpha-weighted integer box filter
//! over straight-alpha RGBA8 pixels.
//!
//! Two consumers scale decoded images: the icon pipeline fits a bundle's
//! artwork into a square slot, and the wallpaper pipeline places a
//! photograph onto a screen. Both want the same arithmetic — average the
//! source pixels a destination pixel covers, weight the colour channels by
//! alpha so a transparent neighbour cannot bleed its colour into an opaque
//! pixel — so that arithmetic lives here once rather than beside each
//! caller.
//!
//! # Why a box filter
//!
//! A destination pixel's value is the mean of every source pixel whose area
//! it covers ([`Region`] → destination extent). On a **downscale** that is a
//! true area average, so a 6688-pixel-wide photograph reduced to a 1920-pixel
//! screen blends rather than aliases into a shimmering mess; on an
//! **upscale** each destination pixel covers exactly one source pixel and the
//! filter degrades to sample-and-hold, which is honest — inventing detail a
//! smoother interpolation would imply is not this crate's job. The
//! arithmetic is integer throughout: no floating point, no rounding drift
//! between two calls that should agree.
//!
//! # Bands
//!
//! [`resample_rows`] produces an arbitrary contiguous run of destination
//! rows, so a caller that cannot hold (or cannot transport) a whole
//! destination image at once can produce it a band at a time at no extra
//! cost — each row is computed from the source directly, never from a
//! previous band. [`resample`] is the whole-image case expressed through
//! that same one implementation.
//!
//! Every entry point is total: degenerate geometry, a source region that
//! does not lie inside its image, a mis-sized output buffer, and a band that
//! runs past the destination all return a typed refusal rather than
//! panicking or writing a partial result.

use alloc::vec;
use alloc::vec::Vec;

/// Bytes per straight-alpha RGBA8 pixel.
const CHANNELS: usize = 4;

/// [`CHANNELS`] for the widened arithmetic every offset is computed in.
const CHANNELS_U64: u64 = 4;

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
/// The band is computed from the source alone, so bands are independent:
/// producing a destination one band at a time yields byte-for-byte the same
/// image as producing it in one call.
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

    // The horizontal spans depend only on the column, so they are computed
    // once for the destination width rather than once per pixel of every
    // row in the band.
    let columns: Vec<(u32, u32)> = (0..dest_width)
        .map(|dest_x| source_span(dest_x, dest_width, region.x, region.width))
        .collect();
    let row_bytes = expected / rows_as_usize(rows);
    for (dest_y, chunk) in (first_row..last).zip(out.chunks_exact_mut(row_bytes)) {
        let (sy0, sy1) = source_span(dest_y, dest_height, region.y, region.height);
        // A row is a whole number of pixels by construction, so the ragged
        // tail this splits off is always empty.
        let (pixels, _tail) = chunk.as_chunks_mut::<CHANNELS>();
        for (span, pixel) in columns.iter().zip(pixels) {
            *pixel = average(src, span.0, span.1, sy0, sy1);
        }
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

/// The half-open span of source samples along one axis that destination
/// sample `dest` of `dest_extent` covers, within the source run starting at
/// `origin` and running for `extent` samples.
///
/// The span is always non-empty: on an upscale, where a destination sample
/// covers less than one source sample, it is the single sample the
/// destination sample's leading edge falls in.
fn source_span(dest: u32, dest_extent: u32, origin: u32, extent: u32) -> (u32, u32) {
    let extent64 = u64::from(extent);
    let dest_extent64 = u64::from(dest_extent);
    let start = u64::from(dest) * extent64 / dest_extent64;
    let end = (u64::from(dest) + 1) * extent64 / dest_extent64;
    let start = start.min(extent64.saturating_sub(1));
    let end = end.max(start + 1).min(extent64);
    // Both ends are bounded by `extent`, which is a `u32`, so neither
    // conversion can lose information; the fallbacks only keep them total.
    (
        origin.saturating_add(u32::try_from(start).unwrap_or(0)),
        origin.saturating_add(u32::try_from(end).unwrap_or(extent)),
    )
}

/// The alpha-weighted mean of the source pixels in the half-open box
/// `[sx0, sx1) × [sy0, sy1)`.
///
/// Colour channels are averaged **weighted by alpha**, so a fully
/// transparent source pixel contributes its transparency but not its
/// (meaningless) colour: scaling artwork with transparent padding must not
/// drag that padding's colour into the visible edge. A box that is entirely
/// transparent has no colour to report and yields transparent black.
fn average(src: &Rgba8Image<'_>, sx0: u32, sx1: u32, sy0: u32, sy1: u32) -> [u8; CHANNELS] {
    let mut weighted = [0u64; 3];
    let mut alpha_sum: u64 = 0;
    let mut samples: u64 = 0;
    for y in sy0..sy1 {
        for x in sx0..sx1 {
            let Some(at) = pixel_offset(x, y, src.width) else {
                continue;
            };
            let Some(px) = src.pixels.get(at..at + CHANNELS) else {
                continue;
            };
            let alpha = u64::from(px[3]);
            weighted[0] += u64::from(px[0]) * alpha;
            weighted[1] += u64::from(px[1]) * alpha;
            weighted[2] += u64::from(px[2]) * alpha;
            alpha_sum += alpha;
            samples += 1;
        }
    }
    if samples == 0 || alpha_sum == 0 {
        return [0, 0, 0, 0];
    }
    let half = alpha_sum / 2;
    let channel = |sum: u64| u8::try_from((sum + half) / alpha_sum).unwrap_or(u8::MAX);
    let alpha = u8::try_from((alpha_sum + samples / 2) / samples).unwrap_or(u8::MAX);
    [
        channel(weighted[0]),
        channel(weighted[1]),
        channel(weighted[2]),
        alpha,
    ]
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
