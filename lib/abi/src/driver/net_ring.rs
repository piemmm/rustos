//! The shared-memory frame-ring transport between the network stack
//! and a link-layer driver (`plans/NETWORK.md` §2.3).
//!
//! A [`FrameRings`] pair lives in one byte region the stack owns (a
//! plain buffer in-process, an `shm_create` region across processes):
//! the **TX ring** carries frames the stack queued for the device and
//! the **RX ring** carries frames the device delivered to the stack.
//! Frames never cross the IPC boundary — only the doorbell call and
//! its [`ServiceReport`] do.
//!
//! # A single-producer, single-consumer ring
//!
//! Each ring has exactly one producer and one consumer, in different
//! address spaces, and they run **concurrently**: a driver woken by its
//! device interrupt fills the receive ring and rings a doorbell without
//! waiting for the stack, and the stack drains that ring without a call
//! back into the driver. So the ring's counters are genuine shared
//! mutable state and are [`AtomicU32`]s, not plain bytes:
//!
//! * the producer writes a slot's bytes, then **releases** its counter;
//! * the consumer **acquires** that counter before reading the slot, and
//!   releases its own only once it has finished with the bytes, so the
//!   producer cannot overwrite a slot still being read.
//!
//! The two counters sit in separate cache lines. In one line every
//! publish would invalidate the peer's read of the other counter and the
//! two CPUs would ping-pong the line for every frame.
//!
//! # Fail closed
//!
//! Both sides treat the region as untrusted input: geometry is agreed
//! out-of-band and validated at construction, and every counter and
//! per-slot length is re-validated on the operation that reads it. Both
//! counters live in memory the *peer* can write, so a corrupt or hostile
//! value is a typed error — never an out-of-bounds access, never a guess.
//! Each operation snapshots the counters once and works from that
//! snapshot, so a peer that mutates its counter mid-operation cannot
//! steer a second read past the bound the first one checked.

use core::sync::atomic::{AtomicU32, Ordering};

use crate::le::{put_u32, read_u32};
use crate::Errno;

/// Alignment a ring region must have: that of the header's counters.
const INDEX_ALIGN: usize = align_of::<AtomicU32>();

/// Bytes assumed for one cache line. Only an upper bound matters — it
/// keeps the producer and consumer counters off each other's line — and
/// 64 covers every Tier-1 target.
const CACHE_LINE_BYTES: usize = 64;

/// Byte length of one ring's control header: the free-running producer
/// and consumer counters, each alone in a cache line.
pub const RING_HEADER_LEN: usize = 2 * CACHE_LINE_BYTES;

/// Byte offset of the producer counter within a ring header.
const HEADER_PRODUCER: usize = 0;

/// Byte offset of the consumer counter within a ring header.
const HEADER_CONSUMER: usize = CACHE_LINE_BYTES;

/// Index of the producer counter among the header's atomic cells.
const PRODUCER_CELL: usize = HEADER_PRODUCER / INDEX_ALIGN;

/// Index of the consumer counter among the header's atomic cells.
const CONSUMER_CELL: usize = HEADER_CONSUMER / INDEX_ALIGN;

/// Extra bytes an in-process buffer needs so an aligned region of the
/// wanted length can be cut from it (see [`aligned_region`]).
pub const REGION_ALIGN_PADDING: usize = INDEX_ALIGN - 1;

/// Cut an aligned `len`-byte region out of `buffer`, or [`None`] when
/// `buffer` is too short.
///
/// [`FrameRings::bind`] requires an aligned region because the ring
/// headers' counters are atomics. A cross-process region comes from
/// `shm` and is page-aligned already; an in-process buffer (a host test,
/// the single-address-space local frame service) is only byte-aligned, so
/// it is over-allocated by [`REGION_ALIGN_PADDING`] and trimmed here —
/// one definition, rather than each caller doing pointer arithmetic.
#[must_use]
pub fn aligned_region(buffer: &mut [u8], len: usize) -> Option<&mut [u8]> {
    let offset = buffer.as_ptr().align_offset(INDEX_ALIGN);
    buffer.get_mut(offset..offset.checked_add(len)?)
}

/// Byte length of one slot's per-frame offload-metadata prefix (the
/// transport-neutral analogue of a device's per-descriptor offload
/// header, e.g. `virtio_net_hdr`): one tag byte followed by the two
/// little-endian `u16` checksum offsets a partial-checksum descriptor
/// carries and the two little-endian `u16` segmentation fields
/// (`gso_size`, `hdr_len`) a TCP-segmentation descriptor carries. It
/// precedes the length prefix in every slot so a frame carries the
/// offload the stack requested (transmit) or the device performed
/// (receive) alongside its bytes, without a second channel.
///
/// Public, like [`RING_HEADER_LEN`], because it is part of the region
/// layout both sides agree on: a consumer reasoning about a slot's bytes
/// derives the offset from it rather than hard-coding one.
pub const SLOT_META_LEN: usize = 1 + 2 + 2 + 2 + 2;

/// Byte offset of `csum_start` within a slot's metadata prefix.
const META_CSUM_START: usize = 1;
/// Byte offset of `csum_offset` within a slot's metadata prefix.
const META_CSUM_OFFSET: usize = 3;
/// Byte offset of `gso_size` within a slot's metadata prefix.
const META_GSO_SIZE: usize = 5;
/// Byte offset of `hdr_len` within a slot's metadata prefix.
const META_HDR_LEN: usize = 7;

/// Byte length of one slot's length prefix.
const SLOT_LEN_PREFIX: usize = 4;

/// Decides whether a received frame can possibly have a local consumer,
/// consulted before it is copied into a receive ring.
///
/// The driver's harvest path evaluates this on the frame it is about to hand
/// over, so a frame nothing here could want costs neither the copy nor —
/// far more expensively — a wake of the network stack and a full protocol
/// parse. On a busy segment most broadcast traffic is addressed to other
/// hosts, and that is what this exists to shed.
///
/// # Its bias is to admit
///
/// It is a load-shedding optimisation and is **never** load-bearing for
/// security: the stack still validates every frame it does receive, and a
/// driver process already owns the device and could drop whatever it liked,
/// so refusing here grants nothing. An implementation that cannot parse a
/// frame confidently must therefore admit it — dropping is the optimisation,
/// delivering is the safe default.
pub trait RxAdmit: core::fmt::Debug {
    /// Whether `frame` may have a local consumer.
    fn admit(&self, frame: &[u8]) -> bool;
}

/// What became of a frame a device offered to a receive ring.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum RxDelivery {
    /// Copied into the ring, awaiting the stack.
    Accepted,
    /// The pre-filter found no possible local consumer, so it was not
    /// copied. The caller still frees the device's slot — the frame is
    /// finished with, not deferred.
    Filtered,
    /// Every ring slot is occupied. The frame stays in the device and the
    /// caller must not free its slot; the stack drains and asks again.
    RingFull,
}

