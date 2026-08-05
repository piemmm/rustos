//! A complete, fail-closed PNG decoder (W3C PNG specification).
//!
//! The decoder is a single forward pass: the 8-byte signature, then chunk
//! framing (length, type, payload, CRC-32) validated one chunk at a time,
//! enforcing the specification's chunk-ordering rules (`IHDR` first and
//! unique, `PLTE` before the first `IDAT`, `IDAT` chunks contiguous, `IEND`
//! last and empty, no data afterwards, an unknown critical chunk refused
//! while an unknown ancillary chunk is skipped once its CRC checks out).
//! Every declared size — a chunk length, a palette entry count, the
//! decompressed image size implied by the geometry — is validated against
//! the bytes actually available, or against a size computed purely from
//! already-bounded geometry, before it is used to allocate or index
//! anything; see the crate documentation for the full bounds policy.
//!
//! Once the chunk stream is validated, the concatenated `IDAT` payload is
//! zlib-decompressed (`tairix_compress::zlib`) into a buffer sized exactly
//! to what the image's geometry implies — a stream producing a different
//! number of bytes is refused rather than truncated or over-read — then
//! each scanline (or, for an interlaced image, each of Adam7's seven
//! passes' scanlines) is unfiltered and its samples expanded to
//! straight-alpha RGBA8.

use alloc::vec;
use alloc::vec::Vec;

use crate::{crc32, DecodeError, DecodeLimits, RasterImage};

/// The 8-byte PNG file signature.
const SIGNATURE: [u8; 8] = crate::PNG_SIGNATURE;

const IHDR: [u8; 4] = *b"IHDR";
const PLTE: [u8; 4] = *b"PLTE";
const IDAT: [u8; 4] = *b"IDAT";
const IEND: [u8; 4] = *b"IEND";
const TRNS: [u8; 4] = *b"tRNS";

/// The `IHDR` payload length (W3C PNG §"IHDR Image header"): four fields of
/// 4 bytes plus five of 1 byte.
const IHDR_LEN: usize = 13;

/// The limits a header probe holds a declared geometry to: none of its own.
///
/// A probe allocates nothing from the geometry it reports, so it has nothing
/// to protect by bounding it — its caller does, and applies its own bounds
/// to the answer. The zero-dimension refusal the shared header parser makes
/// is still enforced, because a zero-sided image is malformed rather than
/// merely large. The progressive-coefficient bound is irrelevant to PNG.
const PROBE_LIMITS: DecodeLimits = DecodeLimits::new(u32::MAX, u32::MAX, u64::MAX, 0);

/// The five legal PNG colour types, made a closed type so an unvalidated
/// byte can never reach the pixel-assembly code — an illegal colour type is
/// refused once, at parse time, rather than needing a fallback arm
/// everywhere it might otherwise appear.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
enum ColourType {
    Grey,
    Truecolour,
    Indexed,
    GreyAlpha,
    Rgba,
}

impl ColourType {
    fn from_byte(byte: u8) -> Result<Self, DecodeError> {
        match byte {
            0 => Ok(Self::Grey),
            2 => Ok(Self::Truecolour),
            3 => Ok(Self::Indexed),
            4 => Ok(Self::GreyAlpha),
            6 => Ok(Self::Rgba),
            _ => Err(DecodeError::InvalidColourType),
        }
    }

    /// Samples per pixel this colour type carries.
    const fn channels(self) -> u32 {
        match self {
            Self::Grey | Self::Indexed => 1,
            Self::Truecolour => 3,
            Self::GreyAlpha => 2,
            Self::Rgba => 4,
        }
    }

    /// Whether `depth` is one of the bit depths this colour type permits
    /// (W3C PNG §"Color type combinations").
    const fn allows_depth(self, depth: u8) -> bool {
        match self {
            Self::Grey => matches!(depth, 1 | 2 | 4 | 8 | 16),
            Self::Truecolour | Self::GreyAlpha | Self::Rgba => matches!(depth, 8 | 16),
            Self::Indexed => matches!(depth, 1 | 2 | 4 | 8),
        }
    }
}

/// A validated `IHDR` chunk.
struct Ihdr {
    width: u32,
    height: u32,
    bit_depth: u8,
    colour_type: ColourType,
    interlaced: bool,
}

