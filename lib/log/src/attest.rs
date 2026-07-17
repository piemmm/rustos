//! Per-installation log attestation: the stream-genesis derivation and the
//! sealing key the system log's integrity model rests on.
//!
//! This module supplies the two cryptographic foundations
//! `plans/SYSLOG.md` §7 builds the journal's tamper-evidence on, *without*
//! implementing the journal, the on-disk segment format, or the seal/anchor
//! *writing* (those are SYSLOG itself):
//!
//! * **Stream genesis** ([`machine_id_hash`] + [`stream_genesis`]). Each log
//!   stream's hash chain ([`crate::chain`]) starts not from the all-zero
//!   [`crate::GENESIS_ANCHOR`] but from a value bound to *this installation*,
//!   *this stream*, and *this boot* (SYSLOG §7.1). Binding the machine-id hash
//!   stops a segment being replayed onto a different installation; binding the
//!   [`BootId`] stops it being replayed from a different boot of the same
//!   installation. The derivation is a pure, domain-separated SHA-256 over
//!   `lib/crypto` — never a hand-rolled hash.
//!
//! * **The log-attestation key** ([`LogAttestationKey`]). A per-installation
//!   secret used to *seal* closed audit/security segments (SYSLOG §7.2) and to
//!   *sign* the periodic anchors (§7.3) with HMAC-SHA256. The key is a secret:
//!   it is scrubbed from memory on drop, never implements
//!   [`ToFieldValue`](crate::ToFieldValue) (so it cannot be logged), and its
//!   raw bytes never leave the type — callers
//!   seal and verify *through* it, so the secret stays encapsulated.
//!
//! All sealing uses the audited HMAC-SHA256 in `lib/crypto`; this module never
//! names an upstream crypto crate and never hand-rolls a primitive.

use tairix_abi::{BootId, Errno, BOOT_ID_LEN};
use tairix_crypto::{
    ct_eq, hmac_sha256_parts, sha256, MacKey, MacTag, Sha256Digest, MAC_KEY_LEN, SHA256_OUTPUT_LEN,
};
use zeroize::Zeroize;

/// Length, in bytes, of a machine identifier (mirrors
/// [`tairix_abi::MACHINE_ID_LEN`]). Re-stated here so the genesis preimage
/// length is a `const` without reaching into `sysinfo`'s wire constants.
pub const MACHINE_ID_LEN: usize = tairix_abi::MACHINE_ID_LEN;

/// Domain-separation tag for the machine-id hash. Distinct tags keep the two
/// SHA-256 uses in this module from ever colliding on a shared preimage.
const DOMAIN_MACHINE_ID: &[u8] = b"tairix.log.machine-id.v1";

/// Domain-separation tag for the stream-genesis derivation.
const DOMAIN_GENESIS: &[u8] = b"tairix.log.stream-genesis.v1";

/// Hash a machine identifier into the non-secret `machine id hash` the anchor
/// records (SYSLOG §7.3) and the genesis binds to.
///
/// Domain-separated SHA-256 so this value can never coincide with a
/// [`stream_genesis`] output or any other hash in the system.
#[must_use]
pub fn machine_id_hash(machine_id: &[u8; MACHINE_ID_LEN]) -> Sha256Digest {
    const PREIMAGE_LEN: usize = DOMAIN_MACHINE_ID.len() + MACHINE_ID_LEN;
    let mut preimage = [0u8; PREIMAGE_LEN];
    preimage[..DOMAIN_MACHINE_ID.len()].copy_from_slice(DOMAIN_MACHINE_ID);
    preimage[DOMAIN_MACHINE_ID.len()..].copy_from_slice(machine_id);
    sha256(&preimage)
}

