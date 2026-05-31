//! Packed virtqueue management (virtio 1.1 §2.7).
//!
//! [`PackedQueue`] is the packed-ring sibling of
//! [`crate::SplitQueue`]. Where the split layout uses three separate
//! structures (descriptor table, avail ring, used ring), the packed
//! layout collapses them into a single descriptor ring plus two small
//! event-suppression structures. Availability and completion are
//! signalled in-band through each descriptor's `AVAIL`/`USED` flag
//! bits, toggled against single-bit wrap counters (§2.7.1).
//!
//! This is a deliberate parallel implementation of the virtqueue
//! contract, not a duplication of [`crate::SplitQueue`]: the two ring
//! formats are distinct wire protocols a device advertises through the
//! `VIRTIO_F_RING_PACKED` feature bit (`AGENTS.md` §2.2 carve-out for
//! parallel implementations of the same contract).

use crate::dma::DmaSlab;
use crate::host::VirtioHost;
use crate::transport::{Direction, Transport, VirtioError};
use rustos_abi::DriverError;

/// Size, in bytes, of one packed-ring descriptor (virtio 1.1 §2.7.5).
const PACKED_DESC_BYTES: usize = 16;
/// Size, in bytes, of one event-suppression structure (§2.7.14):
/// `le16 desc_event_off` + `le16 desc_event_flags`.
const EVENT_SUPPRESS_BYTES: usize = 4;

/// `flags` bit marking a descriptor as continuing into the next ring
/// entry (virtio 1.1 §2.7.6).
pub(crate) const VRING_PACKED_DESC_F_NEXT: u16 = 1;
/// `flags` bit marking a descriptor as device-write (device → driver).
pub(crate) const VRING_PACKED_DESC_F_WRITE: u16 = 2;
/// `AVAIL` flag bit (virtio 1.1 §2.7.1): `1 << 7`.
pub(crate) const VRING_PACKED_DESC_F_AVAIL: u16 = 1 << 7;
/// `USED` flag bit (virtio 1.1 §2.7.1): `1 << 15`.
pub(crate) const VRING_PACKED_DESC_F_USED: u16 = 1 << 15;

pub use crate::queue::{ChainSegment, UsedToken};

/// In-memory form of a packed-ring descriptor (virtio 1.1 §2.7.5).
#[derive(Copy, Clone, Debug, Default)]
pub(crate) struct PackedDescriptor {
    pub addr: u64,
    pub len: u32,
    pub id: u16,
    pub flags: u16,
}

/// Read the packed descriptor at ring position `slot` out of the
/// little-endian bytes of the descriptor ring.
pub(crate) fn read_packed_desc(ring: &[u8], slot: u16) -> PackedDescriptor {
    let off = (slot as usize) * PACKED_DESC_BYTES;
    let addr = u64::from_le_bytes(ring[off..off + 8].try_into().unwrap_or_default());
    let len = u32::from_le_bytes(ring[off + 8..off + 12].try_into().unwrap_or_default());
    let id = u16::from_le_bytes(ring[off + 12..off + 14].try_into().unwrap_or_default());
    let flags = u16::from_le_bytes(ring[off + 14..off + 16].try_into().unwrap_or_default());
    PackedDescriptor {
        addr,
        len,
        id,
        flags,
    }
}

/// Write `d` into ring position `slot` of the descriptor ring bytes.
pub(crate) fn write_packed_desc(ring: &mut [u8], slot: u16, d: PackedDescriptor) {
    let off = (slot as usize) * PACKED_DESC_BYTES;
    ring[off..off + 8].copy_from_slice(&d.addr.to_le_bytes());
    ring[off + 8..off + 12].copy_from_slice(&d.len.to_le_bytes());
    ring[off + 12..off + 14].copy_from_slice(&d.id.to_le_bytes());
    ring[off + 14..off + 16].copy_from_slice(&d.flags.to_le_bytes());
}

/// `true` iff the descriptor flags mark the descriptor available to a
/// device whose Device Ring Wrap Counter is `wrap` (virtio 1.1
/// §2.7.1): the `AVAIL` and `USED` bits differ and `AVAIL` matches
/// `wrap`.
pub(crate) fn desc_is_available(flags: u16, wrap: bool) -> bool {
    let avail = (flags & VRING_PACKED_DESC_F_AVAIL) != 0;
    let used = (flags & VRING_PACKED_DESC_F_USED) != 0;
    avail != used && avail == wrap
}

