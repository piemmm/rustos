//! Digital signature primitives: Ed25519.
//!
//! `abi-v1` uses Ed25519 (RFC 8032) signatures for manifest authentication
//! and capability-token issuance, and SSH uses the same curve for its
//! `ssh-ed25519` host and user keys.
//!
//! Signing lives here rather than in each caller, so `ed25519-dalek` is
//! named in exactly one crate in the workspace. A key is built from a
//! caller-supplied 32-byte seed: this module sources no randomness, because
//! only the caller knows whether its seed must come from the kernel CSPRNG
//! (a real key) or from a fixture (a test).
//!
//! ECDSA over the NIST prime curves is the other signature family TAIRiX
//! speaks; it lives in [`crate::nistp`] beside the ECDH that shares its key
//! encoding.

use core::fmt;

use ed25519_dalek::{
    Signature, Signer, SigningKey, VerifyingKey, PUBLIC_KEY_LENGTH, SECRET_KEY_LENGTH,
    SIGNATURE_LENGTH,
};

/// Wire length, in bytes, of an Ed25519 public key.
pub const ED25519_PUBLIC_KEY_LEN: usize = PUBLIC_KEY_LENGTH;

/// Wire length, in bytes, of an Ed25519 signature.
pub const ED25519_SIGNATURE_LEN: usize = SIGNATURE_LENGTH;

/// Length, in bytes, of an Ed25519 private-key seed.
///
/// RFC 8032 derives the whole key — the scalar and the nonce prefix — from
/// this one value by hashing it, so the seed *is* the private key and is the
/// only form worth storing.
pub const ED25519_SEED_LEN: usize = SECRET_KEY_LENGTH;

/// An Ed25519 private-key seed as raw bytes.
pub type Ed25519Seed = [u8; ED25519_SEED_LEN];

/// Failure to construct, decode, or verify a signature.
///
/// The cause is deliberately opaque: callers receiving a `SignatureError`
/// must treat the entire operation as a security failure and refuse the
/// input. Detailed diagnostics belong in the security audit log, not in the
/// public error type, so that side-channels through error variants cannot
/// leak signing-oracle information.
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub struct SignatureError(());

impl fmt::Display for SignatureError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ed25519 signature verification failed")
    }
}

/// 64-byte Ed25519 signature on the wire.
#[repr(transparent)]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct Ed25519Signature(pub [u8; ED25519_SIGNATURE_LEN]);

impl Ed25519Signature {
    /// Wrap a raw 64-byte signature with no parsing.
    ///
    /// Validity (point decompression, scalar canonicalisation) is enforced
    /// at verify time by [`Ed25519PublicKey::verify`].
    #[must_use]
    pub const fn from_bytes(bytes: [u8; ED25519_SIGNATURE_LEN]) -> Self {
        Self(bytes)
    }

    /// Borrow the raw bytes.
    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; ED25519_SIGNATURE_LEN] {
        &self.0
    }
}

/// 32-byte Ed25519 verifying key.
///
/// Constructing one parses and validates the encoded point; an invalid
/// encoding is rejected before the key can be used for verification.
#[derive(Clone)]
pub struct Ed25519PublicKey {
    inner: VerifyingKey,
}

impl Ed25519PublicKey {
    /// Wrap a 32-byte Ed25519 public key.
    ///
    /// The encoding is checked for basic well-formedness as defined by
    /// upstream [`ed25519_dalek`]; deeper validation (point-order checks,
    /// canonical scalar form) is performed at verification time by
    /// [`Ed25519PublicKey::verify`], so that callers cannot accidentally
    /// bypass the canonical RFC 8032 strict-verification rules. A failure
    /// here is reported as [`SignatureError`] without further detail.
    pub fn from_bytes(bytes: &[u8; ED25519_PUBLIC_KEY_LEN]) -> Result<Self, SignatureError> {
        match VerifyingKey::from_bytes(bytes) {
            Ok(inner) => Ok(Self { inner }),
            Err(_) => Err(SignatureError(())),
        }
    }

