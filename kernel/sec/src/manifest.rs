//! Verification of signed `rxe` manifests.
//!
//! An `rxe` binary carries a [`tairix_abi::ManifestHeader`] followed by a
//! body listing the [`CapabilityId`]s the binary wants to exercise. This
//! module turns those bytes into a [`VerifiedManifest`] only when **every**
//! check from the issue brief succeeds:
//!
//! 1. The header is well-formed (`ManifestHeader::from_bytes`).
//! 2. The ABI version equals [`tairix_abi::ABI_VERSION_CURRENT`].
//! 3. The body has the exact length the header declared.
//! 4. Every capability ID in the body is a *known* `abi-v1` identifier
//!    (i.e. one of the constants exposed by `lib/abi`); unknown values
//!    are refused so a binary cannot smuggle "reserved" IDs the kernel
//!    will silently grant later.
//! 5. The manifest's embedded `signer_pubkey` matches the kernel's
//!    configured authority key — preventing a malicious signer from
//!    swapping in its own public key alongside a valid self-signature.
//! 6. The Ed25519 signature verifies over the canonical signing input:
//!    the manifest header bytes covered by
//!    [`ManifestHeader::signed_range`] concatenated with the body bytes.
//!
//! Every outcome emits exactly one audit event with the documented ID;
//! see [`crate::AuditEvent`] and `docs/src/architecture/security.md`.

#[cfg(test)]
extern crate alloc;

use core::mem::size_of;

use tairix_abi::{CapabilityId, Errno, ManifestHeader, ABI_VERSION_CURRENT};
use tairix_caps::CapabilitySet;
use tairix_crypto::Ed25519PublicKey;
use tairix_log::{Field, Sink};

use crate::audit::{record, AuditEvent};
use crate::identity::format_i32;

/// Per-capability size of the manifest body (little-endian `u16`).
const CAPABILITY_BODY_STRIDE: usize = size_of::<u16>();

/// Outcome of a successful [`verify_manifest`] call.
///
/// The verifier hands the kernel exactly the fields it will consult on
/// subsequent privileged operations: the requested capability set
/// (intersected with the user grant by [`crate::TaskCapabilities`]) and
/// the manifest header itself for any later cross-checks (syscall table
/// hash, ABI version on dynamic loads, etc.).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VerifiedManifest {
    /// ABI version stored in the header (always [`ABI_VERSION_CURRENT`]
    /// for a successfully verified manifest).
    pub abi_version: u32,
    /// Flag bits from the header.
    pub flags: u32,
    /// Hash of the syscall table the binary was linked against. Stage 2.4
    /// does not yet consume this field; it is preserved verbatim so
    /// `kernel/syscall` (Stage 2.6 in the issue brief's numbering) can
    /// cross-check at dispatch time without re-parsing the manifest.
    pub syscall_table_hash: [u8; 32],
    /// Verified signer public key (equal to the kernel's authority key).
    pub signer_pubkey: [u8; 32],
    /// Capabilities the binary requested. Every entry is a known
    /// `abi-v1` identifier; the set may be empty.
    pub requested: CapabilitySet,
}

/// `true` if `id` corresponds to a capability the `abi-v1` kernel knows
/// how to grant.
///
/// Reject-on-unknown is mandatory ("Validate every
/// input; no trusted-caller shortcuts"). When `abi-v2` defines new
/// capabilities, this function moves with the rest of the manifest
/// verification logic to the new ABI module — `abi-v1` keeps the list
/// it shipped with.
#[must_use]
pub fn is_known_capability(id: CapabilityId) -> bool {
    matches!(
        id,
        CapabilityId::FS_MOUNT
            | CapabilityId::NET_RAW
            | CapabilityId::DRV_LOAD
            | CapabilityId::DRV_KERNEL
            | CapabilityId::USER_ADMIN
            | CapabilityId::TIME_SET
            | CapabilityId::IPC_BIND_PRIVILEGED
            | CapabilityId::AUDIT_READ
            | CapabilityId::AUDIT_WRITE
            | CapabilityId::MEM_DMA
            | CapabilityId::IRQ_BIND
            | CapabilityId::MMIO_MAP
            | CapabilityId::PROC_CONTROL
    )
}