/// `true` iff the descriptor flags mark the descriptor used by a
/// device, as observed by a driver whose used wrap counter is `wrap`
/// (virtio 1.1 §2.7.1): the `AVAIL` and `USED` bits are equal and
/// match `wrap`.
pub(crate) fn desc_is_used(flags: u16, wrap: bool) -> bool {
    let avail = (flags & VRING_PACKED_DESC_F_AVAIL) != 0;
    let used = (flags & VRING_PACKED_DESC_F_USED) != 0;
    avail == used && used == wrap
}

/// Packed virtqueue (virtio 1.1 §2.7).
///
/// Owns a single descriptor-ring [`DmaSlab`] plus the driver- and
/// device-event-suppression [`DmaSlab`]s. The driver publishes chains
/// by writing consecutive ring entries and toggling per-descriptor
/// `AVAIL`/`USED` flag bits against the [`Self`] wrap counters, then
/// drains completions the device signals back in-band.
pub struct PackedQueue {
    queue_index: u16,
    queue_size: u16,
    desc: DmaSlab,
    driver_event: DmaSlab,
    device_event: DmaSlab,
    /// Next ring position the driver will write (virtio 1.1 §2.7.6.1
    /// `next_avail`).
    avail_idx: u16,
    /// Driver Ring Wrap Counter (§2.7.1), initialised to `true` (1).
    avail_wrap: bool,
    /// Next ring position the driver will inspect for completions.
    used_idx: u16,
    /// Driver-side mirror of the Device Ring Wrap Counter (§2.7.1),
    /// initialised to `true` (1).
    used_wrap: bool,
    /// Free ring slots (descriptors not currently in flight).
    free_count: u16,
    /// Per-buffer-id descriptor count, indexed by the head ring
    /// position used as the buffer id. Lets [`Self::poll_used`]
    /// advance `used_idx` by the right chain length.
    chain_len: alloc::vec::Vec<u16>,
}

impl PackedQueue {
    /// Required descriptor-ring byte size for `queue_size`.
    #[must_use]
    pub const fn desc_ring_size(queue_size: u16) -> usize {
        PACKED_DESC_BYTES * queue_size as usize
    }
    /// Required byte size of one event-suppression structure.
    #[must_use]
    pub const fn event_suppress_size() -> usize {
        EVENT_SUPPRESS_BYTES
    }

    /// Bring `queue_index` online as a packed queue: allocate the
    /// descriptor ring and the two event-suppression structures via
    /// `host`, program `transport`, and initialise the wrap counters.
    ///
    /// # Errors
    ///
    /// Propagates allocation and transport errors.
    pub fn new<T: Transport>(
        transport: &mut T,
        host: &dyn VirtioHost,
        queue_index: u16,
        requested_size: u16,
    ) -> Result<Self, VirtioError> {
        transport.queue_select(queue_index)?;
        let max = transport.queue_max_size();
        if max == 0 {
            return Err(VirtioError::QueueSizeTooLarge);
        }
        let size = core::cmp::min(requested_size, max);
        if size == 0 || !size.is_power_of_two() {
            return Err(VirtioError::QueueSizeTooLarge);
        }
        let desc = host
            .alloc_dma_zeroed(Self::desc_ring_size(size))
            .map_err(|_| VirtioError::DeviceFault)?;
        let driver_event = host
            .alloc_dma_zeroed(Self::event_suppress_size())
            .map_err(|_| VirtioError::DeviceFault)?;
        let device_event = host
            .alloc_dma_zeroed(Self::event_suppress_size())
            .map_err(|_| VirtioError::DeviceFault)?;
        transport.queue_set(size, desc.phys(), driver_event.phys(), device_event.phys())?;
        Ok(Self {
            queue_index,
            queue_size: size,
            desc,
            driver_event,
            device_event,
            avail_idx: 0,
            avail_wrap: true,
            used_idx: 0,
            used_wrap: true,
            free_count: size,
            chain_len: alloc::vec![0u16; size as usize],
        })
    }

    /// The queue's index on the device.
    #[must_use]
    pub fn index(&self) -> u16 {
        self.queue_index
    }
    /// The queue's negotiated size.
    #[must_use]
    pub fn size(&self) -> u16 {
        self.queue_size
    }
    /// Free ring-slot count.
    #[must_use]
    pub fn free_count(&self) -> u16 {
        self.free_count
    }
    /// Device-visible base address of the driver-event-suppression
    /// structure (programmed into the transport's `queue_driver`).
    #[must_use]
    pub fn driver_event_phys(&self) -> u64 {
        self.driver_event.phys()
    }
    /// Device-visible base address of the device-event-suppression
    /// structure (programmed into the transport's `queue_device`).
    #[must_use]
    pub fn device_event_phys(&self) -> u64 {
        self.device_event.phys()
    }

