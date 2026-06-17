//! GPT (GUID Partition Table) partition scheme — read path.
//!
//! A GPT disk carries a protective MBR in LBA 0, a primary header in LBA
//! 1 (signature `"EFI PART"`), and an array of 128-byte partition entries
//! starting at the LBA the header names. Every field is CRC32-protected.
//! RustOS reads GPT so a UEFI x86_64 disk is a first-class root device
//! (`AGENTS.md` §17 — any scheme on any architecture); the write path
//! lands with the UEFI image builder.
//!
//! Parsing is fail-closed against an untrusted disk (`AGENTS.md` §5.4 /
//! §2.9 / §19.5): the header signature, the header CRC, and the entry-array
//! CRC are all checked before any extent is trusted, and a malformed
//! table is rejected whole.

use crate::{Partition, PartitionError, PartitionTable, PartitionType};
use rustos_abi::driver::block::{Block, BlockGeometry};

/// Largest logical-block size this crate stages a single block of, in
/// bytes. GPT (and MBR) disks use 512- or 4096-byte logical blocks; the
/// boot path reads one block at a time into a buffer of this size.
pub const MAX_BLOCK_SIZE: usize = 4096;

/// GPT header signature: the ASCII bytes `"EFI PART"`.
pub const HEADER_SIGNATURE: [u8; 8] = *b"EFI PART";

/// Bytes in one GPT partition entry.
pub const ENTRY_LEN: usize = 128;

/// Fail-closed cap on the declared partition-entry count an untrusted GPT
/// header may name (`AGENTS.md` §19.5 / §24.4 — a defensive parse bound).
pub const MAX_DECLARED_ENTRIES: u32 = 1024;

/// GPT type GUID of the EFI System Partition (the FAT boot partition),
/// stored in on-disk (mixed-endian) byte order:
/// `C12A7328-F81F-11D2-BA4B-00A0C93EC93B`.
pub const TYPE_GUID_EFI_SYSTEM: [u8; 16] = [
    0x28, 0x73, 0x2a, 0xc1, 0x1f, 0xf8, 0xd2, 0x11, 0xba, 0x4b, 0x00, 0xa0, 0xc9, 0x3e, 0xc9, 0x3b,
];

/// GPT type GUID of the `RustFS` root partition, stored in on-disk
/// (mixed-endian) byte order: `52555354-4653-524F-4F54-000000000001`
/// (`"RUST"`/`"FS"`/`"RO"`/`"OT"` …).
pub const TYPE_GUID_RUSTFS_ROOT: [u8; 16] = [
    0x54, 0x53, 0x55, 0x52, 0x53, 0x46, 0x4f, 0x52, 0x4f, 0x54, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01,
];

/// The all-zero type GUID marks an unused entry.
pub const TYPE_GUID_UNUSED: [u8; 16] = [0u8; 16];

/// Why a GPT table could not be parsed.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum GptError {
    /// The header did not carry the `"EFI PART"` signature.
    BadSignature,
    /// The header's self-CRC did not validate.
    HeaderCrc,
    /// The header declared an entry size other than [`ENTRY_LEN`], or an
    /// entry count past [`MAX_DECLARED_ENTRIES`].
    BadGeometry,
    /// The partition-entry array CRC did not validate.
    EntriesCrc,
    /// A present entry's `first_lba`/`last_lba` is reversed or runs past
    /// the device.
    BadExtent,
    /// More present partitions than the neutral table holds.
    TooManyPartitions,
}

/// IEEE CRC-32 (reflected, polynomial `0xEDB8_8320`), as GPT specifies.
///
/// First-party (`AGENTS.md` §2.12); GPT uses the IEEE polynomial, distinct
/// from the CRC-32C used elsewhere, so it is defined here beside its only
/// consumer rather than shared.
#[must_use]
pub fn crc32(data: &[u8]) -> u32 {
    let mut crc: u32 = 0xffff_ffff;
    for &byte in data {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xedb8_8320 & mask);
        }
    }
    !crc
}

/// Classify a GPT type GUID into the scheme-neutral [`PartitionType`].
#[must_use]
pub fn classify(type_guid: &[u8; 16]) -> PartitionType {
    if *type_guid == TYPE_GUID_EFI_SYSTEM {
        PartitionType::FatBoot
    } else if *type_guid == TYPE_GUID_RUSTFS_ROOT {
        PartitionType::RustFsRoot
    } else {
        PartitionType::Other
    }
}

/// The fields of a validated GPT header the reader needs.
struct Header {
    entries_lba: u64,
    num_entries: u32,
    entry_size: u32,
    entries_crc: u32,
}

