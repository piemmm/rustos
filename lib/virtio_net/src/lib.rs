//! TAIRiX arch-neutral virtio-net link-layer device logic.
//!
//! Implements [`tairix_abi::driver::net::Net`] on top of the
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
//! Virtio-net 1.1. A `struct virtio_net_hdr` prefixes every transmit
//! and receive descriptor chain; the device publishes its MAC through
//! the device-configuration window. Two queues are used: receive queue
//! `0` and transmit queue `1` (virtio-net §5.1.2).
//!
//! Two checksum-offload features are negotiated when the device offers
//! them. Receive (`VIRTIO_NET_F_GUEST_CSUM`): the device may mark a
//! received frame checksum-validated (`VIRTIO_NET_HDR_F_DATA_VALID`) or
//! deliver it with a partial checksum (`VIRTIO_NET_HDR_F_NEEDS_CSUM`);
//! the driver reports which, per frame, through the ring's offload
//! metadata ([`tairix_abi::driver::net_ring::FrameOffload`]) and the
//! stack skips or completes the fold. Transmit (`VIRTIO_NET_F_CSUM`):
//! the stack may hand a TCP segment carrying only the partial
//! (pseudo-header) checksum ([`FrameOffload::TxChecksum`]), and the
//! driver sets `VIRTIO_NET_HDR_F_NEEDS_CSUM` + `csum_start`/`csum_offset`
//! so the device completes it (`plans/NETWORK.md` §2.3). The driver does
//! no checksum arithmetic itself — it never links the stack — so the
//! kernel that links this crate for the device id stays free of
//! `lib/net`. No GSO/segmentation offload is advertised yet: `gso_type`
//! is always `GSO_NONE`.
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
//! `CAP_DRV_LOAD` gate) lives in the `tairix-drv-network-virtio-net`
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
//! [`BufferClass::Sensitive`](tairix_abi::driver::BufferClass) ring
//! class by scrubbing the persistent staging through
//! [`tairix_virtio::BounceBuffer::into_slab`] before the buffers are
//! reused for the next packet; a harvested frame still awaiting a
//! free RX slot is scrubbed after it is delivered, never before.

#![no_std]
#![forbid(unsafe_op_in_unsafe_fn)]
#![deny(missing_docs)]

mod channel;
pub use channel::NetChannelServer;

use core::convert::TryFrom;
use tairix_abi::driver::net::{
    DeviceFacts, LinkState, MacAddress, Net, NetOffloads, MAC_ADDRESS_LEN,
};
use tairix_abi::driver::net_ring::{FrameOffload, FrameRings, RingGeometry, ServiceReport};
use tairix_abi::DriverError;
use tairix_abi::Errno;
use tairix_virtio::{
    BounceBuffer, ChainSegment, Direction, DmaSlab, SplitQueue, Status, Transport, VirtioError,
    VirtioHost,
};

/// The virtio device id of a network device (virtio 1.1 §5.1 —
/// `virtio-net` is device type 1). The single definition both the driver
/// crate's bind table and the kernel's bootstrap-floor virtio-MMIO
/// discovery key a virtio-net node on, so a probed slot whose `DeviceID`
/// register is this value resolves to the virtio-net driver and nothing
/// else. Lives here in `lib/*` (not the `drivers/*` crate) so the
/// arch-neutral kernel discovery can name it without a
/// kernel→driver dependency (the `lib/virtio_input` precedent).
pub const VIRTIO_NET_DEVICE_ID: u32 = 1;

/// Bounded parks per in-flight transmission. The host's device event is
/// shared by every queue, so receive traffic can wake the transmit wait
/// before the chain is consumed; each such wake parks again rather than
/// faulting. A healthy device consumes a transmit chain within a wake
/// or two — exhausting this budget means the device is genuinely stuck,
/// and the transmit fails closed.
const MAX_TX_WAITS: u32 = 64;