/// Per-frame offload descriptor carried in a ring slot's metadata.
///
/// The transport-neutral analogue of a NIC's per-descriptor offload
/// header. A frame carries the offload the stack requested (transmit)
/// or the offload the device performed (receive). The vocabulary is
/// closed and versioned like the rest of `abi-v1`; the decoder is
/// fail-closed — an unrecognised tag decodes to [`FrameOffload::None`]
/// (software path), so a corrupt or hostile meta byte can only *lose*
/// an offload, never fabricate one.
///
/// No offload is ever load-bearing for security (`plans/NETWORK.md`
/// §2.3): [`Validated`](FrameOffload::Validated) lets the stack skip
/// the redundant checksum *fold* and [`NeedsChecksum`] only asks the
/// stack to *finish* a fold it would run anyway; every semantic
/// validation (lengths, addresses, protocol state) still runs, and the
/// offsets a [`NeedsChecksum`] carries are re-validated by the consumer
/// against the frame length before use. Trust is in the device, never
/// in the peer.
///
/// [`NeedsChecksum`]: FrameOffload::NeedsChecksum
#[derive(Copy, Clone, Debug, Eq, PartialEq, Default)]
pub enum FrameOffload {
    /// No offload: the frame's checksums are computed on transmit and
    /// verified on receive in software — the canonical path.
    #[default]
    None,
    /// Receive only: the device verified the frame's transport-layer
    /// checksum, so the stack may skip the software fold.
    Validated,
    /// Receive only: the device delivered a frame whose transport
    /// checksum field holds only the partial (pseudo-header) sum and
    /// must be *completed* by folding the frame bytes from `csum_start`
    /// to the end and storing the result at `csum_start + csum_offset`
    /// (the `virtio_net` `VIRTIO_NET_HDR_F_NEEDS_CSUM` case). The
    /// consumer validates both offsets against the frame length before
    /// touching a byte, and fails closed if they do not fit.
    NeedsChecksum {
        /// Byte offset in the frame where the checksummed range starts.
        csum_start: u16,
        /// Byte offset, past `csum_start`, of the 16-bit checksum field.
        csum_offset: u16,
    },
    /// Transmit only: the stack has placed only the partial
    /// (pseudo-header) checksum in the frame's transport checksum field
    /// and asks the device to complete it — fold the frame bytes from
    /// `csum_start` to the end and store the result at
    /// `csum_start + csum_offset` (virtio `VIRTIO_NET_HDR_F_NEEDS_CSUM`
    /// on the transmit path). The device performs the fold the stack
    /// would otherwise have run in software; the offsets are re-validated
    /// against the frame length by the driver before use, and a device
    /// that ignores the request never sees it (the offload is negotiated
    /// per `NetOffloads`), so the frame still carries a valid — merely
    /// partial — checksum field.
    TxChecksum {
        /// Byte offset in the frame where the checksummed range starts.
        csum_start: u16,
        /// Byte offset, past `csum_start`, of the 16-bit checksum field.
        csum_offset: u16,
    },
    /// Transmit only: the stack has placed one over-size TCP segment in
    /// the frame — an Ethernet + IP + TCP header (with its transport
    /// checksum field left partial, exactly as [`TxChecksum`]) followed
    /// by a payload larger than one `gso_size` — and asks the device to
    /// segment it into MTU-sized packets on the wire (TCP segmentation
    /// offload; virtio `VIRTIO_NET_HDR_GSO_TCPV4`/`TCPV6`). The device
    /// replicates the `hdr_len`-byte header for each `gso_size`-byte
    /// slice, advancing the sequence number, recomputing each segment's
    /// IP/TCP checksums, and setting `PSH`/`FIN` only on the final
    /// segment. `ipv6` selects the GSO type (`TCPV6` vs `TCPV4`). The
    /// fields are re-validated against the frame length by the driver
    /// before use, and a device that never negotiated segmentation never
    /// receives this offload (the stack keeps the whole segment within
    /// one MTU otherwise), so the frame is always independently
    /// transmittable.
    ///
    /// [`TxChecksum`]: FrameOffload::TxChecksum
    TxSegment {
        /// Byte offset in the frame where the checksummed range starts
        /// (the transport header — Ethernet + IP headers).
        csum_start: u16,
        /// Byte offset, past `csum_start`, of the 16-bit TCP checksum
        /// field.
        csum_offset: u16,
        /// Maximum TCP payload bytes per emitted segment (the effective
        /// MSS the device segments against).
        gso_size: u16,
        /// Total header bytes the device replicates per segment
        /// (Ethernet + IP + TCP header, including TCP options).
        hdr_len: u16,
        /// Whether the segment is IPv6 (`TCPV6`) rather than IPv4
        /// (`TCPV4`).
        ipv6: bool,
    },
}

impl FrameOffload {
    /// Encode the offload's one-byte ring tag. Segmentation carries the
    /// IP family in the tag (`4` = `TCPV4`, `5` = `TCPV6`) so the driver
    /// need not parse the frame to choose the GSO type.
    const fn to_tag(self) -> u8 {
        match self {
            FrameOffload::None => 0,
            FrameOffload::Validated => 1,
            FrameOffload::NeedsChecksum { .. } => 2,
            FrameOffload::TxChecksum { .. } => 3,
            FrameOffload::TxSegment { ipv6: false, .. } => 4,
            FrameOffload::TxSegment { ipv6: true, .. } => 5,
        }
    }

    /// The two checksum offsets carried in the slot (zero unless this is
    /// a partial-checksum offload — [`FrameOffload::NeedsChecksum`] on
    /// receive, or [`FrameOffload::TxChecksum`] / [`FrameOffload::TxSegment`]
    /// on transmit).
    const fn offsets(self) -> (u16, u16) {
        match self {
            FrameOffload::NeedsChecksum {
                csum_start,
                csum_offset,
            }
            | FrameOffload::TxChecksum {
                csum_start,
                csum_offset,
            }
            | FrameOffload::TxSegment {
                csum_start,
                csum_offset,
                ..
            } => (csum_start, csum_offset),
            _ => (0, 0),
        }
    }

    /// The two segmentation fields carried in the slot (`gso_size`,
    /// `hdr_len`); zero unless this is [`FrameOffload::TxSegment`].
    const fn gso(self) -> (u16, u16) {
        match self {
            FrameOffload::TxSegment {
                gso_size, hdr_len, ..
            } => (gso_size, hdr_len),
            _ => (0, 0),
        }
    }

    /// Decode a ring meta record, fail-closed: an unknown tag is
    /// [`FrameOffload::None`], never a fabricated offload.
    const fn decode(
        tag: u8,
        csum_start: u16,
        csum_offset: u16,
        gso_size: u16,
        hdr_len: u16,
    ) -> Self {
        match tag {
            1 => FrameOffload::Validated,
            2 => FrameOffload::NeedsChecksum {
                csum_start,
                csum_offset,
            },
            3 => FrameOffload::TxChecksum {
                csum_start,
                csum_offset,
            },
            4 => FrameOffload::TxSegment {
                csum_start,
                csum_offset,
                gso_size,
                hdr_len,
                ipv6: false,
            },
            5 => FrameOffload::TxSegment {
                csum_start,
                csum_offset,
                gso_size,
                hdr_len,
                ipv6: true,
            },
            _ => FrameOffload::None,
        }
    }
}

/// Largest number of receive queues one region carries.
///
/// A multiqueue device (`VIRTIO_NET_F_MQ`) steers received frames across
/// several receive queues so a busy link's receive work can be spread rather
/// than serialised behind one queue (`plans/NETWORK.md` N7c-2). Each receive
/// queue reserves its **own** full ring inside the shared region, so the count
/// is a pinned-memory resource bound, not an unbounded capacity: a hostile or
/// buggy device advertising a huge queue count must not be able to demand an
/// attacker-sized region. Eight receive queues is ample to spread device
/// receive steering across the cores of a general-purpose machine while
/// bounding the region; the *active* count is the device's own `rx_queues`
/// clamped to this ceiling ([`RingGeometry::for_device`]). Transmit stays a
/// single queue: the stack serialises its own egress, so a second transmit ring
/// would add pinned memory and complexity without a consumer.
pub const MAX_RX_QUEUES: u16 = 8;

/// Validated geometry of a receive + transmit frame ring set: how many
/// receive queues the region carries, how many slots each ring holds,
/// and the largest frame one slot carries in each direction.
///
/// A region holds [`Self::rx_queues`] independent **receive** rings
/// followed by one **transmit** ring. The two directions carry
/// **independent** slot capacities so the transmit ring can hold an
/// over-size TCP-segmentation-offload super-frame (up to
/// [`Self::MAX_SLOT_CAPACITY`]) without wasting that space on every
/// receive slot, which only ever holds a link-MTU frame. Every ring
/// shares the slot *count*. Geometry is agreed out-of-band (both sides
/// derive it from the same [`DeviceFacts`](super::net::DeviceFacts) and
/// negotiated [`NetOffloads`](super::net::NetOffloads)) and validated
/// fail-closed here, so a corrupt or hostile geometry can never size an
/// out-of-bounds view over the shared region.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct RingGeometry {
    slots: u32,
    rx_slot_capacity: u32,
    tx_slot_capacity: u32,
    rx_queues: u16,
}

