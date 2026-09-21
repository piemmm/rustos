//! Key agreement (X25519).
//!
//! The one key-agreement primitive exposed by TAIRiX is X25519 (RFC 7748),
//! the Montgomery-form Diffie–Hellman on Curve25519. It is the agreement half
//! of an authenticated session handshake: two peers exchange ephemeral public
//! keys, each computes the same shared secret, and a recorded transcript plus
//! a signature ([`crate::sign`]) says *who* the peer was.
//!
//! As with the rest of `lib/crypto`, the wrapper is narrower than the upstream
//! crate: callers hand in fixed-size byte arrays and never see the upstream
//! types. Secret scalars come **from the caller** — this module sources no
//! randomness, because only the caller knows which generator its threat model
//! requires (`lib/rng`'s CSPRNG for a live session, a fixed vector in a test).
//!
//! # The agreement output is not a key
//!
//! [`X25519SharedSecret`] is raw Diffie–Hellman output: a *curve point
//! coordinate*, not a uniformly-distributed bit string. Condense it before any
//! byte of it reaches a cipher — [`crate::mac::hmac_sha256`] over the
//! agreement output as the *message*, keyed by the handshake transcript as the
//! salt, then one [`crate::kdf::derive_key`] per use under a domain-separating
//! context. The non-uniform value is that PRF's input, never its key. Using it
//! directly as a cipher key is a defect.
//!
//! # Non-contributory results are refused
//!
//! A peer that offers a small-order u-coordinate forces the agreement to the
//! identity point, so both sides derive a shared secret the peer chose rather
//! than one it contributed to. [`X25519SecretKey::agree`] detects that and
//! fails closed; a caller therefore never has to know the small-order list.

use core::fmt;

use x25519_dalek::{PublicKey, SharedSecret, StaticSecret};

/// Length, in bytes, of an X25519 secret scalar.
pub const X25519_SECRET_LEN: usize = 32;

/// Length, in bytes, of an X25519 public key (a Montgomery u-coordinate).
pub const X25519_PUBLIC_KEY_LEN: usize = 32;

/// Length, in bytes, of an X25519 shared secret. Matches
/// [`crate::mac::HMAC_SHA256_KEY_LEN`], so the agreement output keys the KDF directly.
pub const X25519_SHARED_SECRET_LEN: usize = 32;

/// Failure of a key agreement.
///
/// Deliberately opaque and single-variant: the only way agreement fails is a
/// non-contributory peer key, and a caller must treat it as a refused session
/// rather than branch on a cause. Detail belongs in the audit log.
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub struct KeyAgreementError(());

impl fmt::Display for KeyAgreementError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("x25519 key agreement refused")
    }
}

/// A 32-byte X25519 public key on the wire.
///
/// Every 32-byte string is a syntactically valid u-coordinate, so there is
/// nothing to reject at construction; a hostile *choice* of coordinate is
/// caught by [`X25519SecretKey::agree`] instead.
#[repr(transparent)]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct X25519PublicKey(pub [u8; X25519_PUBLIC_KEY_LEN]);

impl X25519PublicKey {
    /// Wrap a raw 32-byte u-coordinate.
    #[must_use]
    pub const fn from_bytes(bytes: [u8; X25519_PUBLIC_KEY_LEN]) -> Self {
        Self(bytes)
    }

    /// Borrow the raw bytes.
    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; X25519_PUBLIC_KEY_LEN] {
        &self.0
    }
}

/// An X25519 secret scalar, wiped on drop.
///
/// The scalar is stored as the caller supplied it; clamping (RFC 7748 §5) is
/// applied by the upstream crate at each *use* — deriving the public key and
/// running the agreement — so arbitrary caller bytes can never reach the curve
/// unclamped and land off the prime-order subgroup.
pub struct X25519SecretKey {
    inner: StaticSecret,
}

