//! A complete, fail-closed TIFF 6.0 decoder.
//!
//! A TIFF is a chain of image file directories, each an independent picture,
//! so it decodes as a page container. Each page is a grid of strips or tiles
//! whose samples the directory's own tags describe: byte order, bit depth,
//! sample format, colour interpretation, plane arrangement, predictor, and
//! one of eight compressions. Nearly every combination those tags can spell
//! is a real file somewhere, which is why the page is validated whole before
//! a single buffer is reserved.
//!
//! # Readings the format's own text does not settle
//!
//! **A plain decode answers the first page the file does not call a reduced
//! copy of another.** A TIFF is an ordered document, not one picture at
//! several sizes, so its first page is its picture — unlike an icon file,
//! where the largest is. `NewSubfileType` is the file's own statement that a
//! page is a thumbnail, so honouring it beats guessing from size: a document
//! whose second page happens to be larger still answers its first.
//!
//! **Orientation is applied, not reported.** The tag says which way up the
//! stored raster is, so a decoder that ignored it would hand every consumer
//! a sideways picture and the format knowledge needed to right it.
//!
//! **A missing photometric under a fax compression reads as
//! `WhiteIsZero`.** Every fax is; the tag is required and its absence is a
//! writer's omission rather than a licence to guess in general, so nothing
//! else defaults.
//!
//! # Refused by name rather than half-read
//!
//! `BigTIFF` (version 43) is a separate format — its own version marker, its
//! own offset width, and its own directory-entry layout — so claiming it
//! would mean claiming it completely. The CIE L\*a\*b\* and `LogLuv`
//! photometrics, old-style JPEG (compression 6), word-aligned CCITT
//! (compression 32771), samples of mixed depth or format, and subsampled
//! `YCbCr` stored in separate planes are each refused with their own reason
//! for the same cause: a picture guessed at is worse than one declined.

use alloc::vec::Vec;

use tairix_compress::zlib;
use tairix_util::fallible;

use crate::ccitt;
use crate::channel::{Channel, Sampler};
use crate::lzw::{CodeSource, Lzw, Widen};
use crate::orientation::Orientation;
use crate::pages::{PageSource, Pages};
use crate::{jpeg, DecodeError, DecodeLimits, RasterImage, RGBA_BYTES};

/// The four openings a TIFF can have: the byte-order mark, then the version
/// in that order. Version 43 is `BigTIFF`, recognised here so its refusal can
/// name it rather than reading as no format at all.
pub(crate) const SIGNATURES: [[u8; 4]; 4] = [
    [b'I', b'I', 42, 0],
    [b'I', b'I', 43, 0],
    [b'M', b'M', 0, 42],
    [b'M', b'M', 0, 43],
];

/// A directory entry's fixed length, and the smallest a whole directory can
/// be: an entry count and the offset of the next directory.
const ENTRY_LEN: usize = 12;
const MIN_IFD_LEN: usize = 6;

/// Pages one file may declare.
///
/// A fixed containment bound rather than a capacity: a directory costs six
/// bytes, so a small file can chain enormous numbers of them, and a chain
/// that loops revisits one for ever. The walk is additionally held to what
/// the file has room for, so a cycle is refused rather than followed.
const MAX_PAGES: u32 = 4096;

/// Bits one pixel's samples may occupy together.
///
/// A fixed containment bound on the working buffer a strip needs. The
/// caller's limits bound the *picture*; without this a page could declare
/// hundreds of samples behind each of those pixels and make the buffer
/// holding one strip's raw samples arbitrarily larger than the image the
/// caller agreed to. At this ceiling that buffer is at most eight times the
/// output the caller asked for, and it is reserved fallibly, so a file that
/// spends the whole allowance is refused rather than served.
const MAX_BITS_PER_PIXEL: u32 = 256;

const TAG_NEW_SUBFILE_TYPE: u16 = 254;
const TAG_SUBFILE_TYPE: u16 = 255;
const TAG_IMAGE_WIDTH: u16 = 256;
const TAG_IMAGE_LENGTH: u16 = 257;
const TAG_BITS_PER_SAMPLE: u16 = 258;
const TAG_COMPRESSION: u16 = 259;
const TAG_PHOTOMETRIC: u16 = 262;
const TAG_FILL_ORDER: u16 = 266;
const TAG_STRIP_OFFSETS: u16 = 273;
const TAG_ORIENTATION: u16 = 274;
const TAG_SAMPLES_PER_PIXEL: u16 = 277;
const TAG_ROWS_PER_STRIP: u16 = 278;
const TAG_STRIP_BYTE_COUNTS: u16 = 279;
const TAG_PLANAR_CONFIGURATION: u16 = 284;
const TAG_T4_OPTIONS: u16 = 292;
const TAG_PREDICTOR: u16 = 317;
const TAG_COLOUR_MAP: u16 = 320;
const TAG_TILE_WIDTH: u16 = 322;
const TAG_TILE_LENGTH: u16 = 323;
const TAG_TILE_OFFSETS: u16 = 324;
const TAG_TILE_BYTE_COUNTS: u16 = 325;
const TAG_INK_SET: u16 = 332;
const TAG_EXTRA_SAMPLES: u16 = 338;
const TAG_SAMPLE_FORMAT: u16 = 339;
const TAG_JPEG_TABLES: u16 = 347;
const TAG_YCBCR_COEFFICIENTS: u16 = 529;
const TAG_YCBCR_SUBSAMPLING: u16 = 530;
const TAG_REFERENCE_BLACK_WHITE: u16 = 532;

const COMPRESSION_NONE: u16 = 1;
const COMPRESSION_CCITT_RLE: u16 = 2;
const COMPRESSION_GROUP3: u16 = 3;
const COMPRESSION_GROUP4: u16 = 4;
const COMPRESSION_LZW: u16 = 5;
const COMPRESSION_JPEG: u16 = 7;
const COMPRESSION_ADOBE_DEFLATE: u16 = 8;
const COMPRESSION_PACK_BITS: u16 = 32773;
const COMPRESSION_DEFLATE: u16 = 32946;

const PHOTOMETRIC_WHITE_ZERO: u16 = 0;
const PHOTOMETRIC_BLACK_ZERO: u16 = 1;
const PHOTOMETRIC_RGB: u16 = 2;
const PHOTOMETRIC_PALETTE: u16 = 3;
const PHOTOMETRIC_MASK: u16 = 4;
const PHOTOMETRIC_SEPARATED: u16 = 5;
const PHOTOMETRIC_YCBCR: u16 = 6;

/// `NewSubfileType`'s low bit: the page is a reduced-resolution copy of
/// another in the same file.
const SUBFILE_REDUCED: u32 = 1;

/// `SubfileType`'s (superseded) spelling of the same thing.
const OLD_SUBFILE_REDUCED: u32 = 2;

/// Fractional bits the colour arithmetic carries, and its unit.
const FRAC: u32 = 16;
const ONE: i64 = 1 << FRAC;

/// A file's byte order, which every multi-byte field in it is read in.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
enum Endian {
    Little,
    Big,
}

impl Endian {
    fn u16(self, bytes: &[u8], at: usize) -> Option<u16> {
        match self {
            Self::Little => crate::le_u16(bytes, at),
            Self::Big => crate::be_u16(bytes, at),
        }
    }

    fn u32(self, bytes: &[u8], at: usize) -> Option<u32> {
        match self {
            Self::Little => crate::le_u32(bytes, at),
            Self::Big => crate::be_u32(bytes, at),
        }
    }
}

/// Bytes one element of a directory entry's field type occupies, or `None`
/// for a type this decoder has no reading of.
const fn type_size(kind: u16) -> Option<u32> {
    Some(match kind {
        // BYTE, ASCII, SBYTE, UNDEFINED.
        1 | 2 | 6 | 7 => 1,
        // SHORT, SSHORT.
        3 | 8 => 2,
        // LONG, SLONG, FLOAT.
        4 | 9 | 11 => 4,
        // RATIONAL, SRATIONAL, DOUBLE.
        5 | 10 | 12 => 8,
        _ => return None,
    })
}

/// One directory entry's values, wherever the entry put them.
#[derive(Copy, Clone, Debug)]
struct Field {
    kind: u16,
    count: u32,
    at: usize,
}

/// One image file directory, and the file it indexes into.
#[derive(Copy, Clone)]
struct Ifd<'a> {
    file: &'a [u8],
    endian: Endian,
    entries: usize,
    count: u16,
}

