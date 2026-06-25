//! Per-process memory-mapped-I/O register-window mapper.
//!
//! A bus driver (`lib/pci`, `drivers/bus/mmio`) that has
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
//!   neighbouring device.
//! * Mapping failure is reported through a [`Result`]; no path panics.
//!
//! The capability check (`CapabilityId::MMIO_MAP`) lives in
//! `kernel/sec::mmio`, since `kernel/mem` deliberately depends on
//! neither `rustos-abi` nor `rustos-caps` (see `kernel/mem/Cargo.toml`).
//!
//! # CPU access
//!
//! On real hardware the mapped bytes are the device's registers,
//! reachable from the CPU through the kernel's direct physical map
//! ([`crate::phys::PhysMap`]). [`MmioMap::region_base`] translates the
//! region's device physical base into a pointer, so the
//! `RegisterWindow` a driver touches addresses the device's own
//! registers — in production via the boot identity map, in host tests
//! via a `SimPhysMap` standing in for the register
//! block, exactly mirroring [`crate::dma::DmaPool`].

use alloc::collections::BTreeMap;
use alloc::vec;
use alloc::vec::Vec;
use core::fmt;
use core::ptr::NonNull;

use crate::frame::{Frame, PhysAddr, PAGE_SHIFT, PAGE_SIZE};
use crate::phys::PhysMap;
use crate::vmm::{AddressSpace, MapFlags, Page, PageTable, PageTableError, VirtAddr};

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
    /// A region's physical frames fall outside the kernel's direct
    /// physical map, so the CPU cannot reach the registers. Indicates
    /// a mis-sized [`crate::phys::PhysMap`] for the platform; the
    /// mapper fails closed.
    DirectMap,
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
            Self::DirectMap => f.write_str("mmio region outside the direct physical map"),
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
}

/// Per-task guard-bracketed MMIO virtual-window allocator, **independent of
/// the address space it maps into**.
///
/// This owns only the per-task bookkeeping a sequence of MMIO mappings needs
/// — the virtual window the mappings live in, the slot occupancy bitmap, and
/// the per-region guard/data accounting — and never an [`AddressSpace`]. Every
/// page-table mutation is performed against a **borrowed** `&mut
/// AddressSpace<P>` the caller passes in, so the same guarded mapping logic
/// serves two consumers without duplication:
///
/// * [`MmioMap`], which bundles a `MmioWindowMap` with an address space it
///   *owns* (the in-kernel driver-host register-window mapper); and
/// * the `mmio_map` syscall facility (`plans/PI.md` P10 chunk 5d-0), which maps
///   a granted device window into the **caller's own running** address space —
///   a space the facility borrows but does not own.
///
/// It is the device-window analogue of [`crate::anon::map_anonymous`]: an
/// architecture-neutral mechanism over a borrowed live `AddressSpace<P>`, with
/// the capability posture, the choice of virtual window, and the lifecycle
/// owned by the higher-level caller. Unlike the anonymous
/// mapper it allocates **no** frames — the physical address is fixed by the
/// hardware (a PCI BAR, a virtio-MMIO slot) — and it brackets every window with
/// unmapped guard pages so a driver that walks off a register block faults
/// instead of poking a neighbouring device.
pub struct MmioWindowMap {
    base: VirtAddr,
    capacity_pages: usize,
    slot_used: Vec<bool>,
    regions: BTreeMap<u64, Record>,
}

impl MmioWindowMap {
    /// Construct an allocator managing the virtual range
    /// `[base, base + capacity_pages * PAGE_SIZE)`.
    ///
    /// # Errors
    ///
    /// [`MmioError::InvalidMapConfig`] if `capacity_pages == 0`,
    /// `base` is not page-aligned, or the window size overflows.
    pub fn new(base: VirtAddr, capacity_pages: usize) -> Result<Self, MmioError> {
        if capacity_pages == 0 || !base.is_page_aligned() {
            return Err(MmioError::InvalidMapConfig);
        }
        // Reject a window whose byte span would overflow the slot
        // offset arithmetic in `virt_of_slot` before committing state.
        capacity_pages
            .checked_mul(PAGE_SIZE)
            .ok_or(MmioError::InvalidMapConfig)?;
        Ok(Self {
            base,
            capacity_pages,
            slot_used: vec![false; capacity_pages],
            regions: BTreeMap::new(),
        })
    }

