//! First-party TAIRiX raster-image decoding (`lib/image`).
//!
//! The desktop's application-icon pipeline decodes a bundle's own icon
//! artwork — SVG or PNG — inside a minimum-capability parser sandbox before
//! it ever touches the compositor (a bundle is untrusted input: its icon
//! ships from whoever authored the `.app`, not from the system). This crate
//! is the PNG half of that pipeline: a complete, `no_std` + `alloc`,
//! `unsafe`-free PNG decoder that turns an untrusted byte stream into a
//! validated, straight-alpha RGBA8 pixel buffer, or a typed refusal —
//! never a panic, and never more memory than the caller allows.
//!
//! # Design
//!
//! [`decode`] dispatches on the format [`sniff`] recognises from a byte
//! signature; today that is PNG only ([`ImageFormat::Png`]), decoded by the
//! private `png` module. Growing the format list to (say) JPEG is not
//! speculative interface here: [`ImageFormat`] stays closed until a real
//! consumer needs a second format, exactly as the icon pipeline needs PNG
//! today.
//!
//! [`RasterImage`] is the one output shape every format decodes into: a
//! row-major, 4-byte-per-pixel, **straight-alpha** RGBA8 buffer (not
//! premultiplied — `lib/raster`'s `Surface::from_rgba8` is where
//! premultiplication happens, once, on the consumer side). Keeping the
//! decoder's output straight-alpha means a decoder never needs to know
//! anything about the compositor's internal pixel representation.
//!
//! # Bounds and fail-closed policy
//!
//! [`DecodeLimits`] is the caller's ceiling on the image this crate will
//! ever produce: a maximum width, height, and total pixel count. A format
//! decoder checks its declared dimensions against the limits **the moment
//! it reads them** — before allocating a single scanline, palette entry, or
//! output pixel — so a hostile "16384×16384 declared, 12 bytes of actual
//! data" file cannot make this crate reserve memory proportional to the lie
//! rather than the bytes actually present. Every other declared size (a
//! chunk length, a decompressed-image byte count, a palette entry count) is
//! validated against the bytes remaining in the input, or against a size
//! computed purely from the already-bounded geometry, before it is used to
//! index or allocate anything.
//!
//! Every public entry point is total: malformed, truncated, or adversarial
//! input returns a [`DecodeError`] variant, never a panic. All size and
//! offset arithmetic over untrusted values uses checked or widened integer
//! operations, so a crafted input cannot provoke an overflow panic even in
//! a debug build.

#![no_std]
#![forbid(unsafe_code)]
#![deny(missing_docs)]

extern crate alloc;

use alloc::vec::Vec;

mod crc32;
mod png;