impl RingGeometry {
    /// Fewest slots a ring may hold; one in flight plus one being
    /// filled is the minimum that avoids strict lock-step.
    pub const MIN_SLOTS: u32 = 2;

    /// Most slots a ring may hold. A validation bound: beyond this a
    /// hostile geometry buys no throughput while reserving unbounded
    /// pinned memory.
    pub const MAX_SLOTS: u32 = 1024;

    /// Smallest slot capacity: the 68-byte IPv4 reassembly-floor MTU
    /// plus the Ethernet header.
    pub const MIN_SLOT_CAPACITY: u32 = 68 + super::net::ETHERNET_HEADER_LEN;

    /// Largest slot capacity: the 65 535-byte MTU ceiling plus the
    /// Ethernet header. Also the ceiling for a TCP-segmentation-offload
    /// super-frame's Ethernet-framed length.
    pub const MAX_SLOT_CAPACITY: u32 = 65_535 + super::net::ETHERNET_HEADER_LEN;

    /// Validate and build a geometry with `rx_queues` receive rings and
    /// one transmit ring.
    ///
    /// # Errors
    ///
    /// Returns [`Errno::OutOfRange`] when `slots` lies outside
    /// [`Self::MIN_SLOTS`]`..=`[`Self::MAX_SLOTS`], either capacity lies
    /// outside [`Self::MIN_SLOT_CAPACITY`]`..=`[`Self::MAX_SLOT_CAPACITY`],
    /// or `rx_queues` lies outside `1..=`[`MAX_RX_QUEUES`].
    pub const fn new(
        slots: u32,
        rx_slot_capacity: u32,
        tx_slot_capacity: u32,
        rx_queues: u16,
    ) -> Result<Self, Errno> {
        if slots < Self::MIN_SLOTS || slots > Self::MAX_SLOTS {
            return Err(Errno::OutOfRange);
        }
        if !Self::capacity_in_range(rx_slot_capacity) || !Self::capacity_in_range(tx_slot_capacity)
        {
            return Err(Errno::OutOfRange);
        }
        if rx_queues < 1 || rx_queues > MAX_RX_QUEUES {
            return Err(Errno::OutOfRange);
        }
        Ok(Self {
            slots,
            rx_slot_capacity,
            tx_slot_capacity,
            rx_queues,
        })
    }

    const fn capacity_in_range(capacity: u32) -> bool {
        capacity >= Self::MIN_SLOT_CAPACITY && capacity <= Self::MAX_SLOT_CAPACITY
    }

    /// Number of slots in each ring.
    #[must_use]
    pub const fn slots(&self) -> u32 {
        self.slots
    }

    /// Largest frame one receive slot carries, in bytes.
    #[must_use]
    pub const fn rx_slot_capacity(&self) -> u32 {
        self.rx_slot_capacity
    }

    /// Largest frame one transmit slot carries, in bytes.
    #[must_use]
    pub const fn tx_slot_capacity(&self) -> u32 {
        self.tx_slot_capacity
    }

    /// Number of receive rings the region carries (`1..=`[`MAX_RX_QUEUES`]).
    #[must_use]
    pub const fn rx_queues(&self) -> u16 {
        self.rx_queues
    }

    /// Bytes one slot of `slot_capacity` occupies: its offload-metadata
    /// prefix, its length prefix, and its payload capacity.
    /// Bytes one slot occupies: its metadata prefix, length prefix, and
    /// payload capacity, rounded up so every ring length — and hence
    /// every ring's start offset inside a region — is a multiple of
    /// [`INDEX_ALIGN`], which is what keeps each ring header's counters
    /// aligned for their atomic access.
    const fn slot_stride(slot_capacity: u32) -> usize {
        (SLOT_META_LEN + SLOT_LEN_PREFIX + slot_capacity as usize).next_multiple_of(INDEX_ALIGN)
    }

    /// Bytes a ring of `slots` slots of `slot_capacity` occupies:
    /// header plus every slot.
    const fn ring_len_for(slots: u32, slot_capacity: u32) -> usize {
        RING_HEADER_LEN + slots as usize * Self::slot_stride(slot_capacity)
    }

    /// Bytes the receive ring occupies.
    #[must_use]
    pub const fn rx_ring_len(&self) -> usize {
        Self::ring_len_for(self.slots, self.rx_slot_capacity)
    }

    /// Bytes the transmit ring occupies.
    #[must_use]
    pub const fn tx_ring_len(&self) -> usize {
        Self::ring_len_for(self.slots, self.tx_slot_capacity)
    }

    /// Bytes a whole [`FrameRings`] region occupies: the `rx_queues`
    /// receive rings followed by the single transmit ring.
    #[must_use]
    pub const fn region_len(&self) -> usize {
        self.rx_queues as usize * self.rx_ring_len() + self.tx_ring_len()
    }

    /// The geometry both the stack and the driver derive from the same
    /// [`DeviceFacts`](super::net::DeviceFacts) and a shared slot count —
    /// the one definition of "how big are the rings for this device", so
    /// the two sides cannot disagree.
    ///
    /// A receive slot holds one link frame (the MTU plus the Ethernet
    /// header). A transmit slot holds the same *unless* the device
    /// negotiated TCP segmentation offload
    /// ([`NetOffloads::TX_SEGMENT_TCP`](super::net::NetOffloads::TX_SEGMENT_TCP)),
    /// in which case it is sized to [`Self::MAX_SLOT_CAPACITY`] so the
    /// stack can hand the device one over-MTU super-segment to split —
    /// the receive ring is never enlarged for it.
    ///
    /// The receive-queue count is the device's own
    /// [`DeviceFacts::rx_queues`](super::net::DeviceFacts::rx_queues)
    /// clamped to [`MAX_RX_QUEUES`], so a device advertising more receive
    /// queues than the transport bounds is served on the first
    /// [`MAX_RX_QUEUES`] and never sizes an over-large region.
    ///
    /// # Errors
    ///
    /// Returns [`Errno::OutOfRange`] when `slots` is out of range or the
    /// MTU (plus the Ethernet header) is not a valid slot capacity.
    pub fn for_device(facts: &super::net::DeviceFacts, slots: u32) -> Result<Self, Errno> {
        let rx = facts
            .mtu
            .checked_add(super::net::ETHERNET_HEADER_LEN)
            .ok_or(Errno::OutOfRange)?;
        let tx = if facts
            .offloads
            .contains(super::net::NetOffloads::TX_SEGMENT_TCP)
        {
            Self::MAX_SLOT_CAPACITY
        } else {
            rx
        };
        let rx_queues = facts.rx_queues.clamp(1, MAX_RX_QUEUES);
        Self::new(slots, rx, tx, rx_queues)
    }
}

/// One direction's bounded single-producer, single-consumer frame queue
/// inside the shared region.
///
/// The producer and consumer counters are free-running (wrapping) `u32`s
/// held in the ring header as atomics, because the two sides run
/// concurrently in different address spaces. Occupancy is their wrapping
/// difference and is re-validated against the slot count on every
/// operation, so a counter the peer corrupted is refused rather than
/// driving an out-of-bounds slot index.
#[derive(Debug)]
pub struct FrameRing<'a> {
    /// The producer counter: slots published, mod 2³².
    producer: &'a AtomicU32,
    /// The consumer counter: slots released back, mod 2³².
    consumer: &'a AtomicU32,
    /// The slot area — the region past the header, so a slot offset needs
    /// no header bias.
    slots_region: &'a mut [u8],
    slots: u32,
    slot_capacity: u32,
}