    /// Map `len` bytes of device physical memory beginning at `phys_base`
    /// into the borrowed `space`, returning a guard-bracketed [`MmioRegion`].
    ///
    /// The window is mapped `READ | WRITE | NO_CACHE | USER` — caching
    /// disabled (these are device registers, not RAM) and **never** executable
    /// (W^X for a register window). The map is
    /// all-or-nothing: a page-table failure part-way unwinds every page this
    /// call mapped before returning, leaving `space` unchanged.
    ///
    /// # Errors
    ///
    /// * [`MmioError::InvalidRegion`] — `len == 0`, or
    ///   `phys_base + page_offset + len` overflows.
    /// * [`MmioError::NoVirtualSpace`] — no run of free slots large
    ///   enough exists.
    /// * [`MmioError::PageTable`] — propagated from the
    ///   [`AddressSpace`] when a mapping operation fails.
    pub fn map_into<P: PageTable>(
        &mut self,
        space: &mut AddressSpace<P>,
        phys_base: u64,
        len: usize,
    ) -> Result<MmioRegion, MmioError> {
        // Device registers: caching disabled, never executable.
        self.map_with_flags(
            space,
            phys_base,
            len,
            MapFlags::READ | MapFlags::WRITE | MapFlags::NO_CACHE | MapFlags::USER,
        )
    }

    /// Map `len` bytes beginning at `phys_base` into the borrowed `space` as
    /// **cacheable** `RW|USER` (normal, write-back) memory, guard-bracketed,
    /// returning the [`MmioRegion`].
    ///
    /// Identical guard-bracketed slot mechanism as [`Self::map_into`], but
    /// the data pages are mapped cacheable rather than device-strongly-
    /// ordered, because the physical frames are ordinary kernel RAM (a
    /// cross-process shared-memory region), not device registers. The frames
    /// are **not** owned by this allocator: they were allocated elsewhere
    /// (the shared-region registry) and this only installs page-table
    /// entries for them, so [`Self::unmap_at`] / a space drop releases only
    /// the mapping, never the frames. Like [`Self::map_into`] it writes
    /// nothing to the frames (the registry zeroed them on allocation).
    ///
    /// # Errors
    ///
    /// The same set as [`Self::map_into`].
    pub fn map_cacheable_into<P: PageTable>(
        &mut self,
        space: &mut AddressSpace<P>,
        phys_base: u64,
        len: usize,
    ) -> Result<MmioRegion, MmioError> {
        self.map_with_flags(
            space,
            phys_base,
            len,
            MapFlags::READ | MapFlags::WRITE | MapFlags::USER,
        )
    }

