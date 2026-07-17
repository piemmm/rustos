//! Inspection view of the TAIRiX `rxe` load image and signed manifest.
//!
//! Both views decode *through* the `lib/abi` wire types — the load image
//! via [`LoadImage::parse_for_inspection`] (every structural load-time
//! invariant, with the CFI tag reported rather than compared, since an
//! inspection tool has no kernel interface hash) and the manifest via
//! [`ManifestHeader::from_bytes`] plus
//! [`tairix_abi::decode_capability_ids`]. There is no second copy of
//! either wire format here, so the view a user reads and the image the
//! kernel loads can never disagree.

use alloc::vec::Vec;

use tairix_abi::{
    decode_capability_ids, CapabilityId, Errno, LoadHeader, LoadImage, ManifestHeader, Segment,
    LOAD_FLAG_PIE, MANIFEST_MAX_CAPABILITIES,
};

pub use tairix_abi::RxeError;

/// A decoded, structurally validated `rxe` load image plus its raw header.
///
/// Holding an `RxeView` is proof the image satisfies every structural
/// load-time invariant (W^X, page alignment, sorted non-overlapping
/// segments, entry inside an executable segment, PIE). The CFI tag is
/// *reported* through [`RxeView::header`], never compared — admitting a
/// binary to execution is the kernel loader's job, not this view's.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RxeView {
    header: LoadHeader,
    image: LoadImage,
}

impl RxeView {
    /// Decode `bytes` as an `rxe` load image for inspection.
    ///
    /// # Errors
    ///
    /// The [`RxeError`] the shared `lib/abi` validator reports for the
    /// first violated invariant; the input is rejected whole.
    pub fn parse(bytes: &[u8]) -> Result<Self, RxeError> {
        let image = LoadImage::parse_for_inspection(bytes)?;
        let header = LoadHeader::from_bytes(bytes)?;
        Ok(Self { header, image })
    }

    /// The raw load header, including the ABI version, flag bits, and the
    /// CFI interface-hash tag the binary was linked against.
    #[must_use]
    pub fn header(&self) -> &LoadHeader {
        &self.header
    }

    /// Image-relative entry-point virtual address.
    #[must_use]
    pub fn entry(&self) -> u64 {
        self.image.entry()
    }

    /// The validated segment table, sorted by ascending virtual address.
    #[must_use]
    pub fn segments(&self) -> &[Segment] {
        self.image.segments()
    }

    /// The shared-library references the image declares, in declaration
    /// order.
    pub fn needed_libraries(&self) -> impl Iterator<Item = &str> {
        self.image.needed_libraries()
    }

    /// Whether the image is position-independent.
    ///
    /// Always `true` for a successfully parsed view (a fixed-address image
    /// is refused), surfaced so a renderer states the property from the
    /// decoded header rather than assuming it.
    #[must_use]
    pub fn is_pie(&self) -> bool {
        self.header.flags & LOAD_FLAG_PIE != 0
    }
}

/// A decoded signed-manifest summary: the header and the capability
/// identifiers the binary requests.
///
/// This summarises what the manifest *says*; it does not verify the
/// Ed25519 signature (verification needs the install's authority key and
/// belongs to the kernel/`appmgr` load gate). The signer public key and
/// signature bytes are surfaced through [`ManifestSummary::header`] so a
/// renderer can display them.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ManifestSummary {
    header: ManifestHeader,
    capabilities: Vec<CapabilityId>,
}

impl ManifestSummary {
    /// Decode `bytes` as a manifest (header plus capability body).
    ///
    /// # Errors
    ///
    /// The [`Errno`] the shared `lib/abi` decoders report: a malformed or
    /// truncated header, a body shorter than the declared count, or an
    /// out-of-range capability identifier. The input is rejected whole.
    pub fn parse(bytes: &[u8]) -> Result<Self, Errno> {
        let header = ManifestHeader::from_bytes(bytes)?;
        let count = usize::from(header.capability_count);
        let body = bytes
            .get(ManifestHeader::WIRE_LEN..)
            .ok_or(Errno::BufferTooSmall)?;
        let mut ids = [CapabilityId::FS_MOUNT; MANIFEST_MAX_CAPABILITIES as usize];
        let written = decode_capability_ids(body, count, &mut ids)?;
        Ok(Self {
            header,
            capabilities: ids[..written].to_vec(),
        })
    }

    /// The raw manifest header (ABI version, flags, syscall-table hash,
    /// signer public key, signature bytes).
    #[must_use]
    pub fn header(&self) -> &ManifestHeader {
        &self.header
    }

    /// The requested capability identifiers, in declaration order.
    #[must_use]
    pub fn capabilities(&self) -> &[CapabilityId] {
        &self.capabilities
    }
}

#[cfg(test)]
mod tests {
    use super::{ManifestSummary, RxeError, RxeView};
    use alloc::vec::Vec;
    use tairix_abi::{
        CapabilityId, Errno, LoadHeader, ManifestHeader, NeededLibrary, Segment, LOAD_FLAG_PIE,
        LOAD_MAGIC, MANIFEST_MAGIC, RXE_PAGE_SIZE,
    };

