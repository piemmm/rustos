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
//! `0` and transmit queue `1` (virtio-net §5.1.2). When mergeable
//! receive buffers (`VIRTIO_NET_F_MRG_RXBUF`) are negotiated the header
//! is the 12-byte `virtio_net_hdr_mrg_rxbuf` on both rings; otherwise it
//! is the 10-byte `virtio_net_hdr`.
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
//! so the device completes it (`plans/NETWORK.md` §2.3). Segmentation
//! (`VIRTIO_NET_F_HOST_TSO4`+`TSO6`): the stack may hand the device one
//! over-size TCP super-segment ([`FrameOffload::TxSegment`]) to split on
//! the wire. The driver does no checksum or segmentation arithmetic
//! itself — it never links the stack — so the kernel that links this
//! crate for the device id stays free of `lib/net`.
//!
//! Mergeable receive buffers (`VIRTIO_NET_F_MRG_RXBUF`) are negotiated
//! when offered: the driver posts a *pool* of receive buffers (so a
//! burst is captured before the stack next services the ring, rather
//! than the device dropping everything past a single outstanding buffer)
//! and reassembles a frame the device delivered across several buffers,
//! reading the buffer count from the first buffer's `num_buffers`. A
//! ≤MTU frame arrives in one buffer; reassembly is bounded to one link
//! frame and fails closed on an over-long or corrupt merge.
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
//! transport (`plans/NETWORK.md` §2.3): every call reaps completed
//! transmissions, drains the TX ring into the device, and harvests
//! delivered frames into the RX ring. The doorbell **never waits**:
//! it moves what is ready and returns, so it is safe to serve across
//! a process boundary (the driver process answering the stack's
//! `Service` request), where parking would block the reply and the
//! serve loop. Waiting for the next device event is the caller's job
//! — the driver parks on the device interrupt and rings the stack's
//! notify port. Each `service` reaps every completed transmission and
//! then drains **every** queued frame the device can still hold into the
//! transmit ring — up to the ring's descriptor capacity, kept in flight
//! at once — so a burst (a TCP data segment and the ACK queued right
//! behind it) egresses together in one call rather than one frame per
//! call. Back-pressure applies only when the transmit ring is genuinely
//! full: the remaining queued frames are left in the frame ring for the
//! next completion-driven call — never waited on, never dropped. A frame
//! in flight has its completion reaped non-blockingly on a later call
//! (the shared device interrupt the completion raises drives it). A
//! reassembled receive frame that finds the RX ring full is likewise
//! held (its buffers stay consumed, not re-posted) and delivered by the
//! next call, before any further completion is harvested.
//!
//! # Staging buffers
//!
//! DMA staging is allocated **once**, at [`VirtioNet::open`]: a fixed
//! pool of single-descriptor receive buffers plus one reassembly buffer
//! on the receive side, and a fixed pool of header + frame-buffer pairs
//! (one per concurrently in-flight transmission, sized to the transmit
//! ring) on the send side — all reused for every packet. Every receive
//! buffer is posted as a device-write descriptor at open and re-posted
//! after its frame is delivered, so the device always owns a pool of
//! receive buffers and an idle `service` poll touches no allocator — no
//! per-packet `dma_alloc`/`dma_free` round trip, no per-poll audit-log
//! traffic on the hot path. The single-buffer common path delivers
//! straight from its source buffer, so it copies once (into the RX
//! ring); only a merged multi-buffer frame is assembled through the
//! reassembly buffer.
//!
//! # Zero-on-free
//!
//! [`Net::service`] honours a
//! [`BufferClass::Sensitive`](tairix_abi::driver::BufferClass) ring
//! class by scrubbing the persistent transmit staging through
//! [`tairix_virtio::BounceBuffer::into_slab`] before the buffers are
//! reused for the next packet, and by zeroing each receive buffer (and
//! the reassembly buffer) after its frame is delivered, before it is
//! re-posted to the device — never before delivery.

#![no_std]
#![forbid(unsafe_op_in_unsafe_fn)]
#![deny(missing_docs)]

mod channel;
pub use channel::NetChannelServer;

use core::convert::TryFrom;
use tairix_abi::driver::net::{
    DeviceFacts, LinkState, MacAddress, Net, NetOffloads, MAC_ADDRESS_LEN,
};
use tairix_abi::driver::net_ring::{
    FrameOffload, FrameRing, FrameRings, RingGeometry, ServiceReport, MAX_RX_QUEUES,
};
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

