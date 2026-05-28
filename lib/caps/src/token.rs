//! Signed capability tokens.
//!
//! A [`CapabilityToken`] is the on-wire envelope a privileged authority
//! issues when it delegates a capability set to a less-privileged task. The
//! envelope is signed with Ed25519; the only signing key in the system
//! belongs to the local capability authority service (issued at install
//! time by the installer; see `PLAN.md` Stage 7).
//!
//! Verification is intentionally the only operation exposed by this module:
//! every caller in the codebase consumes tokens, none of them produces them.
//! Test code in this crate signs tokens via a `dev-dependency` on
//! `ed25519-dalek` so the production audit surface remains a single
//! signature verifier.

use rustos_abi::{Errno, ABI_VERSION_CURRENT};
use rustos_crypto::{Ed25519PublicKey, Ed25519Signature};

use crate::set::CapabilitySet;

/// Revocation epoch attached to every issued [`CapabilityToken`].
///
/// A verifier accepts a token only when its epoch matches the current
/// epoch the authority is advertising. Bumping the authority's epoch is the
/// global mass-revocation primitive described in `AGENTS.md` §5.2.
#[repr(transparent)]
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq, Hash, Ord, PartialOrd)]
pub struct RevocationEpoch(pub u64);

/// Length of an encoded [`CapabilityToken`] body (the bytes the signature
/// covers).
pub const TOKEN_BODY_LEN: usize = 4 // abi version
    + 8 // subject
    + 8 // epoch
    + 32; // capability bitmap (4 × u64)

/// Length of an encoded [`CapabilityToken`] on the wire.
pub const TOKEN_WIRE_LEN: usize = TOKEN_BODY_LEN + 64;

/// Capability token delegated by a privileged authority.
///
/// Wire layout (little-endian):
///
/// | Offset | Size | Field        |
/// |-------:|-----:|--------------|
/// |    0   |  4   | ABI version  |
/// |    4   |  8   | Subject task |
/// |   12   |  8   | Epoch        |
/// |   20   | 32   | Capability bitmap (4 × u64) |
/// |   52   | 64   | Ed25519 signature over bytes 0..52 |
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct CapabilityToken {
    /// ABI version of the encoded token. Must match
    /// [`rustos_abi::ABI_VERSION_CURRENT`] when verifying.
    pub abi_version: u32,
    /// Identifier of the task the token is being delegated to.
    pub subject: u64,
    /// Epoch under which the token was issued.
    pub epoch: RevocationEpoch,
    /// Set of capabilities granted by the token.
    pub caps: CapabilitySet,
    /// Ed25519 signature over the encoded body (bytes 0..[`TOKEN_BODY_LEN`]).
    pub signature: Ed25519Signature,
}

impl CapabilityToken {
    /// Encode the signing input — the bytes the signature must be computed
    /// over and verified against.
    ///
    /// Made `pub` so an out-of-process authority service can sign the same
    /// bytes the verifier will check, with no risk of layout drift.
    #[must_use]
    pub fn signing_input(
        abi_version: u32,
        subject: u64,
        epoch: RevocationEpoch,
        caps: &CapabilitySet,
    ) -> [u8; TOKEN_BODY_LEN] {
        let mut out = [0u8; TOKEN_BODY_LEN];
        out[0..4].copy_from_slice(&abi_version.to_le_bytes());
        out[4..12].copy_from_slice(&subject.to_le_bytes());
        out[12..20].copy_from_slice(&epoch.0.to_le_bytes());
        let words = caps.as_words();
        out[20..28].copy_from_slice(&words[0].to_le_bytes());
        out[28..36].copy_from_slice(&words[1].to_le_bytes());
        out[36..44].copy_from_slice(&words[2].to_le_bytes());
        out[44..52].copy_from_slice(&words[3].to_le_bytes());
        out
    }

