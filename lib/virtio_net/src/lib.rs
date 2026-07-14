//! RustOS arch-neutral virtio-net link-layer device logic.
//!
//! Implements [`rustos_abi::driver::net::Net`] on top of the
//! cross-arch virtio transport from `lib/virtio`. As with
//! `virtio_blk`, the device logic is bus-agnostic: the same source
//! compiles against the PCI and MMIO transports
//! — the queue protocol lives once, in the transport crate.
//!
//! This is a `lib/*` device-logic crate (the `lib/virtio_input`
//! precedent): the virtio-net driver crate
//! (`drivers/network/virtio_net`) links it, and so do the host tests
//! and the QEMU verticals, so the device engine is written once and
//! never re-implemented (§2.2, §17.4). Living in `lib/*` (rather than a
//! `drivers/*` crate) is what lets a future user-space driver *process*
//! link the engine directly: a process crate may depend on `lib/*` but
//! never on another `drivers/*` crate (§17.4).
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
//! [`VirtioNet`] is the device engine: a consumer brings it up with
//! [`VirtioNet::open`] and drives frame I/O through the [`Net`] trait.
//! The driver-host `register` shell that wraps it (and the load-time
//! `CAP_DRV_LOAD` gate) lives in the `rustos-drv-network-virtio-net`
//! crate, which re-exports this type.
//!
//! # Capabilities
//!
//! This crate holds no capability checks: it is pure device logic. The
//! dispatcher above it verifies `CAP_NET_RAW` before frame I/O and the
//! host verifies `CAP_DRV_LOAD` at load (see `lib/abi/src/driver/net.rs`
//! and the driver-shell crate).
//!
//! # Frame rings
//!
//! Frame I/O is [`Net::service`] over the shared-memory frame-ring
//! transport (`plans/NETWORK.md` §2.3): every call drains the TX
//! ring into the device and harvests delivered frames into the RX
//! ring. When nothing moved at all, the call parks once on the
//! host's device-event waiter (`notify_wait`) and re-checks, so a
//! caller looping on `service` stays event-driven, never a spin. A
//! harvested frame that finds the RX ring full is kept staged (the
//! receive chain is simply not re-posted) and delivered by the next
//! call — back-pressure without loss.
//!
//! # Staging buffers
//!
//! DMA staging is allocated **once**, at [`VirtioNet::open`]: one
//! header + one MTU-sized frame buffer per direction, reused for
//! every packet. The receive pair is posted as a device-write chain
//! at open and re-posted after every delivered completion, so the
//! device always owns a receive buffer and an idle `service` poll
//! touches no allocator — no per-packet `dma_alloc`/`dma_free`
//! round trip, no per-poll audit-log traffic on the hot path.
//!
//! # Zero-on-free
//!
//! [`Net::service`] honours a
//! [`BufferClass::Sensitive`](rustos_abi::driver::BufferClass) ring
//! class by scrubbing the persistent staging through
//! [`rustos_virtio::BounceBuffer::into_slab`] before the buffers are
//! reused for the next packet; a harvested frame still awaiting a
//! free RX slot is scrubbed after it is delivered, never before.

#![no_std]
#![forbid(unsafe_op_in_unsafe_fn)]
#![deny(missing_docs)]

use core::convert::TryFrom;
use rustos_abi::driver::net::{
    DeviceFacts, LinkState, MacAddress, Net, NetOffloads, MAC_ADDRESS_LEN,
};
use rustos_abi::driver::net_ring::{FrameRings, ServiceReport};
use rustos_abi::DriverError;
use rustos_abi::Errno;
use rustos_virtio::{
    BounceBuffer, ChainSegment, Direction, DmaSlab, SplitQueue, Status, Transport, VirtioError,
    VirtioHost,
};

/// Bounded parks per in-flight transmission. The host's device event is
/// shared by every queue, so receive traffic can wake the transmit wait
/// before the chain is consumed; each such wake parks again rather than
/// faulting. A healthy device consumes a transmit chain within a wake
/// or two — exhausting this budget means the device is genuinely stuck,
/// and the transmit fails closed.
const MAX_TX_WAITS: u32 = 64;

