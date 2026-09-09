//! The Windows icon and cursor containers (ICO and CUR): a directory of
//! independent pictures at different sizes, each one a device-independent
//! bitmap the BMP decoder reads or a whole PNG file.
//!
//! The two containers differ only in a type field and in what the two 16-bit
//! fields of a directory entry mean — an icon's colour planes and bit count,
//! a cursor's hot spot — and neither is load-bearing here, because the
//! picture's own header is what says how to decode it. So one decoder reads
//! both.
//!
//! A directory entry's declared width, height, and bit count are hints: real
//! files get them wrong, and a 256-pixel side is spelled zero. Everything
//! this decoder acts on comes from the entry's own picture header.
//!
//! # The mask, and the alpha channel that supersedes it
//!
//! An icon's DIB declares twice its picture's height: the colour rows, then
//! a 1-bit AND mask over them saying which pixels are absent. A 32-bit
//! entry instead carries alpha in each pixel's fourth byte, which the BMP
//! decoder leaves undefined for a plain BMP file and reads here — see
//! [`bmp::Dib::with_top_byte_alpha`]. A writer that fills that alpha leaves
//! the mask zero, so honouring both is the same as honouring the alpha; a
//! writer that never fills it leaves every pixel transparent, which is why
//! an all-zero alpha channel counts as no alpha channel and hands the
//! question back to the mask.

use crate::{bmp, png, DecodeError, DecodeLimits, FitBox, RasterImage, PROBE_LIMITS, RGBA_BYTES};

/// The four leading bytes of an icon container: two reserved zero bytes,
/// then the little-endian type — 1 for an icon, 2 for a cursor.
pub(crate) const ICO_SIGNATURE: [u8; 4] = [0, 0, 1, 0];
pub(crate) const CUR_SIGNATURE: [u8; 4] = [0, 0, 2, 0];

/// The directory's own length, and one entry's.
const DIRECTORY_LEN: usize = 6;
const ENTRY_LEN: usize = 16;

/// Offsets within a directory entry of the two fields this decoder reads:
/// the entry's byte length and where in the file it starts.
const ENTRY_BYTES_AT: usize = 8;
const ENTRY_OFFSET_AT: usize = 12;

/// How many pictures the directory declares.
fn entry_count(bytes: &[u8]) -> Result<u16, DecodeError> {
    if !bytes.starts_with(&ICO_SIGNATURE) && !bytes.starts_with(&CUR_SIGNATURE) {
        return Err(DecodeError::IcoBadSignature);
    }
    match crate::le_u16(bytes, 4).ok_or(DecodeError::IcoTruncated)? {
        0 => Err(DecodeError::IcoNoEntries),
        count => Ok(count),
    }
}

/// A directory field, as the byte offset or length it names.
fn field(bytes: &[u8], at: usize) -> Result<usize, DecodeError> {
    usize::try_from(crate::le_u32(bytes, at).ok_or(DecodeError::IcoTruncated)?)
        .map_err(|_| DecodeError::IcoTruncated)
}

/// One entry's picture, as the directory places it.
fn entry(bytes: &[u8], index: u16) -> Result<&[u8], DecodeError> {
    // A 16-bit index over 16-byte rows is at most a megabyte in, so the row's
    // own offsets are far from any pointer width's ceiling.
    let at = DIRECTORY_LEN + usize::from(index) * ENTRY_LEN;
    let len = field(bytes, at + ENTRY_BYTES_AT)?;
    let start = field(bytes, at + ENTRY_OFFSET_AT)?;
    let end = start.checked_add(len).ok_or(DecodeError::IcoTruncated)?;
    bytes.get(start..end).ok_or(DecodeError::IcoTruncated)
}

/// An icon's DIB declares twice its picture's height: the colour rows, then
/// the mask over them.
fn picture_height(dib: &bmp::Dib) -> Result<u32, DecodeError> {
    if !dib.height().is_multiple_of(2) {
        return Err(DecodeError::IcoInvalidMaskHeight);
    }
    Ok(dib.height() / 2)
}

/// An entry's declared geometry, read from its picture's header alone.
fn geometry(picture: &[u8]) -> Result<(u32, u32), DecodeError> {
    if picture.starts_with(&crate::PNG_SIGNATURE) {
        return png::probe(picture);
    }
    let dib = bmp::read_dib(picture)?;
    Ok((dib.width(), picture_height(&dib)?))
}

fn area(width: u32, height: u32) -> u64 {
    u64::from(width) * u64::from(height)
}