/// Verify a signed manifest end-to-end.
///
/// `bytes` is the encoded manifest: the [`ManifestHeader`] followed by
/// `header.capability_count` little-endian `u16` capability identifiers.
/// `authority` is the kernel's configured signer public key.
///
/// # Errors
///
/// * [`Errno::BufferTooSmall`], [`Errno::BadMagic`],
///   [`Errno::LengthOutOfRange`] — header is structurally invalid
///   (audit: [`AuditEvent::ManifestBadHeader`]).
/// * [`Errno::AbiVersionUnsupported`] — header parsed but its ABI
///   version is not [`ABI_VERSION_CURRENT`]
///   (audit: [`AuditEvent::ManifestAbiMismatch`]).
/// * [`Errno::OutOfRange`] — a body entry is outside the capability ID
///   range or names an unknown capability
///   (audit: [`AuditEvent::ManifestUnknownCapability`]).
/// * [`Errno::SignatureInvalid`] — the signer public key disagrees with
///   `authority`, or the Ed25519 signature does not verify
///   (audit: [`AuditEvent::ManifestSignatureInvalid`]).
pub fn verify_manifest<S: Sink + ?Sized>(
    bytes: &[u8],
    authority: &Ed25519PublicKey,
    audit: &S,
) -> Result<VerifiedManifest, Errno> {
    // 1. Header decode. `ManifestHeader::from_bytes` distinguishes between
    //    structural failures (BufferTooSmall, BadMagic, LengthOutOfRange)
    //    and an ABI mismatch (AbiVersionUnsupported); we split those into
    //    two audit IDs so an operator can spot an ABI rollover separately
    //    from a corrupted binary.
    let header = match ManifestHeader::from_bytes(bytes) {
        Ok(h) => h,
        Err(err @ Errno::AbiVersionUnsupported) => {
            emit_errno(audit, AuditEvent::ManifestAbiMismatch, err);
            return Err(err);
        }
        Err(err) => {
            emit_errno(audit, AuditEvent::ManifestBadHeader, err);
            return Err(err);
        }
    };
    debug_assert_eq!(header.abi_version, ABI_VERSION_CURRENT);

    // 2. Body length must match the declared capability count exactly.
    let body_len = usize::from(header.capability_count) * CAPABILITY_BODY_STRIDE;
    let manifest_end = ManifestHeader::WIRE_LEN
        .checked_add(body_len)
        .ok_or_else(|| {
            emit_errno(
                audit,
                AuditEvent::ManifestBadHeader,
                Errno::LengthOutOfRange,
            );
            Errno::LengthOutOfRange
        })?;
    if bytes.len() < manifest_end {
        emit_errno(audit, AuditEvent::ManifestBadHeader, Errno::BufferTooSmall);
        return Err(Errno::BufferTooSmall);
    }

    // 3. Parse and validate each requested capability ID. A duplicate or
    //    out-of-range entry, or one that names an unknown capability, is
    //    refused. Building a `CapabilitySet` from the bag handles
    //    duplicates idempotently — they are not themselves grounds for
    //    rejection but unknowns absolutely are.
    let body = &bytes[ManifestHeader::WIRE_LEN..manifest_end];
    let mut decoded = [CapabilityId::FS_MOUNT; tairix_abi::MANIFEST_MAX_CAPABILITIES as usize];
    let count = usize::from(header.capability_count);
    if tairix_abi::decode_capability_ids(body, count, &mut decoded).is_err() {
        emit_errno(
            audit,
            AuditEvent::ManifestUnknownCapability,
            Errno::OutOfRange,
        );
        return Err(Errno::OutOfRange);
    }
    let mut requested = CapabilitySet::empty();
    for &id in &decoded[..count] {
        if !is_known_capability(id) {
            emit_errno(
                audit,
                AuditEvent::ManifestUnknownCapability,
                Errno::OutOfRange,
            );
            return Err(Errno::OutOfRange);
        }
        requested.insert(id);
    }

    // 4. Authority pinning. The header's embedded `signer_pubkey` must
    //    match the authority key the kernel was configured with at boot.
    //    Without this check, a manifest signed by an attacker's key
    //    would still "verify" against itself.
    if header.signer_pubkey != *authority.as_bytes() {
        emit_errno(
            audit,
            AuditEvent::ManifestSignatureInvalid,
            Errno::SignatureInvalid,
        );
        return Err(Errno::SignatureInvalid);
    }

    // 5. Ed25519 signature. The signing input is the canonical
    //    concatenation of header bytes (excluding the signature) and the
    //    body bytes.
    let signed_range = ManifestHeader::signed_range();
    let signing_input_len = signed_range.end - signed_range.start + body_len;
    let mut signing_input = [0u8;
        ManifestHeader::WIRE_LEN + // headroom for the largest legal payload
        (tairix_abi::MANIFEST_MAX_CAPABILITIES as usize) * CAPABILITY_BODY_STRIDE];
    signing_input[..signed_range.end].copy_from_slice(&bytes[..signed_range.end]);
    signing_input[signed_range.end..signing_input_len].copy_from_slice(body);
    let signature = tairix_crypto::Ed25519Signature::from_bytes(header.signature);
    if authority
        .verify(&signing_input[..signing_input_len], &signature)
        .is_err()
    {
        emit_errno(
            audit,
            AuditEvent::ManifestSignatureInvalid,
            Errno::SignatureInvalid,
        );
        return Err(Errno::SignatureInvalid);
    }

    let mut buf = [0u8; 12];
    let count = format_i32(i32::from(header.capability_count), &mut buf);
    record(
        audit,
        AuditEvent::ManifestVerified,
        &[Field {
            key: "caps",
            value: tairix_log::FieldValue::Str(count),
        }],
    );

    Ok(VerifiedManifest {
        abi_version: header.abi_version,
        flags: header.flags,
        syscall_table_hash: header.syscall_table_hash,
        signer_pubkey: header.signer_pubkey,
        requested,
    })
}

