//! TAIRiX virtio-blk block driver.
//!
//! Implements [`tairix_abi::driver::block::Block`] on top of the
//! bus-agnostic virtio protocol from `lib/virtio`. The driver is
//! bus-agnostic: the same source compiles against the PCI and MMIO
//! transports (from `drivers/bus/virtio`) because both implement the
//! [`tairix_virtio::Transport`] trait. (the queue protocol lives once, in `lib/virtio`.)
//!
//! # Public surface
//!
//! Per the only public *function* is [`register`].
//! [`VirtioBlk`] is a public *type* re-exported so the driver host
//! can instantiate it; the host never reaches into the type beyond
//! the [`Block`] trait.
//!
//! # Capabilities
//!
//! Loading requires [`CapabilityId::DRV_LOAD`]. The block driver
//! does not request `CAP_DRV_KERNEL`; it runs in user space per.
//!
//! # Zero-on-free
//!
//! The classified `read_blocks_with_class` / `write_blocks_with_class`
//! methods honour the sensitive flag by scrubbing the bounce-buffer
//! staging through [`tairix_virtio::BounceBuffer`]'s drop
//! impl (`BufferClass::Sensitive` contract).

#![no_std]
#![forbid(unsafe_op_in_unsafe_fn)]
#![deny(missing_docs)]

// Only the unit tests (`mod tests`) allocate; the driver's data path
// is `alloc`-free by design (persistent DMA staging, no per-request
// heap copy), so the crate needs `alloc` in test builds only.
#[cfg(test)]
extern crate alloc;

use core::convert::TryFrom;
use tairix_abi::driver::block::{Block, BlockGeometry, DiscardCapability};
use tairix_abi::driver::BufferClass;
use tairix_abi::{CapabilityId, DriverBindKey, DriverError, DriverHandle, DriverHost, HwMatchKey};
use tairix_virtio::{
    BounceBuffer, ChainSegment, Direction, DmaSlab, SplitQueue, Status, Transport, VirtioError,
    VirtioHost,
};

/// Per-driver `DriverHandle` marker returned by [`register`].
const REGISTER_HANDLE_MARKER: u64 = 0x564E_4250_0000_0001; // "VBKP"

/// The virtio device id of a block device (virtio 1.1 §5.2 — `virtio-blk`
/// is device type 2). This driver's [`BIND_KEYS`] match key is built from
/// it, so a discovered virtio node whose probed device id is 2 binds this
/// driver and nothing else.
pub const VIRTIO_BLK_DEVICE_ID: u32 = 2;

/// The bind priority [`BIND_KEYS`] carries.
///
/// A virtio device-id match is *exact* (the discovered node's probed
/// device id either is `virtio-blk` or it is not — there is no wildcard,
/// see [`HwMatchKey::matches`]), so it ranks at the exact-match tier
/// alongside the other concrete-identity floor drivers (higher matched priority binds; an unbroken tie is a packaging
/// defect).
const BIND_PRIORITY: u16 = 10;

/// This driver's hardware bind table: a virtio block
/// device, matched by its virtio device id ([`VIRTIO_BLK_DEVICE_ID`]).
///
/// The single source of truth the signed-manifest bind table is authored
/// from and the device manager (or the in-kernel bootstrap-floor
/// catalogue) resolves a discovered node against. The match key carries no transport (PCI vs MMIO) detail: the
/// same driver binds a virtio-blk device however it is attached, because
/// the bus-agnostic [`tairix_virtio::Transport`] abstracts the transport.
pub const BIND_KEYS: &[DriverBindKey] = &[DriverBindKey::new(
    BIND_PRIORITY,
    HwMatchKey::virtio(VIRTIO_BLK_DEVICE_ID),
)];

/// Upper bound on advisory completion wakes a single request tolerates
/// before failing closed.
///
/// A virtio-blk request is serialised by the owner's lock, so exactly one
/// completion is ever outstanding, and a healthy device posts it within a
/// wake or two of the notify (an early wake whose used-ring write is not
/// yet visible, or a wake for the shared line's other queue, costs one
/// extra re-poll). A count far above that turns a *pathological* stream of
/// wakes with no matching completion — a stuck or mis-routed shared
/// interrupt — into a deterministic [`DriverError::DeviceFault`] rather
/// than an unbounded loop, without ever tripping in normal operation.
const MAX_COMPLETION_WAKES: u32 = 1024;

