//! Per-process memory-mapped-I/O register-window mapper.
//!
//! A bus driver (`drivers/bus/pci`, `drivers/bus/mmio`) that has
//! discovered a device's register block needs that block mapped into
//! its address space before it can touch a single register. This
//! module owns the architecture-neutral half of that mapping: it
//! takes a device *physical* address range and installs a
//! caching-disabled mapping for it inside a per-process
//! [`AddressSpace<P>`], handing back an [`MmioRegion`] describing the
//! result.
//!
//! Unlike [`crate::dma::DmaPool`], this mapper does **not** allocate
//! frames from the [`crate::frame::FrameAllocator`]: the physical
//! address is fixed by the hardware (a PCI BAR, a virtio-MMIO slot),
//! so the mapper maps the *device's own* frames. It shares the rest
//! of the DMA pool's discipline:
//!
//! * Guard pages bracket every mapped window so a driver that walks
//!   off the end of a register block faults instead of poking a
//!   neighbouring device (`AGENTS.md` §4).
//! * Mapping failure is reported through a [`Result`]; no path panics
//!   (`AGENTS.md` §2.9).
//!
//! The capability check (`CapabilityId::MMIO_MAP`) lives in
//! `kernel/sec::mmio`, since `kernel/mem` deliberately depends on
//! neither `rustos-abi` nor `rustos-caps` (see `kernel/mem/Cargo.toml`).
//!
//! # Host model
//!
//! On real hardware the mapped bytes are the device's registers. The
//! host-testable model keeps a `Vec<u8>` backing for the window so
//! unit tests and host-side driver code can read and write through
//! [`MmioMap::region_base`] using the same pointer arithmetic
//! production code uses, exactly mirroring [`crate::dma::DmaPool`].

use alloc::collections::BTreeMap;
use alloc::vec;
use alloc::vec::Vec;
use core::fmt;
use core::ptr::NonNull;

use crate::frame::{Frame, PAGE_SHIFT, PAGE_SIZE};
use crate::vmm::{AddressSpace, MapFlags, Page, PageTableError, PageTableOps, VirtAddr};

/// Byte pattern painted into the host-side guard slots. Matches
/// [`crate::dma`]'s `GUARD_BYTE` so an operator who sees the value in
/// a dump recognises it as "this region is supposed to be untouched".
const GUARD_BYTE: u8 = 0xCC;

/// Errors specific to [`MmioMap`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum MmioError {
    /// The requested region is malformed: zero length, or a
    /// `phys_base + len` computation that overflows.
    InvalidRegion,
    /// No run of unused slots large enough for the window (plus its
    /// two guard slots) exists in the mapper's virtual window.
    NoVirtualSpace,
    /// The page-table layer rejected a map/unmap operation.
    PageTable(PageTableError),
    /// `unmap` was called with an [`MmioRegion`] that does not name a
    /// live mapping in this mapper.
    UnknownRegion,
    /// `unmap` detected that a guard slot's host-side pattern was
    /// disturbed — a register-block over-run was observed.
    GuardViolation,
    /// The mapper was constructed with an invalid request (zero
    /// capacity, or a virtual base that is not page aligned).
    InvalidMapConfig,
}

impl From<PageTableError> for MmioError {
    fn from(e: PageTableError) -> Self {
        Self::PageTable(e)
    }
}

impl fmt::Display for MmioError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidRegion => f.write_str("mmio region request is malformed"),
            Self::NoVirtualSpace => f.write_str("mmio mapper has no free virtual window"),
            Self::PageTable(e) => write!(f, "mmio page-table: {e:?}"),
            Self::UnknownRegion => f.write_str("mmio region not from this mapper"),
            Self::GuardViolation => f.write_str("mmio guard-slot violation"),
            Self::InvalidMapConfig => f.write_str("mmio mapper config invalid"),
        }
    }
}

/// A live MMIO mapping handed out by [`MmioMap::map`].
///
/// Like [`crate::dma::DmaBuffer`] this is **not** [`Drop`]: a
/// forgotten region must be a hard error, not a silent leak. Callers
/// release explicitly via [`MmioMap::unmap`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct MmioRegion {
    virt: VirtAddr,
    phys: u64,
    len: usize,
}

