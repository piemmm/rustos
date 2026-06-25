//! The MMIO-map facility seam the `mmio_map` (`abi-v1` number 26) syscall
//! drives (`plans/PI.md` P10 chunk 5d-0 — the `DriverHost` MMIO/DMA surface
//! reachable over IPC).
//!
//! A user-space driver does not map raw physical memory. It maps a
//! **granted** device resource: when the device manager autoloads a driver
//! for a hardware-tree node the kernel mints the driver
//! one unforgeable handle per [`HwResource`] that node requested — and *no
//! more* (no ambient authority). The `mmio_map` syscall takes such a
//! handle (plus the `[offset, offset + len)` sub-region to map) and the
//! kernel resolves it **against the calling task** through the per-task
//! grant table that lives in
//! [`AddressSpaceRegistry`](crate::aspace::AddressSpaceRegistry) (minted at
//! driver admission, reclaimed when the task is withdrawn on exit — the same
//! per-process lifecycle as the task's streams and limits, so handle forgery
//! is rejected exactly as `irq_wait` re-checks its binding). This
//! module owns the two pieces of the handler that are *not* that table:
//!
//! * [`mappable_subwindow`] — the input-validation half: confirm a resolved
//!   grant names a memory window `mmio_map` can map and the requested
//!   sub-region lies wholly inside it, returning that sub-region's
//!   `(phys_base, len)`, or a fail-closed [`Errno`].
//! * [`MmioMapFacility`] — the *mechanism* half: map a validated physical
//!   window into the caller's live address space. Naming a port's concrete
//!   page table and direct physical map is irreducibly architecture-specific, so — like [`crate::memmap::MemMap`] — the
//!   concrete producer is installed at boot through a `with_*` builder and
//!   the handler reaches it through this trait. Until one is installed the
//!   handler holds [`NULL_MMIO_MAP_FACILITY`], which fails closed with
//!   [`Errno::NotImplemented`].
//!
//! Keeping the grant *policy* in the per-task registry (where the
//! capability/grant decision belongs) and the page-table
//! *mechanism* behind this trait keeps page-table knowledge out of
//! `kernel/core`.

use rustos_abi::hwtree::{HwResource, HwResourceKind};
use rustos_abi::{Errno, MsiAllocation};

/// The kernel-side producer that maps a validated device window into the
/// caller's own live address space.
///
/// Implemented by the architecture-port-installed producer. The handler
/// has already resolved + validated the grant (caller ownership, resource
/// kind, length); this trait performs only the page-table mechanism —
/// installing a caching-disabled mapping of `[phys_base, phys_base + len)`
/// into the **caller's own** hardware-isolated address space and reporting
/// its base user virtual address.
///
/// Implementations must be [`Sync`], shared by the per-CPU handlers exactly
/// like [`crate::memmap::MemMap`].
pub trait MmioMapFacility: Sync {
    /// Map `len` bytes of device physical memory beginning at `phys_base`,
    /// caching disabled, into the caller's own address space; return the
    /// base user virtual address of the new mapping.
    ///
    /// The handler guarantees `len` is non-zero and that
    /// `phys_base + len` does not overflow before calling this. The
    /// implementation never makes the mapping executable (W^X for a register window is meaningless and unsafe) and
    /// maps into the *running caller's* space (the active address space on
    /// the CPU servicing the syscall), exactly like
    /// [`crate::memmap::MemMap::map`].
    ///
    /// # Errors
    ///
    /// Returns a stable [`Errno`] — [`Errno::OutOfMemory`] when no
    /// page-table frame or virtual window is available (deterministic OOM,
    /// never a panic), or another stable code the
    /// platform reports. The default producer ([`NullMmioMapFacility`])
    /// returns [`Errno::NotImplemented`] to mark an inert interface.
    fn map_window(&self, phys_base: u64, len: usize) -> Result<u64, Errno>;
}

/// A DMA buffer the [`DmaAllocFacility`] carved for a driver.
///
/// `cpu_va` is the base user virtual address the driver's CPU accesses go
/// through; `device_addr` is the **device-visible** base the driver
/// programs into the hardware. For a coherent bus (and the QEMU `virt`
/// stand-in) `device_addr` is the CPU-physical base; a translating inbound
/// viewport maps it onto the far-side bus address.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub struct DmaCarve {
    /// Base user virtual address of the mapped, guard-bracketed buffer.
    pub cpu_va: u64,
    /// Device-visible base address the driver hands to the hardware.
    pub device_addr: u64,
}

