//! Disc record codec and format identification.
//!
//! A new-map volume carries a 60-byte *disc record* describing its
//! geometry and allocation map. On E-class floppies it sits at byte 4 of
//! the first map zone (disc byte 4); on F-class floppies and hard discs a
//! *boot block* at disc byte `0xC00` embeds it at offset `0x1C0`, guarded
//! by an end-around-carry checksum in the block's final byte.
//!
//! An old-map volume has no disc record: it is identified by the
//! checksummed free-space map in sectors 0–1 and the root directory
//! marker (see `oldmap`). Reference: J.G. Harston, "Acorn 8-Bit ADFS
//! Filesystem Structure" (mdfs.net), and the RISC OS PRM `FileCore`
//! formats chapter.

use crate::volume::{get_u16, get_u32, put_u16, put_u32};
use tairix_abi::DriverError;

/// Size of the on-disc disc record in bytes (`u32` form first so the
/// byte form derives from it losslessly).
pub const DISC_RECORD_SIZE_U32: u32 = 60;

/// Size of the on-disc disc record in bytes.
pub const DISC_RECORD_SIZE: usize = DISC_RECORD_SIZE_U32 as usize;

/// Disc byte address of the boot block on F-class and hard-disc volumes.
pub const BOOT_BLOCK_OFFSET: u64 = 0xC00;

/// Offset of the disc record inside the boot block.
pub const DISC_RECORD_IN_BOOT_BLOCK: usize = 0x1C0;

/// Size of the checksummed boot block.
pub const BOOT_BLOCK_SIZE: usize = 512;

/// The reserved "defect" fragment id.
pub const FRAG_BAD: u32 = 1;
/// The fragment id carrying the map itself and the root directory.
pub const FRAG_ROOT: u32 = 2;

/// A decoded new-map disc record.
///
/// Field names follow the RISC OS PRM. Multi-byte fields are
/// little-endian on disc; `nzones` combines the low byte at offset 9 with
/// the high byte at offset 66-in-boot-block terms (offset 0x42 of the
/// record), and `disc_size` combines the 32-bit low word with the 32-bit
/// high word.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct DiscRecord {
    /// Log2 of the sector size in bytes (8, 9, or 10).
    pub log2secsize: u8,
    /// Sectors per track (physical geometry; informational here).
    pub secspertrack: u8,
    /// Head count (physical geometry; informational here).
    pub heads: u8,
    /// Recording density code (physical geometry; informational here).
    pub density: u8,
    /// Fragment-id length in map bits.
    pub idlen: u8,
    /// Log2 of the bytes described by one map bit.
    pub log2bpmb: u8,
    /// Track-to-track sector skew (physical geometry; informational here).
    pub skew: u8,
    /// `*OPT 4` boot option.
    pub bootoption: u8,
    /// Lowest numbered sector id on a track (physical geometry).
    pub lowsector: u8,
    /// Number of map zones (low and high bytes combined).
    pub nzones: u16,
    /// Non-map bits at the end of each zone sector.
    pub zone_spare: u16,
    /// Indirect disc address of the root directory.
    pub root: u32,
    /// Disc size in bytes.
    pub disc_size: u64,
    /// Disc identifier cycled on every mutation.
    pub disc_id: u16,
    /// Disc name, space padded.
    pub disc_name: [u8; 10],
    /// Filetype presented for the disc as a whole (informational).
    pub disc_type: u32,
    /// Log2 of the share granularity, in sectors, for shared fragments.
    pub log2sharesize: u8,
    /// The "big disc" flag (RISC OS 3.6+ addressing).
    pub big_flag: bool,
    /// Directory format: `0` = fixed 2048-byte new directories, `1` =
    /// variable-length big directories (E+/F+).
    pub format_version: u32,
    /// Root directory size in bytes (big directories only).
    pub root_size: u32,
}

impl DiscRecord {
    /// Decode a disc record from its 60-byte on-disc form.
    pub fn parse(raw: &[u8; DISC_RECORD_SIZE]) -> Self {
        Self {
            log2secsize: raw[0],
            secspertrack: raw[1],
            heads: raw[2],
            density: raw[3],
            idlen: raw[4],
            log2bpmb: raw[5],
            skew: raw[6],
            bootoption: raw[7],
            lowsector: raw[8],
            nzones: u16::from(raw[9]) | u16::from(raw[0x2A]) << 8,
            zone_spare: get_u16(raw, 0x0A),
            root: get_u32(raw, 0x0C),
            disc_size: u64::from(get_u32(raw, 0x10)) | u64::from(get_u32(raw, 0x24)) << 32,
            disc_id: get_u16(raw, 0x14),
            disc_name: [
                raw[0x16], raw[0x17], raw[0x18], raw[0x19], raw[0x1A], raw[0x1B], raw[0x1C],
                raw[0x1D], raw[0x1E], raw[0x1F],
            ],
            disc_type: get_u32(raw, 0x20),
            log2sharesize: raw[0x28] & 0x0F,
            big_flag: raw[0x29] & 1 != 0,
            format_version: get_u32(raw, 0x2C),
            root_size: get_u32(raw, 0x30),
        }
    }

