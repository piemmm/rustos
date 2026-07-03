//! RustOS virtio-net link-layer driver.
//!
//! Implements [`rustos_abi::driver::net::Net`] on top of the
//! cross-arch virtio transport from `drivers/bus/virtio`. As with
//! `virtio_blk`, the driver is bus-agnostic: the same source
//! compiles against the PCI and MMIO transports (
//! — the queue protocol lives once, in the transport crate).
//!
//! # Wire protocol
//!
//! Virtio-net 1.1. Stage 4 supports the legacy "no extended
//! features" subset: a `struct virtio_net_hdr` of all zeros prefixes
//! every transmit and receive descriptor chain, the device negotiates
//! `VIRTIO_NET_F_MAC` to publish a stable link-layer address, and
//! no checksum / GSO offloads are advertised. Two queues are used:
//! receive queue `0` and transmit queue `1` (virtio-net §5.1.2).
//!
//! Higher-layer protocols (ARP, IP, ICMP) live above this trait in
//! user space and are out of scope for `abi-v1` (see
//! `lib/abi/src/driver/net.rs`).
//!
//! # Public surface
//!
//! Per the only public *function* is [`register`].
//! [`VirtioNet`] is a public *type* re-exported so the driver host
//! can instantiate it; the host never reaches the type beyond the
//! [`Net`] trait.
//!
//! # Capabilities
//!
//! Loading requires [`CapabilityId::DRV_LOAD`]; `transmit` and
//! `receive` additionally require the dispatcher to have verified
//! [`CapabilityId::NET_RAW`] (see `lib/abi/src/driver/net.rs`).
//!
//! # Staging buffers
//!
//! DMA staging is allocated **once**, at [`VirtioNet::open`]: one
//! header + one MTU-sized frame buffer per direction, reused for
//! every packet. The receive pair is posted as a device-write chain
//! at open and re-posted after every harvested completion, so the
//! device always owns a receive buffer and an idle `receive` poll
//! touches no allocator — no per-packet `dma_alloc`/`dma_free`
//! round trip, no per-poll audit-log traffic on the hot path.
//!
//! # Zero-on-free
//!
//! [`Net::transmit_with_class`] and [`Net::receive_with_class`]
//! honour [`BufferClass::Sensitive`](rustos_abi::driver::BufferClass)
//! by scrubbing the persistent staging through
//! [`rustos_virtio::BounceBuffer::into_slab`] before the buffers are
//! reused for the next packet.

#![no_std]
#![forbid(unsafe_op_in_unsafe_fn)]
#![deny(missing_docs)]

use core::convert::TryFrom;
use rustos_abi::driver::net::{MacAddress, Net, MAC_ADDRESS_LEN};
use rustos_abi::driver::BufferClass;
use rustos_abi::{CapabilityId, DriverError, DriverHandle, DriverHost};
use rustos_virtio::{
    BounceBuffer, ChainSegment, Direction, DmaSlab, SplitQueue, Status, Transport, VirtioError,
    VirtioHost,
};

/// Per-driver `DriverHandle` marker returned by [`register`].
const REGISTER_HANDLE_MARKER: u64 = 0x564E_4554_0000_0001; // "VNET"

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

/// Virtio-net wire protocol constants (virtio 1.1 §5.1).
mod wire {
    /// `struct virtio_net_hdr` (legacy, no mergeable buffers).
    /// Five 1-byte/2-byte fields totalling 10 bytes; Stage 4 always
    /// writes them as zeroes (no offloads negotiated).
    pub const HEADER_LEN: usize = 10;
    /// Minimum Ethernet frame size (excluding FCS).
    pub const MIN_ETHERNET_FRAME: usize = 14;
    /// Default MTU (1500-byte payload + 14-byte Ethernet header).
    pub const DEFAULT_MTU: usize = 1500 + MIN_ETHERNET_FRAME;
    /// Receive queue index (virtio-net §5.1.2).
    pub const RX_QUEUE: u16 = 0;
    /// Transmit queue index (virtio-net §5.1.2).
    pub const TX_QUEUE: u16 = 1;
    /// Receive queue size (descriptors). Power-of-two per virtio §2.6.
    pub const RX_QUEUE_SIZE: u16 = 8;
    /// Transmit queue size (descriptors). Power-of-two per virtio §2.6.
    pub const TX_QUEUE_SIZE: u16 = 8;
    /// MAC address byte offset in the device-configuration window.
    pub const CONFIG_MAC_OFFSET: usize = 0;
}