    /// Encode a minimal valid load image: one RX code segment holding the
    /// entry point, one RW data segment, one needed library.
    fn valid_image() -> Vec<u8> {
        let code = Segment {
            vaddr: RXE_PAGE_SIZE,
            file_offset: 0,
            file_size: 64,
            mem_size: RXE_PAGE_SIZE,
            permission: tairix_abi::RxePermission::ReadExecute,
        };
        let data = Segment {
            vaddr: RXE_PAGE_SIZE * 2,
            file_offset: 64,
            file_size: 32,
            mem_size: RXE_PAGE_SIZE,
            permission: tairix_abi::RxePermission::ReadWrite,
        };
        let needed = NeededLibrary::from_reference("/System/Libraries/libexample.so")
            .expect("valid reference");
        let header = LoadHeader {
            magic: LOAD_MAGIC,
            abi_version: tairix_abi::ABI_VERSION_CURRENT,
            flags: LOAD_FLAG_PIE,
            segment_count: 2,
            needed_count: 1,
            entry: RXE_PAGE_SIZE + 16,
            cfi_tag: [0xA5; 32],
        };
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&header.to_le_bytes());
        bytes.extend_from_slice(&code.to_le_bytes());
        bytes.extend_from_slice(&data.to_le_bytes());
        bytes.extend_from_slice(&needed.to_le_bytes());
        bytes
    }

    /// Encode a manifest with the given capability ids in the body.
    fn manifest_with(ids: &[CapabilityId]) -> Vec<u8> {
        let count = u16::try_from(ids.len()).expect("test counts fit");
        let header = ManifestHeader {
            magic: MANIFEST_MAGIC,
            abi_version: tairix_abi::ABI_VERSION_CURRENT,
            flags: 0,
            capability_count: count,
            reserved0: 0,
            syscall_table_hash: [0x11; 32],
            signer_pubkey: [0x22; 32],
            signature: [0x33; 64],
        };
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&header.to_le_bytes());
        for id in ids {
            bytes.extend_from_slice(&id.as_u16().to_le_bytes());
        }
        bytes
    }

    #[test]
    fn parses_a_valid_image_and_reports_its_properties() {
        let bytes = valid_image();
        let view = RxeView::parse(&bytes).expect("valid image");
        assert!(view.is_pie());
        assert_eq!(view.entry(), RXE_PAGE_SIZE + 16);
        assert_eq!(view.header().cfi_tag, [0xA5; 32]);
        assert_eq!(view.segments().len(), 2);
        assert!(view.segments()[0].permission.is_executable());
        assert!(view.segments()[1].permission.is_writable());
        let needed: Vec<&str> = view.needed_libraries().collect();
        assert_eq!(needed, ["/System/Libraries/libexample.so"]);
    }

    #[test]
    fn every_truncation_of_a_valid_image_fails_closed() {
        let bytes = valid_image();
        for len in 0..bytes.len() {
            assert!(
                RxeView::parse(&bytes[..len]).is_err(),
                "truncation to {len} bytes must be refused"
            );
        }
    }

    #[test]
    fn header_mutations_are_refused_with_named_errors() {
        let good = valid_image();

        let mut bad_magic = good.clone();
        bad_magic[0] ^= 0xFF;
        assert_eq!(RxeView::parse(&bad_magic), Err(RxeError::BadMagic));

        let mut bad_version = good.clone();
        bad_version[4] ^= 0xFF;
        assert_eq!(RxeView::parse(&bad_version), Err(RxeError::BadAbiVersion));

        // Clearing the PIE bit (byte 8 carries the low flag bits).
        let mut non_pie = good.clone();
        non_pie[8] &= !0x01;
        assert_eq!(
            RxeView::parse(&non_pie),
            Err(RxeError::NotPositionIndependent)
        );

        // Entry pointed outside every executable segment (bytes 16..24).
        let mut wild_entry = good;
        wild_entry[16..24].copy_from_slice(&u64::MAX.to_le_bytes());
        assert_eq!(RxeView::parse(&wild_entry), Err(RxeError::BadEntryPoint));
    }

    #[test]
    fn single_byte_flips_never_panic() {
        let good = valid_image();
        for i in 0..good.len() {
            let mut mutated = good.clone();
            mutated[i] ^= 0x40;
            // Any outcome is fine; the decode must simply not panic and
            // a success must yield a self-consistent view.
            if let Ok(view) = RxeView::parse(&mutated) {
                assert!(!view.segments().is_empty());
            }
        }
    }

    #[test]
    fn parses_a_manifest_summary() {
        let bytes = manifest_with(&[CapabilityId::FS_MOUNT, CapabilityId::NET_RAW]);
        let summary = ManifestSummary::parse(&bytes).expect("valid manifest");
        assert_eq!(summary.header().signer_pubkey, [0x22; 32]);
        assert_eq!(
            summary.capabilities(),
            [CapabilityId::FS_MOUNT, CapabilityId::NET_RAW]
        );
    }

    #[test]
    fn parses_an_empty_capability_list() {
        let bytes = manifest_with(&[]);
        let summary = ManifestSummary::parse(&bytes).expect("valid manifest");
        assert!(summary.capabilities().is_empty());
    }

    #[test]
    fn manifest_truncations_fail_closed() {
        let bytes = manifest_with(&[CapabilityId::FS_MOUNT]);
        for len in 0..bytes.len() {
            assert!(
                ManifestSummary::parse(&bytes[..len]).is_err(),
                "truncation to {len} bytes must be refused"
            );
        }
    }

    #[test]
    fn manifest_with_out_of_range_capability_is_refused() {
        let mut bytes = manifest_with(&[CapabilityId::FS_MOUNT]);
        let body = bytes.len() - 2;
        bytes[body..].copy_from_slice(&u16::MAX.to_le_bytes());
        assert_eq!(ManifestSummary::parse(&bytes), Err(Errno::OutOfRange));
    }

    #[test]
    fn manifest_with_bad_magic_is_refused() {
        let mut bytes = manifest_with(&[]);
        bytes[0] ^= 0xFF;
        assert_eq!(ManifestSummary::parse(&bytes), Err(Errno::BadMagic));
    }
}