    /// Publish `segments` as a single packed descriptor chain
    /// occupying `segments.len()` consecutive ring entries.
    ///
    /// Returns the buffer id (the chain's head ring position), which
    /// the caller stores alongside any out-of-band per-request state
    /// until [`Self::poll_used`] returns it back.
    ///
    /// # Errors
    ///
    /// * [`VirtioError::QueueFull`] if the chain needs more ring slots
    ///   than are free.
    /// * [`VirtioError::DescriptorTableOverflow`] if `segments` is
    ///   empty or longer than `queue_size`.
    pub fn add_chain(&mut self, segments: &[ChainSegment]) -> Result<u16, VirtioError> {
        if segments.is_empty() || segments.len() > self.queue_size as usize {
            return Err(VirtioError::DescriptorTableOverflow);
        }
        let chain_len =
            u16::try_from(segments.len()).map_err(|_| VirtioError::DescriptorTableOverflow)?;
        if chain_len > self.free_count {
            return Err(VirtioError::QueueFull);
        }
        let head = self.avail_idx;
        let buffer_id = head;
        // Stage every descriptor's bytes first; the head descriptor's
        // AVAIL/USED flags are written last so the device never
        // observes a partial chain (virtio 1.1 §2.7.6).
        let mut pos = self.avail_idx;
        let mut wrap = self.avail_wrap;
        let mut head_flags = 0u16;
        for (i, seg) in segments.iter().enumerate() {
            let is_last = i + 1 == segments.len();
            let mut flags = Self::avail_flag_bits(wrap);
            if matches!(seg.direction, Direction::DeviceWrite) {
                flags |= VRING_PACKED_DESC_F_WRITE;
            }
            if !is_last {
                flags |= VRING_PACKED_DESC_F_NEXT;
            }
            let id = if is_last { buffer_id } else { 0 };
            let ring = self.desc.as_bytes_mut();
            if i == 0 {
                // Defer the head: write addr/len/id now, flags last.
                write_packed_desc(
                    ring,
                    pos,
                    PackedDescriptor {
                        addr: seg.phys,
                        len: seg.len,
                        id,
                        flags: 0,
                    },
                );
                head_flags = flags;
            } else {
                write_packed_desc(
                    ring,
                    pos,
                    PackedDescriptor {
                        addr: seg.phys,
                        len: seg.len,
                        id,
                        flags,
                    },
                );
            }
            (pos, wrap) = self.advance(pos, wrap);
        }
        // Publish the head, making the whole chain available at once.
        let ring = self.desc.as_bytes_mut();
        let mut head_desc = read_packed_desc(ring, head);
        head_desc.flags = head_flags;
        write_packed_desc(ring, head, head_desc);
        self.avail_idx = pos;
        self.avail_wrap = wrap;
        self.chain_len[buffer_id as usize] = chain_len;
        self.free_count -= chain_len;
        Ok(buffer_id)
    }

    /// Notify the device that new chain(s) are available on this
    /// queue.
    pub fn kick<T: Transport>(&self, transport: &mut T) {
        transport.notify(self.queue_index);
    }

    /// Pop one completion from the ring, if any, reclaiming the
    /// chain's ring slots.
    ///
    /// # Errors
    ///
    /// * [`VirtioError::NoCompletion`] if no new completion is
    ///   available.
    pub fn poll_used(&mut self) -> Result<UsedToken, VirtioError> {
        let ring = self.desc.as_bytes();
        let d = read_packed_desc(ring, self.used_idx);
        if !desc_is_used(d.flags, self.used_wrap) {
            return Err(VirtioError::NoCompletion);
        }
        let buffer_id = d.id;
        let written = d.len;
        let len = self
            .chain_len
            .get(buffer_id as usize)
            .copied()
            .filter(|&l| l != 0)
            .ok_or(VirtioError::DeviceFault)?;
        // Advance the used cursor past every descriptor in the chain.
        let mut pos = self.used_idx;
        let mut wrap = self.used_wrap;
        for _ in 0..len {
            (pos, wrap) = self.advance(pos, wrap);
        }
        self.used_idx = pos;
        self.used_wrap = wrap;
        self.chain_len[buffer_id as usize] = 0;
        self.free_count = self.free_count.saturating_add(len);
        if self.free_count > self.queue_size {
            self.free_count = self.queue_size;
        }
        Ok(UsedToken {
            head: buffer_id,
            written,
        })
    }

