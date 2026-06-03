//! Block-storage driver class (`drivers/storage/*`).
//!
//! Block drivers expose a fixed-size logical-block array. The Stage 4
//! first driver is `virtio_blk`; the trait carries only the minimum
//! the filesystem layer needs.

use super::{BufferClass, DriverError};

/// Geometry of a block device.
#[repr(C)]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct BlockGeometry {
    /// Size of a single logical block, in bytes. Must be a power of
    /// two; common values are 512 and 4096.
    pub block_size: u32,
    /// Total number of logical blocks. The device's byte capacity is
    /// `block_size as u64 * block_count`.
    pub block_count: u64,
}

/// Discard (TRIM/unmap) capability a block device reports through
/// [`Block::discard_capability`].
///
/// Discard tells the device a range of logical blocks no longer holds
/// useful data, letting flash/thin-provisioned backends reclaim it.
/// It is an *advisory* hint: a caller must never assume a discarded
/// block reads back as zero (`docs/src/filesystem/rustfs-spec.md`
/// §11). A device that does not support discard reports
/// [`DiscardCapability::unsupported`]; such a device is *recorded, not
/// failed* by the caller (§11).
#[repr(C)]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct DiscardCapability {
    /// Whether the device implements discard at all.
    pub supported: bool,
    /// Discard alignment/granularity, in logical blocks. A discard
    /// request's start LBA and block count must each be a multiple of
    /// this value; the caller aligns its ranges inward to satisfy it.
    /// Always `>= 1` when [`supported`](Self::supported) is `true`.
    pub granularity_blocks: u64,
    /// Largest number of logical blocks a single
    /// [`Block::discard`] call may cover. `0` means the device imposes
    /// no per-request limit (the caller still bounds requests by
    /// [`BlockGeometry::block_count`]).
    pub max_blocks_per_request: u64,
}

impl DiscardCapability {
    /// The capability a device that does **not** support discard
    /// reports. The default [`Block::discard_capability`]
    /// implementation returns this.
    #[must_use]
    pub const fn unsupported() -> Self {
        Self {
            supported: false,
            granularity_blocks: 0,
            max_blocks_per_request: 0,
        }
    }
}

/// Trait every block driver implements.
///
/// # Capabilities
///
/// Methods are gated by ownership of the [`DriverHandle`] returned
/// from the driver's `register` entry point (load-time
/// [`CapabilityId::DRV_LOAD`](crate::CapabilityId::DRV_LOAD)).
///
/// [`DriverHandle`]: crate::driver::DriverHandle
pub trait Block {
    /// Report the device geometry.
    ///
    /// # Errors
    ///
    /// * [`DriverError::DeviceFault`] if the underlying hardware
    ///   could not be queried.
    ///
    /// # Capabilities
    ///
    /// Caller must present the driver's [`DriverHandle`].
    ///
    /// [`DriverHandle`]: crate::driver::DriverHandle
    fn geometry(&self) -> Result<BlockGeometry, DriverError>;

    /// Read consecutive logical blocks starting at `lba` into `buf`.
    ///
    /// `buf.len()` must be a positive integer multiple of
    /// `geometry()?.block_size`. The number of blocks read equals
    /// `buf.len() / geometry()?.block_size`.
    ///
    /// # Errors
    ///
    /// * [`DriverError::BufferTooSmall`] if `buf` is empty or not a
    ///   block-size multiple.
    /// * [`DriverError::LengthOutOfRange`] if `lba` or the implied
    ///   end-LBA exceeds [`BlockGeometry::block_count`].
    /// * [`DriverError::DeviceFault`] on hardware I/O failure.
    ///
    /// # Capabilities
    ///
    /// Caller must present the driver's [`DriverHandle`].
    ///
    /// [`DriverHandle`]: crate::driver::DriverHandle
    fn read_blocks(&mut self, lba: u64, buf: &mut [u8]) -> Result<(), DriverError>;

