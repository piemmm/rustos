//! First-party TAIRiX raster-image decoding (`lib/image`).
//!
//! The desktop's application-icon pipeline decodes a bundle's own icon
//! artwork — SVG or PNG — inside a minimum-capability parser sandbox before
//! it ever touches the compositor (a bundle is untrusted input: its icon
//! ships from whoever authored the `.app`, not from the system), and the
//! desktop pinboard decodes a wallpaper — a shipped master or a file the
//! user picked — the same way. This crate is the raster half of both
//! pipelines: complete, `no_std` + `alloc`, `unsafe`-free decoders that turn
//! an untrusted byte stream into a validated, straight-alpha RGBA8 pixel
//! buffer, or a typed refusal — never a panic, and never more memory than the
//! caller allows.
//!
//! # Design
//!
//! [`decode`] and [`decode_fitted`] dispatch on the format [`sniff`]
//! recognises from a byte signature: PNG ([`ImageFormat::Png`]), JPEG
//! ([`ImageFormat::Jpeg`]), GIF ([`ImageFormat::Gif`]), BMP
//! ([`ImageFormat::Bmp`]), and the icon and cursor containers
//! ([`ImageFormat::Ico`]), each decoded by its own private module — BMP and
//! ICO by one, because an icon's entries are the bitmaps BMP already reads.
//! [`ImageFormat`] stays closed and grows only with a real consumer, exactly
//! as PNG was added for the icon pipeline, JPEG for the wallpaper masters,
//! and the rest for the picture viewer. Being the one raster
//! registry is what keeps a format's decoder in a single place: a consumer
//! that decides to admit a further one needs no decoder of its own — though
//! admitting it is that consumer's decision, and the icon pipeline
//! deliberately admits only PNG and SVG.
//!
//! # Naming a format instead of sniffing it
//!
//! Only a format that carries a signature can be recognised from content, and
//! a RISC OS sprite area ([`ImageFormat::Sprite`]) does not: its first word is
//! the sprite count, and RISC OS types a file from its directory entry. So
//! [`sniff`] never answers it and never guesses one from a structural
//! coincidence — a heuristic there would be a false-positive machine, and one
//! this crate would then act on. A caller that already knows the type instead
//! names the format: [`probe_as`], [`decode_as`], and [`Sequence::open_as`]
//! take an [`ImageFormat`] in place of the sniff, and the sniffing entry
//! points are exactly [`sniff`] plus those. The named format's own parser
//! still validates the bytes, so naming the wrong one is refused rather than
//! misread.
//!
//! # Sequences and pages
//!
//! Some containers hold more than one picture. [`Sequence`] is the one shape
//! for all of them: [`Sequence::open`] validates the structure and reports
//! [`SequenceInfo`], and [`Sequence::next_frame`] decodes the entries in
//! order. Stepping is forward-only with [`Sequence::rewind`], because that is
//! what an animation *is* — a frame composites onto its predecessors under
//! the container's own disposal model, so being able to ask for frame *n*
//! directly would mean re-compositing every frame before it. A page
//! container's entries are independent pictures instead, and choosing
//! between them is the point, so [`Sequence::page`] addresses one directly.
//! A still picture is the one-entry case of the same shape, so a consumer
//! that shows pictures, animations, and icon files needs one path rather
//! than three.
//!
//! [`RasterImage`] is the one output shape every format decodes into: a
//! row-major, 4-byte-per-pixel, **straight-alpha** RGBA8 buffer (not
//! premultiplied — `lib/raster`'s `Surface::from_rgba8` is where
//! premultiplication happens, once, on the consumer side). Keeping the
//! decoder's output straight-alpha means a decoder never needs to know
//! anything about the compositor's internal pixel representation.
//!
//! # Reduced-scale decoding
//!
//! [`decode`] always produces an image at its natural (full) size, and is
//! refused outright when that size breaches the caller's limits.
//! [`decode_fitted`] instead produces the smallest size a format's own
//! decode process can still cover a caller's [`FitBox`] with, which for
//! JPEG means choosing the coarsest DCT decode scale (one whole, one half,
//! one quarter, or one eighth) whose result covers the box on both axes,
//! computed via reduced inverse DCTs rather than a full decode followed by
//! a resample. Where even that scale's output would breach the limits,
//! [`decode_fitted`] degrades to the largest scale that fits rather than
//! refusing — a deliberate trade of sharpness for memory, decided from the
//! header's geometry before anything is allocated, and refused only when
//! not even the coarsest scale fits. An icon container has its own kind of
//! scale — it *is* one picture at several sizes — so a fitted decode there
//! takes the smallest entry covering the box rather than computing anything.
//! PNG, GIF, and BMP have no reduced-scale decode process at all — their
//! entropy coding and row layout do not separate into scale-selectable
//! passes the way a block transform does — so for those [`decode_fitted`] is
//! exactly [`decode`] and has no degradation to offer; that is an honest
//! property of the formats, not a gap this crate is missing.
//!
//! # Bounds and fail-closed policy
//!
//! [`DecodeLimits`] is the caller's ceiling on the image this crate will
//! ever produce: a maximum width, height, total pixel count, and — because
//! a progressive JPEG scan must buffer every coefficient of every
//! component before its final scan can produce a single pixel — a maximum
//! size for that coefficient store. A format decoder weighs the size it is
//! about to produce — the declared dimensions for [`decode`], the chosen
//! scale's output for [`decode_fitted`] — against the limits **the moment
//! it reads the header** and before allocating a single scanline, palette
//! entry, coefficient block, or output pixel, so a hostile "16384×16384
//! declared, 12 bytes of actual data" file cannot make this crate reserve
//! memory proportional to the lie rather than the bytes actually present.
//! Every other declared size (a chunk length, a decompressed-image byte
//! count, a palette entry count, a Huffman or quantisation table length) is
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

mod bmp;
mod channel;
mod crc32;
mod gif;
mod ico;
mod jpeg;
mod pages;
mod png;
#[cfg(test)]
mod png_fixture;
mod sprite;

/// Bytes one decoded pixel occupies: straight-alpha RGBA8, the one output
/// shape [`RasterImage`] and every format decoder here produce.
pub(crate) const RGBA_BYTES: usize = 4;

/// Limits a header probe holds a declared geometry to: none of its own.
///
/// A probe allocates nothing from the geometry it reports, so it has nothing
/// to protect by bounding it — its caller does, and applies its own bounds to
/// the answer. The zero-dimension refusal still applies, because a zero-sided
/// picture is malformed rather than merely large.
pub(crate) const PROBE_LIMITS: DecodeLimits = DecodeLimits::new(u32::MAX, u32::MAX, u64::MAX, 0);

/// The little-endian 16-bit value at `at`, or `None` where the input holds
/// fewer than two bytes there.
///
/// The one definition of a little-endian field read: the formats that use
/// one each name their own refusal for a missing field, but none of them
/// needs its own copy of the read.
pub(crate) fn le_u16(data: &[u8], at: usize) -> Option<u16> {
    data.get(at..)
        .and_then(<[u8]>::first_chunk::<2>)
        .map(|bytes| u16::from_le_bytes(*bytes))
}

