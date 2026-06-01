//! `.rxe` envelope splitter — manifest header, capability body, payload.
//!
//! A `.rxe` image is laid out as:
//!
//! ```text
//! ┌──────────────────────────────────────┬───────────────────────────────┬─────────────┐
//! │ DriverManifest header (WIRE_LEN B)   │ capability body (count·2 B)   │ payload …   │
//! └──────────────────────────────────────┴───────────────────────────────┴─────────────┘
//!   covered by signature                   covered by signature             opaque
//! ```
//!
//! The header layout and signed range are pinned by
//! [`rustos_abi::DriverManifest`]. The body that follows the header is a
//! little-endian array of [`rustos_abi::CapabilityId`] u16 values whose
//! length is the manifest's `capability_count` field. Everything beyond
//! that is opaque payload (in production the ELF half of the binary; in
//! tests, an arbitrary `&[u8]` keyed by the [`crate::EntryResolver`]).
//!
//! [`ParsedImage`] is a borrow-only view: it never copies the underlying
//! bytes, so the caller controls the lifetime (and the wipe) of the
//! sensitive buffer that backs it.

use rustos_abi::{CapabilityId, DriverError, DriverManifest, Errno, DRIVER_SIGNATURE_LEN};

use crate::HostError;

/// Parsed but **not yet verified** view of a `.rxe` image.
///
/// The decode is purely structural (lengths, magic, abi version, kind
/// byte). Signature verification, syscall-hash matching, and capability
/// gating happen in [`crate::host`] against the borrowed slices below.
#[derive(Debug)]
pub struct ParsedImage<'a> {
    /// Decoded manifest header.
    pub manifest: DriverManifest,
    /// Bytes the signer covered: header (sans signature) + capability
    /// body. Pass this as the message argument to
    /// [`rustos_crypto::Ed25519PublicKey::verify`].
    pub signed_bytes: &'a [u8],
    /// Capability body as raw little-endian `u16` pairs (length =
    /// `manifest.capability_count * 2`).
    pub capability_body: &'a [u8],
    /// Opaque trailing bytes consumed by the host's
    /// [`crate::EntryResolver`].
    pub payload: &'a [u8],
}

impl<'a> ParsedImage<'a> {
    /// Header length consumed at the start of every `.rxe` image. Equal
    /// to [`DriverManifest::WIRE_LEN`]; re-exposed at this scope so
    /// callers do not have to import the constant separately.
    pub const HEADER_LEN: usize = DriverManifest::WIRE_LEN;

    /// Decode `bytes` into header + capability body + payload.
    ///
    /// Returns:
    ///
    /// * [`HostError::ImageTruncated`] if `bytes` is shorter than the
    ///   header, or shorter than `HEADER_LEN + capability_count * 2`.
    /// * [`HostError::ManifestInvalid`] if [`DriverManifest::from_bytes`]
    ///   rejects the header.
    pub fn parse(bytes: &'a [u8]) -> Result<Self, HostError> {
        if bytes.len() < Self::HEADER_LEN {
            return Err(HostError::ImageTruncated);
        }
        let manifest =
            DriverManifest::from_bytes(&bytes[..Self::HEADER_LEN]).map_err(map_manifest_err)?;
        let body_len = usize::from(manifest.capability_count) * 2;
        let body_end = Self::HEADER_LEN
            .checked_add(body_len)
            .ok_or(HostError::ImageTruncated)?;
        if bytes.len() < body_end {
            return Err(HostError::ImageTruncated);
        }
        let signed_end = Self::HEADER_LEN - DRIVER_SIGNATURE_LEN;
        // The signer's message is header[0..WIRE_LEN-SIG_LEN] followed
        // by the capability body. Both halves are contiguous in the
        // source slice so a single sub-slice of bytes covering
        // [0..signed_end] would *not* be enough — we must hand the
        // verifier the concatenation. Concatenation is performed once
        // at verification time by the host (see `verify::signed_message`)
        // so the parser stays allocation-free.
        Ok(Self {
            manifest,
            signed_bytes: &bytes[..signed_end],
            capability_body: &bytes[Self::HEADER_LEN..body_end],
            payload: &bytes[body_end..],
        })
    }