/// Virtio-net wire protocol constants (virtio 1.1 §5.1).
mod wire {
    /// `struct virtio_net_hdr` (legacy, no mergeable buffers).
    /// Five 1-byte/2-byte fields totalling 10 bytes; Stage 4 always
    /// writes them as zeroes (no offloads negotiated).
    pub const HEADER_LEN: usize = 10;
    /// Minimum Ethernet frame size (excluding FCS).
    pub const MIN_ETHERNET_FRAME: usize = 14;
    /// Link MTU: the largest link-layer payload (IP packet) carried.
    pub const LINK_MTU: usize = 1500;
    /// Largest frame moved: the link MTU plus the Ethernet header.
    pub const MAX_FRAME_LEN: usize = LINK_MTU + MIN_ETHERNET_FRAME;
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
    max_frame_len: usize,
    /// Persistent receive staging (virtio-net header + frame buffer)
    /// the pre-posted receive chain points at. Carved once at open and
    /// reused for every frame; `None` only while a `receive` call
    /// holds the pair in class-aware [`BounceBuffer`] wrappers.
    rx_header: Option<DmaSlab>,
    rx_data: Option<DmaSlab>,
    /// Persistent transmit staging, reused by every serviced frame.
    tx_header: Option<DmaSlab>,
    tx_data: Option<DmaSlab>,
    /// Length of a harvested frame still waiting for RX-ring space
    /// (the receive chain stays un-posted while this is `Some`).
    rx_staged_len: Option<usize>,
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
            .alloc_dma_zeroed(wire::MAX_FRAME_LEN)
            .map_err(|_| VirtioError::DeviceFault)?;
        let tx_header = host
            .alloc_dma_zeroed(wire::HEADER_LEN)
            .map_err(|_| VirtioError::DeviceFault)?;
        let tx_data = host
            .alloc_dma_zeroed(wire::MAX_FRAME_LEN)
            .map_err(|_| VirtioError::DeviceFault)?;
        let mut net = Self {
            transport,
            rx_queue,
            tx_queue,
            host,
            mac: MacAddress::new(mac),
            max_frame_len: wire::MAX_FRAME_LEN,
            rx_header: Some(rx_header),
            rx_data: Some(rx_data),
            tx_header: Some(tx_header),
            tx_data: Some(tx_data),
            rx_staged_len: None,
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

    /// Drain every frame queued in the TX ring into the device.
    fn drain_tx(
        &mut self,
        rings: &mut FrameRings<'_>,
        report: &mut ServiceReport,
    ) -> Result<(), DriverError> {
        loop {
            // Stage through the class-aware wrappers; `into_slab`
            // scrubs the staging before it is put back when the ring
            // class declares the traffic sensitive.
            let (Some(header_slab), Some(data_slab)) = (self.tx_header.take(), self.tx_data.take())
            else {
                return Err(DriverError::DeviceFault);
            };
            let mut header_bb = BounceBuffer::new(header_slab, rings.class);
            let mut data_bb = BounceBuffer::new(data_slab, rings.class);
            let outcome = self.tx_one(rings, &mut header_bb, &mut data_bb);
            self.tx_header = Some(header_bb.into_slab());
            self.tx_data = Some(data_bb.into_slab());
            match outcome? {
                TxOutcome::Sent => report.transmitted += 1,
                TxOutcome::Dropped => {}
                TxOutcome::Empty => return Ok(()),
            }
        }
    }

    /// Pop one frame from the TX ring into the wrapped staging and
    /// hand it to the device, waiting for consumption. A frame the
    /// device cannot move (runt, over-MTU, corrupt slot) is consumed
    /// and dropped so the queue behind it keeps flowing.
    fn tx_one(
        &mut self,
        rings: &mut FrameRings<'_>,
        header_bb: &mut BounceBuffer,
        data_bb: &mut BounceBuffer,
    ) -> Result<TxOutcome, DriverError> {
        let staging = data_bb.full_region_mut();
        let cap = self.max_frame_len.min(staging.len());
        let len = match rings.tx.pop(&mut staging[..cap]) {
            Ok(Some(len)) => len,
            Ok(None) => return Ok(TxOutcome::Empty),
            // Longer than the device moves: consume it and go on.
            Err(Errno::BufferTooSmall) => {
                rings.tx.skip().map_err(|_| DriverError::BadMagic)?;
                return Ok(TxOutcome::Dropped);
            }
            // A corrupt slot was already consumed by the pop.
            Err(Errno::LengthOutOfRange) => return Ok(TxOutcome::Dropped),
            Err(_) => return Err(DriverError::BadMagic),
        };
        if len < wire::MIN_ETHERNET_FRAME {
            return Ok(TxOutcome::Dropped);
        }
        header_bb
            .stage(&Self::build_header())
            .map_err(|()| DriverError::BufferTooSmall)?;
        let frame_len_u32 = u32::try_from(len).map_err(|_| DriverError::LengthOutOfRange)?;
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
        // The device event the host waits on is shared by every queue,
        // so a wake may announce *receive* traffic that landed while
        // this transmission was still in flight. A not-yet-consumed
        // chain after such a wake is normal: park again and re-check,
        // within a bounded budget that fails closed on a genuinely
        // stuck device.
        let mut waits = 0;
        loop {
            self.host.notify_wait(self.tx_queue.index());
            match self.tx_queue.poll_used() {
                Ok(_token) => return Ok(TxOutcome::Sent),
                Err(VirtioError::NoCompletion) => {
                    waits += 1;
                    if waits >= MAX_TX_WAITS {
                        return Err(DriverError::DeviceFault);
                    }
                }
                Err(e) => return Err(e.as_driver_error()),
            }
        }
    }

    /// Move delivered frames from the device into the RX ring until
    /// the device is drained or the ring is full.
    ///
    /// A frame whose completion was harvested but which found the
    /// ring full is *kept staged* — the receive chain is not
    /// re-posted until it is delivered — so back-pressure never drops
    /// a frame the device already handed over.
    fn harvest_rx(
        &mut self,
        rings: &mut FrameRings<'_>,
        report: &mut ServiceReport,
    ) -> Result<(), DriverError> {
        loop {
            if let Some(len) = self.rx_staged_len {
                let Some(data_slab) = self.rx_data.as_mut() else {
                    return Err(DriverError::DeviceFault);
                };
                match rings.rx.push(&data_slab.as_bytes()[..len]) {
                    Ok(()) => {
                        self.rx_staged_len = None;
                        report.received += 1;
                        // The frame left the staging: scrub it now
                        // when the ring class demands it, then hand
                        // the buffer back to the device.
                        if rings.class.is_sensitive() {
                            self.scrub_rx_staging();
                        }
                        self.post_receive_chain()?;
                    }
                    Err(Errno::NoSpace) => {
                        report.rx_ring_full = true;
                        return Ok(());
                    }
                    Err(_) => return Err(DriverError::BadMagic),
                }
            } else {
                match self.rx_queue.poll_used() {
                    Ok(token) => {
                        let total = token.written as usize;
                        let frame_len = total.saturating_sub(wire::HEADER_LEN);
                        if frame_len == 0 {
                            // An empty completion carries nothing for
                            // the stack: just re-arm the device.
                            self.post_receive_chain()?;
                            continue;
                        }
                        self.rx_staged_len = Some(frame_len.min(self.max_frame_len));
                    }
                    Err(VirtioError::NoCompletion) => return Ok(()),
                    Err(e) => return Err(e.as_driver_error()),
                }
            }
        }
    }

    /// Zero the persistent receive staging (header + frame buffer).
    fn scrub_rx_staging(&mut self) {
        for slab in [self.rx_header.as_mut(), self.rx_data.as_mut()]
            .into_iter()
            .flatten()
        {
            slab.as_bytes_mut().fill(0);
        }
    }
}

/// Outcome of one TX-ring pop inside [`VirtioNet::tx_one`].
enum TxOutcome {
    /// A frame was handed to the device.
    Sent,
    /// A frame the device cannot move was consumed and dropped.
    Dropped,
    /// The TX ring is empty.
    Empty,
}

impl<T: Transport> Net for VirtioNet<'_, T> {
    fn device_facts(&self) -> Result<DeviceFacts, DriverError> {
        // No extended features are negotiated: no VIRTIO_NET_F_STATUS
        // (an operational device reports its link up), no offloads,
        // one receive queue.
        Ok(DeviceFacts {
            mac: self.mac,
            mtu: u32::try_from(self.max_frame_len - wire::MIN_ETHERNET_FRAME)
                .map_err(|_| DriverError::LengthOutOfRange)?,
            link: LinkState::Up,
            offloads: NetOffloads::empty(),
            rx_queues: 1,
        })
    }

    fn service(&mut self, rings: &mut FrameRings<'_>) -> Result<ServiceReport, DriverError> {
        let mut report = ServiceReport::default();
        self.drain_tx(rings, &mut report)?;
        self.harvest_rx(rings, &mut report)?;
        if report == ServiceReport::default() {
            // Nothing moved: park once on the device event so a
            // caller looping on `service` waits instead of spinning,
            // then re-check for a completion the wake announced.
            self.host.notify_wait(self.rx_queue.index());
            self.harvest_rx(rings, &mut report)?;
        }
        Ok(report)
    }
}

#[cfg(test)]
mod tests;
