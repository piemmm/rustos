//! Split virtqueue management (virtio 1.1 §2.6).
//!
//! [`SplitQueue`] owns three host-allocated DMA regions — the
//! descriptor table, the avail ring, and the used ring — plus the
//! free-descriptor pool and last-seen used index. It exposes
//! [`SplitQueue::add_chain`] to publish a descriptor chain,
//! [`SplitQueue::kick`] to notify the device, and
//! [`SplitQueue::poll_used`] to drain a completion.
//!
//! Packed queues (virtio 1.1 §2.7) are intentionally absent; see the
//! crate root for the deferral note.

use crate::dma::DmaSlab;
use crate::host::VirtioHost;
use crate::transport::{Direction, Transport, VirtioError};
use core::mem::size_of;
use rustos_abi::DriverError;

/// Wire layout of a virtio split-queue descriptor (virtio 1.1 §2.6.5).
#[repr(C, align(16))]
#[derive(Copy, Clone, Debug, Default)]
pub(crate) struct Descriptor {
    pub addr: u64,
    pub len: u32,
    pub flags: u16,
    pub next: u16,
}

/// `flags` bit indicating the next descriptor in a chain.
pub(crate) const VRING_DESC_F_NEXT: u16 = 1;
/// `flags` bit indicating the device writes (rather than reads).
pub(crate) const VRING_DESC_F_WRITE: u16 = 2;

/// One entry of the used ring.
#[repr(C)]
#[derive(Copy, Clone, Debug, Default)]
pub(crate) struct UsedElem {
    pub id: u32,
    pub len: u32,
}

/// Successful completion handle returned from [`SplitQueue::poll_used`].
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct UsedToken {
    /// Head descriptor index of the completed chain.
    pub head: u16,
    /// Bytes the device wrote into the chain's write-only segments.
    pub written: u32,
}

/// One segment in a publishable descriptor chain.
#[derive(Copy, Clone, Debug)]
pub struct ChainSegment {
    /// Device-visible base address of this segment.
    pub phys: u64,
    /// Length, in bytes.
    pub len: u32,
    /// Direction (device-read vs device-write).
    pub direction: Direction,
}

/// Split virtqueue.
///
/// The descriptor table, avail ring, and used ring each live in
/// host-allocated [`DmaSlab`]s carried inside this struct.
pub struct SplitQueue {
    queue_index: u16,
    queue_size: u16,
    desc: DmaSlab,
    avail: DmaSlab,
    used: DmaSlab,
    /// Index of the first free descriptor in the free-list. Each
    /// free entry's `next` field points to the following free one;
    /// the tail's `next` field is `queue_size` (an out-of-range
    /// sentinel).
    free_head: u16,
    /// Number of free descriptors.
    free_count: u16,
    /// Last used-ring `idx` we observed.
    last_used_idx: u16,
    /// Avail-ring `idx` we'll write next.
    next_avail_idx: u16,
}

const AVAIL_HEADER_BYTES: usize = 4; // flags + idx
const AVAIL_TAIL_BYTES: usize = 2; // used_event
const USED_HEADER_BYTES: usize = 4; // flags + idx
const USED_TAIL_BYTES: usize = 2; // avail_event

impl SplitQueue {
    /// Required descriptor-table byte size for `queue_size`.
    #[must_use]
    pub const fn desc_table_size(queue_size: u16) -> usize {
        size_of::<Descriptor>() * queue_size as usize
    }
    /// Required avail-ring byte size for `queue_size`.
    #[must_use]
    pub const fn avail_ring_size(queue_size: u16) -> usize {
        AVAIL_HEADER_BYTES + (queue_size as usize) * 2 + AVAIL_TAIL_BYTES
    }
    /// Required used-ring byte size for `queue_size`.
    #[must_use]
    pub const fn used_ring_size(queue_size: u16) -> usize {
        USED_HEADER_BYTES + (queue_size as usize) * size_of::<UsedElem>() + USED_TAIL_BYTES
    }