/// Virtio-net wire protocol constants (virtio 1.1 §5.1).
mod wire {
    /// `struct virtio_net_hdr` (legacy, no mergeable buffers): a
    /// 10-byte header prefixing every transmit and receive descriptor
    /// chain. Field layout (virtio 1.1 §5.1.6): `flags` (1), `gso_type`
    /// (1), `hdr_len` (2), `gso_size` (2), `csum_start` (2),
    /// `csum_offset` (2), all little-endian.
    pub const HEADER_LEN: usize = 10;
    /// `struct virtio_net_hdr_mrg_rxbuf` (virtio 1.1 §5.1.6): the
    /// 10-byte `virtio_net_hdr` extended by a little-endian `u16`
    /// `num_buffers` field, used when `VIRTIO_NET_F_MRG_RXBUF` is
    /// negotiated. Because negotiating the feature changes the header
    /// the device expects on **both** rings (transitional devices size
    /// the header uniformly once mergeable is on), the driver prefixes
    /// every transmit and receive chain with a header of this length
    /// too; `num_buffers` is written by the device on receive and left
    /// zero (ignored) on transmit.
    pub const MRG_HEADER_LEN: usize = HEADER_LEN + 2;
    /// `num_buffers` field offset within `virtio_net_hdr_mrg_rxbuf`:
    /// the count of receive buffers the device merged into this frame
    /// (virtio 1.1 §5.1.6.4). Present only when `VIRTIO_NET_F_MRG_RXBUF`
    /// was negotiated; on the first buffer of a received frame it holds
    /// the whole frame's buffer count, and every subsequent buffer of
    /// the same frame carries pure frame data (no header).
    pub const HDR_NUM_BUFFERS_OFFSET: usize = 10;
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
    /// `VIRTIO_NET_F_MRG_RXBUF` (virtio 1.1 §5.1.4, feature bit 15): the
    /// device may deliver one received frame across several receive
    /// buffers, reporting the buffer count in the first buffer's
    /// `virtio_net_hdr_mrg_rxbuf::num_buffers`. Negotiating it lets the
    /// driver post a *pool* of receive buffers (so a burst of frames is
    /// captured before the stack next services the ring, instead of the
    /// device dropping everything past a single outstanding buffer) and
    /// obliges it to reassemble a multi-buffer frame.
    pub const VIRTIO_NET_F_MRG_RXBUF: u64 = 1 << 15;
    /// `VIRTIO_NET_F_STATUS` (virtio 1.1 §5.1.4, feature bit 16): the
    /// device exposes a live link-status word in its configuration space
    /// (`status`, at [`CONFIG_STATUS_OFFSET`]) and raises a
    /// configuration-change interrupt when the word changes. Negotiating
    /// it lets the driver sense link up/down (a NIC unplugged, a carrier
    /// lost) rather than assuming a permanently-up link — the sole live
    /// source of a bond failover (`plans/NETWORK.md` §6.3).
    pub const VIRTIO_NET_F_STATUS: u64 = 1 << 16;
    /// `VIRTIO_NET_S_LINK_UP` (virtio 1.1 §5.1.4): the bit set in the
    /// device-config `status` word while the link carries frames. Valid
    /// only when [`VIRTIO_NET_F_STATUS`] was negotiated.
    pub const VIRTIO_NET_S_LINK_UP: u16 = 1 << 0;
    /// `status` (`le16`) byte offset in the device-configuration window:
    /// it follows the 6-byte MAC (virtio 1.1 §5.1.4). Read only when
    /// [`VIRTIO_NET_F_STATUS`] was negotiated.
    pub const CONFIG_STATUS_OFFSET: usize = 6;
    /// Minimum Ethernet frame size (excluding FCS).
    pub const MIN_ETHERNET_FRAME: usize = 14;
    /// Link MTU: the largest link-layer payload (IP packet) carried.
    pub const LINK_MTU: usize = 1500;
    /// Largest frame moved: the link MTU plus the Ethernet header.
    pub const MAX_FRAME_LEN: usize = LINK_MTU + MIN_ETHERNET_FRAME;
    /// Receive queue index (virtio-net §5.1.2). For a multiqueue device
    /// (`VIRTIO_NET_F_MQ`) the receive queue of pair `i` is
    /// `RX_QUEUE + i * QUEUE_PAIR_STRIDE`.
    pub const RX_QUEUE: u16 = 0;
    /// Transmit queue index (virtio-net §5.1.2). For a multiqueue device
    /// the transmit queue of pair `i` is `TX_QUEUE + i * QUEUE_PAIR_STRIDE`.
    pub const TX_QUEUE: u16 = 1;
    /// Virtqueue-index stride between successive queue pairs: each pair
    /// occupies a receive then a transmit queue (virtio 1.1 §5.1.2).
    pub const QUEUE_PAIR_STRIDE: u16 = 2;
    /// Receive queue size (descriptors). Power-of-two per virtio §2.6.
    pub const RX_QUEUE_SIZE: u16 = 8;
    /// Transmit queue size (descriptors). Power-of-two per virtio §2.6.
    pub const TX_QUEUE_SIZE: u16 = 8;
    /// Control queue size (descriptors). Power-of-two per virtio §2.6; a
    /// small ring, since control commands are one-at-a-time handshakes.
    pub const CTRL_QUEUE_SIZE: u16 = 4;
    /// MAC address byte offset in the device-configuration window.
    pub const CONFIG_MAC_OFFSET: usize = 0;
    /// `max_virtqueue_pairs` (`le16`) byte offset in the device-config
    /// window: it follows the 6-byte MAC and the 2-byte `status` word
    /// (virtio 1.1 §5.1.4). Valid only when [`VIRTIO_NET_F_MQ`] was
    /// negotiated.
    pub const CONFIG_MAX_VQ_PAIRS_OFFSET: usize = 8;
    /// `VIRTIO_NET_F_CTRL_VQ` (virtio 1.1 §5.1.4, feature bit 17): the
    /// device exposes a control virtqueue. Required to issue any control
    /// command, including [`VIRTIO_NET_CTRL_MQ`] queue-pair selection.
    pub const VIRTIO_NET_F_CTRL_VQ: u64 = 1 << 17;
    /// `VIRTIO_NET_F_MQ` (virtio 1.1 §5.1.4, feature bit 22): the device
    /// supports multiple receive/transmit queue pairs and steers received
    /// frames across the enabled receive queues. Requires
    /// [`VIRTIO_NET_F_CTRL_VQ`] to select the pair count at runtime.
    pub const VIRTIO_NET_F_MQ: u64 = 1 << 22;
    /// `VIRTIO_NET_CTRL_MQ` (virtio 1.1 §5.1.6.5): the control-command
    /// class for multiqueue configuration.
    pub const VIRTIO_NET_CTRL_MQ: u8 = 4;
    /// `VIRTIO_NET_CTRL_MQ_VQ_PAIRS_SET` (virtio 1.1 §5.1.6.5): the
    /// command selecting how many receive/transmit queue pairs the device
    /// uses (`struct virtio_net_ctrl_mq { le16 virtqueue_pairs; }`).
    pub const VIRTIO_NET_CTRL_MQ_VQ_PAIRS_SET: u8 = 0;
    /// `VIRTIO_NET_OK` (virtio 1.1 §5.1.6): the ack byte a successful
    /// control command writes back.
    pub const VIRTIO_NET_OK: u8 = 0;
}

/// Transmit frames the driver keeps in flight at once.
///
/// Each frame occupies two transmit descriptors (the `virtio_net_hdr` and
/// the frame body), so the transmit virtqueue's descriptor table
/// ([`wire::TX_QUEUE_SIZE`]) bounds it. Draining every queued frame the
/// device can still hold in a single `service` — rather than one frame
/// per call — is what lets a burst (a TCP data segment and the ACK queued
/// right behind it) egress together instead of stranding the trailing
/// frame in the shared frame ring until an unrelated interrupt drives the
/// next call.
const TX_INFLIGHT_MAX: usize = (wire::TX_QUEUE_SIZE / 2) as usize;

/// One transmit frame handed to the device, awaiting its completion.
///
/// Holds the staging in class-aware [`BounceBuffer`]s (scrubbed on drop
/// when the ring class is sensitive) and the descriptor `head` the device
/// returns at completion, so a used-ring completion maps back to exactly
/// the staging pair that produced it — the driver may hold several frames
/// in flight without confusing their buffers.
struct TxInflight {
    head: u16,
    header: BounceBuffer,
    data: BounceBuffer,
}

/// The transmit staging pool: a fixed set of `virtio_net_hdr` + frame
/// slab pairs (carved once at [`VirtioNet::open`], reused for every
/// frame) split between those idle and those the device currently owns.
///
/// Allocation-free and sized to the transmit ring, so the driver can keep
/// the ring full and never waits on the device to free a slot.
struct TxStaging {
    /// Idle staging pairs available for a new frame.
    free: [Option<(DmaSlab, DmaSlab)>; TX_INFLIGHT_MAX],
    /// Frames the device owns, awaiting completion, keyed by descriptor
    /// head.
    inflight: [Option<TxInflight>; TX_INFLIGHT_MAX],
}

impl TxStaging {
    /// Build the pool from the given idle staging pairs (nothing in
    /// flight yet).
    fn new(free: [Option<(DmaSlab, DmaSlab)>; TX_INFLIGHT_MAX]) -> Self {
        Self {
            free,
            inflight: core::array::from_fn(|_| None),
        }
    }

    /// Claim an idle staging pair for a new frame, or [`None`] when every
    /// pair is in flight (the ring is full — legitimate back-pressure).
    fn take_free(&mut self) -> Option<(DmaSlab, DmaSlab)> {
        self.free.iter_mut().find_map(Option::take)
    }

    /// Return a staging pair to the idle set (a frame the device could not
    /// move, or an empty pop). A free slot always exists because a pair
    /// only ever leaves the pool while it is in flight and every caller
    /// returns exactly what it took; the slabs free themselves on drop if
    /// that invariant is somehow broken (fail closed, never a leak of
    /// device-visible memory).
    fn return_free(&mut self, header: DmaSlab, data: DmaSlab) {
        if let Some(slot) = self.free.iter_mut().find(|s| s.is_none()) {
            *slot = Some((header, data));
        }
    }