    /// Verify that `signature` was produced over `message` by the holder of
    /// this public key.
    ///
    /// Uses the strict Ed25519 verification rules from RFC 8032: the
    /// signature scalar must be canonical and the verifying key must not be
    /// a small-order point. Any rejection is reported as [`SignatureError`]
    /// without further detail (see the type's docstring for rationale).
    pub fn verify(
        &self,
        message: &[u8],
        signature: &Ed25519Signature,
    ) -> Result<(), SignatureError> {
        let dalek_sig = Signature::from_bytes(&signature.0);
        self.inner
            .verify_strict(message, &dalek_sig)
            .map_err(|_| SignatureError(()))
    }

    /// Borrow the raw key bytes.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8; ED25519_PUBLIC_KEY_LEN] {
        self.inner.as_bytes()
    }
}

/// An Ed25519 private key, wiped on drop.
///
/// Built from a caller-supplied seed; this type never generates one. The
/// caller draws the seed from the kernel CSPRNG for a real key and owns
/// wiping its own copy.
pub struct Ed25519SecretKey {
    inner: SigningKey,
}

impl Ed25519SecretKey {
    /// Derive a key pair from a 32-byte seed (RFC 8032 §5.1.5).
    ///
    /// Every 32-byte string is a valid seed — the scalar is derived by
    /// hashing and clamping — so there is nothing to reject and no fallible
    /// path.
    #[must_use]
    pub fn from_seed(seed: &Ed25519Seed) -> Self {
        Self {
            inner: SigningKey::from_bytes(seed),
        }
    }

    /// The seed this key was derived from.
    ///
    /// This *is* the private key. It exists so a key file can be written;
    /// the caller owns wiping the copy it takes.
    #[must_use]
    pub fn seed(&self) -> Ed25519Seed {
        self.inner.to_bytes()
    }

    /// The matching public key.
    #[must_use]
    pub fn public_key(&self) -> Ed25519PublicKey {
        Ed25519PublicKey {
            inner: self.inner.verifying_key(),
        }
    }

    /// Sign `message`.
    ///
    /// Ed25519 is deterministic: the per-signature nonce is derived from the
    /// key and the message, so signing draws no randomness and cannot leak
    /// the key through a repeated nonce.
    #[must_use]
    pub fn sign(&self, message: &[u8]) -> Ed25519Signature {
        Ed25519Signature(self.inner.sign(message).to_bytes())
    }
}

impl fmt::Debug for Ed25519SecretKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // The seed is the private key; it never reaches a log.
        f.debug_struct("Ed25519SecretKey").finish_non_exhaustive()
    }
}

