//! Key derivation.
//!
//! RustOS derives subkeys with HMAC-SHA256 used as a pseudo-random function,
//! the single-block case of HKDF-Expand (RFC 5869): a 256-bit secret keys the
//! MAC and a caller-chosen, domain-separating `context` string is the message.
//! The 256-bit output is itself a 256-bit key, so no expansion past one block
//! is ever required and the construction stays a thin wrapper over the audited
//! [`crate::mac`] primitive rather than a hand-rolled KDF (`AGENTS.md` §2.12).
//!
//! This is what `RustFS` uses to grow its per-volume key hierarchy
//! (`docs/src/filesystem/rustfs-spec.md` §7): one master key derives the
//! metadata-authentication, filename, and content keys, each under a distinct
//! `context`, so a derived key never collides with another use of the master.
//!
//! For **password** material RustOS uses PBKDF2-HMAC-SHA256 (RFC 8018 §5.2):
//! a deliberately slow, salted derivation that makes offline guessing of a
//! stolen `/System/Security/Users` record expensive ([`pbkdf2_sha256`]). It
//! is a standard *construction* over the same audited HMAC primitive — the
//! same shape as `rustos-rng`'s HMAC-DRBG — never a hand-rolled primitive
//! (`AGENTS.md` §2.12). Verification goes through [`crate::ct_eq`]
//! ([`pbkdf2_sha256_verify`]) so a stored hash comparison cannot leak through
//! timing (`AGENTS.md` §19.1).

use core::num::NonZeroU32;

use hmac::{Hmac, Mac};
use sha2::Sha256;

use crate::constant_time::ct_eq;
use crate::mac::{hmac_sha256, MacKey};

/// Length, in bytes, of a derived key. Matches both [`crate::mac::MAC_KEY_LEN`]
/// and [`crate::aead::AEAD_KEY_LEN`], so a derived key drops straight into
/// either primitive without truncation or expansion.
pub const DERIVED_KEY_LEN: usize = 32;

/// A 256-bit derived key as raw bytes.
pub type DerivedKey = [u8; DERIVED_KEY_LEN];

/// Derive a 256-bit subkey from a 256-bit `secret` and a domain-separating
/// `context`.
///
/// Computes `HMAC-SHA256(secret, context)` — the single-block HKDF-Expand
/// case (RFC 5869) — through the audited [`crate::mac`] wrapper. Distinct
/// `context` values yield independent keys from the same `secret`, so callers
/// must give each derived key its own stable, unique context label.
///
/// The output is uniformly random under the PRF assumption on HMAC-SHA256 and
/// reveals nothing about `secret`.
#[must_use]
pub fn derive_key(secret: &MacKey, context: &[u8]) -> DerivedKey {
    hmac_sha256(secret, context)
}

/// Length, in bytes, of a PBKDF2-derived password hash: one SHA-256 block,
/// so the derivation is the single-block PBKDF2 case (`T_1` only).
pub const PASSWORD_HASH_LEN: usize = 32;

/// A PBKDF2-HMAC-SHA256 password hash as raw bytes.
pub type PasswordHash = [u8; PASSWORD_HASH_LEN];

/// Derive a [`PasswordHash`] from `password` and `salt` with `iterations`
/// rounds of PBKDF2-HMAC-SHA256 (RFC 8018 §5.2).
///
/// The output length equals the HMAC output, so exactly one PBKDF2 block is
/// computed: `U_1 = HMAC(password, salt ‖ INT(1))`, `U_i = HMAC(password,
/// U_{i-1})`, and the hash is the XOR of all `U_i`. `iterations` is
/// [`NonZeroU32`] because zero rounds is not a defined PBKDF2 input; the
/// type, not a runtime check, rules it out. Callers choose the cost; the
/// users-database format pins its own accepted range.
#[must_use]
pub fn pbkdf2_sha256(password: &[u8], salt: &[u8], iterations: NonZeroU32) -> PasswordHash {
    // SAFETY-INVARIANT: HMAC accepts a key of any length, so construction
    // from an arbitrary password slice can never return `InvalidLength`.
    let prf = Hmac::<Sha256>::new_from_slice(password).expect("HMAC accepts any key length");

    let mut mac = prf.clone();
    mac.update(salt);
    mac.update(&1u32.to_be_bytes());
    let mut block: PasswordHash = mac.finalize().into_bytes().into();

    let mut out = block;
    for _ in 1..iterations.get() {
        let mut mac = prf.clone();
        mac.update(&block);
        block = mac.finalize().into_bytes().into();
        for (acc, byte) in out.iter_mut().zip(block.iter()) {
            *acc ^= byte;
        }
    }
    out
}