impl MmioRegion {
    /// CPU-side virtual address of the first byte of the register
    /// block. Equal to the page-aligned slot base plus the original
    /// `phys_base`'s within-page offset.
    #[must_use]
    pub fn virt(self) -> VirtAddr {
        self.virt
    }

    /// Device-visible physical base address of the register block.
    #[must_use]
    pub fn phys(self) -> u64 {
        self.phys
    }

    /// Length of the register block in bytes.
    #[must_use]
    pub fn len(self) -> usize {
        self.len
    }

    /// `true` iff the region is zero bytes long. Cannot occur in
    /// practice ([`MmioMap::map`] rejects zero-length requests);
    /// present for the `clippy::len_without_is_empty` lint.
    #[must_use]
    pub fn is_empty(self) -> bool {
        self.len == 0
    }
}

/// Per-live-mapping bookkeeping retained by the mapper.
#[derive(Debug, Clone, Copy)]
struct Record {
    /// Index of the leading guard slot.
    leading_guard_slot: usize,
    /// Number of mapped data pages.
    data_pages: usize,
    /// Offset of `phys_base` within its containing page.
    page_offset: usize,
}

/// Per-process MMIO register-window mapper.
///
/// Generic over [`PageTableOps`] so the same code is exercised by
/// `crate::HostPageTable` in unit tests and driven by the
/// architecture page-table types in production.
pub struct MmioMap<P: PageTableOps> {
    address_space: AddressSpace<P>,
    base: VirtAddr,
    capacity_pages: usize,
    slot_used: Vec<bool>,
    regions: BTreeMap<u64, Record>,
    /// Host-side backing for the window bytes. Over-allocated by one
    /// page and offset by [`Self::align_pad`] so the logical base is
    /// page-aligned; a `RegisterWindow` (`lib/abi`) minted over
    /// `region_base` then satisfies its ≥ 4-byte alignment contract
    /// for word-wide volatile accesses in the host model. On hardware
    /// the backing is the device's own mapped frames.
    storage: Vec<u8>,
    /// Byte offset within `storage` at which the page-aligned logical
    /// window begins.
    align_pad: usize,
}

impl<P: PageTableOps> MmioMap<P> {
    /// Construct a mapper managing the virtual range
    /// `[base, base + capacity_pages * PAGE_SIZE)`.
    ///
    /// # Errors
    ///
    /// [`MmioError::InvalidMapConfig`] if `capacity_pages == 0` or
    /// `base` is not page-aligned.
    pub fn new(
        address_space: AddressSpace<P>,
        base: VirtAddr,
        capacity_pages: usize,
    ) -> Result<Self, MmioError> {
        if capacity_pages == 0 || !base.is_page_aligned() {
            return Err(MmioError::InvalidMapConfig);
        }
        let bytes = capacity_pages
            .checked_mul(PAGE_SIZE)
            .ok_or(MmioError::InvalidMapConfig)?;
        // Over-allocate by one page so the logical window can begin at
        // a page-aligned offset regardless of the allocator's base
        // alignment.
        let raw = bytes
            .checked_add(PAGE_SIZE)
            .ok_or(MmioError::InvalidMapConfig)?;
        let storage = vec![GUARD_BYTE; raw];
        let align_pad = storage.as_ptr().align_offset(PAGE_SIZE);
        if align_pad > PAGE_SIZE {
            // `align_offset` only fails (returns `usize::MAX`) when the
            // requested alignment is unreachable; the extra page makes
            // that impossible, but fail closed rather than index out
            // of range (`AGENTS.md` §2.9).
            return Err(MmioError::InvalidMapConfig);
        }
        Ok(Self {
            address_space,
            base,
            capacity_pages,
            slot_used: vec![false; capacity_pages],
            regions: BTreeMap::new(),
            storage,
            align_pad,
        })
    }

