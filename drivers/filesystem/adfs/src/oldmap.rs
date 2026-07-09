//! Old-map free-space engine (ADFS S/M/L/D and old-map hard discs).
//!
//! The first two 256-byte sectors of an old-map volume hold the
//! free-space map: sector 0 lists the start sector of each free area,
//! sector 1 the matching lengths, and each sector ends in an
//! add-with-carry checksum byte. Every object on an old-map volume is a
//! single contiguous run of 256-byte sectors, so allocation is a
//! first-fit search of this list and freeing merges the released run
//! back in. Reference: J.G. Harston, "Acorn 8-Bit ADFS Filesystem
//! Structure" (mdfs.net).
//!
//! The engine keeps the raw 512-byte map image and edits only the fields
//! it owns — the free-area entries, the end pointer, and the checksums —
//! so the disc name, boot option, disc id, and the Level 3 fileserver
//! partition pointers round-trip byte-exact.

use crate::volume::{get_u24, put_u24, Volume};
use rustos_abi::driver::block::Block;
use rustos_abi::DriverError;

/// Old-map logical sector size in bytes, fixed by the format (`u32`
/// form first so the wide form derives from it losslessly).
pub const OLD_SECTOR_SIZE_U32: u32 = 256;

/// Old-map logical sector size in bytes, fixed by the format.
pub const OLD_SECTOR_SIZE: u64 = OLD_SECTOR_SIZE_U32 as u64;

/// Size of the raw two-sector map image.
pub const OLD_MAP_SIZE: usize = 512;

/// Maximum number of free-area entries the map can hold (82 three-byte
/// entries fill bytes `0x00..0xF6`).
pub const MAX_FREE_AREAS: usize = 82;

/// Byte offset of the free-space end pointer within the map image.
const END_POINTER: usize = 0x1FE;

/// Byte offset of the total-sector count within the map image.
const DISC_SECTORS: usize = 0x0FC;

/// Bits 29–31 of a start or length entry must be zero; the top three
/// bits of the 8-bit API's sector addresses selected the drive.
const ENTRY_LIMIT: u32 = 1 << 29;

/// The old-map free-space engine.
pub struct OldMap {
    raw: [u8; OLD_MAP_SIZE],
    disc_sectors: u32,
}

impl OldMap {
    /// Read and validate the map from sectors 0–1 of `volume`.
    ///
    /// # Errors
    ///
    /// * [`DriverError::BadMagic`] if either checksum fails, the end
    ///   pointer or an entry is structurally invalid, or the recorded
    ///   disc size does not fit the device.
    /// * [`DriverError::DeviceFault`] on an unrecoverable block read.
    pub fn load<B: Block>(volume: &mut Volume<B>) -> Result<Self, DriverError> {
        let mut raw = [0u8; OLD_MAP_SIZE];
        volume.read_bytes(0, &mut raw)?;
        let map = Self {
            disc_sectors: get_u24(&raw, DISC_SECTORS),
            raw,
        };
        map.validate(volume.device_bytes())?;
        Ok(map)
    }

    /// Validate the raw map image (checksums, end pointer, entries).
    fn validate(&self, device_bytes: u64) -> Result<(), DriverError> {
        let (sector0, sector1) = self.raw.split_at(256);
        if old_map_checksum(sector0) != sector0[255] || old_map_checksum(sector1) != sector1[255] {
            return Err(DriverError::BadMagic);
        }
        if self.disc_sectors == 0
            || u64::from(self.disc_sectors) * OLD_SECTOR_SIZE > device_bytes
            || self.disc_sectors >= ENTRY_LIMIT
        {
            return Err(DriverError::BadMagic);
        }
        let end = usize::from(self.raw[END_POINTER]);
        if end % 3 != 0 || end > MAX_FREE_AREAS * 3 {
            return Err(DriverError::BadMagic);
        }
        for index in 0..end / 3 {
            let (start, len) = self.entry(index);
            if start >= ENTRY_LIMIT || len >= ENTRY_LIMIT {
                return Err(DriverError::BadMagic);
            }
            if len == 0
                || start
                    .checked_add(len)
                    .map_or(true, |e| e > self.disc_sectors)
            {
                return Err(DriverError::BadMagic);
            }
        }
        Ok(())
    }

