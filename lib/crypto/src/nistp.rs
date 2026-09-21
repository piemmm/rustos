//! The NIST prime curves: P-256, P-384, and P-521.
//!
//! One module rather than a split across [`crate::sign`] and
//! [`crate::agree`], because ECDSA and ECDH over these curves share their
//! key material and their SEC1 point encoding; splitting them would mean two
//! copies of that encoding. SSH still treats them as separate algorithms —
//! `ecdsa-sha2-nistp*` signs, `ecdh-sha2-nistp*` agrees — and never uses one
//! key for both.
//!
//! Each curve pairs with exactly one hash, as RFC 5656 §6.2.1 assigns them:
//! P-256 with SHA-256, P-384 with SHA-384, P-521 with SHA-512. The pairing
//! is not a parameter, so a caller cannot weaken a curve by choosing a
//! shorter hash for it.
//!
//! # What the wrapper guarantees
//!
//! - **A public key is validated at construction.** Decoding checks the
//!   point is on the curve and is not the identity. These curves have
//!   cofactor 1, so there is no small subgroup to land in and that check is
//!   the whole of point validation — unlike the Montgomery curve in
//!   [`crate::agree`], where a non-contributory result has to be caught
//!   after the fact.
//! - **Signing nonces are deterministic (RFC 6979).** The per-signature
//!   nonce is derived from the private key and the message, so this module
//!   sources no randomness — and an ECDSA signature cannot leak the private
//!   key through a repeated or biased nonce, which is how ECDSA is usually
//!   broken in practice.
//! - **Signature scalars are fixed-width.** `r` and `s` are field-element
//!   byte strings, big-endian and zero-padded, which is what SSH's `mpint`
//!   encoder consumes; no DER is produced or parsed.
//!
//! # The agreement output is not a key
//!
//! As for X25519 ([`crate::agree`]), the agreement output is a curve
//! coordinate rather than a uniform bit string. Condense it through a PRF
//! before any byte of it reaches a cipher.

use core::fmt;

/// Failure to decode a key, or a signature that did not verify.
///
/// Deliberately opaque and single-variant, as elsewhere in this crate: a
/// caller must treat any of these as a refused input rather than branch on a
/// cause, and it already knows from *which* call returned the error whether
/// it was decoding or verifying. Detail belongs in the audit log.
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub struct NistCurveError(());

impl fmt::Display for NistCurveError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("nist prime-curve operation refused")
    }
}

