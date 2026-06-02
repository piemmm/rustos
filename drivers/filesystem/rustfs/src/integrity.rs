//! `RustFS` per-data-record integrity primitives
//! (`docs/src/filesystem/rustfs-spec.md` §6, §8).
//!
//! Stage 4 authenticates a data block only through the AEAD tag in its
//! encryption trailer. Stage 5 adds the spec's two-layer data-integrity field
//! to every file-data block, stored in a fixed trailer that follows the crypto
//! trailer:
//!
//! ```text
//! [ ciphertext content ][ crypto trailer ][ logical hash ][ physical checksum ]
//!   <-- data_capacity -->  <-- nonce+tag -->  <-- 32 B  -->   <-- 8 B  -->
//! ```
//!
//! - The **logical content hash** ([`logical_hash`]) names the block's
//!   *plaintext* content. It is the seam Stage 7 deduplication keys on
//!   (`docs/src/filesystem/rustfs-spec.md` §9) and detects a corruption that
//!   survives decryption. It is computed through `lib/crypto`'s audited
//!   SHA-256 (`AGENTS.md` §2.12 — never hand-rolled). The spec's fixed-v1
//!   constant names BLAKE3-256; `lib/crypto` exposes only the audited
//!   `RustCrypto` SHA-256, and pulling a `blake3` crate in would widen the
//!   trusted computing base with a SIMD backend that does not build cleanly on
//!   the bare-metal kernel targets (the same freestanding-SIMD problem already
//!   documented for `chacha20`/`curve25519-dalek` in `.cargo/config.toml`).
//!   SHA-256 is a 256-bit collision-resistant hash that serves the
//!   integrity-and-dedupe role identically, so `RustFS` v1 uses it and the spec
//!   records the choice (`AGENTS.md` §2.12 outranks the spec's named
//!   primitive: use the audited `lib/crypto` hash, do not hand-roll or import
//!   an unvetted one).
//! - The **physical checksum** ([`physical_checksum`]) is a fast,
//!   non-cryptographic checksum over the at-rest representation (ciphertext +
//!   crypto trailer + logical hash). It detects media / transport corruption
//!   cheaply and is verified *first* on read, so bit rot in the stored block
//!   is caught by the fast check before the AEAD runs. A checksum is not a
//!   cryptographic primitive, so §2.12 does not bar a first-party
//!   implementation; the keyed authenticity of the block still rests on the
//!   AEAD and the §8 metadata MAC.

use rustos_crypto::sha256;

/// Length, in bytes, of a data block's logical content hash (SHA-256).
pub const LOGICAL_HASH_LEN: usize = rustos_crypto::SHA256_OUTPUT_LEN;

/// Length, in bytes, of a data block's fast physical checksum.
pub const PHYS_CHECKSUM_LEN: usize = 8;

/// Bytes of per-data-block integrity trailer appended after the crypto
/// trailer: the [`LOGICAL_HASH_LEN`]-byte logical content hash followed by the
/// [`PHYS_CHECKSUM_LEN`]-byte physical checksum.
pub const DATA_INTEGRITY_TRAILER: usize = LOGICAL_HASH_LEN + PHYS_CHECKSUM_LEN;

/// Which integrity layer rejected a data block. Surfaced to the caller as a
/// single [`rustos_abi::DriverError::DeviceFault`] (the `abi-v1` error surface
/// is frozen, `AGENTS.md` §9), but kept distinct internally so a media /
/// transport corruption (the fast [`Physical`](Self::Physical) checksum) is
/// not confused with a ciphertext tamper (the [`Aead`](Self::Aead) tag) or a
/// plaintext corruption that survived decryption ([`Logical`](Self::Logical)).
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum DataFault {
    /// The fast physical checksum over the at-rest block did not match:
    /// media or transport corruption.
    Physical,
    /// The AEAD tag failed: the ciphertext or its crypto trailer was tampered.
    Aead,
    /// The decrypted plaintext did not match its stored logical hash.
    Logical,
}

/// The logical content hash of a data block's `plaintext` region: the SHA-256
/// digest, through `lib/crypto`. Identical content hashes identically (the
/// Stage 7 dedupe seam); a single flipped plaintext byte hashes differently.
#[must_use]
pub fn logical_hash(plaintext: &[u8]) -> [u8; LOGICAL_HASH_LEN] {
    sha256(plaintext)
}

/// A fast, non-cryptographic checksum over the at-rest `bytes` of a data
/// block (FNV-1a, 64-bit). Cheap to verify on every read; detects bit rot,
/// torn writes, and misdirected reads in the stored representation.
#[must_use]
pub fn physical_checksum(bytes: &[u8]) -> [u8; PHYS_CHECKSUM_LEN] {
    // FNV-1a, 64-bit (offset basis and prime per the reference parameters).
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for &byte in bytes {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash.to_le_bytes()
}

#[cfg(test)]
mod tests {
    use super::{logical_hash, physical_checksum, LOGICAL_HASH_LEN, PHYS_CHECKSUM_LEN};

    #[test]
    fn logical_hash_is_stable_and_content_sensitive() {
        let a = [0x41u8; 200];
        let mut b = a;
        b[100] ^= 0x01;
        assert_eq!(logical_hash(&a), logical_hash(&a));
        assert_ne!(logical_hash(&a), logical_hash(&b));
        assert_eq!(logical_hash(&a).len(), LOGICAL_HASH_LEN);
    }

    #[test]
    fn physical_checksum_detects_single_bit_flips() {
        let a = [0x5au8; 256];
        let mut b = a;
        b[0] ^= 0x01;
        assert_eq!(physical_checksum(&a), physical_checksum(&a));
        assert_ne!(physical_checksum(&a), physical_checksum(&b));
        assert_eq!(physical_checksum(&a).len(), PHYS_CHECKSUM_LEN);
    }

    #[test]
    fn empty_input_hashes_and_checksums_deterministically() {
        // FNV-1a offset basis with no bytes mixed in.
        assert_eq!(
            physical_checksum(&[]),
            0xcbf2_9ce4_8422_2325u64.to_le_bytes()
        );
        assert_eq!(logical_hash(&[]), logical_hash(&[]));
    }
}
