//! Keyed message authentication (HMAC-SHA256).
//!
//! The one MAC exposed by RustOS is HMAC-SHA256 (RFC 2104, FIPS 198-1). It is
//! the keyed authenticator `RustFS` seals every metadata block with
//! (`docs/src/filesystem/rustfs-spec.md` §5, §8): the tag covers a block's
//! identity, owner, generation, expected address, and payload, so a stale,
//! misdirected, wrong-type, torn, or bit-rotted metadata block fails the
//! check and is repaired from its redundant copy rather than trusted.
//!
//! As with the rest of `lib/crypto`, the wrapper exposes a *narrower* API
//! than the upstream crate: callers hand in a fixed-size key and receive a
//! fixed-size tag, never the upstream `Mac` / `KeyInit` traits or the
//! `GenericArray` types. Verification goes through [`crate::ct_eq`] so a
//! caller cannot accidentally reintroduce a timing-leaking `==` over a secret
//! tag.

use hmac::{Hmac, Mac};
use sha2::Sha256;

use crate::constant_time::ct_eq;

/// Length, in bytes, of an HMAC-SHA256 key. A 256-bit key matches the
/// underlying SHA-256 block-truncation point, so no key shortening occurs.
pub const MAC_KEY_LEN: usize = 32;

/// Length, in bytes, of an HMAC-SHA256 tag.
pub const MAC_TAG_LEN: usize = 32;

/// A 256-bit HMAC-SHA256 key as raw bytes.
pub type MacKey = [u8; MAC_KEY_LEN];

/// An HMAC-SHA256 tag as raw bytes.
pub type MacTag = [u8; MAC_TAG_LEN];

type HmacSha256 = Hmac<Sha256>;

/// Compute the HMAC-SHA256 tag of `data` under `key`.
///
/// Wraps [`hmac::Hmac`] so callers never see the upstream `Mac` / `KeyInit`
/// traits; this keeps the surface area auditable.
#[must_use]
pub fn hmac_sha256(key: &MacKey, data: &[u8]) -> MacTag {
    hmac_sha256_parts(key, &[data])
}

/// Compute the HMAC-SHA256 tag of the concatenation of `parts` under `key`.
///
/// Equivalent to [`hmac_sha256`] over `parts.concat()`, but feeds each part
/// to the underlying streaming HMAC in turn so the caller never has to
/// allocate or stack-copy a contiguous buffer. This is what lets the
/// `rustos-rng` HMAC-DRBG compute `HMAC(K, V ‖ byte ‖ data)` (NIST SP
/// 800-90A) over its working state without an allocator (the kernel allocator must not be on the entropy path) or an arbitrary
/// fixed-size scratch bound.
///
/// Wraps [`hmac::Hmac`] so callers never see the upstream `Mac` / `KeyInit`
/// traits; this keeps the surface area auditable.
#[must_use]
pub fn hmac_sha256_parts(key: &MacKey, parts: &[&[u8]]) -> MacTag {
    // SAFETY-INVARIANT: HMAC accepts a key of any length, so constructing it
    // from a fixed 32-byte array can never return `InvalidLength`. The
    // `expect` documents that invariant; it is unreachable in practice.
    let mut mac = HmacSha256::new_from_slice(key).expect("HMAC accepts any key length");
    for part in parts {
        mac.update(part);
    }
    let out = mac.finalize().into_bytes();
    let mut tag = [0u8; MAC_TAG_LEN];
    tag.copy_from_slice(out.as_slice());
    tag
}

/// Verify that `tag` is the HMAC-SHA256 of `data` under `key`, in constant
/// time with respect to the tag contents.
///
/// Returns `true` iff `tag` matches the freshly computed tag. The comparison
/// goes through [`crate::ct_eq`], so it does not leak through timing how many
/// leading tag bytes matched.
#[must_use]
pub fn hmac_sha256_verify(key: &MacKey, data: &[u8], tag: &MacTag) -> bool {
    let expected = hmac_sha256(key, data);
    ct_eq(&expected, tag)
}