impl<'a> Ifd<'a> {
    /// Read the directory at `at`, answering it and the offset of the next.
    fn read(file: &'a [u8], endian: Endian, at: usize) -> Result<(Self, usize), DecodeError> {
        let count = endian.u16(file, at).ok_or(DecodeError::TiffTruncated)?;
        let entries = at.checked_add(2).ok_or(DecodeError::TiffTruncated)?;
        let span = usize::from(count)
            .checked_mul(ENTRY_LEN)
            .ok_or(DecodeError::TiffTruncated)?;
        let after = entries
            .checked_add(span)
            .ok_or(DecodeError::TiffTruncated)?;
        let next = endian.u32(file, after).ok_or(DecodeError::TiffTruncated)?;
        let next = usize::try_from(next).map_err(|_| DecodeError::TiffTruncated)?;
        Ok((
            Self {
                file,
                endian,
                entries,
                count,
            },
            next,
        ))
    }

    /// The entry for `tag`, or `None` where the directory carries none — or
    /// carries one whose type this decoder has no reading of, which the
    /// format asks a reader to pass over rather than refuse.
    fn field(&self, tag: u16) -> Result<Option<Field>, DecodeError> {
        Ok(self.fields([tag])?[0])
    }

    /// The entries for several tags, found in one pass.
    ///
    /// The chain walk reads a handful of tags from every page, so scanning
    /// once for all of them is what keeps that walk linear in the file
    /// rather than multiplying it by the tags wanted.
    fn fields<const N: usize>(&self, tags: [u16; N]) -> Result<[Option<Field>; N], DecodeError> {
        let mut found = [None; N];
        for index in 0..usize::from(self.count) {
            let at = self.entries + index * ENTRY_LEN;
            let entry = self
                .endian
                .u16(self.file, at)
                .ok_or(DecodeError::TiffTruncated)?;
            let Some(slot) = tags
                .iter()
                .position(|tag| *tag == entry)
                .and_then(|slot| found.get_mut(slot))
            else {
                continue;
            };
            if slot.is_some() {
                continue;
            }
            *slot = self.read_field(at)?;
        }
        Ok(found)
    }

    /// The value span of the entry at `at`, or `None` for a field type this
    /// decoder has no reading of.
    fn read_field(&self, at: usize) -> Result<Option<Field>, DecodeError> {
        let kind = self
            .endian
            .u16(self.file, at + 2)
            .ok_or(DecodeError::TiffTruncated)?;
        let count = self
            .endian
            .u32(self.file, at + 4)
            .ok_or(DecodeError::TiffTruncated)?;
        let Some(size) = type_size(kind) else {
            return Ok(None);
        };
        let bytes = u64::from(count) * u64::from(size);
        let values = at + 8;
        let values = if bytes <= 4 {
            values
        } else {
            usize::try_from(
                self.endian
                    .u32(self.file, values)
                    .ok_or(DecodeError::TiffTruncated)?,
            )
            .map_err(|_| DecodeError::TiffTruncated)?
        };
        let end = u64::try_from(values)
            .ok()
            .and_then(|values| values.checked_add(bytes))
            .ok_or(DecodeError::TiffTruncated)?;
        if end > self.file.len() as u64 {
            return Err(DecodeError::TiffTruncated);
        }
        Ok(Some(Field {
            kind,
            count,
            at: values,
        }))
    }

    /// Element `index` of `field`, read as an unsigned integer.
    fn integer(&self, field: &Field, index: u32) -> Result<u32, DecodeError> {
        if index >= field.count {
            return Err(DecodeError::TiffInvalidTagValue);
        }
        let index = usize::try_from(index).map_err(|_| DecodeError::TiffInvalidTagValue)?;
        match field.kind {
            1 | 7 => self
                .file
                .get(field.at + index)
                .map(|byte| u32::from(*byte))
                .ok_or(DecodeError::TiffTruncated),
            3 => self
                .endian
                .u16(self.file, field.at + index * 2)
                .map(u32::from)
                .ok_or(DecodeError::TiffTruncated),
            4 => self
                .endian
                .u32(self.file, field.at + index * 4)
                .ok_or(DecodeError::TiffTruncated),
            _ => Err(DecodeError::TiffInvalidTagValue),
        }
    }

    /// Element `index` of a RATIONAL field, as its numerator and
    /// denominator.
    fn rational(&self, field: &Field, index: u32) -> Result<(u32, u32), DecodeError> {
        if field.kind != 5 || index >= field.count {
            return Err(DecodeError::TiffInvalidTagValue);
        }
        let at = field.at + usize::try_from(index).unwrap_or(usize::MAX) * 8;
        let numerator = self
            .endian
            .u32(self.file, at)
            .ok_or(DecodeError::TiffTruncated)?;
        let denominator = self
            .endian
            .u32(self.file, at + 4)
            .ok_or(DecodeError::TiffTruncated)?;
        if denominator == 0 {
            return Err(DecodeError::TiffInvalidTagValue);
        }
        Ok((numerator, denominator))
    }

    /// A field's first value, or `fallback` where the directory carried no
    /// such field — refusing when there is no fallback and so no page.
    fn first(&self, field: Option<&Field>, fallback: Option<u32>) -> Result<u32, DecodeError> {
        match field {
            Some(field) if field.count > 0 => self.integer(field, 0),
            _ => fallback.ok_or(DecodeError::TiffMissingTag),
        }
    }

    /// `tag`'s first value, or `fallback` where the directory omits it.
    fn value(&self, tag: u16, fallback: u32) -> Result<u32, DecodeError> {
        match self.field(tag)? {
            Some(field) if field.count > 0 => self.integer(&field, 0),
            _ => Ok(fallback),
        }
    }

    /// `tag`'s first value, refusing a directory that omits a tag its page
    /// cannot be read without.
    fn required(&self, tag: u16) -> Result<u32, DecodeError> {
        let field = self.field(tag)?.ok_or(DecodeError::TiffMissingTag)?;
        if field.count == 0 {
            return Err(DecodeError::TiffMissingTag);
        }
        self.integer(&field, 0)
    }
}

/// How a sample's bits are to be read as a number.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
enum SampleFormat {
    Unsigned,
    Signed,
    Float,
}

/// A page's sample layout, uniform across its samples.
///
/// Samples of differing depth or format ([5, 6, 5] RGB, say) are refused by
/// name: the depths this decoder claims are the six the format's own tables
/// list, and a mixed layout is outside them rather than a subset of them.
#[derive(Copy, Clone, Debug)]
struct Samples {
    count: u32,
    bits: u32,
    format: SampleFormat,
    /// Samples the colour interpretation itself consumes, before extras.
    base: u32,
    /// Which sample carries alpha, and whether it is premultiplied.
    alpha: Option<(u32, bool)>,
}

/// How a page's samples become colours.
#[derive(Copy, Clone, Debug)]
enum Colour {
    /// One sample of brightness; `white_zero` inverts it.
    Grey {
        white_zero: bool,
    },
    Rgb,
    /// One sample indexing the directory's colour map.
    Palette,
    /// One bit per pixel marking a region of another picture.
    Mask,
    /// Four subtractive inks.
    Cmyk,
    YCbCr(YCbCr),
}

/// What a `YCbCr` page needs beyond its samples: how far the chrominance is
/// subsampled, the luma weights, and the coded range each channel uses.
#[derive(Copy, Clone, Debug)]
struct YCbCr {
    horizontal: u32,
    vertical: u32,
    /// Red, green, and blue luma weights, in [`FRAC`] fixed point.
    luma: [i64; 3],
    /// Each channel's coded black point and range.
    black: [i64; 3],
    range: [i64; 3],
}

/// Where a page's pixels are stored: a grid of strips or of tiles.
#[derive(Copy, Clone, Debug)]
struct Grid {
    /// One unit's full extent, before clipping to the picture.
    columns: u32,
    rows: u32,
    across: u32,
    down: u32,
    /// Units one plane holds, which the grid's own extent already bounds.
    per_plane: u32,
    tiled: bool,
}

/// One page, validated whole before anything is reserved for it.
struct Page<'a> {
    ifd: Ifd<'a>,
    width: u32,
    height: u32,
    orientation: Orientation,
    compression: u16,
    colour: Colour,
    samples: Samples,
    planar: bool,
    predictor: u32,
    fill_lsb: bool,
    grid: Grid,
    offsets: Field,
    counts: Field,
    /// The two-dimensional coding a Group 3 page permits, if it does.
    group3_2d: bool,
}

/// A page's declared geometry, as a probe reports it.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
struct Geometry {
    width: u32,
    height: u32,
    reduced: bool,
}

fn area(geometry: Geometry) -> u64 {
    u64::from(geometry.width) * u64::from(geometry.height)
}