/// Network device backed by a cross-arch virtio transport.
///
/// `'h` bounds the borrow of the [`VirtioHost`] the driver allocates
/// its DMA regions through. The host is *minted per driver load* by
/// a `VirtioHostFactory` (the seam defined in `lib/virtio`)
/// and lives only for the duration of that load, so the driver borrows
/// it for `'h` rather than demanding a `'static` host (per-process pools are reclaimed when the driver unloads). This
/// mirrors [`VirtioBlk`](../rustos_drv_storage_virtio_blk/struct.VirtioBlk.html).
pub struct VirtioNet<'h, T: Transport> {
    transport: T,
    rx_queue: SplitQueue,
    tx_queue: SplitQueue,
    host: &'h dyn VirtioHost,
    mac: MacAddress,
    mtu: usize,
    /// Persistent receive staging (virtio-net header + frame buffer)
    /// the pre-posted receive chain points at. Carved once at open and
    /// reused for every frame; `None` only while a `receive` call
    /// holds the pair in class-aware [`BounceBuffer`] wrappers.
    rx_header: Option<DmaSlab>,
    rx_data: Option<DmaSlab>,
    /// Persistent transmit staging, reused by every `transmit`.
    tx_header: Option<DmaSlab>,
    tx_data: Option<DmaSlab>,
}

