//! Hashing primitives.
//!
//! The only hash exposed in `abi-v1` is SHA-256 (used by the syscall-table
//! fingerprint embedded in every manifest). Streaming hashing is not exposed
//! here because no caller in the workspace needs it; if and when one does,
//! it should be added with its own audit note rather than smuggled in via
//! a `Default`/`Update`/`Finalize` trait.

use sha2::{Digest, Sha256};

/// Length, in bytes, of a SHA-256 digest.
pub const SHA256_OUTPUT_LEN: usize = 32;

/// SHA-256 digest as raw bytes.
pub type Sha256Digest = [u8; SHA256_OUTPUT_LEN];

/// Compute the SHA-256 digest of `data`.
///
/// Wraps [`sha2::Sha256`] so callers never see the upstream `Digest` /
/// `Update` traits; this keeps the surface area auditable.
#[must_use]
pub fn sha256(data: &[u8]) -> Sha256Digest {
    let mut hasher = Sha256::new();
    hasher.update(data);
    let out = hasher.finalize();
    let mut digest = [0u8; SHA256_OUTPUT_LEN];
    digest.copy_from_slice(out.as_slice());
    digest
}

#[cfg(test)]
mod tests {
    use super::{sha256, SHA256_OUTPUT_LEN};

    #[test]
    fn empty_string_matches_nist_vector() {
        // FIPS 180-4 §A.1: SHA-256 of the empty message.
        let expected: [u8; SHA256_OUTPUT_LEN] = [
            0xe3, 0xb0, 0xc4, 0x42, 0x98, 0xfc, 0x1c, 0x14, 0x9a, 0xfb, 0xf4, 0xc8, 0x99, 0x6f,
            0xb9, 0x24, 0x27, 0xae, 0x41, 0xe4, 0x64, 0x9b, 0x93, 0x4c, 0xa4, 0x95, 0x99, 0x1b,
            0x78, 0x52, 0xb8, 0x55,
        ];
        assert_eq!(sha256(b""), expected);
    }

    #[test]
    fn abc_matches_nist_vector() {
        // FIPS 180-4 §A.1: SHA-256 of "abc".
        let expected: [u8; SHA256_OUTPUT_LEN] = [
            0xba, 0x78, 0x16, 0xbf, 0x8f, 0x01, 0xcf, 0xea, 0x41, 0x41, 0x40, 0xde, 0x5d, 0xae,
            0x22, 0x23, 0xb0, 0x03, 0x61, 0xa3, 0x96, 0x17, 0x7a, 0x9c, 0xb4, 0x10, 0xff, 0x61,
            0xf2, 0x00, 0x15, 0xad,
        ];
        assert_eq!(sha256(b"abc"), expected);
    }
}