    /// Bring `queue_index` online: allocate the rings via `host`,
    /// program `transport`, and initialise the free-descriptor pool.
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
            .alloc_dma_zeroed(Self::desc_table_size(size))
            .map_err(|_| VirtioError::DeviceFault)?;
        let avail = host
            .alloc_dma_zeroed(Self::avail_ring_size(size))
            .map_err(|_| VirtioError::DeviceFault)?;
        let used = host
            .alloc_dma_zeroed(Self::used_ring_size(size))
            .map_err(|_| VirtioError::DeviceFault)?;
        transport.queue_set(size, desc.phys(), avail.phys(), used.phys())?;
        let mut q = Self {
            queue_index,
            queue_size: size,
            desc,
            avail,
            used,
            free_head: 0,
            free_count: size,
            last_used_idx: 0,
            next_avail_idx: 0,
        };
        q.init_free_list();
        Ok(q)
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
    /// Free-descriptor count.
    #[must_use]
    pub fn free_count(&self) -> u16 {
        self.free_count
    }

    fn init_free_list(&mut self) {
        for i in 0..self.queue_size {
            let next = if i + 1 == self.queue_size {
                self.queue_size
            } else {
                i + 1
            };
            self.write_desc(
                i,
                Descriptor {
                    addr: 0,
                    len: 0,
                    flags: 0,
                    next,
                },
            );
        }
        self.free_head = 0;
        self.free_count = self.queue_size;
    }

    fn write_desc(&mut self, idx: u16, d: Descriptor) {
        let offset = (idx as usize) * size_of::<Descriptor>();
        let bytes = self.desc.as_bytes_mut();
        bytes[offset..offset + 8].copy_from_slice(&d.addr.to_le_bytes());
        bytes[offset + 8..offset + 12].copy_from_slice(&d.len.to_le_bytes());
        bytes[offset + 12..offset + 14].copy_from_slice(&d.flags.to_le_bytes());
        bytes[offset + 14..offset + 16].copy_from_slice(&d.next.to_le_bytes());
    }

    fn read_desc(&self, idx: u16) -> Descriptor {
        let offset = (idx as usize) * size_of::<Descriptor>();
        let bytes = self.desc.as_bytes();
        let addr = u64::from_le_bytes(bytes[offset..offset + 8].try_into().unwrap_or_default());
        let len = u32::from_le_bytes(
            bytes[offset + 8..offset + 12]
                .try_into()
                .unwrap_or_default(),
        );
        let flags = u16::from_le_bytes(
            bytes[offset + 12..offset + 14]
                .try_into()
                .unwrap_or_default(),
        );
        let next = u16::from_le_bytes(
            bytes[offset + 14..offset + 16]
                .try_into()
                .unwrap_or_default(),
        );
        Descriptor {
            addr,
            len,
            flags,
            next,
        }
    }

    /// Publish `segments` as a single descriptor chain.
    ///
    /// Returns the head descriptor index, which the caller stores
    /// alongside any out-of-band per-request state until
    /// [`Self::poll_used`] returns it back.
    ///
    /// # Errors
    ///
    /// * [`VirtioError::QueueFull`] if the chain needs more
    ///   descriptors than the free pool holds.
    /// * [`VirtioError::DescriptorTableOverflow`] if `segments` is
    ///   empty or longer than `queue_size`.
    pub fn add_chain(&mut self, segments: &[ChainSegment]) -> Result<u16, VirtioError> {
        if segments.is_empty() || segments.len() > self.queue_size as usize {
            return Err(VirtioError::DescriptorTableOverflow);
        }
        let segments_len =
            u16::try_from(segments.len()).map_err(|_| VirtioError::DescriptorTableOverflow)?;
        if segments_len > self.free_count {
            return Err(VirtioError::QueueFull);
        }
        let head = self.free_head;
        let mut cur = head;
        // Free-list successor of the chain's tail. Captured inside
        // the loop because each iteration overwrites the descriptor
        // whose `next` field still encodes the free-list link.
        let mut after_tail: u16 = self.queue_size;
        for (i, seg) in segments.iter().enumerate() {
            let mut flags: u16 = 0;
            if matches!(seg.direction, Direction::DeviceWrite) {
                flags |= VRING_DESC_F_WRITE;
            }
            let is_last = i + 1 == segments.len();
            let prev = self.read_desc(cur);
            let next_free = prev.next;
            after_tail = next_free;
            let next_chain = if is_last { 0 } else { next_free };
            if !is_last {
                flags |= VRING_DESC_F_NEXT;
            }
            self.write_desc(
                cur,
                Descriptor {
                    addr: seg.phys,
                    len: seg.len,
                    flags,
                    next: next_chain,
                },
            );
            if !is_last {
                cur = next_free;
            }
        }
        self.free_head = after_tail;
        self.free_count -= segments_len;
        // Publish into avail ring.
        let slot = self.next_avail_idx % self.queue_size;
        let avail_bytes = self.avail.as_bytes_mut();
        let off = AVAIL_HEADER_BYTES + (slot as usize) * 2;
        avail_bytes[off..off + 2].copy_from_slice(&head.to_le_bytes());
        self.next_avail_idx = self.next_avail_idx.wrapping_add(1);
        // Publish barrier (virtio 1.1 §2.7.13.3.1): the descriptor-table and
        // avail-ring *entry* stores above must be visible to the device
        // **before** the avail-`idx` store that exposes them, or a device that
        // observes the new index could read a not-yet-written descriptor. A
        // synchronous backend (virtio-blk, drained on the same `notify` the
        // guest issues) tolerates the omission; an *asynchronous* device
        // (virtio-input, which pops a buffer when an input event arrives out
        // of band) does not — it reads the ring from a different context and
        // must see a consistent snapshot (`AGENTS.md` §2.1 — no races). The
        // release fence orders the entry stores before the index store.
        core::sync::atomic::fence(core::sync::atomic::Ordering::Release);
        // Update the avail.idx field (offset 2, little-endian u16).
        avail_bytes[2..4].copy_from_slice(&self.next_avail_idx.to_le_bytes());
        Ok(head)
    }

