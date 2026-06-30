//! Per-process-heap DMA allocator.
//!
//! the charter requires every kernel-side DMA facility to:
//!
//! 1. Carve allocations out of the calling process's heap, never a
//!    global pool;
//! 2. Place guard pages around the slab so a buffer over-run faults
//!    instead of silently corrupting a neighbour;
//! 3. Zero every byte on free, because any DMA buffer may have held
//!    credentials, key material, or capability tokens at some point in
//!    its lifetime;
//! 4. Report exhaustion through a [`Result`] — allocation failure is
//!    never a panic.
//!
//! This module owns the architecture-neutral half of that contract. It
//! composes the existing [`FrameAllocator`] (for contiguous-by-physical
//! frame blocks) with an [`AddressSpace<P>`] (for the per-process
//! virtual-address window). The capability check itself lives in
//! `kernel/sec::dma`, since `kernel/mem` deliberately depends on
//! neither `rustos-abi` nor `rustos-caps` (see `kernel/mem/Cargo.toml`).
//!
//! # Returned shape
//!
//! [`DmaPool::alloc`] hands back a [`DmaBuffer`] carrying:
//!
//! * `virt` — the page-aligned virtual address inside the pool's
//!   window, suitable for handing to the driver's CPU-side code;
//! * `phys` — the page-aligned physical address of the same first
//!   byte, suitable for publishing into a virtio descriptor or any
//!   other bus-master device's address register;
//! * `len` — the *backing* length in bytes. Allocations are rounded
//!   up to the next power-of-two pages because the frame allocator is
//!   a buddy allocator and only guarantees physical contiguity inside
//!   a single order. Drivers consume `len`, not the original request
//!   — analogous to `Vec::capacity` vs the constructor argument.
//!
//! # Guard model
//!
//! Each live allocation occupies *(2 + `data_pages`)* consecutive slots
//! in the pool's virtual window: a leading guard slot, the data
//! slots, then a trailing guard slot. Guard slots are intentionally
//! **left unmapped** in the [`AddressSpace`] so that the MMU faults on
//! a register-block over-run instead of letting it reach a
//! neighbouring allocation, exactly mirroring the convention used by
//! [`crate::slab`].
//!
//! # CPU access
//!
//! A driver's CPU-side code and the device both touch the buffer's
//! *physical frames*. The CPU reaches them through the kernel's direct
//! physical map ([`crate::phys::PhysMap`]): [`DmaPool::bytes`],
//! [`DmaPool::bytes_mut`], and [`DmaPool::slot_base`] translate the
//! buffer's `phys` into a pointer, so the bytes the driver reads are
//! exactly the bytes the device wrote — not a disconnected copy.
//!
//! # Zero-on-free
//!
//! [`DmaPool::free`] zeroes every byte of the data region through a
//! volatile-clear primitive (delegated to the `zeroize` crate per) **before** the frames are returned to the
//! [`FrameAllocator`]. The driver may not see leftover bytes in a
//! later allocation, and a forensic dump of free physical memory
//! cannot recover the credentials the buffer once held.
//!
//! # Not in scope here
//!
//! * IRQ delivery — that's `kernel/sched` / `kernel/syscall` work and
//!   is the subject of the next session prompt's Item 2.
//! * The user-space `VirtioHost` shim — lives in
//!   `drivers/bus/virtio` and will be glued to this pool once the
//!   driver host learns to thread a `DmaPool` through to its loaded
//!   modules.

use alloc::collections::BTreeMap;
use alloc::vec;
use alloc::vec::Vec;
use core::fmt;
use core::ptr::NonNull;

use zeroize::Zeroize;

use crate::error::AllocError;
use crate::frame::{Frame, FrameAllocator, PhysAddr, MAX_ORDER, PAGE_SHIFT, PAGE_SIZE};
use crate::phys::PhysMap;
use crate::ptr::slice_within;
use crate::vmm::{AddressSpace, MapFlags, Page, PageTable, PageTableError, VirtAddr};

