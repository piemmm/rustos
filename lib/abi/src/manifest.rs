//! Manifest header embedded in every loadable `rxe` binary.
//!
//! A [`ManifestHeader`] is the fixed-size prefix of the signed manifest
//! section in an `rxe` executable. The body that follows the header is a
//! length-prefixed list of [`crate::CapabilityId`] values requested by the
//! binary; both halves are covered by [`ManifestHeader::signature`].

use crate::syscall::SYSCALL_TABLE_HASH_LEN;
use crate::Errno;

/// Magic number identifying an `abi-v1` manifest (`"RXM1"` little-endian).
pub const MANIFEST_MAGIC: u32 = u32::from_le_bytes(*b"RXM1");

/// Maximum number of capability identifiers a single manifest may request.
///
/// Bounded so that a malformed or hostile manifest cannot force unbounded
/// parsing work. The value comfortably exceeds the number of capabilities
/// defined in `abi-v1`.
pub const MANIFEST_MAX_CAPABILITIES: u16 = 64;

/// Fixed-size prefix of a signed `rxe` manifest.
///
/// Field order is part of the frozen ABI; reserved fields must be zero.
/// Signature coverage is the entire manifest bytes excluding the
/// `signature` field itself.
#[repr(C)]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct ManifestHeader {
    /// Must equal [`MANIFEST_MAGIC`].
    pub magic: u32,
    /// ABI version this manifest targets; rejected if it does not match
    /// [`crate::ABI_VERSION_CURRENT`].
    pub abi_version: u32,
    /// Implementation-defined flag bits; unknown bits must be zero.
    pub flags: u32,
    /// Number of capability IDs in the body. Capped at
    /// [`MANIFEST_MAX_CAPABILITIES`].
    pub capability_count: u16,
    /// Reserved; must be zero in `abi-v1`.
    pub reserved0: u16,
    /// SHA-256 of the kernel syscall table this binary was linked against.
    pub syscall_table_hash: [u8; SYSCALL_TABLE_HASH_LEN],
    /// Ed25519 public key of the signer.
    pub signer_pubkey: [u8; 32],
    /// Ed25519 signature over the rest of the manifest.
    pub signature: [u8; 64],
}

impl ManifestHeader {
    /// Encoded size of a [`ManifestHeader`] on the wire.
    pub const WIRE_LEN: usize = 4 + 4 + 4 + 2 + 2 + SYSCALL_TABLE_HASH_LEN + 32 + 64;

    /// Encode `self` into its little-endian wire representation.
    #[must_use]
    pub fn to_le_bytes(&self) -> [u8; Self::WIRE_LEN] {
        let mut out = [0u8; Self::WIRE_LEN];
        out[0..4].copy_from_slice(&self.magic.to_le_bytes());
        out[4..8].copy_from_slice(&self.abi_version.to_le_bytes());
        out[8..12].copy_from_slice(&self.flags.to_le_bytes());
        out[12..14].copy_from_slice(&self.capability_count.to_le_bytes());
        out[14..16].copy_from_slice(&self.reserved0.to_le_bytes());
        let mut cursor = 16;
        out[cursor..cursor + SYSCALL_TABLE_HASH_LEN].copy_from_slice(&self.syscall_table_hash);
        cursor += SYSCALL_TABLE_HASH_LEN;
        out[cursor..cursor + 32].copy_from_slice(&self.signer_pubkey);
        cursor += 32;
        out[cursor..cursor + 64].copy_from_slice(&self.signature);
        out
    }