/// The little-endian 32-bit value at `at`, or `None` where the input holds
/// fewer than four bytes there.
pub(crate) fn le_u32(data: &[u8], at: usize) -> Option<u32> {
    data.get(at..)
        .and_then(<[u8]>::first_chunk::<4>)
        .map(|bytes| u32::from_le_bytes(*bytes))
}

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
    /// A buffer the decode needs was refused by the allocator. Unlike every
    /// other variant this is a property of the machine, not of the input, so
    /// the same image may decode later.
    OutOfMemory,

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
    /// A width or height of zero was declared. Raised by the shared limits
    /// check, so it belongs to no one format.
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

    /// The file did not begin with the JPEG SOI marker (`0xFFD8`).
    JpegBadSignature,
    /// A marker's code byte, or its 2-byte segment length, ran past the
    /// end of the input.
    JpegMarkerTruncated,
    /// A segment declared a length shorter than the 2 bytes the length
    /// field itself always counts (ITU-T T.81 §B.1.1.4).
    JpegSegmentTooShort,
    /// A segment declared a length longer than the bytes remaining in the
    /// input.
    JpegSegmentLengthExceedsInput,
    /// A marker code this decoder does not recognise appeared where a
    /// marker was expected.
    JpegUnknownMarker,
    /// `SOF` declared arithmetic entropy coding: only Huffman coding is
    /// supported.
    JpegArithmeticCodingUnsupported,
    /// `SOF` declared a lossless or hierarchical (differential) frame:
    /// only baseline, extended sequential, and progressive DCT frames are
    /// supported.
    JpegLosslessOrHierarchicalUnsupported,
    /// `SOF` declared a sample precision other than 8 bits.
    JpegUnsupportedPrecision,
    /// `SOF` declared a component count other than 1 (greyscale) or 3
    /// (YCbCr, or RGB under an Adobe APP14 transform of zero).
    JpegUnsupportedComponentCount,
    /// A component declared a horizontal or vertical sampling factor of 0
    /// or greater than 4.
    JpegInvalidSamplingFactor,
    /// `SOF`'s payload length was inconsistent with its declared component
    /// count, or two components declared the same component id.
    JpegInvalidFrameHeader,
    /// A second `SOF` marker appeared: only one frame is supported.
    JpegDuplicateFrameHeader,
    /// A marker that requires a frame (`DHT`, `DRI`, or `SOS`) appeared
    /// before any `SOF`.
    JpegMissingFrameHeader,
    /// A `DNL` marker appeared: deferred height is not supported.
    JpegDnlUnsupported,
    /// `DQT` declared a table index greater than 3, an invalid element
    /// precision, or a payload length inconsistent with its element
    /// precision and table count.
    JpegInvalidQuantizationTable,
    /// A component's `SOF` entry, or a scan's own reference, named a
    /// quantisation table index that no `DQT` had loaded yet.
    JpegMissingQuantizationTable,
    /// `DHT` declared a table class or index greater than 3, or its
    /// code-length counts and symbol list do not form a valid canonical
    /// Huffman code (ITU-T T.81 Annex C).
    JpegInvalidHuffmanTable,
    /// A scan referenced a DC or AC Huffman table selector that no `DHT`
    /// had loaded yet.
    JpegMissingHuffmanTable,
    /// While decoding entropy-coded data, no Huffman code in the selected
    /// table matched the next bits of the stream.
    JpegHuffmanCodeNotFound,
    /// An AC run-length skip advanced past the last coefficient of a
    /// block's spectral band.
    JpegCoefficientRunOverflow,
    /// A scan's component selector named a component id absent from the
    /// frame header.
    JpegComponentIdMismatch,
    /// `SOS`'s component count, table selectors, spectral selection, or
    /// successive-approximation fields violated the scan header grammar,
    /// or a baseline/extended-sequential scan did not cover the full
    /// spectrum in one pass.
    JpegInvalidScanHeader,
    /// `DRI`'s payload was not exactly 2 bytes.
    JpegInvalidRestartInterval,
    /// A restart marker was missing where the declared restart interval
    /// required one, or carried the wrong cyclic sequence number.
    JpegRestartMarkerMismatch,
    /// The entropy-coded segment ran out of input before its scan's last
    /// coefficient (MCU-interleaved or non-interleaved) was decoded.
    JpegEntropyDataTruncated,
    /// The stream ended without an `EOI` marker.
    JpegMissingEndOfImage,
    /// The coefficient store a progressive scan must buffer would exceed
    /// [`DecodeLimits::max_progressive_coefficient_bytes`].
    JpegProgressiveCoefficientStoreExceedsLimit,

    /// The file did not begin with the three-byte `GIF` magic.
    GifBadSignature,
    /// The version field was neither `87a` nor `89a`.
    GifUnknownVersion,
    /// A block's header, declared payload, or data sub-block chain ran past
    /// the end of the input.
    GifTruncated,
    /// A block introducer was none of the image separator, the extension
    /// introducer, or the trailer.
    GifUnknownBlock,
    /// An extension block declared a block size the specification fixes at
    /// another value, or omitted its terminator.
    GifMalformedExtension,
    /// A Graphic Control Extension declared a disposal method in the
    /// specification's reserved range (`4`..=`7`).
    GifReservedDisposal,
    /// An Image Descriptor declared a zero width or height.
    GifZeroFrame,
    /// An Image Descriptor placed a frame partly or wholly outside the
    /// logical screen.
    GifFrameOutsideScreen,
    /// The block chain reached its trailer without a single image block.
    GifNoFrames,
    /// The block chain declared more frames than the decoder's fixed
    /// containment bound accepts.
    GifTooManyFrames,
    /// A frame carried no local colour table and the stream carried no global
    /// one, so its indices name no colours at all.
    GifMissingColourTable,
    /// A frame's pixel referenced a colour-table entry beyond the end of the
    /// table in force for it.
    GifPaletteIndexOutOfRange,
    /// An Image Descriptor's LZW minimum code size was outside `2`..=`8`.
    GifInvalidCodeSize,
    /// An LZW code was neither in the table nor the one the reading step
    /// would itself define.
    GifInvalidCode,
    /// A frame's LZW stream ended before it had produced every pixel the
    /// Image Descriptor declares.
    GifTruncatedImageData,

    /// The file did not begin with the two-byte `BM` magic.
    BmpBadSignature,
    /// A header, colour table, or mask field ran past the end of the input.
    BmpTruncated,
    /// The DIB header declared a length that is none of the six the Windows
    /// lineage defines (`BITMAPCOREHEADER` through `BITMAPV5HEADER`).
    BmpUnsupportedHeaderSize,
    /// The DIB header declared a negative width.
    BmpInvalidDimensions,
    /// The DIB header declared a colour-plane count other than 1.
    BmpInvalidPlanes,
    /// The DIB header declared a bit count that is not one of `1`, `2`, `4`,
    /// `8`, `16`, `24`, or `32`.
    BmpUnsupportedBitCount,
    /// The DIB header declared a compression this decoder does not claim:
    /// an embedded JPEG or PNG pixel array, a CMYK encoding, or a code the
    /// format does not define.
    BmpUnsupportedCompression,
    /// The declared compression and bit count contradict each other, or a
    /// run-length-encoded array declared top-down rows, which it may not.
    BmpCompressionMismatch,
    /// A bitfield mask left a colour channel unaddressed, reached outside
    /// the pixel, named a bit another channel already claimed, or held bits
    /// that are not contiguous.
    BmpInvalidMask,
    /// `biClrUsed` declared more colour-table entries than the bit count can
    /// index, or the table does not fit in the gap before the pixel array.
    BmpInvalidPaletteLength,
    /// `bfOffBits` pointed before the end of the header or past the end of
    /// the input.
    BmpInvalidPixelOffset,
    /// The pixel array held fewer bytes than the declared geometry needs.
    BmpPixelDataTruncated,
    /// A pixel referenced a colour-table entry beyond the end of the table.
    BmpPaletteIndexOutOfRange,
    /// A run-length-encoded run, delta, or line reached outside the picture.
    BmpRleOutOfBounds,
    /// A run-length-encoded array ended before it had covered every row.
    BmpRleTruncated,

    /// The file did not begin with an icon or cursor directory header.
    IcoBadSignature,
    /// The directory, or an entry's declared extent, ran past the end of the
    /// input.
    IcoTruncated,
    /// The directory declared no pictures at all.
    IcoNoEntries,
    /// An entry's bitmap declared an odd height, so it cannot be the colour
    /// rows and the mask over them that an icon's is.
    IcoInvalidMaskHeight,

    /// The sprite area's own header was malformed: its sprites start before
    /// it ends, end past the input, or its chain of control blocks does not
    /// advance within it.
    SpriteBadArea,
    /// A control block, palette, image, or mask ran past the end of the
    /// input.
    SpriteTruncated,
    /// The area declared no sprites at all.
    SpriteNoSprites,
    /// A sprite's mode word was not a valid sprite mode word: a mode
    /// selector pointer, a zero DPI field, a RISC OS 5 word whose fixed bits
    /// are wrong, or a pixel format asked for alpha it has no room for.
    SpriteInvalidModeWord,
    /// A sprite declared a numbered screen mode this decoder has no pixel
    /// format for: a Teletext mode, or a third-party extension mode.
    SpriteUnknownMode,
    /// A sprite declared a type outside the depths this decoder claims:
    /// CMYK, JPEG data, YCbCr, or a reserved number.
    SpriteUnsupportedType,
    /// A sprite's first or last used bit was out of range, off a pixel
    /// boundary, or left its rows holding no whole pixels.
    SpriteInvalidWastage,
}