    /// Write consecutive logical blocks starting at `lba` from `buf`.
    ///
    /// Length and range constraints match [`Self::read_blocks`].
    ///
    /// # Errors
    ///
    /// * [`DriverError::BufferTooSmall`] if `buf` is empty or not a
    ///   block-size multiple.
    /// * [`DriverError::LengthOutOfRange`] if the implied end-LBA
    ///   exceeds [`BlockGeometry::block_count`].
    /// * [`DriverError::Unsupported`] if the device is read-only.
    /// * [`DriverError::DeviceFault`] on hardware I/O failure.
    ///
    /// # Capabilities
    ///
    /// Caller must present the driver's [`DriverHandle`].
    ///
    /// [`DriverHandle`]: crate::driver::DriverHandle
    fn write_blocks(&mut self, lba: u64, buf: &[u8]) -> Result<(), DriverError>;

    /// Read `buf.len() / block_size` blocks starting at `lba`,
    /// declaring the payload's sensitivity class.
    ///
    /// Behaviour is identical to [`Self::read_blocks`] except that
    /// when `class == BufferClass::Sensitive` the driver is required
    /// to zero every internal staging copy of the payload before the
    /// method returns (`AGENTS.md` §4). The caller-owned `buf` is
    /// **not** zeroed; ownership of that scrubbing remains with the
    /// caller after the read completes.
    ///
    /// The default implementation delegates to [`Self::read_blocks`]
    /// and is therefore safe to use only by drivers that do **not**
    /// keep an internal copy of the payload after the call returns
    /// (e.g. drivers that DMA straight into `buf`). Any driver that
    /// stages reads through its own buffer must override this method.
    ///
    /// # Errors
    ///
    /// As for [`Self::read_blocks`].
    ///
    /// # Capabilities
    ///
    /// Caller must present the driver's [`DriverHandle`].
    ///
    /// [`DriverHandle`]: crate::driver::DriverHandle
    fn read_blocks_with_class(
        &mut self,
        lba: u64,
        buf: &mut [u8],
        class: BufferClass,
    ) -> Result<(), DriverError> {
        let _ = class;
        self.read_blocks(lba, buf)
    }

    /// Write `buf.len() / block_size` blocks starting at `lba`,
    /// declaring the payload's sensitivity class.
    ///
    /// Behaviour is identical to [`Self::write_blocks`] except that
    /// when `class == BufferClass::Sensitive` the driver is required
    /// to zero every internal staging copy of the payload before the
    /// method returns (`AGENTS.md` §4).
    ///
    /// The default implementation delegates to [`Self::write_blocks`]
    /// and is only safe for drivers that never copy `buf` into a
    /// private staging area. Drivers that bounce-buffer writes
    /// **must** override.
    ///
    /// # Errors
    ///
    /// As for [`Self::write_blocks`].
    ///
    /// # Capabilities
    ///
    /// Caller must present the driver's [`DriverHandle`].
    ///
    /// [`DriverHandle`]: crate::driver::DriverHandle
    fn write_blocks_with_class(
        &mut self,
        lba: u64,
        buf: &[u8],
        class: BufferClass,
    ) -> Result<(), DriverError> {
        let _ = class;
        self.write_blocks(lba, buf)
    }

    /// Report the device's discard (TRIM/unmap) capability.
    ///
    /// Reporting capability is non-destructive and never fails on a
    /// healthy device. The default implementation reports
    /// [`DiscardCapability::unsupported`]; a driver whose backend
    /// supports discard overrides it. A device without discard support
    /// is *recorded, not failed* by the caller
    /// (`docs/src/filesystem/rustfs-spec.md` §11), so this method does
    /// not error merely because discard is unavailable.
    ///
    /// # Errors
    ///
    /// * [`DriverError::DeviceFault`] if the underlying hardware could
    ///   not be queried.
    ///
    /// # Capabilities
    ///
    /// Caller must present the driver's [`DriverHandle`].
    ///
    /// [`DriverHandle`]: crate::driver::DriverHandle
    fn discard_capability(&self) -> Result<DiscardCapability, DriverError> {
        Ok(DiscardCapability::unsupported())
    }