/// Colour-key / per-index transparency declared by a `tRNS` chunk.
enum Trns {
    /// `colour type` 0: the single greyscale sample value that is
    /// transparent, compared at the image's own (unscaled) bit depth.
    GreyKey(u16),
    /// `colour type` 2: the (r, g, b) sample triple that is transparent,
    /// each compared at the image's own (unscaled) bit depth.
    RgbKey(u16, u16, u16),
    /// `colour type` 3: per-palette-index alpha. An index beyond the end of
    /// this list is opaque (the spec's "missing entries default to 255").
    Indexed(Vec<u8>),
}

/// Read one chunk starting at `pos`, verifying its CRC, and return its
/// type, payload, and the position immediately after it.
///
/// # Errors
///
/// [`DecodeError::ChunkTruncated`] if fewer than 8 bytes (the length/type
/// header) remain; [`DecodeError::ChunkLengthExceedsInput`] if the declared
/// length runs past the end of `data` (counting the trailing CRC);
/// [`DecodeError::ChunkCrcMismatch`] if the trailing CRC-32 does not match.
fn read_chunk(data: &[u8], pos: usize) -> Result<([u8; 4], &[u8], usize), DecodeError> {
    let header = data.get(pos..pos + 8).ok_or(DecodeError::ChunkTruncated)?;
    let length = u32::from_be_bytes([header[0], header[1], header[2], header[3]]);
    let length = usize::try_from(length).unwrap_or(usize::MAX);
    let mut chunk_type = [0u8; 4];
    chunk_type.copy_from_slice(&header[4..8]);

    let payload_start = pos + 8;
    let payload_end = payload_start
        .checked_add(length)
        .ok_or(DecodeError::ChunkLengthExceedsInput)?;
    let crc_end = payload_end
        .checked_add(4)
        .ok_or(DecodeError::ChunkLengthExceedsInput)?;
    if crc_end > data.len() {
        return Err(DecodeError::ChunkLengthExceedsInput);
    }

    let payload = &data[payload_start..payload_end];
    let stored_crc_bytes: [u8; 4] = data[payload_end..crc_end].try_into().unwrap_or([0; 4]);
    let stored_crc = u32::from_be_bytes(stored_crc_bytes);
    if stored_crc != crc32::crc32_of(&[&chunk_type, payload]) {
        return Err(DecodeError::ChunkCrcMismatch);
    }
    Ok((chunk_type, payload, crc_end))
}

/// A critical chunk's type has an uppercase first letter (W3C PNG
/// §"Chunk naming conventions"); an ancillary chunk's is lowercase.
fn is_critical(chunk_type: [u8; 4]) -> bool {
    chunk_type[0] & 0x20 == 0
}

/// Validate and parse an `IHDR` payload, checking declared dimensions
/// against `limits` before anything else in the file is trusted.
fn parse_ihdr(data: &[u8], limits: &DecodeLimits) -> Result<Ihdr, DecodeError> {
    if data.len() != IHDR_LEN {
        return Err(DecodeError::InvalidIhdrLength);
    }
    let width = u32::from_be_bytes([data[0], data[1], data[2], data[3]]);
    let height = u32::from_be_bytes([data[4], data[5], data[6], data[7]]);
    limits.check(width, height)?;

    let bit_depth = data[8];
    if !matches!(bit_depth, 1 | 2 | 4 | 8 | 16) {
        return Err(DecodeError::InvalidBitDepth);
    }
    let colour_type = ColourType::from_byte(data[9])?;
    if !colour_type.allows_depth(bit_depth) {
        return Err(DecodeError::UnsupportedColourTypeAndDepth);
    }
    if data[10] != 0 {
        return Err(DecodeError::InvalidCompressionMethod);
    }
    if data[11] != 0 {
        return Err(DecodeError::InvalidFilterMethod);
    }
    let interlaced = match data[12] {
        0 => false,
        1 => true,
        _ => return Err(DecodeError::InvalidInterlaceMethod),
    };

    Ok(Ihdr {
        width,
        height,
        bit_depth,
        colour_type,
        interlaced,
    })
}

/// Validate and parse a `PLTE` payload: 1..=256 RGB triples.
fn parse_palette(data: &[u8]) -> Result<Vec<[u8; 3]>, DecodeError> {
    if data.is_empty() || !data.len().is_multiple_of(3) || data.len() > 3 * 256 {
        return Err(DecodeError::InvalidPaletteLength);
    }
    let (entries, _remainder) = data.as_chunks::<3>();
    Ok(entries.to_vec())
}

