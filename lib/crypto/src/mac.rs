//! Keyed message authentication: HMAC and Poly1305.
//!
//! HMAC-SHA256 (RFC 2104, FIPS 198-1) is the keyed authenticator `ARXFS`
//! seals every metadata block with (`docs/src/filesystem/arxfs-spec.md` §5,
//! §8): the tag covers a block's identity, owner, generation, expected
//! address, and payload, so a stale, misdirected, wrong-type, torn, or
//! bit-rotted metadata block fails the check and is repaired from its
//! redundant copy rather than trusted.
//!
//! HMAC-SHA512 is the SSH transport's `hmac-sha2-512` (RFC 6668).
//! HMAC-SHA1 exists for exactly one caller: OpenSSH's hashed `known_hosts`
//! index (`|1|salt|HMAC-SHA1(salt, host)`), a fixed foreign format. That
//! index is an obfuscation over a hostname — it selects which line to
//! compare a host key against and is never itself an authentication
//! decision — so SHA-1's collision weakness does not bear on it. No other
//! use of SHA-1 is offered, and the bare digest is not exposed at all.
//!
//! Poly1305 (RFC 8439 §2.5) is the one-time authenticator
//! `chacha20-poly1305@openssh.com` keys from a `ChaCha20` block. The packaged
//! [`crate::aead`] cannot serve that construction: OpenSSH authenticates a
//! length field that is encrypted under a *second*, independent key, so the
//! MAC and the cipher are composed by the protocol rather than by the AEAD.
//!
//! As with the rest of `lib/crypto`, the wrappers expose a *narrower* API
//! than the upstream crates: callers hand in a fixed-size key and receive a
//! fixed-size tag, never the upstream `Mac`/`KeyInit` traits or the
//! `GenericArray` types. Every verification goes through [`crate::ct_eq`] so
//! a caller cannot accidentally reintroduce a timing-leaking `==` over a
//! secret tag.

use hmac::{Hmac, KeyInit as HmacKeyInit, Mac};
use poly1305::Poly1305;
use sha1::Sha1;
use sha2::{Sha256, Sha512};

use crate::constant_time::ct_eq;

/// Emit the key/tag types, one-shot, multi-part, and verifying entry points
/// for one HMAC instantiation.
///
/// The five items differ between instantiations only in the upstream digest
/// and the output width, so they are written once rather than three times.
macro_rules! hmac_variant {
    (
        $digest:ty,
        $spec:literal,
        $key_len:ident,
        $tag_len:ident,
        $width:literal,
        $key_ty:ident,
        $tag_ty:ident,
        $one_shot:ident,
        $parts:ident,
        $verify:ident
    ) => {
        #[doc = concat!("Length, in bytes, of an ", $spec, " key. It matches the")]
        /// digest width, so the key is neither zero-extended past the point
        /// where extra bytes stop contributing nor hashed down by HMAC's
        /// over-long-key rule.
        pub const $key_len: usize = $width;

        #[doc = concat!("Length, in bytes, of an ", $spec, " tag.")]
        pub const $tag_len: usize = $width;

        #[doc = concat!("An ", $spec, " key as raw bytes.")]
        pub type $key_ty = [u8; $key_len];

        #[doc = concat!("An ", $spec, " tag as raw bytes.")]
        pub type $tag_ty = [u8; $tag_len];

        #[doc = concat!("Compute the ", $spec, " tag of `data` under `key`.")]
        #[must_use]
        pub fn $one_shot(key: &$key_ty, data: &[u8]) -> $tag_ty {
            $parts(key, &[data])
        }

        #[doc = concat!("Compute the ", $spec, " tag of the concatenation of")]
        /// `parts` under `key`.
        ///
        /// Equivalent to the one-shot over `parts.concat()`, but feeds each
        /// part to the underlying streaming HMAC in turn so the caller never
        /// has to allocate or stack-copy a contiguous buffer. This is what
        /// lets `tairix-rng`'s HMAC-DRBG compute `HMAC(K, V ‖ byte ‖ data)`
        /// (NIST SP 800-90A) over its working state with no allocator — the
        /// kernel allocator must not be on the entropy path — and no
        /// arbitrary fixed-size scratch bound.
        #[must_use]
        pub fn $parts(key: &$key_ty, parts: &[&[u8]]) -> $tag_ty {
            // SAFETY-INVARIANT: HMAC accepts a key of any length, so
            // construction from a fixed-size array can never return
            // `InvalidLength`. The `expect` documents an invariant that is
            // unreachable in practice.
            let mut mac = <Hmac<$digest> as HmacKeyInit>::new_from_slice(key)
                .expect("HMAC accepts any key length");
            for part in parts {
                mac.update(part);
            }
            let out = mac.finalize().into_bytes();
            let mut tag = [0u8; $tag_len];
            tag.copy_from_slice(out.as_slice());
            tag
        }

        #[doc = concat!("Verify that `tag` is the ", $spec, " of `data` under")]
        /// `key`, in constant time with respect to the tag contents.
        ///
        /// The comparison goes through [`crate::ct_eq`], so it does not leak
        /// through timing how many leading tag bytes matched.
        #[must_use]
        pub fn $verify(key: &$key_ty, data: &[u8], tag: &$tag_ty) -> bool {
            ct_eq(&$one_shot(key, data), tag)
        }
    };
}

