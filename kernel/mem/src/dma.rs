//! Per-process-heap DMA allocator.
//!
//! `AGENTS.md` §4 requires every kernel-side DMA facility to:
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
//! **left unmapped** in the [`AddressSpace`] so that a real MMU faults
//! on access; the host-testable model additionally paints the
//! storage-backed guard regions with [`GUARD_BYTE`] and verifies them
//! at free time, exactly mirroring the convention used by
//! [`crate::slab`].
//!
//! # Zero-on-free
//!
//! [`DmaPool::free`] zeroes every byte of the data region through a
//! volatile-clear primitive (delegated to the `zeroize` crate per
//! `AGENTS.md` §6) **before** the frames are returned to the
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
use crate::vmm::{AddressSpace, MapFlags, Page, PageTableError, PageTableOps, VirtAddr};

/// Byte pattern painted into the host-side guard slots. Matches
/// [`crate::slab`]'s `GUARD_BYTE` so an operator who sees the value in
/// a dump recognises it as "this region is supposed to be untouched".
const GUARD_BYTE: u8 = 0xCC;

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
    /// `free` detected that a guard slot's host-side pattern was
    /// disturbed — a buffer over-run was observed. On real hardware
    /// the MMU would have already faulted; the host model surfaces it
    /// through this variant so the same call sites can be tested
    /// everywhere.
    GuardViolation,
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
            Self::GuardViolation => f.write_str("dma guard-slot violation"),
            Self::InvalidPoolConfig => f.write_str("dma pool config invalid"),
            Self::SizeUnsupported => f.write_str("dma request exceeds max buddy order"),
            Self::ZeroSize => f.write_str("zero-sized dma allocation is not permitted"),
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

/// Per-process DMA pool.
///
/// `DmaPool<P>` is generic over a [`PageTableOps`] implementation so
/// it can be exercised by [`crate::HostPageTable`] in unit tests and
/// driven by the architecture page-table types from `kernel/arch/*` in
/// production.
///
/// One pool is intended to live per process that holds
/// `CapabilityId::MEM_DMA`; the capability check itself lives in
/// `kernel/sec::dma` so this crate stays free of the `rustos-abi`
/// dependency.
pub struct DmaPool<'a, P: PageTableOps> {
    address_space: AddressSpace<P>,
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
    /// Host-side backing storage for the pool's virtual window. On
    /// real hardware the bytes live in the per-process page frames;
    /// here we keep them in a `Vec<u8>` so the unit tests can read
    /// and write through the same indexing that production code will
    /// use. The pool maps frames into the `AddressSpace` so the
    /// page-table accounting is exercised identically in both worlds.
    storage: Vec<u8>,
    /// Borrowed frame allocator used to back the data slots.
    ///
    /// `FrameAllocator` is internally synchronised via a
    /// [`rustos_kernel_sync::SpinLock`], so multiple pools may share
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

impl<'a, P: PageTableOps> DmaPool<'a, P> {
    /// Construct a new pool managing the virtual range
    /// `[base, base + capacity_pages * PAGE_SIZE)`.
    ///
    /// The pool maps frames from `frames` into the supplied
    /// [`AddressSpace`]. The borrow of `frames` is explicit (rather
    /// than `'static`) so the host-side tests do not need to leak
    /// their allocators and so a future per-process kernel layout can
    /// hand a process a `&FrameAllocator` carved from the global pool.
    ///
    /// # Errors
    ///
    /// * [`DmaError::InvalidPoolConfig`] if `capacity_pages == 0` or
    ///   `base` is not page-aligned.
    pub fn new(
        address_space: AddressSpace<P>,
        base: VirtAddr,
        capacity_pages: usize,
        frames: &'a FrameAllocator,
    ) -> Result<Self, DmaError> {
        if capacity_pages == 0 || !base.is_page_aligned() {
            return Err(DmaError::InvalidPoolConfig);
        }
        let bytes = capacity_pages
            .checked_mul(PAGE_SIZE)
            .ok_or(DmaError::InvalidPoolConfig)?;
        // Initialise the entire window to the guard byte so any slot
        // that is *not* part of a live data region matches the
        // verification pattern. Data slots are zeroed when they
        // transition from guard to live.
        let storage = vec![GUARD_BYTE; bytes];
        Ok(Self {
            address_space,
            base,
            capacity_pages,
            slot_used: vec![false; capacity_pages],
            allocations: BTreeMap::new(),
            storage,
            frames,
        })
    }

    /// Allocate a contiguous DMA region of at least `requested` bytes.
    ///
    /// The returned buffer's `len` is `requested` rounded up to the
    /// next power-of-two multiple of [`PAGE_SIZE`].
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
        let start_frame = self.frames.alloc_order(order)?;