    /// Notify the device that new chain(s) are available on this
    /// queue.
    pub fn kick<T: Transport>(&self, transport: &mut T) {
        // Notify barrier (virtio 1.1 §2.7.13.3.1): the avail-`idx` store in
        // `add_chain` must be globally visible before the device is notified,
        // so a device that wakes on the notify reads the published index
        // rather than a stale one. Without it an asynchronous device
        // (virtio-input) can observe an empty avail ring and report
        // queue-full on the next event (`AGENTS.md` §2.1).
        core::sync::atomic::fence(core::sync::atomic::Ordering::SeqCst);
        transport.notify(self.queue_index);
    }

    /// Pop one completion from the used ring, if any. Reclaims the
    /// descriptor chain into the free pool.
    ///
    /// # Errors
    ///
    /// * [`VirtioError::NoCompletion`] if no new completion is
    ///   available.
    /// * [`VirtioError::MalformedCompletion`] if the device-written
    ///   completion names a descriptor head outside the granted
    ///   descriptor table (`AGENTS.md` §3.6, CWE-1257 / Thunderclap).
    ///   The bogus entry is skipped (the queue still makes progress) and
    ///   no chain is reclaimed — fail closed (§5.4).
    pub fn poll_used(&mut self) -> Result<UsedToken, VirtioError> {
        let used_bytes = self.used.as_bytes();
        let used_idx = u16::from_le_bytes(used_bytes[2..4].try_into().unwrap_or_default());
        if used_idx == self.last_used_idx {
            return Err(VirtioError::NoCompletion);
        }
        // Consume barrier (virtio 1.1 §2.7.13.3.2): having observed a new
        // used-`idx`, the device's writes to the used-ring *entry* it points
        // at must be acquired before they are read, so the entry read cannot
        // be reordered ahead of — or read stale relative to — the index that
        // announced it (`AGENTS.md` §2.1).
        core::sync::atomic::fence(core::sync::atomic::Ordering::Acquire);
        let slot = (self.last_used_idx % self.queue_size) as usize;
        let entry_off = USED_HEADER_BYTES + slot * size_of::<UsedElem>();
        let id = u32::from_le_bytes(
            used_bytes[entry_off..entry_off + 4]
                .try_into()
                .unwrap_or_default(),
        );
        let written = u32::from_le_bytes(
            used_bytes[entry_off + 4..entry_off + 8]
                .try_into()
                .unwrap_or_default(),
        );
        let head = (id & 0xFFFF) as u16;
        // The completion id is **device-written**, hence untrusted: a
        // buggy or hostile device (CWE-1257 / Thunderclap) can name a
        // head outside the descriptor table the driver granted it.
        // Walking such a head would index `desc` storage outside the
        // granted region, so reject it fail-closed (`AGENTS.md` §3.6 /
        // §5.4). The bogus used entry is still consumed so the queue
        // makes forward progress, but no chain is reclaimed on its word.
        if head >= self.queue_size {
            self.last_used_idx = self.last_used_idx.wrapping_add(1);
            return Err(VirtioError::MalformedCompletion);
        }
        // Reclaim chain into free list.
        self.reclaim_chain(head);
        self.last_used_idx = self.last_used_idx.wrapping_add(1);
        Ok(UsedToken { head, written })
    }

