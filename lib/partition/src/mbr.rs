//! Classic MBR (master boot record) partition scheme.
//!
//! An MBR disk carries its table in the first 512-byte sector: four
//! 16-byte primary-partition entries at offset [`PARTITION_TABLE_OFFSET`]
//! and the `0x55AA` signature at the end. LBAs and sector counts are
//! 32-bit, so an MBR addresses at most 2 TiB of 512-byte sectors; past
//! that a disk must use [`crate::gpt`].
//!
//! This module both writes a table (the image author, [`encode`]) and
//! reads one back (the boot path, [`parse`]); both enforce the same
//! extent invariants through one shared validator (`validate_extents`),
//! so a table this module writes is always one it will read back, and a malformed/hostile table is rejected whole at
//! both ends.

use crate::{Partition, PartitionTable, PartitionType};

/// MBR partition-type byte for a FAT32 partition addressed via LBA.
///
/// The platform firmware (e.g. the Raspberry Pi GPU bootloader) scans the
/// four primary entries for a FAT partition and reads the firmware files
/// from it; [`encode`] writes the boot partition with this type.
pub const PART_TYPE_FAT32_LBA: u8 = 0x0c;

/// MBR partition-type byte for a FAT32 partition addressed via CHS, also
/// classified as [`PartitionType::FatBoot`] on parse.
pub const PART_TYPE_FAT32_CHS: u8 = 0x0b;

/// MBR partition-type byte for the encrypted `ARXFS` data-root partition.
///
/// `0x7f` is the de-facto "reserved for individual or local use" type for
/// a filesystem without an assigned identifier. `ARXFS` volumes are
/// self-identifying (superblock magic + checksums), so the type byte is a
/// routing hint, never a trusted input.
pub const PART_TYPE_ARXFS: u8 = 0x7f;

/// MBR partition-type byte for the read-only, signed-bundle `ARXFS`
/// `/System` partition (the design-B pre-unlock store, `plans/PI.md`).
///
/// `0x7e` is a sibling "reserved for individual or local use" type,
/// distinct from [`PART_TYPE_ARXFS`] so the boot path tells the read-only
/// `/System` volume apart from the encrypted data root by role. Like every
/// type byte it is a routing hint, never a trusted input: the volume is
/// still mounted under its own key and its bundles still verified against
/// the load gate's trust anchor.
pub const PART_TYPE_ARXFS_SYSTEM: u8 = 0x7e;

/// A partition entry with this type byte is unused; it is skipped on read
/// rather than treated as a (malformed) zero-length partition.
pub const PART_TYPE_UNUSED: u8 = 0x00;

/// Size of the MBR sector, in bytes.
pub const MBR_SECTOR_LEN: usize = 512;

/// Byte offset of the first 16-byte primary-partition entry in the MBR
/// sector. The 446 bytes before it hold the (unused, on TAIRiX) bootstrap
/// code area.
pub const PARTITION_TABLE_OFFSET: usize = 446;

/// Bytes per primary-partition entry.
pub const PARTITION_ENTRY_LEN: usize = 16;

/// Number of primary-partition entries an MBR holds.
pub const MAX_PRIMARY_PARTITIONS: usize = 4;

/// The two boot-signature bytes at the end of the MBR sector
/// (`0x55`, `0xAA`).
pub const MBR_SIGNATURE: [u8; 2] = [0x55, 0xaa];

/// Why an MBR sector could not be encoded or parsed.
///
/// The same invariants are enforced on both ends (one
/// `validate_extents`); the encoder rejects an authoring defect and the
/// parser rejects a malformed/hostile on-disk table, so every error here
/// can arise from either path.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum MbrError {
    /// The MBR sector buffer is shorter than [`MBR_SECTOR_LEN`].
    ShortSector,
    /// The `0x55AA` boot signature is absent (not an MBR sector).
    BadSignature,
    /// More than [`MAX_PRIMARY_PARTITIONS`] extents were supplied to the
    /// encoder, or more present entries were parsed than fit.
    TooManyPartitions,
    /// No (non-empty) partition was supplied to the encoder, or none was
    /// present in the parsed table.
    NoPartitions,
    /// A present partition has zero length.
    EmptyPartition,
    /// A present partition starts at sector 0 (covering the MBR itself).
    CoversMbrSector,
    /// A partition's `start_lba + sectors` overflows the 32-bit LBA space.
    LbaOverflow,
    /// Two present partitions overlap.
    Overlap,
    /// A [`Partition`] handed to [`encode`] has an LBA or block count that
    /// does not fit MBR's 32-bit fields (use [`crate::gpt`] for such a
    /// disk).
    ExtentTooLarge,
    /// A [`Partition`] handed to [`encode`] has a role no MBR type byte
    /// represents ([`PartitionType::Other`] — a many-to-one
    /// classification): encoding it would silently drop the partition
    /// from the table, so it is refused instead.
    UnrepresentableRole,
}

