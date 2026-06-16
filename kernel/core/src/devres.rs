//! The MMIO-map facility seam the `mmio_map` (`abi-v1` number 26) syscall
//! drives (`plans/PI.md` P10 chunk 5d-0 — the `DriverHost` MMIO/DMA surface
//! reachable over IPC).
//!
//! A user-space driver does not map raw physical memory. It maps a
//! **granted** device resource: when the device manager autoloads a driver
//! for a hardware-tree node (`AGENTS.md` §18.3) the kernel mints the driver
//! one unforgeable handle per [`HwResource`] that node requested — and *no
//! more* (§4 — no ambient authority). The `mmio_map` syscall takes such a
//! handle and the kernel resolves it **against the calling task** through
//! the per-task grant table that lives in
//! [`AddressSpaceRegistry`](crate::aspace::AddressSpaceRegistry) (minted at
//! driver admission, reclaimed when the task is withdrawn on exit — the same
//! per-process lifecycle as the task's streams and limits, so handle forgery
//! is rejected exactly as `irq_wait` re-checks its binding, §5.4). This
//! module owns the two pieces of the handler that are *not* that table:
//!
//! * [`mappable_window`] — the input-validation half: confirm a resolved
//!   grant names a memory window `mmio_map` can map and return its
//!   `(phys_base, len)`, or a fail-closed [`Errno`] (`AGENTS.md` §5.4).
//! * [`MmioMapFacility`] — the *mechanism* half: map a validated physical
//!   window into the caller's live address space. Naming a port's concrete
//!   page table and direct physical map is irreducibly architecture-specific
//!   (`AGENTS.md` §17.2 / §17.4), so — like [`crate::memmap::MemMap`] — the
//!   concrete producer is installed at boot through a `with_*` builder and
//!   the handler reaches it through this trait. Until one is installed the
//!   handler holds [`NULL_MMIO_MAP_FACILITY`], which fails closed with
//!   [`Errno::NotImplemented`] (`AGENTS.md` §2.9).
//!
//! Keeping the grant *policy* in the per-task registry (where the
//! capability/grant decision belongs, `AGENTS.md` §5.4) and the page-table
//! *mechanism* behind this trait keeps page-table knowledge out of
//! `kernel/core` (`AGENTS.md` §17.4).

use rustos_abi::hwtree::{HwResource, HwResourceKind};
use rustos_abi::Errno;

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
    /// implementation never makes the mapping executable (`AGENTS.md`
    /// §19.2 — W^X for a register window is meaningless and unsafe) and
    /// maps into the *running caller's* space (the active address space on
    /// the CPU servicing the syscall), exactly like
    /// [`crate::memmap::MemMap::map`].
    ///
    /// # Errors
    ///
    /// Returns a stable [`Errno`] — [`Errno::OutOfMemory`] when no
    /// page-table frame or virtual window is available (deterministic OOM,
    /// never a panic, `AGENTS.md` §4 / §2.9), or another stable code the
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
/// viewport maps it onto the far-side bus address (`AGENTS.md` §18.1).
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
/// grant and validated its kind/constraint (`AGENTS.md` §5.4 / §18.3); this
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
    /// device addressing constraint, `AGENTS.md` §18.3; `0` declares no
    /// constraint). Return the buffer's CPU virtual base and its
    /// physically-contiguous base.
    ///
    /// The handler guarantees `len` is non-zero before calling this.
    ///
    /// # Errors
    ///
    /// Returns a stable [`Errno`] — [`Errno::OutOfMemory`] when no
    /// contiguous block or page-table frame is available (deterministic OOM,
    /// `AGENTS.md` §4 / §2.9), [`Errno::OutOfRange`] when the request
    /// exceeds the addressing limit or the maximum contiguous block, or
    /// another stable code the platform reports. The default producer
    /// ([`NullDmaAllocFacility`]) returns [`Errno::NotImplemented`].
    fn alloc(&self, len: usize, addr_limit: u64) -> Result<DmaCarve, Errno>;
}