/// Emit the key types and the ECDSA/ECDH operations for one NIST prime
/// curve.
///
/// The curves differ only in their field width and associated hash, so the
/// wrapper is written once rather than three times.
macro_rules! nist_curve {
    (
        $krate:ident,
        $spec:literal,
        $hash:literal,
        $scalar_len:ident = $scalar:literal,
        $point_len:ident = $point:literal,
        $scalar_ty:ident,
        $point_ty:ident,
        $shared_ty:ident,
        $secret:ident,
        $public:ident
    ) => {
        #[doc = concat!("Length, in bytes, of a ", $spec, " field element: a")]
        /// private scalar, a signature's `r` or `s`, or an agreement output.
        pub const $scalar_len: usize = $scalar;

        #[doc = concat!("Length, in bytes, of an uncompressed SEC1 ", $spec)]
        /// point: the `0x04` tag followed by the affine `X` and `Y`.
        pub const $point_len: usize = $point;

        #[doc = concat!("A ", $spec, " field element, big-endian.")]
        pub type $scalar_ty = [u8; $scalar_len];

        #[doc = concat!("An uncompressed SEC1 ", $spec, " point.")]
        pub type $point_ty = [u8; $point_len];

        #[doc = concat!("A ", $spec, " ECDH output: the shared point's affine")]
        /// `X` coordinate, which is what RFC 5656 §4 makes the SSH shared
        /// secret `K`.
        pub type $shared_ty = [u8; $scalar_len];

        #[doc = concat!("A ", $spec, " private key, wiped on drop.")]
        ///
        /// Holds the agreement and the signing view of one scalar. Both are
        /// built once, at construction, so neither `sign` nor `agree` has a
        /// failure path that cannot happen — and both upstream types wipe
        /// themselves on drop.
        pub struct $secret {
            inner: $krate::SecretKey,
            signing: $krate::ecdsa::SigningKey,
        }

        impl $secret {
            #[doc = concat!("Wrap a big-endian ", $spec, " private scalar.")]
            ///
            /// # Errors
            ///
            /// Returns [`NistCurveError`] if the scalar is zero or is not
            /// below the curve order — neither is a private key, and
            /// accepting one would produce signatures that leak or verify
            /// against the identity.
            pub fn from_scalar(bytes: &$scalar_ty) -> Result<Self, NistCurveError> {
                let inner = $krate::SecretKey::from_slice(bytes).map_err(|_| NistCurveError(()))?;
                let signing =
                    $krate::ecdsa::SigningKey::from_slice(bytes).map_err(|_| NistCurveError(()))?;
                Ok(Self { inner, signing })
            }

            /// The private scalar, big-endian.
            ///
            /// This *is* the private key. It exists so a key file can be
            /// written; the caller owns wiping the copy it takes.
            #[must_use]
            pub fn scalar(&self) -> $scalar_ty {
                let mut out = [0u8; $scalar_len];
                out.copy_from_slice(&self.inner.to_bytes());
                out
            }

            /// The matching public key.
            #[must_use]
            pub fn public_key(&self) -> $public {
                let inner = self.inner.public_key();
                let verifying = $krate::ecdsa::VerifyingKey::from(&self.signing);
                $public { inner, verifying }
            }

            #[doc = concat!("Sign `message` with ECDSA over ", $spec, " and ", $hash, ".")]
            ///
            /// Returns the signature as `(r, s)`, each a fixed-width
            /// big-endian field element. The nonce is RFC 6979
            /// deterministic, so signing the same message twice under the
            /// same key gives the same signature and no randomness is drawn.
            #[must_use]
            pub fn sign(&self, message: &[u8]) -> ($scalar_ty, $scalar_ty) {
                use $krate::ecdsa::signature::Signer as _;

                let signature: $krate::ecdsa::Signature = self.signing.sign(message);
                let (r, s) = signature.split_bytes();
                let mut r_out = [0u8; $scalar_len];
                let mut s_out = [0u8; $scalar_len];
                r_out.copy_from_slice(&r);
                s_out.copy_from_slice(&s);
                (r_out, s_out)
            }

            #[doc = concat!("Agree a shared secret with `peer` over ", $spec, ".")]
            ///
            /// The result is the shared point's `X` coordinate. `peer` was
            /// validated when it was decoded and the curve has cofactor 1,
            /// so there is no non-contributory case left to reject here.
            #[must_use]
            pub fn agree(&self, peer: &$public) -> $shared_ty {
                let shared = $krate::elliptic_curve::ecdh::diffie_hellman(
                    self.inner.to_nonzero_scalar(),
                    peer.inner.as_affine(),
                );
                let mut out = [0u8; $scalar_len];
                out.copy_from_slice(shared.raw_secret_bytes());
                out
            }
        }

        impl fmt::Debug for $secret {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                // The scalar is the private key; it never reaches a log.
                f.debug_struct(stringify!($secret)).finish_non_exhaustive()
            }
        }

        #[doc = concat!("A validated ", $spec, " public key.")]
        ///
        /// As for the private key, the agreement and verifying views are
        /// both built at construction, so no later operation can fail on a
        /// key this type already accepted.
        #[derive(Clone, Debug, Eq, PartialEq)]
        pub struct $public {
            inner: $krate::PublicKey,
            verifying: $krate::ecdsa::VerifyingKey,
        }

        impl $public {
            #[doc = concat!("Decode a SEC1 ", $spec, " point, compressed or")]
            /// uncompressed.
            ///
            /// Both forms are accepted because a peer chooses the encoding;
            /// what this rejects is a point that is not on the curve, the
            /// identity, or a malformed length.
            ///
            /// # Errors
            ///
            /// Returns [`NistCurveError`] for any of those.
            pub fn from_sec1(bytes: &[u8]) -> Result<Self, NistCurveError> {
                let inner =
                    $krate::PublicKey::from_sec1_bytes(bytes).map_err(|_| NistCurveError(()))?;
                let verifying = $krate::ecdsa::VerifyingKey::from_sec1_bytes(bytes)
                    .map_err(|_| NistCurveError(()))?;
                Ok(Self { inner, verifying })
            }

            /// Re-encode as an uncompressed SEC1 point.
            ///
            /// SSH puts this form on the wire, and re-encoding from the
            /// validated point — rather than echoing the bytes a peer sent —
            /// is what makes a byte comparison against another encoding
            /// meaningful.
            #[must_use]
            pub fn to_sec1_uncompressed(&self) -> $point_ty {
                use $krate::elliptic_curve::sec1::ToSec1Point as _;

                let encoded = self.inner.as_affine().to_sec1_point(false);
                let mut out = [0u8; $point_len];
                out.copy_from_slice(encoded.as_bytes());
                out
            }

            #[doc = concat!("Verify an ECDSA over ", $spec, " and ", $hash, " signature")]
            /// `(r, s)` on `message`.
            ///
            /// High-`s` signatures are accepted: ECDSA admits both `s` and
            /// `n - s`, foreign implementations emit either, and SSH imposes
            /// no malleability rule. Rejecting them would fail against
            /// conforming peers for no security gain, since the signature
            /// is not an identifier here.
            ///
            /// # Errors
            ///
            /// Returns [`NistCurveError`] if `(r, s)` is not a pair of
            /// in-range non-zero scalars, or if the signature does not
            /// verify.
            pub fn verify(
                &self,
                message: &[u8],
                r: &$scalar_ty,
                s: &$scalar_ty,
            ) -> Result<(), NistCurveError> {
                use $krate::ecdsa::signature::Verifier as _;

                let signature = $krate::ecdsa::Signature::from_scalars(*r, *s)
                    .map_err(|_| NistCurveError(()))?;
                self.verifying
                    .verify(message, &signature)
                    .map_err(|_| NistCurveError(()))
            }
        }
    };
}

nist_curve!(
    p256,
    "P-256",
    "SHA-256",
    P256_SCALAR_LEN = 32,
    P256_POINT_LEN = 65,
    P256Scalar,
    P256Point,
    P256SharedSecret,
    P256SecretKey,
    P256PublicKey
);

nist_curve!(
    p384,
    "P-384",
    "SHA-384",
    P384_SCALAR_LEN = 48,
    P384_POINT_LEN = 97,
    P384Scalar,
    P384Point,
    P384SharedSecret,
    P384SecretKey,
    P384PublicKey
);

nist_curve!(
    p521,
    "P-521",
    "SHA-512",
    P521_SCALAR_LEN = 66,
    P521_POINT_LEN = 133,
    P521Scalar,
    P521Point,
    P521SharedSecret,
    P521SecretKey,
    P521PublicKey
);

#[cfg(test)]
#[path = "nistp_tests.rs"]
mod tests;