/// One raw MBR primary-partition extent, in [`MBR_SECTOR_LEN`]-byte LBA
/// sectors.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
struct RawExtent {
    type_byte: u8,
    start_lba: u32,
    sectors: u32,
}

/// Classify an MBR type byte into the scheme-neutral [`PartitionType`].
#[must_use]
pub fn classify(type_byte: u8) -> PartitionType {
    match type_byte {
        PART_TYPE_FAT32_LBA | PART_TYPE_FAT32_CHS => PartitionType::FatBoot,
        PART_TYPE_ARXFS_SYSTEM => PartitionType::ARXFSSystem,
        PART_TYPE_ARXFS => PartitionType::ARXFSRoot,
        _ => PartitionType::Other,
    }
}

/// The MBR type byte [`encode`] writes for a scheme-neutral role, or
/// `None` for [`PartitionType::Other`]: `classify` folds every foreign
/// type byte into `Other`, so no single byte can represent it — and the
/// unused byte would make the entry *absent*, silently dropping the
/// partition from the encoded table. An unrepresentable role is refused
/// at [`encode`] instead (fail closed, never silent data loss).
#[must_use]
pub fn type_byte_for(ty: PartitionType) -> Option<u8> {
    match ty {
        PartitionType::FatBoot => Some(PART_TYPE_FAT32_LBA),
        PartitionType::ARXFSSystem => Some(PART_TYPE_ARXFS_SYSTEM),
        PartitionType::ARXFSRoot => Some(PART_TYPE_ARXFS),
        PartitionType::Other => None,
    }
}

/// Validate raw extents against the shared MBR invariants: between one and
/// [`MAX_PRIMARY_PARTITIONS`] partitions, each non-empty, none covering
/// the MBR sector, none wrapping the 32-bit LBA space, and no two
/// overlapping.
fn validate_extents(parts: &[RawExtent]) -> Result<(), MbrError> {
    if parts.is_empty() {
        return Err(MbrError::NoPartitions);
    }
    if parts.len() > MAX_PRIMARY_PARTITIONS {
        return Err(MbrError::TooManyPartitions);
    }
    for (i, p) in parts.iter().enumerate() {
        if p.sectors == 0 {
            return Err(MbrError::EmptyPartition);
        }
        if p.start_lba == 0 {
            return Err(MbrError::CoversMbrSector);
        }
        if p.start_lba.checked_add(p.sectors).is_none() {
            return Err(MbrError::LbaOverflow);
        }
        let p_start = u64::from(p.start_lba);
        let p_end = p_start + u64::from(p.sectors);
        for q in &parts[..i] {
            let q_start = u64::from(q.start_lba);
            let q_end = q_start + u64::from(q.sectors);
            if p_start < q_end && q_start < p_end {
                return Err(MbrError::Overlap);
            }
        }
    }
    Ok(())
}

/// Convert a scheme-neutral [`Partition`] to a raw MBR extent, failing
/// closed if its 64-bit LBA/length does not fit MBR's 32-bit fields.
fn raw_from_partition(part: &Partition) -> Result<RawExtent, MbrError> {
    let start_lba = u32::try_from(part.start_lba).map_err(|_| MbrError::ExtentTooLarge)?;
    let sectors = u32::try_from(part.block_count).map_err(|_| MbrError::ExtentTooLarge)?;
    let type_byte = type_byte_for(part.ty).ok_or(MbrError::UnrepresentableRole)?;
    Ok(RawExtent {
        type_byte,
        start_lba,
        sectors,
    })
}