/// Parse and CRC-validate a GPT header out of a single logical block.
fn parse_header(block: &[u8]) -> Result<Header, GptError> {
    if block.len() < 92 || block[..8] != HEADER_SIGNATURE {
        return Err(GptError::BadSignature);
    }
    // A GPT header is exactly 92 bytes; reject any other declared size
    // rather than trust bytes outside the region we CRC.
    if rd_u32(block, 12) != 92 {
        return Err(GptError::BadGeometry);
    }
    let stored_crc = rd_u32(block, 16);
    // The header CRC is computed with its own CRC field zeroed.
    let mut hdr = [0u8; 92];
    hdr.copy_from_slice(&block[..92]);
    hdr[16..20].copy_from_slice(&[0, 0, 0, 0]);
    if crc32(&hdr) != stored_crc {
        return Err(GptError::HeaderCrc);
    }

    let entries_lba = rd_u64(block, 72);
    let num_entries = rd_u32(block, 80);
    let entry_size = rd_u32(block, 84);
    let entries_crc = rd_u32(block, 88);
    if entry_size as usize != ENTRY_LEN || num_entries > MAX_DECLARED_ENTRIES {
        return Err(GptError::BadGeometry);
    }
    Ok(Header {
        entries_lba,
        num_entries,
        entry_size,
        entries_crc,
    })
}

/// `true` if `dev` carries a GPT (a valid primary header in LBA 1).
///
/// # Errors
///
/// [`PartitionError::Device`] on a read fault.
pub fn is_gpt_disk<B: Block>(dev: &mut B, geo: &BlockGeometry) -> Result<bool, PartitionError> {
    let bs = geo.block_size as usize;
    if bs == 0 || bs > MAX_BLOCK_SIZE || geo.block_count < 2 {
        return Ok(false);
    }
    let mut block = [0u8; MAX_BLOCK_SIZE];
    let buf = &mut block[..bs];
    dev.read_blocks(1, buf)?;
    Ok(parse_header(buf).is_ok())
}

/// Parse the GPT partition table off `dev`.
///
/// # Errors
///
/// [`PartitionError::Gpt`] for a malformed table or
/// [`PartitionError::Device`] on a read fault.
pub fn parse<B: Block>(dev: &mut B, geo: &BlockGeometry) -> Result<PartitionTable, PartitionError> {
    let bs = geo.block_size as usize;
    if bs == 0 || bs > MAX_BLOCK_SIZE {
        return Err(PartitionError::NoScheme);
    }
    let mut block = [0u8; MAX_BLOCK_SIZE];

    let header = {
        let buf = &mut block[..bs];
        dev.read_blocks(1, buf)?;
        parse_header(buf).map_err(PartitionError::Gpt)?
    };

    // Each logical block must hold at least one entry, or the scan below
    // could not make forward progress (`AGENTS.md` §2.1 — no spin).
    if bs < ENTRY_LEN {
        return Err(PartitionError::Gpt(GptError::BadGeometry));
    }
    let entry_size = header.entry_size as usize;

    let mut table = PartitionTable::empty();
    let mut crc: u32 = 0xffff_ffff;
    let mut parsed = 0u32;
    let mut lba = header.entries_lba;
    while parsed < header.num_entries {
        let buf = &mut block[..bs];
        dev.read_blocks(lba, buf)?;
        let mut off = 0;
        while parsed < header.num_entries && off + ENTRY_LEN <= bs {
            let entry = &buf[off..off + ENTRY_LEN];
            crc = crc32_update(crc, entry);
            collect_entry(entry, geo, &mut table)?;
            off += entry_size;
            parsed += 1;
        }
        lba = lba
            .checked_add(1)
            .ok_or(PartitionError::Gpt(GptError::BadExtent))?;
    }

    // Finalise the running CRC (one's complement) and compare against the
    // value the header committed to.
    if !crc != header.entries_crc {
        return Err(PartitionError::Gpt(GptError::EntriesCrc));
    }
    Ok(table)
}

/// Fold one chunk into a running CRC-32 (reflected `0xEDB8_8320`); finalise
/// by complementing the result.
fn crc32_update(mut crc: u32, data: &[u8]) -> u32 {
    for &byte in data {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xedb8_8320 & mask);
        }
    }
    crc
}

/// Validate one GPT entry and, if present, append it to `table`.
fn collect_entry(
    entry: &[u8],
    geo: &BlockGeometry,
    table: &mut PartitionTable,
) -> Result<(), PartitionError> {
    let mut type_guid = [0u8; 16];
    type_guid.copy_from_slice(&entry[0..16]);
    if type_guid == TYPE_GUID_UNUSED {
        return Ok(());
    }
    let first_lba = rd_u64(entry, 32);
    let last_lba = rd_u64(entry, 40);
    if last_lba < first_lba || last_lba >= geo.block_count {
        return Err(PartitionError::Gpt(GptError::BadExtent));
    }
    let block_count = last_lba - first_lba + 1;
    table
        .push(Partition {
            ty: classify(&type_guid),
            start_lba: first_lba,
            block_count,
        })
        .map_err(|_| PartitionError::Gpt(GptError::TooManyPartitions))
}

fn rd_u32(b: &[u8], off: usize) -> u32 {
    u32::from_le_bytes([b[off], b[off + 1], b[off + 2], b[off + 3]])
}

fn rd_u64(b: &[u8], off: usize) -> u64 {
    u64::from_le_bytes([
        b[off],
        b[off + 1],
        b[off + 2],
        b[off + 3],
        b[off + 4],
        b[off + 5],
        b[off + 6],
        b[off + 7],
    ])
}