    /// Decode the capability body into a fixed-size buffer of
    /// [`CapabilityId`] values, validating that every identifier is in
    /// range.
    ///
    /// Returns the number of identifiers decoded; entries beyond the
    /// returned length are not written.
    ///
    /// # Errors
    ///
    /// * [`HostError::CapabilityOutOfRange`] if any identifier exceeds
    ///   [`CAPABILITY_ID_MAX`](rustos_abi::CAPABILITY_ID_MAX).
    /// * [`HostError::ImageTruncated`] if `out` cannot hold
    ///   `capability_count` identifiers (the caller sized the buffer
    ///   too small).
    pub fn decode_capabilities(&self, out: &mut [CapabilityId]) -> Result<usize, HostError> {
        let count = usize::from(self.manifest.capability_count);
        rustos_abi::decode_capability_ids(self.capability_body, count, out).map_err(|e| match e {
            Errno::OutOfRange => HostError::CapabilityOutOfRange,
            _ => HostError::ImageTruncated,
        })
    }
}

fn map_manifest_err(e: DriverError) -> HostError {
    HostError::ManifestInvalid(e)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;
    use alloc::vec::Vec;
    use rustos_abi::{DriverKind, CAPABILITY_ID_MAX, DRIVER_MANIFEST_MAGIC};

    extern crate alloc;

    fn build_image(caps: &[u16], payload: &[u8]) -> Vec<u8> {
        let count = u16::try_from(caps.len()).expect("test caps fit in u16");
        let m = DriverManifest {
            magic: DRIVER_MANIFEST_MAGIC,
            abi_version: rustos_abi::ABI_VERSION_CURRENT,
            kind: DriverKind::UserSpace,
            reserved0: 0,
            capability_count: count,
            syscall_table_hash: [0u8; 32],
            signer_pubkey: [0u8; 32],
            signature: [0u8; 64],
        };
        let mut out = Vec::new();
        out.extend_from_slice(&m.to_le_bytes());
        for c in caps {
            out.extend_from_slice(&c.to_le_bytes());
        }
        out.extend_from_slice(payload);
        out
    }

    #[test]
    fn round_trip_header_body_payload() {
        let img = build_image(&[1, 3, 4], b"\xDE\xAD\xBE\xEF");
        let parsed = ParsedImage::parse(&img).expect("valid");
        assert_eq!(parsed.manifest.capability_count, 3);
        assert_eq!(parsed.capability_body.len(), 6);
        assert_eq!(parsed.payload, b"\xDE\xAD\xBE\xEF");
        assert_eq!(parsed.signed_bytes.len(), DriverManifest::WIRE_LEN - 64,);
        let mut caps = [CapabilityId::from_raw(0).unwrap(); 8];
        let n = parsed.decode_capabilities(&mut caps).expect("decode");
        assert_eq!(n, 3);
        assert_eq!(
            [caps[0], caps[1], caps[2]],
            [
                CapabilityId::FS_MOUNT,
                CapabilityId::DRV_LOAD,
                CapabilityId::DRV_KERNEL
            ],
        );
    }

    #[test]
    fn truncated_image_is_rejected() {
        let img = build_image(&[1, 2], b"");
        // Strip the last capability body byte.
        let short = &img[..img.len() - 1];
        assert!(matches!(
            ParsedImage::parse(short),
            Err(HostError::ImageTruncated)
        ));
    }

    #[test]
    fn header_shorter_than_wire_len_rejected() {
        let buf = vec![0u8; 8];
        assert!(matches!(
            ParsedImage::parse(&buf),
            Err(HostError::ImageTruncated)
        ));
    }

    #[test]
    fn bad_magic_surfaces_manifest_invalid() {
        let mut img = build_image(&[1], b"");
        img[0] ^= 0xFF;
        match ParsedImage::parse(&img) {
            Err(HostError::ManifestInvalid(_)) => {}
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn payload_can_be_empty() {
        let img = build_image(&[], b"");
        let parsed = ParsedImage::parse(&img).expect("valid");
        assert!(parsed.capability_body.is_empty());
        assert!(parsed.payload.is_empty());
    }

    #[test]
    fn decode_into_too_small_buffer_returns_truncated() {
        let img = build_image(&[1, 2, 3], b"");
        let parsed = ParsedImage::parse(&img).expect("valid");
        let mut caps = [CapabilityId::from_raw(0).unwrap(); 1];
        assert_eq!(
            parsed.decode_capabilities(&mut caps),
            Err(HostError::ImageTruncated)
        );
    }

    #[test]
    fn decode_rejects_out_of_range_capability_id() {
        let img = build_image(&[CAPABILITY_ID_MAX + 1], b"");
        let parsed = ParsedImage::parse(&img).expect("valid");
        let mut caps = [CapabilityId::from_raw(0).unwrap(); 4];
        assert_eq!(
            parsed.decode_capabilities(&mut caps),
            Err(HostError::CapabilityOutOfRange)
        );
    }
}
