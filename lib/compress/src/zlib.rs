//! RFC 1950 zlib envelope decoding.
//!
//! A zlib stream wraps a raw [`crate::inflate`] DEFLATE stream in a 2-byte
//! header and a 4-byte trailing checksum. This is the container PNG's
//! `IDAT` data uses (W3C PNG §"Filtering"/"Compression"), so — like
//! [`crate::inflate`] itself — this module exists purely for **decode-only**
//! interoperability with a foreign format; TAIRiX never produces a zlib
//! stream of its own.
//!
//! # Wire format
//!
//! ```text
//! [ CMF : 1 ][ FLG : 1 ][ compressed data (DEFLATE) ][ ADLER32 : 4, big-endian ]
//! ```
//!
//! `CMF` (compression method and flags) packs the compression method in its
//! low nibble (must be `8`, "deflate") and `CINFO` — the base-2 log of the
//! LZ77 window size minus 8 — in its high nibble; RFC 1950 bounds `CINFO` to
//! `7` (a 32 KiB window, the largest DEFLATE window). `FLG` packs `FCHECK`
//! (chosen by the encoder so that the 16-bit big-endian value `CMF:FLG` is a
//! multiple of 31 — the header's only integrity check), `FDICT` (a preset
//! dictionary was used), and `FLEVEL` (a compression-effort hint with no
//! effect on decoding). [`decompress_into`] refuses `FDICT`: a preset
//! dictionary is an out-of-band value neither PNG nor any other TAIRiX
//! consumer of this module supplies, so a stream requesting one cannot be
//! decoded correctly and is rejected rather than silently decoded wrong.
//!
//! # Trailing-byte policy
//!
//! [`crate::inflate::inflate_into_consumed`] reports exactly how many bytes
//! of the compressed body it consumed, which is where this module expects
//! to find the big-endian Adler-32 trailer. A stream with fewer than four
//! bytes left there is refused as [`Error::MissingTrailer`] — a missing or
//! truncated checksum is never treated as "no checksum to verify". Bytes
//! after the trailer are ignored, exactly as [`crate::inflate`] ignores
//! bytes after the DEFLATE stream it decoded; `IDAT` chunk concatenation in
//! `lib/image` relies on this to frame ancillary chunks around a zlib
//! stream without this module needing to know the outer container.

use crate::inflate;

/// Why zlib decoding failed.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum Error {
    /// `src` was too short to even hold the 2-byte header.
    HeaderTooShort,
    /// `CMF`'s low nibble (compression method) was not `8` ("deflate").
    UnsupportedCompressionMethod,
    /// `CMF`'s high nibble (`CINFO`) exceeded `7` (a window larger than
    /// DEFLATE's maximum 32 KiB).
    WindowTooLarge,
    /// The `CMF:FLG` 16-bit value was not a multiple of 31 (`FCHECK`
    /// failed) — the header is corrupt or this is not a zlib stream.
    HeaderCheckFailed,
    /// `FDICT` was set: the stream requires a preset dictionary, which no
    /// caller of this module supplies.
    PresetDictionaryUnsupported,
    /// The wrapped DEFLATE body failed to decode.
    Body(inflate::Error),
    /// Fewer than four bytes remained after the DEFLATE body for the
    /// Adler-32 trailer.
    MissingTrailer,
    /// The trailer's Adler-32 did not match the decompressed output.
    ChecksumMismatch,
}

impl core::fmt::Display for Error {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::HeaderTooShort => f.write_str("zlib header is truncated"),
            Self::UnsupportedCompressionMethod => {
                f.write_str("zlib compression method is not deflate")
            }
            Self::WindowTooLarge => f.write_str("zlib window size exceeds the deflate maximum"),
            Self::HeaderCheckFailed => f.write_str("zlib header check (FCHECK) failed"),
            Self::PresetDictionaryUnsupported => {
                f.write_str("zlib preset dictionary (FDICT) is not supported")
            }
            Self::Body(inner) => write!(f, "zlib body: {inner}"),
            Self::MissingTrailer => f.write_str("zlib stream is missing its Adler-32 trailer"),
            Self::ChecksumMismatch => f.write_str("zlib Adler-32 checksum mismatch"),
        }
    }
}

