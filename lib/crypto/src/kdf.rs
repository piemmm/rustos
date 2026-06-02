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

#[cfg(test)]
mod tests {
    use super::{derive_key, DERIVED_KEY_LEN};

    const SECRET: [u8; 32] = [0x42; 32];

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