/// The kernel-side producer that carves a coherent DMA buffer into the
/// caller's own live address space (`plans/PI.md` P10 chunk 5d-0).
///
/// Implemented by the architecture-port-installed producer, mirroring
/// [`MmioMapFacility`]: the handler has already resolved + owner-checked the
/// grant and validated its kind/constraint; this
/// trait performs only the carve mechanism — a physically-contiguous,
/// zeroed, coherent block mapped `RW`, non-executable, guard-bracketed into
/// the **caller's own** address space, bounded by `addr_limit`.
///
/// Implementations must be [`Sync`], shared by the per-CPU handlers exactly
/// like [`MmioMapFacility`].
pub trait DmaAllocFacility: Sync {
    /// Carve `len` bytes of physically-contiguous, zeroed, coherent DMA
    /// memory into the caller's own address space, bounded so the backing
    /// block lies wholly below `addr_limit` when it is non-zero (the granted
    /// device addressing constraint; `0` declares no
    /// constraint). Return the buffer's CPU virtual base and its
    /// physically-contiguous base.
    ///
    /// The handler guarantees `len` is non-zero before calling this.
    ///
    /// # Errors
    ///
    /// Returns a stable [`Errno`] — [`Errno::OutOfMemory`] when no
    /// contiguous block or page-table frame is available (deterministic OOM), [`Errno::OutOfRange`] when the request
    /// exceeds the addressing limit or the maximum contiguous block, or
    /// another stable code the platform reports. The default producer
    /// ([`NullDmaAllocFacility`]) returns [`Errno::NotImplemented`].
    fn alloc(&self, len: usize, addr_limit: u64) -> Result<DmaCarve, Errno>;
}

/// The DMA-alloc facility installed before any real one exists.
///
/// Every carve fails closed with [`Errno::NotImplemented`] — the fail-closed
/// default require. Mirrors [`NullMmioMapFacility`].
#[derive(Debug, Default, Copy, Clone)]
pub struct NullDmaAllocFacility;

impl DmaAllocFacility for NullDmaAllocFacility {
    fn alloc(&self, _len: usize, _addr_limit: u64) -> Result<DmaCarve, Errno> {
        Err(Errno::NotImplemented)
    }
}

/// The shared [`NullDmaAllocFacility`] the syscall handler defaults to.
pub static NULL_DMA_ALLOC_FACILITY: NullDmaAllocFacility = NullDmaAllocFacility;

/// The MMIO-map facility installed before any real one exists.
///
/// Every map fails closed with [`Errno::NotImplemented`] — the fail-closed
/// default require, so a `mmio_map` issued before
/// the boot path installs the `kernel/mem` producer announces an inert
/// interface rather than pretending a window was mapped. Mirrors
/// [`crate::memmap::NullMemMap`].
#[derive(Debug, Default, Copy, Clone)]
pub struct NullMmioMapFacility;

impl MmioMapFacility for NullMmioMapFacility {
    fn map_window(&self, _phys_base: u64, _len: usize) -> Result<u64, Errno> {
        Err(Errno::NotImplemented)
    }
}

/// The shared [`NullMmioMapFacility`] the syscall handler defaults to.
///
/// `KernelSyscallHandlers::new` points its `mmio_map_facility` borrow here
/// so the field is always valid without an `Option` branch on the hot
/// path; the boot path replaces it with the real producer through
/// `KernelSyscallHandlers::with_mmio_map_facility`.
pub static NULL_MMIO_MAP_FACILITY: NullMmioMapFacility = NullMmioMapFacility;

