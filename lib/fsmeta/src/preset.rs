//! The closed preset registry of well-known foreign-metadata conversions.
//!
//! Each submodule maps one foreign filesystem's native per-file metadata to
//! and from the normalised attribute values `RustFS` stores. Every conversion is
//! exact and checked: a value or instant the foreign format cannot represent
//! fails closed with [`MetadataError`](crate::MetadataError) rather than being
//! silently truncated,
//! wrapped, or guessed. `RustFS` itself never calls these — it stores the
//! resulting bytes verbatim; the foreign-filesystem drivers and the copy tools
//! do the conversion.

pub mod acorn;
pub mod amiga;
pub mod atari;
pub mod mac;
