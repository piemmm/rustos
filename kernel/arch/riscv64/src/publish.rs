//! Set-once boot-state publication for the riscv64 port.
//!
//! Unlike x86_64 — whose live-boot observation slots live in the
//! `rustos-kernel` bin crate's `arch_wrapper` because that crate owns the
//! `KernelArch` wrapper — the riscv64 boot pipeline lives in this arch
//! crate (`boot`). The hooks a driver-bring-up observer needs therefore
//! live here too.
//!
//! # What is published
//!
//! The riscv64 virtio-MMIO QEMU verticals run as freestanding bins that
//! re-use the production `boot` pipeline and hijack the boot
//! hart on `AuditEvent::BootCompleted`. At that point they need two
//! pieces of state the boot pipeline already computed but otherwise
//! moves into the kernel-core hand-off:
//!
//! * the firmware [`BootMemoryMap`] — to carve a per-device DMA pool from
//!   high RAM without re-borrowing the `pub(crate)` kernel state, and
//! * the flattened-device-tree pointer — to walk the `virtio_mmio` slots,
//!   the PLIC base, and each device's `interrupts` cell that the MMIO
//!   bring-up scaffold provisions the transport and external-IRQ path
//!   from.
//!
//! Both are exposed through set-once slots ([`OnceCell`]): the boot
//! pipeline publishes once, before the map is moved into the hand-off,
//! and any observer reads an immutable `'static` view. A second publish
//! is rejected by the `OnceCell` (`AGENTS.md` §2.1 — one-shot publish);
//! the accessors expose no writable surface (`AGENTS.md` §2.4 — no
//! interface creep).
//!
//! # Why not an `IrqTable` slot
//!
//! x86_64 also publishes the kernel-core [`rustos_kernel_irq::IrqTable`]
//! (via `KernelArch::install_irq_dispatch`) because its verticals bind a
//! GSI the boot pipeline already routed through the IO-APIC. The riscv64
//! boot-to-`BootCompleted` slice runs with interrupts disabled and hands
//! the kernel the conservative `IrqRouting::unsupported` routing, so its
//! verticals build their own `PlicController` + `IrqTable` over the
//! DTB-discovered PLIC base. Publishing a `max_line == 0` table here
//! would be a misleading stub (`AGENTS.md` §15.1), so it is omitted.

use rustos_kernel_mem::BootMemoryMap;
use rustos_kernel_sync::once::OnceCell;

/// Set-once slot for a clone of the firmware [`BootMemoryMap`] the boot
/// pipeline assembled.
///
/// Published by [`publish_memory_map`] during `boot::try_boot` before the
/// original map is moved into the `kernel_core` hand-off. The slot owns
/// its own clone, so the live kernel allocator and any observer-built
/// allocator draw from the same firmware description but never share a
/// mutable handle.
static MEMORY_MAP_SLOT: OnceCell<BootMemoryMap> = OnceCell::new();

/// Set-once slot for the flattened-device-tree pointer (`a1`) OpenSBI
/// handed the boot hart.
///
/// Published by [`publish_dtb`] during `boot::try_boot`. The value is the
/// raw physical address of the device-tree blob; it is reachable through
/// the boot identity map for the life of the guest.
static DTB_PTR_SLOT: OnceCell<u64> = OnceCell::new();

/// Publish a clone of the firmware [`BootMemoryMap`] into
/// `MEMORY_MAP_SLOT`.
///
/// Called once from `boot::try_boot` with the assembled map, before the
/// original is moved into the `kernel_core` hand-off. A second call is a
/// no-op (`OnceCell::set` rejects it); the boot pipeline only ever calls
/// this once, so the discarded `Err` cannot mask a real defect
/// (`AGENTS.md` §2.1 — one-shot publish).
pub fn publish_memory_map(map: &BootMemoryMap) {
    let _ = MEMORY_MAP_SLOT.set(map.clone());
}

/// Read the [`BootMemoryMap`] published into `MEMORY_MAP_SLOT` by
/// [`publish_memory_map`].
///
/// Returns `None` until `boot::try_boot` has published the map. The
/// returned reference is to the `'static` slot-owned clone; the accessor
/// exposes no writable surface (`AGENTS.md` §2.4).
#[must_use]
pub fn published_memory_map() -> Option<&'static BootMemoryMap> {
    MEMORY_MAP_SLOT.get().unwrap_or_default()
}

/// Publish the flattened-device-tree pointer into `DTB_PTR_SLOT`.
///
/// Called once from `boot::try_boot` with the verbatim `a1` hand-off
/// value. A second call is a no-op (`OnceCell::set` rejects it), matching
/// the one-shot-publish discipline of [`publish_memory_map`].
pub fn publish_dtb(dtb: u64) {
    let _ = DTB_PTR_SLOT.set(dtb);
}

/// Read the device-tree pointer published into `DTB_PTR_SLOT` by
/// [`publish_dtb`].
///
/// Returns `None` until `boot::try_boot` has published it. The pointer is
/// the raw physical address of the device-tree blob; callers walk it
/// through the boot identity map (`AGENTS.md` §2.4 — read-only accessor).
#[must_use]
pub fn published_dtb() -> Option<u64> {
    match DTB_PTR_SLOT.get() {
        Ok(slot) => slot.copied(),
        Err(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustos_kernel_mem::{MemoryRegion, PhysAddr, RegionKind, PAGE_SIZE};

    /// [`publish_memory_map`] hands [`published_memory_map`] a stable,
    /// `'static` clone of the firmware map, and a second publish is a
    /// no-op. This test is the only publisher of `MEMORY_MAP_SLOT` in the
    /// process, so the set-once slot deterministically reflects the map
    /// published here (`AGENTS.md` §2.1 — one-shot publish).
    #[test]
    fn published_memory_map_returns_the_published_clone() {
        assert!(published_memory_map().is_none());

        let mut map = BootMemoryMap::new();
        map.push(MemoryRegion {
            kind: RegionKind::Usable,
            start: PhysAddr::new(PAGE_SIZE as u64 * 16),
            length: (PAGE_SIZE * 32) as u64,
        });
        publish_memory_map(&map);

        let published = published_memory_map().expect("map published");
        assert_eq!(published.regions().len(), 1);
        assert_eq!(
            published.regions()[0].start,
            PhysAddr::new(PAGE_SIZE as u64 * 16)
        );

        // A second publish is a no-op: the slot keeps its first value.
        let mut other = BootMemoryMap::new();
        other.push(MemoryRegion {
            kind: RegionKind::Usable,
            start: PhysAddr::new(PAGE_SIZE as u64 * 1000),
            length: PAGE_SIZE as u64,
        });
        publish_memory_map(&other);
        assert_eq!(
            published_memory_map().expect("still set").regions().len(),
            1
        );
    }

    /// [`publish_dtb`] hands [`published_dtb`] the published pointer and a
    /// second publish is a no-op. This test is the only publisher of
    /// `DTB_PTR_SLOT` in the process.
    #[test]
    fn published_dtb_returns_the_published_pointer() {
        assert!(published_dtb().is_none());

        publish_dtb(0x8200_0000);
        assert_eq!(published_dtb(), Some(0x8200_0000));

        // A second publish is a no-op: the slot keeps its first value.
        publish_dtb(0xDEAD_BEEF);
        assert_eq!(published_dtb(), Some(0x8200_0000));
    }
}