/// Errors specific to [`DmaPool`].
///
/// Distinct from the bare [`AllocError`] because a DMA pool can fail in
/// ways the bare allocators cannot: it has its own virtual-address
/// window, its own guard-slot bookkeeping, and a constraint on the
/// caller's pointer at `free` time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum DmaError {
    /// The underlying frame or metadata allocator failed.
    Alloc(AllocError),
    /// The page-table layer rejected a map/unmap operation.
    PageTable(PageTableError),
    /// `free` was called with a [`DmaBuffer`] whose `virt` is not the
    /// start of a live allocation in this pool.
    UnknownBuffer,
    /// A buffer's physical frames fall outside the kernel's direct
    /// physical map, so the CPU cannot reach them. Indicates a
    /// mis-sized [`crate::phys::PhysMap`] for the platform; the pool
    /// fails closed rather than synthesising a pointer.
    DirectMap,
    /// The pool was constructed with a request that the allocator
    /// cannot satisfy (e.g. zero capacity, or a virtual base not
    /// page-aligned).
    InvalidPoolConfig,
    /// The requested allocation is larger than the maximum buddy
    /// order the underlying [`FrameAllocator`] supports.
    SizeUnsupported,
    /// The caller asked for a zero-length allocation. Zero-sized DMA
    /// regions are rejected on purpose: a successful return value
    /// would be indistinguishable from a one-byte allocation and is
    /// almost always a bug at the call site.
    ZeroSize,
    /// The contiguous physical block the allocator would hand out lies
    /// (wholly or partly) at or above the device's addressing limit —
    /// the grant's DMA constraint. The block is
    /// returned to the allocator and the request refused fail-closed
    /// rather than handing a device a buffer it cannot reach (or, worse,
    /// one outside the region the kernel granted it).
    AddrLimitExceeded,
}

impl From<AllocError> for DmaError {
    fn from(e: AllocError) -> Self {
        Self::Alloc(e)
    }
}

impl From<PageTableError> for DmaError {
    fn from(e: PageTableError) -> Self {
        Self::PageTable(e)
    }
}

impl fmt::Display for DmaError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Alloc(e) => write!(f, "dma alloc: {e}"),
            Self::PageTable(e) => write!(f, "dma page-table: {e:?}"),
            Self::UnknownBuffer => f.write_str("dma buffer not from this pool"),
            Self::DirectMap => f.write_str("dma buffer outside the direct physical map"),
            Self::InvalidPoolConfig => f.write_str("dma pool config invalid"),
            Self::SizeUnsupported => f.write_str("dma request exceeds max buddy order"),
            Self::ZeroSize => f.write_str("zero-sized dma allocation is not permitted"),
            Self::AddrLimitExceeded => {
                f.write_str("dma buffer exceeds the granted device addressing limit")
            }
        }
    }
}

/// A live DMA region handed out by [`DmaPool::alloc`].
///
/// `DmaBuffer` is **not** [`Drop`]: a forgotten buffer must be a hard
/// error, not a silent leak through a destructor that cannot reach the
/// originating pool. Callers free explicitly via [`DmaPool::free`].
///
/// The struct is `Copy + Clone` because a buffer descriptor is purely
/// addressing data; the kernel side keeps the *single* live record in
/// [`DmaPool`] and refuses a second free of the same `virt` address.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct DmaBuffer {
    virt: VirtAddr,
    phys: PhysAddr,
    len: usize,
}

impl DmaBuffer {
    /// CPU-side virtual address of the first byte of the region.
    ///
    /// Always page-aligned.
    #[must_use]
    pub fn virt(self) -> VirtAddr {
        self.virt
    }

    /// Device-side physical address of the first byte of the region.
    ///
    /// Always page-aligned.
    #[must_use]
    pub fn phys(self) -> PhysAddr {
        self.phys
    }

    /// Backing length in bytes (a power-of-two multiple of
    /// [`PAGE_SIZE`]).
    #[must_use]
    pub fn len(self) -> usize {
        self.len
    }

    /// `true` if the buffer is zero bytes long. Reserved for the
    /// `clippy::len_without_is_empty` lint; cannot actually occur
    /// because [`DmaPool::alloc`] rejects zero-length requests.
    #[must_use]
    pub fn is_empty(self) -> bool {
        self.len == 0
    }
}

