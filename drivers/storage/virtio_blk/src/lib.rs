//! RustOS virtio-blk block driver.
//!
//! Implements [`rustos_abi::driver::block::Block`] on top of the
//! bus-agnostic virtio protocol from `lib/virtio`. The driver is
//! bus-agnostic: the same source compiles against the PCI and MMIO
//! transports (from `drivers/bus/virtio`) because both implement the
//! [`rustos_virtio::Transport`] trait. (`AGENTS.md` §2.2 / §17.4 —
//! the queue protocol lives once, in `lib/virtio`.)
//!
//! # Public surface
//!
//! Per `AGENTS.md` §8 the only public *function* is [`register`].
//! [`VirtioBlk`] is a public *type* re-exported so the driver host
//! can instantiate it; the host never reaches into the type beyond
//! the [`Block`] trait.
//!
//! # Capabilities
//!
//! Loading requires [`CapabilityId::DRV_LOAD`]. The block driver
//! does not request `CAP_DRV_KERNEL`; it runs in user space per
//! `AGENTS.md` §4 / §8.
//!
//! # Zero-on-free
//!
//! The classified `read_blocks_with_class` / `write_blocks_with_class`
//! methods honour the sensitive flag by scrubbing the bounce-buffer
//! staging through [`rustos_virtio::BounceBuffer`]'s drop
//! impl (`AGENTS.md` §4 / `BufferClass::Sensitive` contract).

#![no_std]
#![forbid(unsafe_op_in_unsafe_fn)]
#![deny(missing_docs)]

extern crate alloc;

use core::convert::TryFrom;
use rustos_abi::driver::block::{Block, BlockGeometry};
use rustos_abi::driver::BufferClass;
use rustos_abi::{CapabilityId, DriverError, DriverHandle, DriverHost};
use rustos_virtio::{
    BounceBuffer, ChainSegment, Direction, SplitQueue, Status, Transport, VirtioError, VirtioHost,
};

/// Per-driver `DriverHandle` marker returned by [`register`].
const REGISTER_HANDLE_MARKER: u64 = 0x564E_4250_0000_0001; // "VBKP"

/// Driver entry point (`AGENTS.md` §8).
///
/// # Errors
///
/// * [`DriverError::PermissionDenied`] if the host did not grant
///   [`CapabilityId::DRV_LOAD`].
///
/// # Capabilities
///
/// Requires [`CapabilityId::DRV_LOAD`].
pub fn register(host: &dyn DriverHost) -> Result<DriverHandle, DriverError> {
    if !host.has_capability(CapabilityId::DRV_LOAD) {
        return Err(DriverError::PermissionDenied);
    }
    DriverHandle::from_raw(REGISTER_HANDLE_MARKER)
}

/// Virtio-blk wire protocol constants (virtio 1.1 §5.2.6).
mod wire {
    pub const VIRTIO_BLK_T_IN: u32 = 0;
    pub const VIRTIO_BLK_T_OUT: u32 = 1;
    /// `struct virtio_blk_req` header size: type(4) + reserved(4) + sector(8).
    pub const HEADER_LEN: usize = 16;
    pub const STATUS_LEN: usize = 1;
    pub const STATUS_OK: u8 = 0;
    pub const STATUS_IOERR: u8 = 1;
    /// Device-config byte offset of the capacity (in 512-byte sectors).
    pub const CONFIG_CAPACITY_OFFSET: usize = 0;
    /// Sector size, fixed in `abi-v1` Stage 4. `VIRTIO_BLK_F_BLK_SIZE`
    /// extended-feature negotiation is deferred to Stage 5.
    pub const SECTOR_SIZE: u32 = 512;
}

/// Block device backed by a cross-arch virtio transport.
///
/// `'h` bounds the borrow of the [`VirtioHost`] the driver allocates
/// its DMA regions through. The host is *minted per driver load* by
/// a `VirtioHostFactory` (the seam defined in `lib/virtio`)
/// and lives only for the duration of that load, so the driver borrows
/// it for `'h` rather than demanding a `'static` host (`AGENTS.md`
/// §4 — per-process pools are reclaimed when the driver unloads).
pub struct VirtioBlk<'h, T: Transport> {
    transport: T,
    queue: SplitQueue,
    host: &'h dyn VirtioHost,
    block_size: u32,
    block_count: u64,
}