/// Read a page's outward geometry — the size after orientation, and whether
/// the file calls the page a reduced copy — without reading its pixels.
fn geometry(ifd: &Ifd<'_>) -> Result<Geometry, DecodeError> {
    let [width, height, orientation, new_kind, old_kind] = ifd.fields([
        TAG_IMAGE_WIDTH,
        TAG_IMAGE_LENGTH,
        TAG_ORIENTATION,
        TAG_NEW_SUBFILE_TYPE,
        TAG_SUBFILE_TYPE,
    ])?;
    let width = ifd.first(width.as_ref(), None)?;
    let height = ifd.first(height.as_ref(), None)?;
    let orientation = Orientation::from_tag(ifd.first(orientation.as_ref(), Some(1))?)
        .ok_or(DecodeError::TiffInvalidOrientation)?;
    let reduced = ifd.first(new_kind.as_ref(), Some(0))? & SUBFILE_REDUCED != 0
        || ifd.first(old_kind.as_ref(), Some(0))? == OLD_SUBFILE_REDUCED;
    let (width, height) = orientation.picture_size(width, height);
    Ok(Geometry {
        width,
        height,
        reduced,
    })
}

impl<'a> Page<'a> {
    /// Validate the directory at `at` into everything decoding it needs.
    fn read(file: &'a [u8], endian: Endian, at: usize) -> Result<Self, DecodeError> {
        let (ifd, _) = Ifd::read(file, endian, at)?;
        let width = ifd.required(TAG_IMAGE_WIDTH)?;
        let height = ifd.required(TAG_IMAGE_LENGTH)?;
        if width == 0 || height == 0 {
            return Err(DecodeError::ZeroDimension);
        }
        let orientation = Orientation::from_tag(ifd.value(TAG_ORIENTATION, 1)?)
            .ok_or(DecodeError::TiffInvalidOrientation)?;
        let compression = u16::try_from(ifd.value(TAG_COMPRESSION, u32::from(COMPRESSION_NONE))?)
            .map_err(|_| DecodeError::TiffUnsupportedCompression)?;
        let fax = matches!(
            compression,
            COMPRESSION_CCITT_RLE | COMPRESSION_GROUP3 | COMPRESSION_GROUP4
        );
        let photometric = match ifd.field(TAG_PHOTOMETRIC)? {
            Some(field) if field.count > 0 => u16::try_from(ifd.integer(&field, 0)?)
                .map_err(|_| DecodeError::TiffUnsupportedPhotometric)?,
            // Every fax is white-is-zero, so a writer that left the tag out
            // of one stated it by choosing the compression.
            _ if fax => PHOTOMETRIC_WHITE_ZERO,
            _ => return Err(DecodeError::TiffMissingTag),
        };
        let samples = read_samples(&ifd, photometric, compression)?;
        let colour = read_colour(&ifd, photometric, &samples)?;
        let planar = match ifd.value(TAG_PLANAR_CONFIGURATION, 1)? {
            1 => false,
            2 => true,
            _ => return Err(DecodeError::TiffUnsupportedPlanarConfiguration),
        };
        if planar && samples.count > 1 {
            if compression == COMPRESSION_JPEG {
                return Err(DecodeError::TiffUnsupportedPlanarConfiguration);
            }
            if let Colour::YCbCr(ycbcr) = colour {
                if ycbcr.horizontal != 1 || ycbcr.vertical != 1 {
                    return Err(DecodeError::TiffUnsupportedPlanarConfiguration);
                }
            }
        }
        if fax && (samples.count != 1 || samples.bits != 1) {
            // A facsimile codes runs of white and black, so it has no
            // reading at all for a page that is not one bit of one sample —
            // and the rows it writes would land at the wrong stride.
            return Err(DecodeError::TiffUnsupportedBitDepth);
        }
        if let Colour::YCbCr(ycbcr) = colour {
            if (ycbcr.horizontal != 1 || ycbcr.vertical != 1) && samples.count != 3 {
                // A subsampled block holds its luminance and the one
                // chrominance pair they share, and nothing else, so a
                // fourth sample has nowhere in it to be.
                return Err(DecodeError::TiffSampleCountMismatch);
            }
        }
        let fill_lsb = match ifd.value(TAG_FILL_ORDER, 1)? {
            1 => false,
            2 if samples.bits == 1 => true,
            // Reversing a byte's bits only reorders whole *pixels* where a
            // pixel is one bit; at any other depth it would reorder each
            // pixel's own bits too.
            _ => return Err(DecodeError::TiffUnsupportedFillOrder),
        };
        let predictor = ifd.value(TAG_PREDICTOR, 1)?;
        let subsampled =
            matches!(colour, Colour::YCbCr(ycbcr) if ycbcr.horizontal != 1 || ycbcr.vertical != 1);
        match predictor {
            1 => {}
            // A subsampled page stores blocks rather than rows of samples,
            // so there is no row for a predictor to run along.
            _ if subsampled => return Err(DecodeError::TiffInvalidPredictor),
            2 if matches!(samples.bits, 8 | 16 | 32) => {}
            3 if samples.format == SampleFormat::Float => {}
            _ => return Err(DecodeError::TiffInvalidPredictor),
        }
        let (grid, offsets, counts) = read_grid(&ifd, width, height, &samples, planar)?;
        let group3_2d = compression == COMPRESSION_GROUP3
            && ifd.value(TAG_T4_OPTIONS, 0)? & 1 != 0
            && !matches!(colour, Colour::YCbCr(_));
        Ok(Self {
            ifd,
            width,
            height,
            orientation,
            compression,
            colour,
            samples,
            planar,
            predictor,
            fill_lsb,
            grid,
            offsets,
            counts,
            group3_2d,
        })
    }
}

/// Read and validate the sample layout: how many, how wide, how signed, and
/// which of them is alpha.
fn read_samples(ifd: &Ifd<'_>, photometric: u16, compression: u16) -> Result<Samples, DecodeError> {
    let count = ifd.value(TAG_SAMPLES_PER_PIXEL, 1)?;
    if count == 0 {
        return Err(DecodeError::TiffSampleCountMismatch);
    }
    // One bit is the narrowest sample, so the pixel-width bound settles the
    // sample count too — and settling it here is what keeps the per-sample
    // scans below short.
    if count > MAX_BITS_PER_PIXEL {
        return Err(DecodeError::TiffPixelTooWide);
    }
    let bits = uniform(ifd, TAG_BITS_PER_SAMPLE, count, 1)?;
    if !matches!(bits, 1 | 2 | 4 | 8 | 16 | 32) {
        return Err(DecodeError::TiffUnsupportedBitDepth);
    }
    let format = match uniform(ifd, TAG_SAMPLE_FORMAT, count, 1)? {
        // An undefined format is read as unsigned, which is what the tag's
        // own default says and what a writer that omits it means.
        1 | 4 => SampleFormat::Unsigned,
        2 => SampleFormat::Signed,
        3 if matches!(bits, 16 | 32) => SampleFormat::Float,
        3 => return Err(DecodeError::TiffUnsupportedBitDepth),
        _ => return Err(DecodeError::TiffUnsupportedSampleFormat),
    };
    if count
        .checked_mul(bits)
        .is_none_or(|width| width > MAX_BITS_PER_PIXEL)
    {
        return Err(DecodeError::TiffPixelTooWide);
    }
    let base = match photometric {
        PHOTOMETRIC_RGB | PHOTOMETRIC_YCBCR => 3,
        PHOTOMETRIC_SEPARATED => 4,
        _ => 1,
    };
    if count < base {
        return Err(DecodeError::TiffSampleCountMismatch);
    }
    // A JPEG unit decodes to colour on its own, so an extra sample beside it
    // would be one the container has nowhere to carry.
    if compression == COMPRESSION_JPEG && count != base {
        return Err(DecodeError::TiffSampleCountMismatch);
    }
    let alpha = read_alpha(ifd, base, count)?;
    Ok(Samples {
        count,
        bits,
        format,
        base,
        alpha,
    })
}

/// The value every element of a per-sample tag holds, refusing one whose
/// elements disagree.
fn uniform(ifd: &Ifd<'_>, tag: u16, count: u32, fallback: u32) -> Result<u32, DecodeError> {
    let Some(field) = ifd.field(tag)? else {
        return Ok(fallback);
    };
    if field.count == 0 {
        return Ok(fallback);
    }
    if field.count != count {
        return Err(DecodeError::TiffSampleCountMismatch);
    }
    let first = ifd.integer(&field, 0)?;
    for index in 1..field.count {
        if ifd.integer(&field, index)? != first {
            return Err(DecodeError::TiffMixedSampleLayout);
        }
    }
    Ok(first)
}

/// Which sample `ExtraSamples` marks as alpha, and whether it is
/// premultiplied. An extra the tag calls unspecified is not alpha.
fn read_alpha(ifd: &Ifd<'_>, base: u32, count: u32) -> Result<Option<(u32, bool)>, DecodeError> {
    let Some(field) = ifd.field(TAG_EXTRA_SAMPLES)? else {
        return Ok(None);
    };
    for index in 0..field.count.min(count - base) {
        let associated = match ifd.integer(&field, index)? {
            1 => true,
            2 => false,
            _ => continue,
        };
        return Ok(Some((base + index, associated)));
    }
    Ok(None)
}