impl<'a> FrameRing<'a> {
    /// Bind a ring view over `region` for `slots` slots each carrying up
    /// to `slot_capacity` payload bytes.
    ///
    /// # Errors
    ///
    /// * [`Errno::BufferTooSmall`] — `region` is not exactly the ring
    ///   length `slots` × stride + header.
    /// * [`Errno::BadAlignment`] — `region` is not aligned for the
    ///   header's atomic counters. Use [`aligned_region`] to cut an
    ///   aligned view from a plain in-process buffer.
    pub fn bind(region: &'a mut [u8], slots: u32, slot_capacity: u32) -> Result<Self, Errno> {
        if region.len() != RingGeometry::ring_len_for(slots, slot_capacity) {
            return Err(Errno::BufferTooSmall);
        }
        let (header, slots_region) = region.split_at_mut(RING_HEADER_LEN);
        // Give up the exclusive borrow of the header: from here it is only
        // ever read and written through the two atomics, which the peer
        // accesses concurrently.
        let header: &'a [u8] = header;
        // SAFETY: reinterpreting initialised `u8`s as `AtomicU32`s is the
        // transmute `align_to` documents, and it is valid here: `AtomicU32`
        // has `u32`'s layout and no invalid bit pattern, so every 4-byte
        // group of the header is a legal value. `align_to` itself computes
        // the split, so nothing is assumed about the region's alignment —
        // a misaligned base simply yields a non-empty prefix, which is
        // rejected below. Atomics rather than plain reads are precisely
        // what a peer process concurrently accessing these bytes requires.
        let (prefix, cells, _) = unsafe { header.align_to::<AtomicU32>() };
        if !prefix.is_empty() {
            return Err(Errno::BadAlignment);
        }
        let (Some(producer), Some(consumer)) = (cells.get(PRODUCER_CELL), cells.get(CONSUMER_CELL))
        else {
            return Err(Errno::BadAlignment);
        };
        Ok(Self {
            producer,
            consumer,
            slots_region,
            slots,
            slot_capacity,
        })
    }

    /// Number of slots in the ring.
    #[must_use]
    pub const fn slots(&self) -> u32 {
        self.slots
    }

    /// Largest frame one slot carries, in bytes.
    #[must_use]
    pub const fn slot_capacity(&self) -> u32 {
        self.slot_capacity
    }

    /// Snapshot both counters and validate their occupancy once, so the
    /// rest of an operation works from values a mutating peer can no
    /// longer change under it.
    ///
    /// # Errors
    ///
    /// [`Errno::OutOfRange`] when the occupancy exceeds the slot count —
    /// a counter the peer corrupted, refused rather than acted on.
    fn snapshot(&self) -> Result<(u32, u32), Errno> {
        // Acquire on the peer's counter: it orders this side's reads of
        // the slot bytes the peer released with that counter.
        let producer = self.producer.load(Ordering::Acquire);
        let consumer = self.consumer.load(Ordering::Acquire);
        if producer.wrapping_sub(consumer) > self.slots {
            return Err(Errno::OutOfRange);
        }
        Ok((producer, consumer))
    }

    /// Publish `value` as the producer counter, releasing the slot bytes
    /// written before it.
    fn publish_producer(&self, value: u32) {
        self.producer.store(value, Ordering::Release);
    }

    /// Publish `value` as the consumer counter, releasing the slot the
    /// producer may now refill.
    fn publish_consumer(&self, value: u32) {
        self.consumer.store(value, Ordering::Release);
    }

    /// Frames currently queued.
    ///
    /// # Errors
    ///
    /// Returns [`Errno::OutOfRange`] when the counters are corrupt
    /// (occupancy exceeds the slot count).
    pub fn len(&self) -> Result<u32, Errno> {
        let (producer, consumer) = self.snapshot()?;
        Ok(producer.wrapping_sub(consumer))
    }

    /// Whether no frame is queued.
    ///
    /// # Errors
    ///
    /// As for [`Self::len`].
    pub fn is_empty(&self) -> Result<bool, Errno> {
        Ok(self.len()? == 0)
    }

    /// Whether every slot is occupied.
    ///
    /// # Errors
    ///
    /// As for [`Self::len`].
    pub fn is_full(&self) -> Result<bool, Errno> {
        Ok(self.len()? == self.slots)
    }

    /// Byte offset of `counter`'s slot within [`Self::slots_region`].
    fn slot_offset(&self, counter: u32) -> usize {
        let index = (counter % self.slots) as usize;
        index * RingGeometry::slot_stride(self.slot_capacity)
    }

    /// Queue one frame with [`FrameOffload::None`] metadata.
    ///
    /// The offload-unaware producer path; the offloaded transmit path
    /// uses [`Self::push_with`].
    ///
    /// # Errors
    ///
    /// * [`Errno::NoSpace`] — every slot is occupied.
    /// * [`Errno::LengthOutOfRange`] — `frame` is empty or longer than
    ///   the slot capacity.
    /// * [`Errno::OutOfRange`] — the persisted counters are corrupt.
    pub fn push(&mut self, frame: &[u8]) -> Result<(), Errno> {
        self.push_with(FrameOffload::None, frame)
    }

    /// Queue one frame together with its per-frame offload metadata.
    ///
    /// # Errors
    ///
    /// As for [`Self::push`].
    pub fn push_with(&mut self, offload: FrameOffload, frame: &[u8]) -> Result<(), Errno> {
        if frame.is_empty() || frame.len() > self.slot_capacity as usize {
            return Err(Errno::LengthOutOfRange);
        }
        let (head, consumer) = self.snapshot()?;
        if head.wrapping_sub(consumer) == self.slots {
            return Err(Errno::NoSpace);
        }
        let offset = self.slot_offset(head);
        let frame_len = u32::try_from(frame.len()).map_err(|_| Errno::LengthOutOfRange)?;
        let (csum_start, csum_offset) = offload.offsets();
        let (gso_size, hdr_len) = offload.gso();
        self.slots_region[offset] = offload.to_tag();
        self.slots_region[offset + META_CSUM_START..offset + META_CSUM_START + 2]
            .copy_from_slice(&csum_start.to_le_bytes());
        self.slots_region[offset + META_CSUM_OFFSET..offset + META_CSUM_OFFSET + 2]
            .copy_from_slice(&csum_offset.to_le_bytes());
        self.slots_region[offset + META_GSO_SIZE..offset + META_GSO_SIZE + 2]
            .copy_from_slice(&gso_size.to_le_bytes());
        self.slots_region[offset + META_HDR_LEN..offset + META_HDR_LEN + 2]
            .copy_from_slice(&hdr_len.to_le_bytes());
        put_u32(self.slots_region, offset + SLOT_META_LEN, frame_len);
        let payload = offset + SLOT_META_LEN + SLOT_LEN_PREFIX;
        self.slots_region[payload..payload + frame.len()].copy_from_slice(frame);
        // Release last: the peer must not observe the counter before the
        // bytes it points at.
        self.publish_producer(head.wrapping_add(1));
        Ok(())
    }

    /// Dequeue the oldest frame into `out`, returning its length, or
    /// `None` when the ring is empty. The frame's offload metadata is
    /// discarded; use [`Self::pop_with`] to read it.
    ///
    /// # Errors
    ///
    /// * [`Errno::BufferTooSmall`] — `out` cannot hold the frame.
    /// * [`Errno::LengthOutOfRange`] — the persisted slot length is
    ///   corrupt (zero or beyond the slot capacity); the slot is
    ///   consumed so one corrupt frame cannot wedge the ring.
    /// * [`Errno::OutOfRange`] — the persisted counters are corrupt.
    pub fn pop(&mut self, out: &mut [u8]) -> Result<Option<usize>, Errno> {
        let mut offload = FrameOffload::None;
        self.pop_with(&mut offload, out)
    }