impl DecodeError {
    /// This error's fixed message. [`DecodeError::CompressedData`] is the
    /// one variant whose message is a prefix rather than the whole line,
    /// since it carries an inner error that completes it.
    // One exhaustive table for a flat error enum, so the compiler is what
    // catches a variant with no message. Splitting it per format would need
    // each part to fall through for the others' variants, trading that check
    // for a line count.
    #[allow(clippy::too_many_lines)]
    fn message(&self) -> &'static str {
        match self {
            Self::UnknownFormat => "unrecognised image format",
            Self::WidthExceedsLimit => "image width exceeds the caller's limit",
            Self::HeightExceedsLimit => "image height exceeds the caller's limit",
            Self::PixelCountExceedsLimit => "image pixel count exceeds the caller's limit",
            Self::DimensionsOverflow => "image geometry overflowed size arithmetic",
            Self::OutOfMemory => "image decode buffer could not be allocated",
            Self::BadSignature => "not a PNG file (bad signature)",
            Self::ChunkTruncated => "PNG chunk is truncated",
            Self::ChunkLengthExceedsInput => "PNG chunk declares a length longer than the input",
            Self::ChunkCrcMismatch => "PNG chunk CRC-32 mismatch",
            Self::DataAfterEnd => "PNG data follows the IEND chunk",
            Self::UnknownCriticalChunk => "PNG has an unknown critical chunk",
            Self::HeaderNotFirst => "PNG's first chunk is not IHDR",
            Self::DuplicateHeader => "PNG has more than one IHDR chunk",
            Self::MissingHeader => "PNG has no IHDR chunk",
            Self::PaletteAfterImageData => "PNG's PLTE chunk follows its first IDAT",
            Self::DuplicatePalette => "PNG has more than one PLTE chunk",
            Self::DuplicateTransparency => "PNG has more than one tRNS chunk",
            Self::ImageDataNotContiguous => "PNG's IDAT chunks are not contiguous",
            Self::MissingImageData => "PNG has no IDAT chunk",
            Self::MissingEnd => "PNG has no IEND chunk",
            Self::MalformedEnd => "PNG's IEND chunk is not empty",
            Self::InvalidIhdrLength => "PNG IHDR chunk has the wrong length",
            Self::ZeroDimension => "image declares a zero width or height",
            Self::InvalidBitDepth => "PNG declares an invalid bit depth",
            Self::InvalidColourType => "PNG declares an invalid colour type",
            Self::UnsupportedColourTypeAndDepth => {
                "PNG's colour type and bit depth combination is not permitted"
            }
            Self::InvalidCompressionMethod => "PNG declares an unsupported compression method",
            Self::InvalidFilterMethod => "PNG declares an unsupported filter method",
            Self::InvalidInterlaceMethod => "PNG declares an unsupported interlace method",
            Self::PaletteRequired => "PNG is indexed-colour but has no PLTE chunk",
            Self::PaletteForbidden => "PNG's colour type does not permit a PLTE chunk",
            Self::InvalidPaletteLength => "PNG's PLTE chunk has an invalid length",
            Self::TransparencyForbidden => "PNG's colour type does not permit a tRNS chunk",
            Self::InvalidTransparencyLength => "PNG's tRNS chunk has an invalid length",
            Self::CompressedData(_) => "PNG image data",
            Self::CompressedSizeMismatch => "PNG's decompressed image data has the wrong size",
            Self::InvalidFilterType => "PNG scanline has an invalid filter type",
            Self::PaletteIndexOutOfRange => "PNG pixel references a palette entry out of range",
            Self::JpegBadSignature => "not a JPEG file (bad SOI marker)",
            Self::JpegMarkerTruncated => "JPEG marker is truncated",
            Self::JpegSegmentTooShort => {
                "JPEG segment declares a length shorter than its own length field"
            }
            Self::JpegSegmentLengthExceedsInput => {
                "JPEG segment declares a length longer than the input"
            }
            Self::JpegUnknownMarker => "JPEG has an unrecognised marker",
            Self::JpegArithmeticCodingUnsupported => {
                "JPEG uses arithmetic coding, which is not supported"
            }
            Self::JpegLosslessOrHierarchicalUnsupported => {
                "JPEG is a lossless or hierarchical frame, which is not supported"
            }
            Self::JpegUnsupportedPrecision => "JPEG declares a sample precision other than 8 bits",
            Self::JpegUnsupportedComponentCount => {
                "JPEG declares a component count other than 1 or 3"
            }
            Self::JpegInvalidSamplingFactor => "JPEG component declares an invalid sampling factor",
            Self::JpegInvalidFrameHeader => "JPEG SOF header is malformed",
            Self::JpegDuplicateFrameHeader => "JPEG has more than one SOF marker",
            Self::JpegMissingFrameHeader => {
                "JPEG marker requires a frame header that has not appeared yet"
            }
            Self::JpegDnlUnsupported => {
                "JPEG defers its height to a DNL marker, which is not supported"
            }
            Self::JpegInvalidQuantizationTable => "JPEG DQT segment is malformed",
            Self::JpegMissingQuantizationTable => {
                "JPEG references a quantisation table that was never loaded"
            }
            Self::JpegInvalidHuffmanTable => "JPEG DHT segment is malformed",
            Self::JpegMissingHuffmanTable => {
                "JPEG references a Huffman table that was never loaded"
            }
            Self::JpegHuffmanCodeNotFound => "JPEG entropy-coded data has no matching Huffman code",
            Self::JpegCoefficientRunOverflow => {
                "JPEG AC run-length skip runs past the end of the block"
            }
            Self::JpegComponentIdMismatch => {
                "JPEG scan references a component id absent from its frame"
            }
            Self::JpegInvalidScanHeader => "JPEG SOS header is malformed",
            Self::JpegInvalidRestartInterval => "JPEG DRI segment is malformed",
            Self::JpegRestartMarkerMismatch => "JPEG restart marker is missing or out of sequence",
            Self::JpegEntropyDataTruncated => {
                "JPEG entropy-coded data ends before its scan is complete"
            }
            Self::JpegMissingEndOfImage => "JPEG has no EOI marker",
            Self::JpegProgressiveCoefficientStoreExceedsLimit => {
                "JPEG's progressive coefficient store exceeds the caller's limit"
            }
            Self::GifBadSignature => "not a GIF file (bad signature)",
            Self::GifUnknownVersion => "GIF declares a version other than 87a or 89a",
            Self::GifTruncated => "GIF block runs past the end of the input",
            Self::GifUnknownBlock => "GIF has an unrecognised block introducer",
            Self::GifMalformedExtension => "GIF extension block is malformed",
            Self::GifReservedDisposal => "GIF frame declares a reserved disposal method",
            Self::GifZeroFrame => "GIF frame declares a zero width or height",
            Self::GifFrameOutsideScreen => "GIF frame reaches outside the logical screen",
            Self::GifNoFrames => "GIF has no image blocks",
            Self::GifTooManyFrames => "GIF declares more frames than the decoder accepts",
            Self::GifMissingColourTable => {
                "GIF frame has neither a local nor a global colour table"
            }
            Self::GifPaletteIndexOutOfRange => {
                "GIF pixel references a colour-table entry out of range"
            }
            Self::GifInvalidCodeSize => "GIF frame declares an out-of-range LZW code size",
            Self::GifInvalidCode => "GIF LZW stream holds a code its table cannot resolve",
            Self::GifTruncatedImageData => "GIF LZW stream ends before its frame's last pixel",
            Self::BmpBadSignature => "not a BMP file (bad signature)",
            Self::BmpTruncated => "BMP header runs past the end of the input",
            Self::BmpUnsupportedHeaderSize => "BMP declares an unsupported header size",
            Self::BmpInvalidDimensions => "BMP declares a negative width",
            Self::BmpInvalidPlanes => "BMP declares a colour-plane count other than one",
            Self::BmpUnsupportedBitCount => "BMP declares an unsupported bit count",
            Self::BmpUnsupportedCompression => "BMP declares an unsupported compression",
            Self::BmpCompressionMismatch => {
                "BMP's compression and bit count or row order contradict each other"
            }
            Self::BmpInvalidMask => "BMP declares an invalid channel mask",
            Self::BmpInvalidPaletteLength => "BMP's colour table has an invalid length",
            Self::BmpInvalidPixelOffset => "BMP's pixel-array offset is out of range",
            Self::BmpPixelDataTruncated => "BMP pixel array is shorter than its geometry needs",
            Self::BmpPaletteIndexOutOfRange => {
                "BMP pixel references a colour-table entry out of range"
            }
            Self::BmpRleOutOfBounds => "BMP run-length-encoded run reaches outside the picture",
            Self::BmpRleTruncated => "BMP run-length-encoded array ends before its last row",
            Self::IcoBadSignature => "not an icon or cursor file (bad signature)",
            Self::IcoTruncated => "icon entry runs past the end of the input",
            Self::IcoNoEntries => "icon directory declares no pictures",
            Self::IcoInvalidMaskHeight => "icon entry's bitmap declares an odd height",
            Self::SpriteBadArea => "sprite area header or control-block chain is malformed",
            Self::SpriteTruncated => "sprite runs past the end of the input",
            Self::SpriteNoSprites => "sprite area declares no sprites",
            Self::SpriteInvalidModeWord => "sprite declares an invalid mode word",
            Self::SpriteUnknownMode => "sprite declares an unsupported screen mode",
            Self::SpriteUnsupportedType => "sprite declares an unsupported sprite type",
            Self::SpriteInvalidWastage => "sprite's used-bit fields leave no whole pixels",
        }
    }
}