        // Map every data page. If any map fails, roll back the ones
        // we already mapped and return the frames to the allocator.
        for i in 0..data_pages {
            let virt = self.virt_of_slot(first_data_slot + i);
            let frame = Frame(start_frame.0 + i);
            let page = match Page::from_addr(virt) {
                Ok(p) => p,
                Err(e) => {
                    self.rollback_partial_map(first_data_slot, i, start_frame, order);
                    return Err(DmaError::PageTable(e));
                }
            };
            if let Err(e) = self.address_space.map(
                page,
                frame,
                MapFlags::READ | MapFlags::WRITE | MapFlags::USER,
            ) {
                self.rollback_partial_map(first_data_slot, i, start_frame, order);
                return Err(DmaError::PageTable(e));
            }
        }

        // Zero the data slots in the host-side storage so the caller
        // observes a clean buffer (defence in depth; the storage was
        // painted with GUARD_BYTE on pool construction).
        let data_byte_start = first_data_slot * PAGE_SIZE;
        let data_byte_end = data_byte_start + data_pages * PAGE_SIZE;
        for b in &mut self.storage[data_byte_start..data_byte_end] {
            *b = 0;
        }

        // Mark every slot — guard and data alike — as used so no
        // future allocation can overlap them.
        for s in leading_guard_slot..=trailing_guard_slot {
            self.slot_used[s] = true;
        }

