//! Capability identifiers as carried across the ABI.
//!
//! A [`CapabilityId`] is the wire representation of a kernel capability. The
//! identifier space is dense and bounded by [`CAPABILITY_ID_MAX`] so that
//! capability sets can be represented as fixed-size bitmaps without an
//! allocator.
//!
//! Values defined here are part of the frozen `abi-v1` contract: existing
//! identifiers may not be re-numbered or removed; new capabilities must take
//! the next free integer and bump [`CAPABILITY_ID_MAX`] if necessary.

use crate::Errno;

/// Inclusive upper bound on capability identifiers in `abi-v1`.
///
/// Sized to leave headroom for the capabilities introduced by later stages
/// without forcing a `CapabilitySet` to grow past a single 64-bit word per
/// 64 entries. Increasing this value is a breaking ABI change.
pub const CAPABILITY_ID_MAX: u16 = 255;

/// Stable identifier for a kernel capability.
///
/// The inner integer is the on-wire representation; the wrapper type prevents
/// accidental confusion with other 16-bit ABI values such as syscall numbers.
#[repr(transparent)]
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash, Ord, PartialOrd)]
pub struct CapabilityId(u16);

impl CapabilityId {
    /// Mount and unmount filesystems.
    pub const FS_MOUNT: Self = Self(1);
    /// Open raw network sockets.
    pub const NET_RAW: Self = Self(2);
    /// Load a driver module in user space.
    pub const DRV_LOAD: Self = Self(3);
    /// Load a driver module in kernel space (additional to `DRV_LOAD`).
    pub const DRV_KERNEL: Self = Self(4);
    /// Create, modify, or delete users.
    pub const USER_ADMIN: Self = Self(5);
    /// Adjust the system wall clock.
    pub const TIME_SET: Self = Self(6);
    /// Bind to privileged IPC endpoints.
    pub const IPC_BIND_PRIVILEGED: Self = Self(7);
    /// Read the security audit log.
    pub const AUDIT_READ: Self = Self(8);
    /// Write entries to the security audit log.
    pub const AUDIT_WRITE: Self = Self(9);
    /// Allocate and free DMA-able memory through the per-process heap.
    ///
    /// Granted to user-space drivers that need to publish buffer
    /// addresses to a bus-master device (virtio-blk, virtio-net,
    /// future `NVMe`). Holders may call the kernel's DMA allocator,
    /// which hands back page-aligned, contiguous-by-physical-address
    /// regions out of the calling process's heap, with guard pages
    /// around the slab and zero-on-free for every byte ever made
    /// device-visible (`AGENTS.md` §4).
    pub const MEM_DMA: Self = Self(10);
    /// Bind to a hardware interrupt line and wait for its wake-ups.
    ///
    /// Granted to user-space drivers whose hardware raises an IRQ the
    /// driver must observe (virtio-blk / virtio-net completion queues,
    /// future NIC / `NVMe` driver interrupts). Holders may call the
    /// `irq_bind` / `irq_wait` syscall pair (`abi-v1` numbers 8 and 9),
    /// which mint an opaque [`crate::IrqHandle`] backed by a per-line
    /// kernel wait queue and block on it with a caller-supplied
    /// timeout. The capability does not grant the ability to *raise*
    /// or *mask* an interrupt line; both remain kernel-only
    /// (`AGENTS.md` §5.4 — capability checks before state touches).
    pub const IRQ_BIND: Self = Self(11);
    /// Map a device's memory-mapped register window into a driver's
    /// address space.
    ///
    /// Granted to user-space bus drivers (`drivers/bus/pci`,
    /// `drivers/bus/mmio`) that must read and write a device's
    /// register block (a PCI memory BAR, a virtio-MMIO transport
    /// slot). Holders may call the kernel's MMIO-map facility, which
    /// validates the requested physical region, maps it with caching
    /// disabled (`MapFlags::NO_CACHE`), and hands back a
    /// bounds-checked [`RegisterWindow`](crate::RegisterWindow). The
    /// capability does not let a driver synthesise an arbitrary
    /// pointer: the kernel is the sole minter of a `RegisterWindow`,
    /// so a driver can only reach memory the kernel chose to map for
    /// it (`AGENTS.md` §4 — no ambient authority; §5.4 — capability
    /// checks before state touches).
    pub const MMIO_MAP: Self = Self(12);
    /// Query system information beyond the caller's own principal.
    ///
    /// Required by the System Information API (`AGENTS.md` §16.6) for
    /// queries whose answer spans principals other than the caller —
    /// for example listing every process on the system rather than
    /// only the caller's own. Unprivileged, self-scoped queries ("list
    /// my own processes") require no capability; this one gates the
    /// global view (`AGENTS.md` §5.4 — capability checks before state
    /// touches).
    pub const SYSINFO_GLOBAL: Self = Self(13);
    /// Query kernel-internal system information.
    ///
    /// Required by the System Information API (`AGENTS.md` §16.6) for
    /// queries that expose kernel-internal state — for example kernel
    /// memory statistics — which a global-but-unprivileged observer
    /// must not see.
    pub const SYSINFO_KERNEL: Self = Self(14);
    /// Read the detected hardware tree through the System Information
    /// API.
    ///
    /// Required by the privileged hardware-tree query (`AGENTS.md`
    /// §18.4): the tree is exposed read-only to tools through the
    /// System Information API, and there is no path that bypasses this
    /// capability check.
    pub const SYSINFO_HW: Self = Self(15);