/// Virtio-net wire protocol constants (virtio 1.1 §5.1).
mod wire {
    /// `struct virtio_net_hdr` (legacy, no mergeable buffers): a
    /// 10-byte header prefixing every transmit and receive descriptor
    /// chain. Field layout (virtio 1.1 §5.1.6): `flags` (1), `gso_type`
    /// (1), `hdr_len` (2), `gso_size` (2), `csum_start` (2),
    /// `csum_offset` (2), all little-endian.
    pub const HEADER_LEN: usize = 10;
    /// `flags` byte offset within `virtio_net_hdr`.
    pub const HDR_FLAGS_OFFSET: usize = 0;
    /// `gso_type` byte offset within `virtio_net_hdr`.
    pub const HDR_GSO_TYPE_OFFSET: usize = 1;
    /// `hdr_len` field offset within `virtio_net_hdr`.
    pub const HDR_HDR_LEN_OFFSET: usize = 2;
    /// `gso_size` field offset within `virtio_net_hdr`.
    pub const HDR_GSO_SIZE_OFFSET: usize = 4;
    /// `csum_start` field offset within `virtio_net_hdr`.
    pub const HDR_CSUM_START_OFFSET: usize = 6;
    /// `csum_offset` field offset within `virtio_net_hdr`.
    pub const HDR_CSUM_OFFSET_OFFSET: usize = 8;
    // `VIRTIO_NET_HDR_GSO_NONE` is 0 (the header's zero-initialised
    // default when no segmentation is requested), so no named constant is
    // needed for it.
    /// `VIRTIO_NET_HDR_GSO_TCPV4`: segment as IPv4 TCP.
    pub const VIRTIO_NET_HDR_GSO_TCPV4: u8 = 1;
    /// `VIRTIO_NET_HDR_GSO_TCPV6`: segment as IPv6 TCP.
    pub const VIRTIO_NET_HDR_GSO_TCPV6: u8 = 4;
    /// `VIRTIO_NET_HDR_F_NEEDS_CSUM` (virtio 1.1 §5.1.6.2): the frame's
    /// transport checksum is only partial and the driver must complete
    /// it using `csum_start`/`csum_offset`.
    pub const VIRTIO_NET_HDR_F_NEEDS_CSUM: u8 = 1;
    /// `VIRTIO_NET_HDR_F_DATA_VALID` (virtio 1.1 §5.1.6.2): the device
    /// has verified the frame's transport checksum.
    pub const VIRTIO_NET_HDR_F_DATA_VALID: u8 = 2;
    /// `VIRTIO_NET_F_GUEST_CSUM` (virtio 1.1 §5.1.4, feature bit 1): the
    /// device may deliver receive frames it has checksum-validated
    /// (`DATA_VALID`) or left with a partial checksum (`NEEDS_CSUM`).
    pub const VIRTIO_NET_F_GUEST_CSUM: u64 = 1 << 1;
    /// `VIRTIO_NET_F_CSUM` (virtio 1.1 §5.1.4, feature bit 0): the device
    /// completes the transport checksum of a transmit frame the driver
    /// marks `NEEDS_CSUM`, so the stack may hand the device a segment
    /// carrying only the partial (pseudo-header) checksum.
    pub const VIRTIO_NET_F_CSUM: u64 = 1 << 0;
    /// `VIRTIO_NET_F_HOST_TSO4` (virtio 1.1 §5.1.4, feature bit 11): the
    /// device segments an over-size IPv4 TCP frame the driver marks
    /// `GSO_TCPV4` into MTU-sized packets. Requires `VIRTIO_NET_F_CSUM`.
    pub const VIRTIO_NET_F_HOST_TSO4: u64 = 1 << 11;
    /// `VIRTIO_NET_F_HOST_TSO6` (virtio 1.1 §5.1.4, feature bit 12): the
    /// IPv6 counterpart of [`VIRTIO_NET_F_HOST_TSO4`].
    pub const VIRTIO_NET_F_HOST_TSO6: u64 = 1 << 12;
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
/// mirrors [`VirtioBlk`](../tairix_drv_storage_virtio_blk/struct.VirtioBlk.html).
pub struct VirtioNet<'h, T: Transport> {
    transport: T,
    rx_queue: SplitQueue,
    tx_queue: SplitQueue,
    host: &'h dyn VirtioHost,
    mac: MacAddress,
    /// Largest receive frame the device may deliver (link MTU + Ethernet
    /// header); sizes the receive staging and clamps a harvested frame.
    max_frame_len: usize,
    /// Largest transmit frame the driver stages: the link frame, or a
    /// segmentation-offload super-frame (up to the ring's transmit slot
    /// capacity) when [`Self::host_tso`] is set.
    max_tx_frame_len: usize,
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
    /// Offload metadata of the harvested frame in `rx_staged_len`,
    /// derived from the device's `virtio_net_hdr` flags: what the
    /// device reported it did to the frame's checksum.
    rx_staged_offload: FrameOffload,
    /// Whether `VIRTIO_NET_F_GUEST_CSUM` was negotiated at open: only
    /// then does the driver honour the device's receive checksum flags.
    guest_csum: bool,
    /// Whether `VIRTIO_NET_F_CSUM` was negotiated at open: only then does
    /// the driver ask the device to complete a transmit frame's partial
    /// checksum (and only then does it advertise the transmit-checksum
    /// offload the stack opts into).
    host_csum: bool,
    /// Whether `VIRTIO_NET_F_HOST_TSO4` **and** `VIRTIO_NET_F_HOST_TSO6`
    /// were negotiated at open (both, so segmentation works for either IP
    /// family under the single advertised offload): only then does the
    /// driver build a GSO `virtio_net_hdr` and advertise segmentation.
    host_tso: bool,
}