impl core::fmt::Display for DecodeError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(self.message())?;
        if let Self::CompressedData(inner) = self {
            write!(f, ": {inner}")?;
        }
        Ok(())
    }
}

/// The caller's ceiling on the image [`decode`] will ever produce.
///
/// A format decoder checks a declared width and height against these
/// limits **before** allocating any scanline, palette, or output buffer, so
/// a file that lies about its dimensions cannot make this crate reserve
/// memory proportional to the lie.
// The shared `max` prefix names exactly what this struct is: four ceilings
// on the image `decode` will ever produce. Stripping it (to `width`,
// `height`, `pixels`) would read as the image's *actual* geometry rather
// than its limit, so the prefix stays despite the lint's default advice.
#[allow(clippy::struct_field_names)]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct DecodeLimits {
    max_width: u32,
    max_height: u32,
    max_pixels: u64,
    max_progressive_coefficient_bytes: u64,
}

impl DecodeLimits {
    /// Construct a limit set.
    ///
    /// `max_progressive_coefficient_bytes` bounds the coefficient store a
    /// progressive JPEG scan must buffer (see its accessor's documentation);
    /// a decoder that never sees a progressive JPEG never consults it, so
    /// `0` is a fine value for a caller that only ever decodes PNG or
    /// baseline/extended-sequential JPEG.
    #[must_use]
    pub const fn new(
        max_width: u32,
        max_height: u32,
        max_pixels: u64,
        max_progressive_coefficient_bytes: u64,
    ) -> Self {
        Self {
            max_width,
            max_height,
            max_pixels,
            max_progressive_coefficient_bytes,
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

    /// The maximum permitted size, in bytes, of a progressive JPEG's
    /// coefficient store.
    ///
    /// This is a fixed security bound, not a growable capacity (unlike a
    /// cache or a buffer this crate could shrink under pressure): a
    /// progressive scan (ITU-T T.81 Annex G) is only ever allowed to
    /// refine coefficients a strictly earlier scan already placed, so the
    /// decoder cannot produce a single output pixel until every scan has
    /// been read — every component's every block's every coefficient must
    /// be held, at 2 bytes each, for the whole of the entropy-coded data.
    /// A 25-megapixel 4:2:0 image alone needs roughly 75 MB of that store,
    /// which the charter's 1 GiB operating-conditions floor cannot spend
    /// freely; this bound exists so a hostile or merely huge progressive
    /// stream is refused before that buffer is ever allocated, exactly as
    /// [`Self::max_pixels`] refuses an oversized declared geometry before
    /// the output buffer is allocated.
    #[must_use]
    pub const fn max_progressive_coefficient_bytes(&self) -> u64 {
        self.max_progressive_coefficient_bytes
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
/// Deliberately closed, and grows only with a real consumer: the desktop
/// icon pipeline hands this crate PNG artwork, the desktop pinboard hands it
/// the JPEG wallpaper masters, and the picture viewer opens the rest.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum ImageFormat {
    /// The Portable Network Graphics format (W3C PNG specification).
    Png,
    /// The JPEG File Interchange Format: baseline sequential, extended
    /// sequential, and progressive DCT frames with Huffman coding
    /// (ITU-T T.81), framed as JFIF or Adobe.
    Jpeg,
    /// The Graphics Interchange Format (`GIF89a`, and the `GIF87a` subset): a
    /// palette-indexed LZW sequence over one logical screen, with the
    /// format's full frame-disposal model.
    Gif,
    /// The Windows device-independent bitmap file format (BMP): a
    /// `BITMAPFILEHEADER` over a DIB, from `BITMAPCOREHEADER` through
    /// `BITMAPV5HEADER`, at every bit depth and encoding those define.
    Bmp,
    /// The Windows icon and cursor containers (ICO and CUR): a directory of
    /// independent pictures at different sizes, each a DIB with a 1-bit mask
    /// over it or a whole PNG file.
    Ico,
    /// A RISC OS sprite area (Acorn filetype `&FF9`): a container of
    /// independent, named pictures, at every depth and mode word the format
    /// defines.
    ///
    /// [`sniff`] never answers this, because a sprite area carries no
    /// signature to recognise; a caller that knows the type names it
    /// ([`probe_as`], [`decode_as`], [`Sequence::open_as`]).
    Sprite,
}

/// The 8-byte PNG file signature (W3C PNG §"PNG file signature").
const PNG_SIGNATURE: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];

/// The leading bytes every JPEG stream carries: the SOI marker (`0xFFD8`,
/// ITU-T T.81 §B.2.1) followed by the `0xFF` lead byte of the marker that
/// always follows it. Checking that third byte is what keeps this from
/// colliding with any other two-byte-prefixed format.
const JPEG_SIGNATURE: [u8; 3] = [0xFF, 0xD8, 0xFF];

/// The three magic bytes every GIF opens with (`GIF89a` §17). The version
/// field that follows is the format's own business, so a recognisably-GIF
/// file with an unknown version reaches the decoder and is refused there
/// with the reason, rather than being reported as no format at all.
const GIF_SIGNATURE: [u8; 3] = *b"GIF";

/// Identify the format of `bytes` from its leading signature, or `None` if
/// no supported format is recognised.
///
/// Only a format that carries a signature can be recognised from content.
/// [`ImageFormat::Sprite`] does not — a RISC OS sprite area opens with its
/// sprite count, and RISC OS types a file from its directory entry — so this
/// never answers it, and never guesses one from a structural coincidence. A
/// caller holding a file whose type it already knows, from a filetype or a
/// media type, names the format instead ([`probe_as`], [`decode_as`],
/// [`Sequence::open_as`]).
#[must_use]
pub fn sniff(bytes: &[u8]) -> Option<ImageFormat> {
    if bytes.starts_with(&PNG_SIGNATURE) {
        return Some(ImageFormat::Png);
    }
    if bytes.starts_with(&JPEG_SIGNATURE) {
        return Some(ImageFormat::Jpeg);
    }
    if bytes.starts_with(&GIF_SIGNATURE) {
        return Some(ImageFormat::Gif);
    }
    if bytes.starts_with(&bmp::MAGIC) {
        return Some(ImageFormat::Bmp);
    }
    if bytes.starts_with(&ico::ICO_SIGNATURE) || bytes.starts_with(&ico::CUR_SIGNATURE) {
        return Some(ImageFormat::Ico);
    }
    None
}

/// What an image's own header declares, without decoding a single pixel.
///
/// [`probe`] answers this from the header alone, so a caller that must know
/// the image's shape before it can decide what to *ask* a decode for — a
/// composition that maps part of the source onto part of a destination, and
/// so cannot state its target size until it knows the source's — can settle
/// that question for the price of parsing a header.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct ImageInfo {
    format: ImageFormat,
    width: u32,
    height: u32,
}

impl ImageInfo {
    /// The format the header identifies.
    #[must_use]
    pub const fn format(&self) -> ImageFormat {
        self.format
    }