/// Read and validate the colour interpretation.
fn read_colour(ifd: &Ifd<'_>, photometric: u16, samples: &Samples) -> Result<Colour, DecodeError> {
    match photometric {
        PHOTOMETRIC_WHITE_ZERO => Ok(Colour::Grey { white_zero: true }),
        PHOTOMETRIC_BLACK_ZERO => Ok(Colour::Grey { white_zero: false }),
        PHOTOMETRIC_RGB => Ok(Colour::Rgb),
        PHOTOMETRIC_PALETTE if samples.bits <= 8 => {
            let entries = 1u32 << samples.bits;
            let field = ifd
                .field(TAG_COLOUR_MAP)?
                .ok_or(DecodeError::TiffMissingTag)?;
            if field.count != entries * 3 {
                return Err(DecodeError::TiffInvalidColourMap);
            }
            Ok(Colour::Palette)
        }
        PHOTOMETRIC_MASK if samples.bits == 1 && samples.count == 1 => Ok(Colour::Mask),
        PHOTOMETRIC_SEPARATED if ifd.value(TAG_INK_SET, 1)? == 1 => Ok(Colour::Cmyk),
        PHOTOMETRIC_SEPARATED => Err(DecodeError::TiffUnsupportedInkSet),
        PHOTOMETRIC_YCBCR if samples.bits == 8 => Ok(Colour::YCbCr(read_ycbcr(ifd)?)),
        // A palette, a mask, and `YCbCr` each read only at the depths their
        // guards above name; any other is the depth refusal, not a
        // photometric this decoder does not claim.
        PHOTOMETRIC_PALETTE | PHOTOMETRIC_MASK | PHOTOMETRIC_YCBCR => {
            Err(DecodeError::TiffUnsupportedBitDepth)
        }
        _ => Err(DecodeError::TiffUnsupportedPhotometric),
    }
}

/// The chrominance subsampling, luma weights, and coded ranges a `YCbCr`
/// page converts through, defaulted as the format specifies where the tags
/// are absent.
fn read_ycbcr(ifd: &Ifd<'_>) -> Result<YCbCr, DecodeError> {
    let (horizontal, vertical) = match ifd.field(TAG_YCBCR_SUBSAMPLING)? {
        Some(field) if field.count >= 2 => (ifd.integer(&field, 0)?, ifd.integer(&field, 1)?),
        _ => (2, 2),
    };
    if !matches!(horizontal, 1 | 2 | 4) || !matches!(vertical, 1 | 2 | 4) {
        return Err(DecodeError::TiffInvalidSubsampling);
    }
    let mut luma = [299 * ONE / 1000, 587 * ONE / 1000, 114 * ONE / 1000];
    if let Some(field) = ifd.field(TAG_YCBCR_COEFFICIENTS)? {
        if field.count >= 3 {
            for (index, weight) in luma.iter_mut().enumerate() {
                let index = u32::try_from(index).unwrap_or(u32::MAX);
                let (numerator, denominator) = ifd.rational(&field, index)?;
                *weight = i64::from(numerator) * ONE / i64::from(denominator);
            }
        }
    }
    if luma[1] <= 0 {
        return Err(DecodeError::TiffInvalidTagValue);
    }
    let mut black = [0i64, 128, 128];
    let mut range = [255i64, 127, 127];
    if let Some(field) = ifd.field(TAG_REFERENCE_BLACK_WHITE)? {
        if field.count >= 6 {
            for (channel, (low_slot, range_slot)) in
                black.iter_mut().zip(range.iter_mut()).enumerate()
            {
                let channel = u32::try_from(channel).unwrap_or(u32::MAX);
                let (low, low_div) = ifd.rational(&field, channel * 2)?;
                let (high, high_div) = ifd.rational(&field, channel * 2 + 1)?;
                let low = i64::from(low) / i64::from(low_div);
                let high = i64::from(high) / i64::from(high_div);
                if high <= low {
                    return Err(DecodeError::TiffInvalidTagValue);
                }
                *low_slot = low;
                *range_slot = high - low;
            }
        }
    }
    Ok(YCbCr {
        horizontal,
        vertical,
        luma,
        black,
        range,
    })
}

/// A grid of `across` by `down` units, refusing one whose unit count a
/// 32-bit index could not address.
fn grid(columns: u32, rows: u32, across: u32, down: u32, tiled: bool) -> Result<Grid, DecodeError> {
    let per_plane = across
        .checked_mul(down)
        .ok_or(DecodeError::TiffStripCountMismatch)?;
    Ok(Grid {
        columns,
        rows,
        across,
        down,
        per_plane,
        tiled,
    })
}

/// Read the strip or tile grid, and the two arrays that place each unit in
/// the file.
fn read_grid(
    ifd: &Ifd<'_>,
    width: u32,
    height: u32,
    samples: &Samples,
    planar: bool,
) -> Result<(Grid, Field, Field), DecodeError> {
    let planes = if planar { samples.count } else { 1 };
    let (grid, offsets, counts) = if let Some(offsets) = ifd.field(TAG_TILE_OFFSETS)? {
        let columns = ifd.required(TAG_TILE_WIDTH)?;
        let rows = ifd.required(TAG_TILE_LENGTH)?;
        if columns == 0 || rows == 0 || !columns.is_multiple_of(16) || !rows.is_multiple_of(16) {
            return Err(DecodeError::TiffInvalidTileGeometry);
        }
        let counts = ifd
            .field(TAG_TILE_BYTE_COUNTS)?
            .ok_or(DecodeError::TiffMissingTag)?;
        (
            grid(
                columns,
                rows,
                width.div_ceil(columns),
                height.div_ceil(rows),
                true,
            )?,
            offsets,
            counts,
        )
    } else {
        let offsets = ifd
            .field(TAG_STRIP_OFFSETS)?
            .ok_or(DecodeError::TiffMissingTag)?;
        let counts = ifd
            .field(TAG_STRIP_BYTE_COUNTS)?
            .ok_or(DecodeError::TiffMissingTag)?;
        let rows = ifd.value(TAG_ROWS_PER_STRIP, u32::MAX)?;
        if rows == 0 {
            return Err(DecodeError::TiffInvalidTagValue);
        }
        let rows = rows.min(height);
        (
            grid(width, rows, 1, height.div_ceil(rows), false)?,
            offsets,
            counts,
        )
    };
    let units = grid
        .per_plane
        .checked_mul(planes)
        .ok_or(DecodeError::TiffStripCountMismatch)?;
    if offsets.count < units || counts.count < units {
        return Err(DecodeError::TiffStripCountMismatch);
    }
    Ok((grid, offsets, counts))
}

/// The codec state a decode reuses across every unit and every page: an
/// LZW dictionary and the fax tables are each built once and kept, because
/// building either per strip would dominate decoding one.
struct Codecs {
    lzw: Option<Lzw>,
    fax: Option<ccitt::Codes>,
    jpeg: Vec<u8>,
}

/// The buffers a decode reuses, held apart from each other so a unit can be
/// decompressed into one while the codecs work from the others.
struct Scratch {
    unit: Vec<u8>,
    shuffle: Vec<u8>,
    codecs: Codecs,
}

impl Scratch {
    const fn new() -> Self {
        Self {
            unit: Vec::new(),
            shuffle: Vec::new(),
            codecs: Codecs {
                lzw: None,
                fax: None,
                jpeg: Vec::new(),
            },
        }
    }
}

/// A reader over the flat run of bytes TIFF packs its LZW codes into.
///
/// `lsb_first` is the classic dialect's packing, where a code's first bit is
/// a byte's least significant rather than its most.
struct BitCodes<'a> {
    data: &'a [u8],
    at: u64,
    lsb_first: bool,
}

impl CodeSource for BitCodes<'_> {
    fn code(&mut self, width: u32) -> Result<Option<u16>, DecodeError> {
        let total = self.data.len() as u64 * 8;
        if self.at + u64::from(width) > total {
            return Ok(None);
        }
        let mut value = 0u32;
        for offset in 0..u64::from(width) {
            let at = self.at + offset;
            let byte = self.data.get((at / 8) as usize).copied().unwrap_or(0);
            let bit = if self.lsb_first {
                u32::from(byte >> (at % 8)) & 1
            } else {
                u32::from(byte >> (7 - at % 8)) & 1
            };
            value = if self.lsb_first {
                value | bit << offset
            } else {
                value << 1 | bit
            };
        }
        self.at += u64::from(width);
        u16::try_from(value)
            .map(Some)
            .map_err(|_| DecodeError::TiffInvalidCode)
    }
}