    /// Map `len` bytes of device physical memory beginning at
    /// `phys_base`, returning a guard-bracketed [`MmioRegion`].
    ///
    /// # Errors
    ///
    /// * [`MmioError::InvalidRegion`] — `len == 0`, or
    ///   `phys_base + page_offset + len` overflows.
    /// * [`MmioError::NoVirtualSpace`] — no run of free slots large
    ///   enough exists.
    /// * [`MmioError::PageTable`] — propagated from the
    ///   [`AddressSpace`] when a mapping operation fails.
    pub fn map(&mut self, phys_base: u64, len: usize) -> Result<MmioRegion, MmioError> {
        if len == 0 {
            return Err(MmioError::InvalidRegion);
        }
        // The within-page offset is always `< PAGE_SIZE`, so it fits a
        // `usize` on every target; mask in `u64` then convert without
        // a lossy `as` cast.
        let page_offset = usize::try_from(phys_base & (PAGE_SIZE as u64 - 1))
            .map_err(|_| MmioError::InvalidRegion)?;
        let span = page_offset
            .checked_add(len)
            .ok_or(MmioError::InvalidRegion)?;
        let data_pages = span.div_ceil(PAGE_SIZE);
        // Validate `phys_base + len` does not overflow the physical
        // address space before we commit any state.
        let len_u64 = u64::try_from(len).map_err(|_| MmioError::InvalidRegion)?;
        phys_base
            .checked_add(len_u64)
            .ok_or(MmioError::InvalidRegion)?;
        let block_pages = data_pages.checked_add(2).ok_or(MmioError::NoVirtualSpace)?;
        let leading_guard_slot = self
            .find_free_run(block_pages)
            .ok_or(MmioError::NoVirtualSpace)?;
        let first_data_slot = leading_guard_slot + 1;
        let trailing_guard_slot = leading_guard_slot + 1 + data_pages;

        // Frame number = phys_base >> PAGE_SHIFT, converted without a
        // lossy `as` cast (a 32-bit target cannot address frames whose
        // number overflows `usize`).
        let start_frame_index =
            usize::try_from(phys_base >> PAGE_SHIFT).map_err(|_| MmioError::InvalidRegion)?;
        let start_frame = Frame(start_frame_index);

        for i in 0..data_pages {
            let virt = self.virt_of_slot(first_data_slot + i);
            let frame = Frame(start_frame.0 + i);
            let page = match Page::from_addr(virt) {
                Ok(p) => p,
                Err(e) => {
                    self.rollback_partial_map(first_data_slot, i);
                    return Err(MmioError::PageTable(e));
                }
            };
            if let Err(e) = self.address_space.map(
                page,
                frame,
                MapFlags::READ | MapFlags::WRITE | MapFlags::NO_CACHE | MapFlags::USER,
            ) {
                self.rollback_partial_map(first_data_slot, i);
                return Err(MmioError::PageTable(e));
            }
        }

        // Initialise the host-side data bytes to zero so a freshly
        // mapped window reads deterministically in the test model.
        let data_byte_start = self.align_pad + first_data_slot * PAGE_SIZE;
        let data_byte_end = data_byte_start + data_pages * PAGE_SIZE;
        for b in &mut self.storage[data_byte_start..data_byte_end] {
            *b = 0;
        }

        for s in leading_guard_slot..=trailing_guard_slot {
            self.slot_used[s] = true;
        }

        let window_virt =
            VirtAddr::new(self.virt_of_slot(first_data_slot).as_u64() + page_offset as u64);
        self.regions.insert(
            window_virt.as_u64(),
            Record {
                leading_guard_slot,
                data_pages,
                page_offset,
            },
        );
        Ok(MmioRegion {
            virt: window_virt,
            phys: phys_base,
            len,
        })
    }