    /// Decode `bytes` into a [`ManifestHeader`].
    ///
    /// Returns:
    /// * [`Errno::BufferTooSmall`] if `bytes.len() < WIRE_LEN`.
    /// * [`Errno::BadMagic`] if the magic word does not match, or if
    ///   `reserved0` is non-zero.
    /// * [`Errno::AbiVersionUnsupported`] if `abi_version` is not
    ///   [`crate::ABI_VERSION_CURRENT`].
    /// * [`Errno::LengthOutOfRange`] if `capability_count` exceeds
    ///   [`MANIFEST_MAX_CAPABILITIES`].
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, Errno> {
        if bytes.len() < Self::WIRE_LEN {
            return Err(Errno::BufferTooSmall);
        }
        let magic = u32_le(bytes, 0);
        if magic != MANIFEST_MAGIC {
            return Err(Errno::BadMagic);
        }
        let abi_version = u32_le(bytes, 4);
        if abi_version != crate::ABI_VERSION_CURRENT {
            return Err(Errno::AbiVersionUnsupported);
        }
        let flags = u32_le(bytes, 8);
        let capability_count = u16_le(bytes, 12);
        if capability_count > MANIFEST_MAX_CAPABILITIES {
            return Err(Errno::LengthOutOfRange);
        }
        let reserved0 = u16_le(bytes, 14);
        if reserved0 != 0 {
            return Err(Errno::BadMagic);
        }
        let mut cursor = 16;
        let mut syscall_table_hash = [0u8; SYSCALL_TABLE_HASH_LEN];
        syscall_table_hash.copy_from_slice(&bytes[cursor..cursor + SYSCALL_TABLE_HASH_LEN]);
        cursor += SYSCALL_TABLE_HASH_LEN;
        let mut signer_pubkey = [0u8; 32];
        signer_pubkey.copy_from_slice(&bytes[cursor..cursor + 32]);
        cursor += 32;
        let mut signature = [0u8; 64];
        signature.copy_from_slice(&bytes[cursor..cursor + 64]);
        Ok(Self {
            magic,
            abi_version,
            flags,
            capability_count,
            reserved0,
            syscall_table_hash,
            signer_pubkey,
            signature,
        })
    }

    /// Byte range, within an encoded manifest, that the signature covers.
    ///
    /// The signature itself is excluded so that signing is a fixed operation
    /// over a deterministic byte stream.
    #[must_use]
    pub const fn signed_range() -> core::ops::Range<usize> {
        0..(Self::WIRE_LEN - 64)
    }
}

#[inline]
fn u16_le(bytes: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes([bytes[offset], bytes[offset + 1]])
}

#[inline]
fn u32_le(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes([
        bytes[offset],
        bytes[offset + 1],
        bytes[offset + 2],
        bytes[offset + 3],
    ])
}

#[cfg(test)]
mod tests {
    use super::{ManifestHeader, MANIFEST_MAGIC, MANIFEST_MAX_CAPABILITIES};
    use crate::syscall::SYSCALL_TABLE_HASH_LEN;
    use crate::{Errno, ABI_VERSION_CURRENT};

    fn sample() -> ManifestHeader {
        ManifestHeader {
            magic: MANIFEST_MAGIC,
            abi_version: ABI_VERSION_CURRENT,
            flags: 0,
            capability_count: 3,
            reserved0: 0,
            syscall_table_hash: [0xAB; SYSCALL_TABLE_HASH_LEN],
            signer_pubkey: [0xCD; 32],
            signature: [0xEF; 64],
        }
    }

    #[test]
    fn wire_size_matches_struct() {
        assert_eq!(
            ManifestHeader::WIRE_LEN,
            core::mem::size_of::<ManifestHeader>()
        );
    }

    #[test]
    fn round_trip() {
        let h = sample();
        let bytes = h.to_le_bytes();
        assert_eq!(ManifestHeader::from_bytes(&bytes), Ok(h));
    }

    #[test]
    fn rejects_short() {
        assert_eq!(
            ManifestHeader::from_bytes(&[0u8; 32]),
            Err(Errno::BufferTooSmall)
        );
    }

    #[test]
    fn rejects_bad_magic() {
        let mut bytes = sample().to_le_bytes();
        bytes[0] ^= 0xFF;
        assert_eq!(ManifestHeader::from_bytes(&bytes), Err(Errno::BadMagic));
    }

    #[test]
    fn rejects_bad_version() {
        let mut h = sample();
        h.abi_version = 99;
        let bytes = h.to_le_bytes();
        assert_eq!(
            ManifestHeader::from_bytes(&bytes),
            Err(Errno::AbiVersionUnsupported)
        );
    }

    #[test]
    fn rejects_excess_capabilities() {
        let mut h = sample();
        h.capability_count = MANIFEST_MAX_CAPABILITIES + 1;
        let bytes = h.to_le_bytes();
        assert_eq!(
            ManifestHeader::from_bytes(&bytes),
            Err(Errno::LengthOutOfRange)
        );
    }

    #[test]
    fn rejects_nonzero_reserved() {
        let mut h = sample();
        h.reserved0 = 1;
        let bytes = h.to_le_bytes();
        assert_eq!(ManifestHeader::from_bytes(&bytes), Err(Errno::BadMagic));
    }

    #[test]
    fn signed_range_excludes_signature() {
        let range = ManifestHeader::signed_range();
        assert_eq!(range.end, ManifestHeader::WIRE_LEN - 64);
        assert_eq!(range.start, 0);
    }
}