/// Bytes in the fixed `CMF`/`FLG` header.
const HEADER_LEN: usize = 2;

/// Bytes in the trailing Adler-32 checksum.
const TRAILER_LEN: usize = 4;

/// The modulus Adler-32 sums are reduced under (RFC 1950 §"ADLER32
/// checksum"): the largest prime below `2^16`.
const ADLER_MOD: u32 = 65_521;

/// Bytes summed between modulus reductions.
///
/// Reducing `a`/`b` after every byte is correct but pays a division per
/// byte; both accumulators fit in a `u32` without overflowing for up to
/// [`ADLER_NMAX`] consecutive additions of a `u8` and a running sum, which
/// is the standard Adler-32 blocking bound, so batching the reduction is a
/// free win on any input long enough for it to matter.
const ADLER_NMAX: usize = 5552;

/// The Adler-32 checksum of `data` (RFC 1950), zlib's own trailer checksum
/// — distinct from `lib/crc32c`'s CRC-32C (a different algorithm entirely,
/// used by TAIRiX's own on-disk formats) and from `lib/image`'s private
/// CRC-32 (PNG's own, unrelated, framing checksum).
#[must_use]
pub fn adler32(data: &[u8]) -> u32 {
    let mut a: u32 = 1;
    let mut b: u32 = 0;
    for chunk in data.chunks(ADLER_NMAX) {
        for &byte in chunk {
            a += u32::from(byte);
            b += a;
        }
        a %= ADLER_MOD;
        b %= ADLER_MOD;
    }
    (b << 16) | a
}

/// Decompress the zlib stream in `src` into `dst`, returning the number of
/// bytes produced.
///
/// The header is validated (compression method, window size, `FCHECK`,
/// `FDICT`) before any byte of the body is touched; the wrapped DEFLATE
/// body is then decompressed exactly as [`inflate::inflate_into`] would,
/// and finally the Adler-32 trailer immediately following the body is
/// verified over exactly the bytes produced. A stream failing any of these
/// checks — including one missing its trailer outright — is refused rather
/// than accepted with an unverified body.
///
/// # Errors
///
/// See [`Error`] for every fail-closed refusal reason.
pub fn decompress_into(src: &[u8], dst: &mut [u8]) -> Result<usize, Error> {
    if src.len() < HEADER_LEN {
        return Err(Error::HeaderTooShort);
    }
    let cmf = src[0];
    let flg = src[1];

    if cmf & 0x0F != 8 {
        return Err(Error::UnsupportedCompressionMethod);
    }
    if cmf >> 4 > 7 {
        return Err(Error::WindowTooLarge);
    }
    let header = (u16::from(cmf) << 8) | u16::from(flg);
    if header % 31 != 0 {
        return Err(Error::HeaderCheckFailed);
    }
    if flg & 0x20 != 0 {
        return Err(Error::PresetDictionaryUnsupported);
    }

    let body = &src[HEADER_LEN..];
    let (produced, consumed) = inflate::inflate_into_consumed(body, dst).map_err(Error::Body)?;

    let trailer_end = consumed
        .checked_add(TRAILER_LEN)
        .ok_or(Error::MissingTrailer)?;
    let trailer = body
        .get(consumed..trailer_end)
        .ok_or(Error::MissingTrailer)?;
    let expected = u32::from_be_bytes([trailer[0], trailer[1], trailer[2], trailer[3]]);
    let actual = adler32(&dst[..produced]);
    if actual != expected {
        return Err(Error::ChecksumMismatch);
    }
    Ok(produced)
}

#[cfg(test)]
#[path = "zlib_tests.rs"]
mod tests;