/// Why decoding an image failed. Every variant is a fail-closed refusal:
/// no malformed, truncated, or adversarial input ever panics or produces a
/// partially-decoded image.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DecodeError {
    /// [`sniff`] did not recognise any supported format's signature.
    UnknownFormat,
    /// The declared width exceeded [`DecodeLimits::max_width`].
    WidthExceedsLimit,
    /// The declared height exceeded [`DecodeLimits::max_height`].
    HeightExceedsLimit,
    /// The declared width × height exceeded [`DecodeLimits::max_pixels`].
    PixelCountExceedsLimit,
    /// A size computed from otherwise-valid, bounded geometry overflowed a
    /// 64-bit integer — reachable only with degenerate, very large
    /// caller-supplied [`DecodeLimits`], never with a sane limit.
    DimensionsOverflow,

    /// The file did not begin with the PNG signature.
    BadSignature,
    /// A chunk's header or declared payload ran past the end of the input.
    ChunkTruncated,
    /// A chunk declared a length longer than the bytes remaining in the
    /// input.
    ChunkLengthExceedsInput,
    /// A chunk's CRC-32 did not match its type and payload.
    ChunkCrcMismatch,
    /// A chunk appeared after `IEND`.
    DataAfterEnd,
    /// A chunk whose type is a critical chunk (uppercase first letter) but
    /// is not one this decoder understands.
    UnknownCriticalChunk,

    /// The first chunk after the signature was not `IHDR`.
    HeaderNotFirst,
    /// A second `IHDR` chunk appeared.
    DuplicateHeader,
    /// No `IHDR` chunk was present.
    MissingHeader,
    /// A `PLTE` chunk appeared after the first `IDAT` chunk.
    PaletteAfterImageData,
    /// A second `PLTE` chunk appeared.
    DuplicatePalette,
    /// A second `tRNS` chunk appeared.
    DuplicateTransparency,
    /// An `IDAT` chunk appeared after a non-`IDAT` chunk had already
    /// followed an earlier `IDAT` (the `IDAT` chunks were not contiguous).
    ImageDataNotContiguous,
    /// No `IDAT` chunk was present.
    MissingImageData,
    /// No `IEND` chunk was present.
    MissingEnd,
    /// `IEND` carried a non-empty payload.
    MalformedEnd,

    /// `IHDR`'s payload was not exactly 13 bytes.
    InvalidIhdrLength,
    /// `IHDR` declared a width or height of zero.
    ZeroDimension,
    /// `IHDR`'s bit depth was not one of `1`, `2`, `4`, `8`, or `16`.
    InvalidBitDepth,
    /// `IHDR`'s colour type was not one of `0`, `2`, `3`, `4`, or `6`.
    InvalidColourType,
    /// The (colour type, bit depth) combination is not one the PNG
    /// specification permits.
    UnsupportedColourTypeAndDepth,
    /// `IHDR`'s compression method was not `0`.
    InvalidCompressionMethod,
    /// `IHDR`'s filter method was not `0`.
    InvalidFilterMethod,
    /// `IHDR`'s interlace method was not `0` (none) or `1` (Adam7).
    InvalidInterlaceMethod,

    /// `PLTE` is required for an indexed-colour (`colour type 3`) image but
    /// was absent.
    PaletteRequired,
    /// `PLTE` appeared for a colour type (`0` or `4`) that forbids it.
    PaletteForbidden,
    /// `PLTE`'s payload length was not a positive multiple of 3 no greater
    /// than 768 bytes (1..=256 entries).
    InvalidPaletteLength,
    /// `tRNS` appeared for a colour type (`4` or `6`) that already carries
    /// an explicit alpha channel.
    TransparencyForbidden,
    /// `tRNS`'s payload length did not match what its colour type requires
    /// (2 bytes for greyscale, 6 for truecolour, at most the palette length
    /// for indexed).
    InvalidTransparencyLength,

    /// The `IDAT` stream failed to zlib-decompress.
    CompressedData(tairix_compress::zlib::Error),
    /// The decompressed `IDAT` stream was not exactly the size the image's
    /// geometry implies.
    CompressedSizeMismatch,
    /// A scanline's filter-type byte was not one of the five PNG filters
    /// (`0`..=`4`).
    InvalidFilterType,
    /// An indexed-colour sample referenced a palette entry beyond the end
    /// of the palette.
    PaletteIndexOutOfRange,
}

