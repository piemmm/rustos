//! Byte-addressed staging I/O over a [`Block`] device.
//!
//! ADFS addresses everything in 256-byte (old map) or `1 << log2secsize`
//! (new map) logical sectors, while the backing device exposes its own
//! logical-block size. This module decouples the two: every access is
//! staged through an on-stack scratch buffer one device block at a time,
//! so any ADFS sector size works over any device block size without a
//! special case.

use tairix_abi::driver::block::Block;
use tairix_abi::DriverError;

/// Largest device logical-block size the driver stages through its
/// on-stack scratch buffer. This is a validation bound on the untrusted
/// device geometry, not a scalable capacity: no ADFS medium and no Tier-1
/// block device presents a logical block larger than 4096 bytes, and a
/// device claiming one is refused at `open` rather than trusted.
pub const MAX_BLOCK_SIZE: u32 = 4096;

/// A [`Block`] device wrapped with byte-addressed access.
pub struct Volume<B: Block> {
    device: B,
    /// Device logical-block size in bytes (validated ≤ [`MAX_BLOCK_SIZE`]).
    block_size: u32,
    /// Total device capacity in bytes.
    device_bytes: u64,
}

impl<B: Block> Volume<B> {
    /// Wrap `device`, validating its geometry.
    ///
    /// # Errors
    ///
    /// * [`DriverError::Unsupported`] if the device block size is zero,
    ///   not a power of two, or larger than [`MAX_BLOCK_SIZE`].
    /// * [`DriverError::DeviceFault`] if the geometry cannot be queried.
    pub fn new(device: B) -> Result<Self, DriverError> {
        let geometry = device.geometry()?;
        let block_size = geometry.block_size;
        if block_size == 0 || !block_size.is_power_of_two() || block_size > MAX_BLOCK_SIZE {
            return Err(DriverError::Unsupported);
        }
        let device_bytes = geometry
            .block_count
            .checked_mul(u64::from(block_size))
            .ok_or(DriverError::Unsupported)?;
        Ok(Self {
            device,
            block_size,
            device_bytes,
        })
    }

    /// Total device capacity in bytes.
    pub fn device_bytes(&self) -> u64 {
        self.device_bytes
    }

    /// Unwrap the underlying device.
    pub fn into_device(self) -> B {
        self.device
    }

    /// Read `buf.len()` bytes starting at byte `offset`.
    ///
    /// # Errors
    ///
    /// * [`DriverError::LengthOutOfRange`] if the range leaves the device.
    /// * [`DriverError::DeviceFault`] on an unrecoverable block read.
    pub fn read_bytes(&mut self, offset: u64, buf: &mut [u8]) -> Result<(), DriverError> {
        self.check_range(offset, buf.len())?;
        let block_size = self.block_size as usize;
        let mut scratch = [0u8; MAX_BLOCK_SIZE as usize];
        let scratch = &mut scratch[..block_size];
        let mut done = 0usize;
        while done < buf.len() {
            let at = offset + done as u64;
            let lba = at / u64::from(self.block_size);
            // The intra-block offset is below the (u32) block size.
            let intra = usize::try_from(at % u64::from(self.block_size)).unwrap_or(0);
            let take = (block_size - intra).min(buf.len() - done);
            self.device.read_blocks(lba, scratch)?;
            buf[done..done + take].copy_from_slice(&scratch[intra..intra + take]);
            done += take;
        }
        Ok(())
    }

    /// Write `data.len()` bytes starting at byte `offset`, staging a
    /// read-modify-write for any partial device block.
    ///
    /// # Errors
    ///
    /// * [`DriverError::LengthOutOfRange`] if the range leaves the device.
    /// * [`DriverError::DeviceFault`] on an unrecoverable block access.
    pub fn write_bytes(&mut self, offset: u64, data: &[u8]) -> Result<(), DriverError> {
        self.check_range(offset, data.len())?;
        let block_size = self.block_size as usize;
        let mut scratch = [0u8; MAX_BLOCK_SIZE as usize];
        let scratch = &mut scratch[..block_size];
        let mut done = 0usize;
        while done < data.len() {
            let at = offset + done as u64;
            let lba = at / u64::from(self.block_size);
            // The intra-block offset is below the (u32) block size.
            let intra = usize::try_from(at % u64::from(self.block_size)).unwrap_or(0);
            let take = (block_size - intra).min(data.len() - done);
            if intra != 0 || take != block_size {
                self.device.read_blocks(lba, scratch)?;
            }
            scratch[intra..intra + take].copy_from_slice(&data[done..done + take]);
            self.device.write_blocks(lba, scratch)?;
            done += take;
        }
        Ok(())
    }

    /// Fill `len` bytes starting at byte `dst` with zeroes.
    ///
    /// # Errors
    ///
    /// * [`DriverError::LengthOutOfRange`] if the range leaves the device.
    /// * [`DriverError::DeviceFault`] on an unrecoverable block access.
    pub fn zero_bytes(&mut self, dst: u64, len: u64) -> Result<(), DriverError> {
        let zeroes = [0u8; MAX_BLOCK_SIZE as usize];
        let mut done = 0u64;
        while done < len {
            let take = usize::try_from((len - done).min(512)).unwrap_or(512);
            self.write_bytes(dst + done, &zeroes[..take])?;
            done += take as u64;
        }
        Ok(())
    }

    fn check_range(&self, offset: u64, len: usize) -> Result<(), DriverError> {
        let end = offset
            .checked_add(len as u64)
            .ok_or(DriverError::LengthOutOfRange)?;
        if end > self.device_bytes {
            return Err(DriverError::LengthOutOfRange);
        }
        Ok(())
    }
}

/// Read a little-endian `u16` from `buf` at `at`.
pub fn get_u16(buf: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([buf[at], buf[at + 1]])
}

/// Read a little-endian 24-bit value from `buf` at `at`.
pub fn get_u24(buf: &[u8], at: usize) -> u32 {
    u32::from(buf[at]) | u32::from(buf[at + 1]) << 8 | u32::from(buf[at + 2]) << 16
}

/// Read a little-endian `u32` from `buf` at `at`.
pub fn get_u32(buf: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([buf[at], buf[at + 1], buf[at + 2], buf[at + 3]])
}

/// Write a little-endian `u16` into `buf` at `at`.
pub fn put_u16(buf: &mut [u8], at: usize, value: u16) {
    buf[at..at + 2].copy_from_slice(&value.to_le_bytes());
}

/// Write a little-endian 24-bit value into `buf` at `at` (high byte of
/// `value` must be zero; enforced by the callers' 24-bit field bounds).
pub fn put_u24(buf: &mut [u8], at: usize, value: u32) {
    let bytes = value.to_le_bytes();
    buf[at] = bytes[0];
    buf[at + 1] = bytes[1];
    buf[at + 2] = bytes[2];
}

/// Write a little-endian `u32` into `buf` at `at`.
pub fn put_u32(buf: &mut [u8], at: usize, value: u32) {
    buf[at..at + 4].copy_from_slice(&value.to_le_bytes());
}