/// The kernel-side producer that allocates a message-signalled interrupt
/// (MSI) vector and reports the architecture-built doorbell for it (the
/// `msi_alloc` syscall seam).
///
/// Allocating an MSI vector and bringing the platform's MSI controller up
/// is irreducibly architecture-specific (the BCM2711 root-complex MSI
/// controller, an x86 IO-APIC/LAPIC MSI domain, …), so — like
/// [`MmioMapFacility`] / [`DmaAllocFacility`] — the concrete producer is
/// installed at boot through a `with_*` builder and the handler reaches it
/// through this trait. The handler owns the *policy* (capability gate,
/// minting the caller's device-resource grant, copying the result out); this
/// trait performs only the *mechanism* — minting a free vector, lazily
/// bringing the controller up, and building the doorbell.
///
/// Implementations must be [`Sync`], shared by the per-CPU handlers exactly
/// like [`MmioMapFacility`].
pub trait MsiAllocFacility: Sync {
    /// Allocate one MSI vector and return the [`MsiAllocation`] naming the
    /// virtual interrupt line it is delivered on and the doorbell
    /// `(address, data)` a PCI function's MSI capability is programmed with
    /// to route its message to that line.
    ///
    /// Idempotently brings the platform's MSI controller up on the first
    /// allocation (routing + enabling its shared interrupt line).
    ///
    /// # Errors
    ///
    /// Returns a stable [`Errno`] — [`Errno::OutOfRange`] when the vector
    /// space is exhausted, or another stable code the platform reports. The
    /// default producer ([`NullMsiAllocFacility`]) returns
    /// [`Errno::NotImplemented`] to mark an inert interface (a platform with
    /// no MSI controller).
    fn allocate(&self) -> Result<MsiAllocation, Errno>;
}

/// The MSI-alloc facility installed before any real one exists.
///
/// Every allocation fails closed with [`Errno::NotImplemented`] — the
/// fail-closed default, so an `msi_alloc` issued on a platform with no MSI
/// controller announces an inert interface rather than fabricating a vector.
/// Mirrors [`NullMmioMapFacility`].
#[derive(Debug, Default, Copy, Clone)]
pub struct NullMsiAllocFacility;

impl MsiAllocFacility for NullMsiAllocFacility {
    fn allocate(&self) -> Result<MsiAllocation, Errno> {
        Err(Errno::NotImplemented)
    }
}

/// The shared [`NullMsiAllocFacility`] the syscall handler defaults to.
pub static NULL_MSI_ALLOC_FACILITY: NullMsiAllocFacility = NullMsiAllocFacility;

/// The kernel-side producer that backs the cross-process shared-memory
/// syscalls (`shm_create` / `shm_map` / `shm_unmap`, `plans/USB.md`).
///
/// A shared-memory region is a physically-contiguous block of kernel-owned
/// RAM the kernel allocates once, zeroes, and maps (cacheable, `RW`,
/// non-executable, guard-bracketed) into one or more processes' own live
/// address spaces so they can exchange bulk data without a kernel copy. The
/// *policy* — the per-region registry, reference counting, the per-region
/// capability grant, and the capability gate — lives in `kernel/core`; this
/// trait performs only the *mechanism* that needs the frame allocator and
/// the running task's live space:
///
/// * [`alloc_region`](Self::alloc_region) — allocate a contiguous,
///   **zeroed** block of `pages` frames (no cross-process leak);
/// * [`map_region`](Self::map_region) — map an existing region into the
///   **calling** task's own live space;
/// * [`unmap_region`](Self::unmap_region) — release the caller's mapping;
/// * [`free_region`](Self::free_region) — zero (zero-on-free) and return a
///   region's frames to the allocator at its last reference.
///
/// Zeroing on allocation and on free is done through the kernel direct map
/// of *whatever task is currently running* (the map is identical in every
/// process and covers all RAM), so the last-reference free works even when
/// the task whose teardown drops it is not the region's owner (a hot-removed
/// driver torn down by the device manager). Implementations must be [`Sync`]
/// like [`MmioMapFacility`].
pub trait SharedMemFacility: Sync {
    /// Allocate a physically-contiguous, **zeroed** block of `pages` frames
    /// the kernel owns, returning its physical base and the buddy `order`
    /// the caller must hand back to [`Self::free_region`].
    ///
    /// # Errors
    ///
    /// [`Errno::OutOfMemory`] when no contiguous block is free (deterministic
    /// OOM), [`Errno::OutOfRange`] when `pages` exceeds the maximum buddy
    /// block, [`Errno::LengthOutOfRange`] when `pages` is zero, or
    /// [`Errno::NotImplemented`] for the inert default.
    fn alloc_region(&self, pages: u64) -> Result<(u64, u32), Errno>;

    /// Map the existing region of `pages` frames beginning at `phys_base`
    /// into the calling task's own live space, returning its base user
    /// virtual address.
    ///
    /// # Errors
    ///
    /// [`Errno::OutOfMemory`] (no virtual slot), [`Errno::NotImplemented`]
    /// (no live space / inert default), or another stable code the platform
    /// reports.
    fn map_region(&self, phys_base: u64, pages: u64) -> Result<u64, Errno>;