    /// The natural width the header declares, in pixels.
    #[must_use]
    pub const fn width(&self) -> u32 {
        self.width
    }

    /// The natural height the header declares, in pixels.
    #[must_use]
    pub const fn height(&self) -> u32 {
        self.height
    }
}

/// Read `bytes`' header and answer its format and natural size, decoding no
/// pixels and allocating no pixel buffer.
///
/// The geometry is the file's own declaration, so it is exactly as
/// trustworthy as the file: a hostile image may declare any size at all.
/// Nothing here acts on it — no buffer is sized from it and no limit is
/// applied to it — so a caller must hold the answer to its own bounds
/// before it does. What a probe *does* guarantee is that the header is
/// structurally valid: a malformed one is refused here rather than later.
///
/// # Errors
///
/// [`DecodeError::UnknownFormat`] for an unrecognised signature, and
/// otherwise whichever header refusal the format's own parser raises.
pub fn probe(bytes: &[u8]) -> Result<ImageInfo, DecodeError> {
    probe_as(sniff(bytes).ok_or(DecodeError::UnknownFormat)?, bytes)
}

/// Read `bytes`' header as `format`, rather than as whichever format its
/// signature names.
///
/// This is how a caller reaches a format [`sniff`] cannot recognise, and how
/// one that already knows the type — from a RISC OS filetype, a media type,
/// or the name a file was picked by — skips guessing at it. The format's own
/// parser still validates the bytes, so naming the wrong one is refused
/// rather than misread. See [`probe`] for what a probe does and does not
/// guarantee.
///
/// # Errors
///
/// Whichever header refusal the named format's own parser raises.
pub fn probe_as(format: ImageFormat, bytes: &[u8]) -> Result<ImageInfo, DecodeError> {
    let (width, height) = match format {
        ImageFormat::Png => png::probe(bytes)?,
        ImageFormat::Jpeg => jpeg::probe(bytes)?,
        ImageFormat::Gif => gif::probe(bytes)?,
        ImageFormat::Bmp => bmp::probe(bytes)?,
        ImageFormat::Ico => ico::probe(bytes)?,
        ImageFormat::Sprite => sprite::probe(bytes)?,
    };
    Ok(ImageInfo {
        format,
        width,
        height,
    })
}

/// A caller's target output size for [`decode_fitted`]: the largest width
/// and height it actually intends to use.
///
/// A small, public copy type — not [`RasterImage`]'s own geometry, which is
/// the decoded *result*, not the caller's *request*.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct FitBox {
    width: u32,
    height: u32,
}

impl FitBox {
    /// Construct a fit box of `width` by `height`.
    #[must_use]
    pub const fn new(width: u32, height: u32) -> Self {
        Self { width, height }
    }

    /// The box's width, in pixels.
    #[must_use]
    pub const fn width(&self) -> u32 {
        self.width
    }

    /// The box's height, in pixels.
    #[must_use]
    pub const fn height(&self) -> u32 {
        self.height
    }
}

/// Decode `bytes` into a [`RasterImage`] at its natural (full) size,
/// honouring `limits`.
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
    decode_as(
        sniff(bytes).ok_or(DecodeError::UnknownFormat)?,
        bytes,
        limits,
    )
}