/// Validate and parse a `tRNS` payload against the image's colour type.
fn parse_trns(data: &[u8], ihdr: &Ihdr, palette: Option<&[[u8; 3]]>) -> Result<Trns, DecodeError> {
    match ihdr.colour_type {
        ColourType::Grey => {
            let &[hi, lo] = data else {
                return Err(DecodeError::InvalidTransparencyLength);
            };
            Ok(Trns::GreyKey(u16::from_be_bytes([hi, lo])))
        }
        ColourType::Truecolour => {
            let &[r0, r1, g0, g1, b0, b1] = data else {
                return Err(DecodeError::InvalidTransparencyLength);
            };
            Ok(Trns::RgbKey(
                u16::from_be_bytes([r0, r1]),
                u16::from_be_bytes([g0, g1]),
                u16::from_be_bytes([b0, b1]),
            ))
        }
        ColourType::Indexed => {
            let palette = palette.ok_or(DecodeError::InvalidTransparencyLength)?;
            if data.len() > palette.len() {
                return Err(DecodeError::InvalidTransparencyLength);
            }
            Ok(Trns::Indexed(data.to_vec()))
        }
        ColourType::GreyAlpha | ColourType::Rgba => Err(DecodeError::TransparencyForbidden),
    }
}

/// Read the natural size an `IHDR` chunk declares, decoding nothing.
///
/// Validated by the same header parser a full decode uses, so a probe
/// accepts exactly the headers a decode would: a bad signature, a first
/// chunk that is not `IHDR`, a failed CRC, or an illegal bit depth, colour
/// type, compression, filter or interlace method is refused here too.
pub(crate) fn probe(bytes: &[u8]) -> Result<(u32, u32), DecodeError> {
    if !bytes.starts_with(&SIGNATURE) {
        return Err(DecodeError::BadSignature);
    }
    let (chunk_type, payload, _after) = read_chunk(bytes, SIGNATURE.len())?;
    if chunk_type != IHDR {
        return Err(DecodeError::HeaderNotFirst);
    }
    let ihdr = parse_ihdr(payload, &PROBE_LIMITS)?;
    Ok((ihdr.width, ihdr.height))
}

/// Decode a complete PNG file into a [`RasterImage`].
///
/// # Errors
///
/// See [`DecodeError`] for every fail-closed refusal reason.
pub(crate) fn decode(bytes: &[u8], limits: &DecodeLimits) -> Result<RasterImage, DecodeError> {
    let rest = bytes
        .strip_prefix(&SIGNATURE)
        .ok_or(DecodeError::BadSignature)?;

    let mut pos = 0usize;
    let mut ihdr: Option<Ihdr> = None;
    let mut palette: Option<Vec<[u8; 3]>> = None;
    let mut trns: Option<Trns> = None;
    let mut idat = Vec::new();
    let mut seen_idat = false;
    let mut idat_finished = false;
    let mut seen_iend = false;
    let mut first_chunk = true;

    while pos < rest.len() {
        if seen_iend {
            return Err(DecodeError::DataAfterEnd);
        }
        let (chunk_type, payload, next_pos) = read_chunk(rest, pos)?;
        pos = next_pos;

        if first_chunk && chunk_type != IHDR {
            return Err(DecodeError::HeaderNotFirst);
        }
        first_chunk = false;

        match chunk_type {
            IHDR => {
                if ihdr.is_some() {
                    return Err(DecodeError::DuplicateHeader);
                }
                ihdr = Some(parse_ihdr(payload, limits)?);
            }
            PLTE => {
                let header = ihdr.as_ref().ok_or(DecodeError::MissingHeader)?;
                if seen_idat {
                    return Err(DecodeError::PaletteAfterImageData);
                }
                if matches!(header.colour_type, ColourType::Grey | ColourType::GreyAlpha) {
                    return Err(DecodeError::PaletteForbidden);
                }
                if palette.is_some() {
                    return Err(DecodeError::DuplicatePalette);
                }
                palette = Some(parse_palette(payload)?);
            }
            TRNS => {
                let header = ihdr.as_ref().ok_or(DecodeError::MissingHeader)?;
                if trns.is_some() {
                    return Err(DecodeError::DuplicateTransparency);
                }
                trns = Some(parse_trns(payload, header, palette.as_deref())?);
            }
            IDAT => {
                if idat_finished {
                    return Err(DecodeError::ImageDataNotContiguous);
                }
                idat.extend_from_slice(payload);
                seen_idat = true;
            }
            IEND => {
                if !payload.is_empty() {
                    return Err(DecodeError::MalformedEnd);
                }
                seen_iend = true;
            }
            other => {
                if is_critical(other) {
                    return Err(DecodeError::UnknownCriticalChunk);
                }
                // A recognised-but-unhandled or wholly unknown ancillary
                // chunk: its CRC already checked out above, so it is
                // simply skipped.
            }
        }

        // Any chunk other than IDAT that follows at least one IDAT closes
        // the contiguous run; a further IDAT is then a specification
        // violation rather than a continuation.
        if chunk_type != IDAT && seen_idat {
            idat_finished = true;
        }
    }

    let ihdr = ihdr.ok_or(DecodeError::MissingHeader)?;
    if !seen_iend {
        return Err(DecodeError::MissingEnd);
    }
    if idat.is_empty() {
        return Err(DecodeError::MissingImageData);
    }
    if ihdr.colour_type == ColourType::Indexed && palette.is_none() {
        return Err(DecodeError::PaletteRequired);
    }

    let pixels = decode_pixels(&ihdr, palette.as_deref(), trns.as_ref(), &idat)?;
    Ok(RasterImage::from_parts(ihdr.width, ihdr.height, pixels))
}