    /// Release the calling task's shared mapping based at `base` (`len`
    /// bytes), tearing down only its page-table entries.
    ///
    /// # Errors
    ///
    /// [`Errno::NotFound`] if `base` does not name a live shared mapping of
    /// the caller, or [`Errno::NotImplemented`] for the inert default.
    fn unmap_region(&self, base: u64, len: usize) -> Result<(), Errno>;

    /// Zero the `pages` frames beginning at `phys_base` (zero-on-free) and
    /// return the `order` buddy block to the allocator. Best-effort: a frame
    /// that cannot be reached for scrubbing is dropped rather than panicking
    /// (the inert default is a no-op).
    fn free_region(&self, phys_base: u64, order: u32, pages: u64);
}

/// The shared-memory facility installed before any real one exists.
///
/// Every operation fails closed with [`Errno::NotImplemented`] (and
/// [`free_region`](SharedMemFacility::free_region) is an inert no-op), so a
/// `shm_*` syscall on a build with no live-space producer announces an inert
/// interface rather than pretending a region exists. Mirrors
/// [`NullMmioMapFacility`].
#[derive(Debug, Default, Copy, Clone)]
pub struct NullSharedMemFacility;

impl SharedMemFacility for NullSharedMemFacility {
    fn alloc_region(&self, _pages: u64) -> Result<(u64, u32), Errno> {
        Err(Errno::NotImplemented)
    }
    fn map_region(&self, _phys_base: u64, _pages: u64) -> Result<u64, Errno> {
        Err(Errno::NotImplemented)
    }
    fn unmap_region(&self, _base: u64, _len: usize) -> Result<(), Errno> {
        Err(Errno::NotImplemented)
    }
    fn free_region(&self, _phys_base: u64, _order: u32, _pages: u64) {}
}

/// The shared [`NullSharedMemFacility`] the syscall handler defaults to.
pub static NULL_SHARED_MEM_FACILITY: NullSharedMemFacility = NullSharedMemFacility;

/// Validate that the caller's `[offset, offset + len)` sub-region of a
/// granted [`HwResource`] is a memory window `mmio_map` can map, returning
/// the `(phys_base, len)` the [`MmioMapFacility`] takes (the sub-region's
/// physical base and length).
///
/// This is the input-validation half of the `mmio_map` handler, kept here
/// as a pure function so it is exercised directly by unit tests
/// (validate every input):
///
/// * the resource must be an MMIO register window
///   ([`HwResourceKind::Mmio`]) or an outbound bus window
///   ([`HwResourceKind::BusWindow`]) — the two kinds whose far side is a
///   memory-mapped block. A [`HwResourceKind::Port`] (x86 I/O ports),
///   [`HwResourceKind::Irq`], or [`HwResourceKind::Dma`] grant is **not** a
///   mappable window and is refused with [`Errno::OutOfRange`], even though
///   its required capability may also be `CAP_MMIO_MAP`;
/// * `len` must be non-zero and fit in `usize` on the target;
/// * `[offset, offset + len)` must lie **wholly inside** the granted
///   window (`offset + len <= resource.length()`) — a driver maps a
///   sub-region *inside* its grant, never past it (no
///   ambient authority;). Mapping a bounded sub-region is what lets a
///   driver granted a large outbound bus aperture map just the single BAR
///   it enumerated, instead of the whole 1 GiB window;
/// * `base + offset + len` must not overflow the address space.
///
/// # Errors
///
/// * [`Errno::OutOfRange`] — the resource is not a mappable memory window,
///   its wire discriminant is unknown, or the sub-region escapes the
///   granted window.
/// * [`Errno::LengthOutOfRange`] — `len` is zero, exceeds `usize`, or the
///   absolute `base + offset + len` overflows.
pub fn mappable_subwindow(
    resource: &HwResource,
    offset: u64,
    len: usize,
) -> Result<(u64, usize), Errno> {
    match resource.kind() {
        Some(HwResourceKind::Mmio | HwResourceKind::BusWindow) => {}
        // A known-but-non-window kind, or an unknown discriminant, is the
        // wrong shape for `mmio_map`; refuse rather than mapping it.
        _ => return Err(Errno::OutOfRange),
    }
    if len == 0 {
        return Err(Errno::LengthOutOfRange);
    }
    let len_u64 = u64::try_from(len).map_err(|_| Errno::LengthOutOfRange)?;
    // The requested sub-region must lie wholly inside the granted window:
    // `offset + len <= grant length`. A sub-region that escapes the grant
    // would reach memory the driver was never granted, so it is refused
    // (fail closed; — no ambient authority).
    let end = offset.checked_add(len_u64).ok_or(Errno::OutOfRange)?;
    if end > resource.length() {
        return Err(Errno::OutOfRange);
    }
    // The absolute physical base of the sub-region; an overflow names no
    // real region (validate every input, never wrap).
    let phys_base = resource
        .base()
        .checked_add(offset)
        .ok_or(Errno::LengthOutOfRange)?;
    phys_base
        .checked_add(len_u64)
        .ok_or(Errno::LengthOutOfRange)?;
    Ok((phys_base, len))
}