    /// Encode `self` into its little-endian wire representation.
    #[must_use]
    pub fn to_le_bytes(&self) -> [u8; TOKEN_WIRE_LEN] {
        let body = Self::signing_input(self.abi_version, self.subject, self.epoch, &self.caps);
        let mut out = [0u8; TOKEN_WIRE_LEN];
        out[..TOKEN_BODY_LEN].copy_from_slice(&body);
        out[TOKEN_BODY_LEN..].copy_from_slice(self.signature.as_bytes());
        out
    }

    /// Decode `bytes` into a [`CapabilityToken`] without verifying the
    /// signature.
    ///
    /// Use [`Self::verify`] to validate signature, ABI version, parent
    /// authority, and revocation epoch.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, Errno> {
        if bytes.len() < TOKEN_WIRE_LEN {
            return Err(Errno::BufferTooSmall);
        }
        let abi_version = u32::from_le_bytes(bytes_at::<4>(bytes, 0));
        let subject = u64::from_le_bytes(bytes_at::<8>(bytes, 4));
        let epoch = RevocationEpoch(u64::from_le_bytes(bytes_at::<8>(bytes, 12)));
        let caps = CapabilitySet::from_words([
            u64::from_le_bytes(bytes_at::<8>(bytes, 20)),
            u64::from_le_bytes(bytes_at::<8>(bytes, 28)),
            u64::from_le_bytes(bytes_at::<8>(bytes, 36)),
            u64::from_le_bytes(bytes_at::<8>(bytes, 44)),
        ]);
        let mut sig_bytes = [0u8; 64];
        sig_bytes.copy_from_slice(&bytes[TOKEN_BODY_LEN..TOKEN_WIRE_LEN]);
        Ok(Self {
            abi_version,
            subject,
            epoch,
            caps,
            signature: Ed25519Signature::from_bytes(sig_bytes),
        })
    }

    /// Verify the token end-to-end.
    ///
    /// The following checks must all pass; any failure returns the
    /// indicated [`Errno`] without leaking which one specifically (callers
    /// translate this into a single audit-log event, never into a
    /// distinguishable user-visible status):
    ///
    /// * ABI version matches [`rustos_abi::ABI_VERSION_CURRENT`]
    ///   ⇒ [`Errno::AbiVersionUnsupported`].
    /// * Epoch matches the verifier's current epoch ⇒ [`Errno::NotFound`].
    /// * Signature over the encoded body verifies against `authority`
    ///   ⇒ [`Errno::SignatureInvalid`].
    /// * Delegated set is a subset of the parent set the verifier is
    ///   willing to grant ⇒ [`Errno::DelegationWiden`].
    pub fn verify(
        &self,
        authority: &Ed25519PublicKey,
        parent: &CapabilitySet,
        current_epoch: RevocationEpoch,
    ) -> Result<(), Errno> {
        if self.abi_version != ABI_VERSION_CURRENT {
            return Err(Errno::AbiVersionUnsupported);
        }
        if self.epoch != current_epoch {
            return Err(Errno::NotFound);
        }
        let body = Self::signing_input(self.abi_version, self.subject, self.epoch, &self.caps);
        authority
            .verify(&body, &self.signature)
            .map_err(|_| Errno::SignatureInvalid)?;
        if !self.caps.is_subset_of(parent) {
            return Err(Errno::DelegationWiden);
        }
        Ok(())
    }
}

fn bytes_at<const N: usize>(bytes: &[u8], offset: usize) -> [u8; N] {
    let mut out = [0u8; N];
    out.copy_from_slice(&bytes[offset..offset + N]);
    out
}

#[cfg(test)]
mod tests {
    use super::{CapabilityToken, RevocationEpoch, TOKEN_BODY_LEN, TOKEN_WIRE_LEN};
    use crate::set::CapabilitySet;
    use ed25519_dalek::{Signer, SigningKey};
    use rustos_abi::{CapabilityId, Errno, ABI_VERSION_CURRENT};
    use rustos_crypto::{Ed25519PublicKey, Ed25519Signature};

    fn signing_key() -> SigningKey {
        // Deterministic 32-byte seed. Tests must not depend on RNG.
        let seed = [42u8; 32];
        SigningKey::from_bytes(&seed)
    }