impl core::fmt::Display for DecodeError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::UnknownFormat => f.write_str("unrecognised image format"),
            Self::WidthExceedsLimit => f.write_str("image width exceeds the caller's limit"),
            Self::HeightExceedsLimit => f.write_str("image height exceeds the caller's limit"),
            Self::PixelCountExceedsLimit => {
                f.write_str("image pixel count exceeds the caller's limit")
            }
            Self::DimensionsOverflow => f.write_str("image geometry overflowed size arithmetic"),
            Self::BadSignature => f.write_str("not a PNG file (bad signature)"),
            Self::ChunkTruncated => f.write_str("PNG chunk is truncated"),
            Self::ChunkLengthExceedsInput => {
                f.write_str("PNG chunk declares a length longer than the input")
            }
            Self::ChunkCrcMismatch => f.write_str("PNG chunk CRC-32 mismatch"),
            Self::DataAfterEnd => f.write_str("PNG data follows the IEND chunk"),
            Self::UnknownCriticalChunk => f.write_str("PNG has an unknown critical chunk"),
            Self::HeaderNotFirst => f.write_str("PNG's first chunk is not IHDR"),
            Self::DuplicateHeader => f.write_str("PNG has more than one IHDR chunk"),
            Self::MissingHeader => f.write_str("PNG has no IHDR chunk"),
            Self::PaletteAfterImageData => f.write_str("PNG's PLTE chunk follows its first IDAT"),
            Self::DuplicatePalette => f.write_str("PNG has more than one PLTE chunk"),
            Self::DuplicateTransparency => f.write_str("PNG has more than one tRNS chunk"),
            Self::ImageDataNotContiguous => f.write_str("PNG's IDAT chunks are not contiguous"),
            Self::MissingImageData => f.write_str("PNG has no IDAT chunk"),
            Self::MissingEnd => f.write_str("PNG has no IEND chunk"),
            Self::MalformedEnd => f.write_str("PNG's IEND chunk is not empty"),
            Self::InvalidIhdrLength => f.write_str("PNG IHDR chunk has the wrong length"),
            Self::ZeroDimension => f.write_str("PNG declares a zero width or height"),
            Self::InvalidBitDepth => f.write_str("PNG declares an invalid bit depth"),
            Self::InvalidColourType => f.write_str("PNG declares an invalid colour type"),
            Self::UnsupportedColourTypeAndDepth => {
                f.write_str("PNG's colour type and bit depth combination is not permitted")
            }
            Self::InvalidCompressionMethod => {
                f.write_str("PNG declares an unsupported compression method")
            }
            Self::InvalidFilterMethod => f.write_str("PNG declares an unsupported filter method"),
            Self::InvalidInterlaceMethod => {
                f.write_str("PNG declares an unsupported interlace method")
            }
            Self::PaletteRequired => f.write_str("PNG is indexed-colour but has no PLTE chunk"),
            Self::PaletteForbidden => f.write_str("PNG's colour type does not permit a PLTE chunk"),
            Self::InvalidPaletteLength => f.write_str("PNG's PLTE chunk has an invalid length"),
            Self::TransparencyForbidden => {
                f.write_str("PNG's colour type does not permit a tRNS chunk")
            }
            Self::InvalidTransparencyLength => {
                f.write_str("PNG's tRNS chunk has an invalid length")
            }
            Self::CompressedData(inner) => write!(f, "PNG image data: {inner}"),
            Self::CompressedSizeMismatch => {
                f.write_str("PNG's decompressed image data has the wrong size")
            }
            Self::InvalidFilterType => f.write_str("PNG scanline has an invalid filter type"),
            Self::PaletteIndexOutOfRange => {
                f.write_str("PNG pixel references a palette entry out of range")
            }
        }
    }
}

/// The caller's ceiling on the image [`decode`] will ever produce.
///
/// A format decoder checks a declared width and height against these
/// limits **before** allocating any scanline, palette, or output buffer, so
/// a file that lies about its dimensions cannot make this crate reserve
/// memory proportional to the lie.
// The shared `max` prefix names exactly what this struct is: three ceilings
// on the image `decode` will ever produce. Stripping it (to `width`,
// `height`, `pixels`) would read as the image's *actual* geometry rather
// than its limit, so the prefix stays despite the lint's default advice.
#[allow(clippy::struct_field_names)]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct DecodeLimits {
    max_width: u32,
    max_height: u32,
    max_pixels: u64,
}

impl DecodeLimits {
    /// Construct a limit set.
    #[must_use]
    pub const fn new(max_width: u32, max_height: u32, max_pixels: u64) -> Self {
        Self {
            max_width,
            max_height,
            max_pixels,
        }
    }

    /// The maximum permitted width, in pixels.
    #[must_use]
    pub const fn max_width(&self) -> u32 {
        self.max_width
    }

    /// The maximum permitted height, in pixels.
    #[must_use]
    pub const fn max_height(&self) -> u32 {
        self.max_height
    }

    /// The maximum permitted total pixel count (`width * height`).
    #[must_use]
    pub const fn max_pixels(&self) -> u64 {
        self.max_pixels
    }

    /// Check `width`/`height` against every limit, fail closed the moment
    /// one is exceeded, before the caller allocates anything for them.
    fn check(&self, width: u32, height: u32) -> Result<(), DecodeError> {
        if width == 0 || height == 0 {
            return Err(DecodeError::ZeroDimension);
        }
        if width > self.max_width {
            return Err(DecodeError::WidthExceedsLimit);
        }
        if height > self.max_height {
            return Err(DecodeError::HeightExceedsLimit);
        }
        let pixels = u64::from(width)
            .checked_mul(u64::from(height))
            .ok_or(DecodeError::DimensionsOverflow)?;
        if pixels > self.max_pixels {
            return Err(DecodeError::PixelCountExceedsLimit);
        }
        Ok(())
    }
}

/// A decoded raster image: a row-major, 4-byte-per-pixel, **straight**
/// (non-premultiplied) alpha RGBA8 pixel buffer.
///
/// Straight alpha is a deliberate contract: `lib/raster::Surface` owns the
/// crate's one premultiplication path (`Surface::from_rgba8`), so a decoder
/// never needs to know anything about how its consumer composites pixels.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RasterImage {
    width: u32,
    height: u32,
    pixels: Vec<u8>,
}

