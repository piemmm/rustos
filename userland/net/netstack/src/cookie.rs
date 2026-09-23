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

use tairix_crypto::{hmac_sha256_parts, HMAC_SHA256_KEY_LEN};
use tairix_net::tcp::listen::CookieSecret;
use tairix_util::secret::Wiped;

/// A [`CookieSecret`] backed by HMAC-SHA256 over a per-boot random key.
pub struct CryptoCookieSecret {
    /// The per-boot MAC key: never written anywhere, and wiped when the
    /// secret is dropped.
    key: Wiped<HMAC_SHA256_KEY_LEN>,
}

impl CryptoCookieSecret {
    /// A secret keyed by `fill` — the platform CSPRNG in the service — or
    /// the source's refusal.
    ///
    /// No secret is built from a key the source did not finish writing, so a
    /// failed draw can never leave cookies forgeable under an all-zero key.
    ///
    /// # Errors
    ///
    /// Whatever `fill` refused with.
    pub fn keyed_by<E>(fill: impl FnOnce(&mut [u8]) -> Result<(), E>) -> Result<Self, E> {
        let mut key = Wiped::new();
        fill(&mut key[..])?;
        Ok(Self { key })
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

    fn keyed(byte: u8) -> CryptoCookieSecret {
        CryptoCookieSecret::keyed_by(|key| {
            key.fill(byte);
            Ok::<(), ()>(())
        })
        .expect("the source filled the key")
    }

    #[test]
    fn mac_is_deterministic_and_tuple_bound() {
        let secret = keyed(0x5A);
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
        let a = keyed(0x11);
        let b = keyed(0x22);
        let tuple = [0u8; 8];
        assert_ne!(a.mac(&tuple, 1), b.mac(&tuple, 1));
    }

    #[test]
    fn a_refused_draw_builds_no_secret() {
        // A refused draw leaves no secret at all, never one keyed with zeros
        // under which anyone could forge a cookie.
        let refused = CryptoCookieSecret::keyed_by(|key| {
            key[..4].fill(0xA5);
            Err("entropy not ready")
        });
        assert!(matches!(refused, Err("entropy not ready")));
    }
}