    /// Discard `blocks` logical blocks starting at `lba`, telling the
    /// device the range no longer holds useful data.
    ///
    /// Discard is advisory: it neither reads nor writes caller data and
    /// the caller must never assume the range reads back as zero
    /// afterwards (`docs/src/filesystem/rustfs-spec.md` §11). The caller
    /// is responsible for aligning `lba` and `blocks` to the
    /// granularity reported by [`Self::discard_capability`] and for
    /// keeping `blocks` within
    /// [`DiscardCapability::max_blocks_per_request`].
    ///
    /// The default implementation returns [`DriverError::Unsupported`];
    /// a driver whose backend supports discard overrides it.
    ///
    /// # Errors
    ///
    /// * [`DriverError::Unsupported`] if the device does not support
    ///   discard.
    /// * [`DriverError::LengthOutOfRange`] if `lba` or the implied
    ///   end-LBA exceeds [`BlockGeometry::block_count`].
    /// * [`DriverError::DeviceFault`] on hardware I/O failure.
    ///
    /// # Capabilities
    ///
    /// Caller must present the driver's [`DriverHandle`].
    ///
    /// [`DriverHandle`]: crate::driver::DriverHandle
    fn discard(&mut self, lba: u64, blocks: u64) -> Result<(), DriverError> {
        let _ = (lba, blocks);
        Err(DriverError::Unsupported)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct MockBlock {
        geo: BlockGeometry,
        store: [u8; 1024],
    }

    impl Block for MockBlock {
        fn geometry(&self) -> Result<BlockGeometry, DriverError> {
            Ok(self.geo)
        }

        fn read_blocks(&mut self, lba: u64, buf: &mut [u8]) -> Result<(), DriverError> {
            let bs = self.geo.block_size as usize;
            if buf.is_empty() || buf.len() % bs != 0 {
                return Err(DriverError::BufferTooSmall);
            }
            let blocks = buf.len() / bs;
            let end = lba.saturating_add(blocks as u64);
            if end > self.geo.block_count {
                return Err(DriverError::LengthOutOfRange);
            }
            let Ok(lba_usize) = usize::try_from(lba) else {
                return Err(DriverError::LengthOutOfRange);
            };
            let start = lba_usize * bs;
            buf.copy_from_slice(&self.store[start..start + buf.len()]);
            Ok(())
        }

        fn write_blocks(&mut self, lba: u64, buf: &[u8]) -> Result<(), DriverError> {
            let bs = self.geo.block_size as usize;
            if buf.is_empty() || buf.len() % bs != 0 {
                return Err(DriverError::BufferTooSmall);
            }
            let blocks = buf.len() / bs;
            let end = lba.saturating_add(blocks as u64);
            if end > self.geo.block_count {
                return Err(DriverError::LengthOutOfRange);
            }
            let Ok(lba_usize) = usize::try_from(lba) else {
                return Err(DriverError::LengthOutOfRange);
            };
            let start = lba_usize * bs;
            self.store[start..start + buf.len()].copy_from_slice(buf);
            Ok(())
        }
    }

    #[test]
    fn block_round_trip() {
        let mut dev = MockBlock {
            geo: BlockGeometry {
                block_size: 64,
                block_count: 16,
            },
            store: [0u8; 1024],
        };
        let mut payload = [0u8; 64];
        let mut counter: u8 = 0;
        for b in &mut payload {
            *b = counter;
            counter = counter.wrapping_add(1);
        }
        assert!(dev.write_blocks(2, &payload).is_ok());
        let mut readback = [0u8; 64];
        assert!(dev.read_blocks(2, &mut readback).is_ok());
        assert_eq!(readback, payload);
    }

    #[test]
    fn block_rejects_short_buffer() {
        let mut dev = MockBlock {
            geo: BlockGeometry {
                block_size: 64,
                block_count: 16,
            },
            store: [0u8; 1024],
        };
        let mut tiny = [0u8; 8];
        assert_eq!(
            dev.read_blocks(0, &mut tiny),
            Err(DriverError::BufferTooSmall)
        );
    }

    /// A `Block` implementation that overrides the classified
    /// methods to scrub a private staging copy when the caller
    /// requests sensitive handling. Mirrors the contract documented
    /// on [`Block::read_blocks_with_class`].
    struct SensitiveStagingBlock {
        geo: BlockGeometry,
        store: [u8; 1024],
        staging: [u8; 64],
        scrubbed_after_last_call: bool,
    }

    impl Block for SensitiveStagingBlock {
        fn geometry(&self) -> Result<BlockGeometry, DriverError> {
            Ok(self.geo)
        }
        fn read_blocks(&mut self, lba: u64, buf: &mut [u8]) -> Result<(), DriverError> {
            let bs = self.geo.block_size as usize;
            if buf.is_empty() || buf.len() % bs != 0 {
                return Err(DriverError::BufferTooSmall);
            }
            let blocks = buf.len() / bs;
            let end = lba.saturating_add(blocks as u64);
            if end > self.geo.block_count {
                return Err(DriverError::LengthOutOfRange);
            }
            let Ok(lba_usize) = usize::try_from(lba) else {
                return Err(DriverError::LengthOutOfRange);
            };
            let start = lba_usize * bs;
            // Stage through `self.staging` to model a bounce-buffered driver.
            self.staging[..buf.len()].copy_from_slice(&self.store[start..start + buf.len()]);
            buf.copy_from_slice(&self.staging[..buf.len()]);
            self.scrubbed_after_last_call = false;
            Ok(())
        }
        fn write_blocks(&mut self, lba: u64, buf: &[u8]) -> Result<(), DriverError> {
            let bs = self.geo.block_size as usize;
            if buf.is_empty() || buf.len() % bs != 0 {
                return Err(DriverError::BufferTooSmall);
            }
            let blocks = buf.len() / bs;
            let end = lba.saturating_add(blocks as u64);
            if end > self.geo.block_count {
                return Err(DriverError::LengthOutOfRange);
            }
            let Ok(lba_usize) = usize::try_from(lba) else {
                return Err(DriverError::LengthOutOfRange);
            };
            let start = lba_usize * bs;
            self.staging[..buf.len()].copy_from_slice(buf);
            self.store[start..start + buf.len()].copy_from_slice(&self.staging[..buf.len()]);
            self.scrubbed_after_last_call = false;
            Ok(())
        }
        fn read_blocks_with_class(
            &mut self,
            lba: u64,
            buf: &mut [u8],
            class: BufferClass,
        ) -> Result<(), DriverError> {
            self.read_blocks(lba, buf)?;
            if class.is_sensitive() {
                for byte in &mut self.staging {
                    *byte = 0;
                }
                self.scrubbed_after_last_call = true;
            }
            Ok(())
        }
        fn write_blocks_with_class(
            &mut self,
            lba: u64,
            buf: &[u8],
            class: BufferClass,
        ) -> Result<(), DriverError> {
            self.write_blocks(lba, buf)?;
            if class.is_sensitive() {
                for byte in &mut self.staging {
                    *byte = 0;
                }
                self.scrubbed_after_last_call = true;
            }
            Ok(())
        }
    }

    #[test]
    fn default_with_class_delegates_to_plain_path() {
        let mut dev = MockBlock {
            geo: BlockGeometry {
                block_size: 64,
                block_count: 16,
            },
            store: [0u8; 1024],
        };
        let payload = [0x5Au8; 64];
        assert!(dev
            .write_blocks_with_class(0, &payload, BufferClass::NonSensitive)
            .is_ok());
        let mut readback = [0u8; 64];
        assert!(dev
            .read_blocks_with_class(0, &mut readback, BufferClass::NonSensitive)
            .is_ok());
        assert_eq!(readback, payload);
    }

    #[test]
    fn sensitive_class_triggers_staging_scrub() {
        let mut dev = SensitiveStagingBlock {
            geo: BlockGeometry {
                block_size: 64,
                block_count: 4,
            },
            store: [0u8; 1024],
            staging: [0u8; 64],
            scrubbed_after_last_call: false,
        };
        let payload = [0xC3u8; 64];
        assert!(dev
            .write_blocks_with_class(0, &payload, BufferClass::Sensitive)
            .is_ok());
        assert!(dev.scrubbed_after_last_call);
        assert!(dev.staging.iter().all(|b| *b == 0));
        // Non-sensitive call must NOT scrub the staging buffer.
        let payload2 = [0xAAu8; 64];
        assert!(dev
            .write_blocks_with_class(1, &payload2, BufferClass::NonSensitive)
            .is_ok());
        assert!(!dev.scrubbed_after_last_call);
        assert!(dev.staging.contains(&0xAA));
    }

    #[test]
    fn default_discard_surface_reports_unsupported() {
        let mut dev = MockBlock {
            geo: BlockGeometry {
                block_size: 64,
                block_count: 16,
            },
            store: [0u8; 1024],
        };
        assert_eq!(
            dev.discard_capability(),
            Ok(DiscardCapability::unsupported())
        );
        assert_eq!(dev.discard(0, 4), Err(DriverError::Unsupported));
    }

    /// A `Block` device that supports discard and records the ranges it
    /// was asked to discard into a fixed-size log (`lib/abi` performs no
    /// allocation). Mirrors the contract a real flash/thin backend would
    /// honour and is the shape the `RustFS` trim tests assert against.
    struct RecordingDiscardBlock {
        geo: BlockGeometry,
        granularity: u64,
        discarded: [(u64, u64); 4],
        recorded: usize,
    }

    impl Block for RecordingDiscardBlock {
        fn geometry(&self) -> Result<BlockGeometry, DriverError> {
            Ok(self.geo)
        }
        fn read_blocks(&mut self, _lba: u64, _buf: &mut [u8]) -> Result<(), DriverError> {
            Err(DriverError::Unsupported)
        }
        fn write_blocks(&mut self, _lba: u64, _buf: &[u8]) -> Result<(), DriverError> {
            Err(DriverError::Unsupported)
        }
        fn discard_capability(&self) -> Result<DiscardCapability, DriverError> {
            Ok(DiscardCapability {
                supported: true,
                granularity_blocks: self.granularity,
                max_blocks_per_request: 0,
            })
        }
        fn discard(&mut self, lba: u64, blocks: u64) -> Result<(), DriverError> {
            let end = lba.saturating_add(blocks);
            if end > self.geo.block_count {
                return Err(DriverError::LengthOutOfRange);
            }
            if self.recorded < self.discarded.len() {
                self.discarded[self.recorded] = (lba, blocks);
                self.recorded += 1;
            }
            Ok(())
        }
    }

    #[test]
    fn recording_discard_block_reports_and_records() {
        let mut dev = RecordingDiscardBlock {
            geo: BlockGeometry {
                block_size: 64,
                block_count: 16,
            },
            granularity: 2,
            discarded: [(0, 0); 4],
            recorded: 0,
        };
        assert_eq!(
            dev.discard_capability(),
            Ok(DiscardCapability {
                supported: true,
                granularity_blocks: 2,
                max_blocks_per_request: 0,
            })
        );
        assert!(dev.discard(4, 4).is_ok());
        assert_eq!(dev.discard(14, 4), Err(DriverError::LengthOutOfRange));
        assert_eq!(dev.recorded, 1);
        assert_eq!(dev.discarded[0], (4, 4));
    }

    #[test]
    fn block_rejects_out_of_range_lba() {
        let mut dev = MockBlock {
            geo: BlockGeometry {
                block_size: 64,
                block_count: 4,
            },
            store: [0u8; 1024],
        };
        let mut buf = [0u8; 64];
        assert_eq!(
            dev.read_blocks(100, &mut buf),
            Err(DriverError::LengthOutOfRange)
        );
    }
}