/// Decode `bytes` as `format` at its natural (full) size, rather than as
/// whichever format its signature names.
///
/// This is how a caller reaches a format [`sniff`] cannot recognise (see
/// [`probe_as`]). Where the named format is a container of independent
/// pictures, this answers the largest, which for an icon file is the picture
/// it is at its best size and for a sprite area is the only choice that
/// never silently answers a thumbnail; [`Sequence::open_as`] is how a caller
/// reaches the others.
///
/// # Errors
///
/// See [`DecodeError`] for every fail-closed refusal reason.
pub fn decode_as(
    format: ImageFormat,
    bytes: &[u8],
    limits: &DecodeLimits,
) -> Result<RasterImage, DecodeError> {
    match format {
        ImageFormat::Png => png::decode(bytes, limits),
        ImageFormat::Jpeg => jpeg::decode(bytes, limits),
        ImageFormat::Gif => gif::decode(bytes, limits),
        ImageFormat::Bmp => bmp::decode(bytes, limits),
        ImageFormat::Ico => ico::decode(bytes, limits),
        ImageFormat::Sprite => sprite::decode(bytes, limits),
    }
}

/// Decode `bytes` into a [`RasterImage`] no smaller than it has to be to
/// cover `fit` on both axes, honouring `limits`.
///
/// For a format with a reduced-scale decode process (JPEG: one whole, one
/// half, one quarter, or one eighth of natural size, chosen by decoding
/// with progressively coarser inverse DCTs), this picks the smallest such
/// scale whose result still covers `fit` in both width and height — never
/// scaling up, and never resampling the result to match `fit` exactly.
/// Reduced dimensions round up, so the result may be modestly larger than
/// `fit` but is never smaller.
///
/// # Degrading rather than refusing
///
/// Where the smallest covering scale's own output would breach `limits`,
/// the largest scale that stays within them is decoded instead. That is a
/// deliberate trade of a little sharpness for a decode the caller can
/// actually afford in memory: a screen larger than `limits` allow is served
/// slightly soft rather than not at all, and correctness and memory safety
/// are never what is traded. The scale is settled from the format's own
/// header geometry before any buffer is allocated, so no decode is ever
/// attempted, abandoned, and retried. Only when even the smallest
/// available scale breaches `limits` is the image refused, and then with
/// whichever limit that smallest possible output broke.
///
/// [`decode`] has no such freedom and keeps none: it always means natural
/// size, and is refused outright when that size breaches `limits`.
///
/// # An icon container fits by choosing a page
///
/// An icon file *is* one picture at several sizes, so there is nothing to
/// compute: this takes the smallest page covering `fit` on both axes that
/// also stays within `limits`, falling back to the largest that does. A page
/// is already the picture at that size, so nothing is scaled and nothing is
/// resampled.
///
/// # PNG, GIF, and BMP
///
/// None has a reduced-scale decode process — LZW and DEFLATE entropy coding
/// and a padded row array do not separate into scale-selectable passes the
/// way a block transform does — so for those this is exactly [`decode`],
/// always at natural size, and the degradation above cannot apply. That is
/// an honest property of those formats, not a gap this crate is missing.
///
/// The format is chosen by [`sniff`]; an unrecognised signature is refused
/// as [`DecodeError::UnknownFormat`] before any format-specific parsing
/// runs.
///
/// # Errors
///
/// See [`DecodeError`] for every fail-closed refusal reason.
pub fn decode_fitted(
    bytes: &[u8],
    limits: &DecodeLimits,
    fit: FitBox,
) -> Result<RasterImage, DecodeError> {
    match sniff(bytes).ok_or(DecodeError::UnknownFormat)? {
        ImageFormat::Jpeg => jpeg::decode_fitted(bytes, limits, fit),
        ImageFormat::Ico => ico::decode_fitted(bytes, limits, fit),
        format => decode_as(format, bytes, limits),
    }
}

/// What a container's entries are, which is what decides whether they are
/// *played* or *chosen between*.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum SequenceKind {
    /// Frames composited in order over one canvas, played `loop_count` times
    /// — `None` for ever. Each frame's pixels are the canvas *after* it has
    /// been composited, so a consumer shows the whole thing rather than
    /// having to know the format's disposal model.
    Animation {
        /// How many times the container asks for the sequence to be played.
        loop_count: Option<u32>,
    },
    /// Independent pages, each a picture in its own right. A still image is
    /// the one-page case, not a special case.
    Pages,
}

/// What a container declares about its entries as a whole.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct SequenceInfo {
    format: ImageFormat,
    width: u32,
    height: u32,
    count: u32,
    kind: SequenceKind,
}

impl SequenceInfo {
    /// The format the header identifies.
    #[must_use]
    pub const fn format(&self) -> ImageFormat {
        self.format
    }

    /// The width of the picture the container is: the animation canvas, a
    /// still picture's own width, or — where pages differ in size, as an
    /// icon file's do — the largest page's. Each [`Frame`] carries its own.
    #[must_use]
    pub const fn width(&self) -> u32 {
        self.width
    }

    /// The height of the picture the container is; see [`Self::width`].
    #[must_use]
    pub const fn height(&self) -> u32 {
        self.height
    }

    /// How many entries the container holds; `1` for a still picture.
    #[must_use]
    pub const fn count(&self) -> u32 {
        self.count
    }

    /// Whether the entries are frames to play or pages to choose between.
    #[must_use]
    pub const fn kind(&self) -> SequenceKind {
        self.kind
    }
}

/// One entry of a sequence: the pixels to show, and what the container says
/// about showing them.
///
/// The pixels are borrowed from the decoder rather than copied, because an
/// animation's canvas has to be retained for the next frame to composite
/// onto: handing out an owned buffer per step would copy the whole canvas
/// every frame for nothing.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct Frame<'a> {
    index: u32,
    width: u32,
    height: u32,
    delay_ns: u64,
    pixels: &'a [u8],
}

impl<'a> Frame<'a> {
    /// This entry's zero-based position in the container.
    #[must_use]
    pub const fn index(&self) -> u32 {
        self.index
    }

    /// The width of [`Self::pixels`], in pixels.
    #[must_use]
    pub const fn width(&self) -> u32 {
        self.width
    }

    /// The height of [`Self::pixels`], in pixels.
    #[must_use]
    pub const fn height(&self) -> u32 {
        self.height
    }

    /// How long the container asks for this frame to be shown, in
    /// nanoseconds; `0` where it declares nothing.
    ///
    /// Reported exactly as the file gives it. Clamping a too-fast animation
    /// to a minimum interval is a playback decision, and the thing that plays
    /// frames is the one that knows how fast its screen can show them.
    #[must_use]
    pub const fn delay_ns(&self) -> u64 {
        self.delay_ns
    }

    /// The row-major RGBA8 pixels (straight alpha), exactly
    /// `width * height * 4` bytes.
    #[must_use]
    pub const fn pixels(&self) -> &'a [u8] {
        self.pixels
    }
}