    /// Tear down a mapping previously returned by [`Self::map`].
    ///
    /// # Errors
    ///
    /// * [`MmioError::UnknownRegion`] — `region` is not a live
    ///   mapping of this mapper (covers double-unmap).
    /// * [`MmioError::GuardViolation`] — a guard slot's host-side
    ///   pattern was disturbed while the mapping was live.
    /// * [`MmioError::PageTable`] — propagated from
    ///   [`AddressSpace::unmap`].
    pub fn unmap(&mut self, region: MmioRegion) -> Result<(), MmioError> {
        let record = self
            .regions
            .remove(&region.virt.as_u64())
            .ok_or(MmioError::UnknownRegion)?;
        let data_pages = record.data_pages;
        let first_data_slot = record.leading_guard_slot + 1;
        let trailing_guard_slot = record.leading_guard_slot + 1 + data_pages;

        let guard_ok = self.guards_intact(record.leading_guard_slot, trailing_guard_slot);

        for i in 0..data_pages {
            let virt = self.virt_of_slot(first_data_slot + i);
            let page = Page::from_addr(virt)?;
            let _ = self.address_space.unmap(page)?;
        }

        for s in record.leading_guard_slot..=trailing_guard_slot {
            self.slot_used[s] = false;
        }
        let repaint_start = self.align_pad + record.leading_guard_slot * PAGE_SIZE;
        let repaint_end = self.align_pad + (trailing_guard_slot + 1) * PAGE_SIZE;
        for b in &mut self.storage[repaint_start..repaint_end] {
            *b = GUARD_BYTE;
        }

        if guard_ok {
            Ok(())
        } else {
            Err(MmioError::GuardViolation)
        }
    }

    /// Raw, non-null base pointer to the first register of `region`.
    ///
    /// The pointer is valid for reads and writes of `region.len()`
    /// bytes until the region is released via [`Self::unmap`]. The
    /// caller (the kernel-host `MmioMapper` impl) mints a
    /// `RegisterWindow` carrying this pointer and must ensure the
    /// window is dropped before `unmap` is called for the same
    /// region.
    ///
    /// # Errors
    ///
    /// [`MmioError::UnknownRegion`] if `region` does not name a live
    /// mapping of this mapper.
    pub fn region_base(&self, region: &MmioRegion) -> Result<NonNull<u8>, MmioError> {
        let record = self
            .regions
            .get(&region.virt.as_u64())
            .ok_or(MmioError::UnknownRegion)?;
        let first_data_slot = record.leading_guard_slot + 1;
        let start = self.align_pad + first_data_slot * PAGE_SIZE + record.page_offset;
        let base = self.storage.as_ptr().wrapping_add(start).cast_mut();
        NonNull::new(base).ok_or(MmioError::InvalidMapConfig)
    }

    /// Number of live mappings.
    #[must_use]
    pub fn live(&self) -> usize {
        self.regions.len()
    }

    /// Total pages in the mapper's virtual window.
    #[must_use]
    pub fn capacity_pages(&self) -> usize {
        self.capacity_pages
    }

    /// Number of currently-mapped pages in the underlying address
    /// space (data pages only; guard slots are never mapped).
    #[must_use]
    pub fn mapped_pages(&self) -> usize {
        self.address_space.mapped_pages()
    }

    /// Test trapdoor: write `byte` to the host-side storage at
    /// `offset` from the mapper's `base`, simulating a register-block
    /// over-run that bleeds into a guard slot.
    ///
    /// Returns `None` if `offset` is outside the mapper window.
    #[cfg(test)]
    pub(crate) fn poke_for_test(&mut self, offset: usize, byte: u8) -> Option<()> {
        let idx = self.align_pad.checked_add(offset)?;
        let b = self.storage.get_mut(idx)?;
        *b = byte;
        Some(())
    }

    // -----------------------------------------------------------------
    // Internal helpers
    // -----------------------------------------------------------------

    fn virt_of_slot(&self, slot: usize) -> VirtAddr {
        VirtAddr::new(self.base.as_u64() + ((slot as u64) << PAGE_SHIFT))
    }

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

    fn rollback_partial_map(&mut self, first_data_slot: usize, mapped_so_far: usize) {
        for i in 0..mapped_so_far {
            let virt = self.virt_of_slot(first_data_slot + i);
            if let Ok(page) = Page::from_addr(virt) {
                let _ = self.address_space.unmap(page);
            }
        }
    }

    fn guards_intact(&self, leading_guard_slot: usize, trailing_guard_slot: usize) -> bool {
        let head_start = self.align_pad + leading_guard_slot * PAGE_SIZE;
        let head_end = head_start + PAGE_SIZE;
        let tail_start = self.align_pad + trailing_guard_slot * PAGE_SIZE;
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