impl X25519SecretKey {
    /// Wrap 32 caller-supplied secret bytes.
    ///
    /// The caller draws them from a CSPRNG and owns wiping its own copy; this
    /// type wipes the clamped scalar it holds.
    #[must_use]
    pub fn from_bytes(bytes: [u8; X25519_SECRET_LEN]) -> Self {
        Self {
            inner: StaticSecret::from(bytes),
        }
    }

    /// The public key to send to the peer.
    #[must_use]
    pub fn public_key(&self) -> X25519PublicKey {
        X25519PublicKey(PublicKey::from(&self.inner).to_bytes())
    }

    /// Agree a shared secret with `peer`.
    ///
    /// # Errors
    ///
    /// Returns [`KeyAgreementError`] when `peer` is a small-order point, which
    /// would drive the agreement to the identity and let the peer dictate the
    /// result instead of contributing to it.
    pub fn agree(&self, peer: &X25519PublicKey) -> Result<X25519SharedSecret, KeyAgreementError> {
        let shared = self.inner.diffie_hellman(&PublicKey::from(peer.0));
        if shared.was_contributory() {
            Ok(X25519SharedSecret { inner: shared })
        } else {
            Err(KeyAgreementError(()))
        }
    }
}

impl fmt::Debug for X25519SecretKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("X25519SecretKey").finish_non_exhaustive()
    }
}

/// The raw agreement output, wiped on drop.
///
/// Feed it to [`crate::kdf::derive_key`]; see the module docs for why it is
/// not itself a key.
pub struct X25519SharedSecret {
    inner: SharedSecret,
}

impl X25519SharedSecret {
    /// Borrow the raw agreement output, for use as KDF key material only.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8; X25519_SHARED_SECRET_LEN] {
        self.inner.as_bytes()
    }
}