    /// Record a frame just handed to the device, keyed by its descriptor
    /// `head`, so its completion reclaims exactly this staging pair.
    fn record_inflight(&mut self, head: u16, header: BounceBuffer, data: BounceBuffer) {
        if let Some(slot) = self.inflight.iter_mut().find(|s| s.is_none()) {
            *slot = Some(TxInflight { head, header, data });
        }
    }

    /// Reclaim the staging pair whose frame completed with `head`,
    /// returning it to the idle set. A `head` that matches no in-flight
    /// frame (a duplicate or device-fabricated completion) reclaims
    /// nothing — fail closed.
    fn reclaim(&mut self, head: u16) {
        let frame = self.inflight.iter_mut().find_map(|slot| {
            if slot.as_ref().is_some_and(|f| f.head == head) {
                slot.take()
            } else {
                None
            }
        });
        if let Some(f) = frame {
            self.return_free(f.header.into_slab(), f.data.into_slab());
        }
    }
}

/// Receive buffers the driver keeps posted to the device at once.
///
/// The driver posts a *pool* of receive buffers rather than a single
/// outstanding one, so a burst of frames the device delivers back to
/// back is captured (up to this depth) before the stack next services
/// the ring, instead of the device dropping everything past a single
/// outstanding buffer. Bounded by the receive virtqueue's descriptor
/// table ([`wire::RX_QUEUE_SIZE`]): each buffer is one device-write
/// descriptor, so the pool depth is the queue size.
const RX_POOL: usize = wire::RX_QUEUE_SIZE as usize;

/// One receive buffer in the [`VirtioNet`] pool.
///
/// A buffer is either *posted* — the device owns it and `posted_head`
/// names the descriptor it is queued under — or *held* by the driver
/// (`posted_head` is `None`: the device completed it and the driver has
/// not yet delivered its frame and re-posted it). Its slab is `hdr_len +
/// max_frame_len` bytes: a received frame's first buffer carries the
/// `virtio_net_hdr` inline followed by frame bytes, and a merged frame's
/// subsequent buffers carry pure frame bytes (no header).
struct RxBuffer {
    slab: DmaSlab,
    posted_head: Option<u16>,
}

/// A fully reassembled receive frame awaiting a free RX-ring slot.
///
/// Held only while the shared RX ring is full (legitimate
/// back-pressure): the next `service` retries this push *before*
/// harvesting further completions, so a frame the device already handed
/// over is never dropped and RX-ring order is preserved.
struct PendingRx {
    len: usize,
    offload: FrameOffload,
    /// `Some(index)` — deliver directly from `buffers[index]`'s inline
    /// frame bytes (the single-buffer common path: one copy into the
    /// ring). `None` — deliver from the reassembly buffer (a frame the
    /// device merged across several buffers).
    single_index: Option<usize>,
    /// Pool indices consumed to build this frame, re-posted to the device
    /// once it is delivered.
    consumed: [Option<usize>; RX_POOL],
}

/// One device receive queue: its virtqueue, its own pool of posted
/// receive buffers, a reassembly buffer for a merged frame, and any
/// reassembled frame held while its shared RX ring is full.
///
/// A single-queue device has exactly one of these (`plans/NETWORK.md`
/// N7c-1). A multiqueue device (`VIRTIO_NET_F_MQ`, N7c-2) has one per
/// enabled receive queue: the device steers each received frame into one
/// of them, and the driver harvests each into that queue's own shared RX
/// ring, so a busy link's receive work is spread rather than serialised
/// behind a single queue. Every queue owns an independent buffer pool and
/// reassembly buffer, so the queues never share device-visible memory.
struct RxQueue {
    /// The receive virtqueue this queue's buffers are posted to.
    queue: SplitQueue,
    /// This queue's receive-buffer pool (carved once at open, reused for
    /// the driver's whole life). Each buffer is either posted to the
    /// device or held by the driver between completion and re-post.
    buffers: [Option<RxBuffer>; RX_POOL],
    /// Reassembly buffer for a frame the device merged across several of
    /// this queue's receive buffers (`VIRTIO_NET_F_MRG_RXBUF`); touched
    /// only when `num_buffers` > 1.
    reasm: Option<DmaSlab>,
    /// A reassembled frame awaiting a free slot in this queue's shared RX
    /// ring (back-pressure): the next service retries it before harvesting
    /// further completions, so nothing the device handed over is dropped.
    pending: Option<PendingRx>,
}

impl RxQueue {
    /// Bring one receive virtqueue online and carve its buffer pool +
    /// reassembly buffer.
    fn new<T: Transport>(
        transport: &mut T,
        host: &dyn VirtioHost,
        queue_index: u16,
        rx_buf_len: usize,
    ) -> Result<Self, VirtioError> {
        let queue = SplitQueue::new(transport, host, queue_index, wire::RX_QUEUE_SIZE)?;
        let mut buffers: [Option<RxBuffer>; RX_POOL] = core::array::from_fn(|_| None);
        for slot in &mut buffers {
            let slab = host
                .alloc_dma_zeroed(rx_buf_len)
                .map_err(|_| VirtioError::DeviceFault)?;
            *slot = Some(RxBuffer {
                slab,
                posted_head: None,
            });
        }
        let reasm = host
            .alloc_dma_zeroed(wire::MAX_FRAME_LEN)
            .map_err(|_| VirtioError::DeviceFault)?;
        Ok(Self {
            queue,
            buffers,
            reasm: Some(reasm),
            pending: None,
        })
    }

    /// Post every held receive buffer to the device as one device-write
    /// descriptor, then notify the device once.
    fn post_all<T: Transport>(&mut self, transport: &mut T) -> Result<(), DriverError> {
        let mut posted = false;
        for index in 0..RX_POOL {
            if self
                .buffers
                .get(index)
                .and_then(|b| b.as_ref())
                .is_some_and(|b| b.posted_head.is_none())
            {
                self.post_buffer(index)?;
                posted = true;
            }
        }
        if posted {
            self.queue.kick(transport);
        }
        Ok(())
    }

    /// Post the held receive buffer at `index` as one device-write
    /// descriptor, recording the descriptor head it is queued under.
    /// Does not notify the device (the caller batches the kick).
    fn post_buffer(&mut self, index: usize) -> Result<(), DriverError> {
        let Some(Some(buffer)) = self.buffers.get(index) else {
            return Err(DriverError::DeviceFault);
        };
        let len = u32::try_from(buffer.slab.len()).map_err(|_| DriverError::LengthOutOfRange)?;
        let segments = [ChainSegment {
            phys: buffer.slab.phys(),
            len,
            direction: Direction::DeviceWrite,
        }];
        let head = self
            .queue
            .add_chain(&segments)
            .map_err(VirtioError::as_driver_error)?;
        // Record the head only after the successful post: an add_chain
        // failure leaves the buffer held (posted_head stays None) so it is
        // retried, never leaked to the device untracked.
        if let Some(Some(buffer)) = self.buffers.get_mut(index) {
            buffer.posted_head = Some(head);
        }
        Ok(())
    }