    fn reclaim_chain(&mut self, head: u16) {
        // Walk the chain to the tail, clearing addr/len/flags but
        // **preserving the chain-order `next` field on interior
        // descriptors** — those next pointers are valid free-list
        // links because we built the chain in free-list order.
        // Finally re-link the tail to the previous `free_head`.
        let mut len = 1u16;
        let mut cur = head;
        loop {
            // A chain `next` link is descriptor-table data and may have
            // been corrupted (CWE-1257 / Thunderclap DMA write). Never
            // read a descriptor outside the granted table: bail rather
            // than index out of region (`AGENTS.md` §3.6 / §5.4). The
            // caller validated `head`; this guards every followed `next`.
            if cur >= self.queue_size {
                return;
            }
            let d = self.read_desc(cur);
            if (d.flags & VRING_DESC_F_NEXT) == 0 {
                // Tail: stitch into free list.
                self.write_desc(
                    cur,
                    Descriptor {
                        addr: 0,
                        len: 0,
                        flags: 0,
                        next: self.free_head,
                    },
                );
                break;
            }
            // Interior: preserve `next`, clear everything else.
            let next = d.next;
            self.write_desc(
                cur,
                Descriptor {
                    addr: 0,
                    len: 0,
                    flags: 0,
                    next,
                },
            );
            cur = next;
            len += 1;
            if len > self.queue_size {
                // Cycle detected — corrupt chain; bail without
                // updating free_head to avoid losing more slots.
                return;
            }
        }
        self.free_head = head;
        self.free_count = self.free_count.saturating_add(len);
        // Clamp in case of double-free in a corrupt scenario.
        if self.free_count > self.queue_size {
            self.free_count = self.queue_size;
        }
    }

    /// Map a [`VirtioError`] from queue operations into a
    /// [`DriverError`] suitable for surface-trait returns.
    #[must_use]
    pub fn err_to_driver(err: VirtioError) -> DriverError {
        err.as_driver_error()
    }
}

/// Mock-peer-only view that allows a [`crate::transport::MockTransport`]
/// to read the avail ring, collect a chain by descriptor head, and
/// publish into the used ring without owning the underlying
/// allocations. The implementation reconstructs the ring layouts from
/// the `phys` addresses planted by the driver.
pub(crate) mod ring_view {
    use super::{
        Descriptor, UsedElem, AVAIL_HEADER_BYTES, USED_HEADER_BYTES, VRING_DESC_F_NEXT,
        VRING_DESC_F_WRITE,
    };
    use crate::transport::{ChainView, VirtioError};
    use alloc::vec::Vec;
    use core::mem::size_of;

    pub(crate) struct RingView {
        queue_size: u16,
        desc: *mut u8,
        avail: *mut u8,
        used: *mut u8,
    }

    impl RingView {
        /// Construct a `RingView` from the phys addresses the driver
        /// programmed into the transport. The lifetime of the
        /// resulting pointers tracks the driver-owned allocations.
        ///
        /// # Safety-invariant
        ///
        /// The mock peer (the only caller) treats these `*mut u8`s
        /// as `&mut [u8]` of the appropriate ring length, governed
        /// by [`crate::queue::SplitQueue`]'s sizing helpers. The
        /// queue stores those slices in `DmaSlab`s that
        /// outlive every `RingView` derived from them; we therefore
        /// only access them inside the body of `MockTransport`
        /// methods (which borrow the driver exclusively via the
        /// `&mut self` chain of `kick`/`poll_used`).
        pub(crate) fn from_phys(queue_size: u16, desc: u64, avail: u64, used: u64) -> Self {
            Self {
                queue_size,
                desc: desc as *mut u8,
                avail: avail as *mut u8,
                used: used as *mut u8,
            }
        }