impl RasterImage {
    /// Build a [`RasterImage`] from already-validated geometry and exactly
    /// `width * height * 4` pixel bytes.
    ///
    /// Private to the crate: every format decoder produces a buffer sized
    /// to its own already-checked geometry, so this never needs to
    /// re-validate the invariant it assumes.
    pub(crate) fn from_parts(width: u32, height: u32, pixels: Vec<u8>) -> Self {
        Self {
            width,
            height,
            pixels,
        }
    }

    /// The image width, in pixels.
    #[must_use]
    pub const fn width(&self) -> u32 {
        self.width
    }

    /// The image height, in pixels.
    #[must_use]
    pub const fn height(&self) -> u32 {
        self.height
    }

    /// Borrow the row-major RGBA8 pixel bytes (straight alpha).
    #[must_use]
    pub fn pixels(&self) -> &[u8] {
        &self.pixels
    }

    /// Take ownership of the row-major RGBA8 pixel bytes (straight alpha).
    #[must_use]
    pub fn into_pixels(self) -> Vec<u8> {
        self.pixels
    }
}

/// A raster image format this crate can decode.
///
/// Deliberately closed, and grows only with a real consumer: today the
/// desktop icon pipeline only ever hands this crate PNG artwork.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum ImageFormat {
    /// The Portable Network Graphics format (W3C PNG specification).
    Png,
}

/// The 8-byte PNG file signature (W3C PNG §"PNG file signature").
const PNG_SIGNATURE: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];

/// Identify the format of `bytes` from its leading signature, or `None` if
/// no supported format is recognised.
#[must_use]
pub fn sniff(bytes: &[u8]) -> Option<ImageFormat> {
    if bytes.starts_with(&PNG_SIGNATURE) {
        return Some(ImageFormat::Png);
    }
    None
}

/// Decode `bytes` into a [`RasterImage`], honouring `limits`.
///
/// The format is chosen by [`sniff`]; an unrecognised signature is refused
/// as [`DecodeError::UnknownFormat`] before any format-specific parsing
/// runs. See the crate documentation for the bounds and fail-closed policy
/// every format decoder follows.
///
/// # Errors
///
/// See [`DecodeError`] for every fail-closed refusal reason.
pub fn decode(bytes: &[u8], limits: &DecodeLimits) -> Result<RasterImage, DecodeError> {
    match sniff(bytes) {
        Some(ImageFormat::Png) => png::decode(bytes, limits),
        None => Err(DecodeError::UnknownFormat),
    }
}

#[cfg(test)]
mod tests {
    use super::{decode, sniff, DecodeError, DecodeLimits, ImageFormat};

    #[test]
    fn sniff_recognises_the_png_signature() {
        let mut bytes = super::PNG_SIGNATURE.to_vec();
        bytes.extend_from_slice(b"anything after the signature");
        assert_eq!(sniff(&bytes), Some(ImageFormat::Png));
    }

    #[test]
    fn sniff_rejects_an_unknown_signature() {
        assert_eq!(sniff(b"not a supported image format"), None);
        assert_eq!(sniff(b""), None);
    }

    #[test]
    fn decode_refuses_an_unknown_format_before_any_parsing() {
        let limits = DecodeLimits::new(64, 64, 4096);
        assert_eq!(
            decode(b"definitely not an image", &limits),
            Err(DecodeError::UnknownFormat)
        );
    }

    #[test]
    fn limits_check_rejects_zero_dimensions_and_over_limit_geometry() {
        let limits = DecodeLimits::new(8, 8, 32);
        assert_eq!(limits.check(0, 4), Err(DecodeError::ZeroDimension));
        assert_eq!(limits.check(4, 0), Err(DecodeError::ZeroDimension));
        assert_eq!(limits.check(9, 4), Err(DecodeError::WidthExceedsLimit));
        assert_eq!(limits.check(4, 9), Err(DecodeError::HeightExceedsLimit));
        assert_eq!(limits.check(8, 8), Err(DecodeError::PixelCountExceedsLimit));
        assert_eq!(limits.check(4, 4), Ok(()));
    }

    #[test]
    fn limits_accessors_return_the_constructed_values() {
        let limits = DecodeLimits::new(10, 20, 200);
        assert_eq!(limits.max_width(), 10);
        assert_eq!(limits.max_height(), 20);
        assert_eq!(limits.max_pixels(), 200);
    }
}
