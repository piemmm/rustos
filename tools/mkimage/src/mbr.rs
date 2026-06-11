//! MBR partition-table encoding for the authored images.
//!
//! The Raspberry Pi GPU bootloader understands only the classic MBR
//! scheme: it scans the four primary partition entries in sector 0 for a
//! FAT partition and reads the firmware files from it. The encoder here
//! writes exactly that — a partition table plus the `0x55AA` boot
//! signature — with every extent expressed in LBA sectors. The legacy CHS
//! fields are set to the `0xFF` "use LBA" convention modern firmware and
//! kernels expect.

use crate::device::SECTOR_BYTES;
use crate::MkimageError;

/// MBR partition-type byte for a FAT32 partition addressed via LBA.
pub const PART_TYPE_FAT32_LBA: u8 = 0x0c;

/// MBR partition-type byte for the `RustFS` root partition.
///
/// `0x7f` is the IANA-adjacent "reserved for individual or local use"
/// type from the de-facto partition-type registry, the sanctioned value
/// for a filesystem without an assigned identifier. `RustFS` volumes are
/// self-identifying (superblock magic + checksums), so the type byte is a
/// routing hint, not a trusted input.
pub const PART_TYPE_RUSTFS: u8 = 0x7f;

/// One primary-partition extent, in [`SECTOR_BYTES`]-byte LBA sectors.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct PartitionExtent {
    /// MBR partition-type byte ([`PART_TYPE_FAT32_LBA`], …).
    pub type_byte: u8,
    /// First sector of the partition.
    pub start_lba: u32,
    /// Number of sectors in the partition.
    pub sectors: u32,
}

/// Encode `parts` (up to four primary partitions) as a full
/// [`SECTOR_BYTES`]-byte MBR sector.
///
/// Extents must be non-empty, must not cover sector 0, must not overlap,
/// and must not wrap the 32-bit LBA space; violations are authoring
/// defects and fail closed.
///
/// # Errors
///
/// [`MkimageError::PartitionTable`] on any invalid or overlapping extent.
pub fn encode_mbr(parts: &[PartitionExtent]) -> Result<[u8; SECTOR_BYTES], MkimageError> {
    if parts.is_empty() || parts.len() > 4 {
        return Err(MkimageError::PartitionTable(
            "an MBR holds between one and four primary partitions",
        ));
    }
    for (i, p) in parts.iter().enumerate() {
        if p.sectors == 0 {
            return Err(MkimageError::PartitionTable(
                "a partition must be non-empty",
            ));
        }
        if p.start_lba == 0 {
            return Err(MkimageError::PartitionTable(
                "a partition may not cover the MBR sector",
            ));
        }
        if p.start_lba.checked_add(p.sectors).is_none() {
            return Err(MkimageError::PartitionTable(
                "a partition may not wrap the 32-bit LBA space",
            ));
        }
        for q in &parts[..i] {
            let p_end = u64::from(p.start_lba) + u64::from(p.sectors);
            let q_end = u64::from(q.start_lba) + u64::from(q.sectors);
            if u64::from(p.start_lba) < q_end && u64::from(q.start_lba) < p_end {
                return Err(MkimageError::PartitionTable("partitions may not overlap"));
            }
        }
    }

    let mut sector = [0u8; SECTOR_BYTES];
    for (i, p) in parts.iter().enumerate() {
        let entry = &mut sector[446 + i * 16..446 + (i + 1) * 16];
        // Status: 0x00 (inactive). The Pi firmware ignores the bootable
        // flag; nothing in RustOS consumes it.
        entry[0] = 0x00;
        // CHS fields (start: bytes 1-3, end: bytes 5-7): the all-ones
        // "CHS invalid, use LBA" convention.
        entry[1] = 0xff;
        entry[2] = 0xff;
        entry[3] = 0xff;
        entry[4] = p.type_byte;
        entry[5] = 0xff;
        entry[6] = 0xff;
        entry[7] = 0xff;
        entry[8..12].copy_from_slice(&p.start_lba.to_le_bytes());
        entry[12..16].copy_from_slice(&p.sectors.to_le_bytes());
    }
    sector[510] = 0x55;
    sector[511] = 0xaa;
    Ok(sector)
}

#[cfg(test)]
mod tests {
    use super::*;

    const BOOT: PartitionExtent = PartitionExtent {
        type_byte: PART_TYPE_FAT32_LBA,
        start_lba: 2048,
        sectors: 131_072,
    };
    const ROOT: PartitionExtent = PartitionExtent {
        type_byte: PART_TYPE_RUSTFS,
        start_lba: 133_120,
        sectors: 131_072,
    };

    #[test]
    fn encodes_two_partitions_with_signature() {
        let mbr = encode_mbr(&[BOOT, ROOT]).expect("valid layout encodes");
        assert_eq!(mbr[510], 0x55);
        assert_eq!(mbr[511], 0xaa);

        let e1 = &mbr[446..462];
        assert_eq!(e1[4], PART_TYPE_FAT32_LBA);
        assert_eq!(u32::from_le_bytes(e1[8..12].try_into().unwrap()), 2048);
        assert_eq!(u32::from_le_bytes(e1[12..16].try_into().unwrap()), 131_072);

        let e2 = &mbr[446 + 16..446 + 32];
        assert_eq!(e2[4], PART_TYPE_RUSTFS);
        assert_eq!(u32::from_le_bytes(e2[8..12].try_into().unwrap()), 133_120);
        assert_eq!(u32::from_le_bytes(e2[12..16].try_into().unwrap()), 131_072);

        // The unused third and fourth entries stay zeroed.
        assert!(mbr[446 + 32..510].iter().all(|&b| b == 0));
    }

    #[test]
    fn rejects_empty_table_and_too_many_entries() {
        assert!(matches!(
            encode_mbr(&[]),
            Err(MkimageError::PartitionTable(_))
        ));
        assert!(matches!(
            encode_mbr(&[BOOT; 5]),
            Err(MkimageError::PartitionTable(_))
        ));
    }

    #[test]
    fn rejects_zero_length_and_sector_zero_partitions() {
        let empty = PartitionExtent { sectors: 0, ..BOOT };
        assert!(matches!(
            encode_mbr(&[empty]),
            Err(MkimageError::PartitionTable(_))
        ));
        let at_zero = PartitionExtent {
            start_lba: 0,
            ..BOOT
        };
        assert!(matches!(
            encode_mbr(&[at_zero]),
            Err(MkimageError::PartitionTable(_))
        ));
    }

    #[test]
    fn rejects_overlapping_partitions() {
        let overlapping = PartitionExtent {
            type_byte: PART_TYPE_RUSTFS,
            start_lba: BOOT.start_lba + BOOT.sectors - 1,
            sectors: 16,
        };
        assert!(matches!(
            encode_mbr(&[BOOT, overlapping]),
            Err(MkimageError::PartitionTable(_))
        ));
    }

    #[test]
    fn rejects_lba_wraparound() {
        let wrapping = PartitionExtent {
            type_byte: PART_TYPE_RUSTFS,
            start_lba: u32::MAX,
            sectors: 2,
        };
        assert!(matches!(
            encode_mbr(&[wrapping]),
            Err(MkimageError::PartitionTable(_))
        ));
    }
}