impl<'h, T: Transport> VirtioNet<'h, T> {
    /// Bring the device online.
    ///
    /// Implements the virtio-1.1 §3.1 initialisation sequence:
    /// reset, ACKNOWLEDGE, DRIVER, feature negotiation (accepting
    /// `VIRTIO_NET_F_GUEST_CSUM` when the device offers it),
    /// `FEATURES_OK`, set up the receive and transmit queues,
    /// `DRIVER_OK`, then read the MAC from the device-configuration
    /// window.
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
        // Negotiate only the features this driver implements. The sole
        // extended feature is receive-checksum offload
        // (`VIRTIO_NET_F_GUEST_CSUM`): with it the device may mark a
        // received frame checksum-validated, or deliver it with a
        // partial checksum the stack completes. A feature the device
        // offers but this vocabulary does not name is left un-negotiated
        // (no speculative surface).
        let device_features = transport.device_features();
        let guest_csum = device_features & wire::VIRTIO_NET_F_GUEST_CSUM != 0;
        let host_csum = device_features & wire::VIRTIO_NET_F_CSUM != 0;
        // TCP segmentation offload requires the device to complete the
        // transport checksum (`VIRTIO_NET_F_CSUM`) and to segment both IP
        // families, since the stack advertises one family-neutral
        // `TX_SEGMENT_TCP`; negotiate it only when all three hold.
        let host_tso = host_csum
            && device_features & wire::VIRTIO_NET_F_HOST_TSO4 != 0
            && device_features & wire::VIRTIO_NET_F_HOST_TSO6 != 0;
        let mut driver_features = 0u64;
        if guest_csum {
            driver_features |= wire::VIRTIO_NET_F_GUEST_CSUM;
        }
        if host_csum {
            driver_features |= wire::VIRTIO_NET_F_CSUM;
        }
        if host_tso {
            driver_features |= wire::VIRTIO_NET_F_HOST_TSO4 | wire::VIRTIO_NET_F_HOST_TSO6;
        }
        transport.set_driver_features(driver_features);
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
        // The transmit staging must hold a segmentation-offload super-frame
        // (the transmit ring's slot capacity) when TSO is negotiated, else
        // a single link frame.
        let max_tx_frame_len = if host_tso {
            RingGeometry::MAX_SLOT_CAPACITY as usize
        } else {
            wire::MAX_FRAME_LEN
        };
        let tx_data = host
            .alloc_dma_zeroed(max_tx_frame_len)
            .map_err(|_| VirtioError::DeviceFault)?;
        let mut net = Self {
            transport,
            rx_queue,
            tx_queue,
            host,
            mac: MacAddress::new(mac),
            max_frame_len: wire::MAX_FRAME_LEN,
            max_tx_frame_len,
            rx_header: Some(rx_header),
            rx_data: Some(rx_data),
            tx_header: Some(tx_header),
            tx_data: Some(tx_data),
            rx_staged_len: None,
            rx_staged_offload: FrameOffload::None,
            guest_csum,
            host_csum,
            host_tso,
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

    /// Build the `virtio_net_hdr` for a transmit frame carrying `offload`.
    ///
    /// With no offload every field is zero (the device transmits the
    /// frame's complete software checksum verbatim). A
    /// [`FrameOffload::TxChecksum`] frame — emitted only when
    /// `VIRTIO_NET_F_CSUM` was negotiated — sets `VIRTIO_NET_HDR_F_NEEDS_CSUM`
    /// and the device's `csum_start`/`csum_offset`, so the device folds the
    /// transport bytes and completes the field the stack left partial. No
    /// segmentation (`gso_type` stays `GSO_NONE`).
    ///
    /// A [`FrameOffload::TxSegment`] frame — emitted only when TSO was
    /// negotiated — additionally sets `gso_type`
    /// (`GSO_TCPV4`/`GSO_TCPV6`), `hdr_len`, and `gso_size`, so the device
    /// replicates the header for each `gso_size`-byte slice and completes
    /// each segment's checksum from the partial the stack left.
    fn build_header(&self, offload: FrameOffload) -> [u8; wire::HEADER_LEN] {
        let mut header = [0u8; wire::HEADER_LEN];
        let write_csum =
            |header: &mut [u8; wire::HEADER_LEN], csum_start: u16, csum_offset: u16| {
                header[wire::HDR_FLAGS_OFFSET] = wire::VIRTIO_NET_HDR_F_NEEDS_CSUM;
                header[wire::HDR_CSUM_START_OFFSET..wire::HDR_CSUM_START_OFFSET + 2]
                    .copy_from_slice(&csum_start.to_le_bytes());
                header[wire::HDR_CSUM_OFFSET_OFFSET..wire::HDR_CSUM_OFFSET_OFFSET + 2]
                    .copy_from_slice(&csum_offset.to_le_bytes());
            };
        match offload {
            FrameOffload::TxChecksum {
                csum_start,
                csum_offset,
            } if self.host_csum => write_csum(&mut header, csum_start, csum_offset),
            FrameOffload::TxSegment {
                csum_start,
                csum_offset,
                gso_size,
                hdr_len,
                ipv6,
            } if self.host_tso => {
                write_csum(&mut header, csum_start, csum_offset);
                header[wire::HDR_GSO_TYPE_OFFSET] = if ipv6 {
                    wire::VIRTIO_NET_HDR_GSO_TCPV6
                } else {
                    wire::VIRTIO_NET_HDR_GSO_TCPV4
                };
                header[wire::HDR_HDR_LEN_OFFSET..wire::HDR_HDR_LEN_OFFSET + 2]
                    .copy_from_slice(&hdr_len.to_le_bytes());
                header[wire::HDR_GSO_SIZE_OFFSET..wire::HDR_GSO_SIZE_OFFSET + 2]
                    .copy_from_slice(&gso_size.to_le_bytes());
            }
            _ => {}
        }
        header
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
        // A transmit frame may be a segmentation-offload super-frame, so it
        // is bounded by the (larger) transmit staging, not the link MTU.
        let cap = self.max_tx_frame_len.min(staging.len());
        let mut offload = FrameOffload::None;
        let len = match rings.tx.pop_with(&mut offload, &mut staging[..cap]) {
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
            .stage(&self.build_header(offload))
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
                match rings
                    .rx
                    .push_with(self.rx_staged_offload, &data_slab.as_bytes()[..len])
                {
                    Ok(()) => {
                        self.rx_staged_len = None;
                        self.rx_staged_offload = FrameOffload::None;
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
                        self.rx_staged_offload = self.rx_offload_from_header();
                        self.rx_staged_len = Some(frame_len.min(self.max_frame_len));
                    }
                    Err(VirtioError::NoCompletion) => return Ok(()),
                    Err(e) => return Err(e.as_driver_error()),
                }
            }
        }
    }