    /// Move delivered frames from this queue into its RX `ring` until the
    /// device is drained or the ring is full.
    ///
    /// Each iteration first delivers any frame held from a previous
    /// ring-full call (back-pressure: a frame the device already handed
    /// over is never dropped, and RX-ring order is preserved), then
    /// reassembles the next completed frame from the buffer pool.
    // The receive-offload parameters (header length, frame cap, negotiated
    // feature flags, sensitivity) are the device-wide facts threaded in
    // from the one `VirtioNet` that owns them, rather than duplicated onto
    // each queue; passing them keeps a queue's state to its own rings.
    #[allow(clippy::too_many_arguments)]
    fn harvest<T: Transport>(
        &mut self,
        ring: &mut FrameRing<'_>,
        transport: &mut T,
        hdr_len: usize,
        max_frame_len: usize,
        guest_csum: bool,
        mergeable: bool,
        sensitive: bool,
        report: &mut ServiceReport,
    ) -> Result<(), DriverError> {
        loop {
            if self.pending.is_some() {
                if !self.deliver_pending(ring, transport, hdr_len, sensitive, report)? {
                    // Ring still full: keep the frame held, stop.
                    return Ok(());
                }
                continue;
            }
            // A produced frame is pending; loop to deliver it. Otherwise
            // no completion remains: re-post the pool and stop.
            if !self.collect_frame(hdr_len, max_frame_len, guest_csum, mergeable)? {
                self.post_all(transport)?;
                return Ok(());
            }
        }
    }

    /// Deliver the held reassembled frame into `ring`, returning whether
    /// it was delivered (`false` = the ring is full, keep it).
    fn deliver_pending<T: Transport>(
        &mut self,
        ring: &mut FrameRing<'_>,
        transport: &mut T,
        hdr_len: usize,
        sensitive: bool,
        report: &mut ServiceReport,
    ) -> Result<bool, DriverError> {
        let Some(pending) = self.pending.take() else {
            return Ok(true);
        };
        let len = pending.len;
        let push = if let Some(index) = pending.single_index {
            let buffer = self
                .buffers
                .get(index)
                .and_then(|b| b.as_ref())
                .ok_or(DriverError::DeviceFault)?;
            ring.push_with(
                pending.offload,
                &buffer.slab.as_bytes()[hdr_len..hdr_len + len],
            )
        } else {
            let reasm = self.reasm.as_ref().ok_or(DriverError::DeviceFault)?;
            ring.push_with(pending.offload, &reasm.as_bytes()[..len])
        };
        match push {
            Ok(()) => {
                report.received += 1;
                if sensitive && pending.single_index.is_none() {
                    self.scrub_reasm();
                }
                for index in pending.consumed.iter().flatten() {
                    if sensitive {
                        self.scrub_buffer(*index);
                    }
                    self.post_buffer(*index)?;
                }
                self.queue.kick(transport);
                Ok(true)
            }
            Err(Errno::NoSpace) => {
                report.rx_ring_full = true;
                self.pending = Some(pending);
                Ok(false)
            }
            Err(_) => Err(DriverError::BadMagic),
        }
    }

    /// Reassemble the next completed frame from the buffer pool into
    /// `pending`, returning whether one was produced.
    fn collect_frame(
        &mut self,
        hdr_len: usize,
        max_frame_len: usize,
        guest_csum: bool,
        mergeable: bool,
    ) -> Result<bool, DriverError> {
        loop {
            let token = match self.queue.poll_used() {
                Ok(token) => token,
                Err(VirtioError::NoCompletion) => return Ok(false),
                // A device-fabricated completion: its used slot is
                // consumed; harvest the genuine completions behind it.
                Err(VirtioError::MalformedCompletion) => continue,
                Err(e) => return Err(e.as_driver_error()),
            };
            let Some(index) = self.consume_completed(token.head) else {
                // The completion names no buffer the driver posted: skip.
                continue;
            };
            let written = (token.written as usize).min(self.buffer_len(index));
            if written < hdr_len {
                // A runt completion carries no frame: re-post and go on.
                continue;
            }
            let num_buffers = self.read_num_buffers(index, mergeable);
            if num_buffers == 0 || num_buffers > RX_POOL {
                // A corrupt buffer count: drop this buffer, fail closed.
                continue;
            }
            let offload = self.rx_offload_from_header(index, guest_csum);
            let first_data = written - hdr_len;
            if num_buffers == 1 {
                let len = first_data.min(max_frame_len);
                let mut consumed = [None; RX_POOL];
                consumed[0] = Some(index);
                self.pending = Some(PendingRx {
                    len,
                    offload,
                    single_index: Some(index),
                    consumed,
                });
                return Ok(true);
            }
            // A merged frame is pending; otherwise the merge failed
            // closed (short/over-long) and its buffers are left held for
            // re-posting, so harvesting continues.
            if self.collect_merged(
                index,
                hdr_len,
                first_data,
                num_buffers,
                offload,
                max_frame_len,
            )? {
                return Ok(true);
            }
        }
    }

    /// Reassemble a `num_buffers`-buffer merged frame beginning at
    /// `first_index` into the reassembly buffer and stage it as
    /// `pending`, returning whether it was produced.
    fn collect_merged(
        &mut self,
        first_index: usize,
        hdr_len: usize,
        first_data: usize,
        num_buffers: usize,
        offload: FrameOffload,
        max_frame_len: usize,
    ) -> Result<bool, DriverError> {
        let cap = max_frame_len;
        let mut consumed = [None; RX_POOL];
        consumed[0] = Some(first_index);
        // The first buffer's data follows its inline header.
        let Some(mut len) = self.copy_into_reasm(first_index, hdr_len, first_data, 0, cap)? else {
            return Ok(false);
        };
        for slot in consumed.iter_mut().take(num_buffers).skip(1) {
            // The device promised more buffers than it delivered: a
            // protocol violation. Drop the partial frame fail-closed; its
            // buffers are left held for re-posting.
            let Ok(token) = self.queue.poll_used() else {
                return Ok(false);
            };
            let Some(index) = self.consume_completed(token.head) else {
                return Ok(false);
            };
            *slot = Some(index);
            let written = (token.written as usize).min(self.buffer_len(index));
            // A trailing buffer carries pure frame bytes (no header).
            match self.copy_into_reasm(index, 0, written, len, cap)? {
                Some(new_len) => len = new_len,
                None => return Ok(false),
            }
        }
        self.pending = Some(PendingRx {
            len,
            offload,
            single_index: None,
            consumed,
        });
        Ok(true)
    }

    /// Copy `count` bytes starting at `src_offset` of the buffer at
    /// `index` into the reassembly buffer at `dst_offset`, returning the
    /// new total length or `None` when it would exceed `cap`.
    fn copy_into_reasm(
        &mut self,
        index: usize,
        src_offset: usize,
        count: usize,
        dst_offset: usize,
        cap: usize,
    ) -> Result<Option<usize>, DriverError> {
        let end = match dst_offset.checked_add(count) {
            Some(end) if end <= cap => end,
            _ => return Ok(None),
        };
        // Borrow the source buffer and the reassembly buffer as disjoint
        // fields: the copy reads one and writes the other in one step.
        let src_slab = self
            .buffers
            .get(index)
            .and_then(|b| b.as_ref())
            .ok_or(DriverError::DeviceFault)?;
        let src = src_slab.slab.as_bytes();
        if src_offset.checked_add(count).is_none_or(|e| e > src.len()) {
            return Err(DriverError::LengthOutOfRange);
        }
        let src_span = &src[src_offset..src_offset + count];
        let reasm = self.reasm.as_mut().ok_or(DriverError::DeviceFault)?;
        let dst = reasm.as_bytes_mut();
        if end > dst.len() {
            return Ok(None);
        }
        dst[dst_offset..end].copy_from_slice(src_span);
        Ok(Some(end))
    }