impl fmt::Debug for Ed25519PublicKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Avoid leaking the raw bytes in default `Debug` output.
        f.debug_struct("Ed25519PublicKey").finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::{
        Ed25519PublicKey, Ed25519SecretKey, Ed25519Seed, Ed25519Signature, SignatureError,
        ED25519_SEED_LEN,
    };

    /// RFC 8032 §7.1, test vector 1: empty message.
    const PUBLIC: [u8; 32] = [
        0xd7, 0x5a, 0x98, 0x01, 0x82, 0xb1, 0x0a, 0xb7, 0xd5, 0x4b, 0xfe, 0xd3, 0xc9, 0x64, 0x07,
        0x3a, 0x0e, 0xe1, 0x72, 0xf3, 0xda, 0xa6, 0x23, 0x25, 0xaf, 0x02, 0x1a, 0x68, 0xf7, 0x07,
        0x51, 0x1a,
    ];
    const SIGNATURE: [u8; 64] = [
        0xe5, 0x56, 0x43, 0x00, 0xc3, 0x60, 0xac, 0x72, 0x90, 0x86, 0xe2, 0xcc, 0x80, 0x6e, 0x82,
        0x8a, 0x84, 0x87, 0x7f, 0x1e, 0xb8, 0xe5, 0xd9, 0x74, 0xd8, 0x73, 0xe0, 0x65, 0x22, 0x49,
        0x01, 0x55, 0x5f, 0xb8, 0x82, 0x15, 0x90, 0xa3, 0x3b, 0xac, 0xc6, 0x1e, 0x39, 0x70, 0x1c,
        0xf9, 0xb4, 0x6b, 0xd2, 0x5b, 0xf5, 0xf0, 0x59, 0x5b, 0xbe, 0x24, 0x65, 0x51, 0x41, 0x43,
        0x8e, 0x7a, 0x10, 0x0b,
    ];

    #[test]
    fn rfc8032_vector_one_verifies() {
        let key = Ed25519PublicKey::from_bytes(&PUBLIC).expect("valid key");
        let sig = Ed25519Signature::from_bytes(SIGNATURE);
        assert!(key.verify(b"", &sig).is_ok());
    }

    #[test]
    fn tampered_signature_fails() {
        let key = Ed25519PublicKey::from_bytes(&PUBLIC).expect("valid key");
        let mut bad = SIGNATURE;
        bad[0] ^= 0x01;
        let sig = Ed25519Signature::from_bytes(bad);
        assert_eq!(key.verify(b"", &sig), Err(SignatureError(())));
    }

    #[test]
    fn tampered_message_fails() {
        let key = Ed25519PublicKey::from_bytes(&PUBLIC).expect("valid key");
        let sig = Ed25519Signature::from_bytes(SIGNATURE);
        assert_eq!(key.verify(b"x", &sig), Err(SignatureError(())));
    }

    #[test]
    fn public_key_as_bytes_round_trips() {
        let key = Ed25519PublicKey::from_bytes(&PUBLIC).expect("valid key");
        assert_eq!(key.as_bytes(), &PUBLIC);
    }

    #[test]
    fn signature_as_bytes_returns_input() {
        let sig = Ed25519Signature::from_bytes(SIGNATURE);
        assert_eq!(sig.as_bytes(), &SIGNATURE);
    }

    /// RFC 8032 §7.1, test vector 1: the 32-byte private seed matching
    /// [`PUBLIC`] and [`SIGNATURE`].
    const SEED: Ed25519Seed = [
        0x9d, 0x61, 0xb1, 0x9d, 0xef, 0xfd, 0x5a, 0x60, 0xba, 0x84, 0x4a, 0xf4, 0x92, 0xec, 0x2c,
        0xc4, 0x44, 0x49, 0xc5, 0x69, 0x7b, 0x32, 0x69, 0x19, 0x70, 0x3b, 0xac, 0x03, 0x1c, 0xae,
        0x7f, 0x60,
    ];

    /// RFC 8032 §7.1, test vector 2: seed, public key, one-byte message,
    /// and signature. A second vector catches a wrapper that happened to
    /// work for the empty message alone.
    const SEED_2: Ed25519Seed = [
        0x4c, 0xcd, 0x08, 0x9b, 0x28, 0xff, 0x96, 0xda, 0x9d, 0xb6, 0xc3, 0x46, 0xec, 0x11, 0x4e,
        0x0f, 0x5b, 0x8a, 0x31, 0x9f, 0x35, 0xab, 0xa6, 0x24, 0xda, 0x8c, 0xf6, 0xed, 0x4f, 0xb8,
        0xa6, 0xfb,
    ];
    const PUBLIC_2: [u8; 32] = [
        0x3d, 0x40, 0x17, 0xc3, 0xe8, 0x43, 0x89, 0x5a, 0x92, 0xb7, 0x0a, 0xa7, 0x4d, 0x1b, 0x7e,
        0xbc, 0x9c, 0x98, 0x2c, 0xcf, 0x2e, 0xc4, 0x96, 0x8c, 0xc0, 0xcd, 0x55, 0xf1, 0x2a, 0xf4,
        0x66, 0x0c,
    ];
    const MESSAGE_2: [u8; 1] = [0x72];
    const SIGNATURE_2: [u8; 64] = [
        0x92, 0xa0, 0x09, 0xa9, 0xf0, 0xd4, 0xca, 0xb8, 0x72, 0x0e, 0x82, 0x0b, 0x5f, 0x64, 0x25,
        0x40, 0xa2, 0xb2, 0x7b, 0x54, 0x16, 0x50, 0x3f, 0x8f, 0xb3, 0x76, 0x22, 0x23, 0xeb, 0xdb,
        0x69, 0xda, 0x08, 0x5a, 0xc1, 0xe4, 0x3e, 0x15, 0x99, 0x6e, 0x45, 0x8f, 0x36, 0x13, 0xd0,
        0xf1, 0x1d, 0x8c, 0x38, 0x7b, 0x2e, 0xae, 0xb4, 0x30, 0x2a, 0xee, 0xb0, 0x0d, 0x29, 0x16,
        0x12, 0xbb, 0x0c, 0x00,
    ];

    #[test]
    fn signing_matches_the_rfc8032_vectors() {
        let key = Ed25519SecretKey::from_seed(&SEED);
        assert_eq!(key.public_key().as_bytes(), &PUBLIC);
        assert_eq!(key.sign(b"").as_bytes(), &SIGNATURE);

        let key = Ed25519SecretKey::from_seed(&SEED_2);
        assert_eq!(key.public_key().as_bytes(), &PUBLIC_2);
        assert_eq!(key.sign(&MESSAGE_2).as_bytes(), &SIGNATURE_2);
    }

    /// Ed25519 derives its nonce from the key and message, so signing is
    /// deterministic — the property that makes a repeated nonce impossible.
    #[test]
    fn signing_is_deterministic() {
        let key = Ed25519SecretKey::from_seed(&SEED);
        assert_eq!(key.sign(b"a message"), key.sign(b"a message"));
        assert_ne!(key.sign(b"a message"), key.sign(b"another message"));
    }

    #[test]
    fn a_signed_message_verifies_under_its_own_key() {
        let key = Ed25519SecretKey::from_seed(&SEED_2);
        let public = key.public_key();
        let signature = key.sign(b"the quick brown fox");
        assert!(public.verify(b"the quick brown fox", &signature).is_ok());
        assert_eq!(
            public.verify(b"the quick brown fix", &signature),
            Err(SignatureError(()))
        );

        // A different key's signature does not verify under this one.
        let other = Ed25519SecretKey::from_seed(&SEED);
        assert_eq!(
            public.verify(b"the quick brown fox", &other.sign(b"the quick brown fox")),
            Err(SignatureError(()))
        );
    }

    /// The seed is the whole private key, so a key file written from it
    /// reloads to the same signer.
    #[test]
    fn the_seed_round_trips() {
        let key = Ed25519SecretKey::from_seed(&SEED);
        assert_eq!(key.seed(), SEED);
        assert_eq!(ED25519_SEED_LEN, 32);
        let reloaded = Ed25519SecretKey::from_seed(&key.seed());
        assert_eq!(reloaded.sign(b"x"), key.sign(b"x"));
    }

    /// The private seed never reaches a log.
    #[test]
    fn secret_key_debug_does_not_leak_the_seed() {
        use core::fmt::Write as _;
        let key = Ed25519SecretKey::from_seed(&SEED);
        let mut buf = FmtSink::<128> {
            data: [0; 128],
            len: 0,
        };
        write!(&mut buf, "{key:?}").expect("fits");
        let rendered = core::str::from_utf8(&buf.data[..buf.len]).expect("ascii");
        assert!(rendered.contains("Ed25519SecretKey"));
        assert!(!rendered.contains("0x9d"));
    }

    /// Fixed-size `core::fmt::Write` sink used by Display/Debug tests so
    /// they remain `no_std`-clean (no `alloc` dependency).
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
    fn public_key_debug_does_not_leak_bytes() {
        use core::fmt::Write as _;
        let key = Ed25519PublicKey::from_bytes(&PUBLIC).expect("valid key");
        let mut buf = FmtSink::<128> {
            data: [0; 128],
            len: 0,
        };
        write!(&mut buf, "{key:?}").expect("fits");
        let rendered = core::str::from_utf8(&buf.data[..buf.len]).expect("ascii");
        assert!(rendered.contains("Ed25519PublicKey"));
        // The raw key bytes must not appear in the debug output.
        assert!(!rendered.contains("0xd7"));
    }

    #[test]
    fn signature_error_display_is_opaque() {
        use core::fmt::Write as _;
        let mut sink = FmtSink::<64> {
            data: [0; 64],
            len: 0,
        };
        write!(&mut sink, "{}", SignatureError(())).expect("fits");
        assert!(sink.len > 0);
        assert!(sink.data[..sink.len].iter().all(u8::is_ascii));
        // Debug impl is the auto-derived one but exercising it keeps the
        // derive from being silently dropped in a refactor.
        write!(&mut sink, "{:?}", SignatureError(())).expect("fits");
    }
}