/// Expand an Apple `PackBits` run-length stream into `out`.
fn unpack_bits(data: &[u8], out: &mut [u8]) -> Result<(), DecodeError> {
    let mut read = 0usize;
    let mut written = 0usize;
    while written < out.len() {
        let Some(&control) = data.get(read) else {
            return Err(DecodeError::TiffStripTruncated);
        };
        read += 1;
        // A control byte is a signed count: non-negative literals the bytes
        // that follow, negative repeats the one that does, and -128 is a
        // no-op the format reserves.
        let control = control.cast_signed();
        if control >= 0 {
            let run = usize::from(control.unsigned_abs()) + 1;
            let take = run.min(out.len() - written);
            let source = read
                .checked_add(run)
                .and_then(|end| data.get(read..end))
                .ok_or(DecodeError::TiffStripTruncated)?;
            out[written..written + take].copy_from_slice(&source[..take]);
            read += run;
            written += take;
        } else if control != -128 {
            let run = usize::from(control.unsigned_abs()) + 1;
            let &byte = data.get(read).ok_or(DecodeError::TiffStripTruncated)?;
            read += 1;
            let take = run.min(out.len() - written);
            out[written..written + take].fill(byte);
            written += take;
        }
    }
    Ok(())
}

/// Whether an LZW stream was written in the classic dialect, which packs a
/// code least significant bit first *and* widens one code later than TIFF's
/// own — two differences that always travel together, because they are the
/// two halves of what older writers emitted.
///
/// A clear code opens every stream, so its two spellings tell the dialects
/// apart exactly: `0x80 0x00` most significant bit first, `0x00 0x01` least.
fn classic_lzw(data: &[u8]) -> bool {
    matches!(data, [0x00, second, ..] if second & 1 != 0)
}

/// Expand one unit's bytes into `out`, which is exactly the raw sample bytes
/// the unit holds.
fn decompress(
    page: &Page<'_>,
    codecs: &mut Codecs,
    data: &[u8],
    rows: u32,
    columns: u32,
    out: &mut [u8],
) -> Result<(), DecodeError> {
    match page.compression {
        COMPRESSION_NONE => {
            let source = data
                .get(..out.len())
                .ok_or(DecodeError::TiffStripTruncated)?;
            out.copy_from_slice(source);
            Ok(())
        }
        COMPRESSION_PACK_BITS => unpack_bits(data, out),
        COMPRESSION_LZW => {
            let lzw = match &mut codecs.lzw {
                Some(lzw) => lzw,
                slot => slot.insert(Lzw::new().ok_or(DecodeError::OutOfMemory)?),
            };
            let classic = classic_lzw(data);
            let widen = if classic {
                Widen::WhenFull
            } else {
                Widen::OneEarly
            };
            let mut stream = BitCodes {
                data,
                at: 0,
                lsb_first: classic,
            };
            let written = lzw.expand(&mut stream, 8, widen, &DecodeError::TiffInvalidCode, out)?;
            if written != out.len() {
                return Err(DecodeError::TiffStripTruncated);
            }
            Ok(())
        }
        COMPRESSION_ADOBE_DEFLATE | COMPRESSION_DEFLATE => {
            let written =
                zlib::decompress_into(data, out).map_err(DecodeError::TiffCompressedData)?;
            if written != out.len() {
                return Err(DecodeError::TiffStripTruncated);
            }
            Ok(())
        }
        COMPRESSION_CCITT_RLE | COMPRESSION_GROUP3 | COMPRESSION_GROUP4 => {
            let tables = match &mut codecs.fax {
                Some(tables) => tables,
                slot => slot.insert(ccitt::Codes::new().ok_or(DecodeError::OutOfMemory)?),
            };
            let coding = match page.compression {
                COMPRESSION_CCITT_RLE => ccitt::Coding::ModifiedHuffman,
                COMPRESSION_GROUP4 => ccitt::Coding::Group4,
                _ => ccitt::Coding::Group3 {
                    two_dimensional: page.group3_2d,
                },
            };
            out.fill(0);
            ccitt::decode(data, tables, coding, columns, rows, page.fill_lsb, out)
        }
        _ => Err(DecodeError::TiffUnsupportedCompression),
    }
}

/// Undo the horizontal predictor over one row's samples.
fn undo_horizontal(row: &mut [u8], endian: Endian, bits: u32, stride: usize, count: usize) {
    match bits {
        16 => {
            for index in stride..count {
                let previous = read_u16(row, index - stride, endian);
                let current = read_u16(row, index, endian);
                write_u16(row, index, current.wrapping_add(previous), endian);
            }
        }
        32 => {
            for index in stride..count {
                let previous = read_u32(row, index - stride, endian);
                let current = read_u32(row, index, endian);
                write_u32(row, index, current.wrapping_add(previous), endian);
            }
        }
        _ => {
            for index in stride..count {
                let Some(&previous) = row.get(index - stride) else {
                    return;
                };
                if let Some(byte) = row.get_mut(index) {
                    *byte = byte.wrapping_add(previous);
                }
            }
        }
    }
}

fn read_u16(row: &[u8], index: usize, endian: Endian) -> u16 {
    endian.u16(row, index * 2).unwrap_or(0)
}

fn write_u16(row: &mut [u8], index: usize, value: u16, endian: Endian) {
    let bytes = match endian {
        Endian::Little => value.to_le_bytes(),
        Endian::Big => value.to_be_bytes(),
    };
    if let Some(slot) = row.get_mut(index * 2..index * 2 + 2) {
        slot.copy_from_slice(&bytes);
    }
}

fn read_u32(row: &[u8], index: usize, endian: Endian) -> u32 {
    endian.u32(row, index * 4).unwrap_or(0)
}

fn write_u32(row: &mut [u8], index: usize, value: u32, endian: Endian) {
    let bytes = match endian {
        Endian::Little => value.to_le_bytes(),
        Endian::Big => value.to_be_bytes(),
    };
    if let Some(slot) = row.get_mut(index * 4..index * 4 + 4) {
        slot.copy_from_slice(&bytes);
    }
}

/// Undo the floating-point predictor over one row.
///
/// The row holds each sample's bytes gathered into planes, most significant
/// plane first, horizontally differenced byte by byte. Accumulating puts the
/// bytes back and the shuffle puts each sample's own back together, in the
/// file's byte order.
fn undo_floating_point(
    row: &mut [u8],
    shuffle: &mut Vec<u8>,
    endian: Endian,
    width: usize,
    stride: usize,
) -> Result<(), DecodeError> {
    for index in stride..row.len() {
        let Some(&previous) = row.get(index - stride) else {
            return Ok(());
        };
        if let Some(byte) = row.get_mut(index) {
            *byte = byte.wrapping_add(previous);
        }
    }
    let samples = row.len() / width;
    if !fallible::grow_to(shuffle, row.len(), 0u8) {
        return Err(DecodeError::OutOfMemory);
    }
    let shuffle = shuffle
        .get_mut(..row.len())
        .ok_or(DecodeError::OutOfMemory)?;
    shuffle.copy_from_slice(row);
    for sample in 0..samples {
        for byte in 0..width {
            // Plane zero holds every sample's most significant byte, so on a
            // little-endian file that is the sample's *last* byte.
            let plane = match endian {
                Endian::Big => byte,
                Endian::Little => width - 1 - byte,
            };
            if let (Some(&from), Some(to)) = (
                shuffle.get(plane * samples + sample),
                row.get_mut(sample * width + byte),
            ) {
                *to = from;
            }
        }
    }
    Ok(())
}

/// One sample's raw bits, read from `data` at `bit`.
fn raw_sample(data: &[u8], bit: usize, bits: u32, endian: Endian) -> u32 {
    match bits {
        8 => data.get(bit / 8).copied().map_or(0, u32::from),
        16 => u32::from(endian.u16(data, bit / 8).unwrap_or(0)),
        32 => endian.u32(data, bit / 8).unwrap_or(0),
        _ => {
            let byte = data.get(bit / 8).copied().unwrap_or(0);
            let within = u32::try_from(bit % 8).unwrap_or(0);
            // Samples narrower than a byte divide it exactly, so this is the
            // sample's own shift rather than a saturated one.
            let shift = 8u32.saturating_sub(bits + within);
            u32::from(byte >> shift) & ((1 << bits) - 1)
        }
    }
}

/// Turn one raw sample into eight bits, as its declared format reads it.
fn normalise(raw: u32, layout: &Samples, sampler: &Sampler) -> u8 {
    match layout.format {
        SampleFormat::Unsigned => sampler.sample(raw),
        // A signed sample's range is the unsigned one shifted down by half,
        // so flipping its sign bit maps it back onto the same scale.
        SampleFormat::Signed => sampler.sample(raw ^ (1 << (layout.bits - 1))),
        SampleFormat::Float => float_to_byte(raw, layout.bits),
    }
}