    /// The byte length of the receive buffer at `index` (0 when absent).
    fn buffer_len(&self, index: usize) -> usize {
        self.buffers
            .get(index)
            .and_then(|b| b.as_ref())
            .map_or(0, |b| b.slab.len())
    }

    /// Mark the posted buffer whose descriptor head is `head` as consumed
    /// and return its pool index, or `None` when no posted buffer matches.
    fn consume_completed(&mut self, head: u16) -> Option<usize> {
        let index = self
            .buffers
            .iter()
            .position(|b| b.as_ref().is_some_and(|b| b.posted_head == Some(head)))?;
        if let Some(Some(buffer)) = self.buffers.get_mut(index) {
            buffer.posted_head = None;
        }
        Some(index)
    }

    /// The `num_buffers` count in the buffer at `index`'s inline header,
    /// or 1 when mergeable buffers were not negotiated.
    fn read_num_buffers(&self, index: usize, mergeable: bool) -> usize {
        if !mergeable {
            return 1;
        }
        let Some(buffer) = self.buffers.get(index).and_then(|b| b.as_ref()) else {
            return 0;
        };
        let hdr = buffer.slab.as_bytes();
        if hdr.len() < wire::MRG_HEADER_LEN {
            return 0;
        }
        usize::from(u16::from_le_bytes([
            hdr[wire::HDR_NUM_BUFFERS_OFFSET],
            hdr[wire::HDR_NUM_BUFFERS_OFFSET + 1],
        ]))
    }