impl<'h, T: Transport> VirtioNet<'h, T> {
    /// Bring the device online.
    ///
    /// Implements the virtio-1.1 §3.1 initialisation sequence:
    /// reset, ACKNOWLEDGE, DRIVER, feature negotiation (Stage 4
    /// accepts zero extended features), `FEATURES_OK`, set up the
    /// receive and transmit queues, `DRIVER_OK`, then read the MAC
    /// from the device-configuration window.
    ///
    /// # Errors
    ///
    /// Propagates [`VirtioError`] from the transport / queue setup.
    /// Returns [`VirtioError::FeaturesRejected`] if the device
    /// clears [`Status::FEATURES_OK`] after the driver completed
    /// negotiation.
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
        let rx_queue = SplitQueue::new(&mut transport, host, wire::RX_QUEUE, wire::RX_QUEUE_SIZE)?;
        let tx_queue = SplitQueue::new(&mut transport, host, wire::TX_QUEUE, wire::TX_QUEUE_SIZE)?;
        status = status.with(Status::DRIVER_OK);
        transport.set_status(status);
        // Read MAC from device-config.
        let mut mac = [0u8; MAC_ADDRESS_LEN];
        transport.read_config(wire::CONFIG_MAC_OFFSET, &mut mac);
        // Carve the persistent staging once: one header + one
        // MTU-sized frame buffer per direction, reused for every
        // packet so the polled receive path never touches the
        // allocator.
        let rx_header = host
            .alloc_dma_zeroed(wire::HEADER_LEN)
            .map_err(|_| VirtioError::DeviceFault)?;
        let rx_data = host
            .alloc_dma_zeroed(wire::DEFAULT_MTU)
            .map_err(|_| VirtioError::DeviceFault)?;
        let tx_header = host
            .alloc_dma_zeroed(wire::HEADER_LEN)
            .map_err(|_| VirtioError::DeviceFault)?;
        let tx_data = host
            .alloc_dma_zeroed(wire::DEFAULT_MTU)
            .map_err(|_| VirtioError::DeviceFault)?;
        let mut net = Self {
            transport,
            rx_queue,
            tx_queue,
            host,
            mac: MacAddress::new(mac),
            mtu: wire::DEFAULT_MTU,
            rx_header: Some(rx_header),
            rx_data: Some(rx_data),
            tx_header: Some(tx_header),
            tx_data: Some(tx_data),
        };
        // Arm the receive path: the device owns a posted buffer from
        // DRIVER_OK onward, so a frame arriving before the first
        // `receive` call is captured rather than dropped.
        net.post_receive_chain()
            .map_err(|_| VirtioError::DeviceFault)?;
        Ok(net)
    }

    /// Post the persistent receive staging pair as the single
    /// device-write chain the device fills with the next frame, then
    /// notify the device.
    ///
    /// Called at open and again after every harvested completion, so
    /// exactly one receive chain is outstanding whenever the driver
    /// is idle — the buffers behind it stay owned by the driver for
    /// its whole life, never freed while the device can still write
    /// to them.
    fn post_receive_chain(&mut self) -> Result<(), DriverError> {
        let (Some(header), Some(data)) = (self.rx_header.as_ref(), self.rx_data.as_ref()) else {
            return Err(DriverError::DeviceFault);
        };
        let data_len = u32::try_from(data.len()).map_err(|_| DriverError::LengthOutOfRange)?;
        let segments = [
            ChainSegment {
                phys: header.phys(),
                len: u32::try_from(wire::HEADER_LEN).unwrap_or(0),
                direction: Direction::DeviceWrite,
            },
            ChainSegment {
                phys: data.phys(),
                len: data_len,
                direction: Direction::DeviceWrite,
            },
        ];
        self.rx_queue
            .add_chain(&segments)
            .map_err(VirtioError::as_driver_error)?;
        self.rx_queue.kick(&mut self.transport);
        Ok(())
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
    /// software peer to drive on `kick`.
    pub fn transport_mut(&mut self) -> &mut T {
        &mut self.transport
    }

    fn build_header() -> [u8; wire::HEADER_LEN] {
        // Stage 4 negotiates no offloads, so every field is zero.
        [0u8; wire::HEADER_LEN]
    }

    fn run_transmit(&mut self, frame: &[u8], class: BufferClass) -> Result<(), DriverError> {
        // Validate.
        if frame.len() < wire::MIN_ETHERNET_FRAME {
            return Err(DriverError::BufferTooSmall);
        }
        if frame.len() > self.mtu {
            return Err(DriverError::LengthOutOfRange);
        }
        // Stage into the persistent buffers through the class-aware
        // wrappers; `into_slab` scrubs the staging before it is put
        // back when the caller declared the payload sensitive.
        let (Some(header_slab), Some(data_slab)) = (self.tx_header.take(), self.tx_data.take())
        else {
            return Err(DriverError::DeviceFault);
        };
        let mut header_bb = BounceBuffer::new(header_slab, class);
        let mut data_bb = BounceBuffer::new(data_slab, class);
        let result = self.transmit_chain(&mut header_bb, &mut data_bb, frame);
        self.tx_header = Some(header_bb.into_slab());
        self.tx_data = Some(data_bb.into_slab());
        result
    }

    /// Stage `frame` into the wrapped transmit buffers, publish the
    /// chain, and wait for the device to consume it. Split out of
    /// [`Self::run_transmit`] so every early return still puts the
    /// staging slabs back (the wrappers stay owned by the caller).
    fn transmit_chain(
        &mut self,
        header_bb: &mut BounceBuffer,
        data_bb: &mut BounceBuffer,
        frame: &[u8],
    ) -> Result<(), DriverError> {
        header_bb
            .stage(&Self::build_header())
            .map_err(|()| DriverError::BufferTooSmall)?;
        data_bb
            .stage(frame)
            .map_err(|()| DriverError::BufferTooSmall)?;
        let frame_len_u32 =
            u32::try_from(frame.len()).map_err(|_| DriverError::LengthOutOfRange)?;
        let segments = [
            ChainSegment {
                phys: header_bb.phys(),
                len: u32::try_from(wire::HEADER_LEN).unwrap_or(0),
                direction: Direction::DeviceRead,
            },
            ChainSegment {
                phys: data_bb.phys(),
                len: frame_len_u32,
                direction: Direction::DeviceRead,
            },
        ];
        self.tx_queue
            .add_chain(&segments)
            .map_err(VirtioError::as_driver_error)?;
        self.tx_queue.kick(&mut self.transport);
        self.host.notify_wait(self.tx_queue.index());
        let _token = self
            .tx_queue
            .poll_used()
            .map_err(VirtioError::as_driver_error)?;
        Ok(())
    }

    fn run_receive(&mut self, buf: &mut [u8], class: BufferClass) -> Result<usize, DriverError> {
        if buf.is_empty() {
            return Err(DriverError::BufferTooSmall);
        }
        // Harvest the pre-posted chain. Poll before waiting so a
        // completion the device published earlier is never missed
        // (one interrupt can cover several completions); when the
        // ring is idle, wait once for the device event and re-check.
        // `NoCompletion` after the wait means no frame yet: the chain
        // stays posted and the call reports "nothing pending".
        let token = match self.rx_queue.poll_used() {
            Ok(t) => t,
            Err(VirtioError::NoCompletion) => {
                self.host.notify_wait(self.rx_queue.index());
                match self.rx_queue.poll_used() {
                    Ok(t) => t,
                    Err(VirtioError::NoCompletion) => return Ok(0),
                    Err(e) => return Err(e.as_driver_error()),
                }
            }
            Err(e) => return Err(e.as_driver_error()),
        };
        // The device reports total bytes written across the chain;
        // header consumes `HEADER_LEN`, the rest is frame payload.
        let total = token.written as usize;
        let frame_len = total.saturating_sub(wire::HEADER_LEN);
        // Copy out through the class-aware wrappers; `into_slab`
        // scrubs the persistent staging before the chain is re-posted
        // when the caller declared the traffic sensitive.
        let (Some(header_slab), Some(data_slab)) = (self.rx_header.take(), self.rx_data.take())
        else {
            return Err(DriverError::DeviceFault);
        };
        let header_bb = BounceBuffer::new(header_slab, class);
        let mut data_bb = BounceBuffer::new(data_slab, class);
        let result = if frame_len > buf.len() {
            Err(DriverError::BufferTooSmall)
        } else {
            if frame_len > 0 {
                buf[..frame_len].copy_from_slice(&data_bb.full_region_mut()[..frame_len]);
            }
            Ok(frame_len)
        };
        self.rx_header = Some(header_bb.into_slab());
        self.rx_data = Some(data_bb.into_slab());
        // Re-arm so the device always owns a receive buffer — even
        // when the caller's buffer was too small for this frame.
        self.post_receive_chain()?;
        result
    }
}

impl<T: Transport> Net for VirtioNet<'_, T> {
    fn mac_address(&self) -> Result<MacAddress, DriverError> {
        Ok(self.mac)
    }
    fn transmit(&mut self, frame: &[u8]) -> Result<(), DriverError> {
        self.run_transmit(frame, BufferClass::NonSensitive)
    }
    fn receive(&mut self, buf: &mut [u8]) -> Result<usize, DriverError> {
        self.run_receive(buf, BufferClass::NonSensitive)
    }
    fn transmit_with_class(&mut self, frame: &[u8], class: BufferClass) -> Result<(), DriverError> {
        self.run_transmit(frame, class)
    }
    fn receive_with_class(
        &mut self,
        buf: &mut [u8],
        class: BufferClass,
    ) -> Result<usize, DriverError> {
        self.run_receive(buf, class)
    }
}

#[cfg(test)]
mod tests;