/// Driver entry point.
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
    /// Cache-flush request type (virtio 1.1 §5.2.6, requires
    /// [`VIRTIO_BLK_F_FLUSH`]): commit the device's volatile write cache
    /// to the medium. Carries a header and status only — no data segment.
    pub const VIRTIO_BLK_T_FLUSH: u32 = 4;
    /// Discard request type (virtio 1.1 §5.2.6, requires
    /// [`VIRTIO_BLK_F_DISCARD`]).
    pub const VIRTIO_BLK_T_DISCARD: u32 = 11;
    /// `struct virtio_blk_req` header size: type(4) + reserved(4) + sector(8).
    pub const HEADER_LEN: usize = 16;
    pub const STATUS_LEN: usize = 1;
    pub const STATUS_OK: u8 = 0;
    pub const STATUS_IOERR: u8 = 1;
    /// Most bytes staged into the persistent data buffer for a single
    /// virtio transaction. A `read_blocks`/`write_blocks` call larger
    /// than this is split into block-aligned chunks of at most this
    /// size, so the driver's DMA footprint is a fixed staging window
    /// carved once at open rather than a per-request allocation. A
    /// multiple of [`SECTOR_SIZE`] so every chunk is block-aligned.
    pub const MAX_TRANSFER_LEN: usize = 32 * 1024;
    /// Device-config byte offset of the capacity (in 512-byte sectors).
    pub const CONFIG_CAPACITY_OFFSET: usize = 0;
    /// Sector size, fixed in `abi-v1` Stage 4. `VIRTIO_BLK_F_BLK_SIZE`
    /// extended-feature negotiation is deferred to Stage 5.
    pub const SECTOR_SIZE: u32 = 512;

    /// `VIRTIO_BLK_F_FLUSH` feature bit (virtio 1.1 §5.2.3): the device
    /// has a volatile write cache and honours the flush command. When it
    /// is *not* negotiated the device is write-through — a completed write
    /// is already on the medium — so a flush is unnecessary.
    pub const VIRTIO_BLK_F_FLUSH: u64 = 1 << 9;
    /// `VIRTIO_BLK_F_DISCARD` feature bit (virtio 1.1 §5.2.3): the
    /// device supports the discard command.
    pub const VIRTIO_BLK_F_DISCARD: u64 = 1 << 13;
    /// Device-config byte offset of `max_discard_sectors` (le32): the
    /// most 512-byte sectors a single discard command may cover.
    /// (`struct virtio_blk_config`: `capacity(8) size_max(4) seg_max(4)
    /// geometry(4) blk_size(4) topology(8) writeback(1) unused(1)
    /// num_queues(2)` = 36.)
    pub const CONFIG_MAX_DISCARD_SECTORS_OFFSET: usize = 36;
    /// Device-config byte offset of `discard_sector_alignment` (le32):
    /// the discard alignment/granularity in 512-byte sectors
    /// (`max_discard_sectors`(4) + `max_discard_seg`(4) after offset 36).
    pub const CONFIG_DISCARD_SECTOR_ALIGNMENT_OFFSET: usize = 44;
    /// Size of one `struct virtio_blk_discard_write_zeroes`:
    /// `sector(8) + num_sectors(4) + flags(4)`.
    pub const DISCARD_DESCRIPTOR_LEN: usize = 16;
}

/// Block device backed by a cross-arch virtio transport.
///
/// `'h` bounds the borrow of the [`VirtioHost`] the driver allocates
/// its DMA regions through. The host is *minted per driver load* by
/// a `VirtioHostFactory` (the seam defined in `lib/virtio`)
/// and lives only for the duration of that load, so the driver borrows
/// it for `'h` rather than demanding a `'static` host (per-process pools are reclaimed when the driver unloads).
pub struct VirtioBlk<'h, T: Transport> {
    transport: T,
    queue: SplitQueue,
    host: &'h dyn VirtioHost,
    block_size: u32,
    block_count: u64,
    /// Whether `VIRTIO_BLK_F_DISCARD` was negotiated at `open` and the
    /// device's discard limits read from its config window.
    discard: DiscardLimits,
    /// Whether `VIRTIO_BLK_F_FLUSH` was negotiated at `open`. When it was
    /// not, the device is write-through and [`Self::flush`] has nothing to
    /// commit.
    flush_supported: bool,
    /// Persistent request staging carved once at open and reused by
    /// every request: the `virtio_blk_req` header, the data buffer
    /// (sized to [`wire::MAX_TRANSFER_LEN`], the chunk unit), and the
    /// one-byte status. `None` only while a request holds them in
    /// class-aware [`BounceBuffer`] wrappers. Reusing them keeps the
    /// I/O data path off the DMA allocator (no per-request grant, and
    /// no audit-log entry per request).
    header: Option<DmaSlab>,
    data: Option<DmaSlab>,
    status: Option<DmaSlab>,
}