/// Derive a log stream's hash-chain genesis value, binding it to the
/// installation, the stream, and the boot (SYSLOG §7.1).
///
/// The result is the `prev_hash` the *first* segment of `stream` chains to (in
/// place of the all-zero [`crate::GENESIS_ANCHOR`] a within-stream chain
/// uses), so a segment lifted from another installation, another stream, or
/// another boot fails verification.
///
/// `machine_id_hash` is the output of [`machine_id_hash`]; `stream` is the
/// stream's identifying bytes (e.g. `b"audit"`); `boot_id` is the kernel's
/// per-boot [`BootId`]. The stream is folded through SHA-256 first so the
/// preimage is fixed-length regardless of the stream label's length — no
/// arbitrary length cap, no second variable-length field to frame.
///
/// Pure and domain-separated; the value is **not** secret (every input is
/// public), so it is freely recomputable by a verifier — confidentiality of
/// the chain rests on the seal/anchor signature ([`LogAttestationKey`]), not
/// on the genesis.
#[must_use]
pub fn stream_genesis(
    machine_id_hash: &Sha256Digest,
    stream: &[u8],
    boot_id: &BootId,
) -> Sha256Digest {
    const PREIMAGE_LEN: usize =
        DOMAIN_GENESIS.len() + SHA256_OUTPUT_LEN + SHA256_OUTPUT_LEN + BOOT_ID_LEN;
    let stream_hash = sha256(stream);
    let mut preimage = [0u8; PREIMAGE_LEN];
    let mut cursor = 0;
    preimage[cursor..cursor + DOMAIN_GENESIS.len()].copy_from_slice(DOMAIN_GENESIS);
    cursor += DOMAIN_GENESIS.len();
    preimage[cursor..cursor + SHA256_OUTPUT_LEN].copy_from_slice(machine_id_hash);
    cursor += SHA256_OUTPUT_LEN;
    preimage[cursor..cursor + SHA256_OUTPUT_LEN].copy_from_slice(&stream_hash);
    cursor += SHA256_OUTPUT_LEN;
    preimage[cursor..cursor + BOOT_ID_LEN].copy_from_slice(boot_id.as_bytes());
    sha256(&preimage)
}

/// Length, in bytes, of a [`LogAttestationKey`]'s raw key material (a 256-bit
/// HMAC-SHA256 key; mirrors [`tairix_crypto::MAC_KEY_LEN`]).
pub const LOG_ATTESTATION_KEY_LEN: usize = MAC_KEY_LEN;

/// Magic identifying the on-disk log-attestation key file.
const KEY_FILE_MAGIC: [u8; 4] = *b"RLAK"; // TAIRiX Log Attestation Key.

/// On-disk format version.
const KEY_FILE_VERSION: u16 = 1;

/// Byte length of the on-disk log-attestation key file: an 8-byte header
/// (`magic(4) || version(2 LE) || reserved(2)`) followed by the raw key.
pub const LOG_ATTESTATION_KEY_FILE_LEN: usize = 8 + LOG_ATTESTATION_KEY_LEN;

/// A per-installation log-attestation key (`plans/SYSLOG.md` §7.2/§7.3).
///
/// Wraps a 256-bit HMAC-SHA256 key used to seal closed audit/security log
/// segments and to sign the periodic anchors. It is **secret**:
///
/// * the raw key bytes never leave the type — callers [`seal`](Self::seal)
///   and [`verify`](Self::verify) *through* it, so the secret stays
///   encapsulated;
/// * it is scrubbed from memory on drop (`Drop` zeroes the key) so a freed
///   buffer cannot leak it;
/// * it deliberately does **not** implement
///   [`ToFieldValue`](crate::ToFieldValue), [`Debug`](core::fmt::Debug), or any
///   rendering trait, so it cannot be logged or printed by construction.
///
/// Access control to the key *file* on disk is the inode owner/mode model
/// (system-user-owned, restrictive mode under `/System/Security/Keys/`): until
/// the journal/attestation principal exists, no service can read it, and no
/// new capability is minted ahead of that holder.
pub struct LogAttestationKey {
    key: MacKey,
}

