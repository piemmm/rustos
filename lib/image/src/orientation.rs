//! Which way up a stored raster is, and where that puts each of its pixels.
//!
//! TIFF's `Orientation` tag and the EXIF attribute a JPEG carries in its
//! `APP1` segment are the same tag number with the same eight values, so the
//! map from a stored position to the position it presents at is one
//! definition both formats read rather than two that can drift apart.
//!
//! Applying an orientation is a permutation of positions: nothing is done to
//! a pixel, so a decoder places each one as it produces it and never holds a
//! second copy of the picture.

use crate::{be_u16, be_u32, le_u16, le_u32};

/// The `Orientation` tag, in both formats that carry one.
const TAG_ORIENTATION: u16 = 274;

/// The marker segment identifier that introduces an EXIF attribute block.
const EXIF_IDENTIFIER: &[u8] = b"Exif\0\0";

/// The TIFF header's arbitrary version number, which is how a reader
/// confirms it resolved the byte order the right way round.
const TIFF_MAGIC: u16 = 42;

/// One IFD entry: tag, type, count, and the value or its offset.
const ENTRY_BYTES: usize = 12;

/// Field type 3: a 16-bit unsigned integer, which is what EXIF specifies
/// the orientation is written as.
const TYPE_SHORT: u16 = 3;

/// Field type 4: a 32-bit unsigned integer, which some writers use instead.
const TYPE_LONG: u16 = 4;

/// Which edges of the picture the stored raster's first row and first column
/// lie along, as tag 274 numbers the eight possibilities.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub(crate) struct Orientation(u32);

impl Orientation {
    /// The stored raster is already the picture: first row at the top, first
    /// column on the left.
    pub(crate) const IDENTITY: Self = Self(1);

    /// The orientation tag 274 spells as `code`, or `None` for a value
    /// outside the eight it defines.
    pub(crate) const fn from_tag(code: u32) -> Option<Self> {
        if matches!(code, 1..=8) {
            Some(Self(code))
        } else {
            None
        }
    }

    /// Whether the picture's axes are the stored raster's two swapped.
    pub(crate) const fn transposes(self) -> bool {
        self.0 >= 5
    }

    /// The picture a `width`×`height` stored raster presents as.
    pub(crate) const fn picture_size(self, width: u32, height: u32) -> (u32, u32) {
        if self.transposes() {
            (height, width)
        } else {
            (width, height)
        }
    }

    /// Where the stored pixel at `(x, y)` lands, given the **picture's**
    /// `width` and `height` — the stored raster's two swapped whenever this
    /// orientation transposes.
    ///
    /// Saturating, so a coordinate outside the picture yields an edge
    /// position rather than wrapping or panicking; callers bound their own
    /// loops by the geometry they validated, so that never arises.
    pub(crate) const fn place(self, x: u32, y: u32, width: u32, height: u32) -> (u32, u32) {
        let last_x = width.saturating_sub(1);
        let last_y = height.saturating_sub(1);
        match self.0 {
            2 => (last_x.saturating_sub(x), y),
            3 => (last_x.saturating_sub(x), last_y.saturating_sub(y)),
            4 => (x, last_y.saturating_sub(y)),
            5 => (y, x),
            6 => (last_x.saturating_sub(y), x),
            7 => (last_x.saturating_sub(y), last_y.saturating_sub(x)),
            8 => (y, last_y.saturating_sub(x)),
            _ => (x, y),
        }
    }
}

/// The orientation an EXIF attribute block states, if it states a usable one.
///
/// `payload` is a marker segment's contents, including the `Exif\0\0`
/// identifier that introduces one.
///
/// Metadata is advisory, so this answers `None` for a block that is absent,
/// truncated, malformed, or carries a value outside the eight the tag
/// defines, and the picture is then used as stored. That is deliberately
/// unlike TIFF, where the same tag sits in the directory describing the
/// pixels being decoded and a bad value means the file cannot be read at
/// all: here a camera's malformed metadata must not cost a reader the
/// photograph.
pub(crate) fn from_exif(payload: &[u8]) -> Option<Orientation> {
    let header = payload.strip_prefix(EXIF_IDENTIFIER)?;
    let big = match header.get(..2)? {
        b"MM" => true,
        b"II" => false,
        _ => return None,
    };
    let read_u16 = |at: usize| {
        if big {
            be_u16(header, at)
        } else {
            le_u16(header, at)
        }
    };
    let read_u32 = |at: usize| {
        if big {
            be_u32(header, at)
        } else {
            le_u32(header, at)
        }
    };
    if read_u16(2)? != TIFF_MAGIC {
        return None;
    }

    // Offsets are relative to the start of the TIFF header, which is where
    // `header` begins.
    let directory = usize::try_from(read_u32(4)?).ok()?;
    let count = usize::from(read_u16(directory)?);
    let entries = directory.checked_add(2)?;
    // An entry count is a declaration, not a guarantee: walk only the
    // entries the segment actually holds.
    let available = header.len().saturating_sub(entries) / ENTRY_BYTES;

    (0..count.min(available)).find_map(|index| {
        let at = entries + index * ENTRY_BYTES;
        if read_u16(at)? != TAG_ORIENTATION || read_u32(at + 4)? != 1 {
            return None;
        }
        // A single SHORT or LONG is left-justified in the value field, so it
        // reads at the field's own start under the file's byte order.
        let value = match read_u16(at + 2)? {
            TYPE_SHORT => u32::from(read_u16(at + 8)?),
            TYPE_LONG => read_u32(at + 8)?,
            _ => return None,
        };
        Orientation::from_tag(value)
    })
}

#[cfg(test)]
#[path = "orientation_tests.rs"]
mod tests;
