//! The crypto-backed SYN-cookie secret.
//!
//! The pure `lib/net` listener engine never hand-rolls cryptography (the
//! charter forbids it): it asks an injected [`CookieSecret`] for a keyed MAC
//! over the connection 4-tuple and a rotating counter, and uses the low bits
//! as the cookie. This is the service-side backing of that seam — an
//! HMAC-SHA256 over a per-boot random key, drawn once from the platform RNG
//! by the `Run` glue and never persisted, so an off-path attacker cannot
//! forge a cookie and the key is gone at shutdown.
//!
//! The rotating counter (a coarse time tick) is the engine's concern and is
//! folded into the MAC input here; the key stays fixed for the life of the
//! service, so no per-connection state is kept.

use tairix_crypto::hmac_sha256_parts;
use tairix_net::tcp::listen::CookieSecret;

/// A [`CookieSecret`] backed by HMAC-SHA256 over a per-boot random key.
pub struct CryptoCookieSecret {
    /// The per-boot MAC key. Ephemeral: drawn from the platform RNG at
    /// startup, never written anywhere, and dropped at shutdown.
    key: [u8; 32],
}

impl CryptoCookieSecret {
    /// Build a secret from a 32-byte per-boot random key. The caller draws
    /// `key` from the platform CSPRNG; it must never be persisted.
    #[must_use]
    pub fn new(key: [u8; 32]) -> Self {
        Self { key }
    }
}

impl CookieSecret for CryptoCookieSecret {
    fn mac(&self, tuple: &[u8], counter: u32) -> u32 {
        // Bind the MAC to both the connection identity and the rotating
        // counter so a cookie is valid only for its 4-tuple and its window.
        let tag = hmac_sha256_parts(&self.key, &[tuple, &counter.to_le_bytes()]);
        u32::from_le_bytes([tag[0], tag[1], tag[2], tag[3]])
    }
}

#[cfg(test)]
mod tests {
    use super::CryptoCookieSecret;
    use tairix_net::tcp::listen::CookieSecret;

    #[test]
    fn mac_is_deterministic_and_tuple_bound() {
        let secret = CryptoCookieSecret::new([0x5A; 32]);
        let tuple = [1u8, 2, 3, 4, 5, 6, 7, 8];
        // Same input, same MAC (the handshake must reconstruct it).
        assert_eq!(secret.mac(&tuple, 7), secret.mac(&tuple, 7));
        // A different counter or tuple yields a different MAC (overwhelmingly).
        assert_ne!(secret.mac(&tuple, 7), secret.mac(&tuple, 8));
        let other = [9u8, 2, 3, 4, 5, 6, 7, 8];
        assert_ne!(secret.mac(&tuple, 7), secret.mac(&other, 7));
    }

    #[test]
    fn a_different_key_yields_a_different_mac() {
        let a = CryptoCookieSecret::new([0x11; 32]);
        let b = CryptoCookieSecret::new([0x22; 32]);
        let tuple = [0u8; 8];
        assert_ne!(a.mac(&tuple, 1), b.mac(&tuple, 1));
    }
}
