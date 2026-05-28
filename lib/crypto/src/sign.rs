//! Digital signature primitives.
//!
//! `abi-v1` uses Ed25519 (RFC 8032) signatures for manifest authentication
//! and capability-token issuance. Only verification is exposed here: signing
//! requires private-key material that lives behind the local capability
//! authority service introduced in Stage 2, never in callers of this crate.

use core::fmt;

use ed25519_dalek::{Signature, VerifyingKey, PUBLIC_KEY_LENGTH, SIGNATURE_LENGTH};

/// Wire length, in bytes, of an Ed25519 public key.
pub const ED25519_PUBLIC_KEY_LEN: usize = PUBLIC_KEY_LENGTH;

/// Wire length, in bytes, of an Ed25519 signature.
pub const ED25519_SIGNATURE_LEN: usize = SIGNATURE_LENGTH;

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

impl fmt::Debug for Ed25519PublicKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Avoid leaking the raw bytes in default `Debug` output.
        f.debug_struct("Ed25519PublicKey").finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::{Ed25519PublicKey, Ed25519Signature, SignatureError};

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