    /// Derive the offload tag for a just-completed receive frame from
    /// the device's `virtio_net_hdr` flags.
    ///
    /// Honoured only when `VIRTIO_NET_F_GUEST_CSUM` was negotiated: a
    /// `NEEDS_CSUM` frame carries the device's `csum_start`/`csum_offset`
    /// through to the stack (which completes the fold), a `DATA_VALID`
    /// frame is reported checksum-validated, and anything else is the
    /// plain software path. The offsets are validated against the frame
    /// length by the consumer, not trusted here.
    fn rx_offload_from_header(&self) -> FrameOffload {
        if !self.guest_csum {
            return FrameOffload::None;
        }
        let Some(header) = self.rx_header.as_ref() else {
            return FrameOffload::None;
        };
        let hdr = header.as_bytes();
        if hdr.len() < wire::HEADER_LEN {
            return FrameOffload::None;
        }
        let flags = hdr[wire::HDR_FLAGS_OFFSET];
        if flags & wire::VIRTIO_NET_HDR_F_NEEDS_CSUM != 0 {
            let csum_start = u16::from_le_bytes([
                hdr[wire::HDR_CSUM_START_OFFSET],
                hdr[wire::HDR_CSUM_START_OFFSET + 1],
            ]);
            let csum_offset = u16::from_le_bytes([
                hdr[wire::HDR_CSUM_OFFSET_OFFSET],
                hdr[wire::HDR_CSUM_OFFSET_OFFSET + 1],
            ]);
            FrameOffload::NeedsChecksum {
                csum_start,
                csum_offset,
            }
        } else if flags & wire::VIRTIO_NET_HDR_F_DATA_VALID != 0 {
            FrameOffload::Validated
        } else {
            FrameOffload::None
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
        // No VIRTIO_NET_F_STATUS is negotiated (an operational device
        // reports its link up) and one receive queue is used. Offloads are
        // advertised, each only when its virtio feature was negotiated:
        // receive-checksum validation (`VIRTIO_NET_F_GUEST_CSUM`), TCP
        // transmit-checksum offload (`VIRTIO_NET_F_CSUM`), and TCP
        // segmentation offload (`VIRTIO_NET_F_HOST_TSO4`+`TSO6`). UDP
        // transmit offload is not advertised: the stack keeps UDP on the
        // software path (its zero-checksum-as-0xFFFF rule is not expressed
        // by the virtio partial-checksum contract).
        let mut offloads = NetOffloads::empty();
        if self.guest_csum {
            offloads =
                NetOffloads::from_bits(offloads.bits() | NetOffloads::RX_CSUM_VALIDATED.bits())
                    .unwrap_or(NetOffloads::empty());
        }
        if self.host_csum {
            offloads = NetOffloads::from_bits(offloads.bits() | NetOffloads::TX_CSUM_TCP.bits())
                .unwrap_or(offloads);
        }
        if self.host_tso {
            offloads = NetOffloads::from_bits(offloads.bits() | NetOffloads::TX_SEGMENT_TCP.bits())
                .unwrap_or(offloads);
        }
        Ok(DeviceFacts {
            mac: self.mac,
            mtu: u32::try_from(self.max_frame_len - wire::MIN_ETHERNET_FRAME)
                .map_err(|_| DriverError::LengthOutOfRange)?,
            link: LinkState::Up,
            offloads,
            rx_queues: 1,
        })
    }

    fn service(&mut self, rings: &mut FrameRings<'_>) -> Result<ServiceReport, DriverError> {
        let mut report = ServiceReport::default();
        self.drain_tx(rings, &mut report)?;
        self.harvest_rx(rings, &mut report)?;
        // The doorbell never parks: it drains what is ready and returns.
        // A `Service` doorbell may cross a process boundary (the driver
        // process serving the stack's `NetChannelRequest::Service`), where
        // parking here would block the reply and defeat the wait-set the
        // stack and driver each run. Waiting for the *next* receive event is
        // the caller's job — the driver parks on the device interrupt and
        // rings the stack's notify port, and an empty report simply means
        // "nothing to do yet", not "spin".
        Ok(report)
    }

    fn ack_interrupt(&mut self) {
        // Clear the device's interrupt-status register so the line deasserts;
        // the completed receive descriptors persist in the used ring until the
        // next `service` drains them. The virtio-MMIO transport writes
        // `InterruptACK` for exactly the bits it read as pending.
        self.transport.ack_interrupt();
    }
}

#[cfg(test)]
mod tests;
