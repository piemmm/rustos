//! `AmigaDOS` / FFS preset conversions.
//!
//! An Amiga file carries an 8-bit protection mask (conventionally written
//! `hsparwed`) and an optional file comment. This module represents the
//! protection mask as its canonical eight-character string — each position
//! shows its letter when the bit is set and `-` when clear — which round-trips
//! the raw byte losslessly, and bounds the comment to the `AmigaDOS` limit.

use crate::MetadataError;

/// Longest `AmigaDOS` file comment, in bytes.
pub const MAX_COMMENT_LEN: usize = 79;

/// The protection letters, most-significant bit (`h`, bit 7) first.
const LETTERS: [u8; 8] = *b"hsparwed";

/// Render an 8-bit protection mask as its canonical `hsparwed` string, with
/// `-` for each clear bit.
#[must_use]
pub fn protection_to_value(bits: u8) -> [u8; 8] {
    let mut out = [b'-'; 8];
    for (i, slot) in out.iter_mut().enumerate() {
        let bit = 7 - u32::try_from(i).unwrap_or(0);
        if bits & (1 << bit) != 0 {
            *slot = LETTERS[i];
        }
    }
    out
}

/// Parse a canonical `hsparwed` protection string back into its 8-bit mask.
///
/// Each of the eight positions must be either its expected letter or `-`.
///
/// # Errors
///
/// [`MetadataError::NotRepresentable`] if the value is not exactly eight
/// characters or a position is neither its letter nor `-`.
pub fn protection_from_value(value: &[u8]) -> Result<u8, MetadataError> {
    if value.len() != 8 {
        return Err(MetadataError::NotRepresentable);
    }
    let mut bits = 0u8;
    for (i, &byte) in value.iter().enumerate() {
        let bit = 7 - u32::try_from(i).unwrap_or(0);
        if byte == LETTERS[i] {
            bits |= 1 << bit;
        } else if byte != b'-' {
            return Err(MetadataError::NotRepresentable);
        }
    }
    Ok(bits)
}

/// Validate a file comment against the `AmigaDOS` length limit. The comment is
/// stored verbatim as the `amiga.comment` value.
///
/// # Errors
///
/// [`MetadataError::ValueTooLong`] if `comment` exceeds [`MAX_COMMENT_LEN`].
pub fn validate_comment(comment: &[u8]) -> Result<(), MetadataError> {
    if comment.len() > MAX_COMMENT_LEN {
        return Err(MetadataError::ValueTooLong);
    }
    Ok(())
}