/// Widen a `u32` to `usize`, failing closed rather than truncating.
fn to_usize(value: u32) -> Result<usize, DecodeError> {
    usize::try_from(value).map_err(|_| DecodeError::DimensionsOverflow)
}

/// A pass-local coordinate `index` placed into the full image: `start +
/// index * step`, checked so a degenerate caller-supplied limit cannot
/// silently overflow rather than being refused.
fn placed_coordinate(start: u32, index: u32, step: u32) -> Result<u32, DecodeError> {
    index
        .checked_mul(step)
        .and_then(|scaled| scaled.checked_add(start))
        .ok_or(DecodeError::DimensionsOverflow)
}

/// Widen a `u64` to `usize`, failing closed rather than truncating.
fn to_usize64(value: u64) -> Result<usize, DecodeError> {
    usize::try_from(value).map_err(|_| DecodeError::DimensionsOverflow)
}

/// One Adam7 pass's placement in the final image, or the single implicit
/// pass covering the whole image when the file is not interlaced.
struct Pass {
    row_start: u32,
    col_start: u32,
    row_step: u32,
    col_step: u32,
    width: u32,
    height: u32,
}

/// Adam7's seven passes as `(row_start, col_start, row_step, col_step)`
/// (W3C PNG §"Interlaced data order").
const ADAM7: [(u32, u32, u32, u32); 7] = [
    (0, 0, 8, 8),
    (0, 4, 8, 8),
    (4, 0, 8, 4),
    (0, 2, 4, 4),
    (2, 0, 4, 2),
    (0, 1, 2, 2),
    (1, 0, 2, 1),
];

/// The number of samples a pass covers along one axis of `total` pixels,
/// starting at `start` and stepping by `step`; `0` if the pass starts
/// beyond the image entirely (an empty pass).
///
/// Saturating throughout: `total`, `start`, and `step` are always small in
/// practice (bounded by an already-checked [`DecodeLimits`] and Adam7's
/// fixed step values), so saturation is unreachable outside a degenerate,
/// caller-misconfigured limit — but it keeps this total rather than an
/// overflow panic even then.
const fn pass_extent(total: u32, start: u32, step: u32) -> u32 {
    if start >= total {
        0
    } else {
        let span = total.saturating_sub(start);
        span.saturating_add(step).saturating_sub(1) / step
    }
}

/// The passes an image decodes as: the seven Adam7 passes when interlaced,
/// or a single pass covering the whole image otherwise.
fn passes_for(ihdr: &Ihdr) -> Vec<Pass> {
    if ihdr.interlaced {
        ADAM7
            .iter()
            .map(|&(row_start, col_start, row_step, col_step)| Pass {
                row_start,
                col_start,
                row_step,
                col_step,
                width: pass_extent(ihdr.width, col_start, col_step),
                height: pass_extent(ihdr.height, row_start, row_step),
            })
            .collect()
    } else {
        vec![Pass {
            row_start: 0,
            col_start: 0,
            row_step: 1,
            col_step: 1,
            width: ihdr.width,
            height: ihdr.height,
        }]
    }
}