    /// Encode the record into its 60-byte on-disc form.
    pub fn encode(&self) -> [u8; DISC_RECORD_SIZE] {
        let mut raw = [0u8; DISC_RECORD_SIZE];
        raw[0] = self.log2secsize;
        raw[1] = self.secspertrack;
        raw[2] = self.heads;
        raw[3] = self.density;
        raw[4] = self.idlen;
        raw[5] = self.log2bpmb;
        raw[6] = self.skew;
        raw[7] = self.bootoption;
        raw[8] = self.lowsector;
        raw[9] = self.nzones.to_le_bytes()[0];
        put_u16(&mut raw, 0x0A, self.zone_spare);
        put_u32(&mut raw, 0x0C, self.root);
        // The low word of the 64-bit size, exactly as stored on disc.
        put_u32(
            &mut raw,
            0x10,
            u32::try_from(self.disc_size & 0xFFFF_FFFF).unwrap_or(0),
        );
        put_u16(&mut raw, 0x14, self.disc_id);
        raw[0x16..0x20].copy_from_slice(&self.disc_name);
        put_u32(&mut raw, 0x20, self.disc_type);
        put_u32(
            &mut raw,
            0x24,
            u32::try_from(self.disc_size >> 32).unwrap_or(0),
        );
        raw[0x28] = self.log2sharesize & 0x0F;
        raw[0x29] = u8::from(self.big_flag);
        raw[0x2A] = self.nzones.to_le_bytes()[1];
        put_u32(&mut raw, 0x2C, self.format_version);
        put_u32(&mut raw, 0x30, self.root_size);
        raw
    }

    /// Structurally validate the record, failing closed on anything a
    /// genuine `FileCore` volume cannot carry.
    ///
    /// # Errors
    ///
    /// [`DriverError::BadMagic`] on any structural violation.
    pub fn validate(&self) -> Result<(), DriverError> {
        // Sector size must be 256, 512, or 1024 bytes.
        if !(8..=10).contains(&self.log2secsize) {
            return Err(DriverError::BadMagic);
        }
        // One map bit covers at least 64 bytes (the F format's value)
        // and at most 64 KiB.
        if !(6..=16).contains(&self.log2bpmb) {
            return Err(DriverError::BadMagic);
        }
        // A fragment id must span at least a sector's worth of map bits
        // plus the three reserved ids, and is bounded by the 3-byte
        // indirect disc address (16 bits) — 19 bits with big directories.
        let max_idlen = if self.format_version != 0 { 19 } else { 16 };
        if self.idlen < self.log2secsize + 3 || self.idlen > max_idlen {
            return Err(DriverError::BadMagic);
        }
        if self.nzones == 0 {
            return Err(DriverError::BadMagic);
        }
        // The map bits of a zone are what remains of its sector after the
        // spare region; a spare consuming the whole sector is malformed.
        let zone_bits = 8u32 << self.log2secsize;
        if u32::from(self.zone_spare) + 32 + u32::from(self.idlen) >= zone_bits {
            return Err(DriverError::BadMagic);
        }
        // Only the two defined directory formats exist.
        if self.format_version > 1 {
            return Err(DriverError::BadMagic);
        }
        if self.disc_size == 0 || self.disc_size % (1u64 << self.log2secsize) != 0 {
            return Err(DriverError::BadMagic);
        }
        // Sector count must be representable in 32 bits.
        if self.disc_size >> self.log2secsize > u64::from(u32::MAX) {
            return Err(DriverError::BadMagic);
        }
        // The root must name a real fragment, never free space or the
        // defect list.
        if self.root >> 8 < FRAG_ROOT {
            return Err(DriverError::BadMagic);
        }
        Ok(())
    }

    /// Bytes described by one map bit.
    pub fn bytes_per_map_bit(&self) -> u64 {
        1u64 << self.log2bpmb
    }

    /// Sector size in bytes.
    pub fn sector_size(&self) -> u64 {
        1u64 << self.log2secsize
    }

    /// Map bits carried by each zone sector.
    pub fn zone_bits(&self) -> u32 {
        (8u32 << self.log2secsize) - u32::from(self.zone_spare)
    }

    /// Fragment ids homed in each zone.
    pub fn ids_per_zone(&self) -> u32 {
        self.zone_bits() / (u32::from(self.idlen) + 1)
    }

    /// Disc byte address of the first map zone.
    ///
    /// The map sits in the middle zone of the disc; on a single-zone
    /// volume that is sector 0.
    pub fn map_offset(&self) -> u64 {
        let half_zones = u64::from(self.nzones >> 1);
        let mut map_bits = half_zones * u64::from(self.zone_bits());
        if self.nzones > 1 {
            map_bits -= (DISC_RECORD_SIZE as u64) * 8;
        }
        map_bits * self.bytes_per_map_bit()
    }

    /// Size of the whole map in bytes (`nzones` sectors).
    pub fn map_size(&self) -> u64 {
        u64::from(self.nzones) * self.sector_size()
    }
}

/// Compute the boot-block checksum: an end-around-carry sum of bytes
/// `0..=510`, stored in byte 511.
///
/// The carry is folded in *before* each addition and the final carry is
/// discarded, matching the ARM `ADC` loop `FileCore` uses (and the Linux
/// `adfs_checkbblk` reference) byte for byte.
pub fn boot_block_checksum(block: &[u8; BOOT_BLOCK_SIZE]) -> u8 {
    let mut sum: u32 = 0;
    for &byte in block[..BOOT_BLOCK_SIZE - 1].iter().rev() {
        sum = (sum & 0xFF) + (sum >> 8);
        sum += u32::from(byte);
    }
    (sum & 0xFF) as u8
}