/// Encode `parts` (one to [`MAX_PRIMARY_PARTITIONS`] primary partitions)
/// as a full [`MBR_SECTOR_LEN`]-byte MBR sector.
///
/// The legacy CHS fields are set to the `0xFF` "CHS invalid, use LBA"
/// convention modern firmware and kernels expect; the bootable flag is
/// left clear (nothing in TAIRiX consumes it). Extents are validated by
/// `validate_extents` first.
///
/// # Errors
///
/// An [`MbrError`] on any invalid, overlapping, or too-large extent.
pub fn encode(parts: &[Partition]) -> Result<[u8; MBR_SECTOR_LEN], MbrError> {
    if parts.len() > MAX_PRIMARY_PARTITIONS {
        return Err(MbrError::TooManyPartitions);
    }
    let mut raw = [RawExtent {
        type_byte: PART_TYPE_UNUSED,
        start_lba: 0,
        sectors: 0,
    }; MAX_PRIMARY_PARTITIONS];
    for (slot, part) in raw.iter_mut().zip(parts.iter()) {
        *slot = raw_from_partition(part)?;
    }
    let raw = &raw[..parts.len()];
    validate_extents(raw)?;

    let mut sector = [0u8; MBR_SECTOR_LEN];
    for (i, p) in raw.iter().enumerate() {
        let base = PARTITION_TABLE_OFFSET + i * PARTITION_ENTRY_LEN;
        let entry = &mut sector[base..base + PARTITION_ENTRY_LEN];
        // Status byte: 0x00 (inactive); TAIRiX firmware ignores it.
        entry[0] = 0x00;
        // Starting CHS (bytes 1..=3): the all-ones "use LBA" convention.
        entry[1] = 0xff;
        entry[2] = 0xff;
        entry[3] = 0xff;
        entry[4] = p.type_byte;
        // Ending CHS (bytes 5..=7): likewise.
        entry[5] = 0xff;
        entry[6] = 0xff;
        entry[7] = 0xff;
        entry[8..12].copy_from_slice(&p.start_lba.to_le_bytes());
        entry[12..16].copy_from_slice(&p.sectors.to_le_bytes());
    }
    sector[MBR_SECTOR_LEN - 2] = MBR_SIGNATURE[0];
    sector[MBR_SECTOR_LEN - 1] = MBR_SIGNATURE[1];
    Ok(sector)
}

/// `true` if `sector` is at least [`MBR_SECTOR_LEN`] bytes and carries the
/// `0x55AA` boot signature.
#[must_use]
pub fn has_signature(sector: &[u8]) -> bool {
    sector.len() >= MBR_SECTOR_LEN
        && sector[MBR_SECTOR_LEN - 2] == MBR_SIGNATURE[0]
        && sector[MBR_SECTOR_LEN - 1] == MBR_SIGNATURE[1]
}

/// Parse an MBR partition table out of `sector`, fail-closed.
///
/// `sector` is read off an untrusted device, so it is fully validated
/// before any extent is trusted: the buffer
/// must be at least [`MBR_SECTOR_LEN`] bytes and carry the `0x55AA`
/// signature, every entry whose type byte is not [`PART_TYPE_UNUSED`] is
/// collected, and the present extents must satisfy `validate_extents`.
/// A table that violates any invariant is rejected **whole**.
///
/// # Errors
///
/// An [`MbrError`]: [`MbrError::ShortSector`] for a short buffer,
/// [`MbrError::BadSignature`] for a missing signature, or the first
/// extent invariant `validate_extents` rejects.
pub fn parse(sector: &[u8]) -> Result<PartitionTable, MbrError> {
    if sector.len() < MBR_SECTOR_LEN {
        return Err(MbrError::ShortSector);
    }
    if !has_signature(sector) {
        return Err(MbrError::BadSignature);
    }

    let mut raw = [RawExtent {
        type_byte: PART_TYPE_UNUSED,
        start_lba: 0,
        sectors: 0,
    }; MAX_PRIMARY_PARTITIONS];
    let mut len = 0;
    for i in 0..MAX_PRIMARY_PARTITIONS {
        let base = PARTITION_TABLE_OFFSET + i * PARTITION_ENTRY_LEN;
        let type_byte = sector[base + 4];
        if type_byte == PART_TYPE_UNUSED {
            continue;
        }
        let start_lba = u32::from_le_bytes([
            sector[base + 8],
            sector[base + 9],
            sector[base + 10],
            sector[base + 11],
        ]);
        let sectors = u32::from_le_bytes([
            sector[base + 12],
            sector[base + 13],
            sector[base + 14],
            sector[base + 15],
        ]);
        raw[len] = RawExtent {
            type_byte,
            start_lba,
            sectors,
        };
        len += 1;
    }

    let raw = &raw[..len];
    validate_extents(raw)?;

    let mut table = PartitionTable::empty();
    for p in raw {
        // Cannot exceed MAX_PRIMARY_PARTITIONS <= MAX_PARTITIONS.
        table
            .push(Partition {
                ty: classify(p.type_byte),
                start_lba: u64::from(p.start_lba),
                block_count: u64::from(p.sectors),
            })
            .map_err(|_| MbrError::TooManyPartitions)?;
    }
    Ok(table)
}