/// The page a decode answers.
///
/// With no `fit` that is the largest page the directory holds, which is what
/// a container of one picture at several sizes means by its picture. With one
/// it is the smallest page covering it on both axes, falling back to the
/// largest: an icon file exists precisely so a caller can take the size it
/// wants, so taking it is what fitting means here, with nothing scaled and
/// nothing resampled.
///
/// Only pages inside `limits` are eligible, so a fitted decode degrades to a
/// size the caller can afford rather than refusing. A plain decode passes the
/// probe's own non-limits here and is refused by the decode itself where the
/// picture is too large, because a plain decode always means the picture the
/// container is.
///
/// A page whose own header will not parse is passed over rather than failing
/// the file — the pages are independent pictures, and one bad page is no
/// reason to refuse the good ones. Where none is eligible, the first refusal
/// any page raised is the answer, because that names a real reason.
fn select(
    bytes: &[u8],
    count: u16,
    fit: Option<FitBox>,
    limits: &DecodeLimits,
) -> Result<(u16, u32, u32), DecodeError> {
    let mut covering: Option<(u16, u32, u32)> = None;
    let mut biggest: Option<(u16, u32, u32)> = None;
    let mut refusal: Option<DecodeError> = None;
    for index in 0..count {
        let (width, height) = match entry(bytes, index).and_then(geometry) {
            Ok(geometry) => geometry,
            Err(err) => {
                refusal = refusal.or(Some(err));
                continue;
            }
        };
        if let Err(err) = limits.check(width, height) {
            refusal = refusal.or(Some(err));
            continue;
        }
        if biggest.is_none_or(|(_, w, h)| area(width, height) > area(w, h)) {
            biggest = Some((index, width, height));
        }
        if fit.is_some_and(|fit| width >= fit.width() && height >= fit.height())
            && covering.is_none_or(|(_, w, h)| area(width, height) < area(w, h))
        {
            covering = Some((index, width, height));
        }
    }
    covering
        .or(biggest)
        .ok_or_else(|| refusal.unwrap_or(DecodeError::IcoNoEntries))
}

/// Decode one entry's picture.
fn decode_entry(picture: &[u8], limits: &DecodeLimits) -> Result<RasterImage, DecodeError> {
    if picture.starts_with(&crate::PNG_SIGNATURE) {
        return png::decode(picture, limits);
    }
    let dib = bmp::read_dib(picture)?.with_top_byte_alpha();
    let rows = picture_height(&dib)?;
    let palette_at = dib.palette_at();
    let palette_end = palette_at
        .checked_add(dib.palette_bytes()?)
        .ok_or(DecodeError::IcoTruncated)?;
    let palette = picture
        .get(palette_at..palette_end)
        .ok_or(DecodeError::IcoTruncated)?;
    let pixels = picture
        .get(palette_end..)
        .ok_or(DecodeError::IcoTruncated)?;
    let (mut out, consumed) = bmp::decode_pixels(&dib, palette, pixels, rows, limits)?;
    apply_mask(&dib, pixels, consumed, rows, &mut out)?;
    Ok(RasterImage::from_parts(dib.width(), rows, out))
}

/// Apply the 1-bit AND mask following an entry's colour rows, unless those
/// rows brought an alpha channel that says anything.
fn apply_mask(
    dib: &bmp::Dib,
    pixels: &[u8],
    consumed: usize,
    rows: u32,
    out: &mut [u8],
) -> Result<(), DecodeError> {
    if dib.has_alpha()
        && out
            .as_chunks::<RGBA_BYTES>()
            .0
            .iter()
            .any(|pixel| pixel[3] != 0)
    {
        return Ok(());
    }
    for pixel in out.as_chunks_mut::<RGBA_BYTES>().0 {
        pixel[3] = u8::MAX;
    }
    let mask = pixels.get(consumed..).ok_or(DecodeError::IcoTruncated)?;
    let stride = bmp::stride(dib.width(), 1)?;
    let width = usize::try_from(dib.width()).unwrap_or(usize::MAX);
    let row_bytes = width
        .checked_mul(RGBA_BYTES)
        .ok_or(DecodeError::DimensionsOverflow)?;
    for row in 0..rows {
        let source = if dib.top_down() { row } else { rows - 1 - row };
        let (Some(bits), Some(target)) = (
            usize::try_from(source)
                .ok()
                .and_then(|source| source.checked_mul(stride))
                .and_then(|at| mask.get(at..at.checked_add(stride)?)),
            usize::try_from(row)
                .ok()
                .and_then(|row| row.checked_mul(row_bytes))
                .and_then(|at| out.get_mut(at..at.checked_add(row_bytes)?)),
        ) else {
            return Err(DecodeError::IcoTruncated);
        };
        for (x, pixel) in (0..width).zip(target.as_chunks_mut::<RGBA_BYTES>().0) {
            let byte = bits.get(x / 8).copied().ok_or(DecodeError::IcoTruncated)?;
            if byte >> (7 - x % 8) & 1 != 0 {
                pixel[3] = 0;
            }
        }
    }
    Ok(())
}

