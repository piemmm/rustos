//! Shared extended-file-metadata model for RustOS filesystems.
//!
//! `RustFS` gives every file a general-purpose, namespaced `key → value`
//! extended-attribute store, and uses it to preserve foreign-filesystem
//! per-file metadata (Acorn/RISC OS, Amiga, Atari, classic Mac) that has no
//! POSIX equivalent and would otherwise be destroyed by a copy. This crate is
//! the **one definition** of that model, shared by `RustFS`, the
//! foreign-filesystem drivers, and the copy/move/archive tooling, so a key
//! written by one is read identically by another and no conversion logic is
//! duplicated.
//!
//! It provides three things and nothing else:
//!
//! * the **namespaced-key grammar** ([`key`]): a closed, curated set of
//!   namespaces, each carrying a fixed access class, and a fail-closed
//!   validator for a key's bytes;
//! * the **bounded attribute store** ([`attr`]): [`AttrEntry`] / [`AttrSet`]
//!   with a self-identifying, length-prefixed on-disk encoding a filesystem
//!   driver writes verbatim into one copy-on-write metadata block, and fixed
//!   *security* bounds ([`KEY_MAX`], [`VALUE_MAX`], [`ATTRS_PER_INODE`],
//!   [`TOTAL_ATTR_BYTES`]) that are validation limits, never growable
//!   capacities;
//! * the **foreign-metadata preset registry** ([`preset`]): exact, checked
//!   conversions between each foreign filesystem's native per-file fields and
//!   normalised attribute values, with `Time64` instants converted through the
//!   checked path so a timestamp the foreign format cannot represent fails
//!   closed rather than being silently truncated.
//!
//! Every fallible operation returns [`MetadataError`]; nothing panics. Values
//! are opaque byte strings to this crate — it never interprets what a value
//! *means*; interpretation belongs to the preset registry and the converting
//! tool.

#![no_std]
#![forbid(unsafe_code)]
#![deny(missing_docs)]

extern crate alloc;

pub mod attr;
pub mod key;
pub mod preset;

mod calendar;

#[cfg(test)]
mod tests;

pub use attr::{AttrEntry, AttrFlags, AttrSet};
pub use key::{AttrKey, Namespace, NamespaceAccess};

use rustos_abi::driver::DriverError;

/// Longest attribute key, in bytes. A fixed *security* validation bound, not a
/// tunable capacity: it caps untrusted stored data and never grows.
pub const KEY_MAX: usize = 255;

/// Largest attribute value stored inline in the attribute set, in bytes.
///
/// A fixed security validation bound. It is chosen so that a full attribute
/// set — every entry's key and value plus the self-identifying framing —
/// serialises into a single copy-on-write metadata block on a 4 `KiB`-block
/// volume. A value larger than this is not an extended attribute; it is a
/// *named stream* (a fork), stored through the file-data pipeline.
pub const VALUE_MAX: usize = 3072;

/// Largest number of attributes one inode may carry. A fixed security bound.
pub const ATTRS_PER_INODE: usize = 32;

/// Largest summed size, in bytes, of all key and value bytes on one inode.
///
/// A fixed security bound on the logical attribute payload. The driver
/// additionally fails closed if the *encoded* set (this payload plus framing)
/// does not fit the volume's metadata block; this bound guarantees it always
/// does on a 4 `KiB`-block volume.
pub const TOTAL_ATTR_BYTES: usize = 3072;

/// Why an extended-metadata operation failed. Every variant is a fail-closed
/// rejection; the crate never panics and never silently drops or truncates.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum MetadataError {
    /// A key is longer than [`KEY_MAX`] bytes.
    KeyTooLong,
    /// A key is empty, is not valid UTF-8, has no `namespace.rest` split, has
    /// an empty `rest`, or contains a forbidden byte (`/` or NUL).
    MalformedKey,
    /// A key's namespace is not one of the closed, curated set.
    UnknownNamespace,
    /// A value is longer than [`VALUE_MAX`] bytes.
    ValueTooLong,
    /// Adding an attribute would exceed [`ATTRS_PER_INODE`].
    TooManyAttributes,
    /// Adding or growing an attribute would exceed [`TOTAL_ATTR_BYTES`].
    TotalBytesExceeded,
    /// A serialised attribute set is not a well-formed, in-bounds encoding
    /// (bad magic/version, truncated, out-of-range length, or a duplicate
    /// key). Treated as device corruption by a filesystem driver.
    Corrupt,
    /// A value cannot be represented in the target foreign format (e.g. an
    /// Acorn filetype above 12 bits, a Mac `OSType` that is not four bytes).
    /// The `MetadataNotRepresentable` outcome of the cross-filesystem
    /// preservation contract.
    NotRepresentable,
    /// A `Time64` instant cannot be represented exactly in the target foreign
    /// timestamp format, or a foreign timestamp cannot be represented in
    /// `Time64`. Never silently truncated or wrapped.
    TimestampOutOfRange,
}

impl From<MetadataError> for DriverError {
    /// Map a metadata failure onto the stable driver ABI error a filesystem
    /// driver returns. Every variant fails closed; none maps onto a
    /// success-adjacent code.
    fn from(err: MetadataError) -> Self {
        match err {
            MetadataError::KeyTooLong | MetadataError::ValueTooLong => {
                DriverError::LengthOutOfRange
            }
            MetadataError::MalformedKey
            | MetadataError::UnknownNamespace
            | MetadataError::NotRepresentable
            | MetadataError::TimestampOutOfRange => DriverError::OutOfRange,
            MetadataError::TooManyAttributes | MetadataError::TotalBytesExceeded => {
                DriverError::NoSpace
            }
            MetadataError::Corrupt => DriverError::DeviceFault,
        }
    }
}