    /// Write the map back to sectors 0–1 of `volume`, refreshing both
    /// checksums.
    ///
    /// # Errors
    ///
    /// * [`DriverError::DeviceFault`] on an unrecoverable block write.
    pub fn store<B: Block>(&mut self, volume: &mut Volume<B>) -> Result<(), DriverError> {
        {
            let (sector0, sector1) = self.raw.split_at_mut(256);
            sector0[255] = old_map_checksum(sector0);
            sector1[255] = old_map_checksum(sector1);
        }
        volume.write_bytes(0, &self.raw)
    }

    /// Total sectors on the volume.
    pub fn disc_sectors(&self) -> u32 {
        self.disc_sectors
    }

    /// Number of live free-area entries.
    fn area_count(&self) -> usize {
        usize::from(self.raw[END_POINTER]) / 3
    }

    /// The `index`-th free area as `(start_sector, sector_count)`.
    fn entry(&self, index: usize) -> (u32, u32) {
        (
            get_u24(&self.raw[..256], index * 3),
            get_u24(&self.raw[256..], index * 3),
        )
    }

    fn set_entry(&mut self, index: usize, start: u32, len: u32) {
        put_u24(&mut self.raw[..256], index * 3, start);
        put_u24(&mut self.raw[256..512], index * 3, len);
    }

    /// Remove the `index`-th free area, shifting the tail down.
    fn remove_entry(&mut self, index: usize) {
        let count = self.area_count();
        for i in index..count - 1 {
            let (start, len) = self.entry(i + 1);
            self.set_entry(i, start, len);
        }
        self.set_entry(count - 1, 0, 0);
        // At most 82 areas, so the pointer byte never truncates.
        self.raw[END_POINTER] = u8::try_from((count - 1) * 3).unwrap_or(0);
    }

    /// Insert a free area at `index`, shifting the tail up. The caller
    /// has verified the map is not full.
    fn insert_entry(&mut self, index: usize, start: u32, len: u32) {
        let count = self.area_count();
        let mut i = count;
        while i > index {
            let (s, l) = self.entry(i - 1);
            self.set_entry(i, s, l);
            i -= 1;
        }
        self.set_entry(index, start, len);
        // At most 82 areas, so the pointer byte never truncates.
        self.raw[END_POINTER] = u8::try_from((count + 1) * 3).unwrap_or(0);
    }

    /// Allocate `sectors` contiguous sectors, first-fit, returning the
    /// start sector.
    ///
    /// # Errors
    ///
    /// * [`DriverError::NoSpace`] if no free area is large enough.
    pub fn allocate(&mut self, sectors: u32) -> Result<u32, DriverError> {
        if sectors == 0 {
            return Err(DriverError::NoSpace);
        }
        for index in 0..self.area_count() {
            let (start, len) = self.entry(index);
            if len >= sectors {
                if len == sectors {
                    self.remove_entry(index);
                } else {
                    self.set_entry(index, start + sectors, len - sectors);
                }
                return Ok(start);
            }
        }
        Err(DriverError::NoSpace)
    }

    /// Grow an allocation ending at `end_sector` by `extra` sectors if a
    /// free area starts exactly there, consuming the space. Returns
    /// whether the in-place extension happened.
    pub fn try_extend(&mut self, end_sector: u32, extra: u32) -> bool {
        if extra == 0 {
            return true;
        }
        for index in 0..self.area_count() {
            let (start, len) = self.entry(index);
            if start == end_sector && len >= extra {
                if len == extra {
                    self.remove_entry(index);
                } else {
                    self.set_entry(index, start + extra, len - extra);
                }
                return true;
            }
        }
        false
    }