/// Verify that `expected` is the PBKDF2-HMAC-SHA256 hash of `password` under
/// `salt` and `iterations`, in constant time with respect to the hash
/// contents.
///
/// The comparison goes through [`crate::ct_eq`], so it does not leak through
/// timing how many leading hash bytes matched (`AGENTS.md` §19.1).
#[must_use]
pub fn pbkdf2_sha256_verify(
    password: &[u8],
    salt: &[u8],
    iterations: NonZeroU32,
    expected: &PasswordHash,
) -> bool {
    ct_eq(&pbkdf2_sha256(password, salt, iterations), expected)
}

#[cfg(test)]
mod tests {
    use super::{derive_key, pbkdf2_sha256, pbkdf2_sha256_verify, PasswordHash, DERIVED_KEY_LEN};

    use core::num::NonZeroU32;

    const SECRET: [u8; 32] = [0x42; 32];

    fn rounds(n: u32) -> NonZeroU32 {
        NonZeroU32::new(n).expect("non-zero")
    }

    fn unhex(text: &str) -> PasswordHash {
        let mut out = [0u8; 32];
        for (i, slot) in out.iter_mut().enumerate() {
            *slot = u8::from_str_radix(&text[2 * i..2 * i + 2], 16).expect("hex");
        }
        out
    }

    #[test]
    fn pbkdf2_matches_the_published_sha256_vectors() {
        // The de-facto standard PBKDF2-HMAC-SHA256 vectors (the SHA-256
        // re-computation of the RFC 6070 inputs, as published in the
        // RustCrypto and OpenSSL test suites).
        assert_eq!(
            pbkdf2_sha256(b"password", b"salt", rounds(1)),
            unhex("120fb6cffcf8b32c43e7225256c4f837a86548c92ccc35480805987cb70be17b"),
        );
        assert_eq!(
            pbkdf2_sha256(b"password", b"salt", rounds(2)),
            unhex("ae4d0c95af6b46d32d0adff928f06dd02a303f8ef3c251dfd6e2d85a95474c43"),
        );
        assert_eq!(
            pbkdf2_sha256(b"password", b"salt", rounds(4096)),
            unhex("c5e478d59288c841aa530db6845c4c8d962893a001ce4e11a4963873aa98134a"),
        );
    }

    #[test]
    fn pbkdf2_inputs_are_all_load_bearing() {
        let base = pbkdf2_sha256(b"password", b"salt", rounds(2));
        assert_ne!(pbkdf2_sha256(b"passwore", b"salt", rounds(2)), base);
        assert_ne!(pbkdf2_sha256(b"password", b"selt", rounds(2)), base);
        assert_ne!(pbkdf2_sha256(b"password", b"salt", rounds(3)), base);
    }

    #[test]
    fn pbkdf2_verify_accepts_genuine_and_rejects_tampered_hashes() {
        let hash = pbkdf2_sha256(b"correct horse", b"battery staple", rounds(16));
        assert!(pbkdf2_sha256_verify(
            b"correct horse",
            b"battery staple",
            rounds(16),
            &hash
        ));
        assert!(!pbkdf2_sha256_verify(
            b"wrong horse",
            b"battery staple",
            rounds(16),
            &hash
        ));
        let mut bad = hash;
        bad[0] ^= 0x01;
        assert!(!pbkdf2_sha256_verify(
            b"correct horse",
            b"battery staple",
            rounds(16),
            &bad
        ));
    }

    #[test]
    fn derivation_is_deterministic_and_full_width() {
        let a = derive_key(&SECRET, b"rustfs/content");
        let b = derive_key(&SECRET, b"rustfs/content");
        assert_eq!(a, b);
        assert_eq!(a.len(), DERIVED_KEY_LEN);
    }

    #[test]
    fn distinct_contexts_yield_independent_keys() {
        let content = derive_key(&SECRET, b"rustfs/content");
        let filename = derive_key(&SECRET, b"rustfs/filename");
        let meta = derive_key(&SECRET, b"rustfs/meta-mac");
        assert_ne!(content, filename);
        assert_ne!(content, meta);
        assert_ne!(filename, meta);
    }

    #[test]
    fn distinct_secrets_yield_independent_keys() {
        let other = [0x43; 32];
        assert_ne!(
            derive_key(&SECRET, b"rustfs/content"),
            derive_key(&other, b"rustfs/content")
        );
    }
}