/// Bits per complete pixel for `colour_type` at `bit_depth`.
fn bits_per_pixel(colour_type: ColourType, bit_depth: u8) -> u32 {
    colour_type.channels() * u32::from(bit_depth)
}

/// Bytes per complete pixel used by the filter reconstruction (W3C PNG
/// §"Filtering"): at least one byte, even for sub-byte bit depths.
fn filter_bpp(bits_per_pixel: u32) -> usize {
    core::cmp::max(1, usize::try_from(bits_per_pixel / 8).unwrap_or(usize::MAX))
}

/// The number of sample bytes (excluding the leading filter byte) one
/// scanline of `width` pixels at `bits_per_pixel` occupies.
fn sample_bytes_per_row(width: u64, bits_per_pixel: u32) -> Result<u64, DecodeError> {
    let bits = width
        .checked_mul(u64::from(bits_per_pixel))
        .ok_or(DecodeError::DimensionsOverflow)?;
    bits.checked_add(7)
        .map(|rounded| rounded / 8)
        .ok_or(DecodeError::DimensionsOverflow)
}

/// The total decompressed byte count every declared pass implies: each
/// non-empty pass contributes `height * (1 + sample_bytes_per_row)`.
fn expected_decompressed_len(passes: &[Pass], bits_per_pixel: u32) -> Result<u64, DecodeError> {
    let mut total = 0u64;
    for pass in passes {
        if pass.width == 0 || pass.height == 0 {
            continue;
        }
        let row_len = sample_bytes_per_row(u64::from(pass.width), bits_per_pixel)?
            .checked_add(1)
            .ok_or(DecodeError::DimensionsOverflow)?;
        let pass_len = row_len
            .checked_mul(u64::from(pass.height))
            .ok_or(DecodeError::DimensionsOverflow)?;
        total = total
            .checked_add(pass_len)
            .ok_or(DecodeError::DimensionsOverflow)?;
    }
    Ok(total)
}

/// The Paeth predictor (W3C PNG §"Filter type 4: Paeth"): whichever of `a`
/// (left), `b` (above), `c` (above-left) lies closest to `a + b - c`, tied
/// in favour of `a`, then `b`.
fn paeth_predictor(a: u8, b: u8, c: u8) -> u8 {
    let base = i32::from(a) + i32::from(b) - i32::from(c);
    let pa = (base - i32::from(a)).abs();
    let pb = (base - i32::from(b)).abs();
    let pc = (base - i32::from(c)).abs();
    if pa <= pb && pa <= pc {
        a
    } else if pb <= pc {
        b
    } else {
        c
    }
}

/// Reconstruct one pass's scanlines from its filtered bytes.
///
/// `raw` holds exactly `pass_height` scanlines, each a filter-type byte
/// followed by `row_sample_bytes` filtered sample bytes. Returns the
/// unfiltered sample bytes, `row_sample_bytes` per row, with no filter
/// bytes interleaved.
fn defilter_pass(
    raw: &[u8],
    pass_height: u32,
    row_sample_bytes: usize,
    bpp: usize,
) -> Result<Vec<u8>, DecodeError> {
    let total = row_sample_bytes
        .checked_mul(to_usize(pass_height)?)
        .ok_or(DecodeError::DimensionsOverflow)?;
    let mut out = vec![0u8; total];
    let mut raw_pos = 0usize;
    let mut previous_row_start: Option<usize> = None;

    for row in 0..to_usize(pass_height)? {
        let filter_type = *raw
            .get(raw_pos)
            .ok_or(DecodeError::CompressedSizeMismatch)?;
        raw_pos += 1;
        let filtered = raw
            .get(raw_pos..raw_pos + row_sample_bytes)
            .ok_or(DecodeError::CompressedSizeMismatch)?;
        raw_pos += row_sample_bytes;

        let out_start = row * row_sample_bytes;
        for i in 0..row_sample_bytes {
            let x = filtered[i];
            let a = if i >= bpp {
                out[out_start + i - bpp]
            } else {
                0
            };
            let b = previous_row_start.map_or(0, |p| out[p + i]);
            let c = if i >= bpp {
                previous_row_start.map_or(0, |p| out[p + i - bpp])
            } else {
                0
            };
            let recon = match filter_type {
                0 => x,
                1 => x.wrapping_add(a),
                2 => x.wrapping_add(b),
                3 => {
                    let average = u16::midpoint(u16::from(a), u16::from(b));
                    x.wrapping_add(u8::try_from(average).unwrap_or(u8::MAX))
                }
                4 => x.wrapping_add(paeth_predictor(a, b, c)),
                _ => return Err(DecodeError::InvalidFilterType),
            };
            out[out_start + i] = recon;
        }
        previous_row_start = Some(out_start);
    }
    Ok(out)
}

