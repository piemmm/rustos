//! Set-once boot-state publication for the riscv64 `virt`-board boot
//! consumer.
//!
//! The riscv64 virtio-MMIO QEMU verticals run as freestanding bins that
//! re-use the [`crate::boot`] pipeline and hijack the boot hart on
//! `AuditEvent::BootCompleted`. At that point they need two pieces of
//! state the boot pipeline already computed but otherwise moves into
//! the kernel-core hand-off:
//!
//! * the firmware [`BootMemoryMap`] — to carve a per-device DMA pool
//!   from high RAM without re-borrowing the `pub(crate)` kernel state,
//!   and
//! * the flattened-device-tree pointer — to walk the `virtio_mmio`
//!   slots, the PLIC base, and each device's `interrupts` cell.
//!
//! Both are exposed through set-once slots ([`OnceCell`]): the boot
//! pipeline publishes once, before the map is moved into the hand-off,
//! and any observer reads an immutable `'static` view. A second publish
//! is rejected by the `OnceCell` (`AGENTS.md` §2.1 — one-shot publish);
//! the accessors expose no writable surface (`AGENTS.md` §2.4).
//!
//! These slots used to live in the arch crate. They moved here with the
//! boot pipeline when the arch port became a pure Arch HAL
//! implementation (`AGENTS.md` §17.2): publishing the firmware
//! `BootMemoryMap` requires naming `kernel/mem`, which the arch port no
//! longer does.

use rustos_kernel_mem::BootMemoryMap;
use rustos_sync::once::OnceCell;

/// Set-once slot for a clone of the firmware [`BootMemoryMap`] the boot
/// pipeline assembled.
static MEMORY_MAP_SLOT: OnceCell<BootMemoryMap> = OnceCell::new();

/// Set-once slot for the flattened-device-tree pointer (`a1`) OpenSBI
/// handed the boot hart.
static DTB_PTR_SLOT: OnceCell<u64> = OnceCell::new();

/// Publish a clone of the firmware [`BootMemoryMap`] into the set-once
/// slot.
///
/// Called once from [`crate::boot::try_boot`] with the assembled map,
/// before the original is moved into the `kernel_core` hand-off. A
/// second call is a no-op (`OnceCell::set` rejects it); the boot
/// pipeline only ever calls this once, so the discarded `Err` cannot
/// mask a real defect (`AGENTS.md` §2.1 — one-shot publish).
pub fn publish_memory_map(map: &BootMemoryMap) {
    let _ = MEMORY_MAP_SLOT.set(map.clone());
}

/// Read the [`BootMemoryMap`] published by [`publish_memory_map`].
///
/// Returns `None` until the boot pipeline has published the map. The
/// returned reference is to the `'static` slot-owned clone; the
/// accessor exposes no writable surface (`AGENTS.md` §2.4).
#[must_use]
pub fn published_memory_map() -> Option<&'static BootMemoryMap> {
    MEMORY_MAP_SLOT.get().unwrap_or_default()
}

/// Publish the flattened-device-tree pointer into the set-once slot.
///
/// Called once from [`crate::boot::try_boot`] with the verbatim `a1`
/// hand-off value. A second call is a no-op, matching the
/// one-shot-publish discipline of [`publish_memory_map`].
pub fn publish_dtb(dtb: u64) {
    let _ = DTB_PTR_SLOT.set(dtb);
}

/// Read the device-tree pointer published by [`publish_dtb`].
///
/// Returns `None` until the boot pipeline has published it. The pointer
/// is the raw physical address of the device-tree blob; callers walk it
/// through the boot identity map (`AGENTS.md` §2.4 — read-only
/// accessor).
#[must_use]
pub fn published_dtb() -> Option<u64> {
    match DTB_PTR_SLOT.get() {
        Ok(slot) => slot.copied(),
        Err(_) => None,
    }
}