impl fmt::Debug for X25519SharedSecret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("X25519SharedSecret").finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::{KeyAgreementError, X25519PublicKey, X25519SecretKey, X25519_SHARED_SECRET_LEN};

    /// RFC 7748 §6.1: Alice's secret scalar.
    const ALICE_SECRET: [u8; 32] = [
        0x77, 0x07, 0x6d, 0x0a, 0x73, 0x18, 0xa5, 0x7d, 0x3c, 0x16, 0xc1, 0x72, 0x51, 0xb2, 0x66,
        0x45, 0xdf, 0x4c, 0x2f, 0x87, 0xeb, 0xc0, 0x99, 0x2a, 0xb1, 0x77, 0xfb, 0xa5, 0x1d, 0xb9,
        0x2c, 0x2a,
    ];
    /// RFC 7748 §6.1: Alice's public key.
    const ALICE_PUBLIC: [u8; 32] = [
        0x85, 0x20, 0xf0, 0x09, 0x89, 0x30, 0xa7, 0x54, 0x74, 0x8b, 0x7d, 0xdc, 0xb4, 0x3e, 0xf7,
        0x5a, 0x0d, 0xbf, 0x3a, 0x0d, 0x26, 0x38, 0x1a, 0xf4, 0xeb, 0xa4, 0xa9, 0x8e, 0xaa, 0x9b,
        0x4e, 0x6a,
    ];
    /// RFC 7748 §6.1: Bob's secret scalar.
    const BOB_SECRET: [u8; 32] = [
        0x5d, 0xab, 0x08, 0x7e, 0x62, 0x4a, 0x8a, 0x4b, 0x79, 0xe1, 0x7f, 0x8b, 0x83, 0x80, 0x0e,
        0xe6, 0x6f, 0x3b, 0xb1, 0x29, 0x26, 0x18, 0xb6, 0xfd, 0x1c, 0x2f, 0x8b, 0x27, 0xff, 0x88,
        0xe0, 0xeb,
    ];
    /// RFC 7748 §6.1: Bob's public key.
    const BOB_PUBLIC: [u8; 32] = [
        0xde, 0x9e, 0xdb, 0x7d, 0x7b, 0x7d, 0xc1, 0xb4, 0xd3, 0x5b, 0x61, 0xc2, 0xec, 0xe4, 0x35,
        0x37, 0x3f, 0x83, 0x43, 0xc8, 0x5b, 0x78, 0x67, 0x4d, 0xad, 0xfc, 0x7e, 0x14, 0x6f, 0x88,
        0x2b, 0x4f,
    ];
    /// RFC 7748 §6.1: the agreed secret `K`.
    const SHARED: [u8; 32] = [
        0x4a, 0x5d, 0x9d, 0x5b, 0xa4, 0xce, 0x2d, 0xe1, 0x72, 0x8e, 0x3b, 0xf4, 0x80, 0x35, 0x0f,
        0x25, 0xe0, 0x7e, 0x21, 0xc9, 0x47, 0xd1, 0x9e, 0x33, 0x76, 0xf0, 0x9b, 0x3c, 0x1e, 0x16,
        0x17, 0x42,
    ];

    /// RFC 7748 §5.2, vector 1: the raw X25519 function's input scalar,
    /// input u-coordinate, and output.
    const V1_SCALAR: [u8; 32] = [
        0xa5, 0x46, 0xe3, 0x6b, 0xf0, 0x52, 0x7c, 0x9d, 0x3b, 0x16, 0x15, 0x4b, 0x82, 0x46, 0x5e,
        0xdd, 0x62, 0x14, 0x4c, 0x0a, 0xc1, 0xfc, 0x5a, 0x18, 0x50, 0x6a, 0x22, 0x44, 0xba, 0x44,
        0x9a, 0xc4,
    ];
    const V1_U: [u8; 32] = [
        0xe6, 0xdb, 0x68, 0x67, 0x58, 0x30, 0x30, 0xdb, 0x35, 0x94, 0xc1, 0xa4, 0x24, 0xb1, 0x5f,
        0x7c, 0x72, 0x66, 0x24, 0xec, 0x26, 0xb3, 0x35, 0x3b, 0x10, 0xa9, 0x03, 0xa6, 0xd0, 0xab,
        0x1c, 0x4c,
    ];
    const V1_OUT: [u8; 32] = [
        0xc3, 0xda, 0x55, 0x37, 0x9d, 0xe9, 0xc6, 0x90, 0x8e, 0x94, 0xea, 0x4d, 0xf2, 0x8d, 0x08,
        0x4f, 0x32, 0xec, 0xcf, 0x03, 0x49, 0x1c, 0x71, 0xf7, 0x54, 0xb4, 0x07, 0x55, 0x77, 0xa2,
        0x85, 0x52,
    ];

    #[test]
    fn rfc7748_public_keys_match() {
        assert_eq!(
            X25519SecretKey::from_bytes(ALICE_SECRET).public_key().0,
            ALICE_PUBLIC
        );
        assert_eq!(
            X25519SecretKey::from_bytes(BOB_SECRET).public_key().0,
            BOB_PUBLIC
        );
    }

    #[test]
    fn rfc7748_agreement_matches_both_ways() {
        let alice = X25519SecretKey::from_bytes(ALICE_SECRET);
        let bob = X25519SecretKey::from_bytes(BOB_SECRET);
        let a = alice
            .agree(&X25519PublicKey::from_bytes(BOB_PUBLIC))
            .expect("contributory");
        let b = bob
            .agree(&X25519PublicKey::from_bytes(ALICE_PUBLIC))
            .expect("contributory");
        assert_eq!(a.as_bytes(), &SHARED);
        assert_eq!(b.as_bytes(), &SHARED);
    }

    #[test]
    fn rfc7748_raw_function_vector() {
        let secret = X25519SecretKey::from_bytes(V1_SCALAR);
        let out = secret
            .agree(&X25519PublicKey::from_bytes(V1_U))
            .expect("contributory");
        assert_eq!(out.as_bytes(), &V1_OUT);
    }

    #[test]
    fn small_order_peer_keys_are_refused() {
        // The order-1, order-2, and order-4/8 u-coordinates a hostile peer
        // would offer to force the agreement to the identity.
        let identity = [0u8; 32];
        let mut order_four = [0u8; 32];
        order_four[0] = 1;
        let order_eight_a = [
            0xe0, 0xeb, 0x7a, 0x7c, 0x3b, 0x41, 0xb8, 0xae, 0x16, 0x56, 0xe3, 0xfa, 0xf1, 0x9f,
            0xc4, 0x6a, 0xda, 0x09, 0x8d, 0xeb, 0x9c, 0x32, 0xb1, 0xfd, 0x86, 0x62, 0x05, 0x16,
            0x5f, 0x49, 0xb8, 0x00,
        ];
        let order_eight_b = [
            0x5f, 0x9c, 0x95, 0xbc, 0xa3, 0x50, 0x8c, 0x24, 0xb1, 0xd0, 0xb1, 0x55, 0x9c, 0x83,
            0xef, 0x5b, 0x04, 0x44, 0x5c, 0xc4, 0x58, 0x1c, 0x8e, 0x86, 0xd8, 0x22, 0x4e, 0xdd,
            0xd0, 0x9f, 0x11, 0x57,
        ];
        let secret = X25519SecretKey::from_bytes(ALICE_SECRET);
        for peer in [identity, order_four, order_eight_a, order_eight_b] {
            assert_eq!(
                secret
                    .agree(&X25519PublicKey::from_bytes(peer))
                    .err()
                    .map(|_| ()),
                Some(()),
                "small-order peer key must be refused"
            );
        }
    }

    #[test]
    fn distinct_peers_agree_distinct_secrets() {
        let mine = X25519SecretKey::from_bytes([7u8; 32]);
        let first = X25519SecretKey::from_bytes([9u8; 32]).public_key();
        let second = X25519SecretKey::from_bytes([11u8; 32]).public_key();
        let a = mine.agree(&first).expect("contributory");
        let b = mine.agree(&second).expect("contributory");
        assert_ne!(a.as_bytes(), b.as_bytes());
        assert_eq!(a.as_bytes().len(), X25519_SHARED_SECRET_LEN);
    }

    #[test]
    fn public_key_round_trips_its_bytes() {
        let key = X25519PublicKey::from_bytes(BOB_PUBLIC);
        assert_eq!(key.as_bytes(), &BOB_PUBLIC);
    }

    /// Fixed-size `core::fmt::Write` sink so the Display/Debug checks stay
    /// `no_std`-clean.
    struct FmtSink<const N: usize> {
        data: [u8; N],
        len: usize,
    }

    impl<const N: usize> core::fmt::Write for FmtSink<N> {
        fn write_str(&mut self, s: &str) -> core::fmt::Result {
            let end = self.len + s.len();
            if end > self.data.len() {
                return Err(core::fmt::Error);
            }
            self.data[self.len..end].copy_from_slice(s.as_bytes());
            self.len = end;
            Ok(())
        }
    }

    #[test]
    fn secret_debug_output_does_not_leak_bytes() {
        use core::fmt::Write as _;
        let secret = X25519SecretKey::from_bytes(ALICE_SECRET);
        let shared = secret
            .agree(&X25519PublicKey::from_bytes(BOB_PUBLIC))
            .expect("contributory");
        let mut sink = FmtSink::<192> {
            data: [0; 192],
            len: 0,
        };
        write!(&mut sink, "{secret:?} {shared:?}").expect("fits");
        let rendered = core::str::from_utf8(&sink.data[..sink.len]).expect("ascii");
        assert!(rendered.contains("X25519SecretKey"));
        assert!(rendered.contains("X25519SharedSecret"));
        assert!(!rendered.contains("119"));
        assert!(!rendered.contains("0x77"));
    }

    #[test]
    fn agreement_error_display_is_opaque() {
        use core::fmt::Write as _;
        let mut sink = FmtSink::<64> {
            data: [0; 64],
            len: 0,
        };
        write!(&mut sink, "{}", KeyAgreementError(())).expect("fits");
        assert!(sink.len > 0);
        assert!(sink.data[..sink.len].iter().all(u8::is_ascii));
        write!(&mut sink, "{:?}", KeyAgreementError(())).expect("fits");
    }
}