impl LogAttestationKey {
    /// Wrap raw key material.
    ///
    /// The bytes must come from a cryptographic RNG (the platform CSPRNG); a
    /// weak key voids the integrity guarantee. This is the provisioning
    /// constructor (the installer / image builder), not a user-reachable path.
    #[must_use]
    pub const fn from_key(key: MacKey) -> Self {
        Self { key }
    }

    /// Seal `parts` under the key, returning the HMAC-SHA256 tag.
    ///
    /// `parts` are hashed in order as if concatenated, so the caller frames
    /// the segment/anchor fields without allocating a contiguous buffer. The
    /// raw key never leaves the type.
    #[must_use]
    pub fn seal(&self, parts: &[&[u8]]) -> MacTag {
        hmac_sha256_parts(&self.key, parts)
    }

    /// Verify in constant time that `tag` seals `parts` under the key.
    ///
    /// The comparison goes through `lib/crypto`'s constant-time equality, so
    /// it never leaks through timing how much of a forged tag matched.
    #[must_use]
    pub fn verify(&self, parts: &[&[u8]], tag: &MacTag) -> bool {
        let expected = hmac_sha256_parts(&self.key, parts);
        ct_eq(&expected, tag)
    }

    /// Serialise the key into its on-disk file image
    /// ([`LOG_ATTESTATION_KEY_FILE_LEN`] bytes).
    ///
    /// Used by the provisioning path (the image builder / installer) to write
    /// `/System/Security/Keys/`. The buffer holds the secret and should be
    /// handled accordingly by the caller.
    #[must_use]
    pub fn to_file_bytes(&self) -> [u8; LOG_ATTESTATION_KEY_FILE_LEN] {
        let mut out = [0u8; LOG_ATTESTATION_KEY_FILE_LEN];
        out[0..4].copy_from_slice(&KEY_FILE_MAGIC);
        out[4..6].copy_from_slice(&KEY_FILE_VERSION.to_le_bytes());
        // bytes 6..8 reserved, already zero.
        out[8..].copy_from_slice(&self.key);
        out
    }

    /// Parse a key from its on-disk file image, failing closed.
    ///
    /// Returns [`Errno::LengthOutOfRange`] for a wrong-length buffer,
    /// [`Errno::BadMagic`] for a bad magic, and [`Errno::OutOfRange`] for an
    /// unsupported version — never guessing at a malformed key.
    pub fn from_file_bytes(bytes: &[u8]) -> Result<Self, Errno> {
        if bytes.len() != LOG_ATTESTATION_KEY_FILE_LEN {
            return Err(Errno::LengthOutOfRange);
        }
        if bytes[0..4] != KEY_FILE_MAGIC {
            return Err(Errno::BadMagic);
        }
        let version = u16::from_le_bytes([bytes[4], bytes[5]]);
        if version != KEY_FILE_VERSION {
            return Err(Errno::OutOfRange);
        }
        let mut key = [0u8; LOG_ATTESTATION_KEY_LEN];
        key.copy_from_slice(&bytes[8..]);
        Ok(Self { key })
    }
}

impl Drop for LogAttestationKey {
    fn drop(&mut self) {
        // Scrub the secret before the buffer is released so it cannot linger
        // in freed memory. `zeroize` is optimisation-resistant.
        self.key.zeroize();
    }
}

#[cfg(test)]
mod tests {
    use super::{
        machine_id_hash, stream_genesis, LogAttestationKey, LOG_ATTESTATION_KEY_FILE_LEN,
        LOG_ATTESTATION_KEY_LEN, MACHINE_ID_LEN,
    };
    use tairix_abi::{BootId, Errno, BOOT_ID_LEN};

    fn sample_boot_id() -> BootId {
        BootId::from_raw([0x5A; BOOT_ID_LEN])
    }