/// The validated shape of a granted DMA constraint the `dma_alloc` handler
/// bounds a carve by.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub struct DmaConstraint {
    /// Exclusive CPU-side addressing upper bound a device behind this grant
    /// may reach (`0` declares no constraint).
    pub addr_limit: u64,
    /// Maximum buffer extent in bytes the grant permits (`0` declares no
    /// declared maximum).
    pub max_len: u64,
    /// Far-side (bus/PCIe-space) base of a translating inbound viewport, or
    /// `0` for an untranslated (coherent) constraint.
    pub translated_base: u64,
}

/// Validate that a granted [`HwResource`] is a DMA constraint `dma_alloc`
/// can bound a carve by, returning its [`DmaConstraint`].
///
/// This is the input-validation half of the `dma_alloc` handler, kept here
/// as a pure function so it is exercised directly by unit tests
/// (validate every input):
///
/// * the resource must be a [`HwResourceKind::Dma`] constraint — any other
///   kind (a register window, an IRQ line, a port range), or an unknown wire
///   discriminant, is refused with [`Errno::OutOfRange`];
/// * the `base`/`length` are the CPU-side exclusive addressing limit and the
///   maximum buffer extent, and `translated_base` the far-side viewport base
///   (`0` for an untranslated, coherent constraint).
///
/// # Errors
///
/// [`Errno::OutOfRange`] — the resource is not a DMA constraint, or its wire
/// discriminant is unknown.
pub fn dma_constraint(resource: &HwResource) -> Result<DmaConstraint, Errno> {
    match resource.kind() {
        Some(HwResourceKind::Dma) => {}
        // A known-but-non-DMA kind, or an unknown discriminant, is the wrong
        // shape for `dma_alloc`; refuse rather than carving against it.
        _ => return Err(Errno::OutOfRange),
    }
    Ok(DmaConstraint {
        addr_limit: resource.base(),
        max_len: resource.length(),
        translated_base: resource.translated_base(),
    })
}