/// Borrowed-space DMA window allocator — the single definition of the
/// guarded, contiguous, zeroed DMA carve.
///
/// Owns only the *bookkeeping* (its virtual window base, the slot bitmap,
/// and the live-allocation records); the [`AddressSpace`], the
/// [`FrameAllocator`], and the [`PhysMap`] are **borrowed** per call. This
/// is the device-RAM analogue of [`crate::mmio::MmioWindowMap`]: it lets a
/// retained live address space ([`crate::live::LiveSpace`]) carve a DMA
/// buffer into a space it owns and lends, while the owning [`DmaPool`]
/// wrapper drives the same core over a space it owns outright — so the
/// carve logic has exactly one home.
pub struct DmaWindowMap {
    base: VirtAddr,
    capacity_pages: usize,
    /// Slot bookkeeping: `slot_used[i] == true` iff page `i` of the
    /// window is currently held by some live allocation (whether as a
    /// data slot or a guard slot).
    slot_used: Vec<bool>,
    /// Live allocations keyed by `virt.as_u64()` (the first *data*
    /// page, i.e. the byte after the leading guard). `BTreeMap` rather
    /// than `HashMap` because this crate is `no_std`.
    allocations: BTreeMap<u64, Record>,
}

/// Per-process DMA pool.
///
/// `DmaPool<P>` is generic over a [`PageTable`] implementation so
/// it can be exercised by `crate::HostPageTable` in unit tests and
/// driven by the architecture page-table types from `kernel/arch/*` in
/// production.
///
/// One pool is intended to live per process that holds
/// `CapabilityId::MEM_DMA`; the capability check itself lives in
/// `kernel/sec::dma` so this crate stays free of the `rustos-abi`
/// dependency. The guarded carve mechanism lives in [`DmaWindowMap`]
/// (shared with the `dma_alloc` syscall facility); this
/// type is the thin owning adapter over a space it owns outright.
pub struct DmaPool<'a, P: PageTable> {
    address_space: AddressSpace<P>,
    window: DmaWindowMap,
    /// Direct physical map used to reach a buffer's frames from the
    /// CPU. The same frames the device DMAs to are the bytes the
    /// driver reads/writes, so there is no disconnected copy: in
    /// production this is the boot identity map, in host tests a
    /// `SimPhysMap` standing in for physical RAM.
    phys: &'a dyn PhysMap,
    /// Borrowed frame allocator used to back the data slots.
    ///
    /// `FrameAllocator` is internally synchronised via a
    /// [`rustos_sync::SpinLock`], so multiple pools may share
    /// one. The borrow is explicit (not `'static`) so the host-side
    /// tests do not need to leak their allocators and a future
    /// per-process kernel layout can carve the global allocator into
    /// per-process slices.
    frames: &'a FrameAllocator,
}

/// Per-live-allocation bookkeeping retained by the pool.
#[derive(Debug, Clone, Copy)]
struct Record {
    /// Index of the leading guard slot.
    leading_guard_slot: usize,
    /// Number of data slots (= `2^order`). Always ≥ 1 since `order`
    /// is bounded by [`MAX_ORDER`].
    data_pages: usize,
    /// Buddy-allocator order used to allocate the backing frames.
    order: u32,
    /// Starting frame of the contiguous physical block.
    start_frame: Frame,
}

impl DmaWindowMap {
    /// Construct a window allocator managing the virtual range
    /// `[base, base + capacity_pages * PAGE_SIZE)`.
    ///
    /// # Errors
    ///
    /// * [`DmaError::InvalidPoolConfig`] if `capacity_pages == 0`,
    ///   `base` is not page-aligned, or the window size overflows.
    pub fn new(base: VirtAddr, capacity_pages: usize) -> Result<Self, DmaError> {
        if capacity_pages == 0 || !base.is_page_aligned() {
            return Err(DmaError::InvalidPoolConfig);
        }
        // Reject a window whose byte span would overflow the slot
        // offset arithmetic in `virt_of_slot` before committing state.
        capacity_pages
            .checked_mul(PAGE_SIZE)
            .ok_or(DmaError::InvalidPoolConfig)?;
        Ok(Self {
            base,
            capacity_pages,
            slot_used: vec![false; capacity_pages],
            allocations: BTreeMap::new(),
        })
    }