    fn authority_key(signing: &SigningKey) -> Ed25519PublicKey {
        let vk = signing.verifying_key();
        Ed25519PublicKey::from_bytes(vk.as_bytes()).expect("valid key")
    }

    fn sample_caps() -> CapabilitySet {
        let mut s = CapabilitySet::empty();
        s.insert(CapabilityId::FS_MOUNT);
        s.insert(CapabilityId::DRV_LOAD);
        s
    }

    fn parent_caps() -> CapabilitySet {
        let mut s = sample_caps();
        s.insert(CapabilityId::AUDIT_READ);
        s
    }

    fn sign(subject: u64, epoch: RevocationEpoch, caps: &CapabilitySet) -> CapabilityToken {
        let key = signing_key();
        let body = CapabilityToken::signing_input(ABI_VERSION_CURRENT, subject, epoch, caps);
        let sig = key.sign(&body);
        CapabilityToken {
            abi_version: ABI_VERSION_CURRENT,
            subject,
            epoch,
            caps: *caps,
            signature: Ed25519Signature::from_bytes(sig.to_bytes()),
        }
    }

    #[test]
    fn wire_sizes_are_consistent() {
        assert_eq!(TOKEN_BODY_LEN, 52);
        assert_eq!(TOKEN_WIRE_LEN, 116);
    }

    #[test]
    fn round_trip_encode_decode_preserves_fields() {
        let token = sign(7, RevocationEpoch(3), &sample_caps());
        let bytes = token.to_le_bytes();
        let decoded = CapabilityToken::from_bytes(&bytes).expect("valid token bytes");
        assert_eq!(decoded, token);
    }

    #[test]
    fn verify_accepts_signed_subset_at_current_epoch() {
        let token = sign(7, RevocationEpoch(1), &sample_caps());
        assert_eq!(
            token.verify(
                &authority_key(&signing_key()),
                &parent_caps(),
                RevocationEpoch(1)
            ),
            Ok(()),
        );
    }

    #[test]
    fn verify_rejects_widened_caps() {
        // A signed token whose payload widens the authority must be rejected
        // even though the signature is itself valid.
        let mut widened = sample_caps();
        widened.insert(CapabilityId::USER_ADMIN);
        let token = sign(7, RevocationEpoch(1), &widened);
        let mut parent = parent_caps();
        parent.remove(CapabilityId::DRV_LOAD); // ensure widened is strictly wider.
        assert_eq!(
            token.verify(&authority_key(&signing_key()), &parent, RevocationEpoch(1)),
            Err(Errno::DelegationWiden),
        );
    }

    #[test]
    fn verify_rejects_wrong_epoch() {
        let token = sign(7, RevocationEpoch(1), &sample_caps());
        assert_eq!(
            token.verify(
                &authority_key(&signing_key()),
                &parent_caps(),
                RevocationEpoch(2)
            ),
            Err(Errno::NotFound),
        );
    }

    #[test]
    fn verify_rejects_bad_signature() {
        let mut token = sign(7, RevocationEpoch(1), &sample_caps());
        let mut bad = *token.signature.as_bytes();
        bad[0] ^= 0xAA;
        token.signature = Ed25519Signature::from_bytes(bad);
        assert_eq!(
            token.verify(
                &authority_key(&signing_key()),
                &parent_caps(),
                RevocationEpoch(1)
            ),
            Err(Errno::SignatureInvalid),
        );
    }

    #[test]
    fn verify_rejects_wrong_abi_version() {
        let mut token = sign(7, RevocationEpoch(1), &sample_caps());
        token.abi_version = ABI_VERSION_CURRENT + 1;
        assert_eq!(
            token.verify(
                &authority_key(&signing_key()),
                &parent_caps(),
                RevocationEpoch(1)
            ),
            Err(Errno::AbiVersionUnsupported),
        );
    }

    #[test]
    fn from_bytes_rejects_short_buffer() {
        assert_eq!(
            CapabilityToken::from_bytes(&[0u8; 32]),
            Err(Errno::BufferTooSmall)
        );
    }
}