/// Where a sequence's entries come from.
enum Entries<'a> {
    /// A single-image format: decoded on the first step and lent as the one
    /// entry a still picture has. A rewind costs nothing, because the decode
    /// is kept.
    Still {
        bytes: &'a [u8],
        limits: DecodeLimits,
        decoded: Option<RasterImage>,
        served: bool,
    },
    /// A GIF's block chain, composited onto its retained canvas.
    Gif(gif::Frames<'a>),
    /// An icon container's directory of independent pictures.
    Ico(pages::Pages<ico::Directory<'a>>),
    /// A RISC OS sprite area's chain of independent pictures.
    Sprite(pages::Pages<sprite::Area<'a>>),
}

/// A container's frames or pages, decoded in order.
///
/// Stepping is forward-only with a restart, because that is what an
/// animation is: a frame composites onto its predecessors under the
/// container's disposal model, so a decoder that could be asked for frame
/// *n* directly would have to re-composite every frame before it. Holding
/// the canvas and stepping makes each frame cost its own decode and no more.
///
/// A still picture is the one-entry case of the same shape, so a consumer
/// that shows pictures and animations needs one path rather than two.
pub struct Sequence<'a> {
    info: SequenceInfo,
    entries: Entries<'a>,
}

impl<'a> Sequence<'a> {
    /// Validate `bytes`' structure and prepare to decode its entries,
    /// decoding no pixels.
    ///
    /// A container with one canvas has its geometry weighed against `limits`
    /// here, before that canvas is allocated, so a container that lies about
    /// its size cannot make this reserve memory proportional to the lie — and
    /// a caller learns it cannot afford the picture before it has laid
    /// anything out for it. A page container allocates nothing until a page
    /// is asked for and weighs nothing here: its pages are independent
    /// pictures, and a caller may well want a small one out of a file whose
    /// largest it could never afford.
    ///
    /// # Errors
    ///
    /// [`DecodeError::UnknownFormat`] for an unrecognised signature, a limit
    /// refusal for a geometry the caller will not allow, and otherwise
    /// whichever structural refusal the format's own parser raises.
    pub fn open(bytes: &'a [u8], limits: &DecodeLimits) -> Result<Self, DecodeError> {
        Self::open_as(
            sniff(bytes).ok_or(DecodeError::UnknownFormat)?,
            bytes,
            limits,
        )
    }

    /// Prepare to decode `bytes`' entries as `format`, rather than as
    /// whichever format its signature names.
    ///
    /// This is how a caller reaches a format [`sniff`] cannot recognise (see
    /// [`probe_as`]) — for a RISC OS sprite area it is the only door, since
    /// a container of an application's whole icon set is a sequence rather
    /// than a picture.
    ///
    /// # Errors
    ///
    /// As [`Self::open`], less the unrecognised-signature refusal that
    /// naming the format removes.
    pub fn open_as(
        format: ImageFormat,
        bytes: &'a [u8],
        limits: &DecodeLimits,
    ) -> Result<Self, DecodeError> {
        match format {
            ImageFormat::Gif => {
                let frames = gif::Frames::open(bytes, limits)?;
                Ok(Self {
                    info: SequenceInfo {
                        format: ImageFormat::Gif,
                        width: frames.width(),
                        height: frames.height(),
                        count: frames.count(),
                        kind: SequenceKind::Animation {
                            loop_count: frames.loop_count(),
                        },
                    },
                    entries: Entries::Gif(frames),
                })
            }
            ImageFormat::Ico => {
                let pages = ico::pages(bytes, limits)?;
                Ok(Self::paged(ImageFormat::Ico, pages, Entries::Ico))
            }
            ImageFormat::Sprite => {
                let pages = sprite::pages(bytes, limits)?;
                Ok(Self::paged(ImageFormat::Sprite, pages, Entries::Sprite))
            }
            ImageFormat::Png => Self::still(ImageFormat::Png, png::probe(bytes)?, bytes, limits),
            ImageFormat::Jpeg => Self::still(ImageFormat::Jpeg, jpeg::probe(bytes)?, bytes, limits),
            ImageFormat::Bmp => Self::still(ImageFormat::Bmp, bmp::probe(bytes)?, bytes, limits),
        }
    }

    /// A container of independent pages, whose geometry is its largest.
    fn paged<S: pages::PageSource>(
        format: ImageFormat,
        pages: pages::Pages<S>,
        entries: impl FnOnce(pages::Pages<S>) -> Entries<'a>,
    ) -> Self {
        let info = SequenceInfo {
            format,
            width: pages.width(),
            height: pages.height(),
            count: pages.count(),
            kind: SequenceKind::Pages,
        };
        Self {
            info,
            entries: entries(pages),
        }
    }

    /// The one-entry case: a format holding a single picture.
    fn still(
        format: ImageFormat,
        (width, height): (u32, u32),
        bytes: &'a [u8],
        limits: &DecodeLimits,
    ) -> Result<Self, DecodeError> {
        limits.check(width, height)?;
        Ok(Self {
            info: SequenceInfo {
                format,
                width,
                height,
                count: 1,
                kind: SequenceKind::Pages,
            },
            entries: Entries::Still {
                bytes,
                limits: *limits,
                decoded: None,
                served: false,
            },
        })
    }

    /// What the container declares about its entries as a whole.
    #[must_use]
    pub const fn info(&self) -> SequenceInfo {
        self.info
    }

    /// Decode the next entry, or answer `None` once they are exhausted.
    ///
    /// # Errors
    ///
    /// Whichever refusal the entry's own decode raises. No pixels are handed
    /// out with it, and the refusal is **remembered**: stepping again answers
    /// the same one until [`Self::rewind`]. An animation's frame that stopped
    /// part-way leaves the composition canvas describing no whole frame, and
    /// this is what keeps a later frame from ever being composited onto it. A
    /// caller that means to continue rewinds; one that does not simply
    /// reports the reason.
    pub fn next_frame(&mut self) -> Result<Option<Frame<'_>>, DecodeError> {
        let (width, height) = (self.info.width, self.info.height);
        match &mut self.entries {
            Entries::Still {
                bytes,
                limits,
                decoded,
                served,
            } => {
                if *served {
                    return Ok(None);
                }
                if decoded.is_none() {
                    *decoded = Some(decode(bytes, limits)?);
                }
                *served = true;
                let Some(image) = decoded.as_ref() else {
                    return Ok(None);
                };
                Ok(Some(Frame {
                    index: 0,
                    width: image.width(),
                    height: image.height(),
                    delay_ns: 0,
                    pixels: image.pixels(),
                }))
            }
            Entries::Gif(frames) => {
                let index = frames.index();
                if !frames.step()? {
                    return Ok(None);
                }
                Ok(Some(Frame {
                    index,
                    width,
                    height,
                    delay_ns: frames.delay_ns(),
                    pixels: frames.canvas(),
                }))
            }
            Entries::Ico(pages) => step_page(pages),
            Entries::Sprite(pages) => step_page(pages),
        }
    }

    /// Decode the entry at `index`, or `None` where the container holds no
    /// such entry.
    ///
    /// This is what a page container is *for*: an icon file's pages are
    /// independent pictures at different sizes, and choosing between them is
    /// the point. A page costs its own decode and nothing else, and a page
    /// that refuses says so without disturbing the others.
    ///
    /// An animation has no independent pages — a frame composites onto its
    /// predecessors — so addressing frame `n` restarts the composition and
    /// steps to it, costing exactly what [`Self::rewind`] and `n + 1` calls
    /// to [`Self::next_frame`] would. That is why a player steps rather than
    /// addressing, and why this clears a remembered refusal for an animation
    /// but not for a page container, which never had one to clear.
    ///
    /// # Errors
    ///
    /// Whichever refusal the entry's own decode raises.
    pub fn page(&mut self, index: u32) -> Result<Option<Frame<'_>>, DecodeError> {
        let (width, height) = (self.info.width, self.info.height);
        match &mut self.entries {
            Entries::Still {
                bytes,
                limits,
                decoded,
                ..
            } => {
                if index != 0 {
                    return Ok(None);
                }
                if decoded.is_none() {
                    *decoded = Some(decode(bytes, limits)?);
                }
                Ok(page_frame(0, decoded.as_ref()))
            }
            Entries::Gif(frames) => {
                frames.rewind();
                for _ in 0..=index {
                    if !frames.step()? {
                        return Ok(None);
                    }
                }
                Ok(Some(Frame {
                    index,
                    width,
                    height,
                    delay_ns: frames.delay_ns(),
                    pixels: frames.canvas(),
                }))
            }
            Entries::Ico(pages) => addressed_page(pages, index),
            Entries::Sprite(pages) => addressed_page(pages, index),
        }
    }

    /// Restart at the first entry, clearing a remembered refusal.
    ///
    /// This is how a loop plays again: an animation's canvas is cleared and
    /// its cursor returns to the first frame, so the composition starts from
    /// the same blank canvas it did the first time — which is also what makes
    /// it safe to step on after a refusal.
    pub fn rewind(&mut self) {
        match &mut self.entries {
            Entries::Still { served, .. } => *served = false,
            Entries::Gif(frames) => frames.rewind(),
            Entries::Ico(pages) => pages.rewind(),
            Entries::Sprite(pages) => pages.rewind(),
        }
    }
}