    /// Allocate a contiguous DMA region of at least `requested` bytes
    /// into the borrowed `space`, drawing contiguous frames from `frames`
    /// and reaching them through `phys`.
    ///
    /// When `addr_limit` is non-zero the contiguous block must lie wholly
    /// below it (the granted device addressing constraint); a block that would reach at or above the limit is returned to
    /// the allocator and the request refused with
    /// [`DmaError::AddrLimitExceeded`]. `addr_limit == 0` means "no
    /// constraint declared" (the in-kernel pool path).
    pub fn alloc_into<P: PageTable>(
        &mut self,
        space: &mut AddressSpace<P>,
        frames: &FrameAllocator,
        phys: &dyn PhysMap,
        requested: usize,
        addr_limit: u64,
    ) -> Result<DmaBuffer, DmaError> {
        self.alloc_inner(space, frames, phys, requested, addr_limit)
    }

    /// Free a previously-allocated DMA buffer from the borrowed `space`,
    /// zeroing every byte (zero-on-free) before the frames
    /// return to `frames`.
    ///
    /// # Errors
    ///
    /// As [`DmaPool::free`].
    pub fn free_from<P: PageTable>(
        &mut self,
        space: &mut AddressSpace<P>,
        frames: &FrameAllocator,
        phys: &dyn PhysMap,
        buf: DmaBuffer,
    ) -> Result<(), DmaError> {
        self.free_inner(space, frames, phys, buf)
    }

    /// Free the live allocation whose first data page is at `virt`, zeroing
    /// every byte (zero-on-free) before its frames return to `frames`.
    ///
    /// The symmetric free for the `dma_free` syscall, which keys a release on
    /// the CPU virtual base the carve returned (the driver holds no
    /// [`DmaBuffer`] descriptor across the syscall boundary). The record's own
    /// `phys`/`len` are authoritative; only `virt` is taken from the caller,
    /// so a `virt` that is not the base of a live carve fails closed with
    /// [`DmaError::UnknownBuffer`] (covering a forged, stale, or double free).
    ///
    /// # Errors
    ///
    /// As [`DmaPool::free`].
    pub fn free_at<P: PageTable>(
        &mut self,
        space: &mut AddressSpace<P>,
        frames: &FrameAllocator,
        phys: &dyn PhysMap,
        virt: VirtAddr,
    ) -> Result<(), DmaError> {
        let record = self
            .allocations
            .get(&virt.as_u64())
            .ok_or(DmaError::UnknownBuffer)?;
        let buf = DmaBuffer {
            virt,
            phys: record.start_frame.start(),
            len: record.data_pages * PAGE_SIZE,
        };
        self.free_inner(space, frames, phys, buf)
    }

    /// Look up `buf`'s live record and return its `(physical base, byte
    /// length)`. Returns [`DmaError::UnknownBuffer`] if the buffer is not
    /// live in this window.
    ///
    /// # Errors
    ///
    /// [`DmaError::UnknownBuffer`] if `buf` is not a live allocation.
    pub fn live_frames(&self, buf: &DmaBuffer) -> Result<(PhysAddr, usize), DmaError> {
        let record = self
            .allocations
            .get(&buf.virt.as_u64())
            .ok_or(DmaError::UnknownBuffer)?;
        Ok((record.start_frame.start(), record.data_pages * PAGE_SIZE))
    }

    /// Number of live allocations.
    #[must_use]
    pub fn live(&self) -> usize {
        self.allocations.len()
    }

    /// Total pages in the window.
    #[must_use]
    pub fn capacity_pages(&self) -> usize {
        self.capacity_pages
    }

    /// Reclaim **every** live DMA buffer from the borrowed `space`, zeroing
    /// each backing block (zero-on-free) before its frames
    /// return to `frames`.
    ///
    /// Best-effort teardown for [`crate::live::LiveSpace`]'s `Drop`: a driver
    /// task's exit must not leak the physical frames its DMA buffers held or
    /// leave their (possibly secret-bearing) contents recoverable. A
    /// per-buffer error on this path has no better recovery than dropping it
    /// (never a panic).
    pub fn drain_into<P: PageTable>(
        &mut self,
        space: &mut AddressSpace<P>,
        frames: &FrameAllocator,
        phys: &dyn PhysMap,
    ) {
        // Collect the live keys first so the free loop does not iterate the
        // map while `free_from` removes from it.
        let keys: Vec<u64> = self.allocations.keys().copied().collect();
        for virt in keys {
            let Some(record) = self.allocations.get(&virt) else {
                continue;
            };
            let buf = DmaBuffer {
                virt: VirtAddr::new(virt),
                phys: record.start_frame.start(),
                len: record.data_pages * PAGE_SIZE,
            };
            let _ = self.free_from(space, frames, phys, buf);
        }
    }