/// Scale an IEEE half or single float in `raw` onto `0..=255`, clamping
/// outside the unit interval.
fn float_to_byte(raw: u32, width: u32) -> u8 {
    let (mantissa_bits, exponent_bits) = if width == 16 { (10, 5) } else { (23, 8) };
    let bias = (1u32 << (exponent_bits - 1)) - 1;
    let exponent = raw >> mantissa_bits & ((1 << exponent_bits) - 1);
    let mantissa = raw & ((1 << mantissa_bits) - 1);
    if raw >> (mantissa_bits + exponent_bits) & 1 == 1 {
        return 0;
    }
    if exponent == (1 << exponent_bits) - 1 {
        // Infinity saturates; a value that is not a number has no place on
        // the scale at all.
        return if mantissa == 0 { u8::MAX } else { 0 };
    }
    if exponent >= bias {
        return u8::MAX;
    }
    let significand = if exponent == 0 {
        u64::from(mantissa)
    } else {
        u64::from(mantissa) | 1 << mantissa_bits
    };
    let shift = mantissa_bits + bias - exponent.max(1);
    if shift >= 64 {
        return 0;
    }
    let scaled = (significand * 255 + (1 << (shift - 1))) >> shift;
    u8::try_from(scaled).unwrap_or(u8::MAX)
}

/// A page's colour map, read once and widened to eight bits per channel.
struct Palette {
    entries: Vec<[u8; 3]>,
}

impl Palette {
    fn read(ifd: &Ifd<'_>, bits: u32) -> Result<Self, DecodeError> {
        let field = ifd
            .field(TAG_COLOUR_MAP)?
            .ok_or(DecodeError::TiffMissingTag)?;
        let count = 1u32 << bits;
        let mut entries = usize::try_from(count)
            .ok()
            .and_then(|count| fallible::filled(count, [0u8; 3]))
            .ok_or(DecodeError::OutOfMemory)?;
        for (index, entry) in entries.iter_mut().enumerate() {
            let index = u32::try_from(index).unwrap_or(u32::MAX);
            for (channel, slot) in entry.iter_mut().enumerate() {
                let channel = u32::try_from(channel).unwrap_or(u32::MAX);
                let value = ifd.integer(&field, channel * count + index)?;
                // The map's entries are sixteen bits wide whatever the
                // samples that index them are.
                *slot = u8::try_from(value >> 8).unwrap_or(u8::MAX);
            }
        }
        Ok(Self { entries })
    }
}

/// Everything a page's pixel conversion needs that does not change per
/// pixel.
struct Convert {
    sampler: Sampler,
    palette: Option<Palette>,
}

/// Turn one pixel's gathered samples into straight-alpha RGBA.
fn to_rgba(page: &Page<'_>, convert: &Convert, raw: &[u32; 5]) -> [u8; 4] {
    let samples = &page.samples;
    let value = |index: usize| normalise(raw[index], samples, &convert.sampler);
    let mut pixel = match page.colour {
        Colour::Grey { white_zero } => {
            let grey = value(0);
            let grey = if white_zero { u8::MAX - grey } else { grey };
            [grey, grey, grey, u8::MAX]
        }
        Colour::Rgb => [value(0), value(1), value(2), u8::MAX],
        // A sample is as wide as the map is long, so no index can miss.
        Colour::Palette => {
            let entry = convert
                .palette
                .as_ref()
                .and_then(|palette| palette.entries.get(usize::try_from(raw[0]).unwrap_or(0)))
                .copied()
                .unwrap_or([0, 0, 0]);
            [entry[0], entry[1], entry[2], u8::MAX]
        }
        // A mask states a region rather than a colour, so its interior is
        // drawn as an opaque silhouette and its exterior as nothing at all.
        Colour::Mask => {
            if raw[0] == 0 {
                [0, 0, 0, 0]
            } else {
                [0, 0, 0, u8::MAX]
            }
        }
        Colour::Cmyk => {
            let ink = [value(0), value(1), value(2), value(3)];
            let key = u32::from(u8::MAX - ink[3]);
            let channel = |index: usize| {
                let remaining = u32::from(u8::MAX - ink[index]);
                u8::try_from(remaining * key / 255).unwrap_or(u8::MAX)
            };
            [channel(0), channel(1), channel(2), u8::MAX]
        }
        Colour::YCbCr(ycbcr) => ycbcr_to_rgb(&ycbcr, raw),
    };
    if let Some((_, associated)) = samples.alpha {
        let alpha = normalise(raw[4], samples, &convert.sampler);
        pixel[3] = alpha;
        if associated && alpha != 0 {
            // A premultiplied sample is the colour already scaled by its own
            // alpha, and the crate's output is straight.
            for channel in &mut pixel[..3] {
                let widened = u32::from(*channel) * 255 / u32::from(alpha);
                *channel = u8::try_from(widened).unwrap_or(u8::MAX);
            }
        } else if associated {
            pixel = [0, 0, 0, 0];
        }
    }
    pixel
}

/// Convert one `YCbCr` triple through the page's coded ranges and luma
/// weights.
fn ycbcr_to_rgb(ycbcr: &YCbCr, raw: &[u32; 5]) -> [u8; 4] {
    let coded = |index: usize, scale: i64| {
        (i64::from(raw[index]) - ycbcr.black[index]) * scale * ONE / ycbcr.range[index]
    };
    let luma = coded(0, 255);
    let blue = coded(1, 127);
    let red = coded(2, 127);
    let r = ((red * (2 * ONE - 2 * ycbcr.luma[0])) >> FRAC) + luma;
    let b = ((blue * (2 * ONE - 2 * ycbcr.luma[2])) >> FRAC) + luma;
    let g = (luma - ((ycbcr.luma[2] * b) >> FRAC) - ((ycbcr.luma[0] * r) >> FRAC)) * ONE
        / ycbcr.luma[1];
    let clamp = |value: i64| {
        u8::try_from(((value + ONE / 2) >> FRAC).clamp(0, i64::from(u8::MAX))).unwrap_or(u8::MAX)
    };
    [clamp(r), clamp(g), clamp(b), u8::MAX]
}

/// How one unit's samples are laid out, and how many bytes of them a unit
/// holds per plane.
#[derive(Copy, Clone, Debug)]
struct UnitLayout {
    planes: u32,
    /// Samples one plane carries per pixel: every sample for a chunky page,
    /// one for a planar one.
    per_plane: u32,
    row_bytes: usize,
    /// Bytes one plane of one whole unit holds.
    plane_bytes: usize,
    /// The chrominance-subsampled arrangement, where the page has one.
    blocks: Option<Blocks>,
}

/// A subsampled `YCbCr` page's storage unit: a block of luminance samples
/// followed by the one chrominance pair they share.
#[derive(Copy, Clone, Debug)]
struct Blocks {
    horizontal: u32,
    vertical: u32,
    across: u32,
    /// Bytes one block occupies.
    bytes: usize,
}

impl Blocks {
    fn down(self, rows: u32) -> u32 {
        rows.div_ceil(self.vertical)
    }
}

impl Page<'_> {
    /// Rows the unit in grid row `down` covers: every tile is whole, and
    /// only a strip is cut short by the picture's last row.
    const fn unit_rows(&self, down: u32) -> u32 {
        if self.grid.tiled {
            self.grid.rows
        } else {
            let top = down * self.grid.rows;
            let left = self.height - top;
            if left < self.grid.rows {
                left
            } else {
                self.grid.rows
            }
        }
    }

    /// Work out how one unit's samples are stored, and how many bytes of
    /// them a whole unit holds.
    fn layout(&self, rows: u32) -> Result<UnitLayout, DecodeError> {
        let planes = if self.planar { self.samples.count } else { 1 };
        let per_plane = if self.planar { 1 } else { self.samples.count };
        if let Colour::YCbCr(ycbcr) = self.colour {
            if ycbcr.horizontal != 1 || ycbcr.vertical != 1 {
                let across = self.grid.columns.div_ceil(ycbcr.horizontal);
                let bytes = usize::try_from(ycbcr.horizontal * ycbcr.vertical + 2)
                    .map_err(|_| DecodeError::DimensionsOverflow)?;
                let blocks = Blocks {
                    horizontal: ycbcr.horizontal,
                    vertical: ycbcr.vertical,
                    across,
                    bytes,
                };
                let plane_bytes = usize::try_from(across)
                    .ok()
                    .and_then(|across| across.checked_mul(usize::try_from(blocks.down(rows)).ok()?))
                    .and_then(|blocks_total| blocks_total.checked_mul(bytes))
                    .ok_or(DecodeError::DimensionsOverflow)?;
                return Ok(UnitLayout {
                    planes,
                    per_plane,
                    row_bytes: 0,
                    plane_bytes,
                    blocks: Some(blocks),
                });
            }
        }
        let row_bits =
            u64::from(self.grid.columns) * u64::from(per_plane) * u64::from(self.samples.bits);
        let row_bytes =
            usize::try_from(row_bits.div_ceil(8)).map_err(|_| DecodeError::DimensionsOverflow)?;
        let plane_bytes = usize::try_from(rows)
            .ok()
            .and_then(|rows| rows.checked_mul(row_bytes))
            .ok_or(DecodeError::DimensionsOverflow)?;
        Ok(UnitLayout {
            planes,
            per_plane,
            row_bytes,
            plane_bytes,
            blocks: None,
        })
    }

    /// The bytes unit `index` occupies in the file.
    fn unit_data<'f>(&self, file: &'f [u8], index: u32) -> Result<&'f [u8], DecodeError> {
        let at = usize::try_from(self.ifd.integer(&self.offsets, index)?)
            .map_err(|_| DecodeError::TiffTruncated)?;
        let len = usize::try_from(self.ifd.integer(&self.counts, index)?)
            .map_err(|_| DecodeError::TiffTruncated)?;
        at.checked_add(len)
            .and_then(|end| file.get(at..end))
            .ok_or(DecodeError::TiffTruncated)
    }

    /// Undo whatever predictor the page declares over one unit's plane.
    fn unpredict(
        &self,
        plane: &mut [u8],
        shuffle: &mut Vec<u8>,
        layout: &UnitLayout,
        rows: u32,
    ) -> Result<(), DecodeError> {
        if self.predictor == 1 {
            return Ok(());
        }
        let stride =
            usize::try_from(layout.per_plane).map_err(|_| DecodeError::DimensionsOverflow)?;
        let count = usize::try_from(self.grid.columns)
            .ok()
            .and_then(|columns| columns.checked_mul(stride))
            .ok_or(DecodeError::DimensionsOverflow)?;
        let width = usize::try_from(self.samples.bits / 8).unwrap_or(1).max(1);
        for row in 0..usize::try_from(rows).unwrap_or(0) {
            let from = row * layout.row_bytes;
            let Some(row) = plane.get_mut(from..from + layout.row_bytes) else {
                return Err(DecodeError::DimensionsOverflow);
            };
            if self.predictor == 3 {
                undo_floating_point(row, shuffle, self.ifd.endian, width, stride)?;
            } else {
                undo_horizontal(row, self.ifd.endian, self.samples.bits, stride, count);
            }
        }
        Ok(())
    }
}