/// Resolve the **device-visible** base address a driver programs into its
/// hardware from the CPU-physical base the carve produced, honouring the
/// grant's inbound-viewport translation.
///
/// This is the output half of the `dma_alloc` handler, kept here as a pure
/// function so the translation arithmetic is exercised directly by unit
/// tests:
///
/// * An **untranslated** (coherent) constraint (`translated_base == 0`) — a
///   coherent bus, the QEMU `virt` stand-in, the `VideoCore` mailbox carve —
///   names the device by the CPU-physical base itself, returned unchanged.
/// * A **translating inbound viewport** (`translated_base != 0`) — a PCIe
///   root complex's `dma-ranges`, e.g. the Pi 4's `IB MEM 0x0..0x1ffffffff
///   -> 0x4_0000_0000` — maps the CPU window `[cpu_base, addr_limit)` onto
///   the bus window `[translated_base, translated_base + max_len)`, where
///   `cpu_base = addr_limit - max_len`. A buffer carved at CPU-physical
///   `cpu_phys` is therefore seen by the device at
///   `translated_base + (cpu_phys - cpu_base)`. The carve was already
///   bounded below `addr_limit` by the facility, so it cannot exceed the
///   viewport's top; this only re-bases it onto the far side.
///
/// Every step is checked: a `cpu_phys` below the viewport's CPU base, a
/// base at or past the aperture top, or a far-side overflow fails closed
/// with [`Errno::OutOfRange`] rather than handing back a
/// wrapped or out-of-aperture device address.
///
/// # Errors
///
/// [`Errno::OutOfRange`] when the CPU-physical base does not lie within the
/// translating viewport's CPU window, or the translated address overflows.
pub fn translate_device_addr(constraint: &DmaConstraint, cpu_phys: u64) -> Result<u64, Errno> {
    if constraint.translated_base == 0 {
        return Ok(cpu_phys);
    }
    let cpu_base = constraint
        .addr_limit
        .checked_sub(constraint.max_len)
        .ok_or(Errno::OutOfRange)?;
    let offset = cpu_phys.checked_sub(cpu_base).ok_or(Errno::OutOfRange)?;
    if offset >= constraint.max_len {
        return Err(Errno::OutOfRange);
    }
    constraint
        .translated_base
        .checked_add(offset)
        .ok_or(Errno::OutOfRange)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn null_facility_fails_closed() {
        assert_eq!(
            NULL_MMIO_MAP_FACILITY.map_window(0xFE00_0000, 0x1000),
            Err(Errno::NotImplemented)
        );
        assert_eq!(
            NullMmioMapFacility.map_window(0, 0x1000),
            Err(Errno::NotImplemented)
        );
    }

    #[test]
    fn mappable_subwindow_accepts_the_whole_mmio_and_bus_windows() {
        // Offset 0, full length: the whole granted window (the register-block
        // case where the request equals the grant).
        let mmio = HwResource::mmio(0xFE98_0000, 0x4000);
        assert_eq!(
            mappable_subwindow(&mmio, 0, 0x4000),
            Ok((0xFE98_0000, 0x4000))
        );

        let bus = HwResource::bus_window(0x6000_0000, 0x400_0000, 0xF800_0000);
        assert_eq!(
            mappable_subwindow(&bus, 0, 0x400_0000),
            Ok((0x6000_0000, 0x400_0000))
        );
    }

    #[test]
    fn mappable_subwindow_maps_only_the_requested_sub_region() {
        // A small BAR inside a large (1 GiB) outbound bus window: the
        // sub-region's absolute physical base is `grant.base() + offset` and
        // its length is the request, never the whole 1 GiB grant
        // (the defect this fixes).
        let bus = HwResource::bus_window(0x6_0000_0000, 0x4000_0000, 0xC000_0000);
        assert_eq!(
            mappable_subwindow(&bus, 0x3D50_0000, 0x1000),
            Ok((0x6_3D50_0000, 0x1000))
        );
    }

    #[test]
    fn mappable_subwindow_rejects_a_sub_region_escaping_the_grant() {
        // `offset + len` past the granted window would reach memory the
        // driver was never granted: refused fail-closed.
        let mmio = HwResource::mmio(0xFE98_0000, 0x4000);
        assert_eq!(
            mappable_subwindow(&mmio, 0x3000, 0x2000),
            Err(Errno::OutOfRange)
        );
        // An offset already at the end, any non-zero len: still out of range.
        assert_eq!(
            mappable_subwindow(&mmio, 0x4000, 0x1000),
            Err(Errno::OutOfRange)
        );
    }

    #[test]
    fn mappable_subwindow_rejects_non_window_kinds() {
        // A DMA constraint, an IRQ line, and an x86 port range are not
        // memory windows `mmio_map` can map.
        assert_eq!(
            mappable_subwindow(&HwResource::dma(0x4000_0000, 0), 0, 0x1000),
            Err(Errno::OutOfRange)
        );
        assert_eq!(
            mappable_subwindow(&HwResource::irq(33, 1), 0, 0x1000),
            Err(Errno::OutOfRange)
        );
        assert_eq!(
            mappable_subwindow(&HwResource::port(0x3F8, 8), 0, 0x1000),
            Err(Errno::OutOfRange)
        );
    }

    #[test]
    fn mappable_subwindow_rejects_zero_length() {
        assert_eq!(
            mappable_subwindow(&HwResource::mmio(0xFE00_0000, 0x1000), 0, 0),
            Err(Errno::LengthOutOfRange)
        );
    }

    #[test]
    fn mappable_subwindow_rejects_overflowing_window() {
        // base + offset + len wraps the 64-bit address space: no real region.
        // The grant length is large enough that the bound check passes and the
        // absolute-base overflow is the rejecting condition.
        assert_eq!(
            mappable_subwindow(&HwResource::mmio(u64::MAX - 0x10, u64::MAX), 0, 0x1000),
            Err(Errno::LengthOutOfRange)
        );
    }

    #[test]
    fn null_dma_facility_fails_closed() {
        assert_eq!(
            NULL_DMA_ALLOC_FACILITY.alloc(0x1000, 0),
            Err(Errno::NotImplemented)
        );
        assert_eq!(
            NullDmaAllocFacility.alloc(0x1000, 0x4000_0000),
            Err(Errno::NotImplemented)
        );
    }

    #[test]
    fn dma_constraint_accepts_a_dma_grant() {
        // An untranslated constraint: the limit + extent are recovered and
        // there is no far-side viewport base.
        let plain = HwResource::dma(0x4000_0000, 0x1_0000);
        assert_eq!(
            dma_constraint(&plain),
            Ok(DmaConstraint {
                addr_limit: 0x4000_0000,
                max_len: 0x1_0000,
                translated_base: 0,
            })
        );

        // A translating inbound viewport carries the far-side bus base.
        let translated = HwResource::dma_translated(0xC000_0000, 0x10_0000, 0x4_0000_0000);
        assert_eq!(
            dma_constraint(&translated),
            Ok(DmaConstraint {
                addr_limit: 0xC000_0000,
                max_len: 0x10_0000,
                translated_base: 0x4_0000_0000,
            })
        );
    }

    #[test]
    fn dma_constraint_rejects_non_dma_kinds() {
        // An MMIO window, a bus window, an IRQ line, and a port range are not
        // DMA constraints `dma_alloc` can carve against.
        assert_eq!(
            dma_constraint(&HwResource::mmio(0xFE00_0000, 0x1000)),
            Err(Errno::OutOfRange)
        );
        assert_eq!(
            dma_constraint(&HwResource::bus_window(
                0x6000_0000,
                0x400_0000,
                0xF800_0000
            )),
            Err(Errno::OutOfRange)
        );
        assert_eq!(
            dma_constraint(&HwResource::irq(33, 1)),
            Err(Errno::OutOfRange)
        );
        assert_eq!(
            dma_constraint(&HwResource::port(0x3F8, 8)),
            Err(Errno::OutOfRange)
        );
    }

    #[test]
    fn translate_device_addr_passes_an_untranslated_constraint_through() {
        // A coherent (untranslated) constraint names the device by the
        // CPU-physical base unchanged.
        let coherent = dma_constraint(&HwResource::dma(0x4000_0000, 0x1_0000)).unwrap();
        assert_eq!(translate_device_addr(&coherent, 0x10_0000), Ok(0x10_0000));
        // A zero base passes through too.
        assert_eq!(translate_device_addr(&coherent, 0), Ok(0));
    }

    #[test]
    fn translate_device_addr_rebases_a_translating_viewport() {
        // The Pi 4 inbound viewport: CPU window [0, 0x2_0000_0000) mapped onto
        // bus window [0x4_0000_0000, 0x6_0000_0000) (cpu_base = top - len = 0).
        // A buffer carved at CPU-physical 0x10_0000 is seen by the device at
        // 0x4_0000_0000 + 0x10_0000.
        let viewport = dma_constraint(&HwResource::dma_translated(
            0x2_0000_0000,
            0x2_0000_0000,
            0x4_0000_0000,
        ))
        .unwrap();
        assert_eq!(
            translate_device_addr(&viewport, 0x10_0000),
            Ok(0x4_0010_0000)
        );
        // The lowest CPU address maps to the far-side base exactly.
        assert_eq!(translate_device_addr(&viewport, 0), Ok(0x4_0000_0000));
    }

    #[test]
    fn translate_device_addr_rebases_a_nonzero_cpu_base_viewport() {
        // A viewport whose CPU window does not start at 0: top 0x3000,
        // extent 0x1000 → cpu_base 0x2000, mapped onto bus base 0x9000_0000.
        let viewport =
            dma_constraint(&HwResource::dma_translated(0x3000, 0x1000, 0x9000_0000)).unwrap();
        assert_eq!(translate_device_addr(&viewport, 0x2000), Ok(0x9000_0000));
        assert_eq!(translate_device_addr(&viewport, 0x2800), Ok(0x9000_0800));
        // A base below the viewport's CPU base fails closed (never wraps).
        assert_eq!(
            translate_device_addr(&viewport, 0x1000),
            Err(Errno::OutOfRange)
        );
        // A base at the aperture top is out of the viewport.
        assert_eq!(
            translate_device_addr(&viewport, 0x3000),
            Err(Errno::OutOfRange)
        );
    }
}