/// Negotiated discard limits, or `unsupported` when the device did not
/// offer `VIRTIO_BLK_F_DISCARD`.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
struct DiscardLimits {
    supported: bool,
    granularity_blocks: u64,
    max_blocks_per_request: u64,
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
        // Accept only the features we implement. `VIRTIO_BLK_F_DISCARD`
        // is the sole extended feature negotiated; everything else is
        // declined so the driver never claims behaviour it does not
        // honour.
        let device_features = transport.device_features();
        let discard_offered = device_features & wire::VIRTIO_BLK_F_DISCARD != 0;
        let flush_offered = device_features & wire::VIRTIO_BLK_F_FLUSH != 0;
        let mut driver_features = 0;
        if discard_offered {
            driver_features |= wire::VIRTIO_BLK_F_DISCARD;
        }
        if flush_offered {
            driver_features |= wire::VIRTIO_BLK_F_FLUSH;
        }
        transport.set_driver_features(driver_features);
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
        let discard = if discard_offered {
            let mut max = [0u8; 4];
            transport.read_config(wire::CONFIG_MAX_DISCARD_SECTORS_OFFSET, &mut max);
            let mut align = [0u8; 4];
            transport.read_config(wire::CONFIG_DISCARD_SECTOR_ALIGNMENT_OFFSET, &mut align);
            // A sector is one logical block here (block_size == 512). An
            // alignment of zero means "no alignment requirement"; the
            // capability granularity is always at least one block.
            let granularity_blocks = u64::from(u32::from_le_bytes(align)).max(1);
            DiscardLimits {
                supported: true,
                granularity_blocks,
                max_blocks_per_request: u64::from(u32::from_le_bytes(max)),
            }
        } else {
            DiscardLimits {
                supported: false,
                granularity_blocks: 0,
                max_blocks_per_request: 0,
            }
        };
        // Carve the persistent request staging once. Every request
        // reuses these three buffers, so the block data path never
        // touches the DMA allocator after open.
        let header = host
            .alloc_dma_zeroed(wire::HEADER_LEN)
            .map_err(|_| VirtioError::DeviceFault)?;
        let data = host
            .alloc_dma_zeroed(wire::MAX_TRANSFER_LEN)
            .map_err(|_| VirtioError::DeviceFault)?;
        let status = host
            .alloc_dma_zeroed(wire::STATUS_LEN)
            .map_err(|_| VirtioError::DeviceFault)?;
        Ok(Self {
            transport,
            queue,
            host,
            block_size: wire::SECTOR_SIZE,
            block_count: capacity_sectors,
            discard,
            flush_supported: flush_offered,
            header: Some(header),
            data: Some(data),
            status: Some(status),
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

    /// Number of blocks a byte length spans. The chunking callers pass
    /// a block-aligned length ([`wire::MAX_TRANSFER_LEN`] is a multiple
    /// of the block size and the caller buffer was validated aligned),
    /// so this divides evenly; it fails closed if the count would not
    /// fit a `u64`.
    fn blocks_in(&self, bytes: usize) -> Result<u64, DriverError> {
        u64::try_from(bytes / self.block_size as usize).map_err(|_| DriverError::LengthOutOfRange)
    }

    /// Run one virtio-blk request against the device, reusing the
    /// persistent header/data/status staging carved at open.
    ///
    /// The caller guarantees `payload.len()` fits the data staging
    /// window ([`wire::MAX_TRANSFER_LEN`]); the read/write entry points
    /// chunk larger transfers before calling in. The staging is always
    /// put back (even on a faulted request) so the next request reuses
    /// it — the data path never re-enters the DMA allocator.
    fn run_request(
        &mut self,
        req_type: u32,
        lba: u64,
        payload: Payload<'_>,
        class: BufferClass,
    ) -> Result<(), DriverError> {
        // Take the persistent staging and wrap it in class-aware
        // bounce buffers. Only the data buffer carries caller bytes, so
        // only it inherits `class` (scrubbed on return when sensitive);
        // the header (request type + LBA) and status byte never hold a
        // secret.
        let (Some(header), Some(data), Some(status)) =
            (self.header.take(), self.data.take(), self.status.take())
        else {
            return Err(DriverError::DeviceFault);
        };
        let mut header_bb = BounceBuffer::new(header, BufferClass::NonSensitive);
        let mut data_bb = BounceBuffer::new(data, class);
        let mut status_bb = BounceBuffer::new(status, BufferClass::NonSensitive);
        let result = self.exchange(
            &mut header_bb,
            &mut data_bb,
            &mut status_bb,
            req_type,
            lba,
            payload,
        );
        // Return the staging regardless of outcome (`into_slab` scrubs
        // the data buffer first when the class was sensitive).
        self.header = Some(header_bb.into_slab());
        self.data = Some(data_bb.into_slab());
        self.status = Some(status_bb.into_slab());
        result
    }

    /// Stage `payload` into the persistent buffers, publish the chain,
    /// wait for the device, and decode the result. Split out of
    /// [`Self::run_request`] so every early return still lets the
    /// caller put the staging slabs back.
    fn exchange(
        &mut self,
        header_bb: &mut BounceBuffer,
        data_bb: &mut BounceBuffer,
        status_bb: &mut BounceBuffer,
        req_type: u32,
        lba: u64,
        payload: Payload<'_>,
    ) -> Result<(), DriverError> {
        let payload_len = payload.len();
        let write_outbound = payload.is_write();
        // Stage header.
        header_bb
            .stage(&Self::build_header(req_type, lba))
            .map_err(|()| DriverError::BufferTooSmall)?;
        // Stage outbound data for writes; a read only needs the device
        // to have a buffer of `payload_len` bytes to fill. Fail closed
        // if the chunk does not fit the staging window (the chunking
        // entry points guarantee it does).
        match &payload {
            Payload::Write(src) => data_bb
                .stage(src)
                .map_err(|()| DriverError::BufferTooSmall)?,
            Payload::Read(_) => {
                if payload_len > data_bb.capacity() {
                    return Err(DriverError::BufferTooSmall);
                }
            }
        }
        // Build the descriptor chain. Header + data are read by the
        // device for writes; for reads only the header is. Status is
        // always device-write.
        let data_dir = if write_outbound {
            Direction::DeviceRead
        } else {
            Direction::DeviceWrite
        };
        let payload_len_u32 =
            u32::try_from(payload_len).map_err(|_| DriverError::LengthOutOfRange)?;
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
        self.submit_and_wait(&segments)?;
        // Decode status first, and copy device-written data back to the
        // caller only on success. The data staging is a persistent
        // buffer reused across requests, so copying on a faulted or
        // unsupported request could hand the caller stale bytes left by
        // an earlier request — fail closed and copy nothing instead.
        let status_byte = status_bb.full_region_mut()[0];
        match status_byte {
            wire::STATUS_OK => {
                if let Payload::Read(dst) = payload {
                    // The device wrote `payload_len` bytes into the
                    // staging through its phys-mapped slice;
                    // `full_region_mut` gives us a CPU view of them.
                    dst.copy_from_slice(&data_bb.full_region_mut()[..payload_len]);
                }
                Ok(())
            }
            wire::STATUS_IOERR => Err(DriverError::DeviceFault),
            _ => Err(DriverError::Unsupported),
        }
    }

    /// Publish `segments` on the requestq, kick the device, and wait for
    /// the single outstanding completion, acknowledging the device
    /// interrupt before returning.
    ///
    /// A virtio device signals a finished request by posting a used-ring
    /// entry and raising its single interrupt line; the host notifier
    /// (`notify_wait`) parks the calling task until that line fires (the
    /// mock host drains the queue and returns). A wake is only *advisory*:
    /// the device's used-`idx` DMA write and its interrupt can be observed
    /// in either order, and the one shared line can wake us for an
    /// unrelated queue, so a single empty `poll_used` is never proof that
    /// no completion is coming. Re-scan the ring after every wake and wait
    /// again only when it is genuinely empty — the request is serialised
    /// by the owner's lock, so exactly one completion is outstanding and
    /// the loop ends when it lands. `MAX_COMPLETION_WAKES` bounds a
    /// pathological wake-storm (a stuck shared line delivering wakes with
    /// no matching completion) so the wait fails closed with `DeviceFault`
    /// rather than looping forever.
    fn submit_and_wait(&mut self, segments: &[ChainSegment]) -> Result<(), DriverError> {
        self.queue
            .add_chain(segments)
            .map_err(VirtioError::as_driver_error)?;
        self.queue.kick(&mut self.transport);
        let mut outcome: Result<(), DriverError> = Err(DriverError::DeviceFault);
        for _ in 0..MAX_COMPLETION_WAKES {
            match self.queue.poll_used() {
                Ok(_token) => {
                    outcome = Ok(());
                    break;
                }
                Err(VirtioError::NoCompletion) => {
                    self.host.notify_wait(self.queue.index());
                }
                Err(e) => {
                    outcome = Err(e.as_driver_error());
                    break;
                }
            }
        }
        // Acknowledge the device's interrupt now that its completion has
        // been observed (or the wait gave up), so it de-asserts its line
        // before the next request re-arms the kernel IRQ (otherwise a
        // stale edge re-delivers and the following request mis-pairs its
        // completion). Done regardless of the outcome — a faulted or
        // abandoned request must still leave the device's line clear. A
        // no-op on transports that need no device-side ack (MSI-X PCI, the
        // mock).
        self.transport.ack_interrupt();
        outcome
    }

    /// Issue a `VIRTIO_BLK_T_FLUSH` request, committing the device's
    /// volatile write cache to the medium. The flush request carries a
    /// header and a status byte only — no data segment — so it reuses the
    /// persistent header/status staging and leaves the data slab in place.
    fn run_flush(&mut self) -> Result<(), DriverError> {
        let (Some(header), Some(status)) = (self.header.take(), self.status.take()) else {
            return Err(DriverError::DeviceFault);
        };
        let mut header_bb = BounceBuffer::new(header, BufferClass::NonSensitive);
        let mut status_bb = BounceBuffer::new(status, BufferClass::NonSensitive);
        let result = self.exchange_flush(&mut header_bb, &mut status_bb);
        self.header = Some(header_bb.into_slab());
        self.status = Some(status_bb.into_slab());
        result
    }

    /// Stage the flush header, publish the two-descriptor chain, wait, and
    /// decode the status. Split out so [`Self::run_flush`] always puts the
    /// staging slabs back, mirroring [`Self::exchange`].
    fn exchange_flush(
        &mut self,
        header_bb: &mut BounceBuffer,
        status_bb: &mut BounceBuffer,
    ) -> Result<(), DriverError> {
        header_bb
            .stage(&Self::build_header(wire::VIRTIO_BLK_T_FLUSH, 0))
            .map_err(|()| DriverError::BufferTooSmall)?;
        let segments = [
            ChainSegment {
                phys: header_bb.phys(),
                len: u32::try_from(wire::HEADER_LEN).unwrap_or(0),
                direction: Direction::DeviceRead,
            },
            ChainSegment {
                phys: status_bb.phys(),
                len: u32::try_from(wire::STATUS_LEN).unwrap_or(0),
                direction: Direction::DeviceWrite,
            },
        ];
        self.submit_and_wait(&segments)?;
        match status_bb.full_region_mut()[0] {
            wire::STATUS_OK => Ok(()),
            wire::STATUS_IOERR => Err(DriverError::DeviceFault),
            _ => Err(DriverError::Unsupported),
        }
    }
}

/// The caller data a single [`VirtioBlk::run_request`] carries, keeping
/// the read (device-write) and write (device-read) directions distinct
/// so the write path stages straight from the caller's `&[u8]` with no
/// intermediate copy.
enum Payload<'a> {
    /// The device writes this many bytes into the caller's buffer.
    Read(&'a mut [u8]),
    /// The device reads the caller's bytes.
    Write(&'a [u8]),
}

impl Payload<'_> {
    fn len(&self) -> usize {
        match self {
            Payload::Read(b) => b.len(),
            Payload::Write(b) => b.len(),
        }
    }

    fn is_write(&self) -> bool {
        matches!(self, Payload::Write(_))
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
    fn flush(&mut self) -> Result<(), DriverError> {
        // If the device did not negotiate `VIRTIO_BLK_F_FLUSH` it has no
        // volatile write cache — a completed write is already on the
        // medium — so there is nothing to commit. Otherwise issue the
        // hardware flush and fail closed if the device cannot confirm it.
        if !self.flush_supported {
            return Ok(());
        }
        self.run_flush()
    }
    fn read_blocks_with_class(
        &mut self,
        lba: u64,
        buf: &mut [u8],
        class: BufferClass,
    ) -> Result<(), DriverError> {
        self.validate_block_op(lba, buf.len())?;
        // Split the transfer into staging-window-sized, block-aligned
        // chunks so the persistent buffers are reused for a request of
        // any size: the driver's DMA footprint stays fixed rather than
        // scaling with the request length.
        let mut cur_lba = lba;
        let mut off = 0;
        while off < buf.len() {
            let chunk = (buf.len() - off).min(wire::MAX_TRANSFER_LEN);
            self.run_request(
                wire::VIRTIO_BLK_T_IN,
                cur_lba,
                Payload::Read(&mut buf[off..off + chunk]),
                class,
            )?;
            cur_lba += self.blocks_in(chunk)?;
            off += chunk;
        }
        Ok(())
    }
    fn write_blocks_with_class(
        &mut self,
        lba: u64,
        buf: &[u8],
        class: BufferClass,
    ) -> Result<(), DriverError> {
        self.validate_block_op(lba, buf.len())?;
        // Stage straight from the caller's slice, one block-aligned
        // chunk at a time, so the write path neither allocates a
        // temporary copy nor re-enters the DMA allocator. The sensitive
        // scrub happens inside the bounce buffer on return, so no
        // secret bytes outlive the request.
        let mut cur_lba = lba;
        let mut off = 0;
        while off < buf.len() {
            let chunk = (buf.len() - off).min(wire::MAX_TRANSFER_LEN);
            self.run_request(
                wire::VIRTIO_BLK_T_OUT,
                cur_lba,
                Payload::Write(&buf[off..off + chunk]),
                class,
            )?;
            cur_lba += self.blocks_in(chunk)?;
            off += chunk;
        }
        Ok(())
    }
    fn discard_capability(&self) -> Result<DiscardCapability, DriverError> {
        Ok(if self.discard.supported {
            DiscardCapability {
                supported: true,
                granularity_blocks: self.discard.granularity_blocks,
                max_blocks_per_request: self.discard.max_blocks_per_request,
            }
        } else {
            DiscardCapability::unsupported()
        })
    }
    fn discard(&mut self, lba: u64, blocks: u64) -> Result<(), DriverError> {
        if !self.discard.supported {
            return Err(DriverError::Unsupported);
        }
        if blocks == 0 {
            return Ok(());
        }
        let end = lba
            .checked_add(blocks)
            .ok_or(DriverError::LengthOutOfRange)?;
        if end > self.block_count {
            return Err(DriverError::LengthOutOfRange);
        }
        if self.discard.max_blocks_per_request != 0 && blocks > self.discard.max_blocks_per_request
        {
            return Err(DriverError::LengthOutOfRange);
        }
        // One `struct virtio_blk_discard_write_zeroes`: sector(8) +
        // num_sectors(4) + flags(4). The device reads it; the request
        // carries no caller data, so it is never sensitive.
        let num_sectors = u32::try_from(blocks).map_err(|_| DriverError::LengthOutOfRange)?;
        let mut descriptor = [0u8; wire::DISCARD_DESCRIPTOR_LEN];
        descriptor[0..8].copy_from_slice(&lba.to_le_bytes());
        descriptor[8..12].copy_from_slice(&num_sectors.to_le_bytes());
        // flags [12..16] stay zero: no UNMAP flag in `abi-v1`.
        self.run_request(
            wire::VIRTIO_BLK_T_DISCARD,
            0,
            Payload::Write(&descriptor),
            BufferClass::NonSensitive,
        )
    }
}

#[cfg(test)]
mod tests;