/// Write one pixel at the position the page's orientation puts it.
fn place(out: &mut [u8], width: u32, x: u32, y: u32, pixel: [u8; 4]) {
    let Some(at) = u64::from(y)
        .checked_mul(u64::from(width))
        .and_then(|row| row.checked_add(u64::from(x)))
        .and_then(|index| index.checked_mul(RGBA_BYTES as u64))
        .and_then(|at| usize::try_from(at).ok())
    else {
        return;
    };
    if let Some(slot) = out.get_mut(at..at + RGBA_BYTES) {
        slot.copy_from_slice(&pixel);
    }
}

/// The picture a page decodes to, after its orientation is applied.
const fn output_size(page: &Page<'_>) -> (u32, u32) {
    page.orientation.picture_size(page.width, page.height)
}

/// Decode one page into a straight-alpha RGBA image.
fn decode_page(
    file: &[u8],
    page: &Page<'_>,
    limits: &DecodeLimits,
    scratch: &mut Scratch,
) -> Result<RasterImage, DecodeError> {
    let (out_width, out_height) = output_size(page);
    limits.check(out_width, out_height)?;
    let out_len = usize::try_from(
        u64::from(out_width)
            .checked_mul(u64::from(out_height))
            .and_then(|pixels| pixels.checked_mul(RGBA_BYTES as u64))
            .ok_or(DecodeError::DimensionsOverflow)?,
    )
    .map_err(|_| DecodeError::DimensionsOverflow)?;
    let mut out = fallible::filled(out_len, 0u8).ok_or(DecodeError::OutOfMemory)?;
    let Scratch {
        unit: buffer,
        shuffle,
        codecs,
    } = scratch;
    if page.compression == COMPRESSION_JPEG {
        decode_jpeg_page(file, page, limits, codecs, &mut out, out_width, out_height)?;
        return Ok(RasterImage::from_parts(out_width, out_height, out));
    }
    let convert = Convert {
        sampler: Sampler::new(Channel::fixed(0, page.samples.bits)),
        palette: match page.colour {
            Colour::Palette => Some(Palette::read(&page.ifd, page.samples.bits)?),
            _ => None,
        },
    };
    // A tile's own extent is not bounded by the picture — a writer may use
    // 256-pixel tiles for a 100-pixel image, and nothing in the format caps
    // one — so the working buffer behind a unit is weighed against the same
    // limits the picture was, before it is reserved.
    limits.check(page.grid.columns, page.grid.rows)?;
    let widest = page.layout(page.grid.rows)?;
    let scratch_len = usize::try_from(widest.planes)
        .ok()
        .and_then(|planes| planes.checked_mul(widest.plane_bytes))
        .ok_or(DecodeError::DimensionsOverflow)?;
    if !fallible::grow_to(buffer, scratch_len, 0u8) {
        return Err(DecodeError::OutOfMemory);
    }
    for down in 0..page.grid.down {
        let rows = page.unit_rows(down);
        let layout = page.layout(rows)?;
        for across in 0..page.grid.across {
            let unit = down * page.grid.across + across;
            for plane in 0..layout.planes {
                let index = plane * page.grid.per_plane + unit;
                let data = page.unit_data(file, index)?;
                let from = usize::try_from(plane).unwrap_or(0) * layout.plane_bytes;
                let target = buffer
                    .get_mut(from..from + layout.plane_bytes)
                    .ok_or(DecodeError::DimensionsOverflow)?;
                decompress(page, codecs, data, rows, page.grid.columns, target)?;
                page.unpredict(target, shuffle, &layout, rows)?;
            }
            emit_unit(
                page,
                &convert,
                buffer,
                &layout,
                (across * page.grid.columns, down * page.grid.rows),
                rows,
                (out_width, out_height),
                &mut out,
            );
        }
    }
    Ok(RasterImage::from_parts(out_width, out_height, out))
}

/// Write one decoded unit's pixels into the output.
#[allow(clippy::too_many_arguments)]
fn emit_unit(
    page: &Page<'_>,
    convert: &Convert,
    unit: &[u8],
    layout: &UnitLayout,
    origin: (u32, u32),
    rows: u32,
    output: (u32, u32),
    out: &mut [u8],
) {
    let (out_width, out_height) = output;
    let columns = page.grid.columns.min(page.width.saturating_sub(origin.0));
    let rows = rows.min(page.height.saturating_sub(origin.1));
    for y in 0..rows {
        for x in 0..columns {
            let raw = match layout.blocks {
                Some(blocks) => gather_blocks(unit, &blocks, x, y),
                None => gather_samples(page, unit, layout, x, y),
            };
            let pixel = to_rgba(page, convert, &raw);
            let (dx, dy) =
                page.orientation
                    .place(origin.0 + x, origin.1 + y, out_width, out_height);
            place(out, out_width, dx, dy, pixel);
        }
    }
}

/// Read the samples one pixel of a plainly-arranged unit holds.
fn gather_samples(page: &Page<'_>, unit: &[u8], layout: &UnitLayout, x: u32, y: u32) -> [u32; 5] {
    let bits = usize::try_from(page.samples.bits).unwrap_or(0);
    let per_plane = usize::try_from(layout.per_plane).unwrap_or(1);
    let row = usize::try_from(y).unwrap_or(0) * layout.row_bytes * 8;
    let column = usize::try_from(x).unwrap_or(0) * per_plane * bits;
    let read = |sample: u32| {
        let (plane, within) = if page.planar {
            (usize::try_from(sample).unwrap_or(0), 0)
        } else {
            (0, usize::try_from(sample).unwrap_or(0))
        };
        let at = plane * layout.plane_bytes * 8 + row + column + within * bits;
        raw_sample(unit, at, page.samples.bits, page.ifd.endian)
    };
    let mut raw = [0u32; 5];
    for (sample, slot) in raw.iter_mut().take(page.samples.base as usize).enumerate() {
        *slot = read(u32::try_from(sample).unwrap_or(0));
    }
    if let Some((index, _)) = page.samples.alpha {
        raw[4] = read(index);
    }
    raw
}

/// Read the samples one pixel of a chrominance-subsampled unit holds.
fn gather_blocks(unit: &[u8], blocks: &Blocks, x: u32, y: u32) -> [u32; 5] {
    let luma = blocks.horizontal * blocks.vertical;
    let block = usize::try_from((y / blocks.vertical) * blocks.across + x / blocks.horizontal)
        .unwrap_or(0)
        * blocks.bytes;
    let within = usize::try_from((y % blocks.vertical) * blocks.horizontal + x % blocks.horizontal)
        .unwrap_or(0);
    let byte = |at: usize| unit.get(at).copied().map_or(0, u32::from);
    let chroma = usize::try_from(luma).unwrap_or(0);
    [
        byte(block + within),
        byte(block + chroma),
        byte(block + chroma + 1),
        0,
        0,
    ]
}