    /// The shared guard-bracketed mapping mechanism behind [`Self::map_into`]
    /// (device, caching-disabled) and [`Self::map_cacheable_into`] (shared
    /// RAM, cacheable): the only difference between the two is the data-page
    /// `data_flags`, so there is one definition of the slot/guard/rollback
    /// logic, never two.
    fn map_with_flags<P: PageTable>(
        &mut self,
        space: &mut AddressSpace<P>,
        phys_base: u64,
        len: usize,
        data_flags: MapFlags,
    ) -> Result<MmioRegion, MmioError> {
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
                    self.rollback_partial_map(space, first_data_slot, i);
                    return Err(MmioError::PageTable(e));
                }
            };
            if let Err(e) = space.map(page, frame, data_flags) {
                self.rollback_partial_map(space, first_data_slot, i);
                return Err(MmioError::PageTable(e));
            }
        }

        // No zeroing here: the mapper does not own these frames. For a
        // device window they are the device's own registers (writing them
        // on map would clobber live hardware state); for a shared-memory
        // region the owning registry already zeroed the frames on
        // allocation. The mapper only installs page-table entries.
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
            },
        );
        Ok(MmioRegion {
            virt: window_virt,
            phys: phys_base,
            len,
        })
    }

    /// Tear down a mapping previously returned by [`Self::map_into`] from the
    /// borrowed `space`.
    ///
    /// # Errors
    ///
    /// * [`MmioError::UnknownRegion`] — `region` is not a live
    ///   mapping of this allocator (covers double-unmap).
    /// * [`MmioError::PageTable`] — propagated from
    ///   [`AddressSpace::unmap`].
    pub fn unmap_from<P: PageTable>(
        &mut self,
        space: &mut AddressSpace<P>,
        region: MmioRegion,
    ) -> Result<(), MmioError> {
        self.unmap_at(space, region.virt)
    }

    /// Tear down the mapping based at `virt` (the base [`MmioRegion::virt`] a
    /// prior [`Self::map_into`] / [`Self::map_cacheable_into`] returned) from
    /// the borrowed `space`, keyed by its base virtual address alone.
    ///
    /// The shared-memory map path holds only the base virtual address it
    /// handed userland (not the whole opaque [`MmioRegion`]), so it releases
    /// by base; [`Self::unmap_from`] is the by-region wrapper over this.
    ///
    /// # Errors
    ///
    /// * [`MmioError::UnknownRegion`] — `virt` is not a live mapping of this
    ///   allocator (covers double-unmap).
    /// * [`MmioError::PageTable`] — propagated from [`AddressSpace::unmap`].
    pub fn unmap_at<P: PageTable>(
        &mut self,
        space: &mut AddressSpace<P>,
        virt: VirtAddr,
    ) -> Result<(), MmioError> {
        let record = self
            .regions
            .remove(&virt.as_u64())
            .ok_or(MmioError::UnknownRegion)?;
        let data_pages = record.data_pages;
        let first_data_slot = record.leading_guard_slot + 1;
        let trailing_guard_slot = record.leading_guard_slot + 1 + data_pages;

        for i in 0..data_pages {
            let virt = self.virt_of_slot(first_data_slot + i);
            let page = Page::from_addr(virt)?;
            let _ = space.unmap(page)?;
        }

        for s in record.leading_guard_slot..=trailing_guard_slot {
            self.slot_used[s] = false;
        }
        Ok(())
    }

    /// Raw, non-null base pointer to the first register of `region`, resolved
    /// through the direct physical map `phys`.
    ///
    /// The pointer is valid for reads and writes of `region.len()` bytes until
    /// the region is released via [`Self::unmap_from`].
    ///
    /// # Errors
    ///
    /// * [`MmioError::UnknownRegion`] if `region` does not name a live
    ///   mapping of this allocator.
    /// * [`MmioError::DirectMap`] if the region's registers fall
    ///   outside the direct physical map.
    pub fn region_base(
        &self,
        region: &MmioRegion,
        phys: &dyn PhysMap,
    ) -> Result<NonNull<u8>, MmioError> {
        if !self.regions.contains_key(&region.virt.as_u64()) {
            return Err(MmioError::UnknownRegion);
        }
        // The register block is reachable at its device physical base
        // through the direct map; the within-page offset is already
        // baked into `region.phys`.
        phys.translate(PhysAddr::new(region.phys), region.len)
            .ok_or(MmioError::DirectMap)
    }

    /// Number of live mappings.
    #[must_use]
    pub fn live(&self) -> usize {
        self.regions.len()
    }

    /// Total pages in the allocator's virtual window.
    #[must_use]
    pub fn capacity_pages(&self) -> usize {
        self.capacity_pages
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

    fn rollback_partial_map<P: PageTable>(
        &self,
        space: &mut AddressSpace<P>,
        first_data_slot: usize,
        mapped_so_far: usize,
    ) {
        for i in 0..mapped_so_far {
            let virt = self.virt_of_slot(first_data_slot + i);
            if let Ok(page) = Page::from_addr(virt) {
                let _ = space.unmap(page);
            }
        }
    }
}

