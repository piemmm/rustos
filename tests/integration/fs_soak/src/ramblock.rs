//! A `Vec`-backed [`Block`] device for the in-RAM soak.

use tairix_abi::driver::block::{Block, BlockGeometry};
use tairix_abi::DriverError;

use crate::SECTOR_BYTES;

/// A zero-initialised, RAM-backed [`Block`] device addressing
/// [`SECTOR_BYTES`]-byte logical blocks. The whole image lives in a
/// `Vec`, so there is no real disk and no `mkfs` shell-out
/// (`.junie/filesystems.md`).
pub struct RamBlock {
    store: Vec<u8>,
}

impl RamBlock {
    /// A zeroed device holding at least `bytes` bytes, rounded down to a
    /// whole number of [`SECTOR_BYTES`]-byte sectors (at least one).
    #[must_use]
    pub fn new(bytes: u64) -> Self {
        let sector = u64::from(SECTOR_BYTES);
        let sectors = (bytes / sector).max(1);
        let len = usize::try_from(sectors * sector).unwrap_or(usize::MAX);
        Self {
            store: vec![0u8; len],
        }
    }

    /// Device capacity in bytes.
    #[must_use]
    pub fn len_bytes(&self) -> u64 {
        self.store.len() as u64
    }

    /// Byte span `[start, end)` for a `len`-byte access at sector `lba`,
    /// or an error if the access is unaligned or out of range.
    fn span(&self, lba: u64, len: usize) -> Result<(usize, usize), DriverError> {
        let sector = SECTOR_BYTES as usize;
        if len == 0 || len % sector != 0 {
            return Err(DriverError::BufferTooSmall);
        }
        let start = usize::try_from(lba)
            .ok()
            .and_then(|l| l.checked_mul(sector))
            .ok_or(DriverError::LengthOutOfRange)?;
        let end = start
            .checked_add(len)
            .ok_or(DriverError::LengthOutOfRange)?;
        if end > self.store.len() {
            return Err(DriverError::LengthOutOfRange);
        }
        Ok((start, end))
    }
}

impl Block for RamBlock {
    fn geometry(&self) -> Result<BlockGeometry, DriverError> {
        Ok(BlockGeometry {
            block_size: SECTOR_BYTES,
            block_count: self.store.len() as u64 / u64::from(SECTOR_BYTES),
        })
    }

    fn read_blocks(&mut self, lba: u64, buf: &mut [u8]) -> Result<(), DriverError> {
        let (start, end) = self.span(lba, buf.len())?;
        buf.copy_from_slice(&self.store[start..end]);
        Ok(())
    }

    fn write_blocks(&mut self, lba: u64, buf: &[u8]) -> Result<(), DriverError> {
        let (start, end) = self.span(lba, buf.len())?;
        self.store[start..end].copy_from_slice(buf);
        Ok(())
    }

    fn flush(&mut self) -> Result<(), DriverError> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_a_sector() {
        let mut dev = RamBlock::new(4 * u64::from(SECTOR_BYTES));
        let payload = [0xA5u8; SECTOR_BYTES as usize];
        assert!(dev.write_blocks(2, &payload).is_ok());
        let mut back = [0u8; SECTOR_BYTES as usize];
        assert!(dev.read_blocks(2, &mut back).is_ok());
        assert_eq!(back, payload);
    }

    #[test]
    fn rejects_out_of_range_and_unaligned() {
        let mut dev = RamBlock::new(u64::from(SECTOR_BYTES));
        let mut one = [0u8; SECTOR_BYTES as usize];
        assert_eq!(
            dev.read_blocks(1, &mut one),
            Err(DriverError::LengthOutOfRange)
        );
        let mut tiny = [0u8; 8];
        assert_eq!(
            dev.read_blocks(0, &mut tiny),
            Err(DriverError::BufferTooSmall)
        );
    }
}