        fn read_u16(ptr: *const u8, off: usize) -> u16 {
            // SAFETY: caller proved `ptr + off + 2` lies inside a
            // driver-owned allocation (see `from_phys`).
            unsafe {
                let p = ptr.add(off);
                u16::from_le_bytes([p.read(), p.add(1).read()])
            }
        }
        fn read_u32(ptr: *const u8, off: usize) -> u32 {
            // SAFETY: as above, for a 4-byte read.
            unsafe {
                let p = ptr.add(off);
                u32::from_le_bytes([p.read(), p.add(1).read(), p.add(2).read(), p.add(3).read()])
            }
        }
        fn read_u64(ptr: *const u8, off: usize) -> u64 {
            // SAFETY: as above, for an 8-byte read.
            unsafe {
                let p = ptr.add(off);
                u64::from_le_bytes([
                    p.read(),
                    p.add(1).read(),
                    p.add(2).read(),
                    p.add(3).read(),
                    p.add(4).read(),
                    p.add(5).read(),
                    p.add(6).read(),
                    p.add(7).read(),
                ])
            }
        }
        fn write_u16(ptr: *mut u8, off: usize, v: u16) {
            // SAFETY: caller proved `ptr + off + 2` lies inside a
            // driver-owned allocation; the only writer is the mock
            // peer, which holds `&mut self` on its transport.
            unsafe {
                let p = ptr.add(off);
                let bytes = v.to_le_bytes();
                p.write(bytes[0]);
                p.add(1).write(bytes[1]);
            }
        }
        fn write_u32(ptr: *mut u8, off: usize, v: u32) {
            // SAFETY: as above, for a 4-byte write.
            unsafe {
                let p = ptr.add(off);
                let bytes = v.to_le_bytes();
                for (i, b) in bytes.iter().enumerate() {
                    p.add(i).write(*b);
                }
            }
        }

        pub(crate) fn read_avail_idx(&self) -> u16 {
            Self::read_u16(self.avail.cast_const(), 2)
        }
        pub(crate) fn read_avail_ring(&self, slot: u16) -> u16 {
            let off = AVAIL_HEADER_BYTES + (slot as usize) * 2;
            Self::read_u16(self.avail.cast_const(), off)
        }
        fn read_desc(&self, idx: u16) -> Descriptor {
            let off = (idx as usize) * size_of::<Descriptor>();
            let addr = Self::read_u64(self.desc.cast_const(), off);
            let len = Self::read_u32(self.desc.cast_const(), off + 8);
            let flags = Self::read_u16(self.desc.cast_const(), off + 12);
            let next = Self::read_u16(self.desc.cast_const(), off + 14);
            Descriptor {
                addr,
                len,
                flags,
                next,
            }
        }

        /// Walk the chain rooted at `head` and produce a
        /// [`ChainView`] borrowing the descriptor segments.
        pub(crate) fn collect_chain<'a>(&self, head: u16) -> Result<ChainView<'a>, VirtioError> {
            let mut device_read: Vec<&'a [u8]> = Vec::new();
            let mut device_write: Vec<&'a mut [u8]> = Vec::new();
            let mut cur = head;
            let mut hops = 0u16;
            loop {
                if hops > self.queue_size {
                    return Err(VirtioError::DescriptorTableOverflow);
                }
                let d = self.read_desc(cur);
                // SAFETY: `d.addr` was programmed by the driver from
                // a `DmaSlab` whose backing storage it still owns; the
                // mock peer reconstructs a `&[u8]` of length `d.len`
                // for the duration of one `drain_queue` call.
                if (d.flags & VRING_DESC_F_WRITE) != 0 {
                    let s: &'a mut [u8] = unsafe {
                        core::slice::from_raw_parts_mut(d.addr as *mut u8, d.len as usize)
                    };
                    device_write.push(s);
                } else {
                    let s: &'a [u8] =
                        unsafe { core::slice::from_raw_parts(d.addr as *const u8, d.len as usize) };
                    device_read.push(s);
                }
                if (d.flags & VRING_DESC_F_NEXT) == 0 {
                    break;
                }
                cur = d.next;
                hops += 1;
            }
            Ok(ChainView {
                device_read,
                device_write,
            })
        }

        pub(crate) fn publish_used(&self, head: u16, written: u32) {
            let used_idx = Self::read_u16(self.used.cast_const(), 2);
            let slot = (used_idx as usize) % (self.queue_size as usize);
            let off = USED_HEADER_BYTES + slot * size_of::<UsedElem>();
            Self::write_u32(self.used, off, u32::from(head));
            Self::write_u32(self.used, off + 4, written);
            Self::write_u16(self.used, 2, used_idx.wrapping_add(1));
        }
    }
}