        let virt = self.virt_of_slot(first_data_slot);
        let phys = start_frame.start();
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
        Ok(DmaBuffer { virt, phys, len })
    }

    /// Free a previously-allocated DMA buffer.
    ///
    /// Every byte of the data region is zeroed (via the audited
    /// `zeroize` crate's volatile clear) *before* the backing frames
    /// are returned to the [`FrameAllocator`]. Both guard slots are
    /// validated against the [`GUARD_BYTE`] pattern; a mismatch is
    /// reported as [`DmaError::GuardViolation`] *after* the frames
    /// have nonetheless been recovered, because a guard violation
    /// indicates a programming bug, not a reason to leak memory.
    ///
    /// # Errors
    ///
    /// * [`DmaError::UnknownBuffer`] — the buffer's `virt` is not the
    ///   start of a live allocation in this pool (covers double-free
    ///   and cross-pool free).
    /// * [`DmaError::GuardViolation`] — a guard slot's host-side
    ///   pattern was disturbed while the allocation was live.
    /// * [`DmaError::PageTable`] — propagated from
    ///   [`AddressSpace::unmap`] (e.g. if a higher-level test removed
    ///   a mapping out of band).
    pub fn free(&mut self, buf: DmaBuffer) -> Result<(), DmaError> {
        let record = self
            .allocations
            .remove(&buf.virt.as_u64())
            .ok_or(DmaError::UnknownBuffer)?;
        let data_pages = record.data_pages;
        let first_data_slot = record.leading_guard_slot + 1;
        let trailing_guard_slot = record.leading_guard_slot + 1 + data_pages;

        // 1. Zero the data region (zero-on-free).
        let start = first_data_slot * PAGE_SIZE;
        let end = start + data_pages * PAGE_SIZE;
        // `zeroize` performs a volatile write so the compiler cannot
        // elide the clear (the very reason we depend on the crate).
        self.storage[start..end].zeroize();

        // 2. Verify guard regions. Capture the result but continue
        //    teardown so the frames are not leaked even on a guard
        //    violation.
        let guard_ok = self.guards_intact(record.leading_guard_slot, trailing_guard_slot);

        // 3. Unmap data pages.
        for i in 0..data_pages {
            let virt = self.virt_of_slot(first_data_slot + i);
            let page = Page::from_addr(virt)?;
            // `unmap` returns the frame that was mapped; we already
            // know it from `record.start_frame + i` so we discard the
            // returned value.
            let _ = self.address_space.unmap(page)?;
        }

        // 4. Return frames in one buddy-order operation.
        self.frames
            .free_order(record.start_frame, record.order)
            .map_err(DmaError::Alloc)?;

        // 5. Mark slots free and repaint with the guard byte so
        //    future guard checks remain sound on this range.
        for s in record.leading_guard_slot..=trailing_guard_slot {
            self.slot_used[s] = false;
        }
        let repaint_start = record.leading_guard_slot * PAGE_SIZE;
        let repaint_end = (trailing_guard_slot + 1) * PAGE_SIZE;
        for b in &mut self.storage[repaint_start..repaint_end] {
            *b = GUARD_BYTE;
        }

        if guard_ok {
            Ok(())
        } else {
            Err(DmaError::GuardViolation)
        }
    }

    /// Borrow the data bytes of `buf` mutably.
    ///
    /// Production callers reach the data through the buffer's `virt`
    /// pointer directly; this accessor exists so unit tests and
    /// host-side driver code can read/write the backing storage
    /// without resorting to `unsafe`.
    ///
    /// # Errors
    ///
    /// [`DmaError::UnknownBuffer`] if `buf` is not a live allocation
    /// of this pool.
    pub fn bytes_mut(&mut self, buf: DmaBuffer) -> Result<&mut [u8], DmaError> {
        let record = self
            .allocations
            .get(&buf.virt.as_u64())
            .ok_or(DmaError::UnknownBuffer)?;
        let first_data_slot = record.leading_guard_slot + 1;
        let start = first_data_slot * PAGE_SIZE;
        let end = start + record.data_pages * PAGE_SIZE;
        Ok(&mut self.storage[start..end])
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
        let record = self
            .allocations
            .get(&buf.virt.as_u64())
            .ok_or(DmaError::UnknownBuffer)?;
        let first_data_slot = record.leading_guard_slot + 1;
        let start = first_data_slot * PAGE_SIZE;
        // The host-side storage is a `Vec<u8>` whose buffer is
        // stable for the lifetime of the pool (see [`Self::new`]).
        // Casting the immutable raw pointer to `*mut u8` is sound
        // because (i) the slot bitmap proves no other reference
        // covers `[start, start + buf.len())`, and (ii) every
        // future write through this pointer is gated by the
        // slot's exclusive owner (the `DmaSlab` that records the
        // same `slot` index).
        let base = self.storage.as_ptr().wrapping_add(start).cast_mut();
        NonNull::new(base).ok_or(DmaError::InvalidPoolConfig)
    }

    /// Borrow the data bytes of `buf` immutably.
    ///
    /// # Errors
    ///
    /// See [`Self::bytes_mut`].
    pub fn bytes(&self, buf: DmaBuffer) -> Result<&[u8], DmaError> {
        let record = self
            .allocations
            .get(&buf.virt.as_u64())
            .ok_or(DmaError::UnknownBuffer)?;
        let first_data_slot = record.leading_guard_slot + 1;
        let start = first_data_slot * PAGE_SIZE;
        let end = start + record.data_pages * PAGE_SIZE;
        Ok(&self.storage[start..end])
    }

    /// Number of live allocations.
    #[must_use]
    pub fn live(&self) -> usize {
        self.allocations.len()
    }

    /// Total pages in the pool's virtual window.
    #[must_use]
    pub fn capacity_pages(&self) -> usize {
        self.capacity_pages
    }

    /// Test trapdoor: write `byte` to the host-side storage at
    /// `offset` from the pool's `base`. Used by unit tests to
    /// *simulate* a buffer over-run that bleeds into a guard slot.
    /// Production callers must never need this.
    ///
    /// Returns `None` if `offset` is outside the pool window.
    #[cfg(test)]
    pub(crate) fn poke_for_test(&mut self, offset: usize, byte: u8) -> Option<()> {
        let b = self.storage.get_mut(offset)?;
        *b = byte;
        Some(())
    }

    // -----------------------------------------------------------------
    // Internal helpers
    // -----------------------------------------------------------------

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

    fn rollback_partial_map(
        &mut self,
        first_data_slot: usize,
        mapped_so_far: usize,
        start_frame: Frame,
        order: u32,
    ) {
        // Best-effort: unmap any pages we already mapped. We
        // deliberately discard inner errors — we're already on the
        // error path, and the alternative (panicking) is forbidden
        // by `AGENTS.md` §2.9.
        for i in 0..mapped_so_far {
            let virt = self.virt_of_slot(first_data_slot + i);
            if let Ok(page) = Page::from_addr(virt) {
                let _ = self.address_space.unmap(page);
            }
        }
        let _ = self.frames.free_order(start_frame, order);
    }

    fn guards_intact(&self, leading_guard_slot: usize, trailing_guard_slot: usize) -> bool {
        let head_start = leading_guard_slot * PAGE_SIZE;
        let head_end = head_start + PAGE_SIZE;
        let tail_start = trailing_guard_slot * PAGE_SIZE;
        let tail_end = tail_start + PAGE_SIZE;
        self.storage[head_start..head_end]
            .iter()
            .all(|&b| b == GUARD_BYTE)
            && self.storage[tail_start..tail_end]
                .iter()
                .all(|&b| b == GUARD_BYTE)
    }
}

#[cfg(all(test, not(loom)))]
mod tests;