    /// Construct a [`CapabilityId`] from its raw value, validating the range.
    ///
    /// Returns [`Errno::OutOfRange`] if `raw` exceeds [`CAPABILITY_ID_MAX`].
    pub const fn from_raw(raw: u16) -> Result<Self, Errno> {
        if raw > CAPABILITY_ID_MAX {
            return Err(Errno::OutOfRange);
        }
        Ok(Self(raw))
    }

    /// Raw on-wire value.
    #[must_use]
    pub const fn as_u16(self) -> u16 {
        self.0
    }

    /// Position of this capability inside a 256-bit capability set.
    ///
    /// Always less than 256 by construction.
    #[must_use]
    pub const fn index(self) -> usize {
        self.0 as usize
    }
}

/// Read-only membership test over a principal's granted capabilities.
///
/// The set's concrete representation (`CapabilitySet` and its 256-bit
/// bitmap) lives in `lib/caps`, which depends on this crate. ABI-level
/// host seams — for example `VirtioHostFactory` in `lib/virtio` — must
/// gate on a granted capability without naming `lib/caps`, because the reverse
/// edge `lib/abi -> lib/caps` would invert the `lib/*` layering
/// (`AGENTS.md` §17.4). They therefore accept `&dyn CapabilityQuery`;
/// `lib/caps` implements this for its `CapabilitySet`.
///
/// The trait is object-safe so a seam can hold a `&dyn CapabilityQuery`
/// without monomorphising over the caller's set type.
pub trait CapabilityQuery {
    /// `true` if the queried principal has been granted `cap`.
    fn holds(&self, cap: CapabilityId) -> bool;
}

#[cfg(test)]
mod tests {
    use super::{CapabilityId, CapabilityQuery, CAPABILITY_ID_MAX};
    use crate::Errno;

    /// Minimal `CapabilityQuery` that grants exactly one capability,
    /// proving the trait is object-safe and usable behind `&dyn`.
    struct OneCap(CapabilityId);
    impl CapabilityQuery for OneCap {
        fn holds(&self, cap: CapabilityId) -> bool {
            cap == self.0
        }
    }

    #[test]
    fn capability_query_is_object_safe_and_answers() {
        let query: &dyn CapabilityQuery = &OneCap(CapabilityId::MEM_DMA);
        assert!(query.holds(CapabilityId::MEM_DMA));
        assert!(!query.holds(CapabilityId::NET_RAW));
    }

    #[test]
    fn well_known_ids_are_frozen() {
        // The numeric values are part of abi-v1; do not renumber.
        assert_eq!(CapabilityId::FS_MOUNT.as_u16(), 1);
        assert_eq!(CapabilityId::NET_RAW.as_u16(), 2);
        assert_eq!(CapabilityId::DRV_LOAD.as_u16(), 3);
        assert_eq!(CapabilityId::DRV_KERNEL.as_u16(), 4);
        assert_eq!(CapabilityId::USER_ADMIN.as_u16(), 5);
        assert_eq!(CapabilityId::TIME_SET.as_u16(), 6);
        assert_eq!(CapabilityId::IPC_BIND_PRIVILEGED.as_u16(), 7);
        assert_eq!(CapabilityId::AUDIT_READ.as_u16(), 8);
        assert_eq!(CapabilityId::AUDIT_WRITE.as_u16(), 9);
        assert_eq!(CapabilityId::MEM_DMA.as_u16(), 10);
        assert_eq!(CapabilityId::IRQ_BIND.as_u16(), 11);
        assert_eq!(CapabilityId::MMIO_MAP.as_u16(), 12);
        assert_eq!(CapabilityId::SYSINFO_GLOBAL.as_u16(), 13);
        assert_eq!(CapabilityId::SYSINFO_KERNEL.as_u16(), 14);
        assert_eq!(CapabilityId::SYSINFO_HW.as_u16(), 15);
    }

    #[test]
    fn from_raw_rejects_out_of_range() {
        assert_eq!(CapabilityId::from_raw(0).map(CapabilityId::as_u16), Ok(0));
        assert_eq!(
            CapabilityId::from_raw(CAPABILITY_ID_MAX).map(CapabilityId::as_u16),
            Ok(CAPABILITY_ID_MAX),
        );
        assert_eq!(
            CapabilityId::from_raw(CAPABILITY_ID_MAX + 1),
            Err(Errno::OutOfRange),
        );
    }

    #[test]
    fn index_is_within_bitset_bounds() {
        assert!(CapabilityId::AUDIT_WRITE.index() < 256);
        assert!(CapabilityId::MEM_DMA.index() < 256);
        assert!(CapabilityId::IRQ_BIND.index() < 256);
        assert!(CapabilityId::MMIO_MAP.index() < 256);
        assert!(CapabilityId::SYSINFO_GLOBAL.index() < 256);
        assert!(CapabilityId::SYSINFO_KERNEL.index() < 256);
        assert!(CapabilityId::SYSINFO_HW.index() < 256);
    }
}