/// Decode a page whose units are each a JPEG stream of their own.
#[allow(clippy::too_many_arguments)]
fn decode_jpeg_page(
    file: &[u8],
    page: &Page<'_>,
    limits: &DecodeLimits,
    codecs: &mut Codecs,
    out: &mut [u8],
    out_width: u32,
    out_height: u32,
) -> Result<(), DecodeError> {
    let tables = match page.ifd.field(TAG_JPEG_TABLES)? {
        Some(field) => usize::try_from(field.count)
            .ok()
            .and_then(|count| field.at.checked_add(count))
            .and_then(|end| file.get(field.at..end))
            .ok_or(DecodeError::TiffTruncated)?,
        None => &[][..],
    };
    for down in 0..page.grid.down {
        let rows = page.unit_rows(down);
        for across in 0..page.grid.across {
            let unit = down * page.grid.across + across;
            let data = page.unit_data(file, unit)?;
            let stream = splice_jpeg(&mut codecs.jpeg, tables, data)?;
            let image = jpeg::decode(stream, limits)?;
            if image.width() != page.grid.columns || image.height() != rows {
                return Err(DecodeError::TiffJpegGeometryMismatch);
            }
            let origin = (across * page.grid.columns, down * page.grid.rows);
            let columns = page.grid.columns.min(page.width.saturating_sub(origin.0));
            let rows = rows.min(page.height.saturating_sub(origin.1));
            for y in 0..rows {
                for x in 0..columns {
                    let at = usize::try_from(
                        (u64::from(y) * u64::from(image.width()) + u64::from(x))
                            * RGBA_BYTES as u64,
                    )
                    .unwrap_or(usize::MAX);
                    let Some(pixel) = image
                        .pixels()
                        .get(at..at + RGBA_BYTES)
                        .and_then(|slice| <[u8; RGBA_BYTES]>::try_from(slice).ok())
                    else {
                        return Err(DecodeError::TiffJpegGeometryMismatch);
                    };
                    let (dx, dy) =
                        page.orientation
                            .place(origin.0 + x, origin.1 + y, out_width, out_height);
                    place(out, out_width, dx, dy, pixel);
                }
            }
        }
    }
    Ok(())
}

/// The markers that open and close a JPEG stream, which an abbreviated one
/// is spliced between.
const SOI: [u8; 2] = [0xFF, 0xD8];
const EOI: [u8; 2] = [0xFF, 0xD9];

/// Build the complete JPEG stream one unit codes, from the tables the
/// directory holds apart and the unit's own abbreviated data.
fn splice_jpeg<'s>(
    scratch: &'s mut Vec<u8>,
    tables: &[u8],
    unit: &'s [u8],
) -> Result<&'s [u8], DecodeError> {
    if tables.is_empty() {
        return Ok(unit);
    }
    let tables = tables.strip_prefix(&SOI).unwrap_or(tables);
    let tables = tables.strip_suffix(&EOI).unwrap_or(tables);
    let unit = unit.strip_prefix(&SOI).unwrap_or(unit);
    scratch.clear();
    let len = 2 + tables.len() + unit.len();
    if !fallible::reserve(scratch, len) {
        return Err(DecodeError::OutOfMemory);
    }
    scratch.extend_from_slice(&SOI);
    scratch.extend_from_slice(tables);
    scratch.extend_from_slice(unit);
    Ok(scratch)
}

/// The page a plain decode answers, and the largest the container holds.
#[derive(Copy, Clone, Debug)]
struct Measured {
    primary: u32,
    geometry: Geometry,
    canvas: Geometry,
}

/// Read the file header: its byte order, and where the first directory is.
fn header(file: &[u8]) -> Result<(Endian, usize), DecodeError> {
    let endian = match file.first_chunk::<2>() {
        Some(b"II") => Endian::Little,
        Some(b"MM") => Endian::Big,
        _ => return Err(DecodeError::TiffBadSignature),
    };
    match endian.u16(file, 2).ok_or(DecodeError::TiffTruncated)? {
        42 => {}
        43 => return Err(DecodeError::TiffBigTiffUnsupported),
        _ => return Err(DecodeError::TiffBadSignature),
    }
    let first = usize::try_from(endian.u32(file, 4).ok_or(DecodeError::TiffTruncated)?)
        .map_err(|_| DecodeError::TiffTruncated)?;
    if first == 0 {
        return Err(DecodeError::TiffNoPages);
    }
    Ok((endian, first))
}

/// A page container's chain of directories, walked one page at a time.
///
/// The offset of the page last located is kept, so a sequential walk costs
/// one link per page rather than re-walking the chain for each.
pub(crate) struct Chain<'a> {
    file: &'a [u8],
    endian: Endian,
    first: usize,
    count: u32,
    located: (u32, usize),
    scratch: Scratch,
}

impl<'a> Chain<'a> {
    /// Validate the chain and measure its pages, decoding none of them.
    ///
    /// A directory that will not parse is fatal, because the chain is what
    /// finds the next page. A page whose *geometry* will not read is not: it
    /// is passed over here and refused only if it is asked for, exactly as
    /// one page of an icon file is.
    fn open(file: &'a [u8]) -> Result<(Self, Measured), DecodeError> {
        let (endian, first) = header(file)?;
        // A directory cannot be shorter than its own count and link, so a
        // chain longer than the file has room for must be revisiting one.
        let limit = u32::try_from(file.len() / MIN_IFD_LEN)
            .unwrap_or(u32::MAX)
            .min(MAX_PAGES);
        let mut budget = file.len() / ENTRY_LEN;
        let mut at = first;
        let mut count = 0u32;
        let mut primary: Option<(u32, Geometry)> = None;
        let mut fallback: Option<(u32, Geometry)> = None;
        let mut canvas: Option<Geometry> = None;
        let mut refusal: Option<DecodeError> = None;
        while at != 0 {
            if count >= limit {
                return Err(DecodeError::TiffTooManyPages);
            }
            let (ifd, next) = Ifd::read(file, endian, at)?;
            budget = budget
                .checked_sub(usize::from(ifd.count))
                .ok_or(DecodeError::TiffTooManyPages)?;
            match geometry(&ifd) {
                Ok(found) => {
                    if canvas.is_none_or(|best| area(found) > area(best)) {
                        canvas = Some(found);
                    }
                    if fallback.is_none() {
                        fallback = Some((count, found));
                    }
                    if !found.reduced && primary.is_none() {
                        primary = Some((count, found));
                    }
                }
                Err(err) => refusal = refusal.or(Some(err)),
            }
            count += 1;
            at = next;
        }
        let (primary, geometry) = primary
            .or(fallback)
            .ok_or_else(|| refusal.unwrap_or(DecodeError::TiffNoPages))?;
        let canvas = canvas.unwrap_or(geometry);
        Ok((
            Self {
                file,
                endian,
                first,
                count,
                located: (0, first),
                scratch: Scratch::new(),
            },
            Measured {
                primary,
                geometry,
                canvas,
            },
        ))
    }

    /// Where the directory of the page at `index` begins.
    fn locate(&mut self, index: u32) -> Result<usize, DecodeError> {
        let (mut from, mut at) = if index >= self.located.0 {
            self.located
        } else {
            (0, self.first)
        };
        while from < index {
            let (_, next) = Ifd::read(self.file, self.endian, at)?;
            if next == 0 {
                return Err(DecodeError::TiffNoPages);
            }
            at = next;
            from += 1;
        }
        self.located = (index, at);
        Ok(at)
    }
}

impl PageSource for Chain<'_> {
    fn count(&self) -> u32 {
        self.count
    }

    fn decode(&mut self, index: u32, limits: &DecodeLimits) -> Result<RasterImage, DecodeError> {
        let at = self.locate(index)?;
        let page = Page::read(self.file, self.endian, at)?;
        decode_page(self.file, &page, limits, &mut self.scratch)
    }
}

/// Read the geometry of the picture [`decode`] would answer, from
/// directories alone.
pub(crate) fn probe(bytes: &[u8]) -> Result<(u32, u32), DecodeError> {
    let (_, measured) = Chain::open(bytes)?;
    Ok((measured.geometry.width, measured.geometry.height))
}

/// Decode the page a plain decode answers: the first the file does not call
/// a reduced copy of another.
pub(crate) fn decode(bytes: &[u8], limits: &DecodeLimits) -> Result<RasterImage, DecodeError> {
    let (mut chain, measured) = Chain::open(bytes)?;
    chain.decode(measured.primary, limits)
}

/// Validate the chain and measure its pages, decoding none of them.
pub(crate) fn pages<'a>(
    bytes: &'a [u8],
    limits: &DecodeLimits,
) -> Result<Pages<Chain<'a>>, DecodeError> {
    let (chain, measured) = Chain::open(bytes)?;
    Ok(Pages::new(
        chain,
        limits,
        measured.canvas.width,
        measured.canvas.height,
    ))
}

#[cfg(test)]
#[path = "tiff_tests.rs"]
mod tests;