    /// Return the run `[start, start + sectors)` to the free list,
    /// merging with adjacent free areas.
    ///
    /// # Errors
    ///
    /// * [`DriverError::NoSpace`] if the freed run cannot be merged and
    ///   the map already holds its maximum number of areas (the format's
    ///   "compaction required" condition).
    /// * [`DriverError::BadMagic`] if the run overlaps an existing free
    ///   area or leaves the disc — the caller handed back space the map
    ///   says is already free, which means the volume is corrupt.
    pub fn free_span(&mut self, start: u32, sectors: u32) -> Result<(), DriverError> {
        if sectors == 0 {
            return Ok(());
        }
        let end = start.checked_add(sectors).ok_or(DriverError::BadMagic)?;
        if end > self.disc_sectors {
            return Err(DriverError::BadMagic);
        }
        // Find the insertion point keeping the list sorted by start.
        let count = self.area_count();
        let mut index = 0;
        while index < count && self.entry(index).0 < start {
            index += 1;
        }
        // Overlap with either neighbour is corruption, not a merge.
        if index > 0 {
            let (prev_start, prev_len) = self.entry(index - 1);
            if prev_start + prev_len > start {
                return Err(DriverError::BadMagic);
            }
        }
        if index < count && end > self.entry(index).0 {
            return Err(DriverError::BadMagic);
        }
        let merges_prev = index > 0 && {
            let (prev_start, prev_len) = self.entry(index - 1);
            prev_start + prev_len == start
        };
        let merges_next = index < count && self.entry(index).0 == end;
        match (merges_prev, merges_next) {
            (true, true) => {
                let (prev_start, prev_len) = self.entry(index - 1);
                let (_, next_len) = self.entry(index);
                self.set_entry(index - 1, prev_start, prev_len + sectors + next_len);
                self.remove_entry(index);
            }
            (true, false) => {
                let (prev_start, prev_len) = self.entry(index - 1);
                self.set_entry(index - 1, prev_start, prev_len + sectors);
            }
            (false, true) => {
                let (_, next_len) = self.entry(index);
                self.set_entry(index, start, next_len + sectors);
            }
            (false, false) => {
                if count == MAX_FREE_AREAS {
                    return Err(DriverError::NoSpace);
                }
                self.insert_entry(index, start, sectors);
            }
        }
        Ok(())
    }

    /// Total free sectors.
    pub fn free_sectors(&self) -> u64 {
        let mut total = 0u64;
        for index in 0..self.area_count() {
            total += u64::from(self.entry(index).1);
        }
        total
    }

    /// Build a fresh map image for a volume of `disc_sectors` sectors
    /// whose free space is the single run `[first_free, disc_sectors)`.
    pub fn initialise(disc_sectors: u32, first_free: u32, disc_id: u16) -> Self {
        let mut map = Self {
            raw: [0u8; OLD_MAP_SIZE],
            disc_sectors,
        };
        put_u24(&mut map.raw[..256], 0, first_free);
        put_u24(&mut map.raw[256..512], 0, disc_sectors - first_free);
        put_u24(&mut map.raw, DISC_SECTORS, disc_sectors);
        map.raw[0x1FB] = disc_id.to_le_bytes()[0];
        map.raw[0x1FC] = disc_id.to_le_bytes()[1];
        map.raw[END_POINTER] = 3;
        map
    }
}

/// The old-map sector checksum: start from 255 and add each byte from
/// offset 254 down to 0, folding the carry in *before* each addition.
fn old_map_checksum(sector: &[u8]) -> u8 {
    let mut sum: u32 = 255;
    for &byte in sector[..255].iter().rev() {
        if sum > 255 {
            sum = (sum + 1) & 0xFF;
        }
        sum += u32::from(byte);
    }
    (sum & 0xFF) as u8
}