    /// The borrowed-space carve shared by [`Self::alloc_into`] and
    /// [`DmaPool::alloc`] (the single definition).
    fn alloc_inner<P: PageTable>(
        &mut self,
        space: &mut AddressSpace<P>,
        frames: &FrameAllocator,
        phys: &dyn PhysMap,
        requested: usize,
        addr_limit: u64,
    ) -> Result<DmaBuffer, DmaError> {
        if requested == 0 {
            return Err(DmaError::ZeroSize);
        }
        // Round up to a page count.
        let needed_pages = requested.div_ceil(PAGE_SIZE);
        // Round needed_pages up to the next power of two so we can
        // satisfy it with a single buddy-order allocation.
        let order = needed_pages.next_power_of_two().trailing_zeros();
        if order > MAX_ORDER {
            return Err(DmaError::SizeUnsupported);
        }
        let data_pages = 1usize << order;
        // We need `data_pages + 2` consecutive free slots (leading
        // guard + data + trailing guard).
        let block_pages = data_pages.checked_add(2).ok_or(DmaError::SizeUnsupported)?;
        let leading_guard_slot = self
            .find_free_run(block_pages)
            .ok_or(DmaError::Alloc(AllocError::OutOfMemory))?;
        let first_data_slot = leading_guard_slot + 1;
        let trailing_guard_slot = leading_guard_slot + 1 + data_pages;

        // Reserve frames *before* mutating the slot bitmap so a frame
        // OOM leaves the pool's state untouched.
        let start_frame = frames.alloc_order(order)?;

        // Enforce the granted device addressing limit: the whole contiguous block must lie below `addr_limit`
        // (when one is declared), or the device could be handed a buffer
        // it cannot reach — or one outside the region the kernel granted
        // it. A block that exceeds the limit is returned immediately and
        // the request refused fail-closed. The
        // block was just minted, so the free matches and cannot fail.
        if addr_limit != 0 {
            let data_len = (data_pages as u64) * PAGE_SIZE as u64;
            let block_end = start_frame.start().as_u64().checked_add(data_len);
            if block_end.map_or(true, |end| end > addr_limit) {
                let _ = frames.free_order(start_frame, order);
                return Err(DmaError::AddrLimitExceeded);
            }
        }

        // Map every data page. If any map fails, roll back the ones
        // we already mapped and return the frames to the allocator.
        for i in 0..data_pages {
            let virt = self.virt_of_slot(first_data_slot + i);
            let frame = Frame(start_frame.0 + i);
            let page = match Page::from_addr(virt) {
                Ok(p) => p,
                Err(e) => {
                    self.rollback_partial_map(
                        space,
                        frames,
                        first_data_slot,
                        i,
                        start_frame,
                        order,
                    );
                    return Err(DmaError::PageTable(e));
                }
            };
            if let Err(e) = space.map(
                page,
                frame,
                // The buffer is shared with a DMA-capable device, so it is
                // mapped coherent (`DMA_COHERENT`): on a non-I/O-coherent
                // platform (the BCM2711 PCIe root complex) the port maps it
                // Normal Non-Cacheable, so a descriptor the driver writes is
                // visible to the device — and an event the device writes is
                // visible to the driver — without per-access cache
                // maintenance the driver could not perform from EL0 anyway. On a coherent platform it is
                // ordinary cacheable RAM.
                MapFlags::READ | MapFlags::WRITE | MapFlags::USER | MapFlags::DMA_COHERENT,
            ) {
                self.rollback_partial_map(space, frames, first_data_slot, i, start_frame, order);
                return Err(DmaError::PageTable(e));
            }
        }

        // Zero the data region through the direct map so the caller
        // observes a clean buffer. The frames are mapped above and not
        // yet reachable by any other allocation, so a failure to reach
        // them is a platform-config bug: roll back and fail closed.
        let data_len = data_pages * PAGE_SIZE;
        // SAFETY: when `translate` succeeds it returns a pointer to
        // `data_len` bytes of the frames just mapped; no other live
        // allocation covers them (the slot run was free), so the slice
        // aliases nothing.
        let data = phys
            .translate(start_frame.start(), data_len)
            .and_then(|ptr| unsafe { slice_within(ptr.as_ptr(), data_len, 0, data_len) });
        let Some(data) = data else {
            self.rollback_partial_map(
                space,
                frames,
                first_data_slot,
                data_pages,
                start_frame,
                order,
            );
            return Err(DmaError::DirectMap);
        };
        for b in data.iter_mut() {
            *b = 0;
        }
        phys.clean_invalidate(start_frame.start(), data_len);

        // Mark every slot — guard and data alike — as used so no
        // future allocation can overlap them.
        for s in leading_guard_slot..=trailing_guard_slot {
            self.slot_used[s] = true;
        }

        let virt = self.virt_of_slot(first_data_slot);
        let phys_base = start_frame.start();
        let len = data_pages * PAGE_SIZE;
        self.allocations.insert(
            virt.as_u64(),
            Record {
                leading_guard_slot,
                data_pages,
                order,
                start_frame,
            },
        );
        Ok(DmaBuffer {
            virt,
            phys: phys_base,
            len,
        })
    }