/// Extract sample `channel` (of `channels`) for pixel `x` from a
/// defiltered row, at `bit_depth`.
///
/// Sub-byte depths (1, 2, 4) are unpacked most-significant-bit first
/// (W3C PNG §"Bit depth"), and always carry a single channel (greyscale or
/// palette index).
fn extract_sample(row: &[u8], x: u32, bit_depth: u8, channel: u32, channels: u32) -> Option<u16> {
    match bit_depth {
        1 | 2 | 4 => {
            let depth = u32::from(bit_depth);
            let bit_offset = x.checked_mul(depth)?;
            let byte_index = usize::try_from(bit_offset / 8).ok()?;
            let bit_in_byte = bit_offset % 8;
            let byte = u32::from(*row.get(byte_index)?);
            let shift = 8 - depth - bit_in_byte;
            let mask = (1u32 << depth) - 1;
            u16::try_from((byte >> shift) & mask).ok()
        }
        8 => {
            let index = usize::try_from(x.checked_mul(channels)?.checked_add(channel)?).ok()?;
            row.get(index).copied().map(u16::from)
        }
        16 => {
            let sample_index = x.checked_mul(channels)?.checked_add(channel)?;
            let byte_index = usize::try_from(sample_index.checked_mul(2)?).ok()?;
            let hi = *row.get(byte_index)?;
            let lo = *row.get(byte_index + 1)?;
            Some(u16::from_be_bytes([hi, lo]))
        }
        _ => None,
    }
}

/// Scale a native-bit-depth sample to an 8-bit channel value: the high byte
/// for 16-bit samples, the value itself for 8-bit samples, and `sample *
/// 255 / max` for sub-byte depths (W3C PNG §"Recommendations for Decoders").
fn scale_to_8bit(sample: u16, bit_depth: u8) -> u8 {
    match bit_depth {
        16 => u8::try_from(sample >> 8).unwrap_or(u8::MAX),
        8 => u8::try_from(sample).unwrap_or(u8::MAX),
        _ => {
            let max = (1u32 << u32::from(bit_depth)) - 1;
            let scaled = (u32::from(sample) * 255) / max.max(1);
            u8::try_from(scaled.min(255)).unwrap_or(u8::MAX)
        }
    }
}

/// Assemble one pixel's straight-alpha RGBA8 quad from a defiltered row.
///
/// Colour-key transparency (`tRNS` for greyscale/truecolour) is compared at
/// the image's native bit depth, before any 8-bit scaling.
fn pixel_rgba(
    row: &[u8],
    pixel_x: u32,
    ihdr: &Ihdr,
    palette: Option<&[[u8; 3]]>,
    trns: Option<&Trns>,
) -> Result<[u8; 4], DecodeError> {
    let channels = ihdr.colour_type.channels();
    let depth = ihdr.bit_depth;
    let sample = |channel: u32| {
        extract_sample(row, pixel_x, depth, channel, channels)
            .ok_or(DecodeError::CompressedSizeMismatch)
    };

    match ihdr.colour_type {
        ColourType::Grey => {
            let grey = sample(0)?;
            let colour = scale_to_8bit(grey, depth);
            let alpha = match trns {
                Some(Trns::GreyKey(key)) if *key == grey => 0,
                _ => 255,
            };
            Ok([colour, colour, colour, alpha])
        }
        ColourType::Truecolour => {
            let red = sample(0)?;
            let green = sample(1)?;
            let blue = sample(2)?;
            let alpha = match trns {
                Some(Trns::RgbKey(kr, kg, kb)) if *kr == red && *kg == green && *kb == blue => 0,
                _ => 255,
            };
            Ok([
                scale_to_8bit(red, depth),
                scale_to_8bit(green, depth),
                scale_to_8bit(blue, depth),
                alpha,
            ])
        }
        ColourType::Indexed => {
            let index = sample(0)?;
            let index = usize::from(index);
            let palette = palette.ok_or(DecodeError::PaletteRequired)?;
            let entry = palette
                .get(index)
                .ok_or(DecodeError::PaletteIndexOutOfRange)?;
            let alpha = match trns {
                Some(Trns::Indexed(alphas)) => alphas.get(index).copied().unwrap_or(255),
                _ => 255,
            };
            Ok([entry[0], entry[1], entry[2], alpha])
        }
        ColourType::GreyAlpha => {
            let grey = sample(0)?;
            let alpha_sample = sample(1)?;
            let colour = scale_to_8bit(grey, depth);
            Ok([colour, colour, colour, scale_to_8bit(alpha_sample, depth)])
        }
        ColourType::Rgba => {
            let red = sample(0)?;
            let green = sample(1)?;
            let blue = sample(2)?;
            let alpha_sample = sample(3)?;
            Ok([
                scale_to_8bit(red, depth),
                scale_to_8bit(green, depth),
                scale_to_8bit(blue, depth),
                scale_to_8bit(alpha_sample, depth),
            ])
        }
    }
}