fn emit_errno<S: Sink + ?Sized>(audit: &S, event: AuditEvent, err: Errno) {
    let mut buf = [0u8; 12];
    let cause = format_i32(err.as_i32(), &mut buf);
    record(
        audit,
        event,
        &[Field {
            key: "errno",
            value: tairix_log::FieldValue::Str(cause),
        }],
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audit::RecordingSink;
    use ed25519_dalek::{Signer, SigningKey};
    use tairix_abi::{
        manifest::MANIFEST_MAGIC, syscall::SYSCALL_TABLE_HASH_LEN, MANIFEST_MAX_CAPABILITIES,
    };

    fn signing_key() -> SigningKey {
        SigningKey::from_bytes(&[0x42; 32])
    }

    fn authority_for(key: &SigningKey) -> Ed25519PublicKey {
        Ed25519PublicKey::from_bytes(key.verifying_key().as_bytes()).expect("valid key")
    }

    /// Assemble a manifest (header + body) signed by `key` with the given
    /// capability list.
    fn build(key: &SigningKey, caps: &[CapabilityId]) -> alloc::vec::Vec<u8> {
        build_with(key, caps, ABI_VERSION_CURRENT, 0)
    }

    fn build_with(
        key: &SigningKey,
        caps: &[CapabilityId],
        abi_version: u32,
        flags: u32,
    ) -> alloc::vec::Vec<u8> {
        let pub_bytes = *key.verifying_key().as_bytes();
        let mut header = ManifestHeader {
            magic: MANIFEST_MAGIC,
            abi_version,
            flags,
            capability_count: u16::try_from(caps.len()).expect("fits"),
            reserved0: 0,
            syscall_table_hash: [0xAB; SYSCALL_TABLE_HASH_LEN],
            signer_pubkey: pub_bytes,
            signature: [0u8; 64],
        };
        let mut header_bytes = header.to_le_bytes();
        let mut body = alloc::vec::Vec::with_capacity(caps.len() * 2);
        for c in caps {
            body.extend_from_slice(&c.as_u16().to_le_bytes());
        }

        // Sign over signed_range ∥ body.
        let signed_range = ManifestHeader::signed_range();
        let mut signing = alloc::vec::Vec::with_capacity(signed_range.end + body.len());
        signing.extend_from_slice(&header_bytes[signed_range.clone()]);
        signing.extend_from_slice(&body);
        let sig = key.sign(&signing);
        header.signature = sig.to_bytes();
        header_bytes = header.to_le_bytes();

        let mut out = alloc::vec::Vec::with_capacity(header_bytes.len() + body.len());
        out.extend_from_slice(&header_bytes);
        out.extend_from_slice(&body);
        out
    }

    #[test]
    fn verifies_valid_manifest_and_emits_one_event() {
        let key = signing_key();
        let bytes = build(&key, &[CapabilityId::FS_MOUNT, CapabilityId::AUDIT_READ]);
        let sink = RecordingSink::new();
        let v = verify_manifest(&bytes, &authority_for(&key), &sink).expect("ok");
        assert!(v.requested.contains(CapabilityId::FS_MOUNT));
        assert!(v.requested.contains(CapabilityId::AUDIT_READ));
        assert_eq!(sink.ids(), [AuditEvent::ManifestVerified.id().0]);
    }

    #[test]
    fn rejects_bad_magic_with_bad_header_event() {
        let key = signing_key();
        let mut bytes = build(&key, &[]);
        bytes[0] ^= 0xFF;
        let sink = RecordingSink::new();
        assert_eq!(
            verify_manifest(&bytes, &authority_for(&key), &sink),
            Err(Errno::BadMagic)
        );
        assert_eq!(sink.ids(), [AuditEvent::ManifestBadHeader.id().0]);
    }

    #[test]
    fn rejects_short_buffer_with_bad_header_event() {
        let key = signing_key();
        let sink = RecordingSink::new();
        assert_eq!(
            verify_manifest(&[0u8; 16], &authority_for(&key), &sink),
            Err(Errno::BufferTooSmall)
        );
        assert_eq!(sink.ids(), [AuditEvent::ManifestBadHeader.id().0]);
    }

    #[test]
    fn rejects_wrong_abi_version_with_abi_event() {
        let key = signing_key();
        let bytes = build_with(&key, &[], ABI_VERSION_CURRENT + 1, 0);
        let sink = RecordingSink::new();
        assert_eq!(
            verify_manifest(&bytes, &authority_for(&key), &sink),
            Err(Errno::AbiVersionUnsupported)
        );
        assert_eq!(sink.ids(), [AuditEvent::ManifestAbiMismatch.id().0]);
    }

    #[test]
    fn rejects_truncated_body_with_bad_header_event() {
        let key = signing_key();
        let bytes = build(&key, &[CapabilityId::FS_MOUNT]);
        // Drop the body, keeping only the header.
        let truncated = &bytes[..ManifestHeader::WIRE_LEN];
        let sink = RecordingSink::new();
        assert_eq!(
            verify_manifest(truncated, &authority_for(&key), &sink),
            Err(Errno::BufferTooSmall)
        );
        assert_eq!(sink.ids(), [AuditEvent::ManifestBadHeader.id().0]);
    }

    #[test]
    fn rejects_unknown_capability_id() {
        // Craft a manifest whose body contains a capability id the kernel
        // does not know (any value above the `abi-v1` well-known set is
        // currently unknown).
        let key = signing_key();
        let unknown = CapabilityId::from_raw(200).expect("in range");
        let bytes = build(&key, &[unknown]);
        let sink = RecordingSink::new();
        assert_eq!(
            verify_manifest(&bytes, &authority_for(&key), &sink),
            Err(Errno::OutOfRange)
        );
        assert_eq!(sink.ids(), [AuditEvent::ManifestUnknownCapability.id().0]);
    }

    #[test]
    fn rejects_tampered_signature() {
        let key = signing_key();
        let mut bytes = build(&key, &[CapabilityId::NET_RAW]);
        // Flip a byte inside the signature region.
        let sig_offset = ManifestHeader::WIRE_LEN - 64;
        bytes[sig_offset] ^= 0xAA;
        let sink = RecordingSink::new();
        assert_eq!(
            verify_manifest(&bytes, &authority_for(&key), &sink),
            Err(Errno::SignatureInvalid)
        );
        assert_eq!(sink.ids(), [AuditEvent::ManifestSignatureInvalid.id().0]);
    }

    #[test]
    fn rejects_tampered_body_after_signing() {
        let key = signing_key();
        let mut bytes = build(&key, &[CapabilityId::FS_MOUNT]);
        // Mutate the body to a *different known* capability so the
        // unknown-cap check passes but the signature check must fail.
        bytes[ManifestHeader::WIRE_LEN] =
            u8::try_from(CapabilityId::NET_RAW.as_u16()).expect("low byte fits");
        let sink = RecordingSink::new();
        assert_eq!(
            verify_manifest(&bytes, &authority_for(&key), &sink),
            Err(Errno::SignatureInvalid)
        );
        assert_eq!(sink.ids(), [AuditEvent::ManifestSignatureInvalid.id().0]);
    }

    #[test]
    fn rejects_signer_pubkey_swap() {
        // A manifest whose embedded signer_pubkey does not match the
        // kernel's authority is refused even if the signature itself
        // would verify against the embedded key.
        let attacker = SigningKey::from_bytes(&[0x99; 32]);
        let bytes = build(&attacker, &[CapabilityId::FS_MOUNT]);
        let kernel_authority = authority_for(&signing_key());
        let sink = RecordingSink::new();
        assert_eq!(
            verify_manifest(&bytes, &kernel_authority, &sink),
            Err(Errno::SignatureInvalid)
        );
        assert_eq!(sink.ids(), [AuditEvent::ManifestSignatureInvalid.id().0]);
    }

    #[test]
    fn is_known_capability_covers_abi_v1_constants() {
        for cap in [
            CapabilityId::FS_MOUNT,
            CapabilityId::NET_RAW,
            CapabilityId::DRV_LOAD,
            CapabilityId::DRV_KERNEL,
            CapabilityId::USER_ADMIN,
            CapabilityId::TIME_SET,
            CapabilityId::IPC_BIND_PRIVILEGED,
            CapabilityId::AUDIT_READ,
            CapabilityId::AUDIT_WRITE,
            CapabilityId::MEM_DMA,
            CapabilityId::IRQ_BIND,
            CapabilityId::MMIO_MAP,
            CapabilityId::PROC_CONTROL,
        ] {
            assert!(is_known_capability(cap));
        }
        let reserved = CapabilityId::from_raw(50).unwrap();
        assert!(!is_known_capability(reserved));
    }

    #[test]
    fn max_body_does_not_overflow_signing_buffer() {
        // The stack-allocated signing buffer in `verify_manifest` is
        // sized to `MANIFEST_MAX_CAPABILITIES` entries. A manifest at
        // that exact bound must still verify; this test would have
        // caught a too-small buffer.
        let key = signing_key();
        let caps: alloc::vec::Vec<CapabilityId> = (0..MANIFEST_MAX_CAPABILITIES)
            // Repeat a known capability so duplicates don't trip the
            // unknown-cap check.
            .map(|_| CapabilityId::FS_MOUNT)
            .collect();
        let bytes = build(&key, &caps);
        let sink = RecordingSink::new();
        assert!(verify_manifest(&bytes, &authority_for(&key), &sink).is_ok());
    }
}