    /// Free a previously-allocated DMA buffer.
    ///
    /// Every byte of the data region is zeroed (via the audited
    /// `zeroize` crate's volatile clear) *before* the backing frames
    /// are returned to the [`FrameAllocator`], so neither a later
    /// allocation nor a forensic dump of free memory can recover the
    /// credentials the buffer once held. The clear
    /// runs through the direct map, i.e. on the same physical frames
    /// the device used.
    ///
    /// # Errors
    ///
    /// * [`DmaError::UnknownBuffer`] — the buffer's `virt` is not the
    ///   start of a live allocation in this pool (covers double-free
    ///   and cross-pool free).
    /// * [`DmaError::DirectMap`] — the buffer's frames are outside the
    ///   direct physical map, so the zero-on-free clear cannot run
    ///   (a platform-config bug).
    /// * [`DmaError::PageTable`] — propagated from
    ///   [`AddressSpace::unmap`] (e.g. if a higher-level test removed
    ///   a mapping out of band).
    fn free_inner<P: PageTable>(
        &mut self,
        space: &mut AddressSpace<P>,
        frames: &FrameAllocator,
        phys: &dyn PhysMap,
        buf: DmaBuffer,
    ) -> Result<(), DmaError> {
        let record = self
            .allocations
            .remove(&buf.virt.as_u64())
            .ok_or(DmaError::UnknownBuffer)?;
        let data_pages = record.data_pages;
        let first_data_slot = record.leading_guard_slot + 1;
        let trailing_guard_slot = record.leading_guard_slot + 1 + data_pages;

        // 1. Zero the data region (zero-on-free) on the real frames,
        //    before they are unmapped or returned to the allocator.
        let data_len = data_pages * PAGE_SIZE;
        let ptr = phys
            .translate(record.start_frame.start(), data_len)
            .ok_or(DmaError::DirectMap)?;
        // SAFETY: the frames are still mapped and reserved (the slot
        // bitmap is cleared only below), so the pointer is exclusively
        // owned for `data_len` bytes. `zeroize` performs a volatile
        // write the compiler cannot elide.
        unsafe { slice_within(ptr.as_ptr(), data_len, 0, data_len) }
            .ok_or(DmaError::DirectMap)?
            .zeroize();
        phys.clean_invalidate(record.start_frame.start(), data_len);

        // 2. Unmap data pages.
        for i in 0..data_pages {
            let virt = self.virt_of_slot(first_data_slot + i);
            let page = Page::from_addr(virt)?;
            // `unmap` returns the frame that was mapped; we already
            // know it from `record.start_frame + i` so we discard the
            // returned value.
            let _ = space.unmap(page)?;
        }

        // 3. Return frames in one buddy-order operation.
        frames
            .free_order(record.start_frame, record.order)
            .map_err(DmaError::Alloc)?;

        // 4. Mark guard and data slots free again.
        for s in record.leading_guard_slot..=trailing_guard_slot {
            self.slot_used[s] = false;
        }
        Ok(())
    }

    fn virt_of_slot(&self, slot: usize) -> VirtAddr {
        VirtAddr::new(self.base.as_u64() + ((slot as u64) << PAGE_SHIFT))
    }