#[cfg(test)]
mod tests {
    use super::{hmac_sha256, hmac_sha256_verify, MacKey, MAC_TAG_LEN};

    use crate::hash::sha256;

    fn key(byte: u8) -> MacKey {
        [byte; 32]
    }

    /// The textbook HMAC construction (RFC 2104) for a 32-byte key and an
    /// empty message, computed independently from [`hmac_sha256`] using only
    /// the SHA-256 wrapper. Because a 32-byte key is shorter than SHA-256's
    /// 64-byte block it is zero-padded, never hashed, so this needs no
    /// allocator: every buffer is a fixed-size array.
    fn reference_empty(key: &MacKey) -> super::MacTag {
        const BLOCK: usize = 64;
        let mut k_pad = [0u8; BLOCK];
        k_pad[..32].copy_from_slice(key);
        let mut ipad = [0x36u8; BLOCK];
        let mut opad = [0x5cu8; BLOCK];
        for i in 0..BLOCK {
            ipad[i] ^= k_pad[i];
            opad[i] ^= k_pad[i];
        }
        // Inner hash is over `ipad` followed by the empty message.
        let inner = sha256(&ipad);
        let mut outer_input = [0u8; BLOCK + super::MAC_TAG_LEN];
        outer_input[..BLOCK].copy_from_slice(&opad);
        outer_input[BLOCK..].copy_from_slice(&inner);
        sha256(&outer_input)
    }

    #[test]
    fn tag_is_deterministic_and_full_width() {
        let k = key(0x42);
        let a = hmac_sha256(&k, b"the quick brown fox");
        let b = hmac_sha256(&k, b"the quick brown fox");
        assert_eq!(a, b);
        assert_eq!(a.len(), MAC_TAG_LEN);
    }

    #[test]
    fn parts_equal_the_concatenated_single_shot() {
        // `hmac_sha256_parts` must equal `hmac_sha256` over the joined parts,
        // for any split, so the DRBG's `V ‖ byte ‖ data` form is faithful.
        let k = key(0x91);
        let whole = [0x00u8, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09];
        let expected = super::hmac_sha256(&k, &whole);
        assert_eq!(super::hmac_sha256_parts(&k, &[&whole]), expected);
        assert_eq!(
            super::hmac_sha256_parts(&k, &[&whole[..3], &whole[3..]]),
            expected
        );
        assert_eq!(
            super::hmac_sha256_parts(&k, &[&whole[..1], &whole[1..4], &[], &whole[4..]]),
            expected
        );
    }

    #[test]
    fn different_data_yields_a_different_tag() {
        let k = key(0x42);
        assert_ne!(hmac_sha256(&k, b"alpha"), hmac_sha256(&k, b"beta"));
    }

    #[test]
    fn different_key_yields_a_different_tag() {
        assert_ne!(
            hmac_sha256(&key(0x01), b"same message"),
            hmac_sha256(&key(0x02), b"same message")
        );
    }

    #[test]
    fn verify_accepts_a_genuine_tag_and_rejects_tampering() {
        let k = key(0x7E);
        let data = b"metadata block payload";
        let tag = hmac_sha256(&k, data);
        assert!(hmac_sha256_verify(&k, data, &tag));

        // A flipped tag bit, a flipped message bit, and the wrong key are all
        // rejected.
        let mut bad_tag = tag;
        bad_tag[0] ^= 0x01;
        assert!(!hmac_sha256_verify(&k, data, &bad_tag));
        assert!(!hmac_sha256_verify(&k, b"metadata block payloae", &tag));
        assert!(!hmac_sha256_verify(&key(0x7F), data, &tag));
    }

    #[test]
    fn matches_the_textbook_construction() {
        // Cross-check the wrapper against an independent RFC 2104 computation
        // built from the SHA-256 wrapper, so a future change that silently
        // swapped the algorithm would break this test.
        for byte in [0x00u8, 0x0b, 0x42, 0xff] {
            let k = key(byte);
            assert_eq!(hmac_sha256(&k, b""), reference_empty(&k));
        }
    }
}