    /// Derive the offload tag for a just-completed receive frame from the
    /// device's `virtio_net_hdr` flags in the buffer at `index`.
    ///
    /// Honoured only when `VIRTIO_NET_F_GUEST_CSUM` was negotiated; the
    /// offsets are validated against the frame length by the consumer, not
    /// trusted here.
    fn rx_offload_from_header(&self, index: usize, guest_csum: bool) -> FrameOffload {
        if !guest_csum {
            return FrameOffload::None;
        }
        let Some(buffer) = self.buffers.get(index).and_then(|b| b.as_ref()) else {
            return FrameOffload::None;
        };
        let hdr = buffer.slab.as_bytes();
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

    /// Zero the receive buffer at `index` (a sensitive-class frame just
    /// left it, before it is re-posted to the device).
    fn scrub_buffer(&mut self, index: usize) {
        if let Some(Some(buffer)) = self.buffers.get_mut(index) {
            buffer.slab.as_bytes_mut().fill(0);
        }
    }

    /// Zero the reassembly buffer (a sensitive-class merged frame just
    /// left it).
    fn scrub_reasm(&mut self) {
        if let Some(reasm) = self.reasm.as_mut() {
            reasm.as_bytes_mut().fill(0);
        }
    }
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
    /// The device's receive queues, one per enabled receive queue pair
    /// (`plans/NETWORK.md` N7c-2). Index `0..rx_queue_count` are `Some`;
    /// a single-queue device uses only index 0. The device steers each
    /// received frame into one of these, and [`Self::service`] harvests
    /// every one into its matching shared RX ring.
    rx: [Option<RxQueue>; MAX_RX_QUEUES as usize],
    /// Number of enabled receive queues (`1..=`[`MAX_RX_QUEUES`]).
    rx_queue_count: usize,
    tx_queue: SplitQueue,
    /// The idle transmit queues of a multiqueue device's pairs `1..N`.
    /// The stack serialises its own egress, so the driver transmits only
    /// on `tx_queue` (pair 0); but virtio requires every queue of an
    /// enabled pair to be configured before the pair count is set, so
    /// these are set up and then held (never used) to keep their
    /// device-visible rings alive. Empty on a single-queue device.
    _idle_tx: [Option<SplitQueue>; MAX_RX_QUEUES as usize],
    /// The control virtqueue, present only when `VIRTIO_NET_F_MQ` +
    /// `VIRTIO_NET_F_CTRL_VQ` were negotiated. Used once at open to select
    /// the receive/transmit queue-pair count, then held alive (its ring
    /// stays device-visible). `None` on a single-queue device.
    _ctrl_queue: Option<SplitQueue>,
    /// The [`VirtioHost`] the DMA staging was allocated through. Held only
    /// to bind the driver's lifetime to the host for `'h`: the staging
    /// slabs free through the host's pool on drop, so the host must outlive
    /// this engine. Not read after `open` (the service path never waits on
    /// the device), hence the underscore.
    _host: &'h dyn VirtioHost,
    mac: MacAddress,
    /// Largest receive frame the device may deliver (link MTU + Ethernet
    /// header); sizes the receive staging and clamps a harvested frame.
    max_frame_len: usize,
    /// Largest transmit frame the driver stages: the link frame, or a
    /// segmentation-offload super-frame (up to the ring's transmit slot
    /// capacity) when [`Self::host_tso`] is set.
    max_tx_frame_len: usize,
    /// Length of the `virtio_net_hdr` prefixing every chain: 12 bytes
    /// (`virtio_net_hdr_mrg_rxbuf`) when `VIRTIO_NET_F_MRG_RXBUF` was
    /// negotiated, else 10 (`virtio_net_hdr`). Used for both rings, since
    /// a transitional device sizes the header uniformly once mergeable is
    /// on.
    rx_hdr_len: usize,
    /// The transmit staging pool: a fixed set of header + frame-buffer
    /// pairs (carved once at open, reused for every frame), split between
    /// those idle and those the device currently owns. Its depth is the
    /// number of frames the driver keeps in flight at once, so a burst
    /// drains into the ring in one `service` rather than one frame per
    /// call. The doorbell never waits on the device: an in-flight pair
    /// stays owned by the device — never reused or scrubbed — until its
    /// completion is reaped (non-blockingly) by [`Self::reap_tx`].
    tx: TxStaging,
    /// The virtio feature bits the driver negotiated at open (the subset
    /// of the device's offer this driver implements). Queried through the
    /// [`Self::guest_csum`] / [`Self::host_csum`] / [`Self::host_tso`] /
    /// [`Self::mergeable`] accessors rather than a bag of booleans, so the
    /// negotiated set has one source of truth.
    features: u64,
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
    // The virtio 1.1 §3.1 init sequence is one linear procedure —
    // reset, feature negotiation, per-queue setup, DRIVER_OK, the
    // multiqueue control command, staging carve-out — that reads far more
    // clearly as one flow than split across helpers that would each take
    // (and return) the half-built device.
    #[allow(clippy::too_many_lines)]
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
        // Mergeable receive buffers: the device may deliver one frame
        // across several receive buffers, counting them in the first
        // buffer's `num_buffers`. Negotiating it lets the driver post a
        // pool of receive buffers (burst depth) and obliges it to
        // reassemble a multi-buffer frame. It also grows every chain's
        // header to 12 bytes on both rings.
        let mergeable = device_features & wire::VIRTIO_NET_F_MRG_RXBUF != 0;
        // Multiqueue receive: the device steers received frames across
        // several receive queues (`VIRTIO_NET_F_MQ`), selected at runtime
        // through the control virtqueue (`VIRTIO_NET_F_CTRL_VQ`). Negotiate
        // it only when both are offered and the device advertises more than
        // one queue pair; otherwise stay single-queue.
        let ctrl_vq = device_features & wire::VIRTIO_NET_F_CTRL_VQ != 0;
        let multiqueue = ctrl_vq && device_features & wire::VIRTIO_NET_F_MQ != 0;
        // Link-status sensing: with it the device exposes a live `status`
        // word and raises a config-change interrupt on a link change, so
        // the driver reports a real link state instead of a permanent
        // "up". Without it the link is assumed up once operational.
        let status_feature = device_features & wire::VIRTIO_NET_F_STATUS != 0;
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
        if mergeable {
            driver_features |= wire::VIRTIO_NET_F_MRG_RXBUF;
        }
        if multiqueue {
            driver_features |= wire::VIRTIO_NET_F_MQ | wire::VIRTIO_NET_F_CTRL_VQ;
        }
        if status_feature {
            driver_features |= wire::VIRTIO_NET_F_STATUS;
        }
        transport.set_driver_features(driver_features);
        status = status.with(Status::FEATURES_OK);
        transport.set_status(status);
        if !transport.status().contains(Status::FEATURES_OK) {
            return Err(VirtioError::FeaturesRejected);
        }
        // Header + buffer sizing, shared by every receive queue. The
        // virtio_net_hdr grows to 12 bytes on both rings once mergeable
        // receive buffers are negotiated (a transitional device sizes the
        // header uniformly), else it is the legacy 10.
        let rx_hdr_len = if mergeable {
            wire::MRG_HEADER_LEN
        } else {
            wire::HEADER_LEN
        };
        // Each receive buffer is one device-write descriptor holding the
        // inline header followed by up to a full link frame, so a ≤MTU
        // frame lands in one buffer (`num_buffers` == 1). A merged frame's
        // trailing buffers carry pure frame bytes.
        let rx_buf_len = rx_hdr_len + wire::MAX_FRAME_LEN;
        // The transmit staging must hold a segmentation-offload super-frame
        // (the transmit ring's slot capacity) when TSO is negotiated, else
        // a single link frame.
        let max_tx_frame_len = if host_tso {
            RingGeometry::MAX_SLOT_CAPACITY as usize
        } else {
            wire::MAX_FRAME_LEN
        };
        // How many receive queue pairs to enable. A multiqueue device
        // advertises its maximum in the device-config `max_virtqueue_pairs`
        // field; the driver enables the smaller of that and the transport's
        // `MAX_RX_QUEUES` bound, so a device advertising an over-large count
        // never sizes an unbounded number of pinned rings.
        let max_pairs = if multiqueue {
            let mut buf = [0u8; 2];
            transport.read_config(wire::CONFIG_MAX_VQ_PAIRS_OFFSET, &mut buf);
            u16::from_le_bytes(buf)
        } else {
            1
        };
        let pairs: u16 = if multiqueue {
            max_pairs.clamp(1, MAX_RX_QUEUES)
        } else {
            1
        };
        let rx_queue_count = usize::from(pairs);
        // Bring up one receive + one transmit virtqueue per enabled pair.
        // The driver only transmits on pair 0's queue; the other transmit
        // queues are configured (virtio requires every queue of an enabled
        // pair to be set up before the pair count is selected) and then
        // held idle to keep their rings device-visible.
        let mut rx: [Option<RxQueue>; MAX_RX_QUEUES as usize] = core::array::from_fn(|_| None);
        let mut idle_tx: [Option<SplitQueue>; MAX_RX_QUEUES as usize] =
            core::array::from_fn(|_| None);
        for pair in 0..pairs {
            let rx_index = wire::RX_QUEUE + pair * wire::QUEUE_PAIR_STRIDE;
            rx[usize::from(pair)] = Some(RxQueue::new(&mut transport, host, rx_index, rx_buf_len)?);
        }
        let tx_queue = SplitQueue::new(&mut transport, host, wire::TX_QUEUE, wire::TX_QUEUE_SIZE)?;
        for pair in 1..pairs {
            let tx_index = wire::TX_QUEUE + pair * wire::QUEUE_PAIR_STRIDE;
            idle_tx[usize::from(pair)] = Some(SplitQueue::new(
                &mut transport,
                host,
                tx_index,
                wire::TX_QUEUE_SIZE,
            )?);
        }
        // The control queue is the last virtqueue, at index
        // `2 * max_virtqueue_pairs` (virtio 1.1 §5.1.2), present only when
        // multiqueue was negotiated.
        let mut ctrl_queue = if multiqueue {
            let ctrl_index = max_pairs.max(1) * wire::QUEUE_PAIR_STRIDE;
            Some(SplitQueue::new(
                &mut transport,
                host,
                ctrl_index,
                wire::CTRL_QUEUE_SIZE,
            )?)
        } else {
            None
        };
        status = status.with(Status::DRIVER_OK);
        transport.set_status(status);
        // Select the enabled queue-pair count through the control queue (a
        // runtime command issued after DRIVER_OK). A single enabled pair is
        // already the device default, so the command is only issued for more.
        if let Some(ctrl) = ctrl_queue.as_mut() {
            if pairs > 1 {
                set_virtqueue_pairs(&mut transport, host, ctrl, pairs)?;
            }
        }
        // Read MAC from device-config.
        let mut mac = [0u8; MAC_ADDRESS_LEN];
        transport.read_config(wire::CONFIG_MAC_OFFSET, &mut mac);
        // Carve the transmit staging pool once: one header + one frame
        // buffer per concurrently in-flight transmission, sized to the
        // transmit ring so the driver can keep it full without ever waiting
        // on the device.
        let mut tx_free: [Option<(DmaSlab, DmaSlab)>; TX_INFLIGHT_MAX] =
            core::array::from_fn(|_| None);
        for slot in &mut tx_free {
            let header = host
                .alloc_dma_zeroed(rx_hdr_len)
                .map_err(|_| VirtioError::DeviceFault)?;
            let data = host
                .alloc_dma_zeroed(max_tx_frame_len)
                .map_err(|_| VirtioError::DeviceFault)?;
            *slot = Some((header, data));
        }
        let mut net = Self {
            transport,
            rx,
            rx_queue_count,
            tx_queue,
            _idle_tx: idle_tx,
            _ctrl_queue: ctrl_queue,
            _host: host,
            mac: MacAddress::new(mac),
            max_frame_len: wire::MAX_FRAME_LEN,
            max_tx_frame_len,
            rx_hdr_len,
            tx: TxStaging::new(tx_free),
            features: driver_features,
        };
        // Arm every receive queue: the device owns each posted pool from
        // DRIVER_OK onward, so a burst arriving before the first service is
        // captured rather than dropped.
        for pair in 0..net.rx_queue_count {
            if let Some(q) = net.rx[pair].as_mut() {
                q.post_all(&mut net.transport)
                    .map_err(|_| VirtioError::DeviceFault)?;
            }
        }
        Ok(net)
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

    /// Whether `VIRTIO_NET_F_GUEST_CSUM` was negotiated: only then does
    /// the driver honour the device's receive checksum flags.
    fn guest_csum(&self) -> bool {
        self.features & wire::VIRTIO_NET_F_GUEST_CSUM != 0
    }

    /// Whether `VIRTIO_NET_F_CSUM` was negotiated: only then does the
    /// driver ask the device to complete a transmit frame's partial
    /// checksum (and only then does it advertise the offload).
    fn host_csum(&self) -> bool {
        self.features & wire::VIRTIO_NET_F_CSUM != 0
    }

    /// Whether both `VIRTIO_NET_F_HOST_TSO4` and `VIRTIO_NET_F_HOST_TSO6`
    /// were negotiated: only then does the driver build a GSO header and
    /// advertise segmentation for either IP family.
    fn host_tso(&self) -> bool {
        self.features & (wire::VIRTIO_NET_F_HOST_TSO4 | wire::VIRTIO_NET_F_HOST_TSO6)
            == (wire::VIRTIO_NET_F_HOST_TSO4 | wire::VIRTIO_NET_F_HOST_TSO6)
    }

    /// Whether `VIRTIO_NET_F_MRG_RXBUF` was negotiated: only then does a
    /// received frame's first buffer carry a `num_buffers` count and may
    /// a frame span several receive buffers.
    fn mergeable(&self) -> bool {
        self.features & wire::VIRTIO_NET_F_MRG_RXBUF != 0
    }

    /// Whether `VIRTIO_NET_F_STATUS` was negotiated: only then does the
    /// device-config `status` word carry a meaningful link bit.
    fn status_feature(&self) -> bool {
        self.features & wire::VIRTIO_NET_F_STATUS != 0
    }

    /// The device's current link state.
    ///
    /// Reads the device-config `status` word's `VIRTIO_NET_S_LINK_UP` bit
    /// when link-status sensing was negotiated; a device that does not
    /// expose the word (`VIRTIO_NET_F_STATUS` un-negotiated) is reported
    /// [`LinkState::Up`] once operational — an unsensed link is not a
    /// state the stack can act on. Reads two bytes; the transport zero-
    /// fills a short config window, which reads as link-down (fail
    /// closed: an unreadable status is not treated as "up").
    fn read_link(&self) -> LinkState {
        if !self.status_feature() {
            return LinkState::Up;
        }
        let mut status = [0u8; 2];
        self.transport
            .read_config(wire::CONFIG_STATUS_OFFSET, &mut status);
        if u16::from_le_bytes(status) & wire::VIRTIO_NET_S_LINK_UP != 0 {
            LinkState::Up
        } else {
            LinkState::Down
        }
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
    fn build_header(&self, offload: FrameOffload) -> [u8; wire::MRG_HEADER_LEN] {
        // The buffer is always the widest header (12 bytes); the caller
        // transmits only `rx_hdr_len` of it. The trailing `num_buffers`
        // field is zero on transmit (ignored by the device), so a
        // mergeable device sees a well-formed `virtio_net_hdr_mrg_rxbuf`.
        let mut header = [0u8; wire::MRG_HEADER_LEN];
        let write_csum =
            |header: &mut [u8; wire::MRG_HEADER_LEN], csum_start: u16, csum_offset: u16| {
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
            } if self.host_csum() => write_csum(&mut header, csum_start, csum_offset),
            FrameOffload::TxSegment {
                csum_start,
                csum_offset,
                gso_size,
                hdr_len,
                ipv6,
            } if self.host_tso() => {
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

    /// Reap every completed transmission, then drain **every** queued
    /// frame the device can still hold into the transmit ring — never
    /// waiting on the device.
    ///
    /// Completions are reclaimed first (freeing their staging pairs back
    /// to the pool), then queued frames are staged and handed to the
    /// device until either the frame ring is empty or every staging pair
    /// is in flight (the ring is full). Draining the whole burst in one
    /// call — up to [`TX_INFLIGHT_MAX`] frames concurrently in flight — is
    /// what lets a TCP segment and the ACK queued right behind it egress
    /// together, instead of stranding the trailing frame until an
    /// unrelated interrupt drives the next `service`. When the pool is
    /// exhausted the drain applies back-pressure — the remaining queued
    /// frames stay in the frame ring for the next completion-driven call —
    /// rather than parking (this doorbell may be a cross-process `Service`
    /// call, where parking would block the reply and the serve loop) or
    /// dropping. The device consumes each chain asynchronously and raises
    /// its (shared) interrupt; the driver's interrupt handler rings the
    /// stack, which issues the next `service` that reaps the completions
    /// here.
    fn drain_tx(
        &mut self,
        rings: &mut FrameRings<'_>,
        report: &mut ServiceReport,
    ) -> Result<(), DriverError> {
        self.reap_tx();
        loop {
            let Some((header_slab, data_slab)) = self.tx.take_free() else {
                // Every staging pair is in flight: the ring is full. Leave
                // the remaining queued frames for the next completion-
                // driven call — never wait, never drop (back-pressure).
                return Ok(());
            };
            let mut header_bb = BounceBuffer::new(header_slab, rings.class);
            let mut data_bb = BounceBuffer::new(data_slab, rings.class);
            match self.stage_and_post(rings, &mut header_bb, &mut data_bb)? {
                TxOutcome::Sent(head) => {
                    // The device now owns the chain: hold the staging,
                    // keyed by its descriptor head, until the completion is
                    // reaped (never reused or scrubbed meanwhile).
                    self.tx.record_inflight(head, header_bb, data_bb);
                    report.transmitted += 1;
                }
                TxOutcome::Dropped => {
                    // Nothing was handed to the device: return the staging
                    // to the pool (scrubbed when the class is sensitive).
                    self.tx
                        .return_free(header_bb.into_slab(), data_bb.into_slab());
                }
                TxOutcome::Empty => {
                    self.tx
                        .return_free(header_bb.into_slab(), data_bb.into_slab());
                    return Ok(());
                }
            }
        }
    }

    /// Reclaim every completed transmission's staging, non-blockingly.
    ///
    /// Drains the transmit used ring (never waits): each completion's
    /// descriptor head reclaims exactly the staging pair that produced it
    /// (see [`TxStaging::reclaim`]), returning the buffers to the pool
    /// through [`BounceBuffer::into_slab`], which scrubs them when the ring
    /// class declared the traffic sensitive — the frame's bytes are zeroed
    /// here, after the device has read them, never before. A malformed
    /// completion has consumed its used-ring slot but names no chain the
    /// driver can trust, so it reclaims no staging (fail closed) while the
    /// reap keeps draining the genuine completions behind it.
    fn reap_tx(&mut self) {
        loop {
            match self.tx_queue.poll_used() {
                Ok(token) => self.tx.reclaim(token.head),
                // A device-fabricated / malformed completion: skip it (its
                // used-ring slot is already consumed) and keep reaping the
                // genuine completions behind it — reclaim no staging.
                Err(VirtioError::MalformedCompletion) => {}
                // No further completion (or a transient error): done.
                Err(_) => return,
            }
        }
    }

    /// Copy one queued TX frame into `data_bb`, build its header into
    /// `header_bb`, and post the two-segment chain to the device (add +
    /// kick). Pure staging: it never waits and never records the frame as
    /// in flight — the caller owns the buffers' fate by the returned
    /// [`TxOutcome`], and [`TxOutcome::Sent`] carries the descriptor head
    /// so the caller can key the staging for completion. A frame the
    /// device cannot move (runt, over-MTU, corrupt slot) is consumed and
    /// dropped so the queue behind it keeps flowing.
    fn stage_and_post(
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
        // Stage only the negotiated header length: the 12-byte
        // `virtio_net_hdr_mrg_rxbuf` when mergeable, else the 10-byte
        // legacy header (the trailing zero `num_buffers` is dropped).
        header_bb.stage(&self.build_header(offload)[..self.rx_hdr_len])?;
        let frame_len_u32 = u32::try_from(len).map_err(|_| DriverError::LengthOutOfRange)?;
        let segments = [
            ChainSegment {
                phys: header_bb.phys(),
                len: u32::try_from(self.rx_hdr_len).unwrap_or(0),
                direction: Direction::DeviceRead,
            },
            ChainSegment {
                phys: data_bb.phys(),
                len: frame_len_u32,
                direction: Direction::DeviceRead,
            },
        ];
        let head = self
            .tx_queue
            .add_chain(&segments)
            .map_err(VirtioError::as_driver_error)?;
        self.tx_queue.kick(&mut self.transport);
        Ok(TxOutcome::Sent(head))
    }

    /// Move delivered frames from every receive queue into their RX rings
    /// until each device queue is drained or its ring is full.
    ///
    /// A multiqueue device (`plans/NETWORK.md` N7c-2) steers received
    /// frames across several receive queues; each is harvested into its
    /// own shared RX ring, so a busy link's receive work is spread rather
    /// than serialised behind one queue. Per-queue back-pressure holds a
    /// reassembled frame in that queue when its ring is full and retries
    /// it before the next completion, so nothing the device handed over is
    /// dropped and each ring's order is preserved.
    fn harvest_rx(
        &mut self,
        rings: &mut FrameRings<'_>,
        report: &mut ServiceReport,
    ) -> Result<(), DriverError> {
        let hdr_len = self.rx_hdr_len;
        let max_frame_len = self.max_frame_len;
        let guest_csum = self.guest_csum();
        let mergeable = self.mergeable();
        let sensitive = rings.class.is_sensitive();
        for pair in 0..self.rx_queue_count {
            // Each queue drains into its own shared RX ring; a geometry
            // that provides fewer rings than the device has queues fails
            // closed rather than silently dropping a queue's traffic.
            let ring = rings.rx_ring(pair).map_err(|_| DriverError::BadMagic)?;
            if let Some(queue) = self.rx[pair].as_mut() {
                queue.harvest(
                    ring,
                    &mut self.transport,
                    hdr_len,
                    max_frame_len,
                    guest_csum,
                    mergeable,
                    sensitive,
                    report,
                )?;
            }
        }
        Ok(())
    }
}

/// Bounded poll budget for a control-queue command completion.
///
/// The device processes a virtqueue notify synchronously on the
/// triggering vmexit (a real QEMU / hardware handshake), so the ack is
/// ready on the first poll; the budget is only a fail-closed ceiling on a
/// device that never answers. This is a one-shot control handshake run
/// once at open, before any frame flow — never a steady-state spin.
const CTRL_POLL_BUDGET: usize = 1 << 20;

/// Issue the `VIRTIO_NET_CTRL_MQ_VQ_PAIRS_SET` control command, selecting
/// how many receive/transmit queue pairs the device uses.
///
/// The command chain is one device-read segment (the
/// `virtio_net_ctrl_hdr` `{class, cmd}` plus the `le16` pair count) and
/// one device-write segment (the ack byte). Fails closed on a non-`OK`
/// ack or a device that never completes the command.
fn set_virtqueue_pairs<T: Transport>(
    transport: &mut T,
    host: &dyn VirtioHost,
    ctrl: &mut SplitQueue,
    pairs: u16,
) -> Result<(), VirtioError> {
    let mut cmd = host
        .alloc_dma_zeroed(8)
        .map_err(|_| VirtioError::DeviceFault)?;
    {
        let bytes = cmd.as_bytes_mut();
        bytes[0] = wire::VIRTIO_NET_CTRL_MQ;
        bytes[1] = wire::VIRTIO_NET_CTRL_MQ_VQ_PAIRS_SET;
        bytes[2..4].copy_from_slice(&pairs.to_le_bytes());
        // Ack sentinel: the device overwrites it with VIRTIO_NET_OK on
        // success, so a device that ignores the command fails closed.
        bytes[4] = 0xFF;
    }
    let phys = cmd.phys();
    let segments = [
        ChainSegment {
            phys,
            len: 4,
            direction: Direction::DeviceRead,
        },
        ChainSegment {
            phys: phys + 4,
            len: 1,
            direction: Direction::DeviceWrite,
        },
    ];
    ctrl.add_chain(&segments)?;
    ctrl.kick(transport);
    for _ in 0..CTRL_POLL_BUDGET {
        match ctrl.poll_used() {
            Ok(_) => {
                // Acquire the device's ack write before reading it.
                cmd.sync_range(0, 5);
                return if cmd.as_bytes()[4] == wire::VIRTIO_NET_OK {
                    Ok(())
                } else {
                    Err(VirtioError::DeviceFault)
                };
            }
            Err(VirtioError::NoCompletion | VirtioError::MalformedCompletion) => {}
            Err(e) => return Err(e),
        }
    }
    Err(VirtioError::DeviceFault)
}

/// Outcome of one TX-frame staging in [`VirtioNet::stage_and_post`].
enum TxOutcome {
    /// A frame was handed to the device, carrying the descriptor head the
    /// device returns at completion so the caller keys the staging pair.
    Sent(u16),
    /// A frame the device cannot move was consumed and dropped.
    Dropped,
    /// The TX ring is empty.
    Empty,
}

impl<T: Transport> Net for VirtioNet<'_, T> {
    fn device_facts(&self) -> Result<DeviceFacts, DriverError> {
        // The link state comes from the device (VIRTIO_NET_F_STATUS) when
        // negotiated, else an operational device reports its link up. The
        // receive-queue count is the number of queues actually enabled
        // (one unless VIRTIO_NET_F_MQ was negotiated). Offloads are
        // advertised, each only when its virtio feature was negotiated:
        // receive-checksum validation (`VIRTIO_NET_F_GUEST_CSUM`), TCP
        // transmit-checksum offload (`VIRTIO_NET_F_CSUM`), and TCP
        // segmentation offload (`VIRTIO_NET_F_HOST_TSO4`+`TSO6`). UDP
        // transmit offload is not advertised: the stack keeps UDP on the
        // software path (its zero-checksum-as-0xFFFF rule is not expressed
        // by the virtio partial-checksum contract).
        let mut offloads = NetOffloads::empty();
        if self.guest_csum() {
            offloads =
                NetOffloads::from_bits(offloads.bits() | NetOffloads::RX_CSUM_VALIDATED.bits())
                    .unwrap_or(NetOffloads::empty());
        }
        if self.host_csum() {
            offloads = NetOffloads::from_bits(offloads.bits() | NetOffloads::TX_CSUM_TCP.bits())
                .unwrap_or(offloads);
        }
        if self.host_tso() {
            offloads = NetOffloads::from_bits(offloads.bits() | NetOffloads::TX_SEGMENT_TCP.bits())
                .unwrap_or(offloads);
        }
        Ok(DeviceFacts {
            mac: self.mac,
            mtu: u32::try_from(self.max_frame_len - wire::MIN_ETHERNET_FRAME)
                .map_err(|_| DriverError::LengthOutOfRange)?,
            link: self.read_link(),
            offloads,
            rx_queues: u16::try_from(self.rx_queue_count.max(1)).unwrap_or(1),
        })
    }

    fn service(&mut self, rings: &mut FrameRings<'_>) -> Result<ServiceReport, DriverError> {
        let mut report = ServiceReport::default();
        self.drain_tx(rings, &mut report)?;
        self.harvest_rx(rings, &mut report)?;
        // Report the live link on every doorbell so a config-change the
        // driver interrupt woke the stack for reaches it here: the stack
        // turns a change into a bond-member link report (the sole live
        // source of a failover).
        report.link = self.read_link();
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