    /// Dequeue the oldest frame into `out`, writing its offload
    /// metadata into `offload` and returning its length, or `None`
    /// when the ring is empty.
    ///
    /// # Errors
    ///
    /// As for [`Self::pop`].
    pub fn pop_with(
        &mut self,
        offload: &mut FrameOffload,
        out: &mut [u8],
    ) -> Result<Option<usize>, Errno> {
        let (producer, tail) = self.snapshot()?;
        if producer == tail {
            return Ok(None);
        }
        let offset = self.slot_offset(tail);
        let len = read_u32(self.slots_region, offset + SLOT_META_LEN) as usize;
        if len == 0 || len > self.slot_capacity as usize {
            // Consume the corrupt slot before refusing, so the ring
            // recovers rather than replaying the same corruption.
            self.publish_consumer(tail.wrapping_add(1));
            return Err(Errno::LengthOutOfRange);
        }
        if out.len() < len {
            return Err(Errno::BufferTooSmall);
        }
        let tag = self.slots_region[offset];
        let csum_start = u16::from_le_bytes([
            self.slots_region[offset + META_CSUM_START],
            self.slots_region[offset + META_CSUM_START + 1],
        ]);
        let csum_offset = u16::from_le_bytes([
            self.slots_region[offset + META_CSUM_OFFSET],
            self.slots_region[offset + META_CSUM_OFFSET + 1],
        ]);
        let gso_size = u16::from_le_bytes([
            self.slots_region[offset + META_GSO_SIZE],
            self.slots_region[offset + META_GSO_SIZE + 1],
        ]);
        let hdr_len = u16::from_le_bytes([
            self.slots_region[offset + META_HDR_LEN],
            self.slots_region[offset + META_HDR_LEN + 1],
        ]);
        *offload = FrameOffload::decode(tag, csum_start, csum_offset, gso_size, hdr_len);
        let payload = offset + SLOT_META_LEN + SLOT_LEN_PREFIX;
        out[..len].copy_from_slice(&self.slots_region[payload..payload + len]);
        // Release only now: the slot is free for the producer to refill
        // once its bytes have been copied out, not before.
        self.publish_consumer(tail.wrapping_add(1));
        Ok(Some(len))
    }

    /// Consume the oldest frame without copying it, returning its
    /// declared length, or `None` when the ring is empty.
    ///
    /// The dequeue path for a frame the consumer must reject anyway
    /// (e.g. one longer than the device MTU): the slot is released so
    /// the queue behind it keeps flowing.
    ///
    /// # Errors
    ///
    /// * [`Errno::OutOfRange`] — the counters are corrupt.
    /// * [`Errno::LengthOutOfRange`] — the stored slot length is corrupt
    ///   (zero or beyond the slot capacity). The slot is consumed either
    ///   way, so one corrupt frame cannot wedge the ring, and no caller
    ///   is ever handed a length the slot could not hold.
    pub fn skip(&mut self) -> Result<Option<usize>, Errno> {
        let (producer, tail) = self.snapshot()?;
        if producer == tail {
            return Ok(None);
        }
        let len = read_u32(self.slots_region, self.slot_offset(tail) + SLOT_META_LEN) as usize;
        self.publish_consumer(tail.wrapping_add(1));
        if len == 0 || len > self.slot_capacity as usize {
            return Err(Errno::LengthOutOfRange);
        }
        Ok(Some(len))
    }
}

/// The receive and transmit rings one interface's shared region
/// carries, plus the sensitivity class the driver must honour for its
/// internal staging.
///
/// Layout inside the region: [`RingGeometry::rx_queues`] receive rings
/// first (each exactly [`RingGeometry::rx_ring_len`] bytes), the single
/// transmit ring last ([`RingGeometry::tx_ring_len`] bytes). The stack
/// produces into TX and consumes from every RX ring; the driver's
/// [`Net::service`](super::net::Net::service) call does the opposite,
/// steering each received frame into the ring for the receive queue that
/// delivered it. A single-queue device (the common case) has exactly one
/// receive ring at index 0.
#[derive(Debug)]
pub struct FrameRings<'a> {
    /// The receive rings, one per device receive queue; indices
    /// `0..rx_count` are `Some`, the rest `None`. Frames the device
    /// delivered on queue `q` land in `rx[q]`, awaiting the stack.
    rx: [Option<FrameRing<'a>>; MAX_RX_QUEUES as usize],
    /// Number of live receive rings (`geometry.rx_queues()`).
    rx_count: usize,
    /// Frames the stack queued, awaiting the device (single transmit
    /// queue: the stack serialises its own egress).
    pub tx: FrameRing<'a>,
    /// The receive pre-filter, when the driver host installed one. A frame
    /// it refuses is never copied into a receive ring — the whole point is
    /// to spend nothing on traffic with no local consumer.
    admit: Option<&'a dyn RxAdmit>,
    /// Sensitivity class of the traffic: when
    /// [`Sensitive`](super::BufferClass::Sensitive), the driver zeroes
    /// every internal staging copy before its service call returns.
    pub class: super::BufferClass,
}

impl<'a> FrameRings<'a> {
    /// Bind the receive rings + transmit ring over `region` with
    /// `geometry`.
    ///
    /// # Errors
    ///
    /// Returns [`Errno::BufferTooSmall`] when `region` is not exactly
    /// [`RingGeometry::region_len`] bytes.
    pub fn bind(
        region: &'a mut [u8],
        geometry: RingGeometry,
        class: super::BufferClass,
    ) -> Result<Self, Errno> {
        if region.len() != geometry.region_len() {
            return Err(Errno::BufferTooSmall);
        }
        let rx_count = geometry.rx_queues() as usize;
        let rx_ring_len = geometry.rx_ring_len();
        let mut rx: [Option<FrameRing<'a>>; MAX_RX_QUEUES as usize] =
            core::array::from_fn(|_| None);
        // Peel each receive ring off the front of the region in turn.
        // `mem::take` moves the remaining slice out so each split yields a
        // view with the region's own `'a` lifetime (a plain reborrow in a
        // loop would not), leaving the transmit ring as the tail.
        let mut rest: &'a mut [u8] = region;
        for slot in rx.iter_mut().take(rx_count) {
            let current = core::mem::take(&mut rest);
            let (head, tail) = current.split_at_mut(rx_ring_len);
            *slot = Some(FrameRing::bind(
                head,
                geometry.slots(),
                geometry.rx_slot_capacity(),
            )?);
            rest = tail;
        }
        let tx = FrameRing::bind(rest, geometry.slots(), geometry.tx_slot_capacity())?;
        Ok(Self {
            rx,
            rx_count,
            tx,
            admit: None,
            class,
        })
    }

    /// Install the receive pre-filter this binding evaluates before copying
    /// a frame into a receive ring.
    ///
    /// A binding without one admits every frame, which is the correct
    /// default: the filter can only ever remove work, so its absence costs
    /// performance, never correctness.
    #[must_use]
    pub fn with_admit(mut self, admit: &'a dyn RxAdmit) -> Self {
        self.admit = Some(admit);
        self
    }

    /// Offer a received frame to receive ring `queue`, consulting the
    /// pre-filter first.
    ///
    /// The one place a device's received frame enters a ring, so the filter
    /// runs on every driver's harvest path without any of them repeating it,
    /// and a refused frame is never copied at all.
    ///
    /// # Errors
    ///
    /// [`Errno::OutOfRange`] for a queue index the geometry does not have,
    /// or the ring's own typed refusal (a corrupt counter, an over-long
    /// frame). A full ring is [`RxDelivery::RingFull`], not an error: it is
    /// back-pressure, and the frame stays in the device.
    pub fn deliver(
        &mut self,
        queue: usize,
        offload: FrameOffload,
        frame: &[u8],
    ) -> Result<RxDelivery, Errno> {
        if !self.admit.is_none_or(|admit| admit.admit(frame)) {
            return Ok(RxDelivery::Filtered);
        }
        match self.rx_ring(queue)?.push_with(offload, frame) {
            Ok(()) => Ok(RxDelivery::Accepted),
            Err(Errno::NoSpace) => Ok(RxDelivery::RingFull),
            Err(other) => Err(other),
        }
    }

    /// Number of live receive rings (one per device receive queue).
    #[must_use]
    pub const fn rx_queues(&self) -> usize {
        self.rx_count
    }

    /// The receive ring for queue `index`.
    ///
    /// # Errors
    ///
    /// Returns [`Errno::OutOfRange`] when `index` is not a live receive
    /// queue (`>= `[`Self::rx_queues`]) — fail closed, never an
    /// out-of-bounds access.
    pub fn rx_ring(&mut self, index: usize) -> Result<&mut FrameRing<'a>, Errno> {
        if index >= self.rx_count {
            return Err(Errno::OutOfRange);
        }
        self.rx
            .get_mut(index)
            .and_then(Option::as_mut)
            .ok_or(Errno::OutOfRange)
    }
}