/// Decompress `idat` and reconstruct the full straight-alpha RGBA8 pixel
/// buffer, honouring interlacing.
fn decode_pixels(
    ihdr: &Ihdr,
    palette: Option<&[[u8; 3]]>,
    trns: Option<&Trns>,
    idat: &[u8],
) -> Result<Vec<u8>, DecodeError> {
    let bpp_bits = bits_per_pixel(ihdr.colour_type, ihdr.bit_depth);
    let bpp = filter_bpp(bpp_bits);
    let passes = passes_for(ihdr);

    let expected = expected_decompressed_len(&passes, bpp_bits)?;
    let expected_len = to_usize64(expected)?;
    let mut raw = vec![0u8; expected_len];
    let produced = tairix_compress::zlib::decompress_into(idat, &mut raw)
        .map_err(DecodeError::CompressedData)?;
    if produced != expected_len {
        return Err(DecodeError::CompressedSizeMismatch);
    }

    let pixel_count = u64::from(ihdr.width)
        .checked_mul(u64::from(ihdr.height))
        .ok_or(DecodeError::DimensionsOverflow)?;
    let output_len = to_usize64(
        pixel_count
            .checked_mul(4)
            .ok_or(DecodeError::DimensionsOverflow)?,
    )?;
    let mut output = vec![0u8; output_len];
    let width = to_usize(ihdr.width)?;

    let mut raw_pos = 0usize;
    for pass in &passes {
        if pass.width == 0 || pass.height == 0 {
            continue;
        }
        let row_sample_bytes = to_usize64(sample_bytes_per_row(u64::from(pass.width), bpp_bits)?)?;
        let pass_len = row_sample_bytes
            .checked_add(1)
            .and_then(|full| full.checked_mul(to_usize(pass.height).ok()?))
            .ok_or(DecodeError::DimensionsOverflow)?;
        let pass_raw = raw
            .get(raw_pos..raw_pos + pass_len)
            .ok_or(DecodeError::CompressedSizeMismatch)?;
        raw_pos += pass_len;

        let defiltered = defilter_pass(pass_raw, pass.height, row_sample_bytes, bpp)?;

        for y in 0..pass.height {
            let row_start = to_usize(y)?
                .checked_mul(row_sample_bytes)
                .ok_or(DecodeError::DimensionsOverflow)?;
            let row = defiltered
                .get(row_start..row_start + row_sample_bytes)
                .ok_or(DecodeError::CompressedSizeMismatch)?;
            for x in 0..pass.width {
                let rgba = pixel_rgba(row, x, ihdr, palette, trns)?;
                let out_x = to_usize(placed_coordinate(pass.col_start, x, pass.col_step)?)?;
                let out_y = to_usize(placed_coordinate(pass.row_start, y, pass.row_step)?)?;
                let index = (out_y * width + out_x) * 4;
                output
                    .get_mut(index..index + 4)
                    .ok_or(DecodeError::CompressedSizeMismatch)?
                    .copy_from_slice(&rgba);
            }
        }
    }
    Ok(output)
}

#[cfg(test)]
#[path = "png_tests.rs"]
mod tests;