    /// First slot index of a run of `len` consecutive *unused* slots,
    /// or `None` if no such run exists.
    fn find_free_run(&self, len: usize) -> Option<usize> {
        if len == 0 || len > self.capacity_pages {
            return None;
        }
        let mut start = 0;
        while start + len <= self.capacity_pages {
            let mut ok = true;
            for i in 0..len {
                if self.slot_used[start + i] {
                    start = start + i + 1;
                    ok = false;
                    break;
                }
            }
            if ok {
                return Some(start);
            }
        }
        None
    }

    fn rollback_partial_map<P: PageTable>(
        &self,
        space: &mut AddressSpace<P>,
        frames: &FrameAllocator,
        first_data_slot: usize,
        mapped_so_far: usize,
        start_frame: Frame,
        order: u32,
    ) {
        // Best-effort: unmap any pages we already mapped. We
        // deliberately discard inner errors — we're already on the
        // error path, and the alternative (panicking) is forbidden
        // by.
        for i in 0..mapped_so_far {
            let virt = self.virt_of_slot(first_data_slot + i);
            if let Ok(page) = Page::from_addr(virt) {
                let _ = space.unmap(page);
            }
        }
        let _ = frames.free_order(start_frame, order);
    }
}

impl<'a, P: PageTable> DmaPool<'a, P> {
    /// Construct a new pool managing the virtual range
    /// `[base, base + capacity_pages * PAGE_SIZE)`.
    ///
    /// The pool maps frames from `frames` into the supplied
    /// [`AddressSpace`]. The borrow of `frames` is explicit (rather
    /// than `'static`) so the host-side tests do not need to leak
    /// their allocators and so a future per-process kernel layout can
    /// hand a process a `&FrameAllocator` carved from the global pool.
    ///
    /// `phys` is the kernel's direct physical map; the pool reaches a
    /// buffer's frames through it so the CPU sees exactly the bytes
    /// the device DMAs to.
    ///
    /// # Errors
    ///
    /// * [`DmaError::InvalidPoolConfig`] if `capacity_pages == 0`,
    ///   `base` is not page-aligned, or the window size overflows.
    pub fn new(
        address_space: AddressSpace<P>,
        base: VirtAddr,
        capacity_pages: usize,
        frames: &'a FrameAllocator,
        phys: &'a dyn PhysMap,
    ) -> Result<Self, DmaError> {
        let window = DmaWindowMap::new(base, capacity_pages)?;
        Ok(Self {
            address_space,
            window,
            phys,
            frames,
        })
    }

    /// Allocate a contiguous DMA region of at least `requested` bytes.
    ///
    /// The returned buffer's `len` is `requested` rounded up to the
    /// next power-of-two multiple of [`PAGE_SIZE`]. The in-kernel pool
    /// declares no device addressing constraint (the carve is bounded
    /// by the pool's own window), so it passes `addr_limit == 0`.
    ///
    /// # Errors
    ///
    /// * [`DmaError::ZeroSize`] — `requested == 0`.
    /// * [`DmaError::SizeUnsupported`] — `requested` would round to a
    ///   buddy order exceeding [`MAX_ORDER`].
    /// * [`DmaError::Alloc`]`(`[`AllocError::OutOfMemory`]`)` — no
    ///   contiguous frame block of the requested order is free, or no
    ///   suitable run of unused slots exists in the virtual window.
    /// * [`DmaError::PageTable`] — propagated from the
    ///   [`AddressSpace`] when a mapping operation fails.
    pub fn alloc(&mut self, requested: usize) -> Result<DmaBuffer, DmaError> {
        self.window.alloc_into(
            &mut self.address_space,
            self.frames,
            self.phys,
            requested,
            0,
        )
    }

    /// Free a previously-allocated DMA buffer.
    ///
    /// Every byte of the data region is zeroed (via the audited
    /// `zeroize` crate's volatile clear) *before* the backing frames
    /// are returned to the [`FrameAllocator`], so neither a later
    /// allocation nor a forensic dump of free memory can recover the
    /// credentials the buffer once held. The clear
    /// runs through the direct map, i.e. on the same physical frames
    /// the device used.
    ///
    /// # Errors
    ///
    /// * [`DmaError::UnknownBuffer`] — the buffer's `virt` is not the
    ///   start of a live allocation in this pool (covers double-free
    ///   and cross-pool free).
    /// * [`DmaError::DirectMap`] — the buffer's frames are outside the
    ///   direct physical map, so the zero-on-free clear cannot run
    ///   (a platform-config bug).
    /// * [`DmaError::PageTable`] — propagated from
    ///   [`AddressSpace::unmap`] (e.g. if a higher-level test removed
    ///   a mapping out of band).
    pub fn free(&mut self, buf: DmaBuffer) -> Result<(), DmaError> {
        self.window
            .free_from(&mut self.address_space, self.frames, self.phys, buf)
    }

