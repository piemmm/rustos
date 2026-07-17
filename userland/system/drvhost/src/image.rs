//! `.rxe` envelope splitter — manifest header, capability body, bind
//! table, payload.
//!
//! A `.rxe` image is laid out as:
//!
//! ```text
//! ┌─────────────────────────────────┬───────────────────────┬────────────────────────┬─────────┐
//! │ DriverManifest header (WIRE_LEN B) │ cap body (count·2 B)  │ bind table (count·80 B) │ payload …│
//! └─────────────────────────────────┴───────────────────────┴────────────────────────┴─────────┘
//!   covered by signature                 covered by signature    covered by signature       opaque
//! ```
//!
//! The header layout and signed range are pinned by
//! [`tairix_abi::DriverManifest`]. The body that follows the header is a
//! little-endian array of [`tairix_abi::CapabilityId`] u16 values whose
//! length is the manifest's `capability_count` field, followed by the
//! driver's bind table: `bind_key_count` consecutive
//! [`tairix_abi::DriverBindKey`] records. Everything
//! beyond that is opaque payload (in production the program half of the
//! binary the [`crate::DriverSpawner`] spawns; in tests, an arbitrary
//! `&[u8]`).
//!
//! [`ParsedImage`] is a borrow-only view: it never copies the underlying
//! bytes, so the caller controls the lifetime (and the wipe) of the
//! sensitive buffer that backs it.

use tairix_abi::{
    decode_bind_keys, CapabilityId, DriverBindKey, DriverError, DriverManifest, Errno,
    DRIVER_SIGNATURE_LEN,
};

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
    /// The header bytes the signer covered (the manifest sans its
    /// signature tail). The full signed message is this slice followed
    /// by [`Self::capability_body`] and [`Self::bind_table`]; the host
    /// concatenates the three at verification time.
    pub signed_bytes: &'a [u8],
    /// Capability body as raw little-endian `u16` pairs (length =
    /// `manifest.capability_count * 2`).
    pub capability_body: &'a [u8],
    /// Bind table as raw little-endian [`DriverBindKey`] records
    /// (length = `manifest.bind_key_count * DriverBindKey::WIRE_LEN`).
    pub bind_table: &'a [u8],
    /// Opaque trailing bytes consumed by the host's
    /// [`crate::DriverSpawner`].
    pub payload: &'a [u8],
}

impl<'a> ParsedImage<'a> {
    /// Header length consumed at the start of every `.rxe` image. Equal
    /// to [`DriverManifest::WIRE_LEN`]; re-exposed at this scope so
    /// callers do not have to import the constant separately.
    pub const HEADER_LEN: usize = DriverManifest::WIRE_LEN;

