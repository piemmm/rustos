//! Classic Mac OS (HFS / HFS+) preset conversions.
//!
//! A classic-Mac file carries a four-character type code and creator code
//! (`OSType`s), a 16-bit Finder-flags field, and an optional resource fork.
//! Type and creator are stored verbatim as their four raw bytes; the Finder
//! flags are stored big-endian (their native HFS order). The resource fork is
//! *not* an attribute value — it is a named stream addressed by
//! [`RESOURCE_FORK_KEY`] and stored through the file-data pipeline.

use crate::MetadataError;

/// Length of an `OSType` (type or creator code), in bytes.
pub const OSTYPE_LEN: usize = 4;

/// The attribute key naming the resource fork's named stream.
pub const RESOURCE_FORK_KEY: &[u8] = b"mac.resourcefork";

/// Validate a four-byte `OSType` value (`mac.type` / `mac.creator`), stored
/// verbatim.
///
/// # Errors
///
/// [`MetadataError::NotRepresentable`] if the value is not exactly four bytes.
pub fn validate_ostype(value: &[u8]) -> Result<(), MetadataError> {
    if value.len() != OSTYPE_LEN {
        return Err(MetadataError::NotRepresentable);
    }
    Ok(())
}

/// Encode the 16-bit Finder flags as their canonical big-endian
/// `mac.finderflags` value.
#[must_use]
pub fn finderflags_to_value(flags: u16) -> [u8; 2] {
    flags.to_be_bytes()
}

/// Parse a `mac.finderflags` value (two big-endian bytes) back into the 16-bit
/// flags field.
///
/// # Errors
///
/// [`MetadataError::NotRepresentable`] if the value is not exactly two bytes.
pub fn finderflags_from_value(value: &[u8]) -> Result<u16, MetadataError> {
    let bytes: [u8; 2] = value
        .try_into()
        .map_err(|_| MetadataError::NotRepresentable)?;
    Ok(u16::from_be_bytes(bytes))
}