impl<'h, T: Transport> VirtioBlk<'h, T> {
    /// Bring the device online.
    ///
    /// Implements the virtio-1.1 §3.1 initialisation sequence:
    /// reset, ACKNOWLEDGE, DRIVER, feature negotiation (Stage 4
    /// accepts zero extended features), `FEATURES_OK`, set up the
    /// single requestq, `DRIVER_OK`, then read the capacity from
    /// the device-configuration window.
    ///
    /// # Errors
    ///
    /// Propagates [`VirtioError`] from the transport / queue setup.
    /// The constructor returns [`VirtioError::FeaturesRejected`] if
    /// the device clears [`Status::FEATURES_OK`] after the driver
    /// completed negotiation.
    pub fn open(mut transport: T, host: &'h dyn VirtioHost) -> Result<Self, VirtioError> {
        transport.reset();
        let mut status = Status::default().with(Status::ACKNOWLEDGE);
        transport.set_status(status);
        status = status.with(Status::DRIVER);
        transport.set_status(status);
        let _device_features = transport.device_features();
        transport.set_driver_features(0);
        status = status.with(Status::FEATURES_OK);
        transport.set_status(status);
        if !transport.status().contains(Status::FEATURES_OK) {
            return Err(VirtioError::FeaturesRejected);
        }
        let queue = SplitQueue::new(&mut transport, host, 0, 8)?;
        status = status.with(Status::DRIVER_OK);
        transport.set_status(status);
        // Read capacity from device-config.
        let mut cap = [0u8; 8];
        transport.read_config(wire::CONFIG_CAPACITY_OFFSET, &mut cap);
        let capacity_sectors = u64::from_le_bytes(cap);
        Ok(Self {
            transport,
            queue,
            host,
            block_size: wire::SECTOR_SIZE,
            block_count: capacity_sectors,
        })
    }

    /// Tear the device down for unload (sets the status byte to 0).
    pub fn close(mut self) {
        self.transport.reset();
    }

    /// Borrow the underlying transport (host-side test access only;
    /// not exposed across the driver-class trait surface).
    #[must_use]
    pub fn transport(&self) -> &T {
        &self.transport
    }
    /// Borrow the underlying transport mutably for the in-process
    /// software peer to drive on `kick` (`MockTransport::drain_queue`).
    pub fn transport_mut(&mut self) -> &mut T {
        &mut self.transport
    }

    fn build_header(req_type: u32, sector: u64) -> [u8; wire::HEADER_LEN] {
        let mut out = [0u8; wire::HEADER_LEN];
        out[0..4].copy_from_slice(&req_type.to_le_bytes());
        // reserved [4..8] stays zero per spec.
        out[8..16].copy_from_slice(&sector.to_le_bytes());
        out
    }

    fn validate_block_op(&self, lba: u64, buf_len: usize) -> Result<u64, DriverError> {
        let bs = self.block_size as usize;
        if buf_len == 0 || buf_len % bs != 0 {
            return Err(DriverError::BufferTooSmall);
        }
        let blocks = u64::try_from(buf_len / bs).map_err(|_| DriverError::LengthOutOfRange)?;
        let end = lba
            .checked_add(blocks)
            .ok_or(DriverError::LengthOutOfRange)?;
        if end > self.block_count {
            return Err(DriverError::LengthOutOfRange);
        }
        Ok(blocks)
    }

