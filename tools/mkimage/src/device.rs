//! In-memory [`Block`] device the partition builders format against.
//!
//! Each partition of an image is authored on its own [`MemBlock`] sized to
//! exactly that partition, then the raw bytes are copied into the assembled
//! image at the partition's offset (`crate::build_rpi_image`). The device
//! addresses [`SECTOR_BYTES`]-byte sectors, the logical sector size both the
//! Pi's SD host and the in-tree filesystem drivers use.

use rustos_abi::driver::block::{Block, BlockGeometry};
use rustos_abi::DriverError;

/// Logical sector size of every authored image, in bytes.
pub const SECTOR_BYTES: usize = 512;

/// A zero-initialised, vector-backed [`Block`] device of a fixed sector
/// count.
pub struct MemBlock {
    store: Vec<u8>,
}

impl MemBlock {
    /// A zeroed device of `sectors` [`SECTOR_BYTES`]-byte sectors.
    ///
    /// # Errors
    ///
    /// [`DriverError::LengthOutOfRange`] if the byte capacity overflows
    /// `usize`.
    pub fn new(sectors: u64) -> Result<Self, DriverError> {
        let len = usize::try_from(sectors)
            .ok()
            .and_then(|s| s.checked_mul(SECTOR_BYTES))
            .ok_or(DriverError::LengthOutOfRange)?;
        Ok(Self {
            store: vec![0u8; len],
        })
    }

    /// Consume the device, yielding its raw partition bytes.
    #[must_use]
    pub fn into_bytes(self) -> Vec<u8> {
        self.store
    }

    /// A device over existing partition bytes (used to re-mount and verify
    /// an authored partition).
    ///
    /// # Errors
    ///
    /// [`DriverError::BufferTooSmall`] if `bytes` is empty or not a whole
    /// number of sectors.
    pub fn from_bytes(bytes: Vec<u8>) -> Result<Self, DriverError> {
        if bytes.is_empty() || bytes.len() % SECTOR_BYTES != 0 {
            return Err(DriverError::BufferTooSmall);
        }
        Ok(Self { store: bytes })
    }

    /// Byte span `[start, end)` for `len` bytes at sector `lba`, or an
    /// error if the access is unaligned or out of range.
    fn span(&self, lba: u64, len: usize) -> Result<(usize, usize), DriverError> {
        if len == 0 || len % SECTOR_BYTES != 0 {
            return Err(DriverError::BufferTooSmall);
        }
        let start = usize::try_from(lba)
            .ok()
            .and_then(|l| l.checked_mul(SECTOR_BYTES))
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

impl Block for MemBlock {
    fn geometry(&self) -> Result<BlockGeometry, DriverError> {
        Ok(BlockGeometry {
            block_size: u32::try_from(SECTOR_BYTES).map_err(|_| DriverError::DeviceFault)?,
            block_count: (self.store.len() / SECTOR_BYTES) as u64,
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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_a_sector() {
        let mut dev = MemBlock::new(4).expect("device allocates");
        let wrote = [0xa5u8; SECTOR_BYTES];
        dev.write_blocks(2, &wrote).expect("write in range");
        let mut read = [0u8; SECTOR_BYTES];
        dev.read_blocks(2, &mut read).expect("read in range");
        assert_eq!(read, wrote);
    }

    #[test]
    fn rejects_unaligned_and_out_of_range_access() {
        let mut dev = MemBlock::new(2).expect("device allocates");
        let mut small = [0u8; 100];
        assert_eq!(
            dev.read_blocks(0, &mut small),
            Err(DriverError::BufferTooSmall)
        );
        let mut sector = [0u8; SECTOR_BYTES];
        assert_eq!(
            dev.read_blocks(2, &mut sector),
            Err(DriverError::LengthOutOfRange)
        );
    }

    #[test]
    fn into_bytes_returns_the_whole_store() {
        let dev = MemBlock::new(3).expect("device allocates");
        assert_eq!(dev.into_bytes().len(), 3 * SECTOR_BYTES);
    }
}