/// Per-process MMIO register-window mapper.
///
/// Bundles a [`MmioWindowMap`] with the [`AddressSpace`] it *owns*, so the
/// in-kernel driver host can map a device's register block into a driver's
/// address space and hand back a `RegisterWindow`. The guarded mapping
/// mechanism lives in [`MmioWindowMap`] (shared with the `mmio_map` syscall
/// facility); this type is the thin owning adapter.
///
/// Generic over [`PageTable`] so the same code is exercised by
/// `crate::HostPageTable` in unit tests and driven by the
/// architecture page-table types in production. The `'a` lifetime
/// bounds the borrow of the direct physical map the mapper resolves
/// register pointers through.
pub struct MmioMap<'a, P: PageTable> {
    address_space: AddressSpace<P>,
    window: MmioWindowMap,
    /// Direct physical map used to reach a region's device registers
    /// from the CPU. In production this is the boot identity map; in
    /// host tests a `SimPhysMap` standing in for the
    /// device's register block.
    phys: &'a dyn PhysMap,
}

impl<'a, P: PageTable> MmioMap<'a, P> {
    /// Construct a mapper managing the virtual range
    /// `[base, base + capacity_pages * PAGE_SIZE)`.
    ///
    /// `phys` is the kernel's direct physical map; the mapper resolves
    /// every register window through it so a `RegisterWindow` points
    /// at the device's own registers.
    ///
    /// # Errors
    ///
    /// [`MmioError::InvalidMapConfig`] if `capacity_pages == 0`,
    /// `base` is not page-aligned, or the window size overflows.
    pub fn new(
        address_space: AddressSpace<P>,
        base: VirtAddr,
        capacity_pages: usize,
        phys: &'a dyn PhysMap,
    ) -> Result<Self, MmioError> {
        let window = MmioWindowMap::new(base, capacity_pages)?;
        Ok(Self {
            address_space,
            window,
            phys,
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
        self.window
            .map_into(&mut self.address_space, phys_base, len)
    }

    /// Tear down a mapping previously returned by [`Self::map`].
    ///
    /// # Errors
    ///
    /// * [`MmioError::UnknownRegion`] — `region` is not a live
    ///   mapping of this mapper (covers double-unmap).
    /// * [`MmioError::PageTable`] — propagated from
    ///   [`AddressSpace::unmap`].
    pub fn unmap(&mut self, region: MmioRegion) -> Result<(), MmioError> {
        self.window.unmap_from(&mut self.address_space, region)
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
    /// * [`MmioError::UnknownRegion`] if `region` does not name a live
    ///   mapping of this mapper.
    /// * [`MmioError::DirectMap`] if the region's registers fall
    ///   outside the direct physical map.
    pub fn region_base(&self, region: &MmioRegion) -> Result<NonNull<u8>, MmioError> {
        self.window.region_base(region, self.phys)
    }

    /// Number of live mappings.
    #[must_use]
    pub fn live(&self) -> usize {
        self.window.live()
    }

    /// Total pages in the mapper's virtual window.
    #[must_use]
    pub fn capacity_pages(&self) -> usize {
        self.window.capacity_pages()
    }

    /// Number of currently-mapped pages in the underlying address
    /// space (data pages only; guard slots are never mapped).
    #[must_use]
    pub fn mapped_pages(&self) -> usize {
        self.address_space.mapped_pages()
    }
}

#[cfg(all(test, not(loom)))]
mod tests;
