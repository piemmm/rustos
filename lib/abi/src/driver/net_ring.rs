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
//! # Ownership handoff, not shared mutation
//!
//! The rings are deliberately *not* concurrently shared: every ring
//! mutation by the driver happens inside one blocking
//! [`Net::service`](super::net::Net::service) call, and the stack
//! touches the region only while no such call is in flight. The
//! synchronisation is the call boundary itself, so the whole transport
//! is safe Rust with no shared-memory atomics to get wrong. A driver
//! woken by its interrupt does not write the ring; it notifies the
//! stack, which issues the next service call.
//!
//! # Fail closed
//!
//! Both sides treat the region as untrusted input: geometry is agreed
//! out-of-band and validated at construction, indices and per-slot
//! lengths read back from the region are re-validated on every
//! operation, and any corrupt state is a typed error — never an
//! out-of-bounds access, never a guess.

use crate::le::{put_u32, read_u32};
use crate::Errno;

/// Byte length of one ring's control header: the free-running
/// producer and consumer counters (4 bytes each, little-endian).
pub const RING_HEADER_LEN: usize = 8;

/// Byte length of one slot's length prefix.
const SLOT_LEN_PREFIX: usize = 4;

/// Validated geometry of one frame ring: how many slots it holds and
/// the largest frame one slot carries.
///
/// Geometry is agreed out-of-band (both sides derive it from the same
/// [`DeviceFacts`](super::net::DeviceFacts)) and validated fail-closed
/// here, so a corrupt or hostile geometry can never size an
/// out-of-bounds view over the shared region.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct RingGeometry {
    slots: u32,
    slot_capacity: u32,
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
    /// Ethernet header.
    pub const MAX_SLOT_CAPACITY: u32 = 65_535 + super::net::ETHERNET_HEADER_LEN;

    /// Validate and build a geometry.
    ///
    /// # Errors
    ///
    /// Returns [`Errno::OutOfRange`] when `slots` lies outside
    /// [`Self::MIN_SLOTS`]`..=`[`Self::MAX_SLOTS`] or `slot_capacity`
    /// lies outside
    /// [`Self::MIN_SLOT_CAPACITY`]`..=`[`Self::MAX_SLOT_CAPACITY`].
    pub const fn new(slots: u32, slot_capacity: u32) -> Result<Self, Errno> {
        if slots < Self::MIN_SLOTS || slots > Self::MAX_SLOTS {
            return Err(Errno::OutOfRange);
        }
        if slot_capacity < Self::MIN_SLOT_CAPACITY || slot_capacity > Self::MAX_SLOT_CAPACITY {
            return Err(Errno::OutOfRange);
        }
        Ok(Self {
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

    /// Bytes one slot occupies: its length prefix plus its payload
    /// capacity.
    #[must_use]
    pub const fn slot_stride(&self) -> usize {
        SLOT_LEN_PREFIX + self.slot_capacity as usize
    }

    /// Bytes one whole ring occupies: header plus every slot.
    #[must_use]
    pub const fn ring_len(&self) -> usize {
        RING_HEADER_LEN + self.slots as usize * self.slot_stride()
    }

    /// Bytes a whole [`FrameRings`] region occupies: the RX ring
    /// followed by the TX ring.
    #[must_use]
    pub const fn region_len(&self) -> usize {
        2 * self.ring_len()
    }
}

/// One direction's bounded frame queue inside the shared region.
///
/// The producer and consumer counters are free-running (wrapping)
/// `u32`s persisted little-endian in the ring header; occupancy is
/// their wrapping difference and is re-validated against the slot
/// count on every operation, so a corrupt header is refused rather
/// than driving an out-of-bounds slot index.
#[derive(Debug)]
pub struct FrameRing<'a> {
    region: &'a mut [u8],
    geometry: RingGeometry,
}

impl<'a> FrameRing<'a> {
    /// Bind a ring view over `region` with `geometry`.
    ///
    /// # Errors
    ///
    /// Returns [`Errno::BufferTooSmall`] when `region` is not exactly
    /// [`RingGeometry::ring_len`] bytes.
    pub fn bind(region: &'a mut [u8], geometry: RingGeometry) -> Result<Self, Errno> {
        if region.len() != geometry.ring_len() {
            return Err(Errno::BufferTooSmall);
        }
        Ok(Self { region, geometry })
    }

    /// The ring's validated geometry.
    #[must_use]
    pub const fn geometry(&self) -> &RingGeometry {
        &self.geometry
    }

    fn producer(&self) -> u32 {
        read_u32(self.region, 0)
    }

    fn consumer(&self) -> u32 {
        read_u32(self.region, 4)
    }

    fn set_producer(&mut self, value: u32) {
        put_u32(self.region, 0, value);
    }

    fn set_consumer(&mut self, value: u32) {
        put_u32(self.region, 4, value);
    }

    /// Frames currently queued.
    ///
    /// # Errors
    ///
    /// Returns [`Errno::OutOfRange`] when the persisted counters are
    /// corrupt (occupancy exceeds the slot count).
    pub fn len(&self) -> Result<u32, Errno> {
        let occupancy = self.producer().wrapping_sub(self.consumer());
        if occupancy > self.geometry.slots {
            return Err(Errno::OutOfRange);
        }
        Ok(occupancy)
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
        Ok(self.len()? == self.geometry.slots)
    }

    fn slot_offset(&self, counter: u32) -> usize {
        let index = (counter % self.geometry.slots) as usize;
        RING_HEADER_LEN + index * self.geometry.slot_stride()
    }

    /// Queue one frame.
    ///
    /// # Errors
    ///
    /// * [`Errno::NoSpace`] — every slot is occupied.
    /// * [`Errno::LengthOutOfRange`] — `frame` is empty or longer than
    ///   the slot capacity.
    /// * [`Errno::OutOfRange`] — the persisted counters are corrupt.
    pub fn push(&mut self, frame: &[u8]) -> Result<(), Errno> {
        if frame.is_empty() || frame.len() > self.geometry.slot_capacity as usize {
            return Err(Errno::LengthOutOfRange);
        }
        if self.is_full()? {
            return Err(Errno::NoSpace);
        }
        let head = self.producer();
        let offset = self.slot_offset(head);
        let frame_len = u32::try_from(frame.len()).map_err(|_| Errno::LengthOutOfRange)?;
        put_u32(self.region, offset, frame_len);
        let payload = offset + SLOT_LEN_PREFIX;
        self.region[payload..payload + frame.len()].copy_from_slice(frame);
        self.set_producer(head.wrapping_add(1));
        Ok(())
    }

    /// Dequeue the oldest frame into `out`, returning its length, or
    /// `None` when the ring is empty.
    ///
    /// # Errors
    ///
    /// * [`Errno::BufferTooSmall`] — `out` cannot hold the frame.
    /// * [`Errno::LengthOutOfRange`] — the persisted slot length is
    ///   corrupt (zero or beyond the slot capacity); the slot is
    ///   consumed so one corrupt frame cannot wedge the ring.
    /// * [`Errno::OutOfRange`] — the persisted counters are corrupt.
    pub fn pop(&mut self, out: &mut [u8]) -> Result<Option<usize>, Errno> {
        if self.is_empty()? {
            return Ok(None);
        }
        let tail = self.consumer();
        let offset = self.slot_offset(tail);
        let len = read_u32(self.region, offset) as usize;
        if len == 0 || len > self.geometry.slot_capacity as usize {
            // Consume the corrupt slot before refusing, so the ring
            // recovers rather than replaying the same corruption.
            self.set_consumer(tail.wrapping_add(1));
            return Err(Errno::LengthOutOfRange);
        }
        if out.len() < len {
            return Err(Errno::BufferTooSmall);
        }
        let payload = offset + SLOT_LEN_PREFIX;
        out[..len].copy_from_slice(&self.region[payload..payload + len]);
        self.set_consumer(tail.wrapping_add(1));
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
    /// [`Errno::OutOfRange`] — the persisted counters are corrupt.
    pub fn skip(&mut self) -> Result<Option<usize>, Errno> {
        if self.is_empty()? {
            return Ok(None);
        }
        let tail = self.consumer();
        let len = read_u32(self.region, self.slot_offset(tail)) as usize;
        self.set_consumer(tail.wrapping_add(1));
        Ok(Some(len))
    }
}

/// The paired receive and transmit rings one interface's shared
/// region carries, plus the sensitivity class the driver must honour
/// for its internal staging.
///
/// Layout inside the region: the RX ring first, the TX ring second,
/// each exactly [`RingGeometry::ring_len`] bytes. The stack produces
/// into TX and consumes from RX; the driver's
/// [`Net::service`](super::net::Net::service) call does the opposite.
#[derive(Debug)]
pub struct FrameRings<'a> {
    /// Frames the device delivered, awaiting the stack.
    pub rx: FrameRing<'a>,
    /// Frames the stack queued, awaiting the device.
    pub tx: FrameRing<'a>,
    /// Sensitivity class of the traffic: when
    /// [`Sensitive`](super::BufferClass::Sensitive), the driver zeroes
    /// every internal staging copy before its service call returns.
    pub class: super::BufferClass,
}

impl<'a> FrameRings<'a> {
    /// Bind the RX + TX ring pair over `region` with `geometry`.
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
        let (rx_region, tx_region) = region.split_at_mut(geometry.ring_len());
        Ok(Self {
            rx: FrameRing::bind(rx_region, geometry)?,
            tx: FrameRing::bind(tx_region, geometry)?,
            class,
        })
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
    /// The RX ring filled while the device still had frames pending.
    pub rx_ring_full: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Test geometry: 4 slots of 128 bytes.
    const GEOMETRY: RingGeometry = match RingGeometry::new(4, 128) {
        Ok(g) => g,
        Err(_) => panic!("valid test geometry"),
    };
    const RING_LEN: usize = GEOMETRY.ring_len();

    fn geometry() -> RingGeometry {
        GEOMETRY
    }

    #[test]
    fn geometry_bounds_fail_closed() {
        assert!(RingGeometry::new(2, RingGeometry::MIN_SLOT_CAPACITY).is_ok());
        assert_eq!(RingGeometry::new(1, 128), Err(Errno::OutOfRange));
        assert_eq!(
            RingGeometry::new(RingGeometry::MAX_SLOTS + 1, 128),
            Err(Errno::OutOfRange)
        );
        assert_eq!(
            RingGeometry::new(4, RingGeometry::MIN_SLOT_CAPACITY - 1),
            Err(Errno::OutOfRange)
        );
        assert_eq!(
            RingGeometry::new(4, RingGeometry::MAX_SLOT_CAPACITY + 1),
            Err(Errno::OutOfRange)
        );
    }

    #[test]
    fn bind_requires_exact_region_length() {
        let g = geometry();
        let mut region = [0u8; RING_LEN + 1];
        assert!(FrameRing::bind(&mut region, g).is_err());
        let mut region = [0u8; RING_LEN];
        assert!(FrameRing::bind(&mut region, g).is_ok());
    }

    #[test]
    fn rings_pair_binds_and_keeps_directions_apart() {
        let g = geometry();
        let mut region = [0u8; 2 * RING_LEN];
        let mut rings = FrameRings::bind(&mut region, g, crate::driver::BufferClass::NonSensitive)
            .expect("bind pair");
        rings.tx.push(&[9u8; 40]).expect("tx push");
        assert_eq!(rings.rx.is_empty(), Ok(true));
        let mut out = [0u8; 128];
        assert_eq!(rings.tx.pop(&mut out), Ok(Some(40)));
        // A short pair region is refused whole.
        let mut short = [0u8; 2 * RING_LEN - 1];
        assert!(FrameRings::bind(&mut short, g, crate::driver::BufferClass::NonSensitive).is_err());
    }

    #[test]
    fn push_pop_round_trips_in_order() {
        let g = geometry();
        let mut region = [0u8; RING_LEN];
        let mut ring = FrameRing::bind(&mut region, g).expect("bind");
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
        let g = geometry();
        let mut region = [0u8; RING_LEN];
        let mut ring = FrameRing::bind(&mut region, g).expect("bind");
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
        let g = geometry();
        let mut region = [0u8; RING_LEN];
        let mut ring = FrameRing::bind(&mut region, g).expect("bind");
        // Pre-wrap the counters near u32::MAX to prove wrapping math.
        ring.set_producer(u32::MAX - 1);
        ring.set_consumer(u32::MAX - 1);
        let mut out = [0u8; 128];
        for round in 0..8u8 {
            ring.push(&[round; 30]).expect("push");
            assert_eq!(ring.pop(&mut out), Ok(Some(30)));
            assert_eq!(out[0], round);
        }
    }

    #[test]
    fn oversize_and_empty_frames_are_refused() {
        let g = geometry();
        let mut region = [0u8; RING_LEN];
        let mut ring = FrameRing::bind(&mut region, g).expect("bind");
        assert_eq!(ring.push(&[]), Err(Errno::LengthOutOfRange));
        assert_eq!(ring.push(&[0u8; 129]), Err(Errno::LengthOutOfRange));
    }

    #[test]
    fn corrupt_counters_fail_closed() {
        let g = geometry();
        let mut region = [0u8; RING_LEN];
        let mut ring = FrameRing::bind(&mut region, g).expect("bind");
        ring.set_producer(10);
        ring.set_consumer(0);
        assert_eq!(ring.len(), Err(Errno::OutOfRange));
        assert_eq!(ring.push(&[1u8; 20]), Err(Errno::OutOfRange));
        let mut out = [0u8; 128];
        assert_eq!(ring.pop(&mut out), Err(Errno::OutOfRange));
    }

    #[test]
    fn corrupt_slot_length_is_consumed_and_refused() {
        let g = geometry();
        let mut region = [0u8; RING_LEN];
        let mut ring = FrameRing::bind(&mut region, g).expect("bind");
        ring.push(&[1u8; 20]).expect("push");
        // Corrupt the queued slot's length prefix beyond capacity.
        put_u32(ring.region, RING_HEADER_LEN, 4096);
        let mut out = [0u8; 128];
        assert_eq!(ring.pop(&mut out), Err(Errno::LengthOutOfRange));
        // The corrupt slot was consumed: the ring is usable again.
        assert_eq!(ring.pop(&mut out), Ok(None));
        ring.push(&[2u8; 20]).expect("push after corruption");
        assert_eq!(ring.pop(&mut out), Ok(Some(20)));
    }

    #[test]
    fn skip_consumes_without_copying() {
        let g = geometry();
        let mut region = [0u8; RING_LEN];
        let mut ring = FrameRing::bind(&mut region, g).expect("bind");
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
        let g = geometry();
        let mut region = [0u8; RING_LEN];
        let mut ring = FrameRing::bind(&mut region, g).expect("bind");
        ring.push(&[3u8; 100]).expect("push");
        let mut short = [0u8; 50];
        assert_eq!(ring.pop(&mut short), Err(Errno::BufferTooSmall));
        let mut out = [0u8; 128];
        assert_eq!(ring.pop(&mut out), Ok(Some(100)));
    }
}