/// What one [`Net::service`](super::net::Net::service) call moved.
///
/// `rx_ring_full` reports that the device still holds frames the RX
/// ring could not take: nothing was dropped — the stack drains the
/// ring and issues another service call.
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
pub struct ServiceReport {
    /// Frames moved from the TX ring into the device.
    pub transmitted: u32,
    /// Frames moved from the device into the RX ring.
    pub received: u32,
    /// Frames the device handed over in **this pass**, whether or not they
    /// reached the ring — [`Self::received`] plus those the pre-filter shed.
    ///
    /// This, not `received`, is what tells a drain the device is still
    /// producing. A pass that harvested frames and shed every one of them
    /// has made progress and may find more; reading `received` alone there
    /// says "quiet", so the drain would re-arm and take a fresh interrupt
    /// for each frame of a burst instead of absorbing the burst masked.
    pub harvested: u32,
    /// Frames this device's receive pre-filter has shed **since it was
    /// opened** — no local consumer was possible, so the stack was never
    /// woken for them.
    ///
    /// Cumulative, unlike the two counts above, and deliberately: a driver
    /// drains its rings on its own interrupt and the stack does not
    /// doorbell for every batch, so a per-call delta would simply be lost.
    /// A consumer stores the latest value it saw; it is monotonic, so it
    /// can neither double-count nor go backwards.
    pub filtered: u64,
    /// The RX ring filled while the device still had frames pending.
    pub rx_ring_full: bool,
    /// The device's live link state at the moment of this service call.
    ///
    /// Reported on every doorbell so a link change a device signals
    /// out of band (a virtio config-change interrupt, a PHY carrier
    /// loss) reaches the stack the next time it services the ring —
    /// which the driver provokes by waking the stack on the interrupt.
    /// The stack turns a change into a bond-member link report
    /// (`set_member_link`), the sole live source of a bond failover.
    pub link: super::net::LinkState,
}

impl ServiceReport {
    /// Record one frame the device handed over that reached the ring.
    pub fn record_delivered(&mut self) {
        self.received += 1;
        self.harvested += 1;
    }

    /// Record one frame the device handed over that did **not** reach the
    /// ring — the pre-filter shed it, or the device itself flagged it.
    ///
    /// Still progress: the device produced a frame, which is what a drain
    /// needs to know to keep going. The two recorders exist so
    /// [`Self::harvested`] cannot fall out of step with
    /// [`Self::received`] — a driver that bumped one and forgot the other
    /// would silently cost an interrupt per frame.
    pub fn record_undelivered(&mut self) {
        self.harvested += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Test ring dimensions: 4 slots of 128 bytes.
    const SLOTS: u32 = 4;
    const CAP: u32 = 128;
    const RING_LEN: usize = RingGeometry::ring_len_for(SLOTS, CAP);

    /// Test geometry with equal RX/TX capacities and one receive queue.
    fn geometry() -> RingGeometry {
        RingGeometry::new(SLOTS, CAP, CAP, 1).expect("valid test geometry")
    }

    /// A zeroed backing buffer for one aligned ring region.
    ///
    /// A stack array is only byte-aligned, so every test cuts its region
    /// out of an over-allocated buffer exactly as an in-process caller
    /// does — which also keeps `aligned_region` on the tested path.
    const fn ring_buf<const RINGS: usize>() -> [u8; RINGS] {
        [0u8; RINGS]
    }

    /// Bind a single ring over an aligned region cut from `buffer`.
    fn ring(buffer: &mut [u8]) -> FrameRing<'_> {
        let region = aligned_region(buffer, RING_LEN).expect("aligned region");
        FrameRing::bind(region, SLOTS, CAP).expect("bind")
    }