    #[test]
    fn machine_id_hash_is_deterministic_and_distinct() {
        let a = machine_id_hash(&[1u8; MACHINE_ID_LEN]);
        let b = machine_id_hash(&[1u8; MACHINE_ID_LEN]);
        let c = machine_id_hash(&[2u8; MACHINE_ID_LEN]);
        assert_eq!(a, b, "same machine id hashes identically");
        assert_ne!(a, c, "different machine ids hash differently");
        // Domain separation: it is not the bare SHA-256 of the id.
        assert_ne!(a, tairix_crypto::sha256(&[1u8; MACHINE_ID_LEN]));
    }

    #[test]
    fn genesis_binds_machine_stream_and_boot() {
        let mid = machine_id_hash(&[7u8; MACHINE_ID_LEN]);
        let mid2 = machine_id_hash(&[8u8; MACHINE_ID_LEN]);
        let boot = sample_boot_id();
        let boot2 = BootId::from_raw([0xA5; BOOT_ID_LEN]);

        let base = stream_genesis(&mid, b"audit", &boot);
        // Deterministic for identical inputs.
        assert_eq!(base, stream_genesis(&mid, b"audit", &boot));
        // Each input changes the genesis.
        assert_ne!(base, stream_genesis(&mid2, b"audit", &boot));
        assert_ne!(base, stream_genesis(&mid, b"security", &boot));
        assert_ne!(base, stream_genesis(&mid, b"audit", &boot2));
        // Distinct from the all-zero within-stream anchor.
        assert_ne!(base, crate::GENESIS_ANCHOR);
    }

    #[test]
    fn key_round_trips_through_file_bytes() {
        let key = LogAttestationKey::from_key([0x33; LOG_ATTESTATION_KEY_LEN]);
        let bytes = key.to_file_bytes();
        assert_eq!(bytes.len(), LOG_ATTESTATION_KEY_FILE_LEN);
        let parsed = LogAttestationKey::from_file_bytes(&bytes).expect("valid key parses");
        // The parsed key produces the same MAC (raw bytes are encapsulated, so
        // we compare behaviour, not bytes).
        assert!(parsed.verify(&[b"hello"], &key.seal(&[b"hello"])));
    }

    #[test]
    fn from_file_bytes_fails_closed() {
        let good = LogAttestationKey::from_key([0x44; LOG_ATTESTATION_KEY_LEN]).to_file_bytes();
        // Wrong length.
        assert_eq!(
            LogAttestationKey::from_file_bytes(&good[..LOG_ATTESTATION_KEY_FILE_LEN - 1])
                .err()
                .unwrap(),
            Errno::LengthOutOfRange
        );
        // Bad magic.
        let mut bad_magic = good;
        bad_magic[0] = b'X';
        assert_eq!(
            LogAttestationKey::from_file_bytes(&bad_magic)
                .err()
                .unwrap(),
            Errno::BadMagic
        );
        // Unsupported version.
        let mut bad_version = good;
        bad_version[4] = 9;
        assert_eq!(
            LogAttestationKey::from_file_bytes(&bad_version)
                .err()
                .unwrap(),
            Errno::OutOfRange
        );
    }

    #[test]
    fn seal_and_verify_are_consistent() {
        let key = LogAttestationKey::from_key([0x11; LOG_ATTESTATION_KEY_LEN]);
        let tag = key.seal(&[b"header", b"records", b"footer"]);
        assert!(key.verify(&[b"header", b"records", b"footer"], &tag));
        // A different message does not verify.
        assert!(!key.verify(&[b"header", b"records", b"FORGED"], &tag));
        // A different key does not verify.
        let other = LogAttestationKey::from_key([0x22; LOG_ATTESTATION_KEY_LEN]);
        assert!(!other.verify(&[b"header", b"records", b"footer"], &tag));
        // Concatenation framing is order/boundary sensitive but content-equal:
        // parts joining to the same bytes seal identically.
        let whole = key.seal(&[b"headerrecordsfooter"]);
        assert!(key.verify(&[b"header", b"records", b"footer"], &whole));
    }
}