hmac_variant!(
    Sha256,
    "HMAC-SHA256",
    HMAC_SHA256_KEY_LEN,
    HMAC_SHA256_TAG_LEN,
    32,
    HmacSha256Key,
    HmacSha256Tag,
    hmac_sha256,
    hmac_sha256_parts,
    hmac_sha256_verify
);

hmac_variant!(
    Sha512,
    "HMAC-SHA512",
    HMAC_SHA512_KEY_LEN,
    HMAC_SHA512_TAG_LEN,
    64,
    HmacSha512Key,
    HmacSha512Tag,
    hmac_sha512,
    hmac_sha512_parts,
    hmac_sha512_verify
);

hmac_variant!(
    Sha1,
    "HMAC-SHA1",
    HMAC_SHA1_KEY_LEN,
    HMAC_SHA1_TAG_LEN,
    20,
    HmacSha1Key,
    HmacSha1Tag,
    hmac_sha1,
    hmac_sha1_parts,
    hmac_sha1_verify
);

/// Length, in bytes, of a Poly1305 one-time key.
pub const POLY1305_KEY_LEN: usize = 32;

/// Length, in bytes, of a Poly1305 tag.
pub const POLY1305_TAG_LEN: usize = 16;

/// A Poly1305 one-time key as raw bytes.
///
/// "One-time" is not advice. Poly1305 is a Wegman-Carter authenticator:
/// authenticating two different messages under the same key lets an
/// adversary solve for the key and forge arbitrarily. Every caller must
/// derive a fresh key per message — `chacha20-poly1305@openssh.com` takes
/// one `ChaCha20` block under the packet's own sequence number.
pub type Poly1305Key = [u8; POLY1305_KEY_LEN];

/// A Poly1305 tag as raw bytes.
pub type Poly1305Tag = [u8; POLY1305_TAG_LEN];

/// Compute the Poly1305 tag of `data` under the one-time `key`
/// (RFC 8439 §2.5.1).
///
/// There is no multi-part form. SSH authenticates a packet's encrypted
/// length field and ciphertext, which are contiguous in the buffer the
/// packet was framed into or received from the socket, so a second entry
/// point would buy a caller nothing and would hand-roll block buffering
/// around an authenticator that already does it.
#[must_use]
pub fn poly1305(key: &Poly1305Key, data: &[u8]) -> Poly1305Tag {
    Poly1305::new(key.into()).compute_unpadded(data).into()
}

/// Verify that `tag` is the Poly1305 of `data` under the one-time `key`, in
/// constant time with respect to the tag contents.
#[must_use]
pub fn poly1305_verify(key: &Poly1305Key, data: &[u8], tag: &Poly1305Tag) -> bool {
    ct_eq(&poly1305(key, data), tag)
}

#[cfg(test)]
#[path = "mac_tests.rs"]
mod tests;