    /// Map a [`VirtioError`] from queue operations into a
    /// [`DriverError`] suitable for surface-trait returns.
    #[must_use]
    pub fn err_to_driver(err: VirtioError) -> DriverError {
        err.as_driver_error()
    }

    /// `AVAIL` set to `wrap`, `USED` set to its inverse — the flag
    /// pattern that marks a descriptor available (virtio 1.1 §2.7.1).
    fn avail_flag_bits(wrap: bool) -> u16 {
        if wrap {
            VRING_PACKED_DESC_F_AVAIL
        } else {
            VRING_PACKED_DESC_F_USED
        }
    }

    /// Advance a `(position, wrap)` ring cursor by one entry, toggling
    /// the wrap counter when stepping off the last ring slot
    /// (virtio 1.1 §2.7.1).
    fn advance(&self, pos: u16, wrap: bool) -> (u16, bool) {
        if pos + 1 == self.queue_size {
            (0, !wrap)
        } else {
            (pos + 1, wrap)
        }
    }
}

/// Mock-peer-only view over a packed descriptor ring, mirroring
/// [`crate::queue::ring_view`] for the split format. It lets a
/// [`crate::transport::MockTransport`] inspect descriptors a driver
/// made available, collect a chain, and write completions back —
/// without owning the driver's allocations.
pub(crate) mod packed_ring_view {
    use super::{
        desc_is_available, read_packed_desc, write_packed_desc, PackedDescriptor,
        PACKED_DESC_BYTES, VRING_PACKED_DESC_F_NEXT, VRING_PACKED_DESC_F_USED,
        VRING_PACKED_DESC_F_WRITE,
    };
    use crate::transport::{ChainView, VirtioError};
    use alloc::vec::Vec;

    pub(crate) struct PackedRingView {
        queue_size: u16,
        desc: *mut u8,
    }