    fn run_request(
        &mut self,
        req_type: u32,
        lba: u64,
        payload: &mut [u8],
        write_outbound: bool,
        class: BufferClass,
    ) -> Result<(), DriverError> {
        // Allocate three DMA regions: header, data, status. The
        // `BounceBuffer` wrappers carry the sensitive scrub on drop.
        let header_region = self.host.alloc_dma_zeroed(wire::HEADER_LEN)?;
        let data_region = self.host.alloc_dma_zeroed(payload.len())?;
        let status_region = self.host.alloc_dma_zeroed(wire::STATUS_LEN)?;
        let mut header_bb = BounceBuffer::new(header_region, class);
        let mut data_bb = BounceBuffer::new(data_region, class);
        let mut status_bb = BounceBuffer::new(status_region, class);
        // Stage header.
        let header = Self::build_header(req_type, lba);
        header_bb
            .stage(&header)
            .map_err(|()| DriverError::BufferTooSmall)?;
        // Stage outbound data for write requests.
        if write_outbound {
            data_bb
                .stage(payload)
                .map_err(|()| DriverError::BufferTooSmall)?;
        }
        // Build the descriptor chain. Header + data are read by
        // device for writes; for reads only the header is. Status
        // is always device-write.
        let data_dir = if write_outbound {
            Direction::DeviceRead
        } else {
            Direction::DeviceWrite
        };
        let payload_len_u32 =
            u32::try_from(payload.len()).map_err(|_| DriverError::LengthOutOfRange)?;
        let segments = [
            ChainSegment {
                phys: header_bb.phys(),
                len: u32::try_from(wire::HEADER_LEN).unwrap_or(0),
                direction: Direction::DeviceRead,
            },
            ChainSegment {
                phys: data_bb.phys(),
                len: payload_len_u32,
                direction: data_dir,
            },
            ChainSegment {
                phys: status_bb.phys(),
                len: u32::try_from(wire::STATUS_LEN).unwrap_or(0),
                direction: Direction::DeviceWrite,
            },
        ];
        self.queue
            .add_chain(&segments)
            .map_err(VirtioError::as_driver_error)?;
        self.queue.kick(&mut self.transport);
        // Block until the host's notifier returns. In production
        // the kernel host parks the calling task; the mock host
        // returns immediately (the unit test calls
        // `MockTransport::drain_queue` to advance the peer).
        self.host.notify_wait(self.queue.index());
        let _token = self
            .queue
            .poll_used()
            .map_err(VirtioError::as_driver_error)?;
        // For reads, copy device-written data back to the caller.
        if !write_outbound {
            // The mock peer wrote into `data_bb`'s staging through
            // its phys-mapped slice. `BounceBuffer::full_region_mut`
            // gives us a CPU view of the same bytes.
            payload.copy_from_slice(&data_bb.full_region_mut()[..payload.len()]);
        }
        // Decode status.
        let status_byte = status_bb.full_region_mut()[0];
        match status_byte {
            wire::STATUS_OK => Ok(()),
            wire::STATUS_IOERR => Err(DriverError::DeviceFault),
            _ => Err(DriverError::Unsupported),
        }
    }
}

impl<T: Transport> Block for VirtioBlk<'_, T> {
    fn geometry(&self) -> Result<BlockGeometry, DriverError> {
        Ok(BlockGeometry {
            block_size: self.block_size,
            block_count: self.block_count,
        })
    }
    fn read_blocks(&mut self, lba: u64, buf: &mut [u8]) -> Result<(), DriverError> {
        self.read_blocks_with_class(lba, buf, BufferClass::NonSensitive)
    }
    fn write_blocks(&mut self, lba: u64, buf: &[u8]) -> Result<(), DriverError> {
        self.write_blocks_with_class(lba, buf, BufferClass::NonSensitive)
    }
    fn read_blocks_with_class(
        &mut self,
        lba: u64,
        buf: &mut [u8],
        class: BufferClass,
    ) -> Result<(), DriverError> {
        self.validate_block_op(lba, buf.len())?;
        self.run_request(wire::VIRTIO_BLK_T_IN, lba, buf, false, class)
    }
    fn write_blocks_with_class(
        &mut self,
        lba: u64,
        buf: &[u8],
        class: BufferClass,
    ) -> Result<(), DriverError> {
        self.validate_block_op(lba, buf.len())?;
        // The trait signature uses `&[u8]`; we copy into a local
        // mutable buffer because `run_request` needs `&mut` to
        // double as the read-back path. The local buffer is held
        // inside the bounce buffer regardless; this extra copy is
        // unavoidable for the write path because the abi trait
        // does not allow handing the device a mutable borrow of
        // the caller's slice.
        let mut payload: alloc::vec::Vec<u8> = buf.to_vec();
        let result = self.run_request(wire::VIRTIO_BLK_T_OUT, lba, &mut payload, true, class);
        if class.is_sensitive() {
            payload.fill(0);
        }
        result
    }
}

#[cfg(test)]
mod tests;