    /// Bind a whole ring set over an aligned region cut from `buffer`.
    fn rings(buffer: &mut [u8], g: RingGeometry) -> FrameRings<'_> {
        let region = aligned_region(buffer, g.region_len()).expect("aligned region");
        FrameRings::bind(region, g, crate::driver::BufferClass::NonSensitive).expect("bind")
    }

    #[test]
    fn geometry_bounds_fail_closed() {
        let min = RingGeometry::MIN_SLOT_CAPACITY;
        assert!(RingGeometry::new(2, min, min, 1).is_ok());
        assert_eq!(RingGeometry::new(1, 128, 128, 1), Err(Errno::OutOfRange));
        assert_eq!(
            RingGeometry::new(RingGeometry::MAX_SLOTS + 1, 128, 128, 1),
            Err(Errno::OutOfRange)
        );
        assert_eq!(
            RingGeometry::new(4, min - 1, 128, 1),
            Err(Errno::OutOfRange)
        );
        assert_eq!(
            RingGeometry::new(4, 128, RingGeometry::MAX_SLOT_CAPACITY + 1, 1),
            Err(Errno::OutOfRange)
        );
        // Receive-queue count is bounded 1..=MAX_RX_QUEUES, fail closed.
        assert_eq!(RingGeometry::new(4, 128, 128, 0), Err(Errno::OutOfRange));
        assert_eq!(
            RingGeometry::new(4, 128, 128, MAX_RX_QUEUES + 1),
            Err(Errno::OutOfRange)
        );
        assert!(RingGeometry::new(4, 128, 128, MAX_RX_QUEUES).is_ok());
    }

    #[test]
    fn multi_queue_region_sizes_one_rx_ring_per_queue() {
        let single = RingGeometry::new(SLOTS, CAP, CAP, 1).expect("single");
        let quad = RingGeometry::new(SLOTS, CAP, CAP, 4).expect("quad");
        assert_eq!(quad.rx_queues(), 4);
        // Four receive rings + one transmit ring vs one + one.
        assert_eq!(
            quad.region_len(),
            4 * quad.rx_ring_len() + quad.tx_ring_len()
        );
        assert_eq!(
            quad.region_len() - single.region_len(),
            3 * single.rx_ring_len()
        );
    }

    #[test]
    fn multi_queue_rings_are_independent_and_index_checked() {
        let g = RingGeometry::new(SLOTS, CAP, CAP, 3).expect("triple");
        // Three receive rings + one transmit ring, each RING_LEN bytes.
        assert_eq!(4 * RING_LEN, g.region_len());
        let mut buffer = ring_buf::<{ 4 * RING_LEN + REGION_ALIGN_PADDING }>();
        let mut rings = rings(&mut buffer, g);
        assert_eq!(rings.rx_queues(), 3);
        // Each receive ring is a distinct queue: a frame pushed onto queue
        // 2 is not visible on queue 0 or 1.
        rings
            .rx_ring(2)
            .expect("rx2")
            .push(&[0xAB; 50])
            .expect("push q2");
        assert_eq!(rings.rx_ring(0).expect("rx0").is_empty(), Ok(true));
        assert_eq!(rings.rx_ring(1).expect("rx1").is_empty(), Ok(true));
        let mut out = [0u8; 128];
        assert_eq!(rings.rx_ring(2).expect("rx2").pop(&mut out), Ok(Some(50)));
        assert_eq!(out[0], 0xAB);
        // An out-of-range queue index fails closed.
        assert_eq!(rings.rx_ring(3).map(|_| ()), Err(Errno::OutOfRange));
    }

    #[test]
    fn per_direction_capacities_size_each_ring_independently() {
        // A larger transmit capacity (a GSO super-frame's home) does not
        // enlarge the receive ring.
        const RX_CAP: u32 = RingGeometry::MIN_SLOT_CAPACITY;
        const TX_CAP: u32 = 2048;
        const REGION_LEN: usize =
            RingGeometry::ring_len_for(4, RX_CAP) + RingGeometry::ring_len_for(4, TX_CAP);
        let g = RingGeometry::new(4, RX_CAP, TX_CAP, 1).expect("valid");
        assert_eq!(g.rx_slot_capacity(), RX_CAP);
        assert_eq!(g.tx_slot_capacity(), TX_CAP);
        assert!(g.tx_ring_len() > g.rx_ring_len());
        assert_eq!(g.region_len(), g.rx_ring_len() + g.tx_ring_len());
        assert_eq!(g.region_len(), REGION_LEN);
        // The pair binds and each direction honours its own capacity.
        let mut buffer = ring_buf::<{ REGION_LEN + REGION_ALIGN_PADDING }>();
        let mut rings = rings(&mut buffer, g);
        assert_eq!(rings.rx_ring(0).expect("rx0").slot_capacity(), RX_CAP);
        assert_eq!(rings.tx.slot_capacity(), TX_CAP);
        // The transmit ring accepts a frame the (smaller) receive ring
        // could never hold.
        let big = [0xABu8; RX_CAP as usize + 1];
        rings
            .tx
            .push(&big)
            .expect("tx accepts an over-rx-cap frame");
    }

    #[test]
    fn bind_requires_exact_region_length() {
        let mut buffer = ring_buf::<{ RING_LEN + 1 + REGION_ALIGN_PADDING }>();
        let long = aligned_region(&mut buffer, RING_LEN + 1).expect("aligned");
        assert_eq!(
            FrameRing::bind(long, SLOTS, CAP).map(|_| ()),
            Err(Errno::BufferTooSmall)
        );
        let mut buffer = ring_buf::<{ RING_LEN + REGION_ALIGN_PADDING }>();
        let exact = aligned_region(&mut buffer, RING_LEN).expect("aligned");
        assert!(FrameRing::bind(exact, SLOTS, CAP).is_ok());
    }

    #[test]
    fn bind_refuses_a_region_the_counters_are_not_aligned_for() {
        // The header's counters are atomics, so a misaligned region is
        // refused rather than accessed unsoundly. Start one byte past the
        // aligned base with the exact length, so alignment is the only
        // thing wrong.
        let mut buffer = ring_buf::<{ RING_LEN + INDEX_ALIGN + REGION_ALIGN_PADDING }>();
        let start = buffer.as_ptr().align_offset(INDEX_ALIGN) + 1;
        let misaligned = &mut buffer[start..start + RING_LEN];
        assert_eq!(
            FrameRing::bind(misaligned, SLOTS, CAP).map(|_| ()),
            Err(Errno::BadAlignment)
        );
    }

    // In one cache line every publish would invalidate the peer's read of
    // the other counter, and the two CPUs would ping-pong the line for
    // every frame — so the layout is checked at compile time.
    const _: () = assert!(HEADER_CONSUMER - HEADER_PRODUCER >= CACHE_LINE_BYTES);
    const _: () = assert!(RING_HEADER_LEN >= HEADER_CONSUMER + INDEX_ALIGN);

    #[test]
    fn every_ring_length_keeps_the_next_ring_header_aligned() {
        // A ring set lays its rings end to end, so a ring length that were
        // not a multiple of the counters' alignment would misalign every
        // ring after the first.
        for capacity in [
            RingGeometry::MIN_SLOT_CAPACITY,
            RingGeometry::MIN_SLOT_CAPACITY + 1,
            CAP,
            1499,
            1500,
            RingGeometry::MAX_SLOT_CAPACITY,
        ] {
            for slots in [RingGeometry::MIN_SLOTS, 3, SLOTS, 17] {
                let len = RingGeometry::ring_len_for(slots, capacity);
                assert_eq!(
                    len % INDEX_ALIGN,
                    0,
                    "ring_len_for({slots}, {capacity}) misaligns the next ring"
                );
            }
        }
    }

    #[test]
    fn a_peer_that_corrupts_its_counter_mid_operation_cannot_widen_a_read() {
        let mut buffer = ring_buf::<{ RING_LEN + REGION_ALIGN_PADDING }>();
        let mut ring = ring(&mut buffer);
        ring.push(&[3u8; 20]).expect("push");
        // A producer counter far beyond the ring's capacity is the shape a
        // hostile peer would write to make the consumer walk past the slot
        // area. Occupancy is validated first, so the read never happens.
        ring.producer.store(u32::MAX, Ordering::Relaxed);
        let mut out = [0u8; 128];
        assert_eq!(ring.pop(&mut out), Err(Errno::OutOfRange));
        assert_eq!(ring.skip(), Err(Errno::OutOfRange));
        assert_eq!(ring.len(), Err(Errno::OutOfRange));
    }

    #[test]
    fn skip_refuses_a_corrupt_length_rather_than_reporting_it() {
        let mut buffer = ring_buf::<{ RING_LEN + REGION_ALIGN_PADDING }>();
        let mut ring = ring(&mut buffer);
        ring.push(&[4u8; 20]).expect("push");
        put_u32(ring.slots_region, SLOT_META_LEN, 4096);
        // The slot is consumed either way, so one corrupt frame cannot
        // wedge the ring — but the caller is never handed a length the
        // slot could not have held.
        assert_eq!(ring.skip(), Err(Errno::LengthOutOfRange));
        assert_eq!(ring.skip(), Ok(None));
        ring.push(&[5u8; 20]).expect("push after corruption");
        assert_eq!(ring.skip(), Ok(Some(20)));
    }

    #[test]
    fn rings_pair_binds_and_keeps_directions_apart() {
        let g = geometry();
        let mut buffer = ring_buf::<{ 2 * RING_LEN + REGION_ALIGN_PADDING }>();
        let mut rings = rings(&mut buffer, g);
        rings.tx.push(&[9u8; 40]).expect("tx push");
        assert_eq!(rings.rx_ring(0).expect("rx0").is_empty(), Ok(true));
        let mut out = [0u8; 128];
        assert_eq!(rings.tx.pop(&mut out), Ok(Some(40)));
        // A short pair region is refused whole.
        let mut buffer = ring_buf::<{ 2 * RING_LEN - 1 + REGION_ALIGN_PADDING }>();
        let short = aligned_region(&mut buffer, 2 * RING_LEN - 1).expect("aligned");
        assert!(FrameRings::bind(short, g, crate::driver::BufferClass::NonSensitive).is_err());
    }

    #[test]
    fn push_pop_round_trips_in_order() {
        let mut buffer = ring_buf::<{ RING_LEN + REGION_ALIGN_PADDING }>();
        let mut ring = ring(&mut buffer);
        ring.push(&[1u8; 60]).expect("push a");
        ring.push(&[2u8; 100]).expect("push b");
        let mut out = [0u8; 128];
        assert_eq!(ring.pop(&mut out), Ok(Some(60)));
        assert!(out[..60].iter().all(|&b| b == 1));
        assert_eq!(ring.pop(&mut out), Ok(Some(100)));
        assert!(out[..100].iter().all(|&b| b == 2));
        assert_eq!(ring.pop(&mut out), Ok(None));
    }

    #[test]
    fn push_fails_closed_when_full_and_pop_recovers() {
        let mut buffer = ring_buf::<{ RING_LEN + REGION_ALIGN_PADDING }>();
        let mut ring = ring(&mut buffer);
        for _ in 0..4 {
            ring.push(&[7u8; 20]).expect("push");
        }
        assert_eq!(ring.push(&[7u8; 20]), Err(Errno::NoSpace));
        let mut out = [0u8; 128];
        assert_eq!(ring.pop(&mut out), Ok(Some(20)));
        ring.push(&[8u8; 20]).expect("push after pop");
    }

    #[test]
    fn wrapping_counters_survive_many_cycles() {
        let mut buffer = ring_buf::<{ RING_LEN + REGION_ALIGN_PADDING }>();
        let mut ring = ring(&mut buffer);
        // Pre-wrap the counters near u32::MAX to prove wrapping math.
        ring.producer.store(u32::MAX - 1, Ordering::Relaxed);
        ring.consumer.store(u32::MAX - 1, Ordering::Relaxed);
        let mut out = [0u8; 128];
        for round in 0..8u8 {
            ring.push(&[round; 30]).expect("push");
            assert_eq!(ring.pop(&mut out), Ok(Some(30)));
            assert_eq!(out[0], round);
        }
    }

    #[test]
    fn oversize_and_empty_frames_are_refused() {
        let mut buffer = ring_buf::<{ RING_LEN + REGION_ALIGN_PADDING }>();
        let mut ring = ring(&mut buffer);
        assert_eq!(ring.push(&[]), Err(Errno::LengthOutOfRange));
        assert_eq!(ring.push(&[0u8; 129]), Err(Errno::LengthOutOfRange));
    }

    #[test]
    fn corrupt_counters_fail_closed() {
        let mut buffer = ring_buf::<{ RING_LEN + REGION_ALIGN_PADDING }>();
        let mut ring = ring(&mut buffer);
        // What a hostile or buggy peer would write.
        ring.producer.store(10, Ordering::Relaxed);
        ring.consumer.store(0, Ordering::Relaxed);
        assert_eq!(ring.len(), Err(Errno::OutOfRange));
        assert_eq!(ring.push(&[1u8; 20]), Err(Errno::OutOfRange));
        let mut out = [0u8; 128];
        assert_eq!(ring.pop(&mut out), Err(Errno::OutOfRange));
    }

    #[test]
    fn corrupt_slot_length_is_consumed_and_refused() {
        let mut buffer = ring_buf::<{ RING_LEN + REGION_ALIGN_PADDING }>();
        let mut ring = ring(&mut buffer);
        ring.push(&[1u8; 20]).expect("push");
        // Corrupt the queued slot's length prefix (past the offload-
        // metadata prefix) beyond capacity.
        put_u32(ring.slots_region, SLOT_META_LEN, 4096);
        let mut out = [0u8; 128];
        assert_eq!(ring.pop(&mut out), Err(Errno::LengthOutOfRange));
        // The corrupt slot was consumed: the ring is usable again.
        assert_eq!(ring.pop(&mut out), Ok(None));
        ring.push(&[2u8; 20]).expect("push after corruption");
        assert_eq!(ring.pop(&mut out), Ok(Some(20)));
    }

    #[test]
    fn skip_consumes_without_copying() {
        let mut buffer = ring_buf::<{ RING_LEN + REGION_ALIGN_PADDING }>();
        let mut ring = ring(&mut buffer);
        assert_eq!(ring.skip(), Ok(None));
        ring.push(&[5u8; 90]).expect("push");
        ring.push(&[6u8; 30]).expect("push");
        assert_eq!(ring.skip(), Ok(Some(90)));
        let mut out = [0u8; 128];
        assert_eq!(ring.pop(&mut out), Ok(Some(30)));
        assert_eq!(out[0], 6);
    }

    #[test]
    fn pop_into_short_buffer_fails_without_consuming() {
        let mut buffer = ring_buf::<{ RING_LEN + REGION_ALIGN_PADDING }>();
        let mut ring = ring(&mut buffer);
        ring.push(&[3u8; 100]).expect("push");
        let mut short = [0u8; 50];
        assert_eq!(ring.pop(&mut short), Err(Errno::BufferTooSmall));
        let mut out = [0u8; 128];
        assert_eq!(ring.pop(&mut out), Ok(Some(100)));
    }

    #[test]
    fn offload_metadata_round_trips_per_frame() {
        let mut buffer = ring_buf::<{ RING_LEN + REGION_ALIGN_PADDING }>();
        let mut ring = ring(&mut buffer);
        ring.push_with(FrameOffload::Validated, &[1u8; 40])
            .expect("push validated");
        ring.push_with(
            FrameOffload::NeedsChecksum {
                csum_start: 34,
                csum_offset: 16,
            },
            &[2u8; 40],
        )
        .expect("push needs-checksum");
        // A plain push carries no offload.
        ring.push(&[3u8; 40]).expect("push plain");
        let mut out = [0u8; 128];
        let mut offload = FrameOffload::None;
        assert_eq!(ring.pop_with(&mut offload, &mut out), Ok(Some(40)));
        assert_eq!(offload, FrameOffload::Validated);
        assert_eq!(ring.pop_with(&mut offload, &mut out), Ok(Some(40)));
        assert_eq!(
            offload,
            FrameOffload::NeedsChecksum {
                csum_start: 34,
                csum_offset: 16,
            }
        );
        assert_eq!(ring.pop_with(&mut offload, &mut out), Ok(Some(40)));
        assert_eq!(offload, FrameOffload::None);
    }

    #[test]
    fn tx_checksum_offload_round_trips_per_frame() {
        let mut buffer = ring_buf::<{ RING_LEN + REGION_ALIGN_PADDING }>();
        let mut ring = ring(&mut buffer);
        ring.push_with(
            FrameOffload::TxChecksum {
                csum_start: 34,
                csum_offset: 16,
            },
            &[7u8; 40],
        )
        .expect("push tx-checksum");
        let mut out = [0u8; 128];
        let mut offload = FrameOffload::None;
        assert_eq!(ring.pop_with(&mut offload, &mut out), Ok(Some(40)));
        assert_eq!(
            offload,
            FrameOffload::TxChecksum {
                csum_start: 34,
                csum_offset: 16,
            }
        );
    }

    #[test]
    fn tx_segment_offload_round_trips_per_frame() {
        let mut buffer = ring_buf::<{ RING_LEN + REGION_ALIGN_PADDING }>();
        let mut ring = ring(&mut buffer);
        for ipv6 in [false, true] {
            let seg = FrameOffload::TxSegment {
                csum_start: 34,
                csum_offset: 16,
                gso_size: 1440,
                hdr_len: 54,
                ipv6,
            };
            ring.push_with(seg, &[7u8; 80]).expect("push tx-segment");
            let mut out = [0u8; 128];
            let mut offload = FrameOffload::None;
            assert_eq!(ring.pop_with(&mut offload, &mut out), Ok(Some(80)));
            assert_eq!(offload, seg);
        }
    }

    #[test]
    fn unknown_offload_tag_decodes_to_none_fail_closed() {
        let mut buffer = ring_buf::<{ RING_LEN + REGION_ALIGN_PADDING }>();
        let mut ring = ring(&mut buffer);
        ring.push_with(FrameOffload::Validated, &[9u8; 30])
            .expect("push");
        // Corrupt the offload tag byte (slot start) to an unknown value.
        ring.slots_region[0] = 0xFF;
        let mut out = [0u8; 128];
        let mut offload = FrameOffload::Validated;
        assert_eq!(ring.pop_with(&mut offload, &mut out), Ok(Some(30)));
        // Fail closed: an unknown tag loses the offload, never invents one.
        assert_eq!(offload, FrameOffload::None);
    }
}