/// Decode a page container's next page and lend it.
fn step_page<S: pages::PageSource>(
    pages: &mut pages::Pages<S>,
) -> Result<Option<Frame<'_>>, DecodeError> {
    let index = pages.index();
    if !pages.step()? {
        return Ok(None);
    }
    Ok(page_frame(index, pages.current()))
}

/// Decode the page a container holds at `index` and lend it.
fn addressed_page<S: pages::PageSource>(
    pages: &mut pages::Pages<S>,
    index: u32,
) -> Result<Option<Frame<'_>>, DecodeError> {
    if !pages.page(index)? {
        return Ok(None);
    }
    Ok(page_frame(index, pages.current()))
}

/// Lend a decoded page as a frame. A page is a picture in its own right, so
/// it carries its own geometry and declares no delay.
fn page_frame(index: u32, image: Option<&RasterImage>) -> Option<Frame<'_>> {
    image.map(|image| Frame {
        index,
        width: image.width(),
        height: image.height(),
        delay_ns: 0,
        pixels: image.pixels(),
    })
}

#[cfg(test)]
mod tests {
    use alloc::vec;
    use alloc::vec::Vec;

    use super::{decode, decode_fitted, sniff, DecodeError, DecodeLimits, FitBox, ImageFormat};
    use crate::crc32;

    #[test]
    fn sniff_recognises_the_png_signature() {
        let mut bytes = super::PNG_SIGNATURE.to_vec();
        bytes.extend_from_slice(b"anything after the signature");
        assert_eq!(sniff(&bytes), Some(ImageFormat::Png));
    }

    #[test]
    fn sniff_recognises_the_jpeg_signature() {
        let mut bytes = super::JPEG_SIGNATURE.to_vec();
        bytes.extend_from_slice(b"anything after the signature");
        assert_eq!(sniff(&bytes), Some(ImageFormat::Jpeg));
    }

    #[test]
    fn sniff_recognises_the_gif_signature() {
        let mut bytes = super::GIF_SIGNATURE.to_vec();
        bytes.extend_from_slice(b"89a and whatever follows");
        assert_eq!(sniff(&bytes), Some(ImageFormat::Gif));
        // The version is the decoder's business, so a recognisably-GIF file
        // with an unknown one reaches it and is refused with the reason.
        assert_eq!(sniff(b"GIF99a"), Some(ImageFormat::Gif));
    }

    #[test]
    fn sniff_rejects_an_unknown_signature() {
        assert_eq!(sniff(b"not a supported image format"), None);
        assert_eq!(sniff(b""), None);
        // The SOI marker alone, with no following marker byte, is not
        // enough: real JPEG streams always carry a marker right after SOI.
        assert_eq!(sniff(&[0xFF, 0xD8]), None);
    }

    #[test]
    fn decode_refuses_an_unknown_format_before_any_parsing() {
        let limits = DecodeLimits::new(64, 64, 4096, 0);
        assert_eq!(
            decode(b"definitely not an image", &limits),
            Err(DecodeError::UnknownFormat)
        );
    }

    #[test]
    fn limits_check_rejects_zero_dimensions_and_over_limit_geometry() {
        let limits = DecodeLimits::new(8, 8, 32, 0);
        assert_eq!(limits.check(0, 4), Err(DecodeError::ZeroDimension));
        assert_eq!(limits.check(4, 0), Err(DecodeError::ZeroDimension));
        assert_eq!(limits.check(9, 4), Err(DecodeError::WidthExceedsLimit));
        assert_eq!(limits.check(4, 9), Err(DecodeError::HeightExceedsLimit));
        assert_eq!(limits.check(8, 8), Err(DecodeError::PixelCountExceedsLimit));
        assert_eq!(limits.check(4, 4), Ok(()));
    }

    #[test]
    fn limits_accessors_return_the_constructed_values() {
        let limits = DecodeLimits::new(10, 20, 200, 4_000);
        assert_eq!(limits.max_width(), 10);
        assert_eq!(limits.max_height(), 20);
        assert_eq!(limits.max_pixels(), 200);
        assert_eq!(limits.max_progressive_coefficient_bytes(), 4_000);
    }

    /// A minimal, valid 2x2 8-bit greyscale PNG (a single stored-deflate
    /// `IDAT` block), built directly here rather than reaching into
    /// `png_tests.rs`'s own private fixture helpers.
    ///
    /// The GIF tests borrow it as the still picture a sequence's one-page
    /// case is proved against.
    pub(crate) fn minimal_png() -> Vec<u8> {
        fn chunk(chunk_type: [u8; 4], payload: &[u8]) -> Vec<u8> {
            let mut out = Vec::new();
            let len = u32::try_from(payload.len()).expect("fits");
            out.extend_from_slice(&len.to_be_bytes());
            out.extend_from_slice(&chunk_type);
            out.extend_from_slice(payload);
            let crc = crc32::crc32_of(&[&chunk_type, payload]);
            out.extend_from_slice(&crc.to_be_bytes());
            out
        }
        let raw = [0u8, 10, 20, 0, 30, 40]; // two filter-None rows of 2 grey samples
        let mut idat = vec![0x78u8, 0x9C, 0x01];
        idat.extend_from_slice(&u16::try_from(raw.len()).expect("fits").to_le_bytes());
        idat.extend_from_slice(&(!u16::try_from(raw.len()).expect("fits")).to_le_bytes());
        idat.extend_from_slice(&raw);
        idat.extend_from_slice(&tairix_compress::zlib::adler32(&raw).to_be_bytes());

        let mut ihdr = Vec::new();
        ihdr.extend_from_slice(&2u32.to_be_bytes());
        ihdr.extend_from_slice(&2u32.to_be_bytes());
        ihdr.extend_from_slice(&[8, 0, 0, 0, 0]);

        let mut out = super::PNG_SIGNATURE.to_vec();
        out.extend(chunk(*b"IHDR", &ihdr));
        out.extend(chunk(*b"IDAT", &idat));
        out.extend(chunk(*b"IEND", &[]));
        out
    }

    #[test]
    fn decode_fitted_is_exactly_decode_for_png() {
        // PNG has no reduced-scale decode process at all: `decode_fitted`
        // must be identical to `decode`, whatever box is requested.
        let png = minimal_png();
        let limits = DecodeLimits::new(64, 64, 64 * 64, 0);
        let natural = decode(&png, &limits).expect("decodes");
        let fitted = decode_fitted(&png, &limits, FitBox::new(1, 1)).expect("decodes");
        assert_eq!(natural, fitted);
    }
}