/// The DMA-alloc facility installed before any real one exists.
///
/// Every carve fails closed with [`Errno::NotImplemented`] — the fail-closed
/// default `AGENTS.md` §2.9 / §5.4 require. Mirrors [`NullMmioMapFacility`].
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
/// default `AGENTS.md` §2.9 / §5.4 require, so a `mmio_map` issued before
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

/// Validate that a granted [`HwResource`] is a memory window `mmio_map`
/// can map, returning the `(phys_base, len)` the [`MmioMapFacility`] takes.
///
/// This is the input-validation half of the `mmio_map` handler, kept here
/// as a pure function so it is exercised directly by unit tests
/// (`AGENTS.md` §5.4.3 — validate every input):
///
/// * the resource must be an MMIO register window
///   ([`HwResourceKind::Mmio`]) or an outbound bus window
///   ([`HwResourceKind::BusWindow`]) — the two kinds whose far side is a
///   memory-mapped block. A [`HwResourceKind::Port`] (x86 I/O ports),
///   [`HwResourceKind::Irq`], or [`HwResourceKind::Dma`] grant is **not** a
///   mappable window and is refused with [`Errno::OutOfRange`], even though
///   its required capability may also be `CAP_MMIO_MAP`;
/// * the length must be non-zero and fit in `usize` on the target;
/// * `base + len` must not overflow the address space.
///
/// # Errors
///
/// * [`Errno::OutOfRange`] — the resource is not a mappable memory window,
///   or its wire discriminant is unknown.
/// * [`Errno::LengthOutOfRange`] — the length is zero, exceeds `usize`, or
///   `base + len` overflows.
pub fn mappable_window(resource: &HwResource) -> Result<(u64, usize), Errno> {
    match resource.kind() {
        Some(HwResourceKind::Mmio | HwResourceKind::BusWindow) => {}
        // A known-but-non-window kind, or an unknown discriminant, is the
        // wrong shape for `mmio_map`; refuse rather than mapping it.
        _ => return Err(Errno::OutOfRange),
    }
    let base = resource.base();
    let len_u64 = resource.length();
    if len_u64 == 0 {
        return Err(Errno::LengthOutOfRange);
    }
    let len = usize::try_from(len_u64).map_err(|_| Errno::LengthOutOfRange)?;
    // The window must lie within the address space: a base + len that
    // overflows names no real region (`AGENTS.md` §5.4 — validate every
    // input; fail closed rather than wrapping).
    base.checked_add(len_u64).ok_or(Errno::LengthOutOfRange)?;
    Ok((base, len))
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
/// (`AGENTS.md` §5.4.3 — validate every input):
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
    fn mappable_window_accepts_mmio_and_bus_windows() {
        let mmio = HwResource::mmio(0xFE98_0000, 0x4000);
        assert_eq!(mappable_window(&mmio), Ok((0xFE98_0000, 0x4000)));

        let bus = HwResource::bus_window(0x6000_0000, 0x400_0000, 0xF800_0000);
        assert_eq!(mappable_window(&bus), Ok((0x6000_0000, 0x400_0000)));
    }

    #[test]
    fn mappable_window_rejects_non_window_kinds() {
        // A DMA constraint, an IRQ line, and an x86 port range are not
        // memory windows `mmio_map` can map.
        assert_eq!(
            mappable_window(&HwResource::dma(0x4000_0000, 0)),
            Err(Errno::OutOfRange)
        );
        assert_eq!(
            mappable_window(&HwResource::irq(33, 1)),
            Err(Errno::OutOfRange)
        );
        assert_eq!(
            mappable_window(&HwResource::port(0x3F8, 8)),
            Err(Errno::OutOfRange)
        );
    }

    #[test]
    fn mappable_window_rejects_zero_length() {
        assert_eq!(
            mappable_window(&HwResource::mmio(0xFE00_0000, 0)),
            Err(Errno::LengthOutOfRange)
        );
    }

    #[test]
    fn mappable_window_rejects_overflowing_window() {
        // base + len wraps the 64-bit address space: no real region.
        assert_eq!(
            mappable_window(&HwResource::mmio(u64::MAX - 0x10, 0x1000)),
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
}