    /// One chain collected from the ring: the read/write segments for
    /// the shim, the number of ring entries it spans, and its
    /// buffer id (taken from the last descriptor).
    pub(crate) struct CollectedChain<'a> {
        pub(crate) chain: ChainView<'a>,
        pub(crate) len: u16,
        pub(crate) buffer_id: u16,
    }

    impl PackedRingView {
        /// Construct from the descriptor-ring phys the driver
        /// programmed into the transport.
        ///
        /// # Safety-invariant
        ///
        /// Mirrors [`crate::queue::ring_view::RingView::from_phys`]:
        /// the mock peer treats `desc` as the base of a
        /// `PackedQueue::desc_ring_size(queue_size)`-byte ring living
        /// in driver-owned storage that outlives every view derived
        /// from it; the view is only touched inside `MockTransport`
        /// methods that hold the driver exclusively.
        pub(crate) fn from_phys(queue_size: u16, desc: u64) -> Self {
            Self {
                queue_size,
                desc: desc as *mut u8,
            }
        }

        fn ring_bytes(&self) -> usize {
            (self.queue_size as usize) * PACKED_DESC_BYTES
        }

        pub(crate) fn read_desc(&self, slot: u16) -> PackedDescriptor {
            // SAFETY: `slot < queue_size` and the ring spans
            // `ring_bytes()` driver-owned bytes (see `from_phys`).
            let ring =
                unsafe { core::slice::from_raw_parts(self.desc.cast_const(), self.ring_bytes()) };
            read_packed_desc(ring, slot)
        }

        /// `true` iff the descriptor at `slot` is available to a
        /// device with wrap counter `wrap`.
        pub(crate) fn is_available(&self, slot: u16, wrap: bool) -> bool {
            desc_is_available(self.read_desc(slot).flags, wrap)
        }

        fn advance(&self, pos: u16, wrap: bool) -> (u16, bool) {
            if pos + 1 == self.queue_size {
                (0, !wrap)
            } else {
                (pos + 1, wrap)
            }
        }

        /// Walk the chain starting at `head` (with device wrap
        /// `wrap`), following `NEXT` across consecutive ring entries.
        pub(crate) fn collect_chain<'a>(
            &self,
            head: u16,
            wrap: bool,
        ) -> Result<CollectedChain<'a>, VirtioError> {
            let mut device_read: Vec<&'a [u8]> = Vec::new();
            let mut device_write: Vec<&'a mut [u8]> = Vec::new();
            let mut pos = head;
            let mut cur_wrap = wrap;
            let mut len = 0u16;
            let buffer_id = loop {
                if len >= self.queue_size {
                    return Err(VirtioError::DescriptorTableOverflow);
                }
                let d = self.read_desc(pos);
                // SAFETY: `d.addr`/`d.len` were programmed by the
                // driver from a `DmaSlab` it still owns; the mock peer
                // reconstructs a slice of length `d.len` for the
                // duration of one `drain_packed_queue` call.
                if (d.flags & VRING_PACKED_DESC_F_WRITE) != 0 {
                    let s: &'a mut [u8] = unsafe {
                        core::slice::from_raw_parts_mut(d.addr as *mut u8, d.len as usize)
                    };
                    device_write.push(s);
                } else {
                    let s: &'a [u8] =
                        unsafe { core::slice::from_raw_parts(d.addr as *const u8, d.len as usize) };
                    device_read.push(s);
                }
                len += 1;
                if (d.flags & VRING_PACKED_DESC_F_NEXT) == 0 {
                    break d.id;
                }
                (pos, cur_wrap) = self.advance(pos, cur_wrap);
            };
            Ok(CollectedChain {
                chain: ChainView {
                    device_read,
                    device_write,
                },
                len,
                buffer_id,
            })
        }

        /// Mark the chain whose head is at `slot` used: write
        /// `buffer_id` / `written` into the head descriptor and set
        /// its `AVAIL`/`USED` bits both to `wrap` (virtio 1.1 §2.7.1).
        pub(crate) fn publish_used(&self, slot: u16, wrap: bool, buffer_id: u16, written: u32) {
            let avail_bit = super::VRING_PACKED_DESC_F_AVAIL;
            let used_bit = VRING_PACKED_DESC_F_USED;
            let flags = if wrap { avail_bit | used_bit } else { 0 };
            // SAFETY: as in `read_desc`; the head slot lies inside the
            // driver-owned ring and the mock peer holds the driver
            // exclusively while publishing.
            let ring = unsafe { core::slice::from_raw_parts_mut(self.desc, self.ring_bytes()) };
            let mut d = read_packed_desc(ring, slot);
            d.id = buffer_id;
            d.len = written;
            d.flags = flags;
            write_packed_desc(ring, slot, d);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn descriptor_round_trips_through_ring_bytes() {
        let mut ring = alloc::vec![0u8; PACKED_DESC_BYTES * 2];
        let d = PackedDescriptor {
            addr: 0xDEAD_BEEF_0000_1000,
            len: 0x1234,
            id: 0xABCD,
            flags: VRING_PACKED_DESC_F_NEXT | VRING_PACKED_DESC_F_AVAIL,
        };
        write_packed_desc(&mut ring, 1, d);
        let read = read_packed_desc(&ring, 1);
        assert_eq!(read.addr, d.addr);
        assert_eq!(read.len, d.len);
        assert_eq!(read.id, d.id);
        assert_eq!(read.flags, d.flags);
        // Slot 0 untouched.
        assert_eq!(read_packed_desc(&ring, 0).flags, 0);
    }

    #[test]
    fn availability_and_used_truth_table() {
        // Driver makes available with wrap=1: AVAIL set, USED clear.
        let made_avail = VRING_PACKED_DESC_F_AVAIL;
        assert!(desc_is_available(made_avail, true));
        assert!(!desc_is_available(made_avail, false));
        assert!(!desc_is_used(made_avail, true));
        // Device marks used with wrap=1: AVAIL set, USED set.
        let made_used = VRING_PACKED_DESC_F_AVAIL | VRING_PACKED_DESC_F_USED;
        assert!(desc_is_used(made_used, true));
        assert!(!desc_is_used(made_used, false));
        assert!(!desc_is_available(made_used, true));
        // After a wrap, available is encoded with USED set, AVAIL clear.
        let avail_wrapped = VRING_PACKED_DESC_F_USED;
        assert!(desc_is_available(avail_wrapped, false));
        assert!(!desc_is_available(avail_wrapped, true));
    }

    #[test]
    fn avail_flag_bits_match_wrap_counter() {
        assert_eq!(
            PackedQueue::avail_flag_bits(true),
            VRING_PACKED_DESC_F_AVAIL
        );
        assert_eq!(
            PackedQueue::avail_flag_bits(false),
            VRING_PACKED_DESC_F_USED
        );
    }
}