/// Read the geometry of the picture [`decode`] would answer, from headers
/// alone.
pub(crate) fn probe(bytes: &[u8]) -> Result<(u32, u32), DecodeError> {
    let count = entry_count(bytes)?;
    let (_, width, height) = select(bytes, count, None, &PROBE_LIMITS)?;
    Ok((width, height))
}

/// Decode the container's largest picture at its natural size.
pub(crate) fn decode(bytes: &[u8], limits: &DecodeLimits) -> Result<RasterImage, DecodeError> {
    let count = entry_count(bytes)?;
    let (index, _, _) = select(bytes, count, None, &PROBE_LIMITS)?;
    decode_entry(entry(bytes, index)?, limits)
}

/// Decode the smallest picture covering `fit`, falling back to the largest
/// the caller's limits allow.
pub(crate) fn decode_fitted(
    bytes: &[u8],
    limits: &DecodeLimits,
    fit: FitBox,
) -> Result<RasterImage, DecodeError> {
    let count = entry_count(bytes)?;
    let (index, _, _) = select(bytes, count, Some(fit), limits)?;
    decode_entry(entry(bytes, index)?, limits)
}

/// An icon container's pages, decoded one at a time.
///
/// Nothing is weighed against `limits` when the container opens, because
/// nothing is allocated then: the pages are independent pictures and a
/// caller may well want a small one out of a file whose largest it could
/// never afford, so each page is weighed when it is asked for.
pub(crate) struct Pages<'a> {
    bytes: &'a [u8],
    limits: DecodeLimits,
    count: u16,
    width: u32,
    height: u32,
    cursor: u16,
    /// The page most recently decoded, which is what a frame lends.
    current: Option<RasterImage>,
    /// The refusal a step stopped at, if one did.
    failed: Option<DecodeError>,
}

impl<'a> Pages<'a> {
    /// Validate the directory and measure its pages, decoding none of them.
    pub(crate) fn open(bytes: &'a [u8], limits: &DecodeLimits) -> Result<Self, DecodeError> {
        let count = entry_count(bytes)?;
        let (_, width, height) = select(bytes, count, None, &PROBE_LIMITS)?;
        Ok(Self {
            bytes,
            limits: *limits,
            count,
            width,
            height,
            cursor: 0,
            current: None,
            failed: None,
        })
    }

    /// The largest page's width, which is the picture the container is.
    pub(crate) const fn width(&self) -> u32 {
        self.width
    }

    pub(crate) const fn height(&self) -> u32 {
        self.height
    }

    pub(crate) fn count(&self) -> u32 {
        u32::from(self.count)
    }

    /// The page a step would decode next.
    pub(crate) fn index(&self) -> u32 {
        u32::from(self.cursor)
    }

    /// The page most recently decoded.
    pub(crate) const fn current(&self) -> Option<&RasterImage> {
        self.current.as_ref()
    }

    /// Decode the next page, answering `false` once they are exhausted.
    ///
    /// A refusal is remembered and repeated until [`Self::rewind`], which is
    /// the same contract every container's walk keeps.
    pub(crate) fn step(&mut self) -> Result<bool, DecodeError> {
        if let Some(failed) = &self.failed {
            return Err(failed.clone());
        }
        if self.cursor >= self.count {
            return Ok(false);
        }
        match self.decode_at(self.cursor) {
            Ok(()) => {
                self.cursor += 1;
                Ok(true)
            }
            Err(err) => {
                self.failed = Some(err.clone());
                Err(err)
            }
        }
    }

    /// Decode the page at `index`, answering `false` when there is none.
    pub(crate) fn page(&mut self, index: u32) -> Result<bool, DecodeError> {
        let Ok(index) = u16::try_from(index) else {
            return Ok(false);
        };
        if index >= self.count {
            return Ok(false);
        }
        self.decode_at(index)?;
        Ok(true)
    }

    /// Restart at the first page, clearing a remembered refusal.
    pub(crate) fn rewind(&mut self) {
        self.cursor = 0;
        self.current = None;
        self.failed = None;
    }

    fn decode_at(&mut self, index: u16) -> Result<(), DecodeError> {
        // The previous page's buffer is released before the next is
        // reserved, so holding one page never costs two.
        self.current = None;
        self.current = Some(decode_entry(entry(self.bytes, index)?, &self.limits)?);
        Ok(())
    }
}

#[cfg(test)]
#[path = "ico_tests.rs"]
mod tests;