    /// Decode `bytes` into header + capability body + bind table +
    /// payload.
    ///
    /// Returns:
    ///
    /// * [`HostError::ImageTruncated`] if `bytes` is shorter than the
    ///   header, or shorter than `HEADER_LEN + capability_count * 2 +
    ///   bind_key_count * DriverBindKey::WIRE_LEN`.
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
        let bind_len = usize::from(manifest.bind_key_count) * DriverBindKey::WIRE_LEN;
        let bind_end = body_end
            .checked_add(bind_len)
            .ok_or(HostError::ImageTruncated)?;
        if bytes.len() < bind_end {
            return Err(HostError::ImageTruncated);
        }
        let signed_end = Self::HEADER_LEN - DRIVER_SIGNATURE_LEN;
        // The signer's message is header[0..WIRE_LEN-SIG_LEN] followed
        // by the capability body and the bind table. The halves are
        // contiguous in the source slice so a single sub-slice of bytes
        // covering [0..signed_end] would *not* be enough — we must hand
        // the verifier the concatenation. Concatenation is performed
        // once at verification time by the host (see
        // `Host::verify_signature`) so the parser stays allocation-free.
        Ok(Self {
            manifest,
            signed_bytes: &bytes[..signed_end],
            capability_body: &bytes[Self::HEADER_LEN..body_end],
            bind_table: &bytes[body_end..bind_end],
            payload: &bytes[bind_end..],
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
    ///   [`CAPABILITY_ID_MAX`](tairix_abi::CAPABILITY_ID_MAX).
    /// * [`HostError::ImageTruncated`] if `out` cannot hold
    ///   `capability_count` identifiers (the caller sized the buffer
    ///   too small).
    pub fn decode_capabilities(&self, out: &mut [CapabilityId]) -> Result<usize, HostError> {
        let count = usize::from(self.manifest.capability_count);
        tairix_abi::decode_capability_ids(self.capability_body, count, out).map_err(|e| match e {
            Errno::OutOfRange => HostError::CapabilityOutOfRange,
            _ => HostError::ImageTruncated,
        })
    }

    /// Decode the bind table into a fixed-size buffer of
    /// [`DriverBindKey`] entries, validating every record fail-closed.
    ///
    /// Returns the number of entries decoded; entries beyond the
    /// returned length are not written.
    ///
    /// # Errors
    ///
    /// * [`HostError::BindKeyInvalid`] if any entry fails the
    ///   [`DriverBindKey::from_bytes`] validation (non-zero reserved
    ///   field, unknown match-key kind, out-of-bounds `compatible`
    ///   length).
    /// * [`HostError::ImageTruncated`] if `out` cannot hold
    ///   `bind_key_count` entries (the caller sized the buffer too
    ///   small).
    pub fn decode_bind_table(&self, out: &mut [DriverBindKey]) -> Result<usize, HostError> {
        let count = usize::from(self.manifest.bind_key_count);
        decode_bind_keys(self.bind_table, count, out).map_err(|e| match e {
            DriverError::BufferTooSmall => HostError::ImageTruncated,
            other => HostError::BindKeyInvalid(other),
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
    use tairix_abi::{DriverKind, CAPABILITY_ID_MAX, DRIVER_MANIFEST_MAGIC};

    extern crate alloc;

    fn build_image(caps: &[u16], payload: &[u8]) -> Vec<u8> {
        build_image_with_bind_keys(caps, &[], payload)
    }

    fn build_image_with_bind_keys(
        caps: &[u16],
        bind_keys: &[DriverBindKey],
        payload: &[u8],
    ) -> Vec<u8> {
        let count = u16::try_from(caps.len()).expect("test caps fit in u16");
        let bind_key_count = u8::try_from(bind_keys.len()).expect("test bind keys fit in u8");
        let m = DriverManifest {
            magic: DRIVER_MANIFEST_MAGIC,
            abi_version: tairix_abi::ABI_VERSION_CURRENT,
            kind: DriverKind::UserSpace,
            bind_key_count,
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
        for k in bind_keys {
            out.extend_from_slice(&k.to_le_bytes());
        }
        out.extend_from_slice(payload);
        out
    }

    fn sample_bind_keys() -> [DriverBindKey; 2] {
        let Ok(key) = tairix_abi::HwMatchKey::compatible(b"brcm,bcm2711-emmc2") else {
            unreachable!("compatible string fits HW_COMPATIBLE_MAX")
        };
        [
            DriverBindKey::new(10, key),
            DriverBindKey::new(0, tairix_abi::HwMatchKey::virtio(2)),
        ]
    }

    #[test]
    fn round_trip_header_body_payload() {
        let img = build_image(&[1, 3, 4], b"\xDE\xAD\xBE\xEF");
        let parsed = ParsedImage::parse(&img).expect("valid");
        assert_eq!(parsed.manifest.capability_count, 3);
        assert_eq!(parsed.capability_body.len(), 6);
        assert!(parsed.bind_table.is_empty());
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
        assert!(parsed.bind_table.is_empty());
        assert!(parsed.payload.is_empty());
    }

    #[test]
    fn round_trip_bind_table() {
        let keys = sample_bind_keys();
        let img = build_image_with_bind_keys(&[1], &keys, b"\x01\x02");
        let parsed = ParsedImage::parse(&img).expect("valid");
        assert_eq!(parsed.manifest.bind_key_count, 2);
        assert_eq!(parsed.bind_table.len(), 2 * DriverBindKey::WIRE_LEN);
        assert_eq!(parsed.payload, b"\x01\x02");
        let mut out = [DriverBindKey::new(0, tairix_abi::HwMatchKey::virtio(0)); 4];
        let n = parsed.decode_bind_table(&mut out).expect("decode");
        assert_eq!(n, 2);
        assert_eq!(&out[..2], &keys);
    }

    #[test]
    fn truncated_bind_table_is_rejected() {
        let keys = sample_bind_keys();
        let img = build_image_with_bind_keys(&[1], &keys, b"");
        let short = &img[..img.len() - 1];
        assert!(matches!(
            ParsedImage::parse(short),
            Err(HostError::ImageTruncated)
        ));
    }

    #[test]
    fn invalid_bind_key_fails_closed() {
        let keys = sample_bind_keys();
        let mut img = build_image_with_bind_keys(&[1], &keys, b"");
        // Corrupt the first bind entry's reserved field.
        let bind_start = ParsedImage::HEADER_LEN + 2;
        img[bind_start + 2] = 1;
        let parsed = ParsedImage::parse(&img).expect("structurally valid");
        let mut out = [DriverBindKey::new(0, tairix_abi::HwMatchKey::virtio(0)); 4];
        assert_eq!(
            parsed.decode_bind_table(&mut out),
            Err(HostError::BindKeyInvalid(DriverError::BadMagic))
        );
    }

    #[test]
    fn bind_table_decode_into_too_small_buffer_returns_truncated() {
        let keys = sample_bind_keys();
        let img = build_image_with_bind_keys(&[], &keys, b"");
        let parsed = ParsedImage::parse(&img).expect("valid");
        let mut out = [DriverBindKey::new(0, tairix_abi::HwMatchKey::virtio(0)); 1];
        assert_eq!(
            parsed.decode_bind_table(&mut out),
            Err(HostError::ImageTruncated)
        );
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