    /// Borrow the data bytes of `buf` mutably.
    ///
    /// The slice points at the buffer's physical frames through the
    /// direct map, so writes are seen by the device and vice versa.
    ///
    /// # Errors
    ///
    /// * [`DmaError::UnknownBuffer`] if `buf` is not a live allocation
    ///   of this pool.
    /// * [`DmaError::DirectMap`] if the frames are outside the direct
    ///   physical map.
    pub fn bytes_mut(&mut self, buf: DmaBuffer) -> Result<&mut [u8], DmaError> {
        let (phys, len) = self.window.live_frames(&buf)?;
        let ptr = self.phys.translate(phys, len).ok_or(DmaError::DirectMap)?;
        // SAFETY: `translate` returned a pointer to `len` bytes of this
        // buffer's frames; the slot bitmap proves no other live
        // allocation covers them and `&mut self` makes the borrow
        // exclusive for the returned slice's lifetime.
        unsafe { slice_within(ptr.as_ptr(), len, 0, len) }.ok_or(DmaError::DirectMap)
    }

    /// Raw, non-null base pointer to the data slots of `buf`.
    ///
    /// Companion to [`Self::bytes_mut`] that hands out only a
    /// pointer (no slice borrow), so a future user-space-driver
    /// host shim can mint an owned `DmaSlab` carrying the pointer
    /// independently of the pool's mutable borrow. The disjointness
    /// witness is the pool's slot bitmap: one slot ↔ one
    /// allocation, so the bytes covered by `[ptr, ptr + buf.len())`
    /// alias nothing else the pool has minted.
    ///
    /// # Safety-invariant
    ///
    /// The returned pointer is valid for reads and writes of
    /// `buf.len()` bytes until the buffer is freed via
    /// [`Self::free`]. The caller (a future Stage 4.D Item 0
    /// `KernelVirtioHost`) must ensure the slab carrying this
    /// pointer is dropped strictly before
    /// [`Self::free`] is called for the same `buf`.
    ///
    /// # Errors
    ///
    /// [`DmaError::UnknownBuffer`] if `buf` does not name a live
    /// allocation of this pool.
    pub fn slot_base(&self, buf: &DmaBuffer) -> Result<NonNull<u8>, DmaError> {
        let (phys, len) = self.window.live_frames(buf)?;
        // The pointer names the buffer's physical frames through the
        // direct map. The slot bitmap proves no other live allocation
        // covers `[ptr, ptr + len)`, and every write through it is
        // gated by the slot's exclusive owner (the `DmaSlab` that
        // records the same buffer).
        self.phys.translate(phys, len).ok_or(DmaError::DirectMap)
    }

    /// Borrow the data bytes of `buf` immutably.
    ///
    /// # Errors
    ///
    /// See [`Self::bytes_mut`].
    pub fn bytes(&self, buf: DmaBuffer) -> Result<&[u8], DmaError> {
        let (phys, len) = self.window.live_frames(&buf)?;
        let ptr = self.phys.translate(phys, len).ok_or(DmaError::DirectMap)?;
        // SAFETY: as `bytes_mut`, but the shared borrow of `self`
        // yields a shared slice; the pool holds the single live record
        // for these bytes so no `&mut` alias exists. The base pointer
        // already covers `len` bytes, so no further arithmetic occurs.
        Ok(unsafe { core::slice::from_raw_parts(ptr.as_ptr().cast_const(), len) })
    }

    /// Number of live allocations.
    #[must_use]
    pub fn live(&self) -> usize {
        self.window.live()
    }

    /// Total pages in the pool's virtual window.
    #[must_use]
    pub fn capacity_pages(&self) -> usize {
        self.window.capacity_pages()
    }
}

#[cfg(all(test, not(loom)))]
mod tests;
