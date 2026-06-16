//! Device-resource grants and the MMIO-map facility seam the `mmio_map`
//! (`abi-v1` number 26) syscall drives (`plans/PI.md` P10 chunk 5d-0 — the
//! `DriverHost` MMIO/DMA surface reachable over IPC).
//!
//! A user-space driver does not map raw physical memory. It maps a
//! **granted** device resource: when the device manager autoloads a driver
//! for a hardware-tree node (`AGENTS.md` §18.3) the kernel issues the
//! driver one unforgeable handle per [`HwResource`] that node requested —
//! and *no more* (§4 — no ambient authority). The `mmio_map` syscall takes
//! such a handle, the kernel resolves it **against the calling task**
//! (handle forgery is rejected exactly as `irq_wait` re-checks its binding,
//! §5.4), confirms the grant names a memory window, and maps only that
//! region into the caller's own address space.
//!
//! This module owns the two object-safe boundaries the `mmio_map` handler
//! composes, mirroring [`crate::memmap::MemMap`] and the
//! [`rustos_kernel_irq::IrqTable`] precedents:
//!
//! * [`ResourceGrants`] — the *policy* half: resolve a `(task, handle)`
//!   pair to the granted [`HwResource`], or `None` (fail closed). The
//!   concrete per-process grant table — minted at driver spawn and reclaimed
//!   on exit — is installed by a later landing; until then the handler holds
//!   [`NULL_RESOURCE_GRANTS`], so every `mmio_map` resolves to
//!   [`Errno::NotFound`] rather than mapping anything.
//! * [`MmioMapFacility`] — the *mechanism* half: map a validated physical
//!   window into the caller's live address space. Naming a port's concrete
//!   page table and direct physical map is irreducibly architecture-specific
//!   (`AGENTS.md` §17.2 / §17.4), so — like [`crate::memmap::MemMap`] — the
//!   concrete producer is installed at boot through a `with_*` builder and
//!   the handler reaches it through this trait. Until one is installed the
//!   handler holds [`NULL_MMIO_MAP_FACILITY`], which fails closed with
//!   [`Errno::NotImplemented`] (`AGENTS.md` §2.9).
//!
//! Splitting policy from mechanism keeps the capability/grant decision in
//! the syscall handler (where `AGENTS.md` §5.4 demands it) and the
//! page-table knowledge out of `kernel/core` (`AGENTS.md` §17.4).

use rustos_abi::hwtree::{HwResource, HwResourceKind};
use rustos_abi::Errno;
use rustos_kernel_sec::TaskId;

/// The per-task device-resource grant lookup the `mmio_map` handler
/// consults (`AGENTS.md` §18.3 — a driver reaches only the resources its
/// matched node requested).
///
/// Implementations must be [`Sync`]: the single installed table is shared
/// by the per-CPU syscall handlers, exactly like [`crate::memmap::MemMap`]
/// and the address-space registry.
pub trait ResourceGrants: Sync {
    /// Resolve the device-resource grant identified by `handle` for the
    /// owning `task`.
    ///
    /// Returns the granted [`HwResource`] iff `handle` was minted for
    /// `task`; `None` for an unknown handle, a handle minted for a
    /// *different* task (forgery — `AGENTS.md` §5.4), or a grant that has
    /// since been reclaimed. The owner argument is the kernel-trusted
    /// caller task id, never a caller-supplied value, so a driver cannot
    /// reach another driver's window by guessing a handle value.
    fn lookup(&self, task: TaskId, handle: u64) -> Option<HwResource>;
}

/// The grant table installed before any real one exists.
///
/// Every lookup returns `None` — the fail-closed default `AGENTS.md` §2.9 /
/// §5.4 require: a `mmio_map` issued before the boot/spawn path mints a
/// driver's grants resolves to [`Errno::NotFound`] rather than mapping an
/// ungranted region.
#[derive(Debug, Default, Copy, Clone)]
pub struct EmptyResourceGrants;

impl ResourceGrants for EmptyResourceGrants {
    fn lookup(&self, _task: TaskId, _handle: u64) -> Option<HwResource> {
        None
    }
}

/// The shared [`EmptyResourceGrants`] the syscall handler defaults to.
///
/// An *immutable* static (the type carries no interior mutability), so it
/// is not the global mutable state `AGENTS.md` §2.1 forbids; the boot path
/// replaces the handler's borrow with a real per-process table through
/// `KernelSyscallHandlers::with_resource_grants` once grant issuance lands.
pub static NULL_RESOURCE_GRANTS: EmptyResourceGrants = EmptyResourceGrants;

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn null_resource_grants_resolve_to_none() {
        assert_eq!(NULL_RESOURCE_GRANTS.lookup(TaskId(7), 1), None);
        // Even handle 0 (the reserved invalid value) resolves to nothing.
        assert_eq!(EmptyResourceGrants.lookup(TaskId(0), 0), None);
    }

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
}
